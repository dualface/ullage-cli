use std::collections::HashMap;
use std::fmt;
use std::future::Future;
use std::sync::{Arc, LazyLock, Mutex, Weak};

use sha2::{Digest, Sha256};
use thiserror::Error;
use tokio::sync::Notify;
use zeroize::Zeroize;

use crate::credential::StoredRecord;
use crate::{Credential, CredentialKey, CredentialVersion, StoredCredential};

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum BackendKind {
    MacOsKeychain,
    WindowsCredentialManager,
    LinuxSecretService,
    ExplicitFileFallback,
    OtherPlatform,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Availability {
    Available,
    Unavailable,
}

impl Availability {
    pub fn remediation(self) -> Option<&'static str> {
        match self {
            Self::Available => None,
            Self::Unavailable => Some(
                "unlock or start the current platform's credential service, then retry the probe",
            ),
        }
    }
}

#[derive(Clone, Debug, Error, PartialEq, Eq)]
pub enum CredentialError {
    #[error("invalid credential key component: {0}")]
    InvalidKey(&'static str),
    #[error("invalid credential field name")]
    InvalidFieldName,
    #[error("credential is too large")]
    CredentialTooLarge,
    #[error("stored credential is malformed or uses an unsupported format")]
    CorruptCredential,
    #[error("credential does not exist")]
    NotFound,
    #[error("credential revision overflow")]
    RevisionOverflow,
    #[error("credential backend is unavailable; unlock or start the platform credential service")]
    BackendUnavailable,
    #[error(
        "this machine has no Secret Service; set credentials.file_fallback to true in the Ullage configuration to store credentials as plaintext files"
    )]
    FileFallbackDisabled,
    #[error(
        "credential backend denied access; unlock the credential store and verify the current user's platform permissions"
    )]
    AccessDenied,
    #[error("credential backend operation failed without exposing secret material")]
    BackendFailure,
    #[error("file fallback path must be absolute, non-symlinked, and private to the current user")]
    UnsafeFallbackPath,
    #[error("file fallback I/O failed")]
    FileIo,
    #[error("credential store synchronization failed")]
    Synchronization,
}

impl CredentialError {
    pub fn provider_message(&self, fallback: impl Into<String>) -> String {
        match self {
            Self::FileFallbackDisabled => self.to_string(),
            _ => fallback.into(),
        }
    }
}

pub trait CredentialBackend: Send + Sync {
    fn kind(&self) -> BackendKind;
    /// Identifies handles that address the same underlying vault.
    fn coordination_scope(&self) -> BackendScope;
    fn probe(&self) -> Result<Availability, CredentialError>;
    fn read(&self, key: &CredentialKey) -> Result<Vec<u8>, CredentialError>;
    fn write(&self, key: &CredentialKey, value: &[u8]) -> Result<(), CredentialError>;
}

#[derive(Clone, Copy, PartialEq, Eq, Hash)]
pub struct BackendScope([u8; 32]);

impl BackendScope {
    pub fn new(identity: &[u8]) -> Self {
        Self(Sha256::digest(identity).into())
    }
}

impl fmt::Debug for BackendScope {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str("BackendScope([REDACTED])")
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum ReplaceOutcome {
    Replaced,
    VersionConflict,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum RefreshFailureKind {
    Authentication,
    Network,
    RateLimited,
    ProviderProtocol,
    Other,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct RefreshFailure {
    kind: RefreshFailureKind,
}

impl RefreshFailure {
    pub fn new(kind: RefreshFailureKind) -> Self {
        Self { kind }
    }

    pub fn kind(self) -> RefreshFailureKind {
        self.kind
    }
}

impl fmt::Display for RefreshFailure {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str("provider refresh failed without exposing secret material")
    }
}

impl std::error::Error for RefreshFailure {}

#[derive(Clone, PartialEq, Eq)]
pub enum RefreshError {
    Store(CredentialError),
    Refresh(RefreshFailure),
}

impl fmt::Debug for RefreshError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Store(error) => formatter.debug_tuple("Store").field(error).finish(),
            Self::Refresh(_) => formatter.write_str("Refresh([REDACTED])"),
        }
    }
}

impl fmt::Display for RefreshError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Store(error) => error.fmt(formatter),
            Self::Refresh(_) => {
                formatter.write_str("token refresh failed; provider error was redacted")
            }
        }
    }
}

impl std::error::Error for RefreshError {}

impl From<CredentialError> for RefreshError {
    fn from(value: CredentialError) -> Self {
        Self::Store(value)
    }
}

#[derive(Clone, PartialEq, Eq, Hash)]
struct FlightKey {
    credential: CredentialKey,
}

struct RefreshFlight {
    observed_version: CredentialVersion,
    result: Mutex<Option<Result<StoredCredential, RefreshError>>>,
    completed: Notify,
}

#[derive(Default)]
struct Coordination {
    operation_locks: Mutex<HashMap<CredentialKey, Weak<Mutex<()>>>>,
    refresh_flights: Mutex<HashMap<FlightKey, Weak<RefreshFlight>>>,
}

static COORDINATION_SCOPES: LazyLock<Mutex<HashMap<BackendScope, Weak<Coordination>>>> =
    LazyLock::new(|| Mutex::new(HashMap::new()));

struct FlightLeaderGuard {
    coordination: Arc<Coordination>,
    key: FlightKey,
    flight: Arc<RefreshFlight>,
    armed: bool,
}

impl FlightLeaderGuard {
    fn publish(
        mut self,
        result: Result<StoredCredential, RefreshError>,
    ) -> Result<(), CredentialError> {
        *self
            .flight
            .result
            .lock()
            .map_err(|_| CredentialError::Synchronization)? = Some(result);
        self.flight.completed.notify_waiters();
        self.remove_from_registry()?;
        self.armed = false;
        Ok(())
    }

    fn remove_from_registry(&self) -> Result<(), CredentialError> {
        let mut flights = self
            .coordination
            .refresh_flights
            .lock()
            .map_err(|_| CredentialError::Synchronization)?;
        if flights
            .get(&self.key)
            .and_then(Weak::upgrade)
            .is_some_and(|current| Arc::ptr_eq(&current, &self.flight))
        {
            flights.remove(&self.key);
        }
        Ok(())
    }
}

impl Drop for FlightLeaderGuard {
    fn drop(&mut self) {
        if !self.armed {
            return;
        }
        if let Ok(mut result) = self.flight.result.lock() {
            if result.is_none() {
                *result = Some(Err(RefreshError::Store(CredentialError::Synchronization)));
            }
        }
        self.flight.completed.notify_waiters();
        let _ = self.remove_from_registry();
    }
}

pub struct CredentialStore {
    backend: Arc<dyn CredentialBackend>,
    coordination: Arc<Coordination>,
}

impl CredentialStore {
    pub fn new(backend: impl CredentialBackend + 'static) -> Self {
        let scope = backend.coordination_scope();
        Self {
            backend: Arc::new(backend),
            coordination: coordination_for(scope),
        }
    }

    pub fn backend_kind(&self) -> BackendKind {
        self.backend.kind()
    }

    pub fn probe(&self) -> Result<Availability, CredentialError> {
        self.backend.probe()
    }

    pub fn get(&self, key: &CredentialKey) -> Result<StoredCredential, CredentialError> {
        let lock = self.operation_lock(key)?;
        let _guard = lock.lock().map_err(|_| CredentialError::Synchronization)?;
        self.get_unlocked(key)
    }

    pub fn set(
        &self,
        key: &CredentialKey,
        credential: Credential,
    ) -> Result<StoredCredential, CredentialError> {
        let lock = self.operation_lock(key)?;
        let _guard = lock.lock().map_err(|_| CredentialError::Synchronization)?;
        let version = match self.read_optional_record(key)? {
            Some(StoredRecord::Active(stored)) => CredentialVersion::new(
                stored.version().generation(),
                stored
                    .revision()
                    .checked_add(1)
                    .ok_or(CredentialError::RevisionOverflow)?,
            ),
            Some(StoredRecord::Tombstone { generation }) => CredentialVersion::new(generation, 1),
            None => CredentialVersion::new(1, 1),
        };
        let stored = StoredCredential::new(version, credential);
        self.write_record(key, &StoredRecord::Active(stored.clone()))?;
        Ok(stored)
    }

    pub fn replace(
        &self,
        key: &CredentialKey,
        expected_version: CredentialVersion,
        credential: Credential,
    ) -> Result<ReplaceOutcome, CredentialError> {
        let lock = self.operation_lock(key)?;
        let _guard = lock.lock().map_err(|_| CredentialError::Synchronization)?;
        let current = self.get_unlocked(key)?;
        if current.version() != expected_version {
            return Ok(ReplaceOutcome::VersionConflict);
        }
        let revision = expected_version
            .revision()
            .checked_add(1)
            .ok_or(CredentialError::RevisionOverflow)?;
        let stored = StoredCredential::new(
            CredentialVersion::new(expected_version.generation(), revision),
            credential,
        );
        self.write_record(key, &StoredRecord::Active(stored))?;
        Ok(ReplaceOutcome::Replaced)
    }

    pub fn delete(&self, key: &CredentialKey) -> Result<(), CredentialError> {
        let lock = self.operation_lock(key)?;
        let _guard = lock.lock().map_err(|_| CredentialError::Synchronization)?;
        let current = self.read_record(key)?.active()?;
        let generation = current
            .version()
            .generation()
            .checked_add(1)
            .ok_or(CredentialError::RevisionOverflow)?;
        self.write_record(key, &StoredRecord::Tombstone { generation })
    }

    pub async fn refresh<F, Fut>(
        &self,
        key: &CredentialKey,
        observed_version: CredentialVersion,
        refresher: F,
    ) -> Result<StoredCredential, RefreshError>
    where
        F: FnOnce(StoredCredential) -> Fut,
        Fut: Future<Output = Result<Credential, RefreshFailure>>,
    {
        let flight_key = FlightKey {
            credential: key.clone(),
        };
        let flight = loop {
            let (flight, leader) = self.refresh_flight(&flight_key, observed_version)?;
            if leader {
                break flight;
            }
            let shared_version = flight.observed_version;
            let result = Self::wait_for_flight(&flight).await;
            if shared_version == observed_version {
                return result;
            }
        };
        let leader_guard = FlightLeaderGuard {
            coordination: Arc::clone(&self.coordination),
            key: flight_key.clone(),
            flight: Arc::clone(&flight),
            armed: true,
        };

        let result = match self.get(key).map_err(RefreshError::Store) {
            Ok(current) if current.version() != observed_version => Ok(current),
            Ok(current) => match refresher(current).await.map_err(RefreshError::Refresh) {
                Ok(replacement) => match self.replace(key, observed_version, replacement) {
                    Ok(ReplaceOutcome::Replaced | ReplaceOutcome::VersionConflict) => {
                        self.get(key).map_err(RefreshError::Store)
                    }
                    Err(error) => Err(RefreshError::Store(error)),
                },
                Err(error) => Err(error),
            },
            Err(error) => Err(error),
        };
        leader_guard.publish(result.clone())?;
        result
    }

    fn read_record(&self, key: &CredentialKey) -> Result<StoredRecord, CredentialError> {
        StoredRecord::decode(self.backend.read(key)?)
    }

    fn get_unlocked(&self, key: &CredentialKey) -> Result<StoredCredential, CredentialError> {
        self.read_record(key)?.active()
    }

    fn read_optional_record(
        &self,
        key: &CredentialKey,
    ) -> Result<Option<StoredRecord>, CredentialError> {
        match self.backend.read(key) {
            Ok(raw) => StoredRecord::decode(raw).map(Some),
            Err(CredentialError::NotFound) => Ok(None),
            Err(error) => Err(error),
        }
    }

    fn write_record(
        &self,
        key: &CredentialKey,
        record: &StoredRecord,
    ) -> Result<(), CredentialError> {
        let mut encoded = record.encode()?;
        let result = self.backend.write(key, &encoded);
        encoded.zeroize();
        result
    }

    async fn wait_for_flight(flight: &RefreshFlight) -> Result<StoredCredential, RefreshError> {
        loop {
            let notified = flight.completed.notified();
            if let Some(result) = flight
                .result
                .lock()
                .map_err(|_| CredentialError::Synchronization)?
                .clone()
            {
                return result;
            }
            notified.await;
        }
    }

    fn refresh_flight(
        &self,
        key: &FlightKey,
        observed_version: CredentialVersion,
    ) -> Result<(Arc<RefreshFlight>, bool), CredentialError> {
        let mut flights = self
            .coordination
            .refresh_flights
            .lock()
            .map_err(|_| CredentialError::Synchronization)?;
        flights.retain(|_, flight| flight.strong_count() > 0);
        if let Some(flight) = flights.get(key).and_then(Weak::upgrade) {
            return Ok((flight, false));
        }
        let flight = Arc::new(RefreshFlight {
            observed_version,
            result: Mutex::new(None),
            completed: Notify::new(),
        });
        flights.insert(key.clone(), Arc::downgrade(&flight));
        Ok((flight, true))
    }

    fn operation_lock(&self, key: &CredentialKey) -> Result<Arc<Mutex<()>>, CredentialError> {
        let mut locks = self
            .coordination
            .operation_locks
            .lock()
            .map_err(|_| CredentialError::Synchronization)?;
        locks.retain(|_, lock| lock.strong_count() > 0);
        if let Some(lock) = locks.get(key).and_then(Weak::upgrade) {
            return Ok(lock);
        }
        let lock = Arc::new(Mutex::new(()));
        locks.insert(key.clone(), Arc::downgrade(&lock));
        Ok(lock)
    }
}

fn coordination_for(scope: BackendScope) -> Arc<Coordination> {
    let mut scopes = COORDINATION_SCOPES
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner);
    scopes.retain(|_, coordination| coordination.strong_count() > 0);
    if let Some(coordination) = scopes.get(&scope).and_then(Weak::upgrade) {
        return coordination;
    }
    let coordination = Arc::new(Coordination::default());
    scopes.insert(scope, Arc::downgrade(&coordination));
    coordination
}

impl fmt::Debug for CredentialStore {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter
            .debug_struct("CredentialStore")
            .field("backend", &self.backend.kind())
            .field("credentials", &"[REDACTED]")
            .finish()
    }
}
