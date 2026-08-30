//! Provider-independent authentication request and state types.

mod credential;
mod file_store;
mod native_store;
mod store;
#[cfg(any(windows, test))]
mod windows_identity;

pub use credential::{Credential, CredentialKey, CredentialVersion, SecretValue, StoredCredential};
pub use file_store::{FileFallbackOptions, FileStore};
#[cfg(windows)]
pub use file_store::{
    create_private_windows_directory, create_private_windows_file, windows_handle_acl_is_private,
};
pub use native_store::NativeStore;
pub use store::{
    Availability, BackendKind, BackendScope, CredentialBackend, CredentialError, CredentialStore,
    RefreshError, RefreshFailure, RefreshFailureKind, ReplaceOutcome,
};
#[cfg(windows)]
pub use windows_identity::{current_windows_user_scope, new_windows_service_nonce};

#[cfg(test)]
mod credential_store_tests;

use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum AuthMethod {
    BrowserOAuth,
    DeviceCode,
    ApiToken,
    SessionImport,
    Other(String),
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct AuthStartRequest {
    pub method: Option<AuthMethod>,
}

/// Value the user must hand back to finish a flow, described by the provider so
/// that clients do not have to hard-code per-provider callback formats.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct AuthInputRequest {
    /// Human-readable description of what to paste back.
    pub prompt: String,
    /// The value is a long-lived secret and must never be echoed.
    pub secret: bool,
}

impl AuthInputRequest {
    pub fn visible(prompt: impl Into<String>) -> Self {
        Self {
            prompt: prompt.into(),
            secret: false,
        }
    }

    pub fn secret(prompt: impl Into<String>) -> Self {
        Self {
            prompt: prompt.into(),
            secret: true,
        }
    }
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct AuthChallenge {
    pub flow_id: String,
    pub method: AuthMethod,
    pub verification_uri: Option<String>,
    pub user_code: Option<String>,
    pub expires_at: Option<DateTime<Utc>>,
    /// `None` when the flow completes without the user pasting anything back.
    #[serde(default)]
    pub input: Option<AuthInputRequest>,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct AuthCompleteRequest {
    pub flow_id: String,
    pub authorization_code: Option<String>,
    pub redirect_uri: Option<String>,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "state", rename_all = "snake_case")]
pub enum AuthState {
    NotAuthenticated,
    Pending {
        flow_id: String,
        expires_at: Option<DateTime<Utc>>,
    },
    Authenticated {
        account_label: Option<String>,
        expires_at: Option<DateTime<Utc>>,
    },
    Invalid {
        reason: String,
    },
}

#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct LogoutRequest {
    pub account_label: Option<String>,
}
