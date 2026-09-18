use std::fmt;
use std::time::Duration;

use async_trait::async_trait;
use reqwest::{Client, StatusCode};
use serde::de::DeserializeOwned;
use ullage_core::ProviderError;

use crate::dto::UsageResponse;

const DEFAULT_API_BASE: &str = "https://opencode.ai";
const USAGE_PATH: &str = "/zen/go/v1/usage";

/// Responses larger than this are rejected before deserialization.
const MAX_RESPONSE_BYTES: usize = 2 * 1024 * 1024;

#[derive(Clone, Debug, PartialEq, Eq)]
pub enum ApiFailureKind {
    Authentication,
    /// The key is valid but the account holds no OpenCode Go subscription.
    /// Zen and Go share one key, so a 403 is not a credential failure.
    NoSubscription,
    RateLimit,
    Network,
    Protocol,
}

#[derive(Clone, PartialEq, Eq)]
pub struct ApiFailure {
    pub kind: ApiFailureKind,
    pub message: String,
    pub retry_after_seconds: Option<u64>,
}

impl fmt::Debug for ApiFailure {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter
            .debug_struct("ApiFailure")
            .field("kind", &self.kind)
            .field("message", &"[REDACTED]")
            .field("retry_after_seconds", &self.retry_after_seconds)
            .finish()
    }
}

impl ApiFailure {
    pub fn authentication(message: impl Into<String>) -> Self {
        Self {
            kind: ApiFailureKind::Authentication,
            message: message.into(),
            retry_after_seconds: None,
        }
    }

    pub fn protocol(message: impl Into<String>) -> Self {
        Self {
            kind: ApiFailureKind::Protocol,
            message: message.into(),
            retry_after_seconds: None,
        }
    }

    pub fn into_provider_error(self) -> ProviderError {
        match self.kind {
            ApiFailureKind::Authentication => ProviderError::AuthenticationInvalid {
                message: self.message,
            },
            ApiFailureKind::NoSubscription => ProviderError::UnsupportedCapability {
                capability: "opencode-go subscription".into(),
            },
            ApiFailureKind::RateLimit => ProviderError::RateLimited {
                message: self.message,
                retry_after_seconds: self.retry_after_seconds,
            },
            ApiFailureKind::Network => ProviderError::Network {
                message: self.message,
            },
            ApiFailureKind::Protocol => ProviderError::ProtocolIncompatible {
                message: self.message,
            },
        }
    }
}

#[async_trait]
pub trait OpencodeApi: Send + Sync {
    /// `GET /zen/go/v1/usage` with the pasted key as the bearer token. The
    /// same call validates a key at sign-in and polls usage afterwards.
    async fn usage(&self, api_key: &str) -> Result<UsageResponse, ApiFailure>;
}

#[derive(Clone)]
pub struct HttpOpencodeApi {
    client: Client,
    api_base: String,
}

impl HttpOpencodeApi {
    pub fn new() -> Result<Self, ApiFailure> {
        Self::with_base_url(DEFAULT_API_BASE)
    }

    pub fn with_base_url(api_base: impl Into<String>) -> Result<Self, ApiFailure> {
        let api_base = api_base.into().trim_end_matches('/').to_owned();
        let url = reqwest::Url::parse(&api_base)
            .map_err(|_| ApiFailure::protocol("OpenCode API base URL is not a valid URL"))?;
        if !is_secure_or_loopback(&url) {
            return Err(ApiFailure::protocol(
                "OpenCode API base URL must use HTTPS or loopback HTTP",
            ));
        }
        let client = Client::builder()
            .timeout(Duration::from_secs(30))
            // Redirects are refused outright: a bearer token must never be
            // forwarded to a host the server picked.
            .redirect(reqwest::redirect::Policy::none())
            .build()
            .map_err(|_| ApiFailure {
                kind: ApiFailureKind::Network,
                message: "failed to construct OpenCode HTTP client".into(),
                retry_after_seconds: None,
            })?;
        Ok(Self { client, api_base })
    }
}

#[async_trait]
impl OpencodeApi for HttpOpencodeApi {
    async fn usage(&self, api_key: &str) -> Result<UsageResponse, ApiFailure> {
        let response = self
            .client
            .get(format!("{}{USAGE_PATH}", self.api_base))
            .bearer_auth(api_key)
            .send()
            .await
            .map_err(network_failure)?;
        decode_response(response, "OpenCode Go usage").await
    }
}

async fn decode_response<T: DeserializeOwned>(
    response: reqwest::Response,
    operation: &str,
) -> Result<T, ApiFailure> {
    let status = response.status();
    let retry_after_seconds = response
        .headers()
        .get(reqwest::header::RETRY_AFTER)
        .and_then(|value| value.to_str().ok())
        .and_then(|value| value.parse().ok());
    if let Some(error) = classify_status(status, operation, retry_after_seconds) {
        return Err(error);
    }
    let body = bounded_body(response, operation).await?;
    serde_json::from_slice(&body)
        .map_err(|_| ApiFailure::protocol(format!("{operation} returned incompatible JSON")))
}

async fn bounded_body(
    mut response: reqwest::Response,
    operation: &str,
) -> Result<Vec<u8>, ApiFailure> {
    if response
        .content_length()
        .is_some_and(|length| length > MAX_RESPONSE_BYTES as u64)
    {
        return Err(ApiFailure::protocol(format!(
            "{operation} response exceeds the size limit"
        )));
    }
    let mut body = Vec::new();
    while let Some(chunk) = response.chunk().await.map_err(network_failure)? {
        if body.len().saturating_add(chunk.len()) > MAX_RESPONSE_BYTES {
            return Err(ApiFailure::protocol(format!(
                "{operation} response exceeds the size limit"
            )));
        }
        body.extend_from_slice(&chunk);
    }
    Ok(body)
}

fn classify_status(
    status: StatusCode,
    operation: &str,
    retry_after_seconds: Option<u64>,
) -> Option<ApiFailure> {
    if status.is_success() {
        return None;
    }
    if status == StatusCode::UNAUTHORIZED {
        return Some(ApiFailure::authentication(format!(
            "{operation} rejected the credential"
        )));
    }
    if status == StatusCode::FORBIDDEN {
        return Some(ApiFailure {
            kind: ApiFailureKind::NoSubscription,
            message: format!("{operation} requires an OpenCode Go subscription"),
            retry_after_seconds: None,
        });
    }
    if status == StatusCode::TOO_MANY_REQUESTS {
        return Some(ApiFailure {
            kind: ApiFailureKind::RateLimit,
            message: format!("{operation} was rate limited"),
            retry_after_seconds,
        });
    }
    if status == StatusCode::REQUEST_TIMEOUT || status.is_server_error() {
        return Some(ApiFailure {
            kind: ApiFailureKind::Network,
            message: format!("{operation} returned HTTP {status}"),
            retry_after_seconds: None,
        });
    }
    Some(ApiFailure::protocol(format!(
        "{operation} returned unexpected HTTP {status}"
    )))
}

fn is_secure_or_loopback(url: &reqwest::Url) -> bool {
    let secure = url.scheme() == "https" && url.host().is_some();
    let loopback = url.scheme() == "http"
        && url.host_str().is_some_and(|host| {
            let host = host.trim_start_matches('[').trim_end_matches(']');
            host == "localhost"
                || host
                    .parse::<std::net::IpAddr>()
                    .is_ok_and(|ip| ip.is_loopback())
        });
    secure || loopback
}

fn network_failure(_: reqwest::Error) -> ApiFailure {
    ApiFailure {
        kind: ApiFailureKind::Network,
        message: "OpenCode request failed before a response was received".into(),
        retry_after_seconds: None,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn classifies_usage_http_status() {
        assert_eq!(
            classify_status(StatusCode::UNAUTHORIZED, "usage", None)
                .unwrap()
                .kind,
            ApiFailureKind::Authentication
        );
        assert_eq!(
            classify_status(StatusCode::FORBIDDEN, "usage", None)
                .unwrap()
                .kind,
            ApiFailureKind::NoSubscription
        );
        assert_eq!(
            classify_status(StatusCode::REQUEST_TIMEOUT, "usage", None)
                .unwrap()
                .kind,
            ApiFailureKind::Network
        );
        assert_eq!(
            classify_status(StatusCode::NOT_FOUND, "usage", None)
                .unwrap()
                .kind,
            ApiFailureKind::Protocol
        );
        assert_eq!(
            classify_status(StatusCode::BAD_REQUEST, "usage", None)
                .unwrap()
                .kind,
            ApiFailureKind::Protocol
        );
        let limited = classify_status(StatusCode::TOO_MANY_REQUESTS, "usage", Some(9)).unwrap();
        assert_eq!(limited.kind, ApiFailureKind::RateLimit);
        assert_eq!(limited.retry_after_seconds, Some(9));
        assert_eq!(
            classify_status(StatusCode::SERVICE_UNAVAILABLE, "usage", None)
                .unwrap()
                .kind,
            ApiFailureKind::Network
        );
        assert!(classify_status(StatusCode::OK, "usage", None).is_none());
    }

    #[test]
    fn no_subscription_maps_to_unsupported_capability() {
        let error = ApiFailure {
            kind: ApiFailureKind::NoSubscription,
            message: "requires an OpenCode Go subscription".into(),
            retry_after_seconds: None,
        }
        .into_provider_error();
        assert_eq!(
            error,
            ProviderError::UnsupportedCapability {
                capability: "opencode-go subscription".into(),
            }
        );
    }
}
