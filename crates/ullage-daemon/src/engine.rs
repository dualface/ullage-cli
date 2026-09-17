use std::collections::{BTreeMap, HashMap, HashSet};
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::{Arc, Mutex as StdMutex, RwLock as StdRwLock};
use std::time::Duration;

use async_trait::async_trait;
use chrono::{DateTime, Utc};
use tokio::sync::{Mutex, Notify, RwLock, Semaphore, watch};
use tokio::task::JoinSet;
use ullage_core::{ProviderError, ProviderId, ProviderRegistry, QueryOutcome, SubscriptionUsage};

use crate::model::{
    AccountConfig, AccountId, AccountStatus, DaemonConfig, DaemonError, DaemonStatus,
    FailureRecord, PersistedState, ProbeError, ProbeTrigger, SanitizedError, SnapshotMap,
    SnapshotRecord, sanitize_provider_error,
};
use crate::store::{SnapshotStore, snapshot_for};

mod accounts;

#[async_trait]
pub trait Clock: Send + Sync {
    fn now(&self) -> DateTime<Utc>;
    async fn sleep(&self, duration: Duration);
}

#[derive(Clone, Copy, Debug, Default)]
pub struct SystemClock;

#[async_trait]
impl Clock for SystemClock {
    fn now(&self) -> DateTime<Utc> {
        Utc::now()
    }

    async fn sleep(&self, duration: Duration) {
        tokio::time::sleep(duration).await;
    }
}

#[derive(Clone)]
pub struct DaemonEngine {
    inner: Arc<EngineInner>,
}

struct EngineInner {
    registry: Arc<ProviderRegistry>,
    clock: Arc<dyn Clock>,
    store: Arc<dyn SnapshotStore>,
    snapshots: RwLock<SnapshotMap>,
    failures: RwLock<BTreeMap<AccountId, FailureRecord>>,
    persist_lock: Mutex<()>,
    accounts: RwLock<BTreeMap<AccountId, Arc<AccountRuntime>>>,
    /// Ids retired by removal. The set is never compacted: a tombstone keeps a
    /// recreated account from being mistaken for the removed one.
    removed_accounts: RwLock<std::collections::BTreeSet<AccountId>>,
    accounts_changed: Notify,
    next_account_id: AtomicU64,
    global_limit: Arc<Semaphore>,
    provider_limits: HashMap<ProviderId, Arc<Semaphore>>,
    shutdown: watch::Sender<bool>,
    admission: StdMutex<FlightAdmission>,
    flights_idle: Notify,
    #[cfg(test)]
    test_panic_run_account: AtomicBool,
    #[cfg(test)]
    test_panic_flight_supervisor: AtomicBool,
}

#[derive(Default)]
struct FlightAdmission {
    closed: bool,
    active: usize,
}

struct AccountRuntime {
    config: StdRwLock<AccountConfig>,
    removed: AtomicBool,
    state: Mutex<AccountRuntimeState>,
    schedule_changed: Notify,
}

/// Delay before a crashed per-account task is restarted, so a deterministic
/// panic cannot spin the scheduler.
const ACCOUNT_TASK_RESTART_DELAY: Duration = Duration::from_secs(1);

/// Upper bound honored for a provider-supplied `Retry-After`. Without it a
/// hostile or broken provider could park an account for decades.
const MAXIMUM_RETRY_AFTER: Duration = Duration::from_secs(24 * 60 * 60);

/// Bound on a single `SnapshotStore::stage` call. A store that never returns
/// must not wedge a flight and every waiter attached to it.
const STORE_STAGE_TIMEOUT: Duration = Duration::from_secs(30);

impl AccountRuntime {
    fn config(&self) -> AccountConfig {
        self.config
            .read()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .clone()
    }
}

#[derive(Default)]
struct AccountRuntimeState {
    current_flight: Option<Arc<FlightState>>,
    consecutive_failures: u32,
    next_probe_at: Option<DateTime<Utc>>,
}

struct FlightState {
    /// `StdMutex` so the watchdog `Drop` can still publish a result while the
    /// supervisor task unwinds, where awaiting a tokio mutex is impossible.
    result: StdMutex<Option<Result<QueryOutcome<SubscriptionUsage>, ProbeError>>>,
    completion: Notify,
}

/// Publishes a failure when the flight supervisor task dies before storing a
/// result, so probe waiters are never left on a flight that cannot finish.
struct FlightWatchdog {
    inner: Arc<EngineInner>,
    account: Arc<AccountRuntime>,
    flight: Arc<FlightState>,
}

impl Drop for FlightWatchdog {
    fn drop(&mut self) {
        {
            let mut result = self
                .flight
                .result
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner);
            if result.is_some() {
                return;
            }
            *result = Some(Err(ProbeError::Storage(
                "daemon flight supervisor failed".into(),
            )));
        }
        self.flight.completion.notify_waiters();
        // Best effort only: the lock is held for microseconds elsewhere, and a
        // missed cleanup is repaired on the next `start_probe_account` call.
        if let Ok(mut state) = self.account.state.try_lock() {
            if state
                .current_flight
                .as_ref()
                .is_some_and(|current| Arc::ptr_eq(current, &self.flight))
            {
                state.current_flight = None;
            }
            state.consecutive_failures = state.consecutive_failures.saturating_add(1);
            let config = self.account.config();
            state.next_probe_at = if config.enabled
                && !self.account.removed.load(Ordering::Acquire)
                && !*self.inner.shutdown.borrow()
            {
                let delay = next_delay(
                    &config,
                    state.consecutive_failures,
                    Some(&ProbeError::Storage(
                        "daemon flight supervisor failed".into(),
                    )),
                );
                let delta = chrono::Duration::from_std(delay).unwrap_or(chrono::Duration::MAX);
                Some(
                    self.inner
                        .clock
                        .now()
                        .checked_add_signed(delta)
                        .unwrap_or(DateTime::<Utc>::MAX_UTC),
                )
            } else {
                None
            };
            drop(state);
            self.account.schedule_changed.notify_waiters();
        }
    }
}

struct FlightCompletion {
    result: Result<QueryOutcome<SubscriptionUsage>, ProbeError>,
    consecutive_failures: u32,
}

struct FlightActivityGuard {
    inner: Arc<EngineInner>,
}

impl Drop for FlightActivityGuard {
    fn drop(&mut self) {
        let became_idle = {
            let mut admission = self
                .inner
                .admission
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner);
            admission.active = admission.active.saturating_sub(1);
            admission.active == 0
        };
        if became_idle {
            self.inner.flights_idle.notify_waiters();
        }
    }
}

impl DaemonEngine {
    pub async fn new(
        config: DaemonConfig,
        registry: Arc<ProviderRegistry>,
        clock: Arc<dyn Clock>,
        store: Arc<dyn SnapshotStore>,
    ) -> Result<Self, DaemonError> {
        if config.maximum_concurrency == 0 {
            return Err(DaemonError::InvalidGlobalConcurrency);
        }
        if config.default_provider_concurrency == 0 {
            return Err(DaemonError::InvalidProviderConcurrency);
        }
        let mut provider_limits = HashMap::new();
        for limit in config.provider_limits {
            if limit.maximum_concurrency == 0 {
                return Err(DaemonError::InvalidProviderLimit(limit.provider));
            }
            provider_limits.insert(
                limit.provider,
                Arc::new(Semaphore::new(limit.maximum_concurrency)),
            );
        }
        for descriptor in registry.descriptors() {
            provider_limits
                .entry(descriptor.id)
                .or_insert_with(|| Arc::new(Semaphore::new(config.default_provider_concurrency)));
        }
        let load_store = store.clone();
        let persisted = tokio::spawn(async move { load_store.load().await })
            .await
            .map_err(|_| DaemonError::Storage("snapshot load task failed".into()))?
            .map_err(DaemonError::Storage)?;
        let next_account_sequence = persisted
            .accounts
            .keys()
            .chain(persisted.snapshots.keys())
            .chain(persisted.removed_accounts.iter())
            .filter_map(|id| id.as_str().strip_prefix("account-")?.parse::<u64>().ok())
            .max()
            // Saturate at u64::MAX instead of wrapping back to account-1.
            .map(|sequence| sequence.checked_add(1).unwrap_or(u64::MAX))
            .unwrap_or(1)
            .max(persisted.next_account_sequence);
        let accounts = persisted
            .accounts
            .into_iter()
            .map(|(id, mut config)| {
                // States saved before `MAX_ACCOUNT_TIMEOUT` existed — or
                // edited by hand — may hold a timeout the client transport
                // cannot wait out; migrate them into the contract instead of
                // failing startup over legacy state.
                if config.timeout.is_zero() || config.timeout > ullage_protocol::MAX_ACCOUNT_TIMEOUT
                {
                    config.timeout = ullage_protocol::MAX_ACCOUNT_TIMEOUT;
                }
                (
                    id,
                    Arc::new(AccountRuntime {
                        config: StdRwLock::new(config),
                        removed: AtomicBool::new(false),
                        state: Mutex::new(AccountRuntimeState::default()),
                        schedule_changed: Notify::new(),
                    }),
                )
            })
            .collect();
        let (shutdown, _) = watch::channel(false);
        Ok(Self {
            inner: Arc::new(EngineInner {
                registry,
                clock,
                store,
                snapshots: RwLock::new(persisted.snapshots),
                failures: RwLock::new(persisted.failures),
                persist_lock: Mutex::new(()),
                accounts: RwLock::new(accounts),
                removed_accounts: RwLock::new(persisted.removed_accounts),
                accounts_changed: Notify::new(),
                next_account_id: AtomicU64::new(next_account_sequence),
                global_limit: Arc::new(Semaphore::new(config.maximum_concurrency)),
                provider_limits,
                shutdown,
                admission: StdMutex::new(FlightAdmission::default()),
                flights_idle: Notify::new(),
                #[cfg(test)]
                test_panic_run_account: AtomicBool::new(false),
                #[cfg(test)]
                test_panic_flight_supervisor: AtomicBool::new(false),
            }),
        })
    }

    pub async fn add_account(&self, config: AccountConfig) -> Result<(), DaemonError> {
        if config.interval.is_zero() {
            return Err(DaemonError::InvalidInterval(config.id));
        }
        if config.timeout.is_zero() || config.timeout > ullage_protocol::MAX_ACCOUNT_TIMEOUT {
            return Err(DaemonError::InvalidTimeout(config.id));
        }
        if config.backoff.initial.is_zero()
            || config.backoff.maximum.is_zero()
            || config.backoff.initial > config.backoff.maximum
        {
            return Err(DaemonError::InvalidBackoff(config.id));
        }
        let id = config.id.clone();
        let runtime = Arc::new(AccountRuntime {
            config: StdRwLock::new(config),
            removed: AtomicBool::new(false),
            state: Mutex::new(AccountRuntimeState::default()),
            schedule_changed: Notify::new(),
        });
        let mut accounts = self.inner.accounts.write().await;
        // A tombstone in `removed_accounts` does not block re-adding: it exists
        // so generated ids are never reused and stale flights can tell the new
        // runtime from the removed one.
        if accounts.contains_key(&id) {
            return Err(DaemonError::DuplicateAccount(id));
        }
        let runtime_config = runtime.config();
        // Only a named account claims a selector. Two unnamed accounts of one
        // provider are the ordinary way to add a second sign-in before either
        // has been named, so they do not collide.
        if runtime_config.query.account_label.is_some()
            && accounts.values().any(|account| {
                let account_config = account.config();
                account_config.provider == runtime_config.provider
                    && account_config.query.account_label == runtime_config.query.account_label
            })
        {
            return Err(DaemonError::DuplicateAccountSelector {
                provider: runtime_config.provider,
                account_label: runtime_config.query.account_label,
            });
        }
        accounts.insert(id.clone(), runtime);
        if let Err(error) = self.persist_accounts_spawned(&accounts).await {
            accounts.remove(&id);
            return Err(error);
        }
        drop(accounts);
        self.inner.accounts_changed.notify_one();
        Ok(())
    }

    pub async fn next_account_id(&self) -> AccountId {
        loop {
            let sequence = self.inner.next_account_id.fetch_add(1, Ordering::Relaxed);
            let candidate = AccountId::new(format!("account-{sequence}"));
            if !self.inner.accounts.read().await.contains_key(&candidate)
                && !self
                    .inner
                    .removed_accounts
                    .read()
                    .await
                    .contains(&candidate)
            {
                return candidate;
            }
        }
    }

    pub async fn account_configs(&self) -> Vec<AccountConfig> {
        self.inner
            .accounts
            .read()
            .await
            .values()
            .map(|account| account.config())
            .collect()
    }

    pub async fn account_config(&self, account_id: &AccountId) -> Option<AccountConfig> {
        self.inner
            .accounts
            .read()
            .await
            .get(account_id)
            .map(|account| account.config())
    }

    pub async fn account_was_removed(&self, account_id: &AccountId) -> bool {
        self.inner
            .removed_accounts
            .read()
            .await
            .contains(account_id)
    }

    /// Stages and commits the whole account map under `persist_lock`.
    ///
    /// The caller keeps the `accounts` lock held across this call so the staged
    /// snapshot cannot fall behind a concurrent mutation, and `persist_lock`
    /// keeps commits in the same order. The storage I/O this spans is a small
    /// local JSON write; a slow disk stalls account mutation, never data.
    async fn persist_accounts(
        &self,
        accounts: &BTreeMap<AccountId, Arc<AccountRuntime>>,
    ) -> Result<(), DaemonError> {
        let _guard = self.inner.persist_lock.lock().await;
        let state = PersistedState {
            accounts: accounts
                .iter()
                .map(|(id, account)| (id.clone(), account.config()))
                .collect(),
            removed_accounts: self.inner.removed_accounts.read().await.clone(),
            next_account_sequence: self.inner.next_account_id.load(Ordering::Relaxed),
            snapshots: self.inner.snapshots.read().await.clone(),
            failures: self.inner.failures.read().await.clone(),
        };
        match tokio::time::timeout(STORE_STAGE_TIMEOUT, self.inner.store.stage(&state)).await {
            Ok(Ok(staged)) => staged.commit().await.map_err(DaemonError::Storage),
            Ok(Err(error)) => Err(DaemonError::Storage(error)),
            Err(_) => Err(DaemonError::Storage("snapshot stage timed out".into())),
        }
    }

    /// Persists `accounts` in a spawned task so a cancelled caller cannot drop
    /// the write between stage and commit. The map is captured under the
    /// caller's lock, matching the spawn scheme `remove_account` uses.
    async fn persist_accounts_spawned(
        &self,
        accounts: &BTreeMap<AccountId, Arc<AccountRuntime>>,
    ) -> Result<(), DaemonError> {
        let engine = self.clone();
        let accounts = accounts.clone();
        tokio::spawn(async move { engine.persist_accounts(&accounts).await })
            .await
            .map_err(|_| DaemonError::Storage("account persist task failed".into()))?
    }

    pub async fn remove_account(
        &self,
        account_id: &AccountId,
    ) -> Result<Option<AccountConfig>, DaemonError> {
        let activity = {
            let mut admission = self
                .inner
                .admission
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner);
            if admission.closed {
                return Err(DaemonError::Cancelled);
            }
            admission.active = admission.active.saturating_add(1);
            FlightActivityGuard {
                inner: self.inner.clone(),
            }
        };
        let engine = self.clone();
        let account_id = account_id.clone();
        tokio::spawn(async move {
            let _activity = activity;
            engine.remove_account_transaction(&account_id).await
        })
        .await
        .map_err(|_| DaemonError::Storage("account removal task failed".into()))?
    }

    async fn remove_account_transaction(
        &self,
        account_id: &AccountId,
    ) -> Result<Option<AccountConfig>, DaemonError> {
        let mut accounts = self.inner.accounts.write().await;
        let account = match accounts.remove(account_id) {
            Some(account) => account,
            None => return Ok(None),
        };
        account.removed.store(true, Ordering::Release);
        account.schedule_changed.notify_one();
        self.inner.accounts_changed.notify_one();
        let _guard = self.inner.persist_lock.lock().await;
        let mut snapshots = self.inner.snapshots.read().await.clone();
        let mut failures = self.inner.failures.read().await.clone();
        snapshots.remove(account_id);
        failures.remove(account_id);
        let mut removed_accounts = self.inner.removed_accounts.read().await.clone();
        removed_accounts.insert(account_id.clone());
        let staged = PersistedState {
            accounts: accounts
                .iter()
                .map(|(id, account)| (id.clone(), account.config()))
                .collect(),
            removed_accounts: removed_accounts.clone(),
            next_account_sequence: self.inner.next_account_id.load(Ordering::Relaxed),
            snapshots: snapshots.clone(),
            failures: failures.clone(),
        };
        let stage_store = self.inner.store.clone();
        let stage_state = staged.clone();
        let mut stage_task = tokio::spawn(async move {
            match tokio::time::timeout(STORE_STAGE_TIMEOUT, stage_store.stage(&stage_state)).await {
                Ok(result) => result,
                Err(_) => Err("snapshot stage timed out".into()),
            }
        });
        let mut shutdown = self.inner.shutdown.subscribe();
        let staged = if *shutdown.borrow() {
            stage_task.abort();
            let _ = stage_task.await;
            Err(DaemonError::Cancelled)
        } else {
            tokio::select! {
                joined = &mut stage_task => match joined {
                    Ok(Ok(staged)) => Ok(staged),
                    Ok(Err(error)) => Err(DaemonError::Storage(error)),
                    Err(_) => Err(DaemonError::Storage("snapshot stage task failed".into())),
                },
                changed = shutdown.changed() => {
                    let _ = changed;
                    stage_task.abort();
                    let _ = stage_task.await;
                    Err(DaemonError::Cancelled)
                }
            }
        };
        let staged = match staged {
            Ok(staged) => staged,
            Err(error) => {
                account.removed.store(false, Ordering::Release);
                accounts.insert(account_id.clone(), account);
                self.inner.accounts_changed.notify_one();
                return Err(error);
            }
        };
        let commit_admitted = {
            let admission = self
                .inner
                .admission
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner);
            !admission.closed
        };
        if !commit_admitted {
            account.removed.store(false, Ordering::Release);
            accounts.insert(account_id.clone(), account);
            self.inner.accounts_changed.notify_one();
            return Err(DaemonError::Cancelled);
        }
        // Acquiring the admission gate while it is open is the deletion
        // commit linearization point shared with `shutdown()`.
        let commit_task = tokio::spawn(async move { staged.commit().await });
        let persisted = match commit_task.await {
            Ok(result) => result,
            Err(_) => {
                self.shutdown();
                return Err(DaemonError::Storage("snapshot commit task failed".into()));
            }
        };
        if let Err(error) = persisted {
            account.removed.store(false, Ordering::Release);
            accounts.insert(account_id.clone(), account);
            self.inner.accounts_changed.notify_one();
            return Err(DaemonError::Storage(error));
        }
        *self.inner.snapshots.write().await = snapshots;
        *self.inner.failures.write().await = failures;
        *self.inner.removed_accounts.write().await = removed_accounts;
        drop(accounts);
        Ok(Some(account.config()))
    }

    pub async fn run(&self) {
        let mut tasks = JoinSet::new();
        let mut started = HashSet::new();
        // A `JoinError` carries no task output, so the account owning each
        // spawned task is tracked by its task id and cleaned up either way.
        let mut task_accounts = HashMap::new();

        let mut shutdown = self.inner.shutdown.subscribe();
        loop {
            let accounts: Vec<_> = self.inner.accounts.read().await.values().cloned().collect();
            for account in accounts {
                let account_id = account.config().id;
                if started.insert(account_id.clone()) {
                    let engine = self.clone();
                    let task_account = account_id.clone();
                    let handle = tasks.spawn(async move {
                        engine.run_account(account).await;
                        account_id
                    });
                    task_accounts.insert(handle.id(), task_account);
                }
            }
            if *shutdown.borrow() {
                break;
            }
            tokio::select! {
                changed = shutdown.changed() => {
                    if changed.is_err() || *shutdown.borrow() {
                        break;
                    }
                }
                joined = tasks.join_next_with_id(), if !tasks.is_empty() => {
                    match joined {
                        Some(Ok((task_id, account_id))) => {
                            task_accounts.remove(&task_id);
                            started.remove(&account_id);
                        }
                        Some(Err(error)) => {
                            if let Some(account_id) = task_accounts.remove(&error.id()) {
                                started.remove(&account_id);
                                eprintln!(
                                    "ullage daemon account task {account_id} failed: {error}"
                                );
                                let account = self
                                    .inner
                                    .accounts
                                    .read()
                                    .await
                                    .get(&account_id)
                                    .cloned();
                                let restartable = account.as_ref().is_some_and(|account| {
                                    !account.removed.load(Ordering::Acquire)
                                        && !*self.inner.shutdown.borrow()
                                });
                                if restartable {
                                    // Restart under a short delay so a task that
                                    // panics every run cannot spin the loop.
                                    let account = account.unwrap();
                                    started.insert(account_id.clone());
                                    let engine = self.clone();
                                    let task_account = account_id.clone();
                                    let handle = tasks.spawn(async move {
                                        let mut shutdown = engine.inner.shutdown.subscribe();
                                        tokio::select! {
                                            _ = tokio::time::sleep(ACCOUNT_TASK_RESTART_DELAY) => {
                                                engine.run_account(account).await;
                                            }
                                            changed = shutdown.changed() => {
                                                let _ = changed;
                                            }
                                        }
                                        account_id
                                    });
                                    task_accounts.insert(handle.id(), task_account);
                                }
                            }
                        }
                        None => {}
                    }
                }
                _ = self.inner.accounts_changed.notified() => {}
            }
        }
        while tasks.join_next().await.is_some() {}
        self.wait_for_idle().await;
    }

    pub fn shutdown(&self) {
        self.inner
            .admission
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .closed = true;
        self.inner.shutdown.send_replace(true);
    }

    pub fn is_shutting_down(&self) -> bool {
        *self.inner.shutdown.borrow()
    }

    pub(crate) async fn wait_for_shutdown(&self) {
        let mut shutdown = self.inner.shutdown.subscribe();
        while !*shutdown.borrow() {
            if shutdown.changed().await.is_err() {
                break;
            }
        }
    }

    pub(crate) async fn wait_for_idle(&self) {
        loop {
            let idle = self.inner.flights_idle.notified();
            tokio::pin!(idle);
            idle.as_mut().enable();
            if self
                .inner
                .admission
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner)
                .active
                == 0
            {
                return;
            }
            idle.await;
        }
    }

    pub(crate) fn registry(&self) -> Arc<ProviderRegistry> {
        self.inner.registry.clone()
    }

    pub async fn probe(
        &self,
        account_id: &AccountId,
        trigger: ProbeTrigger,
    ) -> Result<QueryOutcome<SubscriptionUsage>, ProbeError> {
        self.probe_unsanitized(account_id, trigger)
            .await
            .map_err(sanitize_probe_error)
    }

    /// The live runtime for `account_id`, or `AccountNotFound` when it is
    /// missing or already removed.
    async fn live_account(
        &self,
        account_id: &AccountId,
    ) -> Result<Arc<AccountRuntime>, ProbeError> {
        let account = self
            .inner
            .accounts
            .read()
            .await
            .get(account_id)
            .cloned()
            .ok_or_else(|| ProbeError::AccountNotFound(account_id.clone()))?;
        if account.removed.load(Ordering::Acquire) {
            return Err(ProbeError::AccountNotFound(account_id.clone()));
        }
        Ok(account)
    }

    /// Probe without sanitizing the returned provider error. The control
    /// handler uses this only to attach opt-in diagnostics, then sanitizes
    /// the `ControlError` payload.
    pub(crate) async fn probe_unsanitized(
        &self,
        account_id: &AccountId,
        trigger: ProbeTrigger,
    ) -> Result<QueryOutcome<SubscriptionUsage>, ProbeError> {
        let account = self.live_account(account_id).await?;
        self.probe_account(account, trigger).await
    }

    pub async fn start_probe(
        &self,
        account_id: &AccountId,
        trigger: ProbeTrigger,
    ) -> Result<(), ProbeError> {
        let account = self.live_account(account_id).await?;
        self.start_probe_account(account, trigger).await.map(drop)
    }

    pub async fn find_account(
        &self,
        provider: &ProviderId,
        account_label: Option<&str>,
    ) -> Option<AccountId> {
        self.inner
            .accounts
            .read()
            .await
            .values()
            .find(|account| {
                let config = account.config();
                config.provider == *provider
                    && config.query.account_label.as_deref() == account_label
            })
            .map(|account| account.config().id)
    }

    pub async fn show(&self, account_id: &AccountId) -> Option<SnapshotRecord> {
        let snapshots = self.inner.snapshots.read().await;
        snapshot_for(&snapshots, account_id).cloned()
    }

    pub(crate) async fn show_configured_account(
        &self,
        account_id: &AccountId,
    ) -> Result<Option<SnapshotRecord>, ProbeError> {
        let accounts = self.inner.accounts.read().await;
        if !accounts.contains_key(account_id) {
            return Err(ProbeError::AccountNotFound(account_id.clone()));
        }
        Ok(self.inner.snapshots.read().await.get(account_id).cloned())
    }

    pub async fn show_all(&self) -> Vec<SnapshotRecord> {
        self.inner
            .snapshots
            .read()
            .await
            .values()
            .cloned()
            .collect()
    }

    pub async fn status(&self) -> DaemonStatus {
        let accounts: Vec<_> = self.inner.accounts.read().await.values().cloned().collect();
        let snapshots = self.inner.snapshots.read().await;
        let failures = self.inner.failures.read().await;
        let mut statuses = Vec::with_capacity(accounts.len());
        for account in accounts {
            let config = account.config();
            let state = account.state.lock().await;
            let snapshot = snapshots.get(&config.id);
            statuses.push(AccountStatus {
                id: config.id.clone(),
                provider: config.provider,
                enabled: config.enabled,
                in_flight: state.current_flight.is_some(),
                consecutive_failures: state.consecutive_failures,
                next_probe_at: state.next_probe_at,
                has_snapshot: snapshot.is_some(),
                stale: snapshot.is_some_and(|record| record.stale),
                last_error: failures
                    .get(&config.id)
                    .map(|failure| failure.error.clone()),
            });
        }
        DaemonStatus {
            shutting_down: *self.inner.shutdown.borrow(),
            accounts: statuses,
        }
    }

    async fn run_account(&self, account: Arc<AccountRuntime>) {
        #[cfg(test)]
        if self
            .inner
            .test_panic_run_account
            .swap(false, Ordering::SeqCst)
        {
            panic!("injected run_account panic");
        }
        if account.config().enabled {
            let result = self
                .probe_account(account.clone(), ProbeTrigger::Startup)
                .await;
            if matches!(result, Err(ProbeError::Cancelled)) {
                return;
            }
        }
        loop {
            if *self.inner.shutdown.borrow() || account.removed.load(Ordering::Acquire) {
                return;
            }
            let schedule_changed = account.schedule_changed.notified();
            tokio::pin!(schedule_changed);
            schedule_changed.as_mut().enable();
            let next_probe_at = account.state.lock().await.next_probe_at;
            let mut shutdown = self.inner.shutdown.subscribe();
            if *shutdown.borrow() {
                return;
            }
            if account.config().enabled && next_probe_at.is_none() {
                if matches!(
                    self.probe_account(account.clone(), ProbeTrigger::Periodic)
                        .await,
                    Err(ProbeError::Cancelled)
                ) {
                    return;
                }
                continue;
            }
            match next_probe_at {
                Some(deadline) => {
                    let delay = (deadline - self.inner.clock.now())
                        .to_std()
                        .unwrap_or(Duration::ZERO);
                    tokio::select! {
                        _ = self.inner.clock.sleep(delay) => {
                            if !account.config().enabled {
                                account.state.lock().await.next_probe_at = None;
                                continue;
                            }
                            if account
                                .state
                                .lock()
                                .await
                                .next_probe_at
                                .is_some_and(|current| current > self.inner.clock.now())
                            {
                                continue;
                            }
                            if matches!(
                                self.probe_account(account.clone(), ProbeTrigger::Periodic).await,
                                Err(ProbeError::Cancelled)
                            ) {
                                return;
                            }
                        }
                        _ = schedule_changed.as_mut() => continue,
                        changed = shutdown.changed() => {
                            if changed.is_err() || *shutdown.borrow() {
                                return;
                            }
                        }
                    }
                }
                None => tokio::select! {
                    _ = schedule_changed.as_mut() => continue,
                    changed = shutdown.changed() => {
                        if changed.is_err() || *shutdown.borrow() {
                            return;
                        }
                    }
                },
            }
        }
    }

    async fn probe_account(
        &self,
        account: Arc<AccountRuntime>,
        trigger: ProbeTrigger,
    ) -> Result<QueryOutcome<SubscriptionUsage>, ProbeError> {
        let flight = self.start_probe_account(account, trigger).await?;
        loop {
            let notified = flight.completion.notified();
            tokio::pin!(notified);
            notified.as_mut().enable();
            if let Some(result) = flight
                .result
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner)
                .clone()
            {
                return result;
            }
            notified.await;
        }
    }

    async fn start_probe_account(
        &self,
        account: Arc<AccountRuntime>,
        trigger: ProbeTrigger,
    ) -> Result<Arc<FlightState>, ProbeError> {
        let (flight, start_flight) = {
            let mut state = account.state.lock().await;
            if account.removed.load(Ordering::Acquire) {
                return Err(ProbeError::AccountNotFound(account.config().id));
            }
            if trigger == ProbeTrigger::Periodic && !account.config().enabled {
                state.next_probe_at = None;
                return Err(ProbeError::Cancelled);
            }
            let mut admission = self
                .inner
                .admission
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner);
            if admission.closed {
                return Err(ProbeError::Cancelled);
            }
            match state.current_flight.clone() {
                Some(flight) => {
                    // A flight whose result is already stored is finished: hand
                    // it to this waiter and free the slot. This also repairs a
                    // slot a dead supervisor left behind before its watchdog
                    // could run.
                    if flight
                        .result
                        .lock()
                        .unwrap_or_else(std::sync::PoisonError::into_inner)
                        .is_some()
                    {
                        state.current_flight = None;
                    }
                    (flight, false)
                }
                None => {
                    admission.active = admission.active.saturating_add(1);
                    let flight = Arc::new(FlightState {
                        result: StdMutex::new(None),
                        completion: Notify::new(),
                    });
                    state.current_flight = Some(flight.clone());
                    state.next_probe_at = None;
                    (flight, true)
                }
            }
        };

        if start_flight {
            let engine = self.clone();
            let flight_account = account.clone();
            let supervised_flight = flight.clone();
            let activity = FlightActivityGuard {
                inner: self.inner.clone(),
            };
            tokio::spawn(async move {
                let _activity = activity;
                let _watchdog = FlightWatchdog {
                    inner: engine.inner.clone(),
                    account: flight_account.clone(),
                    flight: supervised_flight.clone(),
                };
                #[cfg(test)]
                if engine
                    .inner
                    .test_panic_flight_supervisor
                    .swap(false, Ordering::SeqCst)
                {
                    panic!("injected flight supervisor panic");
                }
                let work_engine = engine.clone();
                let work_account = flight_account.clone();
                let completion = match tokio::spawn(async move {
                    work_engine.prepare_flight_completion(work_account).await
                })
                .await
                {
                    Ok(completion) => completion,
                    Err(_) => {
                        let consecutive_failures = flight_account
                            .state
                            .lock()
                            .await
                            .consecutive_failures
                            .saturating_add(1);
                        FlightCompletion {
                            result: Err(ProbeError::Storage("daemon flight task failed".into())),
                            consecutive_failures,
                        }
                    }
                };
                {
                    *supervised_flight
                        .result
                        .lock()
                        .unwrap_or_else(std::sync::PoisonError::into_inner) =
                        Some(completion.result.clone());
                    let mut state = flight_account.state.lock().await;
                    if state
                        .current_flight
                        .as_ref()
                        .is_some_and(|current| Arc::ptr_eq(current, &supervised_flight))
                    {
                        state.current_flight = None;
                    }
                    state.consecutive_failures = completion.consecutive_failures;
                    let config = flight_account.config();
                    state.next_probe_at = if !config.enabled
                        || flight_account.removed.load(Ordering::Acquire)
                        || matches!(completion.result, Err(ProbeError::Cancelled))
                    {
                        None
                    } else {
                        let delay = next_delay(
                            &config,
                            completion.consecutive_failures,
                            completion.result.as_ref().err(),
                        );
                        let delta =
                            chrono::Duration::from_std(delay).unwrap_or(chrono::Duration::MAX);
                        Some(
                            engine
                                .inner
                                .clock
                                .now()
                                .checked_add_signed(delta)
                                .unwrap_or(DateTime::<Utc>::MAX_UTC),
                        )
                    };
                }
                supervised_flight.completion.notify_waiters();
                flight_account.schedule_changed.notify_waiters();
            });
        }

        Ok(flight)
    }

    async fn prepare_flight_completion(&self, account: Arc<AccountRuntime>) -> FlightCompletion {
        let query_engine = self.clone();
        let query_config = account.config();
        let query_result = match tokio::spawn(async move {
            query_engine.execute_query(&query_config).await
        })
        .await
        {
            Ok(result) => result,
            Err(_) => Err(ProbeError::Provider(ProviderError::ProtocolIncompatible {
                message: "provider query task failed".into(),
            })),
        };
        if matches!(query_result, Err(ProbeError::Cancelled)) {
            return self
                .finish_flight_completion(&account, Err(ProbeError::Cancelled))
                .await;
        }
        let record_engine = self.clone();
        let record_account = account.clone();
        let record_task = tokio::spawn(async move {
            record_engine
                .record_result(record_account, query_result)
                .await
        });
        let result = match record_task.await {
            Ok(result) => result,
            Err(_) => Err(ProbeError::Storage("snapshot storage task failed".into())),
        };
        self.finish_flight_completion(&account, result).await
    }

    async fn finish_flight_completion(
        &self,
        account: &AccountRuntime,
        result: Result<QueryOutcome<SubscriptionUsage>, ProbeError>,
    ) -> FlightCompletion {
        let previous_failures = account.state.lock().await.consecutive_failures;
        let consecutive_failures = if result.is_ok() {
            0
        } else {
            previous_failures.saturating_add(1)
        };
        FlightCompletion {
            result,
            consecutive_failures,
        }
    }

    async fn execute_query(
        &self,
        config: &AccountConfig,
    ) -> Result<QueryOutcome<SubscriptionUsage>, ProbeError> {
        let mut shutdown = self.inner.shutdown.subscribe();
        if *shutdown.borrow() {
            return Err(ProbeError::Cancelled);
        }
        let provider = self
            .inner
            .registry
            .get_for_account(&config.provider, config.id.as_str())?;
        // A resolved provider always has a limit: `provider_limits` is seeded
        // from every registry descriptor at engine construction and the
        // registry cannot gain providers afterwards.
        let provider_limit = self
            .inner
            .provider_limits
            .get(&config.provider)
            .expect("provider concurrency limit covers every registered provider")
            .clone();
        let timeout = self.inner.clock.sleep(config.timeout);
        tokio::pin!(timeout);
        let provider_permit = tokio::select! {
            permit = provider_limit.acquire_owned() => {
                permit.map_err(|_| ProbeError::Cancelled)?
            }
            _ = timeout.as_mut() => return Err(ProbeError::Timeout),
            changed = shutdown.changed() => {
                let _ = changed;
                return Err(ProbeError::Cancelled);
            }
        };
        let global_permit = tokio::select! {
            permit = self.inner.global_limit.clone().acquire_owned() => {
                permit.map_err(|_| ProbeError::Cancelled)?
            }
            _ = timeout.as_mut() => return Err(ProbeError::Timeout),
            changed = shutdown.changed() => {
                let _ = changed;
                return Err(ProbeError::Cancelled);
            }
        };

        let request = provider.query_usage(config.query.clone());
        let result = tokio::select! {
            result = request => result.map_err(ProbeError::Provider),
            _ = timeout.as_mut() => Err(ProbeError::Timeout),
            changed = shutdown.changed() => {
                let _ = changed;
                Err(ProbeError::Cancelled)
            }
        };
        drop(provider_permit);
        drop(global_permit);
        result
    }

    async fn record_result(
        &self,
        account: Arc<AccountRuntime>,
        result: Result<QueryOutcome<SubscriptionUsage>, ProbeError>,
    ) -> Result<QueryOutcome<SubscriptionUsage>, ProbeError> {
        let account_id = account.config().id;
        // Keep the original provider error for opt-in diagnostics. Persist only
        // the sanitized copy so snapshots and failure records stay redacted.
        let result = result.map(sanitize_outcome);
        let persisted = result.clone().map_err(sanitize_probe_error);
        let accounts = self.inner.accounts.read().await;
        // A flight that outlives its account must not write onto an account
        // later recreated under the same id; the runtime pointer is the
        // generation check. The caller still gets the result, persisted or not.
        if !accounts
            .get(&account_id)
            .is_some_and(|current| Arc::ptr_eq(current, &account))
        {
            return result;
        }
        let _guard = self.inner.persist_lock.lock().await;
        let now = self.inner.clock.now();
        let mut snapshots = self.inner.snapshots.read().await.clone();
        let mut failures = self.inner.failures.read().await.clone();
        match &persisted {
            Ok(usage) => {
                snapshots.insert(
                    account_id.clone(),
                    SnapshotRecord {
                        account_id: account_id.clone(),
                        usage: usage.clone(),
                        last_success_at: now,
                        stale: false,
                        last_error: None,
                        last_error_at: None,
                    },
                );
                failures.remove(&account_id);
            }
            Err(error) => {
                let sanitized = SanitizedError::from_probe(error);
                if let Some(snapshot) = snapshots.get_mut(&account_id) {
                    snapshot.stale = true;
                    snapshot.last_error = Some(sanitized.clone());
                    snapshot.last_error_at = Some(now);
                }
                failures.insert(
                    account_id.clone(),
                    FailureRecord {
                        error: sanitized,
                        occurred_at: now,
                    },
                );
            }
        }
        let staged = PersistedState {
            accounts: accounts
                .iter()
                .map(|(id, account)| (id.clone(), account.config()))
                .collect(),
            removed_accounts: self.inner.removed_accounts.read().await.clone(),
            next_account_sequence: self.inner.next_account_id.load(Ordering::Relaxed),
            snapshots: snapshots.clone(),
            failures: failures.clone(),
        };
        // The staged bytes are frozen and `persist_lock` already serializes the
        // commit, so the read guard can be released before the storage I/O.
        drop(accounts);
        let mut shutdown = self.inner.shutdown.subscribe();
        if *shutdown.borrow() {
            return Err(ProbeError::Cancelled);
        }
        let staged_write = tokio::select! {
            staged_write = tokio::time::timeout(
                STORE_STAGE_TIMEOUT,
                self.inner.store.stage(&staged),
            ) => {
                match staged_write {
                    Ok(Ok(staged)) => staged,
                    Ok(Err(error)) => return Err(ProbeError::Storage(error)),
                    Err(_) => {
                        return Err(ProbeError::Storage("snapshot stage timed out".into()));
                    }
                }
            }
            changed = shutdown.changed() => {
                let _ = changed;
                return Err(ProbeError::Cancelled);
            }
        };
        {
            let admission = self
                .inner
                .admission
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner);
            if admission.closed {
                return Err(ProbeError::Cancelled);
            }
            // Acquiring this gate while admission is open is the commit
            // linearization point shared with `shutdown()`.
        }
        staged_write.commit().await.map_err(ProbeError::Storage)?;
        let mut live_snapshots = self.inner.snapshots.write().await;
        let mut live_failures = self.inner.failures.write().await;
        *live_snapshots = snapshots;
        *live_failures = failures;
        result
    }
}

fn next_delay(config: &AccountConfig, failures: u32, error: Option<&ProbeError>) -> Duration {
    let base = if failures == 0 {
        config.interval
    } else {
        let exponent = failures.saturating_sub(1).min(31);
        let multiplier = 1_u32 << exponent;
        config
            .backoff
            .initial
            .saturating_mul(multiplier)
            .min(config.backoff.maximum)
    };
    let rate_limit = match error {
        Some(ProbeError::Provider(ullage_core::ProviderError::RateLimited {
            retry_after_seconds: Some(seconds),
            ..
        })) => Duration::from_secs(*seconds).min(MAXIMUM_RETRY_AFTER),
        _ => Duration::ZERO,
    };
    base.max(rate_limit)
        .saturating_add(deterministic_jitter(&config.id, failures, config.jitter))
}

fn sanitize_probe_error(error: ProbeError) -> ProbeError {
    match error {
        ProbeError::Provider(error) => ProbeError::Provider(sanitize_provider_error(error)),
        other => other,
    }
}

fn sanitize_outcome(outcome: QueryOutcome<SubscriptionUsage>) -> QueryOutcome<SubscriptionUsage> {
    match outcome {
        QueryOutcome::Complete { data } => QueryOutcome::Complete { data },
        QueryOutcome::Partial { data, mut failures } => {
            for failure in &mut failures {
                if !ProviderError::is_sanitized_partial_message(&failure.message) {
                    failure.message = ProviderError::ProtocolIncompatible {
                        message: String::new(),
                    }
                    .sanitized_message()
                    .into();
                }
            }
            QueryOutcome::Partial { data, failures }
        }
    }
}

fn deterministic_jitter(account_id: &AccountId, generation: u32, maximum: Duration) -> Duration {
    let maximum_millis = maximum.as_millis();
    if maximum_millis == 0 {
        return Duration::ZERO;
    }
    let mut hash = 0xcbf29ce484222325_u64;
    for byte in account_id.as_str().bytes().chain(generation.to_le_bytes()) {
        hash ^= u64::from(byte);
        hash = hash.wrapping_mul(0x100000001b3);
    }
    let millis = u128::from(hash) % (maximum_millis + 1);
    Duration::from_millis(millis.min(u128::from(u64::MAX)) as u64)
}

#[cfg(test)]
mod tests {
    use std::sync::atomic::AtomicUsize;

    use ullage_auth::{
        AuthChallenge, AuthCompleteRequest, AuthMethod, AuthStartRequest, AuthState, LogoutRequest,
    };
    use ullage_core::{Capability, Provider, ProviderDescriptor, ProviderResult, UsageQuery};

    use super::*;
    use crate::model::BackoffConfig;
    use crate::store::{MemorySnapshotStore, StagedSnapshot};

    #[derive(Clone)]
    struct TestProvider {
        inner: Arc<TestProviderInner>,
    }

    struct TestProviderInner {
        calls: AtomicUsize,
        started: Notify,
        gate: Option<Arc<Semaphore>>,
    }

    impl TestProvider {
        fn gated() -> (Self, Arc<Semaphore>) {
            let gate = Arc::new(Semaphore::new(0));
            (
                Self {
                    inner: Arc::new(TestProviderInner {
                        calls: AtomicUsize::new(0),
                        started: Notify::new(),
                        gate: Some(gate.clone()),
                    }),
                },
                gate,
            )
        }

        fn immediate() -> Self {
            Self {
                inner: Arc::new(TestProviderInner {
                    calls: AtomicUsize::new(0),
                    started: Notify::new(),
                    gate: None,
                }),
            }
        }

        fn calls(&self) -> usize {
            self.inner.calls.load(Ordering::SeqCst)
        }

        async fn wait_for_calls(&self, expected: usize) {
            tokio::time::timeout(Duration::from_secs(5), async {
                loop {
                    let notified = self.inner.started.notified();
                    tokio::pin!(notified);
                    notified.as_mut().enable();
                    if self.calls() >= expected {
                        return;
                    }
                    notified.await;
                }
            })
            .await
            .expect("provider query was not reached");
        }
    }

    #[async_trait]
    impl Provider for TestProvider {
        type VendorUsage = SubscriptionUsage;

        fn descriptor(&self) -> ProviderDescriptor {
            ProviderDescriptor {
                id: ProviderId::new("test"),
                display_name: "test".into(),
                capabilities: vec![Capability::UsageQuery],
            }
        }

        async fn start_auth(&self, _: AuthStartRequest) -> ProviderResult<AuthChallenge> {
            Ok(AuthChallenge {
                flow_id: "flow-1".into(),
                method: AuthMethod::DeviceCode,
                verification_uri: None,
                user_code: None,
                expires_at: None,
                input: None,
            })
        }

        async fn complete_auth(&self, _: AuthCompleteRequest) -> ProviderResult<AuthState> {
            Ok(AuthState::NotAuthenticated)
        }

        async fn auth_status(&self) -> ProviderResult<AuthState> {
            Ok(AuthState::NotAuthenticated)
        }

        async fn logout(&self, _: LogoutRequest) -> ProviderResult<()> {
            Ok(())
        }

        async fn query(
            &self,
            request: UsageQuery,
        ) -> ProviderResult<QueryOutcome<Self::VendorUsage>> {
            self.inner.calls.fetch_add(1, Ordering::SeqCst);
            self.inner.started.notify_waiters();
            if let Some(gate) = &self.inner.gate {
                gate.acquire().await.unwrap().forget();
            }
            Ok(QueryOutcome::Complete {
                data: SubscriptionUsage {
                    provider: ProviderId::new("test"),
                    account_label: request.account_label,
                    plan: None,
                    subscription_expires_at: None,
                    observed_at: Utc::now(),
                    windows: Vec::new(),
                },
            })
        }

        fn normalize(&self, usage: Self::VendorUsage) -> ProviderResult<SubscriptionUsage> {
            Ok(usage)
        }
    }

    fn test_account(id: &str) -> AccountConfig {
        AccountConfig {
            id: AccountId::new(id),
            provider: ProviderId::new("test"),
            query: UsageQuery {
                account_label: Some(id.into()),
            },
            enabled: true,
            interval: Duration::from_secs(60),
            timeout: Duration::from_secs(5),
            jitter: Duration::ZERO,
            backoff: BackoffConfig {
                initial: Duration::from_secs(10),
                maximum: Duration::from_secs(40),
            },
            metrics: Vec::new(),
        }
    }

    fn engine_with(provider: TestProvider) -> (DaemonEngine, Arc<MemorySnapshotStore>) {
        let mut registry = ProviderRegistry::default();
        registry.register(provider).unwrap();
        let store = Arc::new(MemorySnapshotStore::default());
        (
            DaemonEngine {
                inner: Arc::new(EngineInner {
                    registry: Arc::new(registry),
                    clock: Arc::new(SystemClock),
                    store: store.clone(),
                    snapshots: RwLock::new(SnapshotMap::new()),
                    failures: RwLock::new(BTreeMap::new()),
                    persist_lock: Mutex::new(()),
                    accounts: RwLock::new(BTreeMap::new()),
                    removed_accounts: RwLock::new(std::collections::BTreeSet::new()),
                    accounts_changed: Notify::new(),
                    next_account_id: AtomicU64::new(1),
                    global_limit: Arc::new(Semaphore::new(4)),
                    provider_limits: HashMap::from([(
                        ProviderId::new("test"),
                        Arc::new(Semaphore::new(2)),
                    )]),
                    shutdown: watch::channel(false).0,
                    admission: StdMutex::new(FlightAdmission::default()),
                    flights_idle: Notify::new(),
                    test_panic_run_account: AtomicBool::new(false),
                    test_panic_flight_supervisor: AtomicBool::new(false),
                }),
            },
            store,
        )
    }

    #[test]
    fn rate_limit_hint_is_capped() {
        let config = test_account("account-1");
        let rate_limited = |seconds| {
            ProbeError::Provider(ProviderError::RateLimited {
                message: "slow down".into(),
                retry_after_seconds: Some(seconds),
            })
        };

        assert_eq!(
            next_delay(&config, 0, Some(&rate_limited(120))),
            Duration::from_secs(120)
        );
        assert_eq!(
            next_delay(&config, 0, Some(&rate_limited(u64::MAX))),
            MAXIMUM_RETRY_AFTER
        );
    }

    #[tokio::test]
    async fn account_task_panic_recovers_scheduling() {
        let (engine, _store) = engine_with(TestProvider::immediate());
        let account_id = AccountId::new("account-1");
        engine.add_account(test_account("account-1")).await.unwrap();
        engine
            .inner
            .test_panic_run_account
            .store(true, Ordering::SeqCst);

        let runner = {
            let engine = engine.clone();
            tokio::spawn(async move { engine.run().await })
        };
        // The first run_account panics; the scheduler must remove only this
        // account from `started` and re-admit it after the restart delay.
        tokio::time::timeout(Duration::from_secs(10), async {
            loop {
                if engine.show(&account_id).await.is_some() {
                    return;
                }
                tokio::time::sleep(Duration::from_millis(20)).await;
            }
        })
        .await
        .expect("account never probed after task panic");

        engine.shutdown();
        runner.await.unwrap();
    }

    #[tokio::test]
    async fn flight_supervisor_panic_releases_probe_waiters() {
        let (engine, _store) = engine_with(TestProvider::immediate());
        let account_id = AccountId::new("account-1");
        engine.add_account(test_account("account-1")).await.unwrap();
        engine
            .inner
            .test_panic_flight_supervisor
            .store(true, Ordering::SeqCst);

        let result = tokio::time::timeout(
            Duration::from_secs(5),
            engine.probe(&account_id, ProbeTrigger::Manual),
        )
        .await
        .expect("probe waiter hung after supervisor panic");
        assert!(matches!(result, Err(ProbeError::Storage(_))));

        // The freed slot must accept a fresh flight and finish it.
        let result = engine.probe(&account_id, ProbeTrigger::Manual).await;
        assert!(result.is_ok());
    }

    #[tokio::test]
    async fn recreated_account_does_not_inherit_old_flight_result() {
        let (provider, gate) = TestProvider::gated();
        let (engine, _store) = engine_with(provider.clone());
        let account_id = AccountId::new("account-1");
        engine.add_account(test_account("account-1")).await.unwrap();

        let probe = {
            let engine = engine.clone();
            let account_id = account_id.clone();
            tokio::spawn(async move { engine.probe(&account_id, ProbeTrigger::Manual).await })
        };
        // The query is in flight; swapping the account must not let its result
        // land on the runtime recreated under the same id.
        provider.wait_for_calls(1).await;
        engine.remove_account(&account_id).await.unwrap();
        engine.add_account(test_account("account-1")).await.unwrap();
        gate.add_permits(1);

        probe
            .await
            .unwrap()
            .expect("waiter must still get a result");
        assert!(engine.show(&account_id).await.is_none());
        let status = engine.status().await;
        assert_eq!(
            status
                .accounts
                .iter()
                .find(|account| account.id == account_id)
                .unwrap()
                .consecutive_failures,
            0
        );
    }

    /// A store whose `stage` never returns must not wedge a flight; the bound
    /// is `STORE_STAGE_TIMEOUT` of tokio time.
    struct HangingStore;

    #[async_trait]
    impl SnapshotStore for HangingStore {
        async fn load(&self) -> Result<PersistedState, String> {
            Ok(PersistedState::default())
        }

        async fn stage(&self, _: &PersistedState) -> Result<Box<dyn StagedSnapshot>, String> {
            std::future::pending().await
        }
    }

    #[tokio::test(start_paused = true)]
    async fn a_hanging_stage_times_out_instead_of_wedging_the_flight() {
        let mut registry = ProviderRegistry::default();
        registry.register(TestProvider::immediate()).unwrap();
        let engine = DaemonEngine::new(
            DaemonConfig::default(),
            Arc::new(registry),
            Arc::new(SystemClock),
            Arc::new(HangingStore),
        )
        .await
        .unwrap();
        let account_id = AccountId::new("account-1");

        // `add_account` also stages; its own bounded wait must not hang.
        let add = {
            let engine = engine.clone();
            tokio::spawn(async move { engine.add_account(test_account("account-1")).await })
        };
        tokio::task::yield_now().await;
        tokio::time::advance(STORE_STAGE_TIMEOUT + Duration::from_secs(1)).await;
        assert!(matches!(add.await.unwrap(), Err(DaemonError::Storage(_))));

        // Register the account without persistence to exercise the flight path.
        engine.inner.accounts.write().await.insert(
            account_id.clone(),
            Arc::new(AccountRuntime {
                config: StdRwLock::new(test_account("account-1")),
                removed: AtomicBool::new(false),
                state: Mutex::new(AccountRuntimeState::default()),
                schedule_changed: Notify::new(),
            }),
        );
        let probe = {
            let engine = engine.clone();
            let account_id = account_id.clone();
            tokio::spawn(async move { engine.probe(&account_id, ProbeTrigger::Manual).await })
        };
        tokio::task::yield_now().await;
        tokio::time::advance(STORE_STAGE_TIMEOUT + Duration::from_secs(1)).await;
        assert!(matches!(probe.await.unwrap(), Err(ProbeError::Storage(_))));
    }
}
