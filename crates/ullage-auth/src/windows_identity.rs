use sha2::{Digest, Sha256};

fn scope_from_sid_bytes(sid: &[u8]) -> String {
    let digest = Sha256::digest(sid);
    let mut scope = String::with_capacity(digest.len() * 2);
    for byte in digest {
        use std::fmt::Write;
        write!(&mut scope, "{byte:02x}").expect("writing to a String cannot fail");
    }
    scope
}

#[cfg(windows)]
pub fn new_windows_service_nonce() -> Result<String, std::io::Error> {
    use windows_sys::Win32::Security::Cryptography::{
        BCRYPT_USE_SYSTEM_PREFERRED_RNG, BCryptGenRandom,
    };

    let mut nonce = [0u8; 16];
    let status = unsafe {
        BCryptGenRandom(
            std::ptr::null_mut(),
            nonce.as_mut_ptr(),
            nonce.len() as u32,
            BCRYPT_USE_SYSTEM_PREFERRED_RNG,
        )
    };
    if status != 0 {
        return Err(std::io::Error::other(
            "Windows random number generation failed",
        ));
    }
    let mut encoded = String::with_capacity(nonce.len() * 2);
    for byte in nonce {
        use std::fmt::Write;
        write!(&mut encoded, "{byte:02x}").expect("writing to a String cannot fail");
    }
    Ok(encoded)
}

#[cfg(windows)]
pub fn current_windows_user_scope() -> Result<String, std::io::Error> {
    use std::ptr::null_mut;
    use windows_sys::Win32::Foundation::{CloseHandle, GetLastError, HANDLE};
    use windows_sys::Win32::Security::{
        GetLengthSid, GetTokenInformation, IsValidSid, TOKEN_QUERY, TOKEN_USER, TokenUser,
    };
    use windows_sys::Win32::System::Threading::{GetCurrentProcess, OpenProcessToken};

    struct Token(HANDLE);
    impl Drop for Token {
        fn drop(&mut self) {
            unsafe { CloseHandle(self.0) };
        }
    }

    let mut token = null_mut();
    if unsafe { OpenProcessToken(GetCurrentProcess(), TOKEN_QUERY, &mut token) } == 0 {
        return Err(std::io::Error::last_os_error());
    }
    let token = Token(token);
    let mut required = 0;
    // SAFETY: zero-length query obtains the required TOKEN_USER size; it must
    // fail with ERROR_INSUFFICIENT_BUFFER.
    let queried = unsafe { GetTokenInformation(token.0, TokenUser, null_mut(), 0, &mut required) };
    if queried != 0 || required == 0 {
        return Err(std::io::Error::new(
            std::io::ErrorKind::InvalidData,
            "TokenUser query did not report a buffer size",
        ));
    }
    if unsafe { GetLastError() } != windows_sys::Win32::Foundation::ERROR_INSUFFICIENT_BUFFER {
        return Err(std::io::Error::last_os_error());
    }
    let mut buffer = vec![0u8; required as usize];
    if unsafe {
        GetTokenInformation(
            token.0,
            TokenUser,
            buffer.as_mut_ptr().cast(),
            required,
            &mut required,
        )
    } == 0
    {
        return Err(std::io::Error::last_os_error());
    }
    let sid = unsafe { (*(buffer.as_ptr().cast::<TOKEN_USER>())).User.Sid };
    if unsafe { IsValidSid(sid) } == 0 {
        return Err(std::io::Error::new(
            std::io::ErrorKind::InvalidData,
            "current user SID is invalid",
        ));
    }
    let length = unsafe { GetLengthSid(sid) } as usize;
    if length == 0 {
        return Err(std::io::Error::new(
            std::io::ErrorKind::InvalidData,
            "current user SID is empty",
        ));
    }
    let bytes = unsafe { std::slice::from_raw_parts(sid.cast::<u8>(), length) };
    Ok(scope_from_sid_bytes(bytes))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn windows_user_scopes_are_stable_and_distinct_for_different_sids() {
        let first_sid = b"S-1-5-21-1000";
        let second_sid = b"S-1-5-21-1001";
        assert_eq!(
            scope_from_sid_bytes(first_sid),
            scope_from_sid_bytes(first_sid)
        );
        assert_ne!(
            scope_from_sid_bytes(first_sid),
            scope_from_sid_bytes(second_sid)
        );
        assert_eq!(scope_from_sid_bytes(first_sid).len(), 64);
    }
}
