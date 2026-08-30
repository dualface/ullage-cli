use serde::{Deserialize, Serialize};
use thiserror::Error;

/// Message stored for snapshots written before partial failures kept a
/// provider-defined scope and a category-normalized message.
pub const LEGACY_PARTIAL_FAILURE_MESSAGE: &str = "provider reported partial data";

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct PartialFailure {
    pub scope: String,
    pub message: String,
}

impl PartialFailure {
    /// Builds a failure whose message is the stable category text, never vendor copy.
    pub fn from_error(scope: impl Into<String>, error: &ProviderError) -> Self {
        Self {
            scope: scope.into(),
            message: error.sanitized_message().to_owned(),
        }
    }
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(tag = "outcome", rename_all = "snake_case")]
pub enum QueryOutcome<T> {
    Complete {
        data: T,
    },
    Partial {
        data: T,
        failures: Vec<PartialFailure>,
    },
}

impl<T> QueryOutcome<T> {
    pub fn try_map<U, E>(
        self,
        convert: impl FnOnce(T) -> Result<U, E>,
    ) -> Result<QueryOutcome<U>, E> {
        match self {
            Self::Complete { data } => convert(data).map(|data| QueryOutcome::Complete { data }),
            Self::Partial { data, failures } => {
                convert(data).map(|data| QueryOutcome::Partial { data, failures })
            }
        }
    }
}

#[derive(Clone, Debug, Error, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum ProviderError {
    #[error("authentication is invalid: {message}")]
    AuthenticationInvalid { message: String },
    #[error("rate limited: {message}")]
    RateLimited {
        message: String,
        retry_after_seconds: Option<u64>,
    },
    #[error("network error: {message}")]
    Network { message: String },
    #[error("provider protocol is incompatible: {message}")]
    ProtocolIncompatible { message: String },
    #[error("capability is not supported: {capability}")]
    UnsupportedCapability { capability: String },
}

impl ProviderError {
    pub fn sanitized_message(&self) -> &'static str {
        match self {
            Self::AuthenticationInvalid { .. } => "provider authentication is invalid",
            Self::RateLimited { .. } => "provider rate limited the request",
            Self::Network { .. } => "provider network request failed",
            Self::ProtocolIncompatible { .. } => "provider protocol response is incompatible",
            Self::UnsupportedCapability { .. } => "provider capability",
        }
    }

    pub fn sanitized(self) -> Self {
        let message = self.sanitized_message().to_owned();
        match self {
            Self::AuthenticationInvalid { .. } => Self::AuthenticationInvalid { message },
            Self::RateLimited {
                retry_after_seconds,
                ..
            } => Self::RateLimited {
                message,
                retry_after_seconds,
            },
            Self::Network { .. } => Self::Network { message },
            Self::ProtocolIncompatible { .. } => Self::ProtocolIncompatible { message },
            Self::UnsupportedCapability { .. } => Self::UnsupportedCapability {
                capability: message,
            },
        }
    }

    pub fn is_sanitized_partial_message(message: &str) -> bool {
        matches!(
            message,
            "provider authentication is invalid"
                | "provider rate limited the request"
                | "provider network request failed"
                | "provider protocol response is incompatible"
                | "provider capability"
        ) || message == LEGACY_PARTIAL_FAILURE_MESSAGE
    }
}

pub type ProviderResult<T> = Result<T, ProviderError>;

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn from_error_uses_the_category_message() {
        let failure = PartialFailure::from_error(
            "profile",
            &ProviderError::ProtocolIncompatible {
                message: "vendor said display_name conflicted".into(),
            },
        );
        assert_eq!(failure.scope, "profile");
        assert_eq!(
            failure.message,
            "provider protocol response is incompatible"
        );
        assert!(!failure.message.contains("vendor"));
    }

    #[test]
    fn sanitized_preserves_rate_limit_retry_delay() {
        let error = ProviderError::RateLimited {
            message: "slow down, token=secret".into(),
            retry_after_seconds: Some(17),
        }
        .sanitized();
        assert_eq!(
            error,
            ProviderError::RateLimited {
                message: "provider rate limited the request".into(),
                retry_after_seconds: Some(17),
            }
        );
    }

    #[test]
    fn legacy_partial_failure_json_still_deserializes() {
        let failure: PartialFailure = serde_json::from_value(serde_json::json!({
            "scope": "provider",
            "message": "provider reported partial data"
        }))
        .unwrap();
        assert_eq!(failure.scope, "provider");
        assert_eq!(failure.message, LEGACY_PARTIAL_FAILURE_MESSAGE);
        assert!(ProviderError::is_sanitized_partial_message(
            &failure.message
        ));
    }
}
