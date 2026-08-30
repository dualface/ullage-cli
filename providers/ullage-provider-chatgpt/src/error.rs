use std::fmt;

use ullage_core::ProviderError;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum ChatGptApiErrorKind {
    AuthenticationInvalid,
    WorkspaceAccessDenied,
    RateLimited,
    Network,
    ProtocolIncompatible,
}

#[derive(Clone, PartialEq, Eq)]
pub struct ChatGptApiError {
    pub kind: ChatGptApiErrorKind,
    pub message: String,
    pub retry_after_seconds: Option<u64>,
}

impl ChatGptApiError {
    pub fn new(kind: ChatGptApiErrorKind, message: impl Into<String>) -> Self {
        Self {
            kind,
            message: message.into(),
            retry_after_seconds: None,
        }
    }

    pub fn rate_limited(message: impl Into<String>, retry_after_seconds: Option<u64>) -> Self {
        Self {
            kind: ChatGptApiErrorKind::RateLimited,
            message: message.into(),
            retry_after_seconds,
        }
    }
}

impl fmt::Debug for ChatGptApiError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter
            .debug_struct("ChatGptApiError")
            .field("kind", &self.kind)
            .field("message", &"[REDACTED]")
            .field("retry_after_seconds", &self.retry_after_seconds)
            .finish()
    }
}

impl From<ChatGptApiError> for ProviderError {
    fn from(error: ChatGptApiError) -> Self {
        match error.kind {
            ChatGptApiErrorKind::AuthenticationInvalid => Self::AuthenticationInvalid {
                message: error.message,
            },
            ChatGptApiErrorKind::WorkspaceAccessDenied => Self::AuthenticationInvalid {
                message: format!("workspace access denied: {}", error.message),
            },
            ChatGptApiErrorKind::RateLimited => Self::RateLimited {
                message: error.message,
                retry_after_seconds: error.retry_after_seconds,
            },
            ChatGptApiErrorKind::Network => Self::Network {
                message: error.message,
            },
            ChatGptApiErrorKind::ProtocolIncompatible => Self::ProtocolIncompatible {
                message: error.message,
            },
        }
    }
}
