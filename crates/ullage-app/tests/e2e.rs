use std::collections::BTreeMap;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU32, Ordering};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use async_trait::async_trait;
use chrono::{TimeZone, Utc};
use ullage_auth::{
    AuthChallenge, AuthCompleteRequest, AuthMethod, AuthStartRequest, AuthState, Availability,
    BackendKind, BackendScope, Credential, CredentialBackend, CredentialError, CredentialKey,
    CredentialStore, LogoutRequest, SecretValue,
};
use ullage_cli::{ClientError, ControlClient, ExitCode, run_from};
use ullage_core::{
    Capability, MeasurementUnit, PartialFailure, Provider, ProviderDescriptor, ProviderError,
    ProviderId, ProviderRegistry, ProviderWorkspace, QueryOutcome, RegisteredProvider,
    SubscriptionUsage, UsageMeasurement, UsageQuery, UsageWindow, UsageWindowKind,
};
use ullage_daemon::{
    AccountConfig, AccountId, BackoffConfig, ControlService, DaemonConfig, DaemonEngine,
    JsonSnapshotStore, MemorySnapshotStore, ProbeTrigger, SystemClock,
};
use ullage_protocol::{
    CONTROL_PROTOCOL_VERSION, ControlCommand, ControlError, ControlRequest, ControlResponse,
    ControlResult,
};
use ullage_provider_chatgpt::{ChatGptApiError, ChatGptApiErrorKind, OAuthTokenSet};
use ullage_provider_claude::ClaudeCredential;
use ullage_provider_cursor::{ApiFailure, SecretString};
use ullage_provider_grok::{GrokApiError, OAuthToken};

const SECRET: &str = "e2e-secret-token-value";

struct MemoryBackend {
    values: Mutex<BTreeMap<String, Vec<u8>>>,
}

impl Default for MemoryBackend {
    fn default() -> Self {
        Self {
            values: Mutex::new(BTreeMap::new()),
        }
    }
}

impl CredentialBackend for MemoryBackend {
    fn kind(&self) -> BackendKind {
        BackendKind::ExplicitFileFallback
    }

    fn coordination_scope(&self) -> BackendScope {
        BackendScope::new(b"ullage-e2e")
    }

    fn probe(&self) -> Result<Availability, CredentialError> {
        Ok(Availability::Available)
    }

    fn read(&self, key: &CredentialKey) -> Result<Vec<u8>, CredentialError> {
        let identity = format!("{}:{}", key.service_name(), key.entry_name());
        self.values
            .lock()
            .map_err(|_| CredentialError::Synchronization)?
            .get(&identity)
            .cloned()
            .ok_or(CredentialError::NotFound)
    }

    fn write(&self, key: &CredentialKey, value: &[u8]) -> Result<(), CredentialError> {
        let identity = format!("{}:{}", key.service_name(), key.entry_name());
        self.values
            .lock()
            .map_err(|_| CredentialError::Synchronization)?
            .insert(identity, value.to_vec());
        Ok(())
    }
}

struct ScenarioProvider {
    id: ProviderId,
    account_id: Option<String>,
    store: Option<Arc<CredentialStore>>,
    refresh_count: Arc<AtomicU32>,
    inner: Mutex<ScenarioInner>,
}

#[derive(Default)]
struct ScenarioInner {
    authenticated: bool,
}

impl ScenarioProvider {
    fn new(id: &str) -> Self {
        Self {
            id: ProviderId::new(id),
            account_id: None,
            store: None,
            refresh_count: Arc::new(AtomicU32::new(0)),
            inner: Mutex::new(ScenarioInner::default()),
        }
    }

    fn isolated(id: &str, account_id: &str, store: Arc<CredentialStore>) -> Self {
        Self {
            id: ProviderId::new(id),
            account_id: Some(account_id.into()),
            store: Some(store),
            refresh_count: Arc::new(AtomicU32::new(0)),
            inner: Mutex::new(ScenarioInner::default()),
        }
    }

    fn descriptor_for(id: &str) -> ProviderDescriptor {
        let mut capabilities = vec![
            Capability::Authentication,
            Capability::AuthenticationStatus,
            Capability::Logout,
            Capability::UsageQuery,
        ];
        if id == "chatgpt" {
            capabilities.push(Capability::WorkspaceSelection);
        }
        if id == "claude" {
            capabilities.push(Capability::SubscriptionExpiry);
        }
        ProviderDescriptor {
            id: ProviderId::new(id),
            display_name: id.into(),
            capabilities,
        }
    }
}

#[async_trait]
impl Provider for ScenarioProvider {
    type VendorUsage = SubscriptionUsage;

    fn descriptor(&self) -> ProviderDescriptor {
        Self::descriptor_for(self.id.as_str())
    }

    async fn start_auth(&self, _: AuthStartRequest) -> Result<AuthChallenge, ProviderError> {
        Ok(AuthChallenge {
            flow_id: format!("{}-flow", self.id),
            method: AuthMethod::BrowserOAuth,
            verification_uri: Some("https://example.invalid/auth".into()),
            user_code: None,
            expires_at: None,
            input: None,
        })
    }

    async fn complete_auth(
        &self,
        request: AuthCompleteRequest,
    ) -> Result<AuthState, ProviderError> {
        if request.authorization_code.as_deref() == Some(SECRET) {
            return Err(ProviderError::AuthenticationInvalid {
                message: SECRET.into(),
            });
        }
        self.inner.lock().unwrap().authenticated = true;
        if let (Some(store), Some(account_id)) = (&self.store, &self.account_id) {
            let mut credential = Credential::new();
            credential
                .insert(
                    "session",
                    SecretValue::new(format!("secret-for-{account_id}").into_bytes()),
                )
                .unwrap();
            store
                .set(
                    &CredentialKey::new(self.id.as_str(), account_id).unwrap(),
                    credential,
                )
                .unwrap();
        }
        Ok(AuthState::Authenticated {
            account_label: None,
            expires_at: None,
        })
    }

    async fn auth_status(&self) -> Result<AuthState, ProviderError> {
        if self.inner.lock().unwrap().authenticated {
            Ok(AuthState::Authenticated {
                account_label: None,
                expires_at: None,
            })
        } else {
            Ok(AuthState::NotAuthenticated)
        }
    }

    async fn logout(&self, _: LogoutRequest) -> Result<(), ProviderError> {
        self.inner.lock().unwrap().authenticated = false;
        Ok(())
    }

    async fn list_workspaces(&self) -> Result<Vec<ProviderWorkspace>, ProviderError> {
        if self.id.as_str() != "chatgpt" {
            return Err(ProviderError::UnsupportedCapability {
                capability: "workspace selection".into(),
            });
        }
        Ok(vec![ProviderWorkspace {
            id: "workspace-a".into(),
            label: Some("Workspace A".into()),
        }])
    }

    async fn select_workspace(
        &self,
        workspace_id: &str,
    ) -> Result<ProviderWorkspace, ProviderError> {
        Provider::list_workspaces(self)
            .await?
            .into_iter()
            .find(|workspace| workspace.id == workspace_id)
            .ok_or_else(|| ProviderError::AuthenticationInvalid {
                message: "workspace unavailable".into(),
            })
    }

    async fn query(
        &self,
        request: UsageQuery,
    ) -> Result<QueryOutcome<Self::VendorUsage>, ProviderError> {
        if let (Some(store), Some(account_id)) = (&self.store, &self.account_id) {
            let stored = store
                .get(&CredentialKey::new(self.id.as_str(), account_id).unwrap())
                .map_err(|_| ProviderError::AuthenticationInvalid {
                    message: SECRET.into(),
                })?;
            let expected = format!("secret-for-{account_id}");
            if stored.credential().get("session").map(SecretValue::expose)
                != Some(expected.as_bytes())
            {
                return Err(ProviderError::AuthenticationInvalid {
                    message: SECRET.into(),
                });
            }
        }

        match request.account_label.as_deref() {
            Some("refresh") => {
                self.refresh_count.fetch_add(1, Ordering::SeqCst);
                Ok(QueryOutcome::Complete {
                    data: usage(&self.id, request.account_label, Scenario::Healthy),
                })
            }
            Some("expired-no-refresh") => Err(ProviderError::AuthenticationInvalid {
                message: SECRET.into(),
            }),
            Some("rate-limited") => Err(ProviderError::RateLimited {
                message: SECRET.into(),
                retry_after_seconds: Some(2),
            }),
            Some("timeout") => std::future::pending().await,
            Some("malformed") => Err(ProviderError::ProtocolIncompatible {
                message: SECRET.into(),
            }),
            Some("offline") => Err(ProviderError::Network {
                message: SECRET.into(),
            }),
            Some("missing-5h") => Ok(QueryOutcome::Complete {
                data: usage(&self.id, request.account_label, Scenario::MissingFiveHours),
            }),
            Some("unlimited") => Ok(QueryOutcome::Complete {
                data: usage(&self.id, request.account_label, Scenario::Unlimited),
            }),
            Some("unknown-window") => Ok(QueryOutcome::Complete {
                data: usage(&self.id, request.account_label, Scenario::UnknownWindow),
            }),
            Some("has-expiry") => Ok(QueryOutcome::Complete {
                data: usage(&self.id, request.account_label, Scenario::HasExpiry),
            }),
            Some("partial") => Ok(QueryOutcome::Partial {
                data: usage(&self.id, request.account_label, Scenario::Healthy),
                failures: vec![PartialFailure {
                    scope: "profile".into(),
                    message: SECRET.into(),
                }],
            }),
            _ if self.store.is_some() => Ok(QueryOutcome::Complete {
                data: usage(
                    &self.id,
                    Some(self.account_id.clone().unwrap_or_default()),
                    Scenario::Healthy,
                ),
            }),
            _ => Ok(QueryOutcome::Complete {
                data: usage(&self.id, request.account_label, Scenario::Healthy),
            }),
        }
    }

    fn normalize(
        &self,
        vendor_usage: Self::VendorUsage,
    ) -> Result<SubscriptionUsage, ProviderError> {
        Ok(vendor_usage)
    }
}

#[derive(Clone, Copy)]
enum Scenario {
    Healthy,
    MissingFiveHours,
    Unlimited,
    UnknownWindow,
    HasExpiry,
}

fn observed_at() -> chrono::DateTime<Utc> {
    Utc.with_ymd_and_hms(2026, 8, 27, 12, 0, 0).unwrap()
}

fn usage(
    provider: &ProviderId,
    account_label: Option<String>,
    scenario: Scenario,
) -> SubscriptionUsage {
    let mut windows = Vec::new();
    match scenario {
        Scenario::MissingFiveHours => windows.push(weekly_window(Some(100.0))),
        Scenario::Unlimited => {
            windows.push(five_hour_window(None));
            windows.push(weekly_window(None));
        }
        Scenario::UnknownWindow => windows.push(UsageWindow {
            window: UsageWindowKind::Other {
                id: "rolling_30d".into(),
                label: "Rolling 30 days".into(),
            },
            resets_at: None,
            measurements: vec![UsageMeasurement {
                name: "tokens".into(),
                used: 3.0,
                limit: Some(10.0),
                unit: MeasurementUnit::Tokens,
            }],
        }),
        Scenario::Healthy | Scenario::HasExpiry => {
            windows.push(five_hour_window(Some(100.0)));
            windows.push(weekly_window(Some(500.0)));
        }
    }
    SubscriptionUsage {
        provider: provider.clone(),
        account_label,
        plan: Some("mock".into()),
        subscription_expires_at: match scenario {
            Scenario::HasExpiry => Some(observed_at()),
            _ => None,
        },
        observed_at: observed_at(),
        windows,
    }
}

fn five_hour_window(limit: Option<f64>) -> UsageWindow {
    UsageWindow {
        window: UsageWindowKind::FiveHours,
        resets_at: Some(observed_at()),
        measurements: vec![UsageMeasurement {
            name: "tokens".into(),
            used: 12.0,
            limit,
            unit: MeasurementUnit::Tokens,
        }],
    }
}

fn weekly_window(limit: Option<f64>) -> UsageWindow {
    UsageWindow {
        window: UsageWindowKind::Weekly,
        resets_at: Some(observed_at()),
        measurements: vec![UsageMeasurement {
            name: "tokens".into(),
            used: 40.0,
            limit,
            unit: MeasurementUnit::Tokens,
        }],
    }
}

fn account(id: &str, provider: &str, label: &str) -> AccountConfig {
    AccountConfig {
        id: AccountId::new(id),
        provider: ProviderId::new(provider),
        query: UsageQuery {
            account_label: Some(label.into()),
        },
        enabled: true,
        interval: Duration::from_secs(300),
        timeout: Duration::from_millis(80),
        jitter: Duration::ZERO,
        backoff: BackoffConfig {
            initial: Duration::from_secs(1),
            maximum: Duration::from_secs(8),
        },
    }
}

async fn engine_with(
    registry: ProviderRegistry,
    store: Arc<dyn ullage_daemon::SnapshotStore>,
) -> DaemonEngine {
    DaemonEngine::new(
        DaemonConfig::default(),
        Arc::new(registry),
        Arc::new(SystemClock),
        store,
    )
    .await
    .unwrap()
}

fn control_request(id: &str, command: ControlCommand) -> ControlRequest {
    ControlRequest::new(id, command)
}

async fn handle(control: &ControlService, id: &str, command: ControlCommand) -> ControlResult {
    control.handle(control_request(id, command)).await.result
}

struct RecordingClient {
    result: ControlResult,
}

impl ControlClient for RecordingClient {
    fn send(&self, request: &ControlRequest) -> Result<ControlResponse, ClientError> {
        Ok(ControlResponse {
            version: CONTROL_PROTOCOL_VERSION,
            request_id: request.request_id.clone(),
            result: self.result.clone(),
            diagnostic: None,
        })
    }
}

#[tokio::test]
async fn four_mock_providers_auth_probe_persist_and_show() {
    let mut registry = ProviderRegistry::default();
    for provider in ["claude", "chatgpt", "grok", "cursor"] {
        registry.register(ScenarioProvider::new(provider)).unwrap();
    }
    let snapshots = Arc::new(MemorySnapshotStore::default());
    let engine = engine_with(registry, snapshots.clone()).await;
    for configured in [
        account("claude-a", "claude", "healthy"),
        account("chatgpt-a", "chatgpt", "healthy"),
        account("grok-a", "grok", "healthy"),
        account("cursor-a", "cursor", "healthy"),
    ] {
        engine.add_account(configured).await.unwrap();
    }
    let control = ControlService::new(engine.clone());

    for (provider, account_id) in [
        ("claude", "claude-a"),
        ("chatgpt", "chatgpt-a"),
        ("grok", "grok-a"),
        ("cursor", "cursor-a"),
    ] {
        let provider = ProviderId::new(provider);
        let account = ullage_protocol::AccountId::new(account_id);
        let started = handle(
            &control,
            &format!("{provider}-start"),
            ControlCommand::StartAuth {
                provider: provider.clone(),
                account: account.clone(),
                request: AuthStartRequest {
                    method: None,
                    redirect_uri: None,
                },
            },
        )
        .await;
        let ControlResult::AuthChallenge(challenge) = started else {
            panic!("auth start failed: {started:?}");
        };
        let completed = handle(
            &control,
            &format!("{provider}-complete"),
            ControlCommand::CompleteAuth {
                provider: provider.clone(),
                account: account.clone(),
                request: AuthCompleteRequest {
                    flow_id: challenge.flow_id,
                    authorization_code: Some("mock-code".into()),
                    redirect_uri: Some("https://example.invalid/callback".into()),
                },
            },
        )
        .await;
        assert!(matches!(
            completed,
            ControlResult::AuthState(AuthState::Authenticated { .. })
        ));
        if provider.as_str() == "chatgpt" {
            let selected = handle(
                &control,
                "chatgpt-workspace-select",
                ControlCommand::SelectWorkspace {
                    provider: provider.clone(),
                    account: account.clone(),
                    workspace_id: "workspace-a".into(),
                },
            )
            .await;
            assert!(matches!(
                selected,
                ControlResult::Workspace(ref workspace) if workspace.id == "workspace-a"
            ));
        }
        let probed = handle(
            &control,
            &format!("{provider}-probe"),
            ControlCommand::Probe {
                account_id: account_id.into(),
                wait: true,
            },
        )
        .await;
        let ControlResult::Probe(payload) = probed else {
            panic!("probe failed: {probed:?}");
        };
        assert_eq!(payload.account_id, account_id);
        let QueryOutcome::Complete { data } = payload.usage else {
            panic!("expected complete usage");
        };
        assert_eq!(data.windows.len(), 2);
        assert!(data.subscription_expires_at.is_none());
        let shown = handle(
            &control,
            &format!("{provider}-show"),
            ControlCommand::Show {
                account_id: Some(account_id.into()),
            },
        )
        .await;
        let ControlResult::Snapshots(snapshots) = shown else {
            panic!("show failed: {shown:?}");
        };
        assert_eq!(snapshots.len(), 1);
        assert_eq!(snapshots[0].account_id, account_id);
        assert!(!snapshots[0].stale);
    }
}

#[tokio::test(flavor = "multi_thread")]
async fn same_provider_accounts_keep_credentials_isolated_under_concurrent_probes() {
    let store = Arc::new(CredentialStore::new(MemoryBackend::default()));
    let mut registry = ProviderRegistry::default();
    let credentials = store.clone();
    registry
        .register_factory(
            ScenarioProvider::descriptor_for("claude"),
            move |account_id| {
                Ok(Arc::new(ScenarioProvider::isolated(
                    "claude",
                    account_id,
                    credentials.clone(),
                )) as Arc<dyn RegisteredProvider>)
            },
        )
        .unwrap();
    let engine = engine_with(registry, Arc::new(MemorySnapshotStore::default())).await;
    engine
        .add_account(account("claude-a", "claude", "a"))
        .await
        .unwrap();
    engine
        .add_account(account("claude-b", "claude", "b"))
        .await
        .unwrap();
    let control = ControlService::new(engine.clone());
    for account_id in ["claude-a", "claude-b"] {
        let started = handle(
            &control,
            &format!("{account_id}-start"),
            ControlCommand::StartAuth {
                provider: ProviderId::new("claude"),
                account: ullage_protocol::AccountId::new(account_id),
                request: AuthStartRequest {
                    method: None,
                    redirect_uri: None,
                },
            },
        )
        .await;
        let ControlResult::AuthChallenge(challenge) = started else {
            panic!("auth start failed: {started:?}");
        };
        let completed = handle(
            &control,
            &format!("{account_id}-complete"),
            ControlCommand::CompleteAuth {
                provider: ProviderId::new("claude"),
                account: ullage_protocol::AccountId::new(account_id),
                request: AuthCompleteRequest {
                    flow_id: challenge.flow_id,
                    authorization_code: Some("mock-code".into()),
                    redirect_uri: None,
                },
            },
        )
        .await;
        assert!(matches!(
            completed,
            ControlResult::AuthState(AuthState::Authenticated { .. })
        ));
    }

    let first = handle(
        &control,
        "probe-a",
        ControlCommand::Probe {
            account_id: "claude-a".into(),
            wait: true,
        },
    );
    let second = handle(
        &control,
        "probe-b",
        ControlCommand::Probe {
            account_id: "claude-b".into(),
            wait: true,
        },
    );
    let (first, second) = tokio::join!(first, second);
    for (result, expected) in [(first, "claude-a"), (second, "claude-b")] {
        let ControlResult::Probe(payload) = result else {
            panic!("concurrent probe failed: {result:?}");
        };
        let QueryOutcome::Complete { data } = payload.usage else {
            panic!("expected isolated complete usage");
        };
        assert_eq!(data.account_label.as_deref(), Some(expected));
    }
    let shown = handle(
        &control,
        "show-all",
        ControlCommand::Show { account_id: None },
    )
    .await;
    let ControlResult::Snapshots(snapshots) = shown else {
        panic!("show all failed: {shown:?}");
    };
    assert_eq!(snapshots.len(), 2);
}

#[tokio::test]
async fn missing_unlimited_unknown_optional_expiry_and_partial_data_are_preserved() {
    let mut registry = ProviderRegistry::default();
    registry.register(ScenarioProvider::new("claude")).unwrap();
    let engine = engine_with(registry, Arc::new(MemorySnapshotStore::default())).await;
    for configured in [
        account("missing", "claude", "missing-5h"),
        account("unlimited", "claude", "unlimited"),
        account("unknown", "claude", "unknown-window"),
        account("expiry", "claude", "has-expiry"),
        account("optional", "claude", "healthy"),
        account("partial", "claude", "partial"),
    ] {
        engine.add_account(configured).await.unwrap();
    }
    let control = ControlService::new(engine);

    let missing = probe_usage(&control, "missing").await;
    assert!(
        !missing
            .windows
            .iter()
            .any(|window| matches!(window.window, UsageWindowKind::FiveHours))
    );
    assert!(
        missing
            .windows
            .iter()
            .any(|window| matches!(window.window, UsageWindowKind::Weekly))
    );

    let unlimited = probe_usage(&control, "unlimited").await;
    assert!(
        unlimited
            .windows
            .iter()
            .flat_map(|window| &window.measurements)
            .all(|measurement| measurement.limit.is_none())
    );

    let unknown = probe_usage(&control, "unknown").await;
    assert!(matches!(
        unknown.windows[0].window,
        UsageWindowKind::Other { ref id, .. } if id == "rolling_30d"
    ));

    let expiry = probe_usage(&control, "expiry").await;
    assert!(expiry.subscription_expires_at.is_some());
    let optional = probe_usage(&control, "optional").await;
    assert!(optional.subscription_expires_at.is_none());

    let partial = handle(
        &control,
        "partial",
        ControlCommand::Probe {
            account_id: "partial".into(),
            wait: true,
        },
    )
    .await;
    let ControlResult::Probe(ref payload) = partial else {
        panic!("partial probe failed: {partial:?}");
    };
    let QueryOutcome::Partial { ref failures, .. } = payload.usage else {
        panic!("expected partial outcome");
    };
    assert_eq!(failures[0].scope, "profile");
    assert_eq!(
        failures[0].message,
        "provider protocol response is incompatible"
    );
    assert!(!format!("{payload:?}").contains(SECRET));

    let shown = handle(
        &control,
        "partial-show",
        ControlCommand::Show {
            account_id: Some("partial".into()),
        },
    )
    .await;
    for (command, result) in [
        (
            ["ullage", "--color", "never", "probe", "partial"].as_slice(),
            partial.clone(),
        ),
        (
            [
                "ullage",
                "--color",
                "never",
                "--diagnose",
                "probe",
                "partial",
            ]
            .as_slice(),
            partial,
        ),
        (
            ["ullage", "--color", "never", "show", "partial"].as_slice(),
            shown.clone(),
        ),
        (
            [
                "ullage",
                "--color",
                "never",
                "--diagnose",
                "show",
                "partial",
            ]
            .as_slice(),
            shown,
        ),
    ] {
        let diagnose = command.contains(&"--diagnose");
        let output = run_from(command.iter().copied(), &RecordingClient { result });
        assert_eq!(output.code, ExitCode::Partial, "{command:?}");
        assert!(!output.stdout.contains(SECRET), "{}", output.stdout);
        assert!(!output.stderr.contains(SECRET), "{}", output.stderr);
        if diagnose {
            assert!(
                output
                    .stdout
                    .contains("profile: provider protocol response is incompatible"),
                "{}",
                output.stdout
            );
        } else {
            assert!(
                !output
                    .stdout
                    .contains("profile: provider protocol response is incompatible"),
                "{}",
                output.stdout
            );
        }
    }
}

async fn probe_usage(control: &ControlService, account_id: &str) -> SubscriptionUsage {
    let probed = handle(
        control,
        account_id,
        ControlCommand::Probe {
            account_id: account_id.into(),
            wait: true,
        },
    )
    .await;
    match probed {
        ControlResult::Probe(payload) => match payload.usage {
            QueryOutcome::Complete { data } | QueryOutcome::Partial { data, .. } => data,
        },
        other => panic!("probe {account_id} failed: {other:?}"),
    }
}

#[tokio::test(flavor = "multi_thread")]
async fn token_refresh_rate_limit_timeout_malformed_offline_and_restart() {
    let refresh = ScenarioProvider::new("claude");
    let refresh_count = refresh.refresh_count.clone();
    let mut registry = ProviderRegistry::default();
    registry.register(refresh).unwrap();
    let directory = tempfile::tempdir().unwrap();
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        std::fs::set_permissions(directory.path(), std::fs::Permissions::from_mode(0o700)).unwrap();
    }
    let store = Arc::new(JsonSnapshotStore::new(directory.path().join("state.json")));
    let engine = engine_with(registry, store.clone()).await;
    for configured in [
        account("refresh", "claude", "refresh"),
        account("expired", "claude", "expired-no-refresh"),
        account("limited", "claude", "rate-limited"),
        account("timeout", "claude", "timeout"),
        account("malformed", "claude", "malformed"),
        account("offline", "claude", "offline"),
    ] {
        engine.add_account(configured).await.unwrap();
    }
    let control = ControlService::new(engine.clone());

    let refreshed = handle(
        &control,
        "refresh",
        ControlCommand::Probe {
            account_id: "refresh".into(),
            wait: true,
        },
    )
    .await;
    assert!(matches!(refreshed, ControlResult::Probe(_)));
    assert_eq!(refresh_count.load(Ordering::SeqCst), 1);

    let expired = handle(
        &control,
        "expired",
        ControlCommand::Probe {
            account_id: "expired".into(),
            wait: true,
        },
    )
    .await;
    assert!(matches!(
        expired,
        ControlResult::Error(ControlError::Provider(ProviderError::AuthenticationInvalid { ref message }))
            if message == "provider authentication is invalid"
    ));
    assert!(!format!("{expired:?}").contains(SECRET));

    let limited = handle(
        &control,
        "limited",
        ControlCommand::Probe {
            account_id: "limited".into(),
            wait: true,
        },
    )
    .await;
    assert!(matches!(
        limited,
        ControlResult::Error(ControlError::Provider(ProviderError::RateLimited {
            retry_after_seconds: Some(2),
            ..
        }))
    ));

    let timed_out = handle(
        &control,
        "timeout",
        ControlCommand::Probe {
            account_id: "timeout".into(),
            wait: true,
        },
    )
    .await;
    assert!(matches!(
        timed_out,
        ControlResult::Error(ControlError::Timeout)
    ));

    let malformed = handle(
        &control,
        "malformed",
        ControlCommand::Probe {
            account_id: "malformed".into(),
            wait: true,
        },
    )
    .await;
    assert!(matches!(
        malformed,
        ControlResult::Error(ControlError::Provider(
            ProviderError::ProtocolIncompatible { .. }
        ))
    ));

    let offline = handle(
        &control,
        "offline",
        ControlCommand::Probe {
            account_id: "offline".into(),
            wait: true,
        },
    )
    .await;
    assert!(matches!(
        offline,
        ControlResult::Error(ControlError::Provider(ProviderError::Network { .. }))
    ));

    engine
        .probe(&AccountId::new("refresh"), ProbeTrigger::Manual)
        .await
        .unwrap();
    engine.shutdown();
    drop(control);
    drop(engine);

    let restored = DaemonEngine::new(
        DaemonConfig::default(),
        Arc::new(ProviderRegistry::default()),
        Arc::new(SystemClock),
        store,
    )
    .await
    .unwrap();
    let restarted = ControlService::new(restored);
    let shown = handle(
        &restarted,
        "restart-show",
        ControlCommand::Show {
            account_id: Some("refresh".into()),
        },
    )
    .await;
    let ControlResult::Snapshots(snapshots) = shown else {
        panic!("restart show failed: {shown:?}");
    };
    assert_eq!(snapshots.len(), 1);
    assert_eq!(snapshots[0].account_id, "refresh");
}

#[tokio::test]
async fn sensitive_material_is_absent_from_cli_errors_debug_and_fixtures() {
    let chatgpt = ChatGptApiError::new(ChatGptApiErrorKind::AuthenticationInvalid, SECRET);
    let grok = GrokApiError::AuthenticationInvalid(SECRET.into());
    let cursor = ApiFailure::authentication(SECRET);
    let claude = ClaudeCredential {
        access_token: SECRET.into(),
        refresh_token: Some(SECRET.into()),
        expires_at: None,
    };
    let tokens = OAuthTokenSet::new(SECRET, Some(SECRET.into()), None).unwrap();
    let grok_token = OAuthToken {
        access_token: SECRET.into(),
        refresh_token: Some(SECRET.into()),
        expires_at: None,
        account_label: Some("label".into()),
    };
    let secret_string = SecretString::new(SECRET);
    for rendered in [
        format!("{chatgpt:?}"),
        format!("{grok:?}"),
        format!("{cursor:?}"),
        format!("{claude:?}"),
        format!("{tokens:?}"),
        format!("{grok_token:?}"),
        format!("{secret_string:?}"),
    ] {
        assert!(!rendered.contains(SECRET), "{rendered}");
        assert!(rendered.contains("[REDACTED]"), "{rendered}");
    }

    let usage = SubscriptionUsage {
        provider: ProviderId::new("claude"),
        account_label: Some(SECRET.into()),
        plan: Some("pro".into()),
        subscription_expires_at: None,
        observed_at: observed_at(),
        windows: Vec::new(),
    };
    let result = ControlResult::Snapshots(vec![ullage_protocol::SnapshotPayload {
        account_id: "primary".into(),
        usage: QueryOutcome::Complete { data: usage },
        last_success_at: observed_at(),
        stale: false,
        last_error: None,
        last_error_at: None,
    }]);
    let output = run_from(
        ["ullage", "--output", "json", "show", "primary"],
        &RecordingClient { result },
    );
    assert_eq!(output.code, ExitCode::Success);
    assert!(!output.stdout.contains(SECRET));
    assert!(output.stdout.contains("[redacted]"));

    scan_fixtures_for_live_secrets(&workspace_root());
}

#[cfg(unix)]
#[tokio::test(flavor = "multi_thread")]
async fn unix_socket_carries_auth_probe_persist_and_show() {
    use std::os::unix::fs::PermissionsExt;
    use std::sync::atomic::{AtomicUsize, Ordering as AtomicOrdering};

    static NEXT_SOCKET: AtomicUsize = AtomicUsize::new(0);
    let directory = std::env::temp_dir().join(format!(
        "ullage-e2e-{}-{}",
        std::process::id(),
        NEXT_SOCKET.fetch_add(1, AtomicOrdering::SeqCst)
    ));
    let socket = directory.join("control.sock");
    let mut registry = ProviderRegistry::default();
    registry.register(ScenarioProvider::new("claude")).unwrap();
    let engine = engine_with(registry, Arc::new(MemorySnapshotStore::default())).await;
    engine
        .add_account(account("claude-a", "claude", "healthy"))
        .await
        .unwrap();
    let service = ControlService::new(engine.clone());
    let server = ullage_daemon::UnixControlServer::bind(&socket, service)
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
    let started = send_socket(
        &socket,
        ControlRequest::new(
            "start",
            ControlCommand::StartAuth {
                provider: ProviderId::new("claude"),
                account: ullage_protocol::AccountId::new("claude-a"),
                request: AuthStartRequest {
                    method: None,
                    redirect_uri: None,
                },
            },
        ),
    )
    .await;
    let ControlResult::AuthChallenge(challenge) = started.result else {
        panic!("socket auth start failed: {:?}", started.result);
    };
    let completed = send_socket(
        &socket,
        ControlRequest::new(
            "complete",
            ControlCommand::CompleteAuth {
                provider: ProviderId::new("claude"),
                account: ullage_protocol::AccountId::new("claude-a"),
                request: AuthCompleteRequest {
                    flow_id: challenge.flow_id,
                    authorization_code: Some("mock-code".into()),
                    redirect_uri: None,
                },
            },
        ),
    )
    .await;
    assert!(matches!(
        completed.result,
        ControlResult::AuthState(AuthState::Authenticated { .. })
    ));
    let probed = send_socket(
        &socket,
        ControlRequest::new(
            "probe",
            ControlCommand::Probe {
                account_id: "claude-a".into(),
                wait: true,
            },
        ),
    )
    .await;
    assert!(matches!(probed.result, ControlResult::Probe(_)));
    let shown = send_socket(
        &socket,
        ControlRequest::new(
            "show",
            ControlCommand::Show {
                account_id: Some("claude-a".into()),
            },
        ),
    )
    .await;
    assert!(matches!(
        shown.result,
        ControlResult::Snapshots(ref snapshots) if snapshots.len() == 1
    ));
    engine.shutdown();
    running.await.unwrap().unwrap();
}

#[cfg(unix)]
async fn send_socket(socket: &Path, request: ControlRequest) -> ControlResponse {
    use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader};

    let mut encoded = serde_json::to_vec(&request).unwrap();
    encoded.push(b'\n');
    let mut stream = tokio::net::UnixStream::connect(socket).await.unwrap();
    stream.write_all(&encoded).await.unwrap();
    let mut response = String::new();
    BufReader::new(stream)
        .read_line(&mut response)
        .await
        .unwrap();
    serde_json::from_str(&response).unwrap()
}

fn workspace_root() -> PathBuf {
    PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("../..")
}

fn scan_fixtures_for_live_secrets(root: &Path) {
    let fixtures = root.join("providers");
    let mut files = Vec::new();
    collect_json_files(&fixtures, &mut files);
    assert!(
        !files.is_empty(),
        "expected provider fixtures under {}",
        fixtures.display()
    );
    for path in files {
        let value: serde_json::Value =
            serde_json::from_slice(&std::fs::read(&path).unwrap()).unwrap();
        scan_json(&path, "", &value);
    }
}

fn collect_json_files(dir: &Path, files: &mut Vec<PathBuf>) {
    let Ok(entries) = std::fs::read_dir(dir) else {
        return;
    };
    for entry in entries {
        let entry = entry.unwrap();
        let path = entry.path();
        if path.is_dir() {
            collect_json_files(&path, files);
        } else if path.extension().and_then(|ext| ext.to_str()) == Some("json")
            && path
                .components()
                .any(|component| component.as_os_str() == "fixtures")
        {
            files.push(path);
        }
    }
}

fn scan_json(path: &Path, key: &str, value: &serde_json::Value) {
    match value {
        serde_json::Value::String(text) => {
            assert!(
                !looks_like_live_secret(key, text),
                "{} contains a live-looking secret in {key}",
                path.display()
            );
        }
        serde_json::Value::Array(items) => {
            for item in items {
                scan_json(path, key, item);
            }
        }
        serde_json::Value::Object(map) => {
            for (child, nested) in map {
                scan_json(path, child, nested);
            }
        }
        _ => {}
    }
}

fn looks_like_live_secret(key: &str, value: &str) -> bool {
    let lowered = key.to_ascii_lowercase();
    let sensitive_key = [
        "access_token",
        "refresh_token",
        "id_token",
        "identity_token",
        "authorization",
        "password",
        "secret",
        "api_key",
        "cookie",
    ]
    .iter()
    .any(|name| lowered.contains(name));
    if value.contains("sk-ant-")
        || value.contains("sk-or-")
        || value.contains("ghp_")
        || value.contains("github_pat_")
        || value.contains("Bearer ey")
        || value.starts_with("eyJ")
    {
        return true;
    }
    if !sensitive_key {
        return false;
    }
    let lowered_value = value.to_ascii_lowercase();
    !(value.is_empty()
        || lowered_value.contains("test")
        || lowered_value.contains("mock")
        || lowered_value.contains("example")
        || lowered_value.contains("fixture")
        || lowered_value.contains("redacted")
        || lowered_value.contains("placeholder"))
}
