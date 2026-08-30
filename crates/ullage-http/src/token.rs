use std::io::{ErrorKind, Read, Write};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};

use base64::Engine;
use base64::engine::general_purpose::URL_SAFE_NO_PAD;

const TOKEN_BYTES: usize = 32;
static TEMP_SEQUENCE: AtomicU64 = AtomicU64::new(0);

pub fn generate_token() -> Result<String, String> {
    let mut bytes = [0_u8; TOKEN_BYTES];
    getrandom::fill(&mut bytes).map_err(|_| "http-token could not be generated".to_owned())?;
    Ok(URL_SAFE_NO_PAD.encode(bytes))
}

pub fn load_or_create_token(path: &Path) -> Result<String, String> {
    match load_token(path) {
        Ok(token) => Ok(token),
        Err(error) if is_missing(&error) => {
            let token = generate_token()?;
            persist_new_token(path, &token)?;
            Ok(token)
        }
        Err(error) => Err(error),
    }
}

pub fn rotate_token(path: &Path) -> Result<String, String> {
    match open_existing(path) {
        Ok(_) => {}
        Err(error) if is_missing(&error) => {}
        Err(error) => return Err(error),
    }
    let token = generate_token()?;
    persist_new_token(path, &token)?;
    Ok(token)
}

pub fn load_token(path: &Path) -> Result<String, String> {
    let mut file = open_existing(path)?;
    let mut bytes = Vec::new();
    file.read_to_end(&mut bytes)
        .map_err(|_| "http-token could not be read".to_owned())?;
    parse_token_bytes(&bytes)
}

pub fn constant_time_eq(left: &[u8], right: &[u8]) -> bool {
    let mut diff = left.len() ^ right.len();
    let longest = left.len().max(right.len());
    let mut index = 0;
    while index < longest {
        let left_byte = left.get(index).copied().unwrap_or(0);
        let right_byte = right.get(index).copied().unwrap_or(0);
        diff |= usize::from(left_byte ^ right_byte);
        index += 1;
    }
    diff == 0
}

fn parse_token_bytes(bytes: &[u8]) -> Result<String, String> {
    let token = std::str::from_utf8(bytes)
        .map_err(|_| "http-token is not a valid token".to_owned())?
        .trim();
    let decoded = URL_SAFE_NO_PAD
        .decode(token.as_bytes())
        .map_err(|_| "http-token is not a valid token".to_owned())?;
    if decoded.len() != TOKEN_BYTES {
        return Err("http-token is not a valid token".into());
    }
    Ok(token.to_owned())
}

fn persist_new_token(path: &Path, token: &str) -> Result<(), String> {
    ensure_private_parent(path)?;
    let parent = path
        .parent()
        .ok_or_else(|| "http-token path has no parent directory".to_owned())?;
    let file_name = path
        .file_name()
        .and_then(|name| name.to_str())
        .ok_or_else(|| "http-token path has no valid file name".to_owned())?;
    let sequence = TEMP_SEQUENCE.fetch_add(1, Ordering::Relaxed);
    let temporary = parent.join(format!(
        ".{file_name}.{}.{sequence}.tmp",
        std::process::id()
    ));
    let _guard = TemporaryFile {
        path: temporary.clone(),
    };
    create_private_file(&temporary, token.as_bytes())?;
    std::fs::rename(&temporary, path).map_err(|_| "http-token could not be replaced".to_owned())?;
    Ok(())
}

fn is_missing(error: &str) -> bool {
    error == "http-token is missing"
}

struct TemporaryFile {
    path: PathBuf,
}

impl Drop for TemporaryFile {
    fn drop(&mut self) {
        match std::fs::remove_file(&self.path) {
            Ok(()) => {}
            Err(error) if error.kind() == ErrorKind::NotFound => {}
            Err(_) => {}
        }
    }
}

fn ensure_private_parent(path: &Path) -> Result<(), String> {
    let parent = path
        .parent()
        .ok_or_else(|| "http-token path has no parent directory".to_owned())?;
    #[cfg(unix)]
    {
        use std::os::unix::fs::{DirBuilderExt, MetadataExt, PermissionsExt};

        let mut missing = Vec::new();
        let mut existing = parent;
        let existing_metadata = loop {
            match std::fs::symlink_metadata(existing) {
                Ok(metadata) => break metadata,
                Err(error) if error.kind() == ErrorKind::NotFound => {
                    missing.push(existing.to_path_buf());
                    existing = existing
                        .parent()
                        .ok_or_else(|| "http-token path has no existing ancestor".to_owned())?;
                }
                Err(_) => return Err("http-token parent could not be inspected".into()),
            }
        };
        if existing_metadata.file_type().is_symlink() || !existing_metadata.is_dir() {
            return Err("http-token ancestor must be a real directory".into());
        }
        for directory in missing.into_iter().rev() {
            match std::fs::symlink_metadata(&directory) {
                Ok(_) => return Err("http-token directory changed during creation".into()),
                Err(error) if error.kind() == ErrorKind::NotFound => {}
                Err(_) => return Err("http-token parent could not be inspected".into()),
            }
            let mut builder = std::fs::DirBuilder::new();
            builder.mode(0o700);
            builder
                .create(&directory)
                .map_err(|_| "http-token parent could not be created".to_owned())?;
            let metadata = std::fs::symlink_metadata(&directory)
                .map_err(|_| "http-token parent could not be inspected".to_owned())?;
            // SAFETY: `geteuid` has no arguments and no memory-safety preconditions.
            if metadata.file_type().is_symlink()
                || !metadata.is_dir()
                || metadata.uid() != unsafe { libc::geteuid() }
                || metadata.permissions().mode() & 0o077 != 0
            {
                return Err("new http-token directory is not private".into());
            }
        }
        let metadata = std::fs::symlink_metadata(parent)
            .map_err(|_| "http-token parent could not be inspected".to_owned())?;
        if metadata.file_type().is_symlink() || !metadata.is_dir() {
            return Err("http-token parent must be a real directory".into());
        }
        // SAFETY: `geteuid` has no arguments and no memory-safety preconditions.
        if metadata.uid() != unsafe { libc::geteuid() } {
            return Err("http-token parent is not owned by the current user".into());
        }
        if metadata.permissions().mode() & 0o077 != 0 {
            return Err("http-token parent permissions allow access by other users".into());
        }
        Ok(())
    }
    #[cfg(windows)]
    {
        ensure_private_windows_directory(parent)
    }
    #[cfg(not(any(unix, windows)))]
    {
        std::fs::create_dir_all(parent)
            .map_err(|_| "http-token parent could not be created".to_owned())
    }
}

fn create_private_file(path: &Path, bytes: &[u8]) -> Result<(), String> {
    #[cfg(unix)]
    {
        use std::os::unix::fs::OpenOptionsExt;

        let mut options = std::fs::OpenOptions::new();
        options
            .write(true)
            .create_new(true)
            .mode(0o600)
            .custom_flags(libc::O_CLOEXEC | libc::O_NOFOLLOW);
        let mut file = options
            .open(path)
            .map_err(|_| "http-token could not be created".to_owned())?;
        file.write_all(bytes)
            .map_err(|_| "http-token could not be written".to_owned())?;
        file.sync_all()
            .map_err(|_| "http-token could not be written".to_owned())?;
        Ok(())
    }
    #[cfg(windows)]
    {
        use std::os::windows::io::AsRawHandle;

        let mut file = ullage_auth::create_private_windows_file(path)
            .map_err(|_| "http-token could not be created".to_owned())?;
        if !ullage_auth::windows_handle_acl_is_private(file.as_raw_handle())
            .map_err(|_| "http-token ACL validation failed".to_owned())?
        {
            return Err(
                "http-token must be owned by and accessible only to the current user".into(),
            );
        }
        file.write_all(bytes)
            .map_err(|_| "http-token could not be written".to_owned())?;
        file.sync_all()
            .map_err(|_| "http-token could not be written".to_owned())?;
        Ok(())
    }
    #[cfg(not(any(unix, windows)))]
    {
        let _ = (path, bytes);
        Err("http-token storage is unsupported on this platform".into())
    }
}

fn open_existing(path: &Path) -> Result<std::fs::File, String> {
    #[cfg(unix)]
    {
        use std::os::unix::fs::{MetadataExt, OpenOptionsExt, PermissionsExt};

        let mut options = std::fs::OpenOptions::new();
        options
            .read(true)
            .custom_flags(libc::O_CLOEXEC | libc::O_NOFOLLOW);
        let file = match options.open(path) {
            Ok(file) => file,
            Err(error) if error.kind() == ErrorKind::NotFound => {
                return Err("http-token is missing".into());
            }
            Err(error) if error.raw_os_error() == Some(libc::ELOOP) => {
                return Err("http-token must be a regular file".into());
            }
            Err(_) => return Err("http-token could not be opened".into()),
        };
        let metadata = file
            .metadata()
            .map_err(|_| "http-token could not be validated".to_owned())?;
        // SAFETY: `geteuid` has no arguments and no memory-safety preconditions.
        if !metadata.is_file()
            || metadata.uid() != unsafe { libc::geteuid() }
            || metadata.permissions().mode() & 0o777 != 0o600
        {
            return Err(
                "http-token must be owned by and accessible only to the current user".into(),
            );
        }
        Ok(file)
    }
    #[cfg(windows)]
    {
        use std::os::windows::fs::{MetadataExt, OpenOptionsExt};
        use std::os::windows::io::AsRawHandle;
        use windows_sys::Win32::Storage::FileSystem::FILE_FLAG_OPEN_REPARSE_POINT;

        const FILE_ATTRIBUTE_REPARSE_POINT: u32 = 0x400;
        let mut options = std::fs::OpenOptions::new();
        options
            .read(true)
            .custom_flags(FILE_FLAG_OPEN_REPARSE_POINT);
        let file = match options.open(path) {
            Ok(file) => file,
            Err(error) if error.kind() == ErrorKind::NotFound => {
                return Err("http-token is missing".into());
            }
            Err(_) => return Err("http-token could not be opened".into()),
        };
        let metadata = file
            .metadata()
            .map_err(|_| "http-token could not be validated".to_owned())?;
        if !metadata.is_file()
            || metadata.file_attributes() & FILE_ATTRIBUTE_REPARSE_POINT != 0
            || !ullage_auth::windows_handle_acl_is_private(file.as_raw_handle())
                .map_err(|_| "http-token ACL validation failed".to_owned())?
        {
            return Err(
                "http-token must be owned by and accessible only to the current user".into(),
            );
        }
        Ok(file)
    }
    #[cfg(not(any(unix, windows)))]
    {
        let _ = path;
        Err("http-token storage is unsupported on this platform".into())
    }
}

#[cfg(windows)]
fn ensure_private_windows_directory(path: &Path) -> Result<(), String> {
    let mut missing = Vec::new();
    let mut existing = path;
    loop {
        match std::fs::symlink_metadata(existing) {
            Ok(_) => break,
            Err(error) if error.kind() == ErrorKind::NotFound => {
                missing.push(existing.to_path_buf());
                existing = existing
                    .parent()
                    .ok_or_else(|| "http-token path has no existing Windows ancestor".to_owned())?;
            }
            Err(_) => return Err("http-token parent could not be inspected".into()),
        }
    }
    for directory in missing.into_iter().rev() {
        ullage_auth::create_private_windows_directory(&directory)
            .map_err(|_| "http-token parent could not be created".to_owned())?;
    }
    validate_private_windows_directory(path)
}

#[cfg(windows)]
fn validate_private_windows_directory(path: &Path) -> Result<(), String> {
    use std::os::windows::fs::{MetadataExt, OpenOptionsExt};
    use std::os::windows::io::AsRawHandle;
    use windows_sys::Win32::Foundation::GENERIC_READ;
    use windows_sys::Win32::Storage::FileSystem::{
        FILE_FLAG_BACKUP_SEMANTICS, FILE_FLAG_OPEN_REPARSE_POINT,
    };

    const FILE_ATTRIBUTE_REPARSE_POINT: u32 = 0x400;
    let mut options = std::fs::OpenOptions::new();
    options
        .access_mode(GENERIC_READ)
        .custom_flags(FILE_FLAG_BACKUP_SEMANTICS | FILE_FLAG_OPEN_REPARSE_POINT);
    let directory = options
        .open(path)
        .map_err(|_| "http-token parent could not be opened".to_owned())?;
    let metadata = directory
        .metadata()
        .map_err(|_| "http-token parent could not be validated".to_owned())?;
    if !metadata.is_dir()
        || metadata.file_attributes() & FILE_ATTRIBUTE_REPARSE_POINT != 0
        || !ullage_auth::windows_handle_acl_is_private(directory.as_raw_handle())
            .map_err(|_| "http-token parent ACL validation failed".to_owned())?
    {
        return Err("http-token parent must be a private non-reparse directory".into());
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn generated_tokens_are_256_bit_base64url() {
        let token = generate_token().unwrap();
        let decoded = URL_SAFE_NO_PAD.decode(token.as_bytes()).unwrap();
        assert_eq!(decoded.len(), TOKEN_BYTES);
        assert!(!token.contains('+') && !token.contains('/') && !token.contains('='));
    }

    #[test]
    fn constant_time_eq_rejects_length_and_prefix_differences() {
        assert!(constant_time_eq(b"abcdef", b"abcdef"));
        assert!(!constant_time_eq(b"abcdef", b"abcde"));
        assert!(!constant_time_eq(b"abcdef", b"abcdeg"));
        assert!(!constant_time_eq(b"abc", b"abcdef"));
    }

    #[cfg(unix)]
    fn make_private_dir(path: &std::path::Path) {
        use std::os::unix::fs::PermissionsExt;
        std::fs::set_permissions(path, std::fs::Permissions::from_mode(0o700)).unwrap();
    }

    #[cfg(unix)]
    #[test]
    fn persists_private_token_and_refuses_to_repair_permissions() {
        use std::os::unix::fs::PermissionsExt;

        let directory = tempfile::tempdir().unwrap();
        make_private_dir(directory.path());
        let path = directory.path().join("http-token");
        let token = load_or_create_token(&path).unwrap();
        let metadata = std::fs::metadata(&path).unwrap();
        assert_eq!(metadata.permissions().mode() & 0o777, 0o600);
        assert_eq!(load_token(&path).unwrap(), token);

        std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o644)).unwrap();
        let error = load_token(&path).unwrap_err();
        assert!(error.contains("owned by and accessible only to the current user"),);
        assert_eq!(
            std::fs::metadata(&path).unwrap().permissions().mode() & 0o777,
            0o644
        );
        assert!(rotate_token(&path).is_err());

        std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o700)).unwrap();
        assert!(load_token(&path).is_err());
        assert_eq!(
            std::fs::metadata(&path).unwrap().permissions().mode() & 0o777,
            0o700
        );
    }

    #[cfg(unix)]
    #[test]
    fn rotate_replaces_the_token_without_leaving_the_old_value() {
        let directory = tempfile::tempdir().unwrap();
        make_private_dir(directory.path());
        let path = directory.path().join("http-token");
        let first = load_or_create_token(&path).unwrap();
        let second = rotate_token(&path).unwrap();
        assert_ne!(first, second);
        assert_eq!(load_token(&path).unwrap(), second);
    }
}
