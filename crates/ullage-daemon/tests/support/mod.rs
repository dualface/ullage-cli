//! Helpers shared by the daemon integration test files.

use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use async_trait::async_trait;
use chrono::{DateTime, TimeDelta, Utc};
use tokio::sync::Notify;
use ullage_core::{ProviderId, ProviderRegistry, SubscriptionUsage, UsageQuery};
use ullage_daemon::{AccountConfig, AccountId, BackoffConfig, Clock, DaemonConfig, DaemonEngine};

/// A clock tests control by hand: `advance` moves it, `wait_for_sleep` observes
/// a scheduled delay, and without either it stays frozen at the fixed instant.
#[derive(Clone)]
pub(crate) struct ManualClock {
    base: DateTime<Utc>,
    elapsed_millis: Arc<AtomicU64>,
    changed: Arc<Notify>,
    pub(crate) sleep_durations: Arc<Mutex<Vec<Duration>>>,
    sleep_started: Arc<Notify>,
}

impl ManualClock {
    pub(crate) fn new() -> Self {
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

    // Unused by metrics.rs, which never moves the clock off its fixed base.
    #[allow(dead_code)]
    pub(crate) fn advance(&self, duration: Duration) {
        self.elapsed_millis
            .fetch_add(duration.as_millis().try_into().unwrap(), Ordering::SeqCst);
        self.changed.notify_waiters();
    }

    #[allow(dead_code)]
    pub(crate) async fn wait_for_sleep(&self, expected: Duration) {
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

pub async fn engine_with(
    registry: Arc<ProviderRegistry>,
    clock: Arc<dyn Clock>,
    store: Arc<dyn ullage_daemon::SnapshotStore>,
    config: DaemonConfig,
) -> DaemonEngine {
    DaemonEngine::new(config, registry, clock, store)
        .await
        .unwrap()
}

pub fn account(id: &str, provider: &str, interval: Duration) -> AccountConfig {
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
        metrics: Vec::new(),
    }
}

pub fn usage(provider: &ProviderId, account_label: Option<&str>) -> SubscriptionUsage {
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
