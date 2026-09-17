use std::path::{Path, PathBuf};
use std::sync::Arc;

use async_trait::async_trait;
use tokio::sync::RwLock;

use crate::model::{AccountId, PersistedState, SnapshotMap, SnapshotRecord};
use crate::privatefs::{
    self, ErrorStyle, FileStrictness, TemporaryFile, commit_temporary, create_private_temporary,
    temporary_path,
};

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

struct StagedJsonSnapshot {
    temporary: TemporaryFile,
    destination: PathBuf,
}

#[async_trait]
impl StagedSnapshot for StagedJsonSnapshot {
    async fn commit(self: Box<Self>) -> Result<(), String> {
        commit_temporary(&self.temporary.0, &self.destination, ErrorStyle::Snapshot)
    }
}

impl JsonSnapshotStore {
    pub fn new(path: impl Into<PathBuf>) -> Self {
        Self { path: path.into() }
    }

    pub fn path(&self) -> &Path {
        &self.path
    }
}

#[async_trait]
impl SnapshotStore for JsonSnapshotStore {
    async fn load(&self) -> Result<PersistedState, String> {
        privatefs::ensure_private_parent(&self.path, ErrorStyle::Snapshot)?;
        privatefs::sweep_stale_temporary_files(&self.path);
        let Some(bytes) = privatefs::read_private_file(
            &self.path,
            FileStrictness::OwnerOnly,
            ErrorStyle::Snapshot,
        )?
        else {
            return Ok(PersistedState::default());
        };
        serde_json::from_slice(&bytes).map_err(|error| error.to_string())
    }

    async fn stage(&self, state: &PersistedState) -> Result<Box<dyn StagedSnapshot>, String> {
        privatefs::ensure_private_parent(&self.path, ErrorStyle::Snapshot)?;
        #[cfg(not(any(unix, windows)))]
        return Err("snapshot storage is unsupported on this platform".into());
        #[cfg(windows)]
        privatefs::validate_private_file(
            &self.path,
            FileStrictness::OwnerOnly,
            ErrorStyle::Snapshot,
        )?;
        let bytes = serde_json::to_vec(state).map_err(|error| error.to_string())?;
        let temporary_path = temporary_path(&self.path, ErrorStyle::Snapshot)?;

        let file = create_private_temporary(&temporary_path, ErrorStyle::Snapshot)?;
        let temporary = TemporaryFile(temporary_path.clone());
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

pub(crate) fn snapshot_for<'a>(
    snapshots: &'a SnapshotMap,
    account_id: &AccountId,
) -> Option<&'a SnapshotRecord> {
    snapshots.get(account_id)
}
