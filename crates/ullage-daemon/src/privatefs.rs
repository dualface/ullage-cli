//! Private state files: the one implementation of private-parent creation,
//! private file create/open, atomic temp-file rename, and crash sweeping that
//! both the JSON snapshot store and the device store share.
//!
//! Each caller keeps its own error vocabulary through [`ErrorStyle`]: the
//! snapshot store reports fixed "snapshot …" phrases while the device store
//! names the filesystem path, so every message a caller produced before this
//! extraction stays byte-identical.

use std::io::{ErrorKind, Write};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};

/// Which vocabulary errors use.
#[derive(Clone, Copy)]
pub(crate) enum ErrorStyle {
    /// `JsonSnapshotStore`: fixed "snapshot …" phrases, bare `io::Error` text.
    Snapshot,
    /// `DeviceStore`: the filesystem path with a `could not be …` clause.
    Path,
}

/// How strictly a private file's mode is checked on unix: the snapshot file
/// accepts any owner-only mode, the device file requires exactly `0o600`.
#[derive(Clone, Copy)]
pub(crate) enum FileStrictness {
    OwnerOnly,
    Exact,
}

static TEMP_SEQUENCE: AtomicU64 = AtomicU64::new(0);

impl ErrorStyle {
    fn no_parent(self, path: &Path) -> String {
        match self {
            Self::Snapshot => "snapshot path has no parent directory".into(),
            Self::Path => format!("{} has no parent directory", path.display()),
        }
    }

    fn no_existing_ancestor(self, path: &Path) -> String {
        match self {
            Self::Snapshot => "snapshot path has no existing ancestor".into(),
            Self::Path => format!("{} has no existing ancestor", path.display()),
        }
    }

    fn inspect(self, path: &Path, error: impl std::fmt::Display) -> String {
        match self {
            Self::Snapshot => error.to_string(),
            Self::Path => format!("{} could not be inspected: {error}", path.display()),
        }
    }

    fn ancestor_not_directory(self, parent: &Path) -> String {
        match self {
            Self::Snapshot => "snapshot ancestor must be a real directory".into(),
            Self::Path => format!("{} ancestor must be a real directory", parent.display()),
        }
    }

    fn changed_during_creation(self, directory: &Path) -> String {
        match self {
            Self::Snapshot => "snapshot directory changed during creation".into(),
            Self::Path => format!("{} changed during creation", directory.display()),
        }
    }

    fn create(self, path: &Path, error: impl std::fmt::Display) -> String {
        match self {
            Self::Snapshot => error.to_string(),
            Self::Path => format!("{} could not be created: {error}", path.display()),
        }
    }

    fn new_directory_not_private(self, directory: &Path) -> String {
        match self {
            Self::Snapshot => "new snapshot directory is not private".into(),
            Self::Path => format!("{} is not a private directory", directory.display()),
        }
    }

    fn parent_not_directory(self, parent: &Path) -> String {
        match self {
            Self::Snapshot => "snapshot parent must be a real directory".into(),
            Self::Path => format!("{} must be a private directory", parent.display()),
        }
    }

    fn parent_not_owned(self, parent: &Path) -> String {
        match self {
            Self::Snapshot => "snapshot parent is not owned by the current user".into(),
            Self::Path => format!("{} must be a private directory", parent.display()),
        }
    }

    fn parent_permissions(self, parent: &Path) -> String {
        match self {
            Self::Snapshot => "snapshot parent permissions allow access by other users".into(),
            Self::Path => format!("{} must be a private directory", parent.display()),
        }
    }

    fn not_regular_file(self, path: &Path) -> String {
        match self {
            Self::Snapshot => "snapshot path must be a regular file".into(),
            Self::Path => format!("{} must be a regular file", path.display()),
        }
    }

    fn open(self, path: &Path, error: impl std::fmt::Display) -> String {
        match self {
            Self::Snapshot => error.to_string(),
            Self::Path => format!("{} could not be opened: {error}", path.display()),
        }
    }

    fn validate(self, path: &Path, error: impl std::fmt::Display) -> String {
        match self {
            Self::Snapshot => error.to_string(),
            Self::Path => format!("{} could not be validated: {error}", path.display()),
        }
    }

    fn not_private_file(self, path: &Path) -> String {
        match self {
            Self::Snapshot => {
                "snapshot file must be private, regular, and owned by the current user".into()
            }
            Self::Path => format!("{} must be a private regular file", path.display()),
        }
    }

    #[cfg(windows)]
    fn not_private_windows_file(self, path: &Path) -> String {
        match self {
            Self::Snapshot => "snapshot file must be private, regular, and non-reparse".into(),
            Self::Path => format!("{} must be a private regular file", path.display()),
        }
    }

    #[cfg(windows)]
    fn not_private_windows_directory(self, path: &Path) -> String {
        match self {
            Self::Snapshot => "snapshot parent must be a private non-reparse directory".into(),
            Self::Path => format!("{} must be a private non-reparse directory", path.display()),
        }
    }

    fn read(self, path: &Path, error: impl std::fmt::Display) -> String {
        match self {
            Self::Snapshot => error.to_string(),
            Self::Path => format!("{} could not be read: {error}", path.display()),
        }
    }

    fn no_file_name(self, path: &Path) -> String {
        match self {
            Self::Snapshot => "snapshot path has no valid file name".into(),
            Self::Path => format!("{} has no valid file name", path.display()),
        }
    }

    fn write(self, path: &Path, error: impl std::fmt::Display) -> String {
        match self {
            Self::Snapshot => error.to_string(),
            Self::Path => format!("{} could not be written: {error}", path.display()),
        }
    }

    #[cfg(windows)]
    fn acl(self, path: &Path, error: impl std::fmt::Display) -> String {
        match self {
            Self::Snapshot => error.to_string(),
            Self::Path => format!("{} ACL validation failed: {error}", path.display()),
        }
    }

    #[cfg(windows)]
    fn must_be_private(self, path: &Path) -> String {
        match self {
            Self::Snapshot => "snapshot file must be private".into(),
            Self::Path => format!("{} must be private", path.display()),
        }
    }

    fn replace(self, path: &Path, error: impl std::fmt::Display) -> String {
        match self {
            Self::Snapshot => error.to_string(),
            Self::Path => format!("{} could not be replaced: {error}", path.display()),
        }
    }

    fn invalid_parent_path(self, path: &Path) -> String {
        match self {
            Self::Snapshot => "snapshot parent path is not valid".into(),
            Self::Path => format!("{} has no valid parent path", path.display()),
        }
    }

    #[cfg(windows)]
    fn no_windows_anchor(self, path: &Path) -> String {
        match self {
            Self::Snapshot => "snapshot path has no existing Windows anchor".into(),
            Self::Path => format!("{} has no existing Windows ancestor", path.display()),
        }
    }

    #[cfg(not(any(unix, windows)))]
    fn unsupported(self) -> String {
        match self {
            Self::Snapshot => "snapshot storage is unsupported on this platform".into(),
            Self::Path => "device storage is unsupported on this platform".into(),
        }
    }
}

/// Validates — creating if needed — that the directory holding `path` is a
/// private, user-owned, non-symlink directory.
pub(crate) fn ensure_private_parent(path: &Path, style: ErrorStyle) -> Result<(), String> {
    let parent = path.parent().ok_or_else(|| style.no_parent(path))?;
    #[cfg(unix)]
    {
        ensure_private_unix_parent(path, parent, style)
    }
    #[cfg(windows)]
    {
        ensure_private_windows_directory(parent, style)
    }
    // Unsupported platforms reject before the directory is created, traversed,
    // or swept: no private-mode primitive exists to enforce here.
    #[cfg(not(any(unix, windows)))]
    {
        let _ = parent;
        Err(style.unsupported())
    }
}

#[cfg(unix)]
fn ensure_private_unix_parent(path: &Path, parent: &Path, style: ErrorStyle) -> Result<(), String> {
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
                    .ok_or_else(|| style.no_existing_ancestor(path))?;
            }
            Err(error) => return Err(style.inspect(parent, &error)),
        }
    };
    if existing_metadata.file_type().is_symlink() || !existing_metadata.is_dir() {
        return Err(style.ancestor_not_directory(parent));
    }
    for directory in missing.into_iter().rev() {
        match std::fs::symlink_metadata(&directory) {
            Ok(_) => return Err(style.changed_during_creation(&directory)),
            Err(error) if error.kind() == ErrorKind::NotFound => {}
            Err(error) => return Err(style.inspect(&directory, &error)),
        }
        {
            let mut builder = std::fs::DirBuilder::new();
            builder.mode(0o700);
            builder
                .create(&directory)
                .map_err(|error| style.create(&directory, &error))?;
        }
        let metadata = std::fs::symlink_metadata(&directory)
            .map_err(|error| style.inspect(&directory, &error))?;
        // SAFETY: `geteuid` has no arguments and no memory-safety preconditions.
        if metadata.file_type().is_symlink()
            || !metadata.is_dir()
            || metadata.uid() != unsafe { libc::geteuid() }
            || metadata.permissions().mode() & 0o077 != 0
        {
            return Err(style.new_directory_not_private(&directory));
        }
    }
    let metadata =
        std::fs::symlink_metadata(parent).map_err(|error| style.inspect(parent, &error))?;
    if metadata.file_type().is_symlink() || !metadata.is_dir() {
        return Err(style.parent_not_directory(parent));
    }
    // SAFETY: `geteuid` has no arguments and no memory-safety preconditions.
    if metadata.uid() != unsafe { libc::geteuid() } {
        return Err(style.parent_not_owned(parent));
    }
    if metadata.permissions().mode() & 0o077 != 0 {
        return Err(style.parent_permissions(parent));
    }
    Ok(())
}

/// The `.name.pid.sequence.tmp` path `stage`/`replace_private_file` writes
/// before renaming it over `path`.
pub(crate) fn temporary_path(path: &Path, style: ErrorStyle) -> Result<PathBuf, String> {
    let file_name = path
        .file_name()
        .and_then(|name| name.to_str())
        .ok_or_else(|| style.no_file_name(path))?;
    let sequence = TEMP_SEQUENCE.fetch_add(1, Ordering::Relaxed);
    Ok(path.with_file_name(format!(
        ".{file_name}.{}.{sequence}.tmp",
        std::process::id()
    )))
}

/// A temp file deleted on drop unless its rename already moved it away.
pub(crate) struct TemporaryFile(pub(crate) PathBuf);

impl Drop for TemporaryFile {
    fn drop(&mut self) {
        match std::fs::remove_file(&self.0) {
            Ok(()) => {}
            Err(error) if error.kind() == ErrorKind::NotFound => {}
            Err(_) => {}
        }
    }
}

/// Removes temp files left behind by a crashed writer, matched on the
/// `.name.pid.sequence.tmp` naming this module generates. Best effort: a file
/// that cannot be removed is left for the next open.
pub(crate) fn sweep_stale_temporary_files(path: &Path) {
    let Some(parent) = path.parent() else {
        return;
    };
    let Some(file_name) = path.file_name().and_then(|name| name.to_str()) else {
        return;
    };
    let prefix = format!(".{file_name}.");
    let Ok(entries) = std::fs::read_dir(parent) else {
        return;
    };
    for entry in entries.flatten() {
        let Some(name) = entry.file_name().to_str().map(str::to_owned) else {
            continue;
        };
        if !is_stale_temporary_name(&name, &prefix) {
            continue;
        }
        if entry.file_type().is_ok_and(|file_type| file_type.is_file()) {
            let _ = std::fs::remove_file(entry.path());
        }
    }
}

/// Matches only the `.name.pid.sequence.tmp` temp names this module
/// generates, so the sweep never touches a user file that merely looks
/// similar. A file whose recorded writer is still running is not stale: two
/// daemons racing on one state directory must not delete each other's
/// in-flight writes.
fn is_stale_temporary_name(name: &str, prefix: &str) -> bool {
    let Some(rest) = name.strip_prefix(prefix) else {
        return false;
    };
    let Some(body) = rest.strip_suffix(".tmp") else {
        return false;
    };
    let mut parts = body.split('.');
    let Some(writer_pid) = parts.next().and_then(|part| part.parse::<u32>().ok()) else {
        return false;
    };
    let numeric = |part: Option<&str>| {
        part.is_some_and(|part| !part.is_empty() && part.bytes().all(|byte| byte.is_ascii_digit()))
    };
    numeric(parts.next()) && parts.next().is_none() && !writer_process_is_alive(writer_pid)
}

/// Whether the process that wrote a temp file is still running. Between a
/// writer closing its handle and the rename, the file is unlocked, so the
/// recorded pid is probed on both supported platforms.
#[cfg(unix)]
fn writer_process_is_alive(pid: u32) -> bool {
    // kill(pid, 0) probes existence without signalling; EPERM still means the
    // process exists.
    if unsafe { libc::kill(pid as libc::pid_t, 0) } == 0 {
        return true;
    }
    std::io::Error::last_os_error().raw_os_error() == Some(libc::EPERM)
}

#[cfg(windows)]
fn writer_process_is_alive(pid: u32) -> bool {
    use windows_sys::Win32::Foundation::CloseHandle;
    use windows_sys::Win32::System::Threading::{
        GetExitCodeProcess, OpenProcess, PROCESS_QUERY_LIMITED_INFORMATION,
    };

    // STILL_ACTIVE marks a live process; a queryable exit code or a failed
    // open means the writer is gone.
    const STILL_ACTIVE: u32 = 259;
    // SAFETY: `pid` names a process; the returned handle is closed below.
    let handle = unsafe { OpenProcess(PROCESS_QUERY_LIMITED_INFORMATION, 0, pid) };
    if handle.is_null() {
        return false;
    }
    let mut exit_code = 0u32;
    // SAFETY: `handle` is a valid process handle owned by this scope.
    let queried = unsafe { GetExitCodeProcess(handle, &mut exit_code) };
    // SAFETY: `handle` is no longer needed.
    unsafe { CloseHandle(handle) };
    queried != 0 && exit_code == STILL_ACTIVE
}

#[cfg(not(any(unix, windows)))]
fn writer_process_is_alive(_pid: u32) -> bool {
    false
}

/// Opens `path` for reading when it exists and proves private, regular, and
/// owned by the current user. `None` means the file is absent; any other
/// shape is an error.
pub(crate) fn open_private_file(
    path: &Path,
    strictness: FileStrictness,
    style: ErrorStyle,
) -> Result<Option<std::fs::File>, String> {
    #[cfg(unix)]
    {
        use std::os::unix::fs::{MetadataExt, OpenOptionsExt, PermissionsExt};

        let mut options = std::fs::OpenOptions::new();
        options
            .read(true)
            .custom_flags(libc::O_CLOEXEC | libc::O_NOFOLLOW);
        let file = match options.open(path) {
            Ok(file) => file,
            Err(error) if error.kind() == ErrorKind::NotFound => return Ok(None),
            Err(error) if error.raw_os_error() == Some(libc::ELOOP) => {
                return Err(style.not_regular_file(path));
            }
            Err(error) => return Err(style.open(path, &error)),
        };
        let metadata = file
            .metadata()
            .map_err(|error| style.validate(path, &error))?;
        let private_mode = match strictness {
            FileStrictness::OwnerOnly => metadata.permissions().mode() & 0o077 == 0,
            FileStrictness::Exact => metadata.permissions().mode() & 0o777 == 0o600,
        };
        // SAFETY: `geteuid` has no arguments and no memory-safety preconditions.
        if !metadata.is_file() || metadata.uid() != unsafe { libc::geteuid() } || !private_mode {
            return Err(style.not_private_file(path));
        }
        Ok(Some(file))
    }
    #[cfg(windows)]
    {
        open_private_windows_file(path, style)
    }
    #[cfg(not(any(unix, windows)))]
    {
        let _ = (path, strictness);
        Err(style.unsupported())
    }
}

/// Reads a private file's bytes, or `None` when it does not exist. The parent
/// is validated first so a hostile directory is rejected before the file is
/// touched.
pub(crate) fn read_private_file(
    path: &Path,
    strictness: FileStrictness,
    style: ErrorStyle,
) -> Result<Option<Vec<u8>>, String> {
    use std::io::Read;

    ensure_private_parent(path, style)?;
    let Some(mut file) = open_private_file(path, strictness, style)? else {
        return Ok(None);
    };
    let mut bytes = Vec::new();
    file.read_to_end(&mut bytes)
        .map_err(|error| style.read(path, &error))?;
    Ok(Some(bytes))
}

/// Rejects an existing destination that is not already a private file, so a
/// rename can never land on something another principal planted.
pub(crate) fn validate_private_file(
    path: &Path,
    strictness: FileStrictness,
    style: ErrorStyle,
) -> Result<(), String> {
    open_private_file(path, strictness, style).map(|_| ())
}

/// Creates a temp file nobody else can open or swap: `create_new` plus
/// `0o600` and `O_NOFOLLOW` on unix, a private ACL on Windows. The snapshot
/// flow trusts the create flags alone; `replace_private_file` re-checks the
/// ACL after creation for the device store's stricter original behavior.
pub(crate) fn create_private_temporary(
    path: &Path,
    style: ErrorStyle,
) -> Result<std::fs::File, String> {
    #[cfg(unix)]
    {
        use std::os::unix::fs::OpenOptionsExt;

        let mut options = std::fs::OpenOptions::new();
        options
            .write(true)
            .create_new(true)
            .mode(0o600)
            .custom_flags(libc::O_CLOEXEC | libc::O_NOFOLLOW);
        options
            .open(path)
            .map_err(|error| style.create(path, &error))
    }
    #[cfg(windows)]
    {
        ullage_auth::create_private_windows_file(path).map_err(|error| style.create(path, &error))
    }
    #[cfg(not(any(unix, windows)))]
    {
        let _ = path;
        Err(style.unsupported())
    }
}

/// Writes `bytes` and fsyncs the temp file before its rename.
pub(crate) fn write_and_sync(
    file: &mut std::fs::File,
    bytes: &[u8],
    path: &Path,
    style: ErrorStyle,
) -> Result<(), String> {
    file.write_all(bytes)
        .and_then(|()| file.sync_all())
        .map_err(|error| style.write(path, &error))
}

/// The whole atomic-write flow: private parent, existing-destination check,
/// temp file, fsync, rename, directory sync. The temp guard removes the
/// staged file if any step fails.
pub(crate) fn replace_private_file(
    path: &Path,
    bytes: &[u8],
    style: ErrorStyle,
) -> Result<(), String> {
    ensure_private_parent(path, style)?;
    validate_private_file(path, FileStrictness::Exact, style)?;
    let temporary = temporary_path(path, style)?;
    let _guard = TemporaryFile(temporary.clone());
    let mut file = create_private_temporary(&temporary, style)?;
    #[cfg(windows)]
    validate_created_acl(&temporary, &file, style)?;
    write_and_sync(&mut file, bytes, &temporary, style)?;
    commit_temporary(&temporary, path, style)
}

/// Re-checks the ACL `create_private_windows_file` attached to a fresh temp
/// file, matching the device store's original post-create verification.
#[cfg(windows)]
fn validate_created_acl(
    path: &Path,
    file: &std::fs::File,
    style: ErrorStyle,
) -> Result<(), String> {
    use std::os::windows::io::AsRawHandle;

    if !ullage_auth::windows_handle_acl_is_private(file.as_raw_handle())
        .map_err(|error| style.acl(path, &error))?
    {
        return Err(style.must_be_private(path));
    }
    Ok(())
}

/// Atomically replaces `destination` with `temporary` and fsyncs the parent
/// directory. A failed sync is logged, not returned as a commit error: the
/// destination already carries the new bytes and reporting failure would make
/// callers roll back in-memory state the disk no longer matches.
pub(crate) fn commit_temporary(
    temporary: &Path,
    destination: &Path,
    style: ErrorStyle,
) -> Result<(), String> {
    replace_path(temporary, destination).map_err(|error| style.replace(destination, &error))?;
    if let Err(error) = sync_parent_directory(destination, style) {
        match style {
            ErrorStyle::Snapshot => {
                eprintln!("ullage snapshot directory sync failed after commit: {error}")
            }
            ErrorStyle::Path => eprintln!(
                "{} could not be synced after commit: {error}",
                destination.display()
            ),
        }
    }
    Ok(())
}

#[cfg(unix)]
fn replace_path(source: &Path, destination: &Path) -> std::io::Result<()> {
    std::fs::rename(source, destination)
}

#[cfg(windows)]
fn replace_path(source: &Path, destination: &Path) -> std::io::Result<()> {
    use std::os::windows::ffi::OsStrExt;
    use windows_sys::Win32::Storage::FileSystem::{
        MOVEFILE_REPLACE_EXISTING, MOVEFILE_WRITE_THROUGH, MoveFileExW,
    };

    let source = source
        .as_os_str()
        .encode_wide()
        .chain(Some(0))
        .collect::<Vec<_>>();
    let destination = destination
        .as_os_str()
        .encode_wide()
        .chain(Some(0))
        .collect::<Vec<_>>();
    // SAFETY: both paths are valid, null-terminated UTF-16 buffers for the duration of the call.
    let replaced = unsafe {
        MoveFileExW(
            source.as_ptr(),
            destination.as_ptr(),
            MOVEFILE_REPLACE_EXISTING | MOVEFILE_WRITE_THROUGH,
        )
    };
    if replaced == 0 {
        Err(std::io::Error::last_os_error())
    } else {
        Ok(())
    }
}

#[cfg(not(any(unix, windows)))]
fn replace_path(source: &Path, destination: &Path) -> std::io::Result<()> {
    std::fs::rename(source, destination)
}

/// fsyncs the directory holding `path` so a committed rename survives a crash.
/// On Windows the write-through rename already flushes the directory entry.
#[cfg(unix)]
fn sync_parent_directory(path: &Path, style: ErrorStyle) -> Result<(), String> {
    use std::os::unix::ffi::OsStrExt;
    use std::os::unix::io::FromRawFd;

    let parent = path.parent().ok_or_else(|| style.no_parent(path))?;
    let path = std::ffi::CString::new(parent.as_os_str().as_bytes())
        .map_err(|_| style.invalid_parent_path(path))?;
    // SAFETY: `path` is a valid null-terminated path; the fd is checked below
    // and ownership is passed to `File` only on success.
    let descriptor = unsafe { libc::open(path.as_ptr(), libc::O_RDONLY | libc::O_DIRECTORY) };
    if descriptor < 0 {
        return Err(std::io::Error::last_os_error().to_string());
    }
    // SAFETY: `descriptor` is a valid, freshly opened fd owned by this scope.
    let directory = unsafe { std::fs::File::from_raw_fd(descriptor) };
    directory.sync_all().map_err(|error| error.to_string())
}

#[cfg(not(unix))]
fn sync_parent_directory(_path: &Path, _style: ErrorStyle) -> Result<(), String> {
    Ok(())
}

#[cfg(windows)]
fn ensure_private_windows_directory(path: &Path, style: ErrorStyle) -> Result<(), String> {
    let mut missing = Vec::new();
    let mut existing = path;
    loop {
        match std::fs::symlink_metadata(existing) {
            Ok(_) => break,
            Err(error) if error.kind() == ErrorKind::NotFound => {
                missing.push(existing.to_path_buf());
                existing = existing
                    .parent()
                    .ok_or_else(|| style.no_windows_anchor(path))?;
            }
            Err(error) => return Err(style.inspect(path, &error)),
        }
    }
    for directory in missing.into_iter().rev() {
        ullage_auth::create_private_windows_directory(&directory)
            .map_err(|error| style.create(&directory, &error))?;
    }
    validate_private_windows_directory(path, style)
}

#[cfg(windows)]
fn validate_private_windows_directory(path: &Path, style: ErrorStyle) -> Result<(), String> {
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
        .map_err(|error| style.open(path, &error))?;
    let metadata = directory
        .metadata()
        .map_err(|error| style.validate(path, &error))?;
    if !metadata.is_dir()
        || metadata.file_attributes() & FILE_ATTRIBUTE_REPARSE_POINT != 0
        || !ullage_auth::windows_handle_acl_is_private(directory.as_raw_handle())
            .map_err(|error| style.acl(path, &error))?
    {
        return Err(style.not_private_windows_directory(path));
    }
    Ok(())
}

#[cfg(windows)]
fn open_private_windows_file(
    path: &Path,
    style: ErrorStyle,
) -> Result<Option<std::fs::File>, String> {
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
        Err(error) if error.kind() == ErrorKind::NotFound => return Ok(None),
        Err(error) => return Err(style.open(path, &error)),
    };
    let metadata = file
        .metadata()
        .map_err(|error| style.validate(path, &error))?;
    if !metadata.is_file()
        || metadata.file_attributes() & FILE_ATTRIBUTE_REPARSE_POINT != 0
        || !ullage_auth::windows_handle_acl_is_private(file.as_raw_handle())
            .map_err(|error| style.acl(path, &error))?
    {
        return Err(style.not_private_windows_file(path));
    }
    Ok(Some(file))
}
