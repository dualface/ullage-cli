//! HTTP transport for the sub2api admin API.
//!
//! The gateway answers every admin endpoint with a `{code, message, data}`
//! envelope: `code` is `0` on success and either an HTTP status number or a
//! machine-readable string such as `UNAUTHORIZED` on failure. The admin API
//! key travels in the `x-api-key` header and is never put in URLs or error
//! text.

use std::fmt;
use std::time::Duration;

use async_trait::async_trait;
use reqwest::{Client, StatusCode};
use serde::de::DeserializeOwned;
use ullage_core::ProviderError;

use crate::dto::{AccountsPage, AdminAccount, UsageInfo};

const ADMIN_PREFIX: &str = "/api/v1/admin";

/// Responses larger than this are rejected before deserialization.
const MAX_RESPONSE_BYTES: usize = 2 * 1024 * 1024;

/// The accounts listing is paged; the lookup scan reads pages of this size.
pub const ACCOUNTS_PAGE_SIZE: i64 = 100;

#[derive(Clone, Debug, PartialEq, Eq)]
pub enum ApiFailureKind {
    /// 401/403, or an `UNAUTHORIZED` envelope code: the admin key itself was
    /// rejected.
    Authentication,
    /// The upstream account the credential points at no longer exists in the
    /// gateway. Treated as a credential failure: the stored reference cannot
    /// produce usage anymore.
    UpstreamAccountMissing,
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

    pub fn upstream_account_missing(message: impl Into<String>) -> Self {
        Self {
            kind: ApiFailureKind::UpstreamAccountMissing,
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
            ApiFailureKind::Authentication | ApiFailureKind::UpstreamAccountMissing => {
                ProviderError::AuthenticationInvalid {
                    message: self.message,
                }
            }
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
pub trait Sub2apiApi: Send + Sync {
    /// `GET /api/v1/admin/accounts?page=&page_size=`, one page.
    async fn accounts(
        &self,
        base_url: &str,
        admin_key: &str,
        page: i64,
    ) -> Result<AccountsPage, ApiFailure>;

    /// `GET /api/v1/admin/accounts/:id`, one upstream account.
    async fn account(
        &self,
        base_url: &str,
        admin_key: &str,
        account_id: i64,
    ) -> Result<AdminAccount, ApiFailure>;

    /// `GET /api/v1/admin/accounts/:id/usage?force=...`. `force=true` makes
    /// the gateway probe the upstream instead of answering from its cache.
    async fn usage(
        &self,
        base_url: &str,
        admin_key: &str,
        account_id: i64,
        force: bool,
    ) -> Result<UsageInfo, ApiFailure>;
}

/// Validates the user-pasted gateway base URL. The admin key grants full
/// read/write access to the gateway, so it may only be sent over HTTPS or to
/// a loopback HTTP endpoint, where the local gateway typically listens in
/// plaintext.
pub fn validated_base_url(input: &str) -> Result<String, ApiFailure> {
    let base_url = input.trim().trim_end_matches('/').to_owned();
    let url = reqwest::Url::parse(&base_url)
        .map_err(|_| ApiFailure::protocol("the sub2api base URL is not a valid URL"))?;
    if !is_secure_or_loopback(&url) {
        return Err(ApiFailure::protocol(
            "the sub2api base URL must use HTTPS or loopback HTTP",
        ));
    }
    Ok(base_url)
}

#[derive(Clone)]
pub struct HttpSub2apiApi {
    client: Client,
}

impl HttpSub2apiApi {
    pub fn new() -> Result<Self, ApiFailure> {
        let client = Client::builder()
            .timeout(Duration::from_secs(30))
            // Redirects are refused outright: the admin key must never be
            // forwarded to a host the server picked.
            .redirect(reqwest::redirect::Policy::none())
            .build()
            .map_err(|_| ApiFailure {
                kind: ApiFailureKind::Network,
                message: "failed to construct the sub2api HTTP client".into(),
                retry_after_seconds: None,
            })?;
        Ok(Self { client })
    }

    async fn get(
        &self,
        base_url: &str,
        admin_key: &str,
        path: &str,
        query: &[(&str, String)],
    ) -> Result<reqwest::Response, ApiFailure> {
        let url = format!("{}{}{}", base_url, ADMIN_PREFIX, path);
        self.client
            .get(url)
            .header("x-api-key", admin_key)
            .query(query)
            .send()
            .await
            .map_err(|_| network_failure("sub2api"))
    }
}

#[async_trait]
impl Sub2apiApi for HttpSub2apiApi {
    async fn accounts(
        &self,
        base_url: &str,
        admin_key: &str,
        page: i64,
    ) -> Result<AccountsPage, ApiFailure> {
        let response = self
            .get(
                base_url,
                admin_key,
                "/accounts",
                &[
                    ("page", page.to_string()),
                    ("page_size", ACCOUNTS_PAGE_SIZE.to_string()),
                ],
            )
            .await?;
        decode_envelope(response, "sub2api account listing").await
    }

    async fn account(
        &self,
        base_url: &str,
        admin_key: &str,
        account_id: i64,
    ) -> Result<AdminAccount, ApiFailure> {
        let response = self
            .get(base_url, admin_key, &format!("/accounts/{account_id}"), &[])
            .await?;
        decode_envelope(response, "sub2api account lookup").await
    }

    async fn usage(
        &self,
        base_url: &str,
        admin_key: &str,
        account_id: i64,
        force: bool,
    ) -> Result<UsageInfo, ApiFailure> {
        let response = self
            .get(
                base_url,
                admin_key,
                &format!("/accounts/{account_id}/usage"),
                &[("force", force.to_string())],
            )
            .await?;
        decode_envelope(response, "sub2api account usage").await
    }
}

/// The response envelope: `code` is `0` on success; on failure it is either
/// the HTTP status number or a machine-readable string like `UNAUTHORIZED`.
#[derive(serde::Deserialize)]
#[serde(bound(deserialize = "T: DeserializeOwned"))]
struct Envelope<T> {
    code: serde_json::Value,
    #[serde(default)]
    message: Option<String>,
    #[serde(default)]
    data: Option<T>,
}

async fn decode_envelope<T: DeserializeOwned>(
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
    let envelope: Envelope<T> = serde_json::from_slice(&body)
        .map_err(|_| ApiFailure::protocol(format!("{operation} returned incompatible JSON")))?;
    if envelope.code.as_i64() == Some(0) {
        return envelope
            .data
            .ok_or_else(|| ApiFailure::protocol(format!("{operation} returned no data")));
    }
    Err(classify_envelope_error(
        &envelope.code,
        envelope.message.as_deref(),
        operation,
    ))
}

/// A 200 response whose envelope still reports a failure: the HTTP status
/// gave nothing away, so the machine-readable `code` and the gateway's own
/// `message` decide the class. The code is a string like `UNAUTHORIZED` for
/// middleware rejections and an HTTP status number for handler errors.
fn classify_envelope_error(
    code: &serde_json::Value,
    message: Option<&str>,
    operation: &str,
) -> ApiFailure {
    let detail = message
        .map(|text| text.trim())
        .filter(|text| !text.is_empty())
        .unwrap_or("the gateway reported an error");
    // A numeric `code` is the HTTP status repeated in the body; a string
    // `code` is a middleware-style machine name such as `UNAUTHORIZED`.
    if let Some(status) = code.as_i64() {
        return match status {
            401 | 403 => ApiFailure::authentication(format!("{operation} rejected the credential")),
            404 => ApiFailure::upstream_account_missing(detail.to_owned()),
            429 => ApiFailure {
                kind: ApiFailureKind::RateLimit,
                message: detail.to_owned(),
                retry_after_seconds: None,
            },
            _ => ApiFailure::protocol(format!("{operation} failed: {detail}")),
        };
    }
    let upper = code.as_str().unwrap_or_default().to_ascii_uppercase();
    if upper.contains("UNAUTHORIZED") || upper.contains("FORBIDDEN") {
        return ApiFailure::authentication(format!("{operation} rejected the credential"));
    }
    if upper.contains("NOT_FOUND") {
        return ApiFailure::upstream_account_missing(detail.to_owned());
    }
    if upper.contains("RATE_LIMIT") || upper.contains("TOO_MANY_REQUESTS") {
        return ApiFailure {
            kind: ApiFailureKind::RateLimit,
            message: detail.to_owned(),
            retry_after_seconds: None,
        };
    }
    ApiFailure::protocol(format!("{operation} failed: {detail}"))
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
    while let Some(chunk) = response
        .chunk()
        .await
        .map_err(|_| network_failure("sub2api"))?
    {
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
    if status == StatusCode::UNAUTHORIZED || status == StatusCode::FORBIDDEN {
        return Some(ApiFailure::authentication(format!(
            "{operation} rejected the credential"
        )));
    }
    if status == StatusCode::NOT_FOUND {
        return Some(ApiFailure::upstream_account_missing(format!(
            "{operation} found no matching upstream account"
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

fn network_failure(operation: &str) -> ApiFailure {
    ApiFailure {
        kind: ApiFailureKind::Network,
        message: format!("{operation} request failed before a response was received"),
        retry_after_seconds: None,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn classifies_http_status() {
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
            ApiFailureKind::Authentication
        );
        assert_eq!(
            classify_status(StatusCode::NOT_FOUND, "usage", None)
                .unwrap()
                .kind,
            ApiFailureKind::UpstreamAccountMissing
        );
        assert_eq!(
            classify_status(StatusCode::REQUEST_TIMEOUT, "usage", None)
                .unwrap()
                .kind,
            ApiFailureKind::Network
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
    fn classifies_envelope_error_codes() {
        assert_eq!(
            classify_envelope_error(
                &serde_json::json!("UNAUTHORIZED"),
                Some("Authorization required"),
                "usage"
            )
            .kind,
            ApiFailureKind::Authentication
        );
        assert_eq!(
            classify_envelope_error(
                &serde_json::json!("ACCOUNT_NOT_FOUND"),
                Some("account not found"),
                "usage"
            )
            .kind,
            ApiFailureKind::UpstreamAccountMissing
        );
        assert_eq!(
            classify_envelope_error(&serde_json::json!(404), Some("missing"), "usage").kind,
            ApiFailureKind::UpstreamAccountMissing
        );
        assert_eq!(
            classify_envelope_error(&serde_json::json!(500), Some("boom"), "usage").kind,
            ApiFailureKind::Protocol
        );
    }

    #[test]
    fn envelope_error_maps_to_provider_error() {
        let error = ApiFailure::authentication("rejected").into_provider_error();
        assert_eq!(
            error,
            ProviderError::AuthenticationInvalid {
                message: "rejected".into()
            }
        );
        let missing = ApiFailure::upstream_account_missing("gone").into_provider_error();
        assert_eq!(
            missing,
            ProviderError::AuthenticationInvalid {
                message: "gone".into()
            }
        );
    }

    #[test]
    fn base_url_accepts_https_and_loopback_http() {
        assert!(validated_base_url("https://gateway.example.com/").is_ok());
        assert!(validated_base_url("http://127.0.0.1:55001").is_ok());
        assert!(validated_base_url("http://localhost:55001/").is_ok());
        assert!(validated_base_url("http://[::1]:55001").is_ok());
        assert!(validated_base_url("http://192.168.1.5:55001").is_err());
        assert!(validated_base_url("http://gateway.example.com").is_err());
        assert!(validated_base_url("not a url").is_err());
    }
}
