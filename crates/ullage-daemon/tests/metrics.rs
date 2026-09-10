//! Control-surface tests for the persisted per-account metric filter.
//!
//! The engine and protocol tests live in `daemon.rs`; this file keeps the
//! metric-filter cases on their own so neither integration test file has to
//! grow for the other concern.

use std::sync::Arc;
use std::time::Duration;

use async_trait::async_trait;
use chrono::{DateTime, Utc};
use tokio::sync::Notify;
use ullage_auth::{AuthChallenge, AuthCompleteRequest, AuthStartRequest, AuthState, LogoutRequest};
use ullage_core::{
    Capability, Provider, ProviderDescriptor, ProviderError, ProviderId, ProviderRegistry,
    ProviderResult, QueryOutcome, SubscriptionUsage, UsageQuery,
};
use ullage_daemon::{
    AccountConfig, AccountId, Clock, ControlService, DaemonConfig, DaemonError, MemorySnapshotStore,
};
use ullage_protocol::{AccountError, ControlCommand, ControlError, ControlRequest, ControlResult};

mod support;

use support::{account, engine_with, usage};

/// A clock frozen at the test instant. Scheduled probes wait for a wake-up that
/// never comes, so only the explicit control commands under test run.
struct ManualClock {
    now: DateTime<Utc>,
    changed: Notify,
}

impl ManualClock {
    fn new() -> Self {
        Self {
            now: DateTime::parse_from_rfc3339("2026-08-27T12:00:00Z")
                .unwrap()
                .to_utc(),
            changed: Notify::new(),
        }
    }
}

#[async_trait]
impl Clock for ManualClock {
    fn now(&self) -> DateTime<Utc> {
        self.now
    }

    async fn sleep(&self, _: Duration) {
        self.changed.notified().await;
    }
}

/// A provider that answers every usage query with one complete snapshot.
struct FixedProvider {
    id: ProviderId,
}

impl FixedProvider {
    fn new(id: &str) -> Self {
        Self {
            id: ProviderId::new(id),
        }
    }
}

#[async_trait]
impl Provider for FixedProvider {
    type VendorUsage = SubscriptionUsage;

    fn descriptor(&self) -> ProviderDescriptor {
        ProviderDescriptor {
            id: self.id.clone(),
            display_name: format!("{} mock", self.id),
            capabilities: vec![Capability::Authentication, Capability::UsageQuery],
        }
    }

    async fn start_auth(&self, _: AuthStartRequest) -> ProviderResult<AuthChallenge> {
        Err(unsupported())
    }

    async fn complete_auth(&self, _: AuthCompleteRequest) -> ProviderResult<AuthState> {
        Err(unsupported())
    }

    async fn auth_status(&self) -> ProviderResult<AuthState> {
        Err(unsupported())
    }

    async fn logout(&self, _: LogoutRequest) -> ProviderResult<()> {
        Err(unsupported())
    }

    async fn query(&self, request: UsageQuery) -> ProviderResult<QueryOutcome<Self::VendorUsage>> {
        Ok(QueryOutcome::Complete {
            data: usage(&self.id, request.account_label.as_deref()),
        })
    }

    fn normalize(&self, usage: Self::VendorUsage) -> ProviderResult<SubscriptionUsage> {
        Ok(usage)
    }
}

fn unsupported() -> ProviderError {
    ProviderError::UnsupportedCapability {
        capability: "authentication".into(),
    }
}

#[tokio::test(flavor = "multi_thread")]
async fn set_account_metrics_validates_persists_and_restores() {
    let store = Arc::new(MemorySnapshotStore::default());
    let engine = engine_with(
        Arc::new(ProviderRegistry::default()),
        Arc::new(ManualClock::new()),
        store.clone(),
        DaemonConfig::default(),
    )
    .await;
    let id = AccountId::new("filtered");
    engine
        .add_account(account("filtered", "claude", Duration::from_secs(60)))
        .await
        .unwrap();

    let updated = engine
        .set_account_metrics(&id, vec![" Usage ".into(), "usage".into(), "Codex".into()])
        .await
        .unwrap()
        .unwrap();
    assert_eq!(updated.metrics, vec!["Usage", "Codex"]);
    assert_eq!(
        engine.account_config(&id).await.unwrap().metrics,
        vec!["Usage", "Codex"]
    );

    let invalid = engine.set_account_metrics(&id, vec!["  ".into()]).await;
    assert!(matches!(
        invalid,
        Err(DaemonError::InvalidAccountMetrics(ref account)) if account == &id
    ));
    assert_eq!(
        engine.account_config(&id).await.unwrap().metrics,
        vec!["Usage", "Codex"],
        "a rejected filter must not change the stored value"
    );

    let missing = AccountId::new("missing");
    assert!(
        engine
            .set_account_metrics(&missing, vec!["usage".into()])
            .await
            .unwrap()
            .is_none()
    );

    let restarted = engine_with(
        Arc::new(ProviderRegistry::default()),
        Arc::new(ManualClock::new()),
        store,
        DaemonConfig::default(),
    )
    .await;
    assert_eq!(
        restarted.account_config(&id).await.unwrap().metrics,
        vec!["Usage", "Codex"]
    );
}

#[test]
fn legacy_persisted_accounts_without_metrics_still_load() {
    let legacy: AccountConfig = serde_json::from_value(serde_json::json!({
        "id": "legacy",
        "provider": "claude",
        "query": {"account_label": null},
        "enabled": true,
        "interval": {"secs": 300, "nanos": 0},
        "timeout": {"secs": 30, "nanos": 0},
        "jitter": {"secs": 5, "nanos": 0},
        "backoff": {
            "initial": {"secs": 30, "nanos": 0},
            "maximum": {"secs": 1800, "nanos": 0}
        }
    }))
    .unwrap();
    assert!(legacy.metrics.is_empty());
}

#[tokio::test(flavor = "multi_thread")]
async fn control_commands_carry_the_persisted_metric_filter() {
    let provider = FixedProvider::new("filtered");
    let mut registry = ProviderRegistry::default();
    registry.register(provider).unwrap();
    let engine = engine_with(
        Arc::new(registry),
        Arc::new(ManualClock::new()),
        Arc::new(MemorySnapshotStore::default()),
        DaemonConfig::default(),
    )
    .await;
    let service = ControlService::new(engine.clone());

    let added = service
        .handle(ControlRequest::new(
            "add",
            ControlCommand::AddAccount {
                provider: ProviderId::new("filtered"),
                label: None,
            },
        ))
        .await;
    let account_id = match added.result {
        ControlResult::Account(account) => {
            assert!(account.metrics.is_empty(), "new accounts start unfiltered");
            account.id
        }
        other => panic!("unexpected add result: {other:?}"),
    };

    let set = service
        .handle(ControlRequest::new(
            "set-metrics",
            ControlCommand::SetAccountMetrics {
                account: account_id.clone(),
                metrics: vec![" Usage ".into(), "codex".into()],
            },
        ))
        .await;
    assert!(matches!(
        set.result,
        ControlResult::Account(ref account) if account.metrics == ["Usage", "codex"]
    ));

    let invalid = service
        .handle(ControlRequest::new(
            "set-invalid",
            ControlCommand::SetAccountMetrics {
                account: account_id.clone(),
                metrics: vec!["bad\u{7}".into()],
            },
        ))
        .await;
    assert!(matches!(
        invalid.result,
        ControlResult::Error(ControlError::InvalidAccountMetrics)
    ));

    let missing = service
        .handle(ControlRequest::new(
            "set-missing",
            ControlCommand::SetAccountMetrics {
                account: ullage_protocol::AccountId::new("missing"),
                metrics: vec!["usage".into()],
            },
        ))
        .await;
    assert!(matches!(
        missing.result,
        ControlResult::Error(ControlError::Account(AccountError::NotFound(ref id)))
            if id.as_str() == "missing"
    ));

    let listed = service
        .handle(ControlRequest::new("list", ControlCommand::ListAccounts))
        .await;
    assert!(matches!(
        listed.result,
        ControlResult::Accounts(ref accounts)
            if accounts.len() == 1 && accounts[0].metrics == ["Usage", "codex"]
    ));

    let shown = service
        .handle(ControlRequest::new(
            "show-account",
            ControlCommand::ShowAccount {
                account: account_id.clone(),
            },
        ))
        .await;
    assert!(matches!(
        shown.result,
        ControlResult::Account(ref account) if account.metrics == ["Usage", "codex"]
    ));

    let probe = service
        .handle(ControlRequest::new(
            "probe",
            ControlCommand::Probe {
                account_id: account_id.as_str().to_owned(),
                wait: true,
            },
        ))
        .await;
    assert!(matches!(
        probe.result,
        ControlResult::Probe(ref payload) if payload.metrics == ["Usage", "codex"]
    ));

    let snapshots = service
        .handle(ControlRequest::new(
            "show-one",
            ControlCommand::Show {
                account_id: Some(account_id.as_str().to_owned()),
            },
        ))
        .await;
    assert!(matches!(
        snapshots.result,
        ControlResult::Snapshots(ref payloads)
            if payloads.len() == 1 && payloads[0].metrics == ["Usage", "codex"]
    ));

    let all = service
        .handle(ControlRequest::new(
            "show-all",
            ControlCommand::Show { account_id: None },
        ))
        .await;
    assert!(matches!(
        all.result,
        ControlResult::Snapshots(ref payloads)
            if payloads.len() == 1 && payloads[0].metrics == ["Usage", "codex"]
    ));
}
