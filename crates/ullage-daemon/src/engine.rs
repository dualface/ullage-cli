use std::collections::{BTreeMap, HashMap, HashSet};
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::{Arc, Mutex as StdMutex, RwLock as StdRwLock};
use std::time::Duration;

use async_trait::async_trait;
use chrono::{DateTime, Utc};
use tokio::sync::{Mutex, Notify, RwLock, Semaphore, watch};
use tokio::task::JoinSet;
use ullage_core::{
    ProviderError, ProviderId, ProviderRegistry, QueryOutcome, SubscriptionUsage,
    summary::MetricFilter,
};

use crate::model::{
    AccountConfig, AccountId, AccountStatus, DaemonConfig, DaemonError, DaemonStatus,
    FailureRecord, PersistedState, ProbeError, ProbeTrigger, SanitizedError, SnapshotMap,
    SnapshotRecord, sanitize_provider_error,
};
use crate::store::{SnapshotStore, snapshot_for};

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
    removed_accounts: RwLock<std::collections::BTreeSet<AccountId>>,
    accounts_changed: Notify,
    next_account_id: AtomicU64,
    global_limit: Arc<Semaphore>,
    provider_limits: HashMap<ProviderId, Arc<Semaphore>>,
    default_provider_limit: usize,
    shutdown: watch::Sender<bool>,
    admission: StdMutex<FlightAdmission>,
    flights_idle: Notify,
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
    result: Mutex<Option<Result<QueryOutcome<SubscriptionUsage>, ProbeError>>>,
    completion: Notify,
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
        crate::install_redacting_panic_hook();
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
            .and_then(|sequence| sequence.checked_add(1))
            .unwrap_or(1)
            .max(persisted.next_account_sequence);
        let accounts = persisted
            .accounts
            .into_iter()
            .map(|(id, config)| {
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
                default_provider_limit: config.default_provider_concurrency,
                shutdown,
                admission: StdMutex::new(FlightAdmission::default()),
                flights_idle: Notify::new(),
            }),
        })
    }

    pub async fn add_account(&self, config: AccountConfig) -> Result<(), DaemonError> {
        if config.interval.is_zero() {
            return Err(DaemonError::InvalidInterval(config.id));
        }
        if config.timeout.is_zero() {
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
        if let Err(error) = self.persist_accounts(&accounts).await {
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
            if !self.inner.accounts.read().await.contains_key(&candidate) {
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

    pub async fn set_account_enabled(
        &self,
        account_id: &AccountId,
        enabled: bool,
    ) -> Result<Option<AccountConfig>, DaemonError> {
        let accounts = self.inner.accounts.write().await;
        let Some(account) = accounts.get(account_id).cloned() else {
            return Ok(None);
        };
        let (previous_enabled, updated) = {
            let mut config = account
                .config
                .write()
                .unwrap_or_else(std::sync::PoisonError::into_inner);
            let previous_enabled = config.enabled;
            config.enabled = enabled;
            (previous_enabled, config.clone())
        };
        if let Err(error) = self.persist_accounts(&accounts).await {
            account
                .config
                .write()
                .unwrap_or_else(std::sync::PoisonError::into_inner)
                .enabled = previous_enabled;
            account.schedule_changed.notify_one();
            return Err(error);
        }
        if !enabled {
            account.state.lock().await.next_probe_at = None;
        }
        account.schedule_changed.notify_one();
        drop(accounts);
        Ok(Some(updated))
    }

    /// Renames an account in place. The provider plus label pair stays unique so
    /// that label selectors keep resolving to exactly one account.
    pub async fn set_account_label(
        &self,
        account_id: &AccountId,
        label: Option<String>,
    ) -> Result<Option<AccountConfig>, DaemonError> {
        let accounts = self.inner.accounts.write().await;
        let Some(account) = accounts.get(account_id).cloned() else {
            return Ok(None);
        };
        let provider = account.config().provider;
        if label.is_some()
            && accounts.iter().any(|(id, other)| {
                let other_config = other.config();
                id != account_id
                    && other_config.provider == provider
                    && other_config.query.account_label == label
            })
        {
            return Err(DaemonError::DuplicateAccountSelector {
                provider,
                account_label: label,
            });
        }
        let (previous_label, updated) = {
            let mut config = account
                .config
                .write()
                .unwrap_or_else(std::sync::PoisonError::into_inner);
            let previous_label = std::mem::replace(&mut config.query.account_label, label);
            (previous_label, config.clone())
        };
        if let Err(error) = self.persist_accounts(&accounts).await {
            account
                .config
                .write()
                .unwrap_or_else(std::sync::PoisonError::into_inner)
                .query
                .account_label = previous_label;
            return Err(error);
        }
        drop(accounts);
        Ok(Some(updated))
    }

    /// Replaces the display metric filter of one account.
    ///
    /// The filter is validated by [`MetricFilter`]; a rejected value leaves the
    /// account untouched. A failed persistence rolls the in-memory change back.
    pub async fn set_account_metrics(
        &self,
        account_id: &AccountId,
        metrics: Vec<String>,
    ) -> Result<Option<AccountConfig>, DaemonError> {
        let filter = MetricFilter::new(metrics)
            .map_err(|_| DaemonError::InvalidAccountMetrics(account_id.clone()))?;
        let accounts = self.inner.accounts.write().await;
        let Some(account) = accounts.get(account_id).cloned() else {
            return Ok(None);
        };
        let (previous_metrics, updated) = {
            let mut config = account
                .config
                .write()
                .unwrap_or_else(std::sync::PoisonError::into_inner);
            let previous_metrics = std::mem::replace(&mut config.metrics, filter.names().to_vec());
            (previous_metrics, config.clone())
        };
        if let Err(error) = self.persist_accounts(&accounts).await {
            account
                .config
                .write()
                .unwrap_or_else(std::sync::PoisonError::into_inner)
                .metrics = previous_metrics;
            return Err(error);
        }
        drop(accounts);
        Ok(Some(updated))
    }

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
        self.inner
            .store
            .stage(&state)
            .await
            .map_err(DaemonError::Storage)?
            .commit()
            .await
            .map_err(DaemonError::Storage)
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
        let mut stage_task = tokio::spawn(async move { stage_store.stage(&stage_state).await });
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

        let mut shutdown = self.inner.shutdown.subscribe();
        loop {
            let accounts: Vec<_> = self.inner.accounts.read().await.values().cloned().collect();
            for account in accounts {
                let account_id = account.config().id;
                if started.insert(account_id.clone()) {
                    let engine = self.clone();
                    tasks.spawn(async move {
                        engine.run_account(account).await;
                        account_id
                    });
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
                joined = tasks.join_next(), if !tasks.is_empty() => {
                    if let Some(Ok(account_id)) = joined {
                        started.remove(&account_id);
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

    /// Probe without sanitizing the returned provider error. The control
    /// handler uses this only to attach opt-in diagnostics, then sanitizes
    /// the `ControlError` payload.
    pub(crate) async fn probe_unsanitized(
        &self,
        account_id: &AccountId,
        trigger: ProbeTrigger,
    ) -> Result<QueryOutcome<SubscriptionUsage>, ProbeError> {
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
        self.probe_account(account, trigger).await
    }

    pub async fn start_probe(
        &self,
        account_id: &AccountId,
        trigger: ProbeTrigger,
    ) -> Result<(), ProbeError> {
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
    ) -> Result<Option<SnapshotRecord>, ()> {
        let accounts = self.inner.accounts.read().await;
        if !accounts.contains_key(account_id) {
            return Err(());
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
            if let Some(result) = flight.result.lock().await.clone() {
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
            if let Some(flight) = &state.current_flight {
                (flight.clone(), false)
            } else {
                admission.active = admission.active.saturating_add(1);
                let flight = Arc::new(FlightState {
                    result: Mutex::new(None),
                    completion: Notify::new(),
                });
                state.current_flight = Some(flight.clone());
                state.next_probe_at = None;
                (flight, true)
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
                let work_engine = engine.clone();
                let work_account = flight_account.clone();
                let completion = match tokio::spawn(async move {
                    work_engine.prepare_flight_completion(&work_account).await
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
                    *supervised_flight.result.lock().await = Some(completion.result.clone());
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

    async fn prepare_flight_completion(&self, account: &AccountRuntime) -> FlightCompletion {
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
                .finish_flight_completion(account, Err(ProbeError::Cancelled))
                .await;
        }
        let record_engine = self.clone();
        let account_id = account.config().id;
        let record_task =
            tokio::spawn(
                async move { record_engine.record_result(&account_id, query_result).await },
            );
        let result = match record_task.await {
            Ok(result) => result,
            Err(_) => Err(ProbeError::Storage("snapshot storage task failed".into())),
        };
        self.finish_flight_completion(account, result).await
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
        let provider = self
            .inner
            .registry
            .get_for_account(&config.provider, config.id.as_str())?;
        let provider_limit = self
            .inner
            .provider_limits
            .get(&config.provider)
            .cloned()
            .unwrap_or_else(|| Arc::new(Semaphore::new(self.inner.default_provider_limit)));
        let mut shutdown = self.inner.shutdown.subscribe();
        if *shutdown.borrow() {
            return Err(ProbeError::Cancelled);
        }
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
        account_id: &AccountId,
        result: Result<QueryOutcome<SubscriptionUsage>, ProbeError>,
    ) -> Result<QueryOutcome<SubscriptionUsage>, ProbeError> {
        // Keep the original provider error for opt-in diagnostics. Persist only
        // the sanitized copy so snapshots and failure records stay redacted.
        let result = result.map(sanitize_outcome);
        let persisted = result.clone().map_err(sanitize_probe_error);
        let accounts = self.inner.accounts.read().await;
        if !accounts.contains_key(account_id) {
            return Err(ProbeError::Cancelled);
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
                failures.remove(account_id);
            }
            Err(error) => {
                let sanitized = SanitizedError::from_probe(error);
                if let Some(snapshot) = snapshots.get_mut(account_id) {
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
        let mut shutdown = self.inner.shutdown.subscribe();
        if *shutdown.borrow() {
            return Err(ProbeError::Cancelled);
        }
        let staged_write = tokio::select! {
            staged_write = self.inner.store.stage(&staged) => {
                staged_write.map_err(ProbeError::Storage)?
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
        drop(accounts);
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
        })) => Duration::from_secs(*seconds),
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
