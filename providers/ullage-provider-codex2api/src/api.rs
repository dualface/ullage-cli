use std::fmt;
use std::time::Duration;

use async_trait::async_trait;
use reqwest::{Client, StatusCode};
use serde::de::DeserializeOwned;
use ullage_core::ProviderError;

use crate::dto::{AccountsResponse, UsageRefreshResponse};

const ACCOUNTS_PATH: &str = "/api/admin/accounts";

/// Responses larger than this are rejected before deserialization.
const MAX_RESPONSE_BYTES: usize = 2 * 1024 * 1024;

/// The admin key travels in this header; the gateway also accepts a bearer
/// token, but the dedicated header keeps the credential out of generic
/// authorization handling.
const ADMIN_KEY_HEADER: &str = "X-Admin-Key";

#[derive(Clone, Debug, PartialEq, Eq)]
pub enum ApiFailureKind {
    /// 401/403, or the bound upstream account no longer exists in the list:
    /// the credential cannot produce data and the session must go Invalid.
    Authentication,
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

    pub fn network(message: impl Into<String>) -> Self {
        Self {
            kind: ApiFailureKind::Network,
            message: message.into(),
            retry_after_seconds: None,
        }
    }

    pub fn into_provider_error(self) -> ProviderError {
        match self.kind {
            ApiFailureKind::Authentication => ProviderError::AuthenticationInvalid {
                message: self.message,
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
pub trait Codex2apiApi: Send + Sync {
    /// `GET /api/admin/accounts` with the admin key. The response embeds every
    /// upstream account's quota fields; the call also triggers the gateway's
    /// debounced background usage probe.
    async fn list_accounts(
        &self,
        base_url: &str,
        admin_key: &str,
    ) -> Result<AccountsResponse, ApiFailure>;

    /// `POST /api/admin/accounts/:id/usage/refresh`: a synchronous single-
    /// account probe returning the freshly measured quota fields.
    async fn refresh_usage(
        &self,
        base_url: &str,
        admin_key: &str,
        account_id: i64,
    ) -> Result<UsageRefreshResponse, ApiFailure>;
}

#[derive(Clone)]
pub struct HttpCodex2apiApi {
    client: Client,
}

impl HttpCodex2apiApi {
    pub fn new() -> Result<Self, ApiFailure> {
        let client = Client::builder()
            .timeout(Duration::from_secs(30))
            // Redirects are refused outright: the admin key must never be
            // forwarded to a host the server picked.
            .redirect(reqwest::redirect::Policy::none())
            .build()
            .map_err(|_| ApiFailure::network("failed to construct codex2api HTTP client"))?;
        Ok(Self { client })
    }

    /// Validates a pasted gateway base URL. HTTPS is required anywhere except
    /// loopback, where the gateway typically serves plain HTTP.
    pub fn check_base_url(base_url: &str) -> Result<(), ApiFailure> {
        let url = reqwest::Url::parse(base_url).map_err(|_| {
            ApiFailure::protocol("the codex2api gateway base URL is not a valid URL")
        })?;
        if url.path() != "/" && url.path() != "" {
            return Err(ApiFailure::protocol(
                "the codex2api gateway base URL must not contain a path",
            ));
        }
        if url.query().is_some() || url.fragment().is_some() {
            return Err(ApiFailure::protocol(
                "the codex2api gateway base URL must not contain a query or fragment",
            ));
        }
        if !is_secure_or_loopback(&url) {
            return Err(ApiFailure::protocol(
                "the codex2api gateway base URL must use HTTPS or loopback HTTP",
            ));
        }
        Ok(())
    }

    fn endpoint(&self, base_url: &str, path: &str) -> String {
        format!("{}{}", base_url.trim_end_matches('/'), path)
    }
}

#[async_trait]
impl Codex2apiApi for HttpCodex2apiApi {
    async fn list_accounts(
        &self,
        base_url: &str,
        admin_key: &str,
    ) -> Result<AccountsResponse, ApiFailure> {
        let response = self
            .client
            .get(self.endpoint(base_url, ACCOUNTS_PATH))
            .header(ADMIN_KEY_HEADER, admin_key)
            .send()
            .await
            .map_err(network_failure)?;
        decode_response(response, "codex2api account list").await
    }

    async fn refresh_usage(
        &self,
        base_url: &str,
        admin_key: &str,
        account_id: i64,
    ) -> Result<UsageRefreshResponse, ApiFailure> {
        let response = self
            .client
            .post(self.endpoint(
                base_url,
                &format!("{ACCOUNTS_PATH}/{account_id}/usage/refresh"),
            ))
            .header(ADMIN_KEY_HEADER, admin_key)
            .send()
            .await
            .map_err(network_failure)?;
        decode_response(response, "codex2api usage refresh").await
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
    // The admin API answers both 401 and 403 for a missing or wrong
    // X-Admin-Key; there is no finer-grained credential to fall back to.
    if status == StatusCode::UNAUTHORIZED || status == StatusCode::FORBIDDEN {
        return Some(ApiFailure::authentication(format!(
            "{operation} rejected the admin key"
        )));
    }
    if status == StatusCode::TOO_MANY_REQUESTS {
        return Some(ApiFailure {
            kind: ApiFailureKind::RateLimit,
            message: format!("{operation} was rate limited"),
            retry_after_seconds,
        });
    }
    if status == StatusCode::REQUEST_TIMEOUT || status.is_server_error() {
        return Some(ApiFailure::network(format!(
            "{operation} returned HTTP {status}"
        )));
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
    ApiFailure::network("codex2api request failed before a response was received")
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn classifies_admin_http_status() {
        for status in [StatusCode::UNAUTHORIZED, StatusCode::FORBIDDEN] {
            assert_eq!(
                classify_status(status, "list", None).unwrap().kind,
                ApiFailureKind::Authentication
            );
        }
        assert_eq!(
            classify_status(StatusCode::REQUEST_TIMEOUT, "list", None)
                .unwrap()
                .kind,
            ApiFailureKind::Network
        );
        assert_eq!(
            classify_status(StatusCode::BAD_GATEWAY, "refresh", None)
                .unwrap()
                .kind,
            ApiFailureKind::Network
        );
        assert_eq!(
            classify_status(StatusCode::NOT_FOUND, "refresh", None)
                .unwrap()
                .kind,
            ApiFailureKind::Protocol
        );
        let limited = classify_status(StatusCode::TOO_MANY_REQUESTS, "list", Some(9)).unwrap();
        assert_eq!(limited.kind, ApiFailureKind::RateLimit);
        assert_eq!(limited.retry_after_seconds, Some(9));
        assert!(classify_status(StatusCode::OK, "list", None).is_none());
    }

    #[test]
    fn base_url_accepts_https_and_loopback_http() {
        assert!(HttpCodex2apiApi::check_base_url("https://gw.example.com").is_ok());
        assert!(HttpCodex2apiApi::check_base_url("https://gw.example.com/").is_ok());
        assert!(HttpCodex2apiApi::check_base_url("http://localhost:8080").is_ok());
        assert!(HttpCodex2apiApi::check_base_url("http://127.0.0.1:8080").is_ok());
        assert!(HttpCodex2apiApi::check_base_url("http://[::1]:8080").is_ok());
        assert!(HttpCodex2apiApi::check_base_url("http://192.168.1.10:8080").is_err());
        assert!(HttpCodex2apiApi::check_base_url("ftp://localhost").is_err());
        assert!(HttpCodex2apiApi::check_base_url("not a url").is_err());
        assert!(HttpCodex2apiApi::check_base_url("https://gw.example.com/api").is_err());
    }
}
