use std::collections::BTreeMap;
use std::sync::{Arc, Mutex};
use std::time::Duration;

use async_trait::async_trait;
use chrono::Utc;
use ullage_auth::{
    AuthChallenge, AuthCompleteRequest, AuthMethod, AuthStartRequest, AuthState, Availability,
    BackendKind, BackendScope, CredentialBackend, CredentialError, CredentialKey, CredentialStore,
    LogoutRequest,
};
use ullage_core::{
    Capability, Provider, ProviderDescriptor, ProviderError, ProviderId, ProviderRegistry,
    ProviderWorkspace, QueryOutcome, SubscriptionUsage, UsageQuery,
};
use ullage_daemon::{
    AccountConfig, AccountId, BackoffConfig, ControlService, DaemonConfig, DaemonEngine,
    MemorySnapshotStore, ProbeTrigger, SystemClock,
};
use ullage_protocol::{ControlCommand, ControlRequest, ControlResult};

struct MockProviderServer {
    id: ProviderId,
}

#[async_trait]
impl Provider for MockProviderServer {
    type VendorUsage = SubscriptionUsage;

    fn descriptor(&self) -> ProviderDescriptor {
        ProviderDescriptor {
            id: self.id.clone(),
            display_name: self.id.as_str().into(),
            capabilities: vec![
                Capability::Authentication,
                Capability::AuthenticationStatus,
                Capability::Logout,
                Capability::UsageQuery,
            ],
        }
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

    async fn complete_auth(&self, _: AuthCompleteRequest) -> Result<AuthState, ProviderError> {
        Ok(AuthState::Authenticated {
            account_label: None,
            expires_at: None,
            account_key: None,
        })
    }

    async fn auth_status(&self) -> Result<AuthState, ProviderError> {
        Ok(AuthState::Authenticated {
            account_label: None,
            expires_at: None,
            account_key: None,
        })
    }

    async fn logout(&self, _: LogoutRequest) -> Result<(), ProviderError> {
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
        self.list_workspaces()
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
        if request.account_label.as_deref() == Some("broken") {
            return Err(ProviderError::Network {
                message: "mock failure".into(),
            });
        }
        Ok(QueryOutcome::Complete {
            data: SubscriptionUsage {
                provider: self.id.clone(),
                account_label: request.account_label,
                plan: Some("mock".into()),
                subscription_expires_at: None,
                observed_at: Utc::now(),
                windows: Vec::new(),
            },
        })
    }

    fn normalize(
        &self,
        vendor_usage: Self::VendorUsage,
    ) -> Result<SubscriptionUsage, ProviderError> {
        Ok(vendor_usage)
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
        timeout: Duration::from_secs(5),
        jitter: Duration::ZERO,
        backoff: BackoffConfig::default(),
        metrics: Vec::new(),
    }
}

#[tokio::test]
async fn four_mock_provider_servers_cover_control_and_multi_account_isolation() {
    let mut registry = ProviderRegistry::default();
    for provider in ["claude", "chatgpt", "grok", "cursor"] {
        registry
            .register(MockProviderServer {
                id: ProviderId::new(provider),
            })
            .unwrap();
    }
    let snapshots = Arc::new(MemorySnapshotStore::default());
    let engine = DaemonEngine::new(
        DaemonConfig::default(),
        Arc::new(registry),
        Arc::new(SystemClock),
        snapshots.clone(),
    )
    .await
    .unwrap();
    for configured in [
        account("claude-a", "claude", "a@example.test"),
        account("claude-b", "claude", "b@example.test"),
        account("chatgpt-a", "chatgpt", "workspace-a"),
        account("grok-a", "grok", "grok-a"),
        account("cursor-a", "cursor", "cursor-a"),
        account("cursor-broken", "cursor", "broken"),
    ] {
        engine.add_account(configured).await.unwrap();
    }
    let control = ControlService::new(engine.clone());
    let providers = control
        .handle(ControlRequest::new(
            "providers",
            ControlCommand::ListProviders,
        ))
        .await;
    assert!(matches!(providers.result, ControlResult::Providers(items) if items.len() == 4));

    for (provider, account_id) in [
        ("claude", "claude-a"),
        ("chatgpt", "chatgpt-a"),
        ("grok", "grok-a"),
        ("cursor", "cursor-a"),
    ] {
        let provider = ProviderId::new(provider);
        let account = ullage_protocol::AccountId::new(account_id);
        let started = control
            .handle(ControlRequest::new(
                format!("{provider}-start"),
                ControlCommand::StartAuth {
                    provider: provider.clone(),
                    account: account.clone(),
                    request: AuthStartRequest {
                        method: None,
                        redirect_uri: None,
                    },
                },
            ))
            .await;
        let flow_id = match started.result {
            ControlResult::AuthChallenge(challenge) => challenge.flow_id,
            other => panic!("unexpected auth start result: {other:?}"),
        };
        let completed = control
            .handle(ControlRequest::new(
                format!("{provider}-complete"),
                ControlCommand::CompleteAuth {
                    provider: provider.clone(),
                    account: account.clone(),
                    request: AuthCompleteRequest {
                        flow_id,
                        authorization_code: Some("mock-code".into()),
                        redirect_uri: Some("https://example.invalid/callback".into()),
                    },
                },
            ))
            .await;
        assert!(matches!(
            completed.result,
            ControlResult::AuthState(AuthState::Authenticated { .. })
        ));
        let status = control
            .handle(ControlRequest::new(
                format!("{provider}-status"),
                ControlCommand::AuthStatus {
                    provider: provider.clone(),
                    account: account.clone(),
                },
            ))
            .await;
        assert!(matches!(status.result, ControlResult::AuthState(_)));
        if provider.as_str() == "chatgpt" {
            let listed = control
                .handle(ControlRequest::new(
                    "chatgpt-workspaces",
                    ControlCommand::ListWorkspaces {
                        provider: provider.clone(),
                        account: account.clone(),
                    },
                ))
                .await;
            assert!(matches!(
                listed.result,
                ControlResult::Workspaces(ref workspaces)
                    if workspaces.iter().any(|workspace| workspace.id == "workspace-a")
            ));
            let selected = control
                .handle(ControlRequest::new(
                    "chatgpt-workspace-select",
                    ControlCommand::SelectWorkspace {
                        provider: provider.clone(),
                        account: account.clone(),
                        workspace_id: "workspace-a".into(),
                    },
                ))
                .await;
            assert!(matches!(
                selected.result,
                ControlResult::Workspace(ref workspace) if workspace.id == "workspace-a"
            ));
        }
        let logout = control
            .handle(ControlRequest::new(
                format!("{provider}-logout"),
                ControlCommand::Logout {
                    provider,
                    account,
                    request: LogoutRequest::default(),
                },
            ))
            .await;
        assert!(matches!(logout.result, ControlResult::Ack));
    }

    for id in ["claude-a", "claude-b", "chatgpt-a", "grok-a", "cursor-a"] {
        engine
            .probe(&AccountId::new(id), ProbeTrigger::Manual)
            .await
            .unwrap();
    }
    assert!(
        engine
            .probe(&AccountId::new("cursor-broken"), ProbeTrigger::Manual)
            .await
            .is_err()
    );
    let records = snapshots.records().await;
    assert_eq!(records.len(), 5);
    assert_eq!(
        records[&AccountId::new("claude-a")]
            .usage
            .data()
            .account_label
            .as_deref(),
        Some("a@example.test")
    );
    assert_eq!(
        records[&AccountId::new("claude-b")]
            .usage
            .data()
            .account_label
            .as_deref(),
        Some("b@example.test")
    );

    let disabled = control
        .handle(ControlRequest::new(
            "disable",
            ControlCommand::SetAccountEnabled {
                account: ullage_protocol::AccountId::new("claude-b"),
                enabled: false,
            },
        ))
        .await;
    assert!(matches!(disabled.result, ControlResult::Account(account) if !account.enabled));
    let removed = control
        .handle(ControlRequest::new(
            "remove",
            ControlCommand::RemoveAccount {
                account: ullage_protocol::AccountId::new("claude-b"),
            },
        ))
        .await;
    assert!(matches!(removed.result, ControlResult::Ack));
}

trait OutcomeData<T> {
    fn data(&self) -> &T;
}

impl<T> OutcomeData<T> for QueryOutcome<T> {
    fn data(&self) -> &T {
        match self {
            QueryOutcome::Complete { data } | QueryOutcome::Partial { data, .. } => data,
        }
    }
}

#[derive(Default)]
struct MemoryBackend {
    values: Mutex<BTreeMap<String, Vec<u8>>>,
}

impl CredentialBackend for MemoryBackend {
    fn kind(&self) -> BackendKind {
        BackendKind::ExplicitFileFallback
    }
    fn coordination_scope(&self) -> BackendScope {
        BackendScope::new(b"registry-test")
    }
    fn probe(&self) -> Result<Availability, CredentialError> {
        Ok(Availability::Available)
    }
    fn read(&self, key: &CredentialKey) -> Result<Vec<u8>, CredentialError> {
        let identity = format!("{}:{}", key.service_name(), key.entry_name());
        self.values
            .lock()
            .unwrap()
            .get(&identity)
            .cloned()
            .ok_or(CredentialError::NotFound)
    }
    fn write(&self, key: &CredentialKey, value: &[u8]) -> Result<(), CredentialError> {
        let identity = format!("{}:{}", key.service_name(), key.entry_name());
        self.values.lock().unwrap().insert(identity, value.to_vec());
        Ok(())
    }
}

#[test]
fn production_composition_registers_all_providers() {
    let credentials = Arc::new(CredentialStore::new(MemoryBackend::default()));
    let registry = ullage_app::registry_with_credentials(credentials).unwrap();
    let ids = registry
        .descriptors()
        .into_iter()
        .map(|descriptor| descriptor.id.as_str().to_owned())
        .collect::<Vec<_>>();
    assert_eq!(ids, ["chatgpt", "claude", "cursor", "grok", "opencode"]);
    assert!(
        registry
            .descriptors()
            .into_iter()
            .find(|descriptor| descriptor.id.as_str() == "chatgpt")
            .is_some_and(|descriptor| descriptor
                .capabilities
                .contains(&Capability::WorkspaceSelection))
    );

    let claude = ProviderId::new("claude");
    let first_account = registry
        .get_for_account(&claude, "claude-a")
        .expect("first production Claude account");
    let second_account = registry
        .get_for_account(&claude, "claude-b")
        .expect("second production Claude account");
    assert!(!Arc::ptr_eq(&first_account, &second_account));
}

#[cfg(unix)]
#[test]
fn single_binary_contains_daemon_failure_output() {
    use std::os::unix::fs::PermissionsExt;

    let directory = tempfile::tempdir().unwrap();
    let daemon = directory.path().join("noisy-daemon");
    let control_socket = directory.path().join("control.sock");
    std::fs::write(
        &daemon,
        "#!/bin/sh\nprintf 'STDOUT_SENTINEL sensitive\\033[31m\\n'\nprintf 'STDERR_SENTINEL\\n' >&2\nsleep 0.15\nexit 1\n",
    )
    .unwrap();
    std::fs::set_permissions(&daemon, std::fs::Permissions::from_mode(0o700)).unwrap();

    let output = std::process::Command::new(env!("CARGO_BIN_EXE_ullage"))
        .args(["--output", "json", "daemon", "run"])
        .env("ULLAGE_DAEMON_BIN", &daemon)
        .env("ULLAGE_CONTROL_SOCKET", &control_socket)
        .output()
        .unwrap();

    assert!(!output.status.success());
    assert!(output.stdout.is_empty());
    let stderr = String::from_utf8_lossy(&output.stderr);
    let envelope: serde_json::Value = serde_json::from_str(stderr.trim()).unwrap();
    assert_eq!(envelope["error"]["kind"], "daemon_process_failed");
    // The daemon's stderr tail rides along in `message`; its ANSI colors are
    // stripped and its stdout stays out of the report.
    let message = envelope["error"]["message"].as_str().unwrap();
    assert!(message.contains("STDERR_SENTINEL"), "{stderr}");
    assert!(!message.contains("STDOUT_SENTINEL"), "{stderr}");
    assert!(!message.contains('\u{1b}'), "{stderr}");
}

#[cfg(unix)]
#[test]
fn single_binary_rejects_a_daemon_that_exits_during_startup() {
    use std::os::unix::fs::PermissionsExt;

    let directory = tempfile::tempdir().unwrap();
    let daemon = directory.path().join("exiting-daemon");
    let control_socket = directory.path().join("control.sock");
    std::fs::write(&daemon, "#!/bin/sh\nexit 0\n").unwrap();
    std::fs::set_permissions(&daemon, std::fs::Permissions::from_mode(0o700)).unwrap();

    let output = std::process::Command::new(env!("CARGO_BIN_EXE_ullage"))
        .args(["--output", "json", "daemon", "run"])
        .env("ULLAGE_DAEMON_BIN", &daemon)
        .env("ULLAGE_CONTROL_SOCKET", &control_socket)
        .output()
        .unwrap();

    assert!(!output.status.success());
    assert!(output.stdout.is_empty());
    let stderr = String::from_utf8_lossy(&output.stderr);
    let envelope: serde_json::Value = serde_json::from_str(stderr.trim()).unwrap();
    assert_eq!(envelope["error"]["kind"], "daemon_process_failed");
    // With no stderr output the failure still says what happened.
    assert!(
        envelope["error"]["message"]
            .as_str()
            .is_some_and(|message| message.contains("exited during startup")),
        "{stderr}"
    );
}
