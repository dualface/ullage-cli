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

/// Longest a single account query may run before the daemon gives up. The
/// client's probe-wait read limit stays above this so a legal long query is
/// never reported as a dead daemon.
pub const MAX_ACCOUNT_TIMEOUT: std::time::Duration = std::time::Duration::from_secs(240);

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
    /// Display metric names the account's summary view hides; empty means no
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
    /// Replace the display metric names the account's summary view hides.
    SetAccountMetrics {
        account: AccountId,
        metrics: Vec<String>,
    },
    RemoveAccount {
        account: AccountId,
    },
    // `account_id` stays a bare `String` on `Probe`/`Show`: the wire already
    // carries unvalidated user input here and `AccountId` is just a newtype.
    // Typed ids are a protocol v11 candidate; recorded, not changed.
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
    /// Build version of the daemon that produced the response. Daemons built
    /// before this field existed omit it, so `None` already tells a client it
    /// talks to an older binary.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub daemon_version: Option<String>,
}

impl ControlResponse {
    pub fn new(request_id: impl Into<String>, result: ControlResult) -> Self {
        Self {
            version: CONTROL_PROTOCOL_VERSION,
            request_id: request_id.into(),
            result,
            diagnostic: None,
            daemon_version: Some(env!("CARGO_PKG_VERSION").to_owned()),
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
    /// Display metric names the account stored to hide, read when this
    /// response was built.
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
    /// The request envelope parsed but the command payload did not.
    InvalidRequest,
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
    /// Display metric names the account stored to hide when the snapshot was
    /// read.
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
    AccountNotFound,
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
mod tests;
