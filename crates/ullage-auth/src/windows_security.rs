//! Windows-only ACL and security-attribute helpers shared by the file vault
//! and the native Windows vault.
//!
//! Compiled only on Windows; every item here talks to `windows_sys` directly.

use std::path::Path;

use crate::CredentialError;

pub(crate) fn windows_file_attributes_are_safe(attributes: u32) -> bool {
    const FILE_ATTRIBUTE_REPARSE_POINT: u32 = 0x400;
    attributes & FILE_ATTRIBUTE_REPARSE_POINT == 0
}

pub fn create_private_windows_directory(path: &Path) -> Result<(), CredentialError> {
    use std::os::windows::ffi::OsStrExt;
    use windows_sys::Win32::Foundation::LocalFree;
    use windows_sys::Win32::Security::{
        InitializeSecurityDescriptor, SE_DACL_PROTECTED, SECURITY_ATTRIBUTES, SECURITY_DESCRIPTOR,
        SetSecurityDescriptorControl, SetSecurityDescriptorDacl, SetSecurityDescriptorOwner,
    };
    use windows_sys::Win32::Storage::FileSystem::CreateDirectoryW;

    let (acl, mut owner) = private_acl(true)?;
    // SAFETY: zero is a valid pre-initialization state for SECURITY_DESCRIPTOR.
    let mut descriptor: SECURITY_DESCRIPTOR = unsafe { std::mem::zeroed() };
    // SAFETY: descriptor is writable and revision 1 is the documented descriptor revision.
    let initialized = unsafe {
        InitializeSecurityDescriptor((&mut descriptor as *mut SECURITY_DESCRIPTOR).cast(), 1)
    } != 0;
    // SAFETY: descriptor was initialized, and owner/ACL remain alive through CreateDirectoryW.
    let configured = initialized
        && unsafe {
            SetSecurityDescriptorOwner(
                (&mut descriptor as *mut SECURITY_DESCRIPTOR).cast(),
                owner.sid(),
                0,
            )
        } != 0
        && unsafe {
            SetSecurityDescriptorDacl(
                (&mut descriptor as *mut SECURITY_DESCRIPTOR).cast(),
                1,
                acl,
                0,
            )
        } != 0
        && unsafe {
            SetSecurityDescriptorControl(
                (&mut descriptor as *mut SECURITY_DESCRIPTOR).cast(),
                SE_DACL_PROTECTED,
                SE_DACL_PROTECTED,
            )
        } != 0;
    if !configured {
        // SAFETY: acl was allocated by SetEntriesInAclW.
        unsafe { LocalFree(acl.cast()) };
        return Err(CredentialError::AccessDenied);
    }

    let wide_path = path
        .as_os_str()
        .encode_wide()
        .chain(Some(0))
        .collect::<Vec<_>>();
    let attributes = SECURITY_ATTRIBUTES {
        nLength: std::mem::size_of::<SECURITY_ATTRIBUTES>() as u32,
        lpSecurityDescriptor: (&mut descriptor as *mut SECURITY_DESCRIPTOR).cast(),
        bInheritHandle: 0,
    };
    // SAFETY: path is NUL-terminated and the security descriptor is valid for this call.
    let created = unsafe { CreateDirectoryW(wide_path.as_ptr(), &attributes) } != 0;
    // SAFETY: acl was allocated by SetEntriesInAclW.
    unsafe { LocalFree(acl.cast()) };
    if created {
        Ok(())
    } else {
        Err(CredentialError::FileIo)
    }
}

pub fn create_private_windows_file(path: &Path) -> Result<std::fs::File, CredentialError> {
    use std::os::windows::ffi::OsStrExt;
    use std::os::windows::io::FromRawHandle;
    use windows_sys::Win32::Foundation::{GENERIC_WRITE, INVALID_HANDLE_VALUE, LocalFree};
    use windows_sys::Win32::Security::{
        InitializeSecurityDescriptor, SE_DACL_PROTECTED, SECURITY_ATTRIBUTES, SECURITY_DESCRIPTOR,
        SetSecurityDescriptorControl, SetSecurityDescriptorDacl, SetSecurityDescriptorOwner,
    };
    use windows_sys::Win32::Storage::FileSystem::{
        CREATE_NEW, CreateFileW, FILE_ATTRIBUTE_NORMAL, FILE_FLAG_OPEN_REPARSE_POINT, READ_CONTROL,
        WRITE_DAC,
    };

    let (acl, mut owner) = private_acl(false)?;
    // SAFETY: zero is a valid pre-initialization state for SECURITY_DESCRIPTOR.
    let mut descriptor: SECURITY_DESCRIPTOR = unsafe { std::mem::zeroed() };
    // SAFETY: descriptor is writable and revision 1 is the documented descriptor revision.
    let initialized = unsafe {
        InitializeSecurityDescriptor((&mut descriptor as *mut SECURITY_DESCRIPTOR).cast(), 1)
    } != 0;
    // SAFETY: descriptor was initialized, and owner/ACL remain alive through CreateFileW.
    let configured = initialized
        && unsafe {
            SetSecurityDescriptorOwner(
                (&mut descriptor as *mut SECURITY_DESCRIPTOR).cast(),
                owner.sid(),
                0,
            )
        } != 0
        && unsafe {
            SetSecurityDescriptorDacl(
                (&mut descriptor as *mut SECURITY_DESCRIPTOR).cast(),
                1,
                acl,
                0,
            )
        } != 0
        && unsafe {
            SetSecurityDescriptorControl(
                (&mut descriptor as *mut SECURITY_DESCRIPTOR).cast(),
                SE_DACL_PROTECTED,
                SE_DACL_PROTECTED,
            )
        } != 0;
    if !configured {
        // SAFETY: acl was allocated by SetEntriesInAclW.
        unsafe { LocalFree(acl.cast()) };
        return Err(CredentialError::AccessDenied);
    }

    let wide_path = path
        .as_os_str()
        .encode_wide()
        .chain(Some(0))
        .collect::<Vec<_>>();
    let attributes = SECURITY_ATTRIBUTES {
        nLength: std::mem::size_of::<SECURITY_ATTRIBUTES>() as u32,
        lpSecurityDescriptor: (&mut descriptor as *mut SECURITY_DESCRIPTOR).cast(),
        bInheritHandle: 0,
    };
    // SAFETY: path is NUL-terminated and the security descriptor remains valid for the call.
    let handle = unsafe {
        CreateFileW(
            wide_path.as_ptr(),
            GENERIC_WRITE | READ_CONTROL | WRITE_DAC,
            0,
            &attributes,
            CREATE_NEW,
            FILE_ATTRIBUTE_NORMAL | FILE_FLAG_OPEN_REPARSE_POINT,
            std::ptr::null_mut(),
        )
    };
    // SAFETY: acl was allocated by SetEntriesInAclW.
    unsafe { LocalFree(acl.cast()) };
    if handle == INVALID_HANDLE_VALUE {
        return Err(CredentialError::FileIo);
    }
    // SAFETY: CreateFileW returned a new owned handle.
    Ok(unsafe { std::fs::File::from_raw_handle(handle.cast()) })
}

pub fn windows_handle_acl_is_private(
    handle: std::os::windows::io::RawHandle,
) -> Result<bool, CredentialError> {
    handle_acl_is_private(handle.cast())
}

pub(crate) fn ensure_private_handle_acl(
    handle: windows_sys::Win32::Foundation::HANDLE,
    directory: bool,
) -> Result<(), CredentialError> {
    use windows_sys::Win32::Security::Authorization::{SE_FILE_OBJECT, SetSecurityInfo};
    let (acl, mut owner) = private_acl(directory)?;
    set_private_acl(
        |security, dacl| {
            // SAFETY: handle is held open and pointers remain live for the call.
            unsafe {
                SetSecurityInfo(
                    handle,
                    SE_FILE_OBJECT,
                    security,
                    std::ptr::null_mut(),
                    std::ptr::null_mut(),
                    dacl,
                    std::ptr::null(),
                )
            }
        },
        acl,
        &mut owner,
    )
}

fn private_acl(
    directory: bool,
) -> Result<(*mut windows_sys::Win32::Security::ACL, CurrentUserSid), CredentialError> {
    use std::ptr::{null, null_mut};
    use windows_sys::Win32::Foundation::ERROR_SUCCESS;
    use windows_sys::Win32::Security::Authorization::{
        EXPLICIT_ACCESS_W, SET_ACCESS, SetEntriesInAclW, TRUSTEE_IS_SID, TRUSTEE_IS_USER, TRUSTEE_W,
    };
    use windows_sys::Win32::Security::{NO_INHERITANCE, SUB_CONTAINERS_AND_OBJECTS_INHERIT};
    use windows_sys::Win32::Storage::FileSystem::FILE_ALL_ACCESS;

    let mut owner = CurrentUserSid::new()?;
    let access = EXPLICIT_ACCESS_W {
        grfAccessPermissions: FILE_ALL_ACCESS,
        grfAccessMode: SET_ACCESS,
        grfInheritance: if directory {
            SUB_CONTAINERS_AND_OBJECTS_INHERIT
        } else {
            NO_INHERITANCE
        },
        Trustee: TRUSTEE_W {
            pMultipleTrustee: null_mut(),
            MultipleTrusteeOperation: 0,
            TrusteeForm: TRUSTEE_IS_SID,
            TrusteeType: TRUSTEE_IS_USER,
            ptstrName: owner.sid().cast(),
        },
    };
    let mut acl = null_mut();
    // SAFETY: access and acl are valid for the call; no existing ACL is merged.
    if unsafe { SetEntriesInAclW(1, &access, null(), &mut acl) } != ERROR_SUCCESS {
        return Err(CredentialError::AccessDenied);
    }
    Ok((acl, owner))
}

fn set_private_acl(
    setter: impl FnOnce(
        windows_sys::Win32::Security::OBJECT_SECURITY_INFORMATION,
        *mut windows_sys::Win32::Security::ACL,
    ) -> windows_sys::Win32::Foundation::WIN32_ERROR,
    acl: *mut windows_sys::Win32::Security::ACL,
    _owner: &mut CurrentUserSid,
) -> Result<(), CredentialError> {
    use windows_sys::Win32::Foundation::{ERROR_SUCCESS, LocalFree};
    use windows_sys::Win32::Security::{
        DACL_SECURITY_INFORMATION, PROTECTED_DACL_SECURITY_INFORMATION,
    };
    let status = setter(
        DACL_SECURITY_INFORMATION | PROTECTED_DACL_SECURITY_INFORMATION,
        acl,
    );
    // SAFETY: acl was allocated by the Win32 local allocator.
    unsafe { LocalFree(acl.cast()) };
    if status == ERROR_SUCCESS {
        Ok(())
    } else {
        Err(CredentialError::AccessDenied)
    }
}

pub(crate) fn handle_acl_is_private(
    handle: windows_sys::Win32::Foundation::HANDLE,
) -> Result<bool, CredentialError> {
    use std::ptr::null_mut;
    use windows_sys::Win32::Foundation::{ERROR_SUCCESS, LocalFree};
    use windows_sys::Win32::Security::Authorization::{
        GRANT_ACCESS, GetExplicitEntriesFromAclW, GetSecurityInfo, SE_FILE_OBJECT, SET_ACCESS,
        TRUSTEE_IS_SID,
    };
    use windows_sys::Win32::Security::{
        DACL_SECURITY_INFORMATION, EqualSid, GetSecurityDescriptorControl,
        OWNER_SECURITY_INFORMATION, SE_DACL_PROTECTED,
    };
    use windows_sys::Win32::Storage::FileSystem::FILE_ALL_ACCESS;

    let mut current_user = CurrentUserSid::new()?;
    let mut object_owner = null_mut();
    let mut dacl = null_mut();
    let mut descriptor = null_mut();
    // SAFETY: out-pointers are valid and descriptor is released below.
    let status = unsafe {
        GetSecurityInfo(
            handle,
            SE_FILE_OBJECT,
            OWNER_SECURITY_INFORMATION | DACL_SECURITY_INFORMATION,
            &mut object_owner,
            null_mut(),
            &mut dacl,
            null_mut(),
            &mut descriptor,
        )
    };
    if status != ERROR_SUCCESS {
        return Err(CredentialError::AccessDenied);
    }

    let mut control = 0;
    let mut revision = 0;
    // SAFETY: descriptor was returned by GetSecurityInfo.
    let protected =
        unsafe { GetSecurityDescriptorControl(descriptor, &mut control, &mut revision) } != 0
            && control & SE_DACL_PROTECTED != 0;
    // SAFETY: both SIDs are valid while descriptor/current_user are alive.
    let owner_matches = unsafe { EqualSid(current_user.sid(), object_owner) } != 0;

    let mut count = 0;
    let mut entries = null_mut();
    // SAFETY: dacl belongs to descriptor and output pointers are valid.
    let entries_status = unsafe { GetExplicitEntriesFromAclW(dacl, &mut count, &mut entries) };
    let only_current_user = if entries_status == ERROR_SUCCESS && count == 1 {
        // SAFETY: one EXPLICIT_ACCESS_W entry was returned.
        let entry = unsafe { &*entries };
        entry.Trustee.TrusteeForm == TRUSTEE_IS_SID
            && matches!(entry.grfAccessMode, SET_ACCESS | GRANT_ACCESS)
            && entry.grfAccessPermissions == FILE_ALL_ACCESS
            // SAFETY: TrusteeForm confirms ptstrName is a SID.
            && unsafe { EqualSid(current_user.sid(), entry.Trustee.ptstrName.cast()) } != 0
    } else {
        false
    };
    if !entries.is_null() {
        // SAFETY: entries was allocated by GetExplicitEntriesFromAclW.
        unsafe { LocalFree(entries.cast()) };
    }
    // SAFETY: descriptor was allocated by GetSecurityInfo.
    unsafe { LocalFree(descriptor.cast()) };
    Ok(protected && owner_matches && only_current_user)
}

pub(crate) fn handle_owned_by_current_user(
    handle: windows_sys::Win32::Foundation::HANDLE,
) -> Result<bool, CredentialError> {
    use std::ptr::null_mut;
    use windows_sys::Win32::Foundation::{ERROR_SUCCESS, LocalFree};
    use windows_sys::Win32::Security::Authorization::{GetSecurityInfo, SE_FILE_OBJECT};
    use windows_sys::Win32::Security::{EqualSid, OWNER_SECURITY_INFORMATION};
    let mut owner = CurrentUserSid::new()?;
    let mut object_owner = null_mut();
    let mut descriptor = null_mut();
    // SAFETY: out-pointers are valid and descriptor is released below.
    let status = unsafe {
        GetSecurityInfo(
            handle,
            SE_FILE_OBJECT,
            OWNER_SECURITY_INFORMATION,
            &mut object_owner,
            null_mut(),
            null_mut(),
            null_mut(),
            &mut descriptor,
        )
    };
    if status != ERROR_SUCCESS {
        return Err(CredentialError::AccessDenied);
    }
    // SAFETY: both SIDs are valid for the duration of the comparison.
    let matches = unsafe { EqualSid(owner.sid(), object_owner) } != 0;
    // SAFETY: descriptor was allocated by GetSecurityInfo.
    unsafe { LocalFree(descriptor.cast()) };
    Ok(matches)
}

struct CurrentUserSid {
    token: windows_sys::Win32::Foundation::HANDLE,
    buffer: Vec<u8>,
}

impl CurrentUserSid {
    fn new() -> Result<Self, CredentialError> {
        use std::ffi::c_void;
        use std::ptr::null_mut;
        use windows_sys::Win32::Foundation::{ERROR_INSUFFICIENT_BUFFER, GetLastError};
        use windows_sys::Win32::Security::{GetTokenInformation, TOKEN_QUERY, TokenUser};
        use windows_sys::Win32::System::Threading::{GetCurrentProcess, OpenProcessToken};
        let mut token = null_mut();
        // SAFETY: token is an out-pointer and GetCurrentProcess returns a pseudo-handle.
        if unsafe { OpenProcessToken(GetCurrentProcess(), TOKEN_QUERY, &mut token) } == 0 {
            return Err(CredentialError::AccessDenied);
        }
        let mut required = 0;
        // SAFETY: zero-length query obtains the required TOKEN_USER size.
        unsafe { GetTokenInformation(token, TokenUser, null_mut(), 0, &mut required) };
        if unsafe { GetLastError() } != ERROR_INSUFFICIENT_BUFFER || required == 0 {
            // SAFETY: token is a valid handle.
            unsafe { windows_sys::Win32::Foundation::CloseHandle(token) };
            return Err(CredentialError::AccessDenied);
        }
        let mut buffer = vec![0_u8; required as usize];
        // SAFETY: buffer is writable for required bytes.
        if unsafe {
            GetTokenInformation(
                token,
                TokenUser,
                buffer.as_mut_ptr().cast::<c_void>(),
                required,
                &mut required,
            )
        } == 0
        {
            // SAFETY: token is a valid handle.
            unsafe { windows_sys::Win32::Foundation::CloseHandle(token) };
            return Err(CredentialError::AccessDenied);
        }
        Ok(Self { token, buffer })
    }

    fn sid(&mut self) -> windows_sys::Win32::Security::PSID {
        use windows_sys::Win32::Security::TOKEN_USER;
        // SAFETY: GetTokenInformation initialized buffer as TOKEN_USER.
        unsafe { (*(self.buffer.as_mut_ptr().cast::<TOKEN_USER>())).User.Sid }
    }
}

impl Drop for CurrentUserSid {
    fn drop(&mut self) {
        // SAFETY: token is a valid handle owned by this value.
        unsafe { windows_sys::Win32::Foundation::CloseHandle(self.token) };
    }
}
