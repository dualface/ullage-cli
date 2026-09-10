//! Versioned DTOs for the local daemon control channel.

use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};
use ullage_auth::BackendKind;
pub use ullage_auth::{
    AuthChallenge, AuthCompleteRequest, AuthInputRequest, AuthMethod, AuthStartRequest, AuthState,
    LogoutRequest,
};
pub use ullage_core::{
    Capability, MeasurementUnit, PartialFailure, ProviderDescriptor, ProviderError, ProviderId,
    ProviderWorkspace, QueryOutcome, RegistryError, SubscriptionUsage, UsageMeasurement,
    UsageQuery, UsageWindow, UsageWindowKind,
};

pub const CONTROL_PROTOCOL_VERSION: u16 = 10;

#[derive(Clone, Debug, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize)]
#[serde(transparent)]
pub struct AccountId(String);

impl AccountId {
    pub fn new(value: impl Into<String>) -> Self {
        Self(value.into())
    }

    pub fn as_str(&self) -> &str {
        &self.0
    }
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct Account {
    pub id: AccountId,
    pub provider: ProviderId,
    pub label: Option<String>,
    pub enabled: bool,
    /// Display metric names the account's summary view keeps; empty means no
    /// filter.
    #[serde(default)]
    pub metrics: Vec<String>,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "kind", content = "account", rename_all = "snake_case")]
pub enum AccountError {
    NotFound(AccountId),
    Duplicate(AccountId),
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct ControlRequest {
    pub version: u16,
    pub request_id: String,
    pub command: ControlCommand,
    /// Opt-in for diagnostic output on this response.
    ///
    /// The daemon attaches unsanitized provider error text only for
    /// authentication and probe errors. Show uses the same flag so the CLI
    /// can display sanitized partial-failure details.
    #[serde(default)]
    pub diagnostics: bool,
}

impl ControlRequest {
    pub fn new(request_id: impl Into<String>, command: ControlCommand) -> Self {
        Self {
            version: CONTROL_PROTOCOL_VERSION,
            request_id: request_id.into(),
            command,
            diagnostics: false,
        }
    }

    #[must_use]
    pub fn with_diagnostics(mut self, diagnostics: bool) -> Self {
        self.diagnostics = diagnostics;
        self
    }
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "command", rename_all = "snake_case")]
pub enum ControlCommand {
    DaemonStatus,
    CreatePairCode,
    ListDevices,
    RevokeDevice {
        device_id: String,
    },
    ListProviders,
    AddAccount {
        provider: ProviderId,
        label: Option<String>,
    },
    ListAccounts,
    ShowAccount {
        account: AccountId,
    },
    SetAccountEnabled {
        account: AccountId,
        enabled: bool,
    },
    SetAccountLabel {
        account: AccountId,
        label: Option<String>,
    },
    SetAccountMetrics {
        account: AccountId,
        metrics: Vec<String>,
    },
    RemoveAccount {
        account: AccountId,
    },
    Probe {
        account_id: String,
        wait: bool,
    },
    Show {
        account_id: Option<String>,
    },
    QueryUsage {
        provider: ProviderId,
        query: UsageQuery,
    },
    StartAuth {
        provider: ProviderId,
        account: AccountId,
        request: AuthStartRequest,
    },
    CompleteAuth {
        provider: ProviderId,
        account: AccountId,
        request: AuthCompleteRequest,
    },
    AuthStatus {
        provider: ProviderId,
        account: AccountId,
    },
    Logout {
        provider: ProviderId,
        account: AccountId,
        request: LogoutRequest,
    },
    ListWorkspaces {
        provider: ProviderId,
        account: AccountId,
    },
    SelectWorkspace {
        provider: ProviderId,
        account: AccountId,
        workspace_id: String,
    },
    /// Removes the accounts of `provider` that are signed in as the same
    /// identity as `account`, which is asked for once `account` is fully set up.
    /// Doing it then rather than at sign-in means nothing is deleted until its
    /// replacement is known to work.
    RetireDuplicateAccounts {
        provider: ProviderId,
        account: AccountId,
    },
}

impl ControlCommand {
    /// Whether this command may request diagnostics.
    ///
    /// Authentication and probe errors may attach unsanitized provider text on
    /// `ControlResponse::diagnostic`. Show and probe also use this opt-in so
    /// the CLI can display sanitized partial-failure scope and category.
    pub fn accepts_diagnostics(&self) -> bool {
        matches!(
            self,
            Self::StartAuth { .. }
                | Self::CompleteAuth { .. }
                | Self::AuthStatus { .. }
                | Self::Logout { .. }
                | Self::Probe { .. }
                | Self::Show { .. }
        )
    }
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct ControlResponse {
    pub version: u16,
    pub request_id: String,
    pub result: ControlResult,
    /// Unsanitized provider error detail, present only when the request asked
    /// for diagnostics and the result is an error.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub diagnostic: Option<String>,
}

impl ControlResponse {
    pub fn new(request_id: impl Into<String>, result: ControlResult) -> Self {
        Self {
            version: CONTROL_PROTOCOL_VERSION,
            request_id: request_id.into(),
            result,
            diagnostic: None,
        }
    }

    #[must_use]
    pub fn with_diagnostic(mut self, diagnostic: Option<String>) -> Self {
        self.diagnostic = diagnostic;
        self
    }
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(tag = "result", content = "payload", rename_all = "snake_case")]
pub enum ControlResult {
    DaemonStatus(DaemonStatusPayload),
    PairCode(PairCodePayload),
    Devices(Vec<DevicePayload>),
    Providers(Vec<ProviderDescriptor>),
    Accounts(Vec<Account>),
    Account(Account),
    Usage(QueryOutcome<SubscriptionUsage>),
    Probe(ProbePayload),
    Snapshots(Vec<SnapshotPayload>),
    AuthChallenge(AuthChallenge),
    AuthState(AuthState),
    Workspaces(Vec<ProviderWorkspace>),
    Workspace(ProviderWorkspace),
    Ack,
    Error(ControlError),
    ProtocolMismatch { supported_version: u16 },
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct ProbePayload {
    pub account_id: String,
    pub usage: QueryOutcome<SubscriptionUsage>,
    /// Display metric names stored for the account when the probe ran.
    #[serde(default)]
    pub metrics: Vec<String>,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct PairCodePayload {
    pub code: String,
    pub expires_at: DateTime<Utc>,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct DevicePayload {
    pub id: String,
    pub name: String,
    pub created_at: DateTime<Utc>,
    pub last_seen_at: DateTime<Utc>,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "kind", content = "detail", rename_all = "snake_case")]
pub enum ControlError {
    Account(AccountError),
    Provider(ProviderError),
    Registry(RegistryError),
    AccountNotFound {
        account_id: String,
    },
    AccountSelectorNotFound {
        provider: ProviderId,
        account_label: Option<String>,
    },
    DeviceNotFound {
        device_id: String,
    },
    /// The supplied display metric filter was rejected by validation.
    InvalidAccountMetrics,
    Timeout,
    Cancelled,
    Storage,
    UnsupportedCommand,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum CredentialBackendId {
    LinuxSecretService,
    MacosKeychain,
    WindowsCredentialManager,
    FileFallback,
    OtherPlatform,
}

impl CredentialBackendId {
    pub fn native() -> Self {
        Self::from(if cfg!(target_os = "macos") {
            BackendKind::MacOsKeychain
        } else if cfg!(target_os = "windows") {
            BackendKind::WindowsCredentialManager
        } else if cfg!(target_os = "linux") {
            BackendKind::LinuxSecretService
        } else {
            BackendKind::OtherPlatform
        })
    }

    pub fn as_str(self) -> &'static str {
        match self {
            Self::LinuxSecretService => "linux_secret_service",
            Self::MacosKeychain => "macos_keychain",
            Self::WindowsCredentialManager => "windows_credential_manager",
            Self::FileFallback => "file_fallback",
            Self::OtherPlatform => "other_platform",
        }
    }
}

impl From<BackendKind> for CredentialBackendId {
    fn from(kind: BackendKind) -> Self {
        match kind {
            BackendKind::MacOsKeychain => Self::MacosKeychain,
            BackendKind::WindowsCredentialManager => Self::WindowsCredentialManager,
            BackendKind::LinuxSecretService => Self::LinuxSecretService,
            BackendKind::ExplicitFileFallback => Self::FileFallback,
            BackendKind::OtherPlatform => Self::OtherPlatform,
        }
    }
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct DaemonStatusPayload {
    pub shutting_down: bool,
    pub accounts: Vec<AccountStatusPayload>,
    pub credential_backend: CredentialBackendId,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct AccountStatusPayload {
    pub account_id: String,
    pub provider: ProviderId,
    pub enabled: bool,
    pub in_flight: bool,
    pub consecutive_failures: u32,
    pub next_probe_at: Option<DateTime<Utc>>,
    pub has_snapshot: bool,
    pub stale: bool,
    pub last_error: Option<SanitizedErrorPayload>,
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct SnapshotPayload {
    pub account_id: String,
    pub usage: QueryOutcome<SubscriptionUsage>,
    pub last_success_at: DateTime<Utc>,
    pub stale: bool,
    pub last_error: Option<SanitizedErrorPayload>,
    pub last_error_at: Option<DateTime<Utc>>,
    /// Display metric names stored for the account when the snapshot was read.
    #[serde(default)]
    pub metrics: Vec<String>,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum SanitizedErrorPayload {
    AuthenticationInvalid,
    RateLimited { retry_after_seconds: Option<u64> },
    Network,
    ProtocolIncompatible,
    UnsupportedCapability,
    Timeout,
    Cancelled,
    ProviderNotFound,
    Storage,
}

impl From<AccountError> for ControlError {
    fn from(error: AccountError) -> Self {
        Self::Account(error)
    }
}

impl From<ProviderError> for ControlError {
    fn from(error: ProviderError) -> Self {
        Self::Provider(error)
    }
}

impl From<RegistryError> for ControlError {
    fn from(error: RegistryError) -> Self {
        Self::Registry(error)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn request_uses_the_current_protocol_version() {
        let request = ControlRequest::new("request-1", ControlCommand::ListProviders);
        let json = serde_json::to_string(&request).unwrap();
        let decoded: ControlRequest = serde_json::from_str(&json).unwrap();

        assert_eq!(decoded.version, CONTROL_PROTOCOL_VERSION);
        assert_eq!(decoded, request);
    }

    #[test]
    fn partial_usage_preserves_data_and_failures() {
        let observed_at = chrono::DateTime::parse_from_rfc3339("2026-08-27T12:00:00Z")
            .unwrap()
            .to_utc();
        let response = ControlResponse {
            version: CONTROL_PROTOCOL_VERSION,
            request_id: "request-2".into(),
            diagnostic: None,
            result: ControlResult::Usage(QueryOutcome::Partial {
                data: SubscriptionUsage {
                    provider: ProviderId::new("test"),
                    account_label: None,
                    plan: None,
                    subscription_expires_at: None,
                    observed_at,
                    windows: Vec::new(),
                },
                failures: vec![ullage_core::PartialFailure {
                    scope: "weekly".into(),
                    message: "temporarily unavailable".into(),
                }],
            }),
        };

        let json = serde_json::to_value(&response).unwrap();
        assert_eq!(
            json,
            serde_json::json!({
                "version": CONTROL_PROTOCOL_VERSION,
                "request_id": "request-2",
                "result": {
                    "result": "usage",
                    "payload": {
                        "outcome": "partial",
                        "data": {
                            "provider": "test",
                            "account_label": null,
                            "plan": null,
                            "subscription_expires_at": null,
                            "observed_at": "2026-08-27T12:00:00Z",
                            "windows": []
                        },
                        "failures": [{
                            "scope": "weekly",
                            "message": "temporarily unavailable"
                        }]
                    }
                }
            })
        );
        assert_eq!(
            serde_json::from_value::<ControlResponse>(json).unwrap(),
            response
        );
    }

    #[test]
    fn daemon_commands_and_auth_challenge_round_trip() {
        let commands = [
            ControlCommand::DaemonStatus,
            ControlCommand::Probe {
                account_id: "primary".into(),
                wait: true,
            },
            ControlCommand::Show { account_id: None },
            ControlCommand::SetAccountMetrics {
                account: AccountId::new("account-1"),
                metrics: vec!["usage".into(), "Codex".into()],
            },
        ];
        for (index, command) in commands.into_iter().enumerate() {
            let request = ControlRequest::new(format!("daemon-{index}"), command);
            let encoded = serde_json::to_string(&request).unwrap();
            assert_eq!(
                serde_json::from_str::<ControlRequest>(&encoded).unwrap(),
                request
            );
        }

        let response = ControlResponse {
            version: CONTROL_PROTOCOL_VERSION,
            request_id: "auth-challenge".into(),
            diagnostic: None,
            result: ControlResult::AuthChallenge(AuthChallenge {
                flow_id: "flow-1".into(),
                method: ullage_auth::AuthMethod::DeviceCode,
                verification_uri: Some("https://example.invalid/device".into()),
                user_code: Some("code-1".into()),
                expires_at: None,
                input: None,
            }),
        };
        let encoded = serde_json::to_string(&response).unwrap();
        assert_eq!(
            serde_json::from_str::<ControlResponse>(&encoded).unwrap(),
            response
        );

        for (index, error) in [
            ControlError::Timeout,
            ControlError::Cancelled,
            ControlError::Storage,
            ControlError::InvalidAccountMetrics,
            ControlError::AccountSelectorNotFound {
                provider: ProviderId::new("test"),
                account_label: Some("secondary".into()),
            },
        ]
        .into_iter()
        .enumerate()
        {
            let response = ControlResponse {
                version: CONTROL_PROTOCOL_VERSION,
                request_id: format!("daemon-error-{index}"),
                result: ControlResult::Error(error),
                diagnostic: Some("provider detail".into()),
            };
            let encoded = serde_json::to_string(&response).unwrap();
            assert_eq!(
                serde_json::from_str::<ControlResponse>(&encoded).unwrap(),
                response
            );
        }
    }

    #[test]
    fn account_and_payload_metrics_round_trip_and_default_to_empty() {
        let account = Account {
            id: AccountId::new("account-1"),
            provider: ProviderId::new("claude"),
            label: None,
            enabled: true,
            metrics: vec!["usage".into(), "Codex".into()],
        };
        let json = serde_json::to_value(&account).unwrap();
        assert_eq!(json["metrics"], serde_json::json!(["usage", "Codex"]));
        assert_eq!(serde_json::from_value::<Account>(json).unwrap(), account);

        let legacy_account: Account = serde_json::from_value(serde_json::json!({
            "id": "account-1",
            "provider": "claude",
            "label": null,
            "enabled": true
        }))
        .unwrap();
        assert!(legacy_account.metrics.is_empty());

        let observed_at = chrono::DateTime::parse_from_rfc3339("2026-08-27T12:00:00Z")
            .unwrap()
            .to_utc();
        let usage = SubscriptionUsage {
            provider: ProviderId::new("claude"),
            account_label: None,
            plan: None,
            subscription_expires_at: None,
            observed_at,
            windows: Vec::new(),
        };
        let snapshot = SnapshotPayload {
            account_id: "account-1".into(),
            usage: QueryOutcome::Complete {
                data: usage.clone(),
            },
            last_success_at: observed_at,
            stale: false,
            last_error: None,
            last_error_at: None,
            metrics: vec!["usage".into()],
        };
        let json = serde_json::to_value(&snapshot).unwrap();
        assert_eq!(json["metrics"], serde_json::json!(["usage"]));
        assert_eq!(
            serde_json::from_value::<SnapshotPayload>(json.clone()).unwrap(),
            snapshot
        );
        let mut legacy = json;
        legacy.as_object_mut().unwrap().remove("metrics");
        assert!(
            serde_json::from_value::<SnapshotPayload>(legacy)
                .unwrap()
                .metrics
                .is_empty()
        );

        let probe = ProbePayload {
            account_id: "account-1".into(),
            usage: QueryOutcome::Complete { data: usage },
            metrics: vec!["Codex".into()],
        };
        let json = serde_json::to_value(&probe).unwrap();
        assert_eq!(json["metrics"], serde_json::json!(["Codex"]));
        assert_eq!(
            serde_json::from_value::<ProbePayload>(json.clone()).unwrap(),
            probe
        );
        let mut legacy = json;
        legacy.as_object_mut().unwrap().remove("metrics");
        assert!(
            serde_json::from_value::<ProbePayload>(legacy)
                .unwrap()
                .metrics
                .is_empty()
        );
    }

    #[test]
    fn daemon_status_serializes_the_credential_backend() {
        let response = ControlResponse {
            version: CONTROL_PROTOCOL_VERSION,
            request_id: "status-1".into(),
            diagnostic: None,
            result: ControlResult::DaemonStatus(DaemonStatusPayload {
                shutting_down: false,
                accounts: Vec::new(),
                credential_backend: CredentialBackendId::FileFallback,
            }),
        };
        let json = serde_json::to_value(&response).unwrap();
        assert_eq!(
            json["result"]["payload"]["credential_backend"],
            "file_fallback"
        );
        assert_eq!(
            serde_json::from_value::<ControlResponse>(json).unwrap(),
            response
        );
    }

    #[test]
    fn authentication_probe_and_show_commands_accept_diagnostics() {
        assert!(
            ControlCommand::AuthStatus {
                provider: ProviderId::new("claude"),
                account: AccountId::new("account-1"),
            }
            .accepts_diagnostics()
        );
        assert!(
            ControlCommand::Probe {
                account_id: "account-1".into(),
                wait: true,
            }
            .accepts_diagnostics()
        );
        assert!(
            ControlCommand::Show {
                account_id: Some("account-1".into()),
            }
            .accepts_diagnostics()
        );
        assert!(ControlCommand::Show { account_id: None }.accepts_diagnostics());
        assert!(!ControlCommand::ListProviders.accepts_diagnostics());
        assert!(
            !ControlCommand::QueryUsage {
                provider: ProviderId::new("claude"),
                query: UsageQuery::default(),
            }
            .accepts_diagnostics()
        );
    }

    #[test]
    fn diagnostics_are_opt_in_and_omitted_by_default() {
        let request = ControlRequest::new("request-3", ControlCommand::ListProviders);
        let encoded = serde_json::to_value(&request).unwrap();
        assert_eq!(encoded["diagnostics"], false);
        let decoded: ControlRequest = serde_json::from_value(serde_json::json!({
            "version": CONTROL_PROTOCOL_VERSION,
            "request_id": "legacy",
            "command": { "command": "list_providers" }
        }))
        .unwrap();
        assert!(!decoded.diagnostics);

        let response = ControlResponse::new("request-3", ControlResult::Ack);
        let encoded = serde_json::to_value(&response).unwrap();
        assert!(encoded.get("diagnostic").is_none());
        let with_detail = response.with_diagnostic(Some("provider detail".into()));
        assert_eq!(
            serde_json::to_value(&with_detail).unwrap()["diagnostic"],
            "provider detail"
        );
    }

    #[test]
    fn registry_not_found_is_a_control_error() {
        let error = ControlError::from(RegistryError::NotFound(ProviderId::new("missing")));
        let json = serde_json::to_string(&error).unwrap();

        assert_eq!(serde_json::from_str::<ControlError>(&json).unwrap(), error);
    }
}
