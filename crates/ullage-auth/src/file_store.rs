use std::fmt;
use std::io::{Read, Write};
use std::path::{Component, Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};

use cap_fs_ext::{DirExt, FollowSymlinks, OpenOptionsFollowExt};
use cap_std::ambient_authority;
use cap_std::fs::{Dir, File, OpenOptions};
use sha2::{Digest, Sha256};
use zeroize::Zeroize;

use crate::credential::{MAX_RECORD_BYTES, hex};
use crate::{
    Availability, BackendKind, BackendScope, CredentialBackend, CredentialError, CredentialKey,
};

const TEMP_CREATE_ATTEMPTS: usize = 16;
const MAX_RECORD_BYTES_U64: u64 = MAX_RECORD_BYTES as u64;
static TEMP_SEQUENCE: AtomicU64 = AtomicU64::new(0);

pub struct FileFallbackOptions {
    directory: PathBuf,
}

impl FileFallbackOptions {
    /// Explicitly opts into plaintext-on-disk fallback storage.
    pub fn new(directory: impl Into<PathBuf>) -> Result<Self, CredentialError> {
        let directory = directory.into();
        if !directory.is_absolute() {
            return Err(CredentialError::UnsafeFallbackPath);
        }
        Ok(Self { directory })
    }
}

impl fmt::Debug for FileFallbackOptions {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter
            .debug_struct("FileFallbackOptions")
            .field("directory", &"[LOCAL PATH REDACTED]")
            .finish()
    }
}

pub struct FileStore {
    directory: Dir,
    coordination_scope: BackendScope,
}

impl FileStore {
    pub fn new(options: FileFallbackOptions) -> Result<Self, CredentialError> {
        let directory = open_private_directory(&options.directory)?;
        let coordination_scope = file_coordination_scope(&directory)?;
        sweep_stale_temp_files(&directory);
        Ok(Self {
            directory,
            coordination_scope,
        })
    }

    fn credential_name(key: &CredentialKey) -> String {
        format!("{}.credential", hex(&Sha256::digest(key.stable_bytes())))
    }

    fn temp_name() -> String {
        let sequence = TEMP_SEQUENCE.fetch_add(1, Ordering::Relaxed);
        format!(".ullage-{}-{sequence}.tmp", std::process::id())
    }

    fn open_private_file(&self, name: &str) -> Result<File, CredentialError> {
        let mut options = OpenOptions::new();
        options.read(true).follow(FollowSymlinks::No);
        add_windows_dacl_access(&mut options, false);
        let file = self
            .directory
            .open_with(name, &options)
            .map_err(map_read_error)?;
        verify_file_handle(&file)?;
        protect_file_handle(&file)?;
        Ok(file)
    }

    fn create_temporary_file(&self) -> Result<(String, File), CredentialError> {
        for _ in 0..TEMP_CREATE_ATTEMPTS {
            let name = Self::temp_name();
            let mut options = OpenOptions::new();
            options
                .write(true)
                .create_new(true)
                .follow(FollowSymlinks::No);
            set_create_mode(&mut options);
            add_windows_dacl_access(&mut options, true);
            // create_new reports collisions as AlreadyExists, which covers
            // stale temp files, other processes' in-flight names, and a
            // planted symlink; each attempt gets a fresh unique name.
            let file = match self.directory.open_with(&name, &options) {
                Ok(file) => file,
                Err(error) if error.kind() == std::io::ErrorKind::AlreadyExists => continue,
                Err(error) => return Err(map_open_error(error)),
            };
            verify_file_handle(&file)?;
            protect_file_handle(&file)?;
            return Ok((name, file));
        }
        Err(CredentialError::FileIo)
    }

    fn sync_directory(&self) -> Result<(), CredentialError> {
        sync_directory_handle(&self.directory)
    }
}

#[cfg(unix)]
fn sync_directory_handle(directory: &Dir) -> Result<(), CredentialError> {
    use std::os::fd::{AsRawFd, FromRawFd};
    let name = b".\0";
    // SAFETY: directory is a live directory descriptor and name is NUL-terminated.
    let descriptor = unsafe {
        libc::openat(
            directory.as_raw_fd(),
            name.as_ptr().cast(),
            libc::O_RDONLY | libc::O_DIRECTORY | libc::O_CLOEXEC,
        )
    };
    if descriptor < 0 {
        return Err(CredentialError::FileIo);
    }
    // SAFETY: descriptor is newly owned and valid.
    let file = unsafe { std::fs::File::from_raw_fd(descriptor) };
    file.sync_all().map_err(|_| CredentialError::FileIo)
}

#[cfg(windows)]
fn sync_directory_handle(directory: &Dir) -> Result<(), CredentialError> {
    use std::os::windows::io::AsRawHandle;
    use windows_sys::Win32::Storage::FileSystem::FlushFileBuffers;
    // SAFETY: directory is a live handle; FlushFileBuffers has no pointer arguments.
    if unsafe { FlushFileBuffers(directory.as_raw_handle().cast()) } == 0 {
        return Err(CredentialError::FileIo);
    }
    Ok(())
}

#[cfg(not(any(unix, windows)))]
fn sync_directory_handle(_directory: &Dir) -> Result<(), CredentialError> {
    Ok(())
}

impl fmt::Debug for FileStore {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter
            .debug_struct("FileStore")
            .field("directory", &"[FIXED PRIVATE HANDLE]")
            .finish()
    }
}

impl CredentialBackend for FileStore {
    fn kind(&self) -> BackendKind {
        BackendKind::ExplicitFileFallback
    }

    fn coordination_scope(&self) -> BackendScope {
        self.coordination_scope
    }

    fn probe(&self) -> Result<Availability, CredentialError> {
        verify_directory_handle(&self.directory)?;
        let (name, mut file) = self.create_temporary_file()?;
        let probe_result = file
            .write_all(b"probe")
            .and_then(|()| file.sync_all())
            .map_err(|_| CredentialError::FileIo);
        drop(file);
        let cleanup_result = self
            .directory
            .remove_file(&name)
            .map_err(|_| CredentialError::FileIo);
        probe_result?;
        cleanup_result?;
        Ok(Availability::Available)
    }

    fn read(&self, key: &CredentialKey) -> Result<Vec<u8>, CredentialError> {
        verify_directory_handle(&self.directory)?;
        let name = Self::credential_name(key);
        let file = self.open_private_file(&name)?;
        let metadata = file.metadata().map_err(|_| CredentialError::FileIo)?;
        if metadata.len() > MAX_RECORD_BYTES_U64 {
            return Err(CredentialError::CredentialTooLarge);
        }
        let mut content = Vec::with_capacity(metadata.len() as usize);
        file.take(MAX_RECORD_BYTES_U64 + 1)
            .read_to_end(&mut content)
            .map_err(|_| CredentialError::FileIo)?;
        if content.len() as u64 > MAX_RECORD_BYTES_U64 {
            content.zeroize();
            return Err(CredentialError::CredentialTooLarge);
        }
        Ok(content)
    }

    fn write(&self, key: &CredentialKey, value: &[u8]) -> Result<(), CredentialError> {
        verify_directory_handle(&self.directory)?;
        if value.len() > MAX_RECORD_BYTES {
            return Err(CredentialError::CredentialTooLarge);
        }
        let destination = Self::credential_name(key);
        if let Ok(metadata) = self.directory.symlink_metadata(&destination) {
            if !metadata.is_file() || capability_metadata_is_reparse_or_symlink(&metadata) {
                return Err(CredentialError::UnsafeFallbackPath);
            }
        }

        let (temporary, mut file) = self.create_temporary_file()?;
        let write_result = (|| {
            file.write_all(value).map_err(|_| CredentialError::FileIo)?;
            file.sync_all().map_err(|_| CredentialError::FileIo)?;
            drop(file);
            self.directory
                .rename(&temporary, &self.directory, &destination)
                .map_err(|_| CredentialError::FileIo)?;
            self.sync_directory()
        })();
        if write_result.is_err() {
            let _ = self.directory.remove_file(&temporary);
        }
        write_result
    }
}

fn open_private_directory(path: &Path) -> Result<Dir, CredentialError> {
    let mut anchor = PathBuf::new();
    let mut names = Vec::new();
    for component in path.components() {
        match component {
            Component::RootDir | Component::Prefix(_) => anchor.push(component.as_os_str()),
            Component::Normal(name) => names.push(name.to_os_string()),
            Component::CurDir | Component::ParentDir => {
                return Err(CredentialError::UnsafeFallbackPath);
            }
        }
    }
    if anchor.as_os_str().is_empty() || names.is_empty() {
        return Err(CredentialError::UnsafeFallbackPath);
    }

    let mut current = Dir::open_ambient_dir(&anchor, ambient_authority())
        .map_err(|_| CredentialError::UnsafeFallbackPath)?;
    for (index, name) in names.iter().enumerate() {
        let is_leaf = index + 1 == names.len();
        current = match current.open_dir_nofollow(name) {
            Ok(directory) => directory,
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => {
                run_directory_create_hook(name);
                create_directory_from_handle(&current, name, path, is_leaf)?;
                current.open_dir_nofollow(name).map_err(|error| {
                    if error.kind() == std::io::ErrorKind::PermissionDenied {
                        CredentialError::AccessDenied
                    } else {
                        CredentialError::UnsafeFallbackPath
                    }
                })?
            }
            Err(error) if error.kind() == std::io::ErrorKind::PermissionDenied => {
                return Err(CredentialError::AccessDenied);
            }
            Err(_) => return Err(CredentialError::UnsafeFallbackPath),
        };
    }
    // A pre-existing vault directory with the wrong permissions is refused on
    // every platform instead of silently repaired; only directories this
    // process creates get private modes or ACLs.
    verify_directory_handle(&current)?;
    protect_directory_handle(&current)?;
    Ok(current)
}

#[cfg(unix)]
fn create_directory_from_handle(
    parent: &Dir,
    name: &std::ffi::OsStr,
    _full_path: &Path,
    _is_leaf: bool,
) -> Result<(), CredentialError> {
    use cap_std::fs::DirBuilderExt;
    let mut builder = cap_std::fs::DirBuilder::new();
    builder.mode(0o700);
    match parent.create_dir_with(name, &builder) {
        Ok(()) => Ok(()),
        Err(error) if error.kind() == std::io::ErrorKind::AlreadyExists => Ok(()),
        Err(_) => Err(CredentialError::FileIo),
    }
}

#[cfg(windows)]
fn create_directory_from_handle(
    _parent: &Dir,
    _name: &std::ffi::OsStr,
    full_path: &Path,
    is_leaf: bool,
) -> Result<(), CredentialError> {
    if !is_leaf {
        return Err(CredentialError::FileIo);
    }
    create_private_windows_directory(full_path)
}

#[cfg(not(any(unix, windows)))]
fn create_directory_from_handle(
    _parent: &Dir,
    _name: &std::ffi::OsStr,
    _full_path: &Path,
    _is_leaf: bool,
) -> Result<(), CredentialError> {
    Err(CredentialError::UnsafeFallbackPath)
}

#[cfg(all(test, unix))]
type DirectoryCreateHook = (
    std::ffi::OsString,
    std::sync::Arc<std::sync::Barrier>,
    std::sync::Arc<std::sync::Barrier>,
);

#[cfg(all(test, unix))]
static DIRECTORY_CREATE_HOOK: std::sync::Mutex<Option<DirectoryCreateHook>> =
    std::sync::Mutex::new(None);

#[cfg(all(test, unix))]
pub(crate) struct DirectoryCreateHookGuard;

#[cfg(all(test, unix))]
impl DirectoryCreateHookGuard {
    /// Installs the hook and returns a guard; dropping the guard clears the
    /// hook even when the test panics. The mutex is only held for the
    /// install/clear writes so `run_directory_create_hook` never blocks.
    pub(crate) fn install(hook: DirectoryCreateHook) -> Self {
        *DIRECTORY_CREATE_HOOK
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner) = Some(hook);
        Self
    }
}

#[cfg(all(test, unix))]
impl Drop for DirectoryCreateHookGuard {
    fn drop(&mut self) {
        *DIRECTORY_CREATE_HOOK
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner) = None;
    }
}

#[cfg(all(test, unix))]
fn run_directory_create_hook(name: &std::ffi::OsStr) {
    let hook = DIRECTORY_CREATE_HOOK
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner)
        .clone();
    if let Some((target, entered, release)) = hook {
        if target == name {
            entered.wait();
            release.wait();
        }
    }
}

#[cfg(not(all(test, unix)))]
fn run_directory_create_hook(_name: &std::ffi::OsStr) {}

#[cfg(unix)]
fn set_create_mode(options: &mut OpenOptions) {
    use cap_std::fs::OpenOptionsExt;
    options.mode(0o600);
}

#[cfg(not(unix))]
fn set_create_mode(_options: &mut OpenOptions) {}

#[cfg(windows)]
fn add_windows_dacl_access(options: &mut OpenOptions, writable: bool) {
    use cap_std::fs::OpenOptionsExt;
    use windows_sys::Win32::Foundation::{GENERIC_READ, GENERIC_WRITE};
    use windows_sys::Win32::Storage::FileSystem::WRITE_DAC;
    let data_access = if writable {
        GENERIC_WRITE
    } else {
        GENERIC_READ
    };
    options.access_mode(data_access | WRITE_DAC);
}

#[cfg(not(windows))]
fn add_windows_dacl_access(_options: &mut OpenOptions, _writable: bool) {}

#[cfg(unix)]
fn verify_directory_handle(directory: &Dir) -> Result<(), CredentialError> {
    use cap_std::fs::MetadataExt;
    let metadata = directory
        .dir_metadata()
        .map_err(|_| CredentialError::UnsafeFallbackPath)?;
    // SAFETY: geteuid has no preconditions.
    if !metadata.is_dir()
        || metadata.uid() != unsafe { libc::geteuid() }
        || metadata.mode() & 0o077 != 0
    {
        return Err(CredentialError::UnsafeFallbackPath);
    }
    Ok(())
}

#[cfg(windows)]
fn verify_directory_handle(directory: &Dir) -> Result<(), CredentialError> {
    use cap_fs_ext::OsMetadataExt;
    use std::os::windows::io::AsRawHandle;
    let metadata = directory
        .dir_metadata()
        .map_err(|_| CredentialError::UnsafeFallbackPath)?;
    // Reparse points (junctions, symlinks) redirect the handle elsewhere and
    // are rejected just like verify_file_handle rejects them for records.
    if !metadata.is_dir()
        || !windows_file_attributes_are_safe(metadata.file_attributes())
        || !handle_acl_is_private(directory.as_raw_handle().cast())?
    {
        return Err(CredentialError::UnsafeFallbackPath);
    }
    Ok(())
}

#[cfg(not(any(unix, windows)))]
fn verify_directory_handle(_directory: &Dir) -> Result<(), CredentialError> {
    Err(CredentialError::UnsafeFallbackPath)
}

#[cfg(unix)]
fn verify_file_handle(file: &File) -> Result<(), CredentialError> {
    use cap_std::fs::MetadataExt;
    let metadata = file.metadata().map_err(|_| CredentialError::FileIo)?;
    // SAFETY: geteuid has no preconditions.
    if !metadata.is_file()
        || metadata.uid() != unsafe { libc::geteuid() }
        || metadata.mode() & 0o077 != 0
    {
        return Err(CredentialError::UnsafeFallbackPath);
    }
    Ok(())
}

#[cfg(windows)]
fn verify_file_handle(file: &File) -> Result<(), CredentialError> {
    use cap_fs_ext::OsMetadataExt;
    use std::os::windows::io::AsRawHandle;
    let metadata = file.metadata().map_err(|_| CredentialError::FileIo)?;
    if !metadata.is_file()
        || !windows_file_attributes_are_safe(metadata.file_attributes())
        || !handle_owned_by_current_user(file.as_raw_handle().cast())?
    {
        return Err(CredentialError::UnsafeFallbackPath);
    }
    Ok(())
}

#[cfg(windows)]
pub(crate) fn windows_file_attributes_are_safe(attributes: u32) -> bool {
    const FILE_ATTRIBUTE_REPARSE_POINT: u32 = 0x400;
    attributes & FILE_ATTRIBUTE_REPARSE_POINT == 0
}

#[cfg(not(any(unix, windows)))]
fn verify_file_handle(_file: &File) -> Result<(), CredentialError> {
    Err(CredentialError::UnsafeFallbackPath)
}

#[cfg(unix)]
fn protect_directory_handle(_directory: &Dir) -> Result<(), CredentialError> {
    Ok(())
}

#[cfg(windows)]
fn protect_directory_handle(directory: &Dir) -> Result<(), CredentialError> {
    use std::os::windows::io::AsRawHandle;
    if handle_acl_is_private(directory.as_raw_handle().cast())? {
        Ok(())
    } else {
        Err(CredentialError::UnsafeFallbackPath)
    }
}

#[cfg(not(any(unix, windows)))]
fn protect_directory_handle(_directory: &Dir) -> Result<(), CredentialError> {
    Err(CredentialError::UnsafeFallbackPath)
}

#[cfg(unix)]
fn protect_file_handle(_file: &File) -> Result<(), CredentialError> {
    Ok(())
}

#[cfg(windows)]
fn protect_file_handle(file: &File) -> Result<(), CredentialError> {
    use std::os::windows::io::AsRawHandle;
    let handle = file.as_raw_handle().cast();
    ensure_private_handle_acl(handle, false)?;
    if handle_acl_is_private(handle)? {
        Ok(())
    } else {
        Err(CredentialError::UnsafeFallbackPath)
    }
}

#[cfg(not(any(unix, windows)))]
fn protect_file_handle(_file: &File) -> Result<(), CredentialError> {
    Err(CredentialError::UnsafeFallbackPath)
}

#[cfg(windows)]
fn capability_metadata_is_reparse_or_symlink(metadata: &cap_std::fs::Metadata) -> bool {
    use cap_fs_ext::OsMetadataExt;
    metadata.is_symlink() || !windows_file_attributes_are_safe(metadata.file_attributes())
}

#[cfg(not(windows))]
fn capability_metadata_is_reparse_or_symlink(metadata: &cap_std::fs::Metadata) -> bool {
    metadata.is_symlink()
}

fn map_read_error(error: std::io::Error) -> CredentialError {
    match error.kind() {
        std::io::ErrorKind::NotFound => CredentialError::NotFound,
        std::io::ErrorKind::PermissionDenied => CredentialError::AccessDenied,
        _ => {
            // A no-follow open that hits a symlink surfaces as ELOOP on unix.
            #[cfg(unix)]
            if error.raw_os_error() == Some(libc::ELOOP) {
                return CredentialError::UnsafeFallbackPath;
            }
            CredentialError::FileIo
        }
    }
}

fn map_open_error(error: std::io::Error) -> CredentialError {
    if error.kind() == std::io::ErrorKind::PermissionDenied {
        CredentialError::AccessDenied
    } else {
        CredentialError::FileIo
    }
}

/// Removes leftover `.ullage-<pid>-*.tmp` files whose pid is not this process.
/// A same-pid collision is impossible while this process holds its pid, so
/// only files from dead processes (or another live vault user) are removed;
/// for the latter the concurrent write simply fails cleanly.
fn sweep_stale_temp_files(directory: &Dir) {
    let own_pid = std::process::id();
    let Ok(entries) = directory.entries() else {
        return;
    };
    for entry in entries.flatten() {
        let name = entry.file_name();
        let Some(name) = name.to_str() else {
            continue;
        };
        let Some(rest) = name.strip_prefix(".ullage-") else {
            continue;
        };
        let Some((pid, _)) = rest.split_once('-') else {
            continue;
        };
        if pid.parse::<u32>() == Ok(own_pid) {
            continue;
        }
        let _ = directory.remove_file(entry.file_name());
    }
}

#[cfg(unix)]
fn file_coordination_scope(directory: &Dir) -> Result<BackendScope, CredentialError> {
    use cap_std::fs::MetadataExt;
    let metadata = directory
        .dir_metadata()
        .map_err(|_| CredentialError::UnsafeFallbackPath)?;
    let mut identity = b"ullage-file-vault\0unix\0".to_vec();
    identity.extend_from_slice(&metadata.dev().to_le_bytes());
    identity.extend_from_slice(&metadata.ino().to_le_bytes());
    Ok(BackendScope::new(&identity))
}

#[cfg(windows)]
fn file_coordination_scope(directory: &Dir) -> Result<BackendScope, CredentialError> {
    use std::os::windows::io::AsRawHandle;
    use windows_sys::Win32::Storage::FileSystem::{
        BY_HANDLE_FILE_INFORMATION, GetFileInformationByHandle,
    };
    // SAFETY: zero is a valid output buffer initialization.
    let mut information: BY_HANDLE_FILE_INFORMATION = unsafe { std::mem::zeroed() };
    // SAFETY: the directory handle and output pointer remain valid for the call.
    if unsafe { GetFileInformationByHandle(directory.as_raw_handle().cast(), &mut information) }
        == 0
    {
        return Err(CredentialError::UnsafeFallbackPath);
    }
    let mut identity = b"ullage-file-vault\0windows\0".to_vec();
    identity.extend_from_slice(&information.dwVolumeSerialNumber.to_le_bytes());
    identity.extend_from_slice(&information.nFileIndexHigh.to_le_bytes());
    identity.extend_from_slice(&information.nFileIndexLow.to_le_bytes());
    Ok(BackendScope::new(&identity))
}

#[cfg(not(any(unix, windows)))]
fn file_coordination_scope(_directory: &Dir) -> Result<BackendScope, CredentialError> {
    Err(CredentialError::UnsafeFallbackPath)
}

#[cfg(windows)]
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

#[cfg(windows)]
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

#[cfg(windows)]
pub fn windows_handle_acl_is_private(
    handle: std::os::windows::io::RawHandle,
) -> Result<bool, CredentialError> {
    handle_acl_is_private(handle.cast())
}

#[cfg(windows)]
fn ensure_private_handle_acl(
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

#[cfg(windows)]
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

#[cfg(windows)]
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

#[cfg(windows)]
fn handle_acl_is_private(
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

#[cfg(windows)]
fn handle_owned_by_current_user(
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

#[cfg(windows)]
struct CurrentUserSid {
    token: windows_sys::Win32::Foundation::HANDLE,
    buffer: Vec<u8>,
}

#[cfg(windows)]
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

#[cfg(windows)]
impl Drop for CurrentUserSid {
    fn drop(&mut self) {
        // SAFETY: token is a valid handle owned by this value.
        unsafe { windows_sys::Win32::Foundation::CloseHandle(self.token) };
    }
}
