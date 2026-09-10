//! Helpers shared by the daemon integration test files.

use std::sync::Arc;
use std::time::Duration;

use chrono::DateTime;
use ullage_core::{ProviderId, ProviderRegistry, SubscriptionUsage, UsageQuery};
use ullage_daemon::{AccountConfig, AccountId, BackoffConfig, Clock, DaemonConfig, DaemonEngine};

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
