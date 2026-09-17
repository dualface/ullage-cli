//! Provider-independent authentication request and state types.

mod credential;
mod file_store;
mod native_store;
mod redirect;
mod store;
#[cfg(any(windows, test))]
mod windows_identity;
#[cfg(windows)]
mod windows_security;

pub use credential::{Credential, CredentialKey, CredentialVersion, SecretValue, StoredCredential};
pub use file_store::{FileFallbackOptions, FileStore};
pub use native_store::NativeStore;
pub use redirect::validate_loopback_http_redirect_uri;
#[cfg(windows)]
pub use windows_security::{
    create_private_windows_directory, create_private_windows_file, windows_handle_acl_is_private,
};

/// Opaque form of a provider's account identity.
///
/// The raw value is an email address or a provider account id, which names a
/// person. Nothing reads it: accounts are only ever compared for equality, so
/// hashing keeps that comparison working while keeping the identity itself out
/// of the control protocol, logs and terminal output. Trimming and lowercasing
/// first makes the comparison insensitive to how a provider spells it back.
///
/// `None` for a value that is empty once trimmed, which names no account and
/// must therefore never match another.
pub fn account_identity(value: &str) -> Option<String> {
    let value = value.trim().to_lowercase();
    if value.is_empty() {
        return None;
    }
    Some(format!("{:x}", sha2::Sha256::digest(value.as_bytes())))
}
pub use store::{
    Availability, BackendKind, BackendScope, CredentialBackend, CredentialError, CredentialStore,
    RefreshError, RefreshFailure, RefreshFailureKind, ReplaceOutcome,
};
#[cfg(windows)]
pub use windows_identity::{current_windows_user_scope, new_windows_service_nonce};

#[cfg(test)]
mod identity_tests {
    use super::account_identity;

    #[test]
    fn identity_hides_the_value_while_still_matching_the_same_account() {
        let key = account_identity("User@Example.Test").unwrap();
        // Spelling differences a provider may introduce must still match.
        assert_eq!(account_identity(" user@example.test ").as_ref(), Some(&key));
        // And the address itself must not survive into what anything can read.
        assert!(!key.contains("example"));
        assert_eq!(key.len(), 64);
        assert!(key.chars().all(|c| c.is_ascii_hexdigit()));

        assert_ne!(account_identity("other@example.test").unwrap(), key);
        assert_eq!(account_identity("   "), None);
        assert_eq!(account_identity(""), None);
    }
}

#[cfg(test)]
mod credential_store_tests;

use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};
use sha2::Digest;

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum AuthMethod {
    BrowserOAuth,
    DeviceCode,
    ApiToken,
    SessionImport,
    Other(String),
}

#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct AuthStartRequest {
    pub method: Option<AuthMethod>,
    /// Loopback HTTP callback chosen by the local client for this flow.
    ///
    /// Omitted on older clients and ignored on the remote HTTP transport.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub redirect_uri: Option<String>,
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
        /// Display name for the account, which the user may override.
        account_label: Option<String>,
        /// Stable identity of whoever is signed in, as the provider reports it.
        /// Unlike the label this is never user-chosen, so it is what tells two
        /// accounts holding the same upstream sign-in apart from two distinct
        /// ones that happen to share a name. `None` means the provider cannot
        /// report an identity, and no such comparison is possible.
        #[serde(default)]
        account_key: Option<String>,
        expires_at: Option<DateTime<Utc>>,
    },
    Invalid {
        reason: String,
        /// Identity of the account whose credential went invalid, when the
        /// provider still knows it. An expired sign-in is still that account's
        /// sign-in, so a fresh one for the same identity supersedes it.
        #[serde(default)]
        account_key: Option<String>,
    },
}

#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct LogoutRequest {
    pub account_label: Option<String>,
}
