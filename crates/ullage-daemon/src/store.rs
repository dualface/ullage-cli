use std::io::ErrorKind;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::sync::atomic::{AtomicU64, Ordering};

use async_trait::async_trait;
use tokio::sync::RwLock;

use crate::model::{AccountId, PersistedState, SnapshotMap, SnapshotRecord};

#[async_trait]
pub trait SnapshotStore: Send + Sync {
    async fn load(&self) -> Result<PersistedState, String>;
    async fn stage(&self, state: &PersistedState) -> Result<Box<dyn StagedSnapshot>, String>;
}

#[async_trait]
pub trait StagedSnapshot: Send {
    /// Crosses the backend commit point and returns its final outcome.
    async fn commit(self: Box<Self>) -> Result<(), String>;
}

#[derive(Clone, Default)]
pub struct MemorySnapshotStore {
    state: Arc<RwLock<PersistedState>>,
}

struct StagedMemorySnapshot {
    target: Arc<RwLock<PersistedState>>,
    state: PersistedState,
}

#[async_trait]
impl StagedSnapshot for StagedMemorySnapshot {
    async fn commit(self: Box<Self>) -> Result<(), String> {
        *self.target.write().await = self.state;
        Ok(())
    }
}

impl MemorySnapshotStore {
    pub async fn records(&self) -> SnapshotMap {
        self.state.read().await.snapshots.clone()
    }

    pub async fn state(&self) -> PersistedState {
        self.state.read().await.clone()
    }
}

#[async_trait]
impl SnapshotStore for MemorySnapshotStore {
    async fn load(&self) -> Result<PersistedState, String> {
        Ok(self.state.read().await.clone())
    }

    async fn stage(&self, state: &PersistedState) -> Result<Box<dyn StagedSnapshot>, String> {
        Ok(Box::new(StagedMemorySnapshot {
            target: self.state.clone(),
            state: state.clone(),
        }))
    }
}

#[derive(Clone, Debug)]
pub struct JsonSnapshotStore {
    path: PathBuf,
}

struct TemporarySnapshot {
    path: PathBuf,
}

struct StagedJsonSnapshot {
    temporary: TemporarySnapshot,
    destination: PathBuf,
}

#[async_trait]
impl StagedSnapshot for StagedJsonSnapshot {
    async fn commit(self: Box<Self>) -> Result<(), String> {
        replace_path(&self.temporary.path, &self.destination).map_err(|error| error.to_string())?;
        // The rename is only durable once the directory entry is on disk. A
        // failed sync is logged, not returned as a commit error: the
        // destination already carries the new bytes and reporting failure
        // would make callers roll back in-memory state the disk no longer
        // matches.
        if let Err(error) = sync_parent_directory(&self.destination) {
            eprintln!("ullage snapshot directory sync failed after commit: {error}");
        }
        Ok(())
    }
}

impl Drop for TemporarySnapshot {
    fn drop(&mut self) {
        match std::fs::remove_file(&self.path) {
            Ok(()) => {}
            Err(error) if error.kind() == ErrorKind::NotFound => {}
            Err(_) => {}
        }
    }
}

impl JsonSnapshotStore {
    pub fn new(path: impl Into<PathBuf>) -> Self {
        Self { path: path.into() }
    }

    pub fn path(&self) -> &Path {
        &self.path
    }

    async fn ensure_private_parent(&self) -> Result<(), String> {
        let parent = self
            .path
            .parent()
            .ok_or_else(|| "snapshot path has no parent directory".to_owned())?;
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
                            .ok_or_else(|| "snapshot path has no existing ancestor".to_owned())?;
                    }
                    Err(error) => return Err(error.to_string()),
                }
            };
            if existing_metadata.file_type().is_symlink() || !existing_metadata.is_dir() {
                return Err("snapshot ancestor must be a real directory".into());
            }
            for directory in missing.into_iter().rev() {
                match std::fs::symlink_metadata(&directory) {
                    Ok(_) => return Err("snapshot directory changed during creation".into()),
                    Err(error) if error.kind() == ErrorKind::NotFound => {}
                    Err(error) => return Err(error.to_string()),
                }
                {
                    let mut builder = std::fs::DirBuilder::new();
                    builder.mode(0o700);
                    builder
                        .create(&directory)
                        .map_err(|error| error.to_string())?;
                }
                let metadata =
                    std::fs::symlink_metadata(&directory).map_err(|error| error.to_string())?;
                if metadata.file_type().is_symlink()
                    || !metadata.is_dir()
                    || metadata.uid() != unsafe { libc::geteuid() }
                    || metadata.permissions().mode() & 0o077 != 0
                {
                    return Err("new snapshot directory is not private".into());
                }
            }
            let metadata = tokio::fs::symlink_metadata(parent)
                .await
                .map_err(|error| error.to_string())?;
            if metadata.file_type().is_symlink() || !metadata.is_dir() {
                return Err("snapshot parent must be a real directory".into());
            }
            // SAFETY: `geteuid` has no arguments and no memory-safety preconditions.
            if metadata.uid() != unsafe { libc::geteuid() } {
                return Err("snapshot parent is not owned by the current user".into());
            }
            if metadata.permissions().mode() & 0o077 != 0 {
                return Err("snapshot parent permissions allow access by other users".into());
            }
        }
        #[cfg(windows)]
        ensure_private_windows_directory(parent)?;
        #[cfg(not(any(unix, windows)))]
        {
            let _ = parent;
            return Err("snapshot storage is unsupported on this platform".into());
        }
        Ok(())
    }

    /// Removes temp files left behind by a crashed `stage`, matched on the
    /// `.name.pid.sequence.tmp` naming this store generates. Best effort: a
    /// file that cannot be removed is left for the next start.
    async fn sweep_stale_temporary_files(&self) {
        let Some(parent) = self.path.parent() else {
            return;
        };
        let Some(file_name) = self.path.file_name().and_then(|name| name.to_str()) else {
            return;
        };
        let prefix = format!(".{file_name}.");
        let Ok(mut entries) = tokio::fs::read_dir(parent).await else {
            return;
        };
        while let Ok(Some(entry)) = entries.next_entry().await {
            let Some(name) = entry.file_name().to_str().map(str::to_owned) else {
                continue;
            };
            if !is_stale_temporary_name(&name, &prefix) {
                continue;
            }
            if entry
                .file_type()
                .await
                .is_ok_and(|file_type| file_type.is_file())
            {
                let _ = tokio::fs::remove_file(entry.path()).await;
            }
        }
    }
}

/// Matches only the `.name.pid.sequence.tmp` temp names `stage` generates, so
/// the sweep never touches a user file that merely looks similar. A file
/// whose recorded writer is still running is not stale: two daemons racing on
/// one state directory must not delete each other's in-flight writes.
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

/// Whether the process that wrote a temp file is still running. On Windows
/// the name check alone decides: the file stays locked while its writer holds
/// it open, so deleting it simply fails.
#[cfg(unix)]
fn writer_process_is_alive(pid: u32) -> bool {
    // kill(pid, 0) probes existence without signalling; EPERM still means the
    // process exists.
    if unsafe { libc::kill(pid as libc::pid_t, 0) } == 0 {
        return true;
    }
    std::io::Error::last_os_error().raw_os_error() == Some(libc::EPERM)
}

#[cfg(not(unix))]
fn writer_process_is_alive(_pid: u32) -> bool {
    false
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
fn sync_parent_directory(path: &Path) -> Result<(), String> {
    use std::os::unix::ffi::OsStrExt;
    use std::os::unix::io::FromRawFd;

    let parent = path
        .parent()
        .ok_or_else(|| "snapshot path has no parent directory".to_owned())?;
    let path = std::ffi::CString::new(parent.as_os_str().as_bytes())
        .map_err(|_| "snapshot parent path is not valid".to_owned())?;
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
fn sync_parent_directory(_path: &Path) -> Result<(), String> {
    Ok(())
}

#[async_trait]
impl SnapshotStore for JsonSnapshotStore {
    async fn load(&self) -> Result<PersistedState, String> {
        self.ensure_private_parent().await?;
        self.sweep_stale_temporary_files().await;
        #[cfg(unix)]
        return load_private_unix_snapshot(&self.path);
        #[cfg(windows)]
        return load_private_windows_snapshot(&self.path);

        #[cfg(not(any(unix, windows)))]
        {
            Err("snapshot storage is unsupported on this platform".into())
        }
    }

    async fn stage(&self, state: &PersistedState) -> Result<Box<dyn StagedSnapshot>, String> {
        self.ensure_private_parent().await?;
        #[cfg(not(any(unix, windows)))]
        return Err("snapshot storage is unsupported on this platform".into());
        #[cfg(windows)]
        validate_private_windows_destination(&self.path)?;
        let bytes = serde_json::to_vec(state).map_err(|error| error.to_string())?;
        let file_name = self
            .path
            .file_name()
            .and_then(|name| name.to_str())
            .ok_or_else(|| "snapshot path has no valid file name".to_owned())?;
        static TEMPORARY_SEQUENCE: AtomicU64 = AtomicU64::new(0);
        let sequence = TEMPORARY_SEQUENCE.fetch_add(1, Ordering::Relaxed);
        let temporary_path = self.path.with_file_name(format!(
            ".{file_name}.{}.{sequence}.tmp",
            std::process::id()
        ));

        #[cfg(windows)]
        let file = ullage_auth::create_private_windows_file(&temporary_path)
            .map_err(|error| error.to_string())?;
        #[cfg(not(windows))]
        let file = {
            let mut options = std::fs::OpenOptions::new();
            options.write(true).create_new(true);
            #[cfg(unix)]
            {
                use std::os::unix::fs::OpenOptionsExt;

                options.mode(0o600);
            }
            options
                .open(&temporary_path)
                .map_err(|error| error.to_string())?
        };
        let temporary = TemporarySnapshot {
            path: temporary_path.clone(),
        };
        let mut file = tokio::fs::File::from_std(file);
        use tokio::io::AsyncWriteExt;
        file.write_all(&bytes)
            .await
            .map_err(|error| error.to_string())?;
        file.sync_all().await.map_err(|error| error.to_string())?;
        drop(file);
        Ok(Box::new(StagedJsonSnapshot {
            temporary,
            destination: self.path.clone(),
        }))
    }
}

#[cfg(unix)]
fn load_private_unix_snapshot(path: &Path) -> Result<PersistedState, String> {
    use std::io::Read;
    use std::os::unix::fs::{MetadataExt, OpenOptionsExt, PermissionsExt};

    let mut options = std::fs::OpenOptions::new();
    options
        .read(true)
        .custom_flags(libc::O_CLOEXEC | libc::O_NOFOLLOW);
    let mut file = match options.open(path) {
        Ok(file) => file,
        Err(error) if error.kind() == ErrorKind::NotFound => {
            return Ok(PersistedState::default());
        }
        Err(error) if error.raw_os_error() == Some(libc::ELOOP) => {
            return Err("snapshot path must be a regular file".into());
        }
        Err(error) => return Err(error.to_string()),
    };
    let metadata = file.metadata().map_err(|error| error.to_string())?;
    // SAFETY: `geteuid` has no arguments and no memory-safety preconditions.
    if !metadata.is_file()
        || metadata.uid() != unsafe { libc::geteuid() }
        || metadata.permissions().mode() & 0o077 != 0
    {
        return Err("snapshot file must be private, regular, and owned by the current user".into());
    }
    let mut bytes = Vec::new();
    file.read_to_end(&mut bytes)
        .map_err(|error| error.to_string())?;
    serde_json::from_slice(&bytes).map_err(|error| error.to_string())
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
                    .ok_or_else(|| "snapshot path has no existing Windows anchor".to_owned())?;
            }
            Err(error) => return Err(error.to_string()),
        }
    }
    for directory in missing.into_iter().rev() {
        ullage_auth::create_private_windows_directory(&directory)
            .map_err(|error| error.to_string())?;
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
    let directory = options.open(path).map_err(|error| error.to_string())?;
    let metadata = directory.metadata().map_err(|error| error.to_string())?;
    if !metadata.is_dir()
        || metadata.file_attributes() & FILE_ATTRIBUTE_REPARSE_POINT != 0
        || !ullage_auth::windows_handle_acl_is_private(directory.as_raw_handle())
            .map_err(|error| error.to_string())?
    {
        return Err("snapshot parent must be a private non-reparse directory".into());
    }
    Ok(())
}

#[cfg(windows)]
fn open_private_windows_snapshot(path: &Path) -> Result<Option<std::fs::File>, String> {
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
        Err(error) => return Err(error.to_string()),
    };
    let metadata = file.metadata().map_err(|error| error.to_string())?;
    if !metadata.is_file()
        || metadata.file_attributes() & FILE_ATTRIBUTE_REPARSE_POINT != 0
        || !ullage_auth::windows_handle_acl_is_private(file.as_raw_handle())
            .map_err(|error| error.to_string())?
    {
        return Err("snapshot file must be private, regular, and non-reparse".into());
    }
    Ok(Some(file))
}

#[cfg(windows)]
fn load_private_windows_snapshot(path: &Path) -> Result<PersistedState, String> {
    use std::io::Read;

    let Some(mut file) = open_private_windows_snapshot(path)? else {
        return Ok(PersistedState::default());
    };
    let mut bytes = Vec::new();
    file.read_to_end(&mut bytes)
        .map_err(|error| error.to_string())?;
    serde_json::from_slice(&bytes).map_err(|error| error.to_string())
}

#[cfg(windows)]
fn validate_private_windows_destination(path: &Path) -> Result<(), String> {
    open_private_windows_snapshot(path).map(|_| ())
}

pub(crate) fn snapshot_for<'a>(
    snapshots: &'a SnapshotMap,
    account_id: &AccountId,
) -> Option<&'a SnapshotRecord> {
    snapshots.get(account_id)
}
