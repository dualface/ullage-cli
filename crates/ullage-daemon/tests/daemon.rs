use std::collections::VecDeque;
use std::future::{Future, poll_fn};
use std::sync::atomic::{AtomicBool, AtomicU64, AtomicUsize, Ordering};
use std::sync::{Arc, Mutex};
use std::task::Poll;
use std::time::Duration;

use async_trait::async_trait;
use chrono::{DateTime, TimeDelta, Utc};
use tokio::sync::{Notify, Semaphore};
use ullage_auth::{
    AuthChallenge, AuthCompleteRequest, AuthMethod, AuthStartRequest, AuthState, LogoutRequest,
};
use ullage_core::{
    Capability, PartialFailure, Provider, ProviderDescriptor, ProviderError, ProviderId,
    ProviderRegistry, ProviderResult, QueryOutcome, SubscriptionUsage, UsageQuery,
};
#[cfg(unix)]
use ullage_daemon::UnixControlServer;
use ullage_daemon::{
    AccountConfig, AccountId, BackoffConfig, Clock, ControlService, ControlTransport, DaemonConfig,
    DaemonEngine, DaemonError, JsonSnapshotStore, MemorySnapshotStore, PersistedState, ProbeError,
    ProbeTrigger, ProviderLimit, SanitizedError, SnapshotRecord,
};
use ullage_protocol::{
    AccountError, CONTROL_PROTOCOL_VERSION, ControlCommand, ControlError, ControlRequest,
    ControlResult,
};

#[derive(Clone)]
struct ManualClock {
    base: DateTime<Utc>,
    elapsed_millis: Arc<AtomicU64>,
    changed: Arc<Notify>,
    sleep_durations: Arc<Mutex<Vec<Duration>>>,
    sleep_started: Arc<Notify>,
}

impl ManualClock {
    fn new() -> Self {
        Self {
            base: DateTime::parse_from_rfc3339("2026-08-27T12:00:00Z")
                .unwrap()
                .to_utc(),
            elapsed_millis: Arc::new(AtomicU64::new(0)),
            changed: Arc::new(Notify::new()),
            sleep_durations: Arc::new(Mutex::new(Vec::new())),
            sleep_started: Arc::new(Notify::new()),
        }
    }

    fn advance(&self, duration: Duration) {
        self.elapsed_millis
            .fetch_add(duration.as_millis().try_into().unwrap(), Ordering::SeqCst);
        self.changed.notify_waiters();
    }

    async fn wait_for_sleep(&self, expected: Duration) {
        tokio::time::timeout(Duration::from_secs(1), async {
            loop {
                let started = self.sleep_started.notified();
                tokio::pin!(started);
                started.as_mut().enable();
                if self.sleep_durations.lock().unwrap().contains(&expected) {
                    return;
                }
                started.await;
            }
        })
        .await
        .unwrap();
    }
}

#[async_trait]
impl Clock for ManualClock {
    fn now(&self) -> DateTime<Utc> {
        self.base
            + TimeDelta::milliseconds(
                self.elapsed_millis
                    .load(Ordering::SeqCst)
                    .try_into()
                    .unwrap(),
            )
    }

    async fn sleep(&self, duration: Duration) {
        let target = self
            .elapsed_millis
            .load(Ordering::SeqCst)
            .saturating_add(duration.as_millis().try_into().unwrap());
        self.sleep_durations.lock().unwrap().push(duration);
        self.sleep_started.notify_waiters();
        loop {
            let changed = self.changed.notified();
            tokio::pin!(changed);
            changed.as_mut().enable();
            if self.elapsed_millis.load(Ordering::SeqCst) >= target {
                return;
            }
            changed.await;
        }
    }
}

#[derive(Clone)]
struct MockProvider {
    inner: Arc<MockInner>,
}

struct MockInner {
    id: ProviderId,
    results: Mutex<VecDeque<ProviderResult<QueryOutcome<SubscriptionUsage>>>>,
    calls: AtomicUsize,
    active: AtomicUsize,
    maximum_active: AtomicUsize,
    started: Notify,
    gate: Option<Arc<Semaphore>>,
    panic_query: bool,
}

impl MockProvider {
    fn new(
        id: &str,
        results: impl IntoIterator<Item = ProviderResult<QueryOutcome<SubscriptionUsage>>>,
    ) -> Self {
        Self {
            inner: Arc::new(MockInner {
                id: ProviderId::new(id),
                results: Mutex::new(results.into_iter().collect()),
                calls: AtomicUsize::new(0),
                active: AtomicUsize::new(0),
                maximum_active: AtomicUsize::new(0),
                started: Notify::new(),
                gate: None,
                panic_query: false,
            }),
        }
    }

    fn gated(id: &str) -> (Self, Arc<Semaphore>) {
        Self::gated_with_results(id, [])
    }

    fn gated_with_results(
        id: &str,
        results: impl IntoIterator<Item = ProviderResult<QueryOutcome<SubscriptionUsage>>>,
    ) -> (Self, Arc<Semaphore>) {
        let gate = Arc::new(Semaphore::new(0));
        (
            Self {
                inner: Arc::new(MockInner {
                    id: ProviderId::new(id),
                    results: Mutex::new(results.into_iter().collect()),
                    calls: AtomicUsize::new(0),
                    active: AtomicUsize::new(0),
                    maximum_active: AtomicUsize::new(0),
                    started: Notify::new(),
                    gate: Some(gate.clone()),
                    panic_query: false,
                }),
            },
            gate,
        )
    }

    fn panicking_gated(id: &str) -> (Self, Arc<Semaphore>) {
        let gate = Arc::new(Semaphore::new(0));
        (
            Self {
                inner: Arc::new(MockInner {
                    id: ProviderId::new(id),
                    results: Mutex::new(VecDeque::new()),
                    calls: AtomicUsize::new(0),
                    active: AtomicUsize::new(0),
                    maximum_active: AtomicUsize::new(0),
                    started: Notify::new(),
                    gate: Some(gate.clone()),
                    panic_query: true,
                }),
            },
            gate,
        )
    }

    fn calls(&self) -> usize {
        self.inner.calls.load(Ordering::SeqCst)
    }

    fn maximum_active(&self) -> usize {
        self.inner.maximum_active.load(Ordering::SeqCst)
    }

    async fn wait_for_calls(&self, expected: usize) {
        tokio::time::timeout(Duration::from_secs(2), async {
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
        .unwrap();
    }
}

struct ActiveQuery<'a>(&'a MockInner);

impl Drop for ActiveQuery<'_> {
    fn drop(&mut self) {
        self.0.active.fetch_sub(1, Ordering::SeqCst);
    }
}

#[async_trait]
impl Provider for MockProvider {
    type VendorUsage = SubscriptionUsage;

    fn descriptor(&self) -> ProviderDescriptor {
        ProviderDescriptor {
            id: self.inner.id.clone(),
            display_name: format!("{} mock", self.inner.id),
            capabilities: vec![Capability::Authentication, Capability::UsageQuery],
        }
    }

    async fn start_auth(&self, _: AuthStartRequest) -> ProviderResult<AuthChallenge> {
        Ok(AuthChallenge {
            flow_id: "flow-1".into(),
            method: AuthMethod::DeviceCode,
            verification_uri: Some("https://example.invalid/device".into()),
            user_code: Some("secret-code".into()),
            expires_at: None,
            input: None,
        })
    }

    async fn complete_auth(&self, _: AuthCompleteRequest) -> ProviderResult<AuthState> {
        Ok(AuthState::Authenticated {
            account_label: Some("primary".into()),
            expires_at: None,
        })
    }

    async fn auth_status(&self) -> ProviderResult<AuthState> {
        Ok(AuthState::Pending {
            flow_id: "flow-1".into(),
            expires_at: None,
        })
    }

    async fn logout(&self, _: LogoutRequest) -> ProviderResult<()> {
        Ok(())
    }

    async fn query(&self, request: UsageQuery) -> ProviderResult<QueryOutcome<Self::VendorUsage>> {
        self.inner.calls.fetch_add(1, Ordering::SeqCst);
        let active = self.inner.active.fetch_add(1, Ordering::SeqCst) + 1;
        self.inner
            .maximum_active
            .fetch_max(active, Ordering::SeqCst);
        self.inner.started.notify_waiters();
        let _active = ActiveQuery(&self.inner);
        if let Some(gate) = &self.inner.gate {
            gate.acquire().await.unwrap().forget();
        }
        assert!(!self.inner.panic_query, "secret provider panic detail");
        self.inner
            .results
            .lock()
            .unwrap()
            .pop_front()
            .unwrap_or_else(|| {
                Ok(QueryOutcome::Complete {
                    data: usage(&self.inner.id, request.account_label.as_deref()),
                })
            })
    }

    fn normalize(&self, usage: Self::VendorUsage) -> ProviderResult<SubscriptionUsage> {
        Ok(usage)
    }
}

#[derive(Default)]
struct PanickingStore(AtomicBool);

struct PanickingStageStore {
    state: PersistedState,
    armed: AtomicBool,
}

#[derive(Default)]
struct ShutdownOnStageStore {
    engine: Mutex<Option<DaemonEngine>>,
}

struct NoopStagedSnapshot;

#[async_trait]
impl ullage_daemon::StagedSnapshot for NoopStagedSnapshot {
    async fn commit(self: Box<Self>) -> Result<(), String> {
        Ok(())
    }
}

#[async_trait]
impl ullage_daemon::SnapshotStore for PanickingStore {
    async fn load(&self) -> Result<ullage_daemon::PersistedState, String> {
        Ok(ullage_daemon::PersistedState::default())
    }

    async fn stage(
        &self,
        _: &ullage_daemon::PersistedState,
    ) -> Result<Box<dyn ullage_daemon::StagedSnapshot>, String> {
        if self.0.load(Ordering::SeqCst) {
            panic!("secret storage panic detail")
        }
        Ok(Box::new(NoopStagedSnapshot))
    }
}

#[async_trait]
impl ullage_daemon::SnapshotStore for PanickingStageStore {
    async fn load(&self) -> Result<PersistedState, String> {
        Ok(self.state.clone())
    }

    async fn stage(
        &self,
        _: &PersistedState,
    ) -> Result<Box<dyn ullage_daemon::StagedSnapshot>, String> {
        if self.armed.load(Ordering::SeqCst) {
            panic!("secret removal storage panic detail")
        }
        Ok(Box::new(NoopStagedSnapshot))
    }
}

#[async_trait]
impl ullage_daemon::SnapshotStore for ShutdownOnStageStore {
    async fn load(&self) -> Result<PersistedState, String> {
        Ok(PersistedState::default())
    }

    async fn stage(
        &self,
        _: &PersistedState,
    ) -> Result<Box<dyn ullage_daemon::StagedSnapshot>, String> {
        let engine = self.engine.lock().unwrap().clone();
        if let Some(engine) = engine {
            engine.shutdown();
        }
        Ok(Box::new(NoopStagedSnapshot))
    }
}

#[derive(Default)]
struct PanickingLoadStore;

#[async_trait]
impl ullage_daemon::SnapshotStore for PanickingLoadStore {
    async fn load(&self) -> Result<ullage_daemon::PersistedState, String> {
        panic!("secret load panic detail")
    }

    async fn stage(
        &self,
        _: &ullage_daemon::PersistedState,
    ) -> Result<Box<dyn ullage_daemon::StagedSnapshot>, String> {
        Ok(Box::new(NoopStagedSnapshot))
    }
}

#[derive(Default)]
struct HangingStore {
    stage_started: Notify,
    armed: AtomicBool,
}

impl HangingStore {
    fn arm(&self) {
        self.armed.store(true, Ordering::SeqCst);
    }

    async fn wait_for_stage(&self) {
        tokio::time::timeout(Duration::from_secs(1), self.stage_started.notified())
            .await
            .unwrap();
    }
}

#[async_trait]
impl ullage_daemon::SnapshotStore for HangingStore {
    async fn load(&self) -> Result<ullage_daemon::PersistedState, String> {
        Ok(ullage_daemon::PersistedState::default())
    }

    async fn stage(
        &self,
        _: &ullage_daemon::PersistedState,
    ) -> Result<Box<dyn ullage_daemon::StagedSnapshot>, String> {
        if self.armed.load(Ordering::SeqCst) {
            self.stage_started.notify_one();
            std::future::pending().await
        } else {
            Ok(Box::new(NoopStagedSnapshot))
        }
    }
}

#[derive(Clone, Default)]
struct CommitGateStore {
    inner: Arc<CommitGateInner>,
}

struct CommitGateInner {
    state: tokio::sync::RwLock<ullage_daemon::PersistedState>,
    commit_started: Notify,
    commit_gate: Semaphore,
    armed: AtomicBool,
}

impl Default for CommitGateInner {
    fn default() -> Self {
        Self {
            state: tokio::sync::RwLock::new(ullage_daemon::PersistedState::default()),
            commit_started: Notify::new(),
            commit_gate: Semaphore::new(0),
            armed: AtomicBool::new(false),
        }
    }
}

struct CommitGateSnapshot {
    inner: Arc<CommitGateInner>,
    state: ullage_daemon::PersistedState,
}

#[derive(Clone, Default)]
struct MutationRaceStore {
    inner: Arc<MutationRaceInner>,
}

struct MutationRaceInner {
    state: tokio::sync::RwLock<PersistedState>,
    fail_next_stage: AtomicBool,
    failed_stage_started: Notify,
    release_failed_stage: Semaphore,
}

impl Default for MutationRaceInner {
    fn default() -> Self {
        Self {
            state: tokio::sync::RwLock::new(PersistedState::default()),
            fail_next_stage: AtomicBool::new(false),
            failed_stage_started: Notify::new(),
            release_failed_stage: Semaphore::new(0),
        }
    }
}

struct MutationRaceSnapshot {
    inner: Arc<MutationRaceInner>,
    state: PersistedState,
}

#[async_trait]
impl ullage_daemon::StagedSnapshot for MutationRaceSnapshot {
    async fn commit(self: Box<Self>) -> Result<(), String> {
        *self.inner.state.write().await = self.state;
        Ok(())
    }
}

#[async_trait]
impl ullage_daemon::SnapshotStore for MutationRaceStore {
    async fn load(&self) -> Result<PersistedState, String> {
        Ok(self.inner.state.read().await.clone())
    }

    async fn stage(
        &self,
        state: &PersistedState,
    ) -> Result<Box<dyn ullage_daemon::StagedSnapshot>, String> {
        if self.inner.fail_next_stage.swap(false, Ordering::SeqCst) {
            self.inner.failed_stage_started.notify_one();
            self.inner
                .release_failed_stage
                .acquire()
                .await
                .unwrap()
                .forget();
            return Err("injected account mutation failure".into());
        }
        Ok(Box::new(MutationRaceSnapshot {
            inner: self.inner.clone(),
            state: state.clone(),
        }))
    }
}

impl MutationRaceStore {
    fn fail_next_stage(&self) {
        self.inner.fail_next_stage.store(true, Ordering::SeqCst);
    }
}

#[async_trait]
impl ullage_daemon::StagedSnapshot for CommitGateSnapshot {
    async fn commit(self: Box<Self>) -> Result<(), String> {
        *self.inner.state.write().await = self.state;
        self.inner.commit_started.notify_one();
        self.inner.commit_gate.acquire().await.unwrap().forget();
        Ok(())
    }
}

impl CommitGateStore {
    fn arm(&self) {
        self.inner.armed.store(true, Ordering::SeqCst);
    }

    async fn wait_for_commit(&self) {
        tokio::time::timeout(Duration::from_secs(1), self.inner.commit_started.notified())
            .await
            .unwrap();
    }

    fn finish_commit(&self) {
        self.inner.commit_gate.add_permits(1);
    }

    async fn state(&self) -> ullage_daemon::PersistedState {
        self.inner.state.read().await.clone()
    }
}

#[async_trait]
impl ullage_daemon::SnapshotStore for CommitGateStore {
    async fn load(&self) -> Result<ullage_daemon::PersistedState, String> {
        Ok(self.state().await)
    }

    async fn stage(
        &self,
        state: &ullage_daemon::PersistedState,
    ) -> Result<Box<dyn ullage_daemon::StagedSnapshot>, String> {
        if !self.inner.armed.load(Ordering::SeqCst) {
            *self.inner.state.write().await = state.clone();
            return Ok(Box::new(NoopStagedSnapshot));
        }
        Ok(Box::new(CommitGateSnapshot {
            inner: self.inner.clone(),
            state: state.clone(),
        }))
    }
}

fn usage(provider: &ProviderId, account_label: Option<&str>) -> SubscriptionUsage {
    SubscriptionUsage {
        provider: provider.clone(),
        account_label: account_label.map(str::to_owned),
        plan: Some("test".into()),
        subscription_expires_at: None,
        observed_at: DateTime::parse_from_rfc3339("2026-08-27T12:00:00Z")
            .unwrap()
            .to_utc(),
        windows: Vec::new(),
    }
}

fn outcome_plan(outcome: &QueryOutcome<SubscriptionUsage>) -> Option<&str> {
    match outcome {
        QueryOutcome::Complete { data } | QueryOutcome::Partial { data, .. } => {
            data.plan.as_deref()
        }
    }
}

fn account(id: &str, provider: &str, interval: Duration) -> AccountConfig {
    AccountConfig {
        id: AccountId::new(id),
        provider: ProviderId::new(provider),
        query: UsageQuery {
            account_label: Some(id.into()),
        },
        enabled: true,
        interval,
        timeout: Duration::from_secs(5),
        jitter: Duration::ZERO,
        backoff: BackoffConfig {
            initial: Duration::from_secs(10),
            maximum: Duration::from_secs(40),
        },
    }
}

async fn engine_with(
    registry: Arc<ProviderRegistry>,
    clock: Arc<dyn Clock>,
    store: Arc<dyn ullage_daemon::SnapshotStore>,
    config: DaemonConfig,
) -> DaemonEngine {
    DaemonEngine::new(config, registry, clock, store)
        .await
        .unwrap()
}

#[tokio::test(flavor = "multi_thread")]
async fn startup_queries_all_accounts_and_backoff_isolates_failures() {
    let failed = MockProvider::new(
        "failed",
        [
            Err(ProviderError::Network {
                message: "token=must-not-persist".into(),
            }),
            Ok(QueryOutcome::Complete {
                data: usage(&ProviderId::new("failed"), Some("account-a")),
            }),
        ],
    );
    let healthy = MockProvider::new(
        "healthy",
        [Ok(QueryOutcome::Partial {
            data: usage(&ProviderId::new("healthy"), Some("account-b")),
            failures: vec![PartialFailure {
                scope: "usage".into(),
                message: "optional window unavailable; token=must-not-persist".into(),
            }],
        })],
    );
    let mut registry = ProviderRegistry::default();
    registry.register(failed.clone()).unwrap();
    registry.register(healthy.clone()).unwrap();
    let registry = Arc::new(registry);
    let clock = Arc::new(ManualClock::new());
    let store = Arc::new(MemorySnapshotStore::default());
    let engine = engine_with(
        registry,
        clock.clone(),
        store.clone(),
        DaemonConfig::default(),
    )
    .await;
    let mut failed_account = account("account-a", "failed", Duration::from_secs(100));
    failed_account.jitter = Duration::from_secs(5);
    engine.add_account(failed_account).await.unwrap();
    engine
        .add_account(account("account-b", "healthy", Duration::from_secs(100)))
        .await
        .unwrap();

    let running = tokio::spawn({
        let engine = engine.clone();
        async move { engine.run().await }
    });
    failed.wait_for_calls(1).await;
    healthy.wait_for_calls(1).await;
    tokio::time::timeout(Duration::from_secs(1), async {
        loop {
            let failed_done = engine.status().await.accounts.iter().any(|status| {
                status.id == AccountId::new("account-a")
                    && status.consecutive_failures == 1
                    && status.next_probe_at.is_some()
            });
            if failed_done && engine.show(&AccountId::new("account-b")).await.is_some() {
                return;
            }
            tokio::task::yield_now().await;
        }
    })
    .await
    .unwrap();

    let failed_status = engine
        .status()
        .await
        .accounts
        .into_iter()
        .find(|status| status.id == AccountId::new("account-a"))
        .unwrap();
    assert_eq!(failed_status.consecutive_failures, 1);
    assert_eq!(failed_status.last_error, Some(SanitizedError::Network));
    assert!(matches!(
        engine.show(&AccountId::new("account-b")).await.unwrap().usage,
        QueryOutcome::Partial { failures, .. }
            if failures.len() == 1
                && failures[0].scope == "usage"
                && failures[0].message == "provider protocol response is incompatible"
    ));
    let persisted = serde_json::to_string(&store.state().await).unwrap();
    assert!(!persisted.contains("optional window unavailable"));
    assert!(!persisted.contains("must-not-persist"));
    assert!(persisted.contains("\"scope\":\"usage\""));
    assert_eq!(store.state().await.failures.len(), 1);

    let backoff_with_jitter = (failed_status.next_probe_at.unwrap() - clock.now())
        .to_std()
        .unwrap();
    assert!(backoff_with_jitter >= Duration::from_secs(10));
    assert!(backoff_with_jitter <= Duration::from_secs(15));
    clock.wait_for_sleep(backoff_with_jitter).await;
    clock.advance(backoff_with_jitter - Duration::from_millis(1));
    tokio::task::yield_now().await;
    assert_eq!(failed.calls(), 1);
    clock.advance(Duration::from_millis(1));
    failed.wait_for_calls(2).await;
    tokio::time::timeout(Duration::from_secs(1), async {
        while engine.show(&AccountId::new("account-a")).await.is_none() {
            tokio::task::yield_now().await;
        }
    })
    .await
    .unwrap();
    assert!(engine.show(&AccountId::new("account-a")).await.is_some());
    assert_eq!(healthy.calls(), 1);

    engine.shutdown();
    running.await.unwrap();
}

#[tokio::test(flavor = "multi_thread")]
async fn manual_rate_limit_moves_the_existing_periodic_deadline() {
    let provider = MockProvider::new(
        "rate-limited",
        [
            Ok(QueryOutcome::Complete {
                data: usage(&ProviderId::new("rate-limited"), Some("rate-limited")),
            }),
            Err(ProviderError::RateLimited {
                message: "sensitive provider response".into(),
                retry_after_seconds: Some(3_600),
            }),
        ],
    );
    let mut registry = ProviderRegistry::default();
    registry.register(provider.clone()).unwrap();
    let clock = Arc::new(ManualClock::new());
    let engine = engine_with(
        Arc::new(registry),
        clock.clone(),
        Arc::new(MemorySnapshotStore::default()),
        DaemonConfig::default(),
    )
    .await;
    engine
        .add_account(account(
            "rate-limited",
            "rate-limited",
            Duration::from_secs(10),
        ))
        .await
        .unwrap();
    let running = tokio::spawn({
        let engine = engine.clone();
        async move { engine.run().await }
    });
    provider.wait_for_calls(1).await;
    clock.wait_for_sleep(Duration::from_secs(10)).await;

    assert!(matches!(
        engine
            .probe(&AccountId::new("rate-limited"), ProbeTrigger::Manual)
            .await,
        Err(ProbeError::Provider(ProviderError::RateLimited {
            retry_after_seconds: Some(3_600),
            ..
        }))
    ));
    clock.wait_for_sleep(Duration::from_secs(3_600)).await;
    clock.advance(Duration::from_secs(10));
    tokio::task::yield_now().await;
    assert_eq!(provider.calls(), 2);
    clock.advance(Duration::from_secs(3_590));
    provider.wait_for_calls(3).await;

    engine.shutdown();
    running.await.unwrap();
}

#[tokio::test]
async fn manual_and_periodic_probes_share_one_flight_and_provider_limit() {
    let (provider, gate) = MockProvider::gated("limited");
    let mut registry = ProviderRegistry::default();
    registry.register(provider.clone()).unwrap();
    let registry = Arc::new(registry);
    let engine = engine_with(
        registry,
        Arc::new(ManualClock::new()),
        Arc::new(MemorySnapshotStore::default()),
        DaemonConfig {
            maximum_concurrency: 2,
            default_provider_concurrency: 2,
            provider_limits: vec![ProviderLimit {
                provider: ProviderId::new("limited"),
                maximum_concurrency: 1,
            }],
        },
    )
    .await;
    engine
        .add_account(account("one", "limited", Duration::from_secs(60)))
        .await
        .unwrap();
    engine
        .add_account(account("two", "limited", Duration::from_secs(60)))
        .await
        .unwrap();

    let first = tokio::spawn({
        let engine = engine.clone();
        async move {
            engine
                .probe(&AccountId::new("one"), ProbeTrigger::Manual)
                .await
        }
    });
    provider.wait_for_calls(1).await;
    let follower = tokio::spawn({
        let engine = engine.clone();
        async move {
            engine
                .probe(&AccountId::new("one"), ProbeTrigger::Periodic)
                .await
        }
    });
    tokio::task::yield_now().await;
    assert_eq!(provider.calls(), 1);
    gate.add_permits(1);
    assert_eq!(first.await.unwrap(), follower.await.unwrap());

    let one = tokio::spawn({
        let engine = engine.clone();
        async move {
            engine
                .probe(&AccountId::new("one"), ProbeTrigger::Manual)
                .await
        }
    });
    let two = tokio::spawn({
        let engine = engine.clone();
        async move {
            engine
                .probe(&AccountId::new("two"), ProbeTrigger::Manual)
                .await
        }
    });
    provider.wait_for_calls(2).await;
    tokio::task::yield_now().await;
    assert_eq!(provider.calls(), 2);
    gate.add_permits(1);
    provider.wait_for_calls(3).await;
    gate.add_permits(1);
    one.await.unwrap().unwrap();
    two.await.unwrap().unwrap();
    assert_eq!(provider.maximum_active(), 1);
}

#[tokio::test(flavor = "multi_thread")]
async fn global_limit_applies_across_different_providers() {
    let (first_provider, first_gate) = MockProvider::gated("first");
    let (second_provider, second_gate) = MockProvider::gated("second");
    let mut registry = ProviderRegistry::default();
    registry.register(first_provider.clone()).unwrap();
    registry.register(second_provider.clone()).unwrap();
    let engine = engine_with(
        Arc::new(registry),
        Arc::new(ManualClock::new()),
        Arc::new(MemorySnapshotStore::default()),
        DaemonConfig {
            maximum_concurrency: 1,
            default_provider_concurrency: 2,
            provider_limits: Vec::new(),
        },
    )
    .await;
    engine
        .add_account(account("first", "first", Duration::from_secs(60)))
        .await
        .unwrap();
    engine
        .add_account(account("second", "second", Duration::from_secs(60)))
        .await
        .unwrap();

    let first = tokio::spawn({
        let engine = engine.clone();
        async move {
            engine
                .probe(&AccountId::new("first"), ProbeTrigger::Manual)
                .await
        }
    });
    let second = tokio::spawn({
        let engine = engine.clone();
        async move {
            engine
                .probe(&AccountId::new("second"), ProbeTrigger::Manual)
                .await
        }
    });
    tokio::time::timeout(Duration::from_secs(1), async {
        loop {
            let total = first_provider.calls() + second_provider.calls();
            if total == 1 {
                return;
            }
            tokio::task::yield_now().await;
        }
    })
    .await
    .unwrap();
    assert_eq!(first_provider.calls() + second_provider.calls(), 1);

    if first_provider.calls() == 1 {
        first_gate.add_permits(1);
        second_provider.wait_for_calls(1).await;
        second_gate.add_permits(1);
    } else {
        second_gate.add_permits(1);
        first_provider.wait_for_calls(1).await;
        first_gate.add_permits(1);
    }
    first.await.unwrap().unwrap();
    second.await.unwrap().unwrap();
}

#[tokio::test(flavor = "multi_thread")]
async fn saturated_provider_does_not_reserve_global_capacity() {
    let (slow_provider, slow_gate) = MockProvider::gated("slow");
    let (healthy_provider, healthy_gate) = MockProvider::gated("healthy-limit");
    let mut registry = ProviderRegistry::default();
    registry.register(slow_provider.clone()).unwrap();
    registry.register(healthy_provider.clone()).unwrap();
    let engine = engine_with(
        Arc::new(registry),
        Arc::new(ManualClock::new()),
        Arc::new(MemorySnapshotStore::default()),
        DaemonConfig {
            maximum_concurrency: 2,
            default_provider_concurrency: 1,
            provider_limits: Vec::new(),
        },
    )
    .await;
    for id in ["slow-one", "slow-two"] {
        engine
            .add_account(account(id, "slow", Duration::from_secs(60)))
            .await
            .unwrap();
    }
    engine
        .add_account(account(
            "healthy-limit",
            "healthy-limit",
            Duration::from_secs(60),
        ))
        .await
        .unwrap();

    let slow_one = tokio::spawn({
        let engine = engine.clone();
        async move {
            engine
                .probe(&AccountId::new("slow-one"), ProbeTrigger::Manual)
                .await
        }
    });
    slow_provider.wait_for_calls(1).await;
    let slow_two = tokio::spawn({
        let engine = engine.clone();
        async move {
            engine
                .probe(&AccountId::new("slow-two"), ProbeTrigger::Manual)
                .await
        }
    });
    tokio::time::sleep(Duration::from_millis(1)).await;
    let healthy = tokio::spawn({
        let engine = engine.clone();
        async move {
            engine
                .probe(&AccountId::new("healthy-limit"), ProbeTrigger::Manual)
                .await
        }
    });
    healthy_provider.wait_for_calls(1).await;
    assert_eq!(slow_provider.calls(), 1);

    healthy_gate.add_permits(1);
    healthy.await.unwrap().unwrap();
    slow_gate.add_permits(1);
    slow_provider.wait_for_calls(2).await;
    slow_gate.add_permits(1);
    slow_one.await.unwrap().unwrap();
    slow_two.await.unwrap().unwrap();
}

#[tokio::test(flavor = "multi_thread")]
async fn duplicate_control_selectors_are_rejected() {
    let provider = MockProvider::new("selector", []);
    let mut registry = ProviderRegistry::default();
    registry.register(provider).unwrap();
    let engine = engine_with(
        Arc::new(registry),
        Arc::new(ManualClock::new()),
        Arc::new(MemorySnapshotStore::default()),
        DaemonConfig::default(),
    )
    .await;
    engine
        .add_account(account("same-label", "selector", Duration::from_secs(60)))
        .await
        .unwrap();
    let mut duplicate = account("different-id", "selector", Duration::from_secs(60));
    duplicate.query.account_label = Some("same-label".into());
    assert!(matches!(
        engine.add_account(duplicate).await,
        Err(DaemonError::DuplicateAccountSelector { .. })
    ));
}

#[tokio::test(flavor = "multi_thread")]
async fn renaming_an_account_keeps_provider_labels_unique() {
    let provider = MockProvider::new("renamed", []);
    let mut registry = ProviderRegistry::default();
    registry.register(provider).unwrap();
    let engine = engine_with(
        Arc::new(registry),
        Arc::new(ManualClock::new()),
        Arc::new(MemorySnapshotStore::default()),
        DaemonConfig::default(),
    )
    .await;
    engine
        .add_account(account("account-a", "renamed", Duration::from_secs(60)))
        .await
        .unwrap();
    let mut other = account("account-b", "renamed", Duration::from_secs(60));
    other.query.account_label = Some("other".into());
    engine.add_account(other).await.unwrap();

    assert!(matches!(
        engine
            .set_account_label(&AccountId::new("account-b"), Some("account-a".into()))
            .await,
        Err(DaemonError::DuplicateAccountSelector { .. })
    ));
    let renamed = engine
        .set_account_label(&AccountId::new("account-b"), Some("work".into()))
        .await
        .unwrap()
        .unwrap();
    assert_eq!(renamed.query.account_label.as_deref(), Some("work"));
}

struct FailingAuthProvider {
    id: ProviderId,
}

#[async_trait]
impl Provider for FailingAuthProvider {
    type VendorUsage = SubscriptionUsage;

    fn descriptor(&self) -> ProviderDescriptor {
        ProviderDescriptor {
            id: self.id.clone(),
            display_name: "failing auth".into(),
            capabilities: vec![Capability::Authentication],
        }
    }

    async fn start_auth(&self, _: AuthStartRequest) -> ProviderResult<AuthChallenge> {
        Err(ProviderError::AuthenticationInvalid {
            message: "secret start detail must-not-cross-control-boundary".into(),
        })
    }

    async fn complete_auth(&self, _: AuthCompleteRequest) -> ProviderResult<AuthState> {
        Err(ProviderError::AuthenticationInvalid {
            message: "secret complete detail must-not-cross-control-boundary".into(),
        })
    }

    async fn auth_status(&self) -> ProviderResult<AuthState> {
        Ok(AuthState::NotAuthenticated)
    }

    async fn logout(&self, _: LogoutRequest) -> ProviderResult<()> {
        Ok(())
    }

    async fn query(&self, _: UsageQuery) -> ProviderResult<QueryOutcome<Self::VendorUsage>> {
        Err(ProviderError::Network {
            message: "secret query detail must-not-cross-control-boundary".into(),
        })
    }

    fn normalize(&self, usage: Self::VendorUsage) -> ProviderResult<SubscriptionUsage> {
        Ok(usage)
    }
}

#[tokio::test(flavor = "multi_thread")]
async fn diagnostics_are_opt_in_for_auth_and_probe() {
    let mut registry = ProviderRegistry::default();
    registry
        .register(FailingAuthProvider {
            id: ProviderId::new("diag"),
        })
        .unwrap();
    let engine = engine_with(
        Arc::new(registry),
        Arc::new(ManualClock::new()),
        Arc::new(MemorySnapshotStore::default()),
        DaemonConfig::default(),
    )
    .await;
    engine
        .add_account(account("primary", "diag", Duration::from_secs(60)))
        .await
        .unwrap();
    let service = ControlService::new(engine);

    let start = ControlCommand::StartAuth {
        provider: ProviderId::new("diag"),
        account: ullage_protocol::AccountId::new("primary"),
        request: AuthStartRequest {
            method: None,
            redirect_uri: None,
        },
    };
    let hidden = service
        .handle(ControlRequest::new("auth-hidden", start.clone()))
        .await;
    assert!(matches!(hidden.result, ControlResult::Error(_)));
    assert_eq!(hidden.diagnostic, None);
    let encoded = serde_json::to_string(&hidden).unwrap();
    assert!(!encoded.contains("must-not-cross-control-boundary"));

    let shown = service
        .handle(ControlRequest::new("auth-shown", start).with_diagnostics(true))
        .await;
    assert!(matches!(shown.result, ControlResult::Error(_)));
    assert_eq!(
        shown.diagnostic.as_deref(),
        Some("authentication is invalid: secret start detail must-not-cross-control-boundary")
    );

    let probe_command = ControlCommand::Probe {
        account_id: "primary".into(),
        wait: true,
    };
    let hidden_probe = service
        .handle(ControlRequest::new("probe-hidden", probe_command.clone()))
        .await;
    assert!(matches!(hidden_probe.result, ControlResult::Error(_)));
    assert_eq!(hidden_probe.diagnostic, None);
    let encoded_hidden_probe = serde_json::to_string(&hidden_probe).unwrap();
    assert!(!encoded_hidden_probe.contains("must-not-cross-control-boundary"));

    let shown_probe = service
        .handle(ControlRequest::new("probe-shown", probe_command).with_diagnostics(true))
        .await;
    assert!(matches!(shown_probe.result, ControlResult::Error(_)));
    assert_eq!(
        shown_probe.diagnostic.as_deref(),
        Some("network error: secret query detail must-not-cross-control-boundary")
    );
    let encoded_shown_probe = serde_json::to_string(&shown_probe).unwrap();
    assert!(encoded_shown_probe.contains("provider network request failed"));

    let query = service
        .handle(
            ControlRequest::new(
                "query-shown",
                ControlCommand::QueryUsage {
                    provider: ProviderId::new("diag"),
                    query: UsageQuery {
                        account_label: Some("primary".into()),
                    },
                },
            )
            .with_diagnostics(true),
        )
        .await;
    assert!(matches!(query.result, ControlResult::Error(_)));
    assert_eq!(query.diagnostic, None);
    let encoded_query = serde_json::to_string(&query).unwrap();
    assert!(!encoded_query.contains("must-not-cross-control-boundary"));
}

struct RecordingAuthProvider {
    id: ProviderId,
    last_start: Arc<Mutex<Option<AuthStartRequest>>>,
}

#[async_trait]
impl Provider for RecordingAuthProvider {
    type VendorUsage = SubscriptionUsage;

    fn descriptor(&self) -> ProviderDescriptor {
        ProviderDescriptor {
            id: self.id.clone(),
            display_name: "recording auth".into(),
            capabilities: vec![Capability::Authentication],
        }
    }

    async fn start_auth(&self, request: AuthStartRequest) -> ProviderResult<AuthChallenge> {
        *self.last_start.lock().unwrap() = Some(request);
        Ok(AuthChallenge {
            flow_id: "recorded-flow".into(),
            method: AuthMethod::BrowserOAuth,
            verification_uri: Some("https://example.test/authorize".into()),
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

    async fn query(&self, _: UsageQuery) -> ProviderResult<QueryOutcome<Self::VendorUsage>> {
        Err(ProviderError::UnsupportedCapability {
            capability: "usage".into(),
        })
    }

    fn normalize(&self, usage: Self::VendorUsage) -> ProviderResult<SubscriptionUsage> {
        Ok(usage)
    }
}

#[tokio::test(flavor = "multi_thread")]
async fn auth_start_redirect_uri_is_local_only() {
    let last_start = Arc::new(Mutex::new(None));
    let provider = RecordingAuthProvider {
        id: ProviderId::new("redirect"),
        last_start: Arc::clone(&last_start),
    };
    let mut registry = ProviderRegistry::default();
    registry.register(provider).unwrap();
    let engine = engine_with(
        Arc::new(registry),
        Arc::new(ManualClock::new()),
        Arc::new(MemorySnapshotStore::default()),
        DaemonConfig::default(),
    )
    .await;
    engine
        .add_account(account("primary", "redirect", Duration::from_secs(60)))
        .await
        .unwrap();
    let service = ControlService::new(engine);

    let rejected = service
        .handle_with_transport(
            ControlRequest::new(
                "redirect-reject",
                ControlCommand::StartAuth {
                    provider: ProviderId::new("redirect"),
                    account: ullage_protocol::AccountId::new("primary"),
                    request: AuthStartRequest {
                        method: Some(AuthMethod::BrowserOAuth),
                        redirect_uri: Some("https://attacker.invalid/callback".into()),
                    },
                },
            ),
            ControlTransport::Local,
        )
        .await;
    assert!(matches!(
        rejected.result,
        ControlResult::Error(ControlError::Provider(
            ProviderError::AuthenticationInvalid { .. }
        ))
    ));
    assert!(last_start.lock().unwrap().is_none());

    let start = ControlCommand::StartAuth {
        provider: ProviderId::new("redirect"),
        account: ullage_protocol::AccountId::new("primary"),
        request: AuthStartRequest {
            method: Some(AuthMethod::BrowserOAuth),
            redirect_uri: Some("http://127.0.0.1:54321/auth/callback".into()),
        },
    };

    let accepted = service
        .handle_with_transport(
            ControlRequest::new("redirect-local", start.clone()),
            ControlTransport::Local,
        )
        .await;
    assert!(matches!(accepted.result, ControlResult::AuthChallenge(_)));
    assert_eq!(
        last_start
            .lock()
            .unwrap()
            .as_ref()
            .and_then(|request| request.redirect_uri.as_deref()),
        Some("http://127.0.0.1:54321/auth/callback")
    );

    *last_start.lock().unwrap() = None;
    let remote = service
        .handle_with_transport(
            ControlRequest::new("redirect-remote", start),
            ControlTransport::RemoteHttp,
        )
        .await;
    assert!(matches!(remote.result, ControlResult::AuthChallenge(_)));
    assert_eq!(
        last_start
            .lock()
            .unwrap()
            .as_ref()
            .and_then(|request| request.redirect_uri.as_deref()),
        None
    );
}

struct InvalidAuthProvider {
    id: ProviderId,
}

#[async_trait]
impl Provider for InvalidAuthProvider {
    type VendorUsage = SubscriptionUsage;

    fn descriptor(&self) -> ProviderDescriptor {
        ProviderDescriptor {
            id: self.id.clone(),
            display_name: "invalid auth".into(),
            capabilities: vec![Capability::Authentication],
        }
    }

    async fn start_auth(&self, _: AuthStartRequest) -> ProviderResult<AuthChallenge> {
        Ok(AuthChallenge {
            flow_id: "flow-1".into(),
            method: AuthMethod::DeviceCode,
            verification_uri: Some("https://example.invalid/device".into()),
            user_code: None,
            expires_at: None,
            input: None,
        })
    }

    async fn complete_auth(&self, _: AuthCompleteRequest) -> ProviderResult<AuthState> {
        Ok(AuthState::Invalid {
            reason: "secret invalid detail must-not-cross-control-boundary".into(),
        })
    }

    async fn auth_status(&self) -> ProviderResult<AuthState> {
        Ok(AuthState::Invalid {
            reason: "secret status detail must-not-cross-control-boundary".into(),
        })
    }

    async fn logout(&self, _: LogoutRequest) -> ProviderResult<()> {
        Ok(())
    }

    async fn query(&self, _: UsageQuery) -> ProviderResult<QueryOutcome<Self::VendorUsage>> {
        Ok(QueryOutcome::Complete {
            data: usage(&self.id, None),
        })
    }

    fn normalize(&self, usage: Self::VendorUsage) -> ProviderResult<SubscriptionUsage> {
        Ok(usage)
    }
}

#[tokio::test(flavor = "multi_thread")]
async fn invalid_auth_state_reason_is_opt_in() {
    let mut registry = ProviderRegistry::default();
    registry
        .register(InvalidAuthProvider {
            id: ProviderId::new("invalid"),
        })
        .unwrap();
    let engine = engine_with(
        Arc::new(registry),
        Arc::new(ManualClock::new()),
        Arc::new(MemorySnapshotStore::default()),
        DaemonConfig::default(),
    )
    .await;
    engine
        .add_account(account("primary", "invalid", Duration::from_secs(60)))
        .await
        .unwrap();
    let service = ControlService::new(engine);
    let complete = ControlCommand::CompleteAuth {
        provider: ProviderId::new("invalid"),
        account: ullage_protocol::AccountId::new("primary"),
        request: AuthCompleteRequest {
            flow_id: "flow-1".into(),
            authorization_code: None,
            redirect_uri: None,
        },
    };

    let hidden = service
        .handle(ControlRequest::new("invalid-hidden", complete.clone()))
        .await;
    assert_eq!(
        hidden.result,
        ControlResult::AuthState(AuthState::Invalid {
            reason: "provider authentication is invalid".into(),
        })
    );
    let encoded = serde_json::to_string(&hidden).unwrap();
    assert!(!encoded.contains("must-not-cross-control-boundary"));

    let shown = service
        .handle(ControlRequest::new("invalid-shown", complete).with_diagnostics(true))
        .await;
    assert_eq!(
        shown.result,
        ControlResult::AuthState(AuthState::Invalid {
            reason: "secret invalid detail must-not-cross-control-boundary".into(),
        })
    );
}

#[tokio::test(flavor = "multi_thread")]
async fn json_store_restores_success_and_sanitized_failure() {
    static NEXT_PATH: AtomicUsize = AtomicUsize::new(0);
    let root = std::env::temp_dir().join(format!(
        "ullage-daemon-{}-{}",
        std::process::id(),
        NEXT_PATH.fetch_add(1, Ordering::SeqCst)
    ));
    let directory = root.join("nested/state");
    let path = directory.join("snapshots.json");
    let provider = MockProvider::new(
        "persisted",
        [
            Ok(QueryOutcome::Complete {
                data: usage(&ProviderId::new("persisted"), Some("saved")),
            }),
            Err(ProviderError::AuthenticationInvalid {
                message: "secret credential text".into(),
            }),
        ],
    );
    let mut registry = ProviderRegistry::default();
    registry.register(provider).unwrap();
    let registry = Arc::new(registry);
    let store = Arc::new(JsonSnapshotStore::new(&path));
    let engine = engine_with(
        registry.clone(),
        Arc::new(ManualClock::new()),
        store.clone(),
        DaemonConfig::default(),
    )
    .await;
    engine
        .add_account(account("saved", "persisted", Duration::from_secs(60)))
        .await
        .unwrap();
    engine
        .probe(&AccountId::new("saved"), ProbeTrigger::Manual)
        .await
        .unwrap();
    assert!(matches!(
        engine
            .probe(&AccountId::new("saved"), ProbeTrigger::Manual)
            .await,
        Err(ProbeError::Provider(
            ProviderError::AuthenticationInvalid { .. }
        ))
    ));
    engine
        .set_account_enabled(&AccountId::new("saved"), false)
        .await
        .unwrap()
        .unwrap();

    let restored = engine_with(
        registry,
        Arc::new(ManualClock::new()),
        store,
        DaemonConfig::default(),
    )
    .await;
    let restored_account = restored
        .account_config(&AccountId::new("saved"))
        .await
        .unwrap();
    assert!(!restored_account.enabled);
    assert_eq!(
        restored_account.query.account_label.as_deref(),
        Some("saved")
    );
    let record = restored.show(&AccountId::new("saved")).await.unwrap();
    assert!(record.stale);
    assert_eq!(
        record.last_error,
        Some(SanitizedError::AuthenticationInvalid)
    );
    let bytes = std::fs::read(&path).unwrap();
    assert!(
        !String::from_utf8(bytes)
            .unwrap()
            .contains("secret credential text")
    );
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        assert_eq!(
            std::fs::metadata(&directory).unwrap().permissions().mode() & 0o777,
            0o700
        );
        assert_eq!(
            std::fs::metadata(&path).unwrap().permissions().mode() & 0o777,
            0o600
        );
    }
    std::fs::remove_dir_all(root).unwrap();
}

#[tokio::test(flavor = "multi_thread")]
async fn legacy_partial_failure_snapshots_still_load() {
    let failure: PartialFailure = serde_json::from_value(serde_json::json!({
        "scope": "provider",
        "message": "provider reported partial data"
    }))
    .unwrap();
    assert_eq!(failure.scope, "provider");
    assert_eq!(failure.message, "provider reported partial data");

    let account_id = AccountId::new("legacy");
    let mut persisted = PersistedState::default();
    persisted.accounts.insert(
        account_id.clone(),
        account("legacy", "legacy", Duration::from_secs(60)),
    );
    persisted.snapshots.insert(
        account_id.clone(),
        SnapshotRecord {
            account_id: account_id.clone(),
            usage: QueryOutcome::Partial {
                data: usage(&ProviderId::new("legacy"), Some("legacy")),
                failures: vec![failure],
            },
            last_success_at: Utc::now(),
            stale: false,
            last_error: None,
            last_error_at: None,
        },
    );
    let encoded = serde_json::to_vec(&persisted).unwrap();
    let decoded: PersistedState = serde_json::from_slice(&encoded).unwrap();

    let store = Arc::new(MemorySnapshotStore::default());
    ullage_daemon::SnapshotStore::stage(store.as_ref(), &decoded)
        .await
        .unwrap()
        .commit()
        .await
        .unwrap();

    let provider = MockProvider::new("legacy", []);
    let mut registry = ProviderRegistry::default();
    registry.register(provider).unwrap();
    let engine = engine_with(
        Arc::new(registry),
        Arc::new(ManualClock::new()),
        store,
        DaemonConfig::default(),
    )
    .await;
    let snapshot = engine.show(&account_id).await.unwrap();
    match snapshot.usage {
        QueryOutcome::Partial { failures, .. } => {
            assert_eq!(failures[0].scope, "provider");
            assert_eq!(failures[0].message, "provider reported partial data");
        }
        other => panic!("expected legacy partial snapshot, got {other:?}"),
    }
}

#[cfg(unix)]
#[tokio::test(flavor = "multi_thread")]
async fn snapshot_load_rejects_public_parents_and_symbolic_links() {
    use std::os::unix::fs::{PermissionsExt, symlink};

    static NEXT_UNSAFE_PATH: AtomicUsize = AtomicUsize::new(0);
    let directory = std::env::temp_dir().join(format!(
        "ullage-unsafe-snapshot-{}-{}",
        std::process::id(),
        NEXT_UNSAFE_PATH.fetch_add(1, Ordering::SeqCst)
    ));
    std::fs::create_dir(&directory).unwrap();
    std::fs::set_permissions(&directory, std::fs::Permissions::from_mode(0o755)).unwrap();
    let path = directory.join("snapshots.json");
    std::fs::write(&path, br#"{"snapshots":{},"failures":{}}"#).unwrap();
    let public_store = JsonSnapshotStore::new(&path);
    assert!(
        ullage_daemon::SnapshotStore::load(&public_store)
            .await
            .unwrap_err()
            .contains("parent permissions")
    );

    std::fs::set_permissions(&directory, std::fs::Permissions::from_mode(0o700)).unwrap();
    std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o600)).unwrap();
    let link = directory.join("linked.json");
    symlink(&path, &link).unwrap();
    let linked_store = JsonSnapshotStore::new(&link);
    assert!(
        ullage_daemon::SnapshotStore::load(&linked_store)
            .await
            .unwrap_err()
            .contains("regular file")
    );
    std::fs::remove_dir_all(directory).unwrap();
}

#[tokio::test(flavor = "multi_thread")]
async fn shutdown_cancels_an_in_flight_startup_query() {
    let (provider, _gate) = MockProvider::gated("blocked");
    let mut registry = ProviderRegistry::default();
    registry.register(provider.clone()).unwrap();
    let engine = engine_with(
        Arc::new(registry),
        Arc::new(ManualClock::new()),
        Arc::new(MemorySnapshotStore::default()),
        DaemonConfig::default(),
    )
    .await;
    engine
        .add_account(account("blocked", "blocked", Duration::from_secs(60)))
        .await
        .unwrap();
    let running = tokio::spawn({
        let engine = engine.clone();
        async move { engine.run().await }
    });
    provider.wait_for_calls(1).await;
    engine.shutdown();
    tokio::time::timeout(Duration::from_secs(1), running)
        .await
        .unwrap()
        .unwrap();
    assert!(engine.is_shutting_down());
    assert_eq!(engine.status().await.accounts[0].last_error, None);
}

#[tokio::test(flavor = "multi_thread")]
async fn control_service_supports_status_auth_probe_show_and_version_checks() {
    let provider = MockProvider::new(
        "control",
        [
            Ok(QueryOutcome::Complete {
                data: usage(&ProviderId::new("control"), Some("primary")),
            }),
            Ok(QueryOutcome::Complete {
                data: usage(&ProviderId::new("control"), Some("primary")),
            }),
            Ok(QueryOutcome::Complete {
                data: usage(&ProviderId::new("control"), Some("primary")),
            }),
            Err(ProviderError::Network {
                message: "token=must-not-cross-control-boundary".into(),
            }),
        ],
    );
    let mut registry = ProviderRegistry::default();
    registry.register(provider.clone()).unwrap();
    let registry = Arc::new(registry);
    let engine = engine_with(
        registry.clone(),
        Arc::new(ManualClock::new()),
        Arc::new(MemorySnapshotStore::default()),
        DaemonConfig::default(),
    )
    .await;
    engine
        .add_account(account("primary", "control", Duration::from_secs(60)))
        .await
        .unwrap();
    let service = ControlService::new(engine.clone());

    assert_eq!(service.daemon_status().await.accounts.len(), 1);
    let empty_show = service
        .handle(ControlRequest::new(
            "show-empty",
            ControlCommand::Show {
                account_id: Some("primary".into()),
            },
        ))
        .await;
    assert!(matches!(
        empty_show.result,
        ControlResult::Snapshots(ref values) if values.is_empty()
    ));
    service.probe(&AccountId::new("primary")).await.unwrap();
    assert!(service.show(&AccountId::new("primary")).await.is_some());

    let status = service
        .handle(ControlRequest::new(
            "status-1",
            ControlCommand::DaemonStatus,
        ))
        .await;
    assert!(matches!(status.result, ControlResult::DaemonStatus(_)));

    let probe = service
        .handle(ControlRequest::new(
            "probe-1",
            ControlCommand::Probe {
                account_id: "primary".into(),
                wait: true,
            },
        ))
        .await;
    assert!(matches!(
        probe.result,
        ControlResult::Probe(ref payload) if payload.account_id == "primary"
    ));

    let missing_trigger = service
        .handle(ControlRequest::new(
            "probe-no-wait-missing",
            ControlCommand::Probe {
                account_id: "missing".into(),
                wait: false,
            },
        ))
        .await;
    assert!(matches!(
        missing_trigger.result,
        ControlResult::Error(ullage_protocol::ControlError::AccountNotFound { .. })
    ));

    let show = service
        .handle(ControlRequest::new(
            "show-1",
            ControlCommand::Show {
                account_id: Some("primary".into()),
            },
        ))
        .await;
    assert!(matches!(show.result, ControlResult::Snapshots(ref values) if values.len() == 1));

    let response = service
        .handle(ControlRequest::new(
            "query-1",
            ControlCommand::QueryUsage {
                provider: ProviderId::new("control"),
                query: UsageQuery {
                    account_label: Some("primary".into()),
                },
            },
        ))
        .await;
    assert!(matches!(response.result, ControlResult::Usage(_)));

    let error_response = service
        .handle(ControlRequest::new(
            "query-error",
            ControlCommand::QueryUsage {
                provider: ProviderId::new("control"),
                query: UsageQuery {
                    account_label: Some("primary".into()),
                },
            },
        ))
        .await;
    let encoded_error = serde_json::to_string(&error_response).unwrap();
    assert!(encoded_error.contains("provider network request failed"));
    assert!(!encoded_error.contains("must-not-cross-control-boundary"));

    let triggered = service
        .handle(ControlRequest::new(
            "probe-no-wait",
            ControlCommand::Probe {
                account_id: "primary".into(),
                wait: false,
            },
        ))
        .await;
    assert!(matches!(triggered.result, ControlResult::Ack));

    let missing_selector = service
        .handle(ControlRequest::new(
            "missing-selector",
            ControlCommand::QueryUsage {
                provider: ProviderId::new("control"),
                query: UsageQuery {
                    account_label: Some("secondary".into()),
                },
            },
        ))
        .await;
    assert_eq!(
        missing_selector.result,
        ControlResult::Error(ullage_protocol::ControlError::AccountSelectorNotFound {
            provider: ProviderId::new("control"),
            account_label: Some("secondary".into()),
        })
    );
    let missing_provider = service
        .handle(ControlRequest::new(
            "missing-provider",
            ControlCommand::QueryUsage {
                provider: ProviderId::new("missing"),
                query: UsageQuery {
                    account_label: Some("primary".into()),
                },
            },
        ))
        .await;
    assert_eq!(
        missing_provider.result,
        ControlResult::Error(ullage_protocol::ControlError::Registry(
            ullage_core::RegistryError::NotFound(ProviderId::new("missing")),
        ))
    );

    let auth = service
        .handle(ControlRequest::new(
            "auth-1",
            ControlCommand::StartAuth {
                provider: ProviderId::new("control"),
                account: ullage_protocol::AccountId::new("primary"),
                request: AuthStartRequest {
                    method: None,
                    redirect_uri: None,
                },
            },
        ))
        .await;
    assert!(matches!(
        auth.result,
        ControlResult::AuthChallenge(AuthChallenge { ref user_code, .. })
            if user_code.as_deref() == Some("secret-code")
    ));

    assert!(!ControlCommand::CreatePairCode.accepts_diagnostics());
    assert!(!ControlCommand::ListDevices.accepts_diagnostics());
    assert!(
        !ControlCommand::RevokeDevice {
            device_id: "missing".into()
        }
        .accepts_diagnostics()
    );
    let pair_code = service
        .handle(
            ControlRequest::new("pair-code", ControlCommand::CreatePairCode).with_diagnostics(true),
        )
        .await;
    assert_eq!(pair_code.diagnostic, None);
    let ControlResult::PairCode(pair_code) = pair_code.result else {
        panic!("create pair code should return a code");
    };
    let credential = service
        .pair_device(&pair_code.code, "control-device")
        .unwrap();
    let devices = service
        .handle(ControlRequest::new("devices", ControlCommand::ListDevices))
        .await;
    let encoded_devices = serde_json::to_string(&devices).unwrap();
    assert!(!encoded_devices.contains(&credential.device_token));
    assert!(!encoded_devices.contains("token_hash"));
    assert!(matches!(
        devices.result,
        ControlResult::Devices(ref devices)
            if devices.len() == 1 && devices[0].id == credential.device_id
    ));
    let missing_device = service
        .handle(ControlRequest::new(
            "revoke-missing",
            ControlCommand::RevokeDevice {
                device_id: "missing".into(),
            },
        ))
        .await;
    assert!(matches!(
        missing_device.result,
        ControlResult::Error(ullage_protocol::ControlError::DeviceNotFound { ref device_id })
            if device_id == "missing"
    ));
    for request_id in ["revoke", "revoke-again"] {
        let revoked = service
            .handle(ControlRequest::new(
                request_id,
                ControlCommand::RevokeDevice {
                    device_id: credential.device_id.clone(),
                },
            ))
            .await;
        assert_eq!(revoked.result, ControlResult::Ack);
    }
    assert!(
        !service
            .authenticate_device(&credential.device_token)
            .unwrap()
    );

    let mut incompatible = ControlRequest::new("old", ControlCommand::ListProviders);
    incompatible.version = 8;
    assert!(matches!(
        service.handle(incompatible).await.result,
        ControlResult::ProtocolMismatch {
            supported_version
        } if supported_version == CONTROL_PROTOCOL_VERSION
    ));

    engine.shutdown();
    let rejected_trigger = service
        .handle(ControlRequest::new(
            "probe-no-wait-cancelled",
            ControlCommand::Probe {
                account_id: "primary".into(),
                wait: false,
            },
        ))
        .await;
    assert_eq!(
        rejected_trigger.result,
        ControlResult::Error(ullage_protocol::ControlError::Cancelled)
    );
    let cancelled = service
        .handle(ControlRequest::new(
            "probe-cancelled",
            ControlCommand::Probe {
                account_id: "primary".into(),
                wait: true,
            },
        ))
        .await;
    assert_eq!(
        cancelled.result,
        ControlResult::Error(ullage_protocol::ControlError::Cancelled)
    );
}

#[tokio::test(flavor = "multi_thread")]
async fn control_service_manages_accounts_through_the_real_engine() {
    let provider = MockProvider::new("managed", []);
    let mut registry = ProviderRegistry::default();
    registry.register(provider.clone()).unwrap();
    let engine = engine_with(
        Arc::new(registry),
        Arc::new(ManualClock::new()),
        Arc::new(MemorySnapshotStore::default()),
        DaemonConfig::default(),
    )
    .await;
    let service = ControlService::new(engine.clone());
    let running_engine = engine.clone();
    let running = tokio::spawn(async move { running_engine.run().await });

    let added = service
        .handle(ControlRequest::new(
            "add",
            ControlCommand::AddAccount {
                provider: ProviderId::new("managed"),
                label: Some("work".into()),
            },
        ))
        .await;
    let account_id = match added.result {
        ControlResult::Account(account) => {
            assert_eq!(account.provider, ProviderId::new("managed"));
            assert_eq!(account.label.as_deref(), Some("work"));
            assert!(account.enabled);
            account.id
        }
        other => panic!("unexpected add result: {other:?}"),
    };
    provider.wait_for_calls(1).await;
    tokio::time::timeout(Duration::from_secs(2), async {
        while engine.status().await.accounts[0].in_flight {
            tokio::task::yield_now().await;
        }
    })
    .await
    .unwrap();

    let duplicate = service
        .handle(ControlRequest::new(
            "duplicate",
            ControlCommand::AddAccount {
                provider: ProviderId::new("managed"),
                label: Some("work".into()),
            },
        ))
        .await;
    assert!(matches!(
        duplicate.result,
        ControlResult::Error(ullage_protocol::ControlError::Account(
            AccountError::Duplicate(_)
        ))
    ));

    let listed = service
        .handle(ControlRequest::new("list", ControlCommand::ListAccounts))
        .await;
    assert!(matches!(
        listed.result,
        ControlResult::Accounts(ref accounts)
            if accounts.len() == 1 && accounts[0].id == account_id
    ));

    let disabled = service
        .handle(ControlRequest::new(
            "disable",
            ControlCommand::SetAccountEnabled {
                account: account_id.clone(),
                enabled: false,
            },
        ))
        .await;
    assert!(matches!(
        disabled.result,
        ControlResult::Account(ref account) if !account.enabled
    ));
    assert!(!engine.status().await.accounts[0].enabled);

    let enabled = service
        .handle(ControlRequest::new(
            "enable",
            ControlCommand::SetAccountEnabled {
                account: account_id.clone(),
                enabled: true,
            },
        ))
        .await;
    assert!(matches!(
        enabled.result,
        ControlResult::Account(ref account) if account.enabled
    ));
    provider.wait_for_calls(2).await;

    let shown = service
        .handle(ControlRequest::new(
            "show",
            ControlCommand::ShowAccount {
                account: account_id.clone(),
            },
        ))
        .await;
    assert!(matches!(shown.result, ControlResult::Account(_)));

    let removed = service
        .handle(ControlRequest::new(
            "remove",
            ControlCommand::RemoveAccount {
                account: account_id.clone(),
            },
        ))
        .await;
    assert!(matches!(removed.result, ControlResult::Ack));
    let missing = service
        .handle(ControlRequest::new(
            "show-missing",
            ControlCommand::ShowAccount {
                account: account_id.clone(),
            },
        ))
        .await;
    assert!(matches!(
        missing.result,
        ControlResult::Error(ullage_protocol::ControlError::Account(AccountError::NotFound(
            ref missing
        ))) if missing == &account_id
    ));
    assert!(engine.status().await.accounts.is_empty());
    assert!(engine.show_all().await.is_empty());
    engine.shutdown();
    running.await.unwrap();
}

#[tokio::test(flavor = "multi_thread")]
async fn cancelled_remove_caller_cannot_leave_account_and_snapshots_half_committed() {
    let store = Arc::new(CommitGateStore::default());
    let account_id = AccountId::new("removed");
    let mut persisted = PersistedState::default();
    persisted.snapshots.insert(
        account_id.clone(),
        SnapshotRecord {
            account_id: account_id.clone(),
            usage: QueryOutcome::Complete {
                data: usage(&ProviderId::new("removed"), Some("removed")),
            },
            last_success_at: Utc::now(),
            stale: false,
            last_error: None,
            last_error_at: None,
        },
    );
    *store.inner.state.write().await = persisted;
    let provider = MockProvider::new("removed", []);
    let mut registry = ProviderRegistry::default();
    registry.register(provider).unwrap();
    let engine = engine_with(
        Arc::new(registry),
        Arc::new(ManualClock::new()),
        store.clone(),
        DaemonConfig::default(),
    )
    .await;
    engine
        .add_account(account("removed", "removed", Duration::from_secs(60)))
        .await
        .unwrap();
    store.arm();
    assert!(engine.show(&account_id).await.is_some());

    let remove_engine = engine.clone();
    let remove_id = account_id.clone();
    let removal = tokio::spawn(async move { remove_engine.remove_account(&remove_id).await });
    store.wait_for_commit().await;
    removal.abort();
    let _ = removal.await;
    store.finish_commit();

    tokio::time::timeout(Duration::from_secs(2), async {
        loop {
            let status = engine.status().await;
            if status.accounts.is_empty() && engine.show_all().await.is_empty() {
                break;
            }
            tokio::task::yield_now().await;
        }
    })
    .await
    .unwrap();
    assert!(store.state().await.snapshots.is_empty());
}

#[tokio::test(flavor = "multi_thread")]
async fn show_waits_for_concurrent_remove_and_returns_not_found_after_commit() {
    let store = Arc::new(CommitGateStore::default());
    let account_id = AccountId::new("show-remove");
    let mut persisted = PersistedState::default();
    persisted.snapshots.insert(
        account_id.clone(),
        SnapshotRecord {
            account_id: account_id.clone(),
            usage: QueryOutcome::Complete {
                data: usage(&ProviderId::new("show-remove"), Some("show-remove")),
            },
            last_success_at: Utc::now(),
            stale: false,
            last_error: None,
            last_error_at: None,
        },
    );
    *store.inner.state.write().await = persisted;
    let mut registry = ProviderRegistry::default();
    registry
        .register(MockProvider::new("show-remove", []))
        .unwrap();
    let engine = engine_with(
        Arc::new(registry),
        Arc::new(ManualClock::new()),
        store.clone(),
        DaemonConfig::default(),
    )
    .await;
    engine
        .add_account(account(
            "show-remove",
            "show-remove",
            Duration::from_secs(60),
        ))
        .await
        .unwrap();
    store.arm();
    let service = ControlService::new(engine);
    let removing = tokio::spawn({
        let service = service.clone();
        let account = ullage_protocol::AccountId::new("show-remove");
        async move {
            service
                .handle(ControlRequest::new(
                    "remove-race",
                    ControlCommand::RemoveAccount { account },
                ))
                .await
        }
    });
    store.wait_for_commit().await;
    let showing = tokio::spawn({
        let service = service.clone();
        async move {
            service
                .handle(ControlRequest::new(
                    "show-race",
                    ControlCommand::Show {
                        account_id: Some("show-remove".into()),
                    },
                ))
                .await
        }
    });
    tokio::task::yield_now().await;
    assert!(!showing.is_finished());

    store.finish_commit();
    assert!(matches!(removing.await.unwrap().result, ControlResult::Ack));
    assert!(matches!(
        showing.await.unwrap().result,
        ControlResult::Error(ullage_protocol::ControlError::AccountNotFound { ref account_id })
            if account_id == "show-remove"
    ));
}

#[tokio::test(flavor = "multi_thread")]
async fn shutdown_cancels_hanging_remove_staging_and_restores_the_account() {
    let store = Arc::new(HangingStore::default());
    let mut registry = ProviderRegistry::default();
    registry
        .register(MockProvider::new("remove-hang", []))
        .unwrap();
    let engine = engine_with(
        Arc::new(registry),
        Arc::new(ManualClock::new()),
        store.clone(),
        DaemonConfig::default(),
    )
    .await;
    let mut config = account("remove-hang", "remove-hang", Duration::from_secs(60));
    config.enabled = false;
    engine.add_account(config).await.unwrap();
    store.arm();
    let running_engine = engine.clone();
    let running = tokio::spawn(async move { running_engine.run().await });
    let remove_engine = engine.clone();
    let removal = tokio::spawn(async move {
        remove_engine
            .remove_account(&AccountId::new("remove-hang"))
            .await
    });
    store.wait_for_stage().await;

    engine.shutdown();
    tokio::time::timeout(Duration::from_secs(2), running)
        .await
        .unwrap()
        .unwrap();
    assert!(matches!(
        removal.await.unwrap(),
        Err(DaemonError::Cancelled)
    ));
    assert_eq!(engine.status().await.accounts.len(), 1);
}

#[tokio::test(flavor = "multi_thread")]
async fn shutdown_after_remove_staging_prevents_commit_and_restores_the_account() {
    let store = Arc::new(ShutdownOnStageStore::default());
    let mut registry = ProviderRegistry::default();
    registry
        .register(MockProvider::new("remove-stage-race", []))
        .unwrap();
    let engine = engine_with(
        Arc::new(registry),
        Arc::new(ManualClock::new()),
        store.clone(),
        DaemonConfig::default(),
    )
    .await;
    let mut config = account(
        "remove-stage-race",
        "remove-stage-race",
        Duration::from_secs(60),
    );
    config.enabled = false;
    engine.add_account(config).await.unwrap();
    *store.engine.lock().unwrap() = Some(engine.clone());

    assert!(matches!(
        engine
            .remove_account(&AccountId::new("remove-stage-race"))
            .await,
        Err(DaemonError::Cancelled)
    ));
    assert_eq!(engine.status().await.accounts.len(), 1);
}

#[tokio::test(flavor = "multi_thread")]
async fn panicking_remove_staging_rolls_back_account_and_snapshot() {
    let account_id = AccountId::new("remove-panic");
    let snapshot = SnapshotRecord {
        account_id: account_id.clone(),
        usage: QueryOutcome::Complete {
            data: usage(&ProviderId::new("remove-panic"), Some("remove-panic")),
        },
        last_success_at: Utc::now(),
        stale: false,
        last_error: None,
        last_error_at: None,
    };
    let mut persisted = PersistedState::default();
    persisted
        .snapshots
        .insert(account_id.clone(), snapshot.clone());
    let mut registry = ProviderRegistry::default();
    registry
        .register(MockProvider::new("remove-panic", []))
        .unwrap();
    let store = Arc::new(PanickingStageStore {
        state: persisted,
        armed: AtomicBool::new(false),
    });
    let engine = engine_with(
        Arc::new(registry),
        Arc::new(ManualClock::new()),
        store.clone(),
        DaemonConfig::default(),
    )
    .await;
    engine
        .add_account(account(
            "remove-panic",
            "remove-panic",
            Duration::from_secs(60),
        ))
        .await
        .unwrap();
    store.armed.store(true, Ordering::SeqCst);

    assert!(matches!(
        engine.remove_account(&account_id).await,
        Err(DaemonError::Storage(_))
    ));
    assert_eq!(engine.status().await.accounts.len(), 1);
    assert_eq!(engine.show(&account_id).await, Some(snapshot));
    assert!(!engine.is_shutting_down());
}

#[tokio::test]
async fn account_configuration_and_id_sequence_survive_restart() {
    let store = Arc::new(MemorySnapshotStore::default());
    let mut first_registry = ProviderRegistry::default();
    first_registry
        .register(MockProvider::new("restart", []))
        .unwrap();
    let first = engine_with(
        Arc::new(first_registry),
        Arc::new(ManualClock::new()),
        store.clone(),
        DaemonConfig::default(),
    )
    .await;
    first
        .add_account(account("account-1", "restart", Duration::from_secs(60)))
        .await
        .unwrap();
    first
        .set_account_enabled(&AccountId::new("account-1"), false)
        .await
        .unwrap()
        .unwrap();
    drop(first);

    let mut second_registry = ProviderRegistry::default();
    second_registry
        .register(MockProvider::new("restart", []))
        .unwrap();
    let second = engine_with(
        Arc::new(second_registry),
        Arc::new(ManualClock::new()),
        store,
        DaemonConfig::default(),
    )
    .await;

    let restored = second.account_configs().await;
    assert_eq!(restored.len(), 1);
    assert_eq!(restored[0].id, AccountId::new("account-1"));
    assert!(!restored[0].enabled);
    assert_eq!(second.next_account_id().await, AccountId::new("account-2"));
}

#[tokio::test(flavor = "multi_thread")]
async fn failed_enable_cannot_roll_back_a_later_successful_disable() {
    let store = Arc::new(MutationRaceStore::default());
    let mut registry = ProviderRegistry::default();
    registry
        .register(MockProvider::new("mutation-race", []))
        .unwrap();
    let engine = engine_with(
        Arc::new(registry),
        Arc::new(ManualClock::new()),
        store.clone(),
        DaemonConfig::default(),
    )
    .await;
    let account_id = AccountId::new("mutation-race");
    engine
        .add_account(account(
            "mutation-race",
            "mutation-race",
            Duration::from_secs(60),
        ))
        .await
        .unwrap();
    store.fail_next_stage();
    let enabling = tokio::spawn({
        let engine = engine.clone();
        let account_id = account_id.clone();
        async move { engine.set_account_enabled(&account_id, true).await }
    });
    tokio::time::timeout(
        Duration::from_secs(1),
        store.inner.failed_stage_started.notified(),
    )
    .await
    .unwrap();
    let disabling = tokio::spawn({
        let engine = engine.clone();
        let account_id = account_id.clone();
        async move { engine.set_account_enabled(&account_id, false).await }
    });
    tokio::task::yield_now().await;
    assert!(!disabling.is_finished());

    store.inner.release_failed_stage.add_permits(1);
    assert!(matches!(
        enabling.await.unwrap(),
        Err(DaemonError::Storage(_))
    ));
    let disabled = disabling.await.unwrap().unwrap().unwrap();
    assert!(!disabled.enabled);
    assert!(!engine.account_config(&account_id).await.unwrap().enabled);
    assert!(
        !store
            .inner
            .state
            .read()
            .await
            .accounts
            .get(&account_id)
            .unwrap()
            .enabled
    );
}

#[tokio::test(flavor = "multi_thread")]
async fn failed_disable_wakes_the_scheduler_after_enabled_state_is_restored() {
    let store = Arc::new(MutationRaceStore::default());
    let provider = MockProvider::new("disable-rollback", []);
    let mut registry = ProviderRegistry::default();
    registry.register(provider.clone()).unwrap();
    let clock = Arc::new(ManualClock::new());
    let engine = engine_with(
        Arc::new(registry),
        clock.clone(),
        store.clone(),
        DaemonConfig::default(),
    )
    .await;
    let account_id = AccountId::new("disable-rollback");
    engine
        .add_account(account(
            "disable-rollback",
            "disable-rollback",
            Duration::from_secs(60),
        ))
        .await
        .unwrap();
    let running = tokio::spawn({
        let engine = engine.clone();
        async move { engine.run().await }
    });
    provider.wait_for_calls(1).await;
    tokio::time::timeout(Duration::from_secs(1), async {
        loop {
            if engine.status().await.accounts[0].next_probe_at.is_some() {
                break;
            }
            tokio::task::yield_now().await;
        }
    })
    .await
    .unwrap();
    clock.wait_for_sleep(Duration::from_secs(60)).await;

    store.fail_next_stage();
    let disabling = tokio::spawn({
        let engine = engine.clone();
        let account_id = account_id.clone();
        async move { engine.set_account_enabled(&account_id, false).await }
    });
    tokio::time::timeout(
        Duration::from_secs(1),
        store.inner.failed_stage_started.notified(),
    )
    .await
    .unwrap();
    clock.sleep_durations.lock().unwrap().clear();
    clock.advance(Duration::from_secs(60));
    for _ in 0..10 {
        tokio::task::yield_now().await;
    }

    store.inner.release_failed_stage.add_permits(1);
    assert!(matches!(
        disabling.await.unwrap(),
        Err(DaemonError::Storage(_))
    ));
    assert!(engine.account_config(&account_id).await.unwrap().enabled);
    clock.wait_for_sleep(Duration::from_secs(60)).await;
    clock.advance(Duration::from_secs(60));
    provider.wait_for_calls(2).await;

    engine.shutdown();
    running.await.unwrap();
}

#[tokio::test]
async fn legacy_orphan_snapshot_ids_are_not_reused() {
    let store = Arc::new(MemorySnapshotStore::default());
    let account_id = AccountId::new("account-7");
    let mut state = PersistedState::default();
    state.snapshots.insert(
        account_id.clone(),
        SnapshotRecord {
            account_id,
            usage: QueryOutcome::Complete {
                data: usage(&ProviderId::new("legacy"), Some("old")),
            },
            last_success_at: Utc::now(),
            stale: false,
            last_error: None,
            last_error_at: None,
        },
    );
    ullage_daemon::SnapshotStore::stage(store.as_ref(), &state)
        .await
        .unwrap()
        .commit()
        .await
        .unwrap();
    let engine = engine_with(
        Arc::new(ProviderRegistry::default()),
        Arc::new(ManualClock::new()),
        store,
        DaemonConfig::default(),
    )
    .await;

    assert_eq!(engine.next_account_id().await, AccountId::new("account-8"));
}

#[tokio::test(flavor = "multi_thread")]
async fn disabling_an_in_flight_account_clears_its_periodic_schedule() {
    let (provider, gate) = MockProvider::gated("disable-race");
    let mut registry = ProviderRegistry::default();
    registry.register(provider.clone()).unwrap();
    let clock = Arc::new(ManualClock::new());
    let engine = engine_with(
        Arc::new(registry),
        clock.clone(),
        Arc::new(MemorySnapshotStore::default()),
        DaemonConfig::default(),
    )
    .await;
    engine
        .add_account(account(
            "disable-race",
            "disable-race",
            Duration::from_secs(60),
        ))
        .await
        .unwrap();
    let running_engine = engine.clone();
    let running = tokio::spawn(async move { running_engine.run().await });
    provider.wait_for_calls(1).await;

    engine
        .set_account_enabled(&AccountId::new("disable-race"), false)
        .await
        .unwrap()
        .unwrap();
    gate.add_permits(1);
    tokio::time::timeout(Duration::from_secs(2), async {
        while engine.status().await.accounts[0].in_flight {
            tokio::task::yield_now().await;
        }
    })
    .await
    .unwrap();
    let status = engine.status().await;
    assert!(!status.accounts[0].enabled);
    assert_eq!(status.accounts[0].next_probe_at, None);

    clock.advance(Duration::from_secs(600));
    for _ in 0..20 {
        tokio::task::yield_now().await;
    }
    assert_eq!(provider.calls(), 1);
    engine.shutdown();
    running.await.unwrap();
}

#[tokio::test(flavor = "multi_thread")]
async fn timeout_is_reported_without_waiting_for_provider_completion() {
    let (provider, _gate) = MockProvider::gated("timeout");
    let mut registry = ProviderRegistry::default();
    registry.register(provider).unwrap();
    let clock = Arc::new(ManualClock::new());
    let engine = engine_with(
        Arc::new(registry),
        clock.clone(),
        Arc::new(MemorySnapshotStore::default()),
        DaemonConfig::default(),
    )
    .await;
    let mut config = account("timeout", "timeout", Duration::from_secs(60));
    config.timeout = Duration::from_millis(10);
    engine.add_account(config).await.unwrap();
    let probing = tokio::spawn({
        let engine = engine.clone();
        async move {
            engine
                .probe(&AccountId::new("timeout"), ProbeTrigger::Manual)
                .await
        }
    });
    clock.wait_for_sleep(Duration::from_millis(10)).await;
    clock.advance(Duration::from_millis(10));
    assert_eq!(probing.await.unwrap(), Err(ProbeError::Timeout));
}

#[tokio::test(flavor = "multi_thread")]
async fn timeout_includes_waiting_for_provider_capacity() {
    let (provider, gate) = MockProvider::gated("capacity-timeout");
    let mut registry = ProviderRegistry::default();
    registry.register(provider.clone()).unwrap();
    let clock = Arc::new(ManualClock::new());
    let engine = engine_with(
        Arc::new(registry),
        clock.clone(),
        Arc::new(MemorySnapshotStore::default()),
        DaemonConfig {
            maximum_concurrency: 2,
            default_provider_concurrency: 1,
            provider_limits: Vec::new(),
        },
    )
    .await;
    let mut occupying = account(
        "capacity-occupying",
        "capacity-timeout",
        Duration::from_secs(60),
    );
    occupying.timeout = Duration::from_secs(60);
    engine.add_account(occupying).await.unwrap();
    let mut waiting = account(
        "capacity-waiting",
        "capacity-timeout",
        Duration::from_secs(60),
    );
    waiting.timeout = Duration::from_secs(5);
    engine.add_account(waiting).await.unwrap();

    let first = tokio::spawn({
        let engine = engine.clone();
        async move {
            engine
                .probe(&AccountId::new("capacity-occupying"), ProbeTrigger::Manual)
                .await
        }
    });
    provider.wait_for_calls(1).await;
    let second = tokio::spawn({
        let engine = engine.clone();
        async move {
            engine
                .probe(&AccountId::new("capacity-waiting"), ProbeTrigger::Manual)
                .await
        }
    });
    clock.wait_for_sleep(Duration::from_secs(5)).await;
    clock.advance(Duration::from_secs(5));
    assert_eq!(second.await.unwrap(), Err(ProbeError::Timeout));
    assert_eq!(provider.calls(), 1);

    gate.add_permits(1);
    first.await.unwrap().unwrap();
}

#[tokio::test(flavor = "multi_thread")]
async fn disabled_manual_probe_does_not_publish_a_periodic_deadline() {
    let provider = MockProvider::new("disabled", []);
    let mut registry = ProviderRegistry::default();
    registry.register(provider).unwrap();
    let engine = engine_with(
        Arc::new(registry),
        Arc::new(ManualClock::new()),
        Arc::new(MemorySnapshotStore::default()),
        DaemonConfig::default(),
    )
    .await;
    let mut disabled = account("disabled", "disabled", Duration::from_secs(60));
    disabled.enabled = false;
    engine.add_account(disabled).await.unwrap();

    engine
        .probe(&AccountId::new("disabled"), ProbeTrigger::Manual)
        .await
        .unwrap();
    assert_eq!(engine.status().await.accounts[0].next_probe_at, None);
}

#[tokio::test(flavor = "multi_thread")]
async fn cancelling_the_leader_does_not_orphan_single_flight_followers() {
    let (provider, _gate) = MockProvider::gated("cancelled-leader");
    let mut registry = ProviderRegistry::default();
    registry.register(provider.clone()).unwrap();
    let engine = engine_with(
        Arc::new(registry),
        Arc::new(ManualClock::new()),
        Arc::new(MemorySnapshotStore::default()),
        DaemonConfig::default(),
    )
    .await;
    engine
        .add_account(account(
            "cancelled-leader",
            "cancelled-leader",
            Duration::from_secs(60),
        ))
        .await
        .unwrap();

    let leader = tokio::spawn({
        let engine = engine.clone();
        async move {
            engine
                .probe(&AccountId::new("cancelled-leader"), ProbeTrigger::Manual)
                .await
        }
    });
    provider.wait_for_calls(1).await;
    leader.abort();
    let follower = tokio::spawn({
        let engine = engine.clone();
        async move {
            engine
                .probe(&AccountId::new("cancelled-leader"), ProbeTrigger::Periodic)
                .await
        }
    });
    tokio::task::yield_now().await;
    assert_eq!(provider.calls(), 1);
    engine.shutdown();
    assert_eq!(follower.await.unwrap(), Err(ProbeError::Cancelled));
    tokio::time::timeout(Duration::from_secs(1), async {
        while engine.status().await.accounts[0].in_flight {
            tokio::task::yield_now().await;
        }
    })
    .await
    .unwrap();
}

#[tokio::test(flavor = "multi_thread")]
async fn shutdown_closes_flight_admission_before_idle_can_be_observed() {
    let provider = MockProvider::new("closed", []);
    let mut registry = ProviderRegistry::default();
    registry.register(provider.clone()).unwrap();
    let engine = engine_with(
        Arc::new(registry),
        Arc::new(ManualClock::new()),
        Arc::new(MemorySnapshotStore::default()),
        DaemonConfig::default(),
    )
    .await;
    engine
        .add_account(account("closed", "closed", Duration::from_secs(60)))
        .await
        .unwrap();

    engine.shutdown();
    assert_eq!(
        engine
            .probe(&AccountId::new("closed"), ProbeTrigger::Manual)
            .await,
        Err(ProbeError::Cancelled)
    );
    assert_eq!(provider.calls(), 0);
    assert!(!engine.status().await.accounts[0].in_flight);
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn followers_keep_their_result_when_the_next_flight_completes() {
    let provider_id = ProviderId::new("flight-generation");
    let mut first_usage = usage(&provider_id, Some("flight-generation"));
    first_usage.plan = Some("first-flight".into());
    let mut second_usage = first_usage.clone();
    second_usage.plan = Some("second-flight".into());
    let (provider, gate) = MockProvider::gated_with_results(
        "flight-generation",
        [
            Ok(QueryOutcome::Complete { data: first_usage }),
            Ok(QueryOutcome::Complete { data: second_usage }),
        ],
    );
    let mut registry = ProviderRegistry::default();
    registry.register(provider.clone()).unwrap();
    let engine = engine_with(
        Arc::new(registry),
        Arc::new(ManualClock::new()),
        Arc::new(MemorySnapshotStore::default()),
        DaemonConfig::default(),
    )
    .await;
    engine
        .add_account(account(
            "flight-generation",
            "flight-generation",
            Duration::from_secs(60),
        ))
        .await
        .unwrap();

    let leader = tokio::spawn({
        let engine = engine.clone();
        async move {
            engine
                .probe(&AccountId::new("flight-generation"), ProbeTrigger::Manual)
                .await
        }
    });
    provider.wait_for_calls(1).await;
    let mut followers = Vec::new();
    for _ in 0..64 {
        let engine = engine.clone();
        followers.push(Box::pin(tokio::task::unconstrained(async move {
            engine
                .probe(&AccountId::new("flight-generation"), ProbeTrigger::Periodic)
                .await
        })));
    }
    // Poll each follower through the immediately-ready engine locks and into
    // the first flight's completion wait before releasing that flight.
    for follower in &mut followers {
        poll_fn(|context| match follower.as_mut().poll(context) {
            Poll::Pending => Poll::Ready(()),
            Poll::Ready(_) => panic!("gated follower completed unexpectedly"),
        })
        .await;
    }
    assert_eq!(provider.calls(), 1);

    gate.add_permits(1);
    let first = leader.await.unwrap().unwrap();
    assert_eq!(outcome_plan(&first), Some("first-flight"));
    let next = tokio::spawn({
        let engine = engine.clone();
        async move {
            engine
                .probe(&AccountId::new("flight-generation"), ProbeTrigger::Manual)
                .await
        }
    });
    provider.wait_for_calls(2).await;
    // Extra permits keep a follower that unexpectedly starts a later flight
    // from blocking forever; its result assertion still exposes that flight.
    gate.add_permits(followers.len() + 1);
    assert_eq!(
        outcome_plan(&next.await.unwrap().unwrap()),
        Some("second-flight")
    );
    for follower in followers {
        assert_eq!(outcome_plan(&follower.await.unwrap()), Some("first-flight"));
    }
}

#[tokio::test(flavor = "multi_thread")]
async fn provider_panic_completes_all_waiters_and_allows_shutdown() {
    let (provider, gate) = MockProvider::panicking_gated("provider-panic");
    let mut registry = ProviderRegistry::default();
    registry.register(provider.clone()).unwrap();
    let engine = engine_with(
        Arc::new(registry),
        Arc::new(ManualClock::new()),
        Arc::new(MemorySnapshotStore::default()),
        DaemonConfig::default(),
    )
    .await;
    engine
        .add_account(account(
            "provider-panic",
            "provider-panic",
            Duration::from_secs(60),
        ))
        .await
        .unwrap();

    let first = tokio::spawn({
        let engine = engine.clone();
        async move {
            engine
                .probe(&AccountId::new("provider-panic"), ProbeTrigger::Manual)
                .await
        }
    });
    provider.wait_for_calls(1).await;
    let follower = tokio::spawn({
        let engine = engine.clone();
        async move {
            engine
                .probe(&AccountId::new("provider-panic"), ProbeTrigger::Periodic)
                .await
        }
    });
    tokio::task::yield_now().await;
    // Both permits preserve bounded completion even if the follower was
    // descheduled before it could attach to the first flight.
    gate.add_permits(2);

    let expected = Err(ProbeError::Provider(ProviderError::ProtocolIncompatible {
        message: "provider protocol response is incompatible".into(),
    }));
    assert_eq!(first.await.unwrap(), expected);
    assert_eq!(follower.await.unwrap(), expected);
    assert!(!engine.status().await.accounts[0].in_flight);
    engine.shutdown();
    tokio::time::timeout(Duration::from_secs(1), engine.run())
        .await
        .unwrap();
}

#[tokio::test(flavor = "multi_thread")]
async fn storage_panic_clears_the_flight_and_allows_shutdown() {
    let provider = MockProvider::new("storage-panic", []);
    let mut registry = ProviderRegistry::default();
    registry.register(provider).unwrap();
    let store = Arc::new(PanickingStore::default());
    let engine = engine_with(
        Arc::new(registry),
        Arc::new(ManualClock::new()),
        store.clone(),
        DaemonConfig::default(),
    )
    .await;
    engine
        .add_account(account(
            "storage-panic",
            "storage-panic",
            Duration::from_secs(60),
        ))
        .await
        .unwrap();
    store.0.store(true, Ordering::SeqCst);

    assert_eq!(
        engine
            .probe(&AccountId::new("storage-panic"), ProbeTrigger::Manual)
            .await,
        Err(ProbeError::Storage("snapshot storage task failed".into()))
    );
    let status = engine.status().await;
    assert!(!status.accounts[0].in_flight);
    assert_eq!(status.accounts[0].last_error, None);
    engine.shutdown();
    tokio::time::timeout(Duration::from_secs(1), engine.run())
        .await
        .unwrap();
}

#[tokio::test(flavor = "multi_thread")]
async fn storage_load_panic_becomes_a_sanitized_startup_error() {
    let error = match DaemonEngine::new(
        DaemonConfig::default(),
        Arc::new(ProviderRegistry::default()),
        Arc::new(ManualClock::new()),
        Arc::new(PanickingLoadStore),
    )
    .await
    {
        Ok(_) => panic!("panicking load unexpectedly succeeded"),
        Err(error) => error,
    };
    assert_eq!(
        error.to_string(),
        "snapshot storage failed: snapshot load task failed"
    );
}

#[tokio::test(flavor = "multi_thread")]
async fn maximum_interval_clamps_the_deadline_without_orphaning_waiters() {
    let provider = MockProvider::new("maximum-interval", []);
    let mut registry = ProviderRegistry::default();
    registry.register(provider).unwrap();
    let engine = engine_with(
        Arc::new(registry),
        Arc::new(ManualClock::new()),
        Arc::new(MemorySnapshotStore::default()),
        DaemonConfig::default(),
    )
    .await;
    engine
        .add_account(account(
            "maximum-interval",
            "maximum-interval",
            Duration::MAX,
        ))
        .await
        .unwrap();

    engine
        .probe(&AccountId::new("maximum-interval"), ProbeTrigger::Manual)
        .await
        .unwrap();
    let status = engine.status().await;
    assert!(!status.accounts[0].in_flight);
    assert_eq!(
        status.accounts[0].next_probe_at,
        Some(DateTime::<Utc>::MAX_UTC)
    );
}

#[tokio::test(flavor = "multi_thread")]
async fn shutdown_cancels_a_hanging_store_and_completes_followers() {
    let provider = MockProvider::new("hanging-store", []);
    let mut registry = ProviderRegistry::default();
    registry.register(provider).unwrap();
    let store = Arc::new(HangingStore::default());
    let engine = engine_with(
        Arc::new(registry),
        Arc::new(ManualClock::new()),
        store.clone(),
        DaemonConfig::default(),
    )
    .await;
    engine
        .add_account(account(
            "hanging-store",
            "hanging-store",
            Duration::from_secs(60),
        ))
        .await
        .unwrap();
    store.arm();

    let leader = tokio::spawn({
        let engine = engine.clone();
        async move {
            engine
                .probe(&AccountId::new("hanging-store"), ProbeTrigger::Manual)
                .await
        }
    });
    store.wait_for_stage().await;
    assert!(
        engine
            .show(&AccountId::new("hanging-store"))
            .await
            .is_none()
    );
    let follower = tokio::spawn({
        let engine = engine.clone();
        async move {
            engine
                .probe(&AccountId::new("hanging-store"), ProbeTrigger::Periodic)
                .await
        }
    });
    tokio::task::yield_now().await;
    engine.shutdown();

    assert_eq!(leader.await.unwrap(), Err(ProbeError::Cancelled));
    assert_eq!(follower.await.unwrap(), Err(ProbeError::Cancelled));
    assert!(!engine.status().await.accounts[0].in_flight);
    assert!(
        engine
            .show(&AccountId::new("hanging-store"))
            .await
            .is_none()
    );
    tokio::time::timeout(Duration::from_secs(1), engine.run())
        .await
        .unwrap();
}

#[tokio::test(flavor = "multi_thread")]
async fn shutdown_waits_for_a_started_commit_before_publishing_it() {
    let provider = MockProvider::new("commit-gate", []);
    let mut registry = ProviderRegistry::default();
    registry.register(provider).unwrap();
    let store = Arc::new(CommitGateStore::default());
    let engine = engine_with(
        Arc::new(registry),
        Arc::new(ManualClock::new()),
        store.clone(),
        DaemonConfig::default(),
    )
    .await;
    engine
        .add_account(account(
            "commit-gate",
            "commit-gate",
            Duration::from_secs(60),
        ))
        .await
        .unwrap();
    store.arm();

    let probing = tokio::spawn({
        let engine = engine.clone();
        async move {
            engine
                .probe(&AccountId::new("commit-gate"), ProbeTrigger::Manual)
                .await
        }
    });
    store.wait_for_commit().await;
    engine.shutdown();
    tokio::task::yield_now().await;
    assert!(!probing.is_finished());
    assert!(engine.show(&AccountId::new("commit-gate")).await.is_none());

    store.finish_commit();
    probing.await.unwrap().unwrap();
    assert!(engine.show(&AccountId::new("commit-gate")).await.is_some());
    assert!(
        store
            .state()
            .await
            .snapshots
            .contains_key(&AccountId::new("commit-gate"))
    );
    tokio::time::timeout(Duration::from_secs(1), engine.run())
        .await
        .unwrap();
}

#[cfg(unix)]
async fn send_control_request(
    socket: &std::path::Path,
    request: ControlRequest,
) -> ullage_protocol::ControlResponse {
    let mut encoded = serde_json::to_vec(&request).unwrap();
    encoded.push(b'\n');
    send_raw_control_request(socket, &encoded).await
}

#[cfg(unix)]
async fn send_raw_control_request(
    socket: &std::path::Path,
    encoded: &[u8],
) -> ullage_protocol::ControlResponse {
    use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader};

    let mut stream = tokio::net::UnixStream::connect(socket).await.unwrap();
    stream.write_all(encoded).await.unwrap();
    let mut response = String::new();
    BufReader::new(stream)
        .read_line(&mut response)
        .await
        .unwrap();
    serde_json::from_str(&response).unwrap()
}

#[cfg(unix)]
#[tokio::test(flavor = "multi_thread")]
async fn unix_control_socket_is_private_framed_and_cleaned_up() {
    use std::os::unix::fs::PermissionsExt;

    static NEXT_SOCKET: AtomicUsize = AtomicUsize::new(0);
    let directory = std::env::temp_dir().join(format!(
        "ullage-control-{}-{}",
        std::process::id(),
        NEXT_SOCKET.fetch_add(1, Ordering::SeqCst)
    ));
    let socket = directory.join("daemon.sock");
    let provider = MockProvider::new("socket", []);
    let mut registry = ProviderRegistry::default();
    registry.register(provider).unwrap();
    let registry = Arc::new(registry);
    let engine = engine_with(
        registry.clone(),
        Arc::new(ManualClock::new()),
        Arc::new(MemorySnapshotStore::default()),
        DaemonConfig::default(),
    )
    .await;
    engine
        .add_account(account("socket", "socket", Duration::from_secs(60)))
        .await
        .unwrap();
    let service = ControlService::new(engine.clone());
    let server = UnixControlServer::bind(&socket, service.clone())
        .await
        .unwrap();
    assert_eq!(
        std::fs::metadata(&directory).unwrap().permissions().mode() & 0o777,
        0o700
    );
    assert_eq!(
        std::fs::metadata(&socket).unwrap().permissions().mode() & 0o777,
        0o600
    );
    let running = tokio::spawn(server.run());

    let response = send_control_request(
        &socket,
        ControlRequest::new("socket-1", ControlCommand::DaemonStatus),
    )
    .await;
    assert!(matches!(
        response.result,
        ControlResult::DaemonStatus(ref status) if status.accounts.len() == 1
    ));
    let response = send_control_request(
        &socket,
        ControlRequest::new(
            "socket-2",
            ControlCommand::Probe {
                account_id: "socket".into(),
                wait: true,
            },
        ),
    )
    .await;
    assert!(matches!(
        response.result,
        ControlResult::Probe(ref payload) if payload.account_id == "socket"
    ));
    let response = send_control_request(
        &socket,
        ControlRequest::new(
            "socket-3",
            ControlCommand::Show {
                account_id: Some("socket".into()),
            },
        ),
    )
    .await;
    assert!(matches!(
        response.result,
        ControlResult::Snapshots(ref snapshots) if snapshots.len() == 1
    ));
    let response = send_control_request(
        &socket,
        ControlRequest::new(
            "socket-4",
            ControlCommand::StartAuth {
                provider: ProviderId::new("socket"),
                account: ullage_protocol::AccountId::new("socket"),
                request: AuthStartRequest {
                    method: None,
                    redirect_uri: None,
                },
            },
        ),
    )
    .await;
    assert!(matches!(
        response.result,
        ControlResult::AuthChallenge(AuthChallenge { ref verification_uri, .. })
            if verification_uri.as_deref() == Some("https://example.invalid/device")
    ));
    let response = send_control_request(
        &socket,
        ControlRequest::new("socket-5", ControlCommand::ListProviders),
    )
    .await;
    assert!(matches!(response.result, ControlResult::Providers(_)));
    let future_request = format!(
        "{{\"version\":{},\"request_id\":\"future\",\"command\":{{\"command\":\"future_command\"}}}}\n",
        CONTROL_PROTOCOL_VERSION + 1
    );
    let mismatch = send_raw_control_request(&socket, future_request.as_bytes()).await;
    assert!(matches!(
        mismatch.result,
        ControlResult::ProtocolMismatch {
            supported_version
        } if supported_version == CONTROL_PROTOCOL_VERSION
    ));

    let active_path = directory.join("active.sock");
    let active_listener = std::os::unix::net::UnixListener::bind(&active_path).unwrap();
    let active_error = match UnixControlServer::bind(&active_path, service.clone()).await {
        Ok(_) => panic!("active socket was unexpectedly replaced"),
        Err(error) => error,
    };
    assert!(active_error.contains("already active"));
    drop(active_listener);
    std::fs::remove_file(&active_path).unwrap();

    let stale_path = directory.join("stale.sock");
    drop(std::os::unix::net::UnixListener::bind(&stale_path).unwrap());
    let recovered = UnixControlServer::bind(&stale_path, service).await.unwrap();
    let recovered_running = tokio::spawn(recovered.run());

    engine.shutdown();
    running.await.unwrap().unwrap();
    recovered_running.await.unwrap().unwrap();
    assert!(!socket.exists());
    assert!(!stale_path.exists());
    std::fs::remove_dir(directory).unwrap();
}
