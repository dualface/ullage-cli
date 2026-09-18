//! Connect-RPC transport for Devin's `SeatManagementService`.
//!
//! Two calls share one client: the PKCE authorization-code exchange at
//! `api.devin.ai`, and `GetUserStatus` at the per-account `api_server_url`
//! the exchange returned. Both speak Connect-RPC JSON and carry the
//! `Connect-Protocol-Version: 1` header.

use std::fmt;
use std::time::Duration;

use async_trait::async_trait;
use reqwest::{Client, StatusCode};
use serde::Serialize;
use serde::de::DeserializeOwned;
use serde_json::json;

use crate::dto::UserStatusResponse;

const EXCHANGE_URL: &str = "https://api.devin.ai/exa.seat_management_pb.SeatManagementService/ExchangePKCEAuthorizationCode";
const USER_STATUS_PATH: &str = "/exa.seat_management_pb.SeatManagementService/GetUserStatus";
/// The server a manually pasted key reports against, matching what the
/// official CLI stores in `credentials.toml`.
pub const DEFAULT_API_SERVER_URL: &str = "https://server.codeium.com";

/// Responses larger than this are rejected before deserialization.
const MAX_RESPONSE_BYTES: usize = 2 * 1024 * 1024;

#[derive(Clone, Debug, PartialEq, Eq)]
pub enum ApiFailureKind {
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

    pub fn into_provider_error(self) -> ullage_core::ProviderError {
        match self.kind {
            ApiFailureKind::Authentication => ullage_core::ProviderError::AuthenticationInvalid {
                message: self.message,
            },
            ApiFailureKind::RateLimit => ullage_core::ProviderError::RateLimited {
                message: self.message,
                retry_after_seconds: self.retry_after_seconds,
            },
            ApiFailureKind::Network => ullage_core::ProviderError::Network {
                message: self.message,
            },
            ApiFailureKind::Protocol => ullage_core::ProviderError::ProtocolIncompatible {
                message: self.message,
            },
        }
    }
}

/// What a successful `ExchangePKCEAuthorizationCode` returns. Only `api_key`
/// is required; the server may omit every other field.
#[derive(Clone, Debug, Default, PartialEq, Eq, serde::Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct ExchangeResponse {
    pub api_key: String,
    #[serde(default)]
    pub api_server_url: Option<String>,
    #[serde(default)]
    pub devin_webapp_host: Option<String>,
    #[serde(default)]
    pub devin_api_url: Option<String>,
    #[serde(default)]
    pub session_token: Option<String>,
}

#[async_trait]
pub trait DevinApi: Send + Sync {
    /// `ExchangePKCEAuthorizationCode`: the PKCE exchange completing browser
    /// sign-in. `redirect_uri` must be the exact URI the authorization used.
    async fn exchange_pkce_code(
        &self,
        code: &str,
        code_verifier: &str,
        redirect_uri: &str,
    ) -> Result<ExchangeResponse, ApiFailure>;

    /// `GetUserStatus`: plan name plus daily/weekly quota state. Also the
    /// credential check at sign-in, so a dead key never reaches the store.
    async fn user_status(
        &self,
        api_key: &str,
        api_server_url: &str,
    ) -> Result<UserStatusResponse, ApiFailure>;
}

#[derive(Clone)]
pub struct HttpDevinApi {
    client: Client,
    exchange_url: String,
}

impl HttpDevinApi {
    pub fn new() -> Result<Self, ApiFailure> {
        Self::with_exchange_url(EXCHANGE_URL)
    }

    pub fn with_exchange_url(exchange_url: impl Into<String>) -> Result<Self, ApiFailure> {
        let exchange_url = exchange_url.into();
        let url = reqwest::Url::parse(&exchange_url)
            .map_err(|_| ApiFailure::protocol("Devin exchange endpoint is not a valid URL"))?;
        if !is_secure_or_loopback(&url) {
            return Err(ApiFailure::protocol(
                "Devin exchange endpoint must use HTTPS or loopback HTTP",
            ));
        }
        let client = Client::builder()
            .timeout(Duration::from_secs(30))
            // Redirects are refused outright: a credential must never be
            // forwarded to a host the server picked.
            .redirect(reqwest::redirect::Policy::none())
            .build()
            .map_err(|_| ApiFailure::network("failed to construct Devin HTTP client"))?;
        Ok(Self {
            client,
            exchange_url,
        })
    }

    async fn post_connect<T: Serialize, R: DeserializeOwned>(
        &self,
        url: &str,
        body: &T,
        operation: &str,
        bad_request_is_authentication: bool,
    ) -> Result<R, ApiFailure> {
        let response = self
            .client
            .post(url)
            .header("Connect-Protocol-Version", "1")
            .json(body)
            .send()
            .await
            .map_err(network_failure)?;
        decode_response(response, operation, bad_request_is_authentication).await
    }
}

#[async_trait]
impl DevinApi for HttpDevinApi {
    async fn exchange_pkce_code(
        &self,
        code: &str,
        code_verifier: &str,
        redirect_uri: &str,
    ) -> Result<ExchangeResponse, ApiFailure> {
        // A rejected grant is a client error here: the flow is dead and only
        // a fresh sign-in can produce a working code.
        self.post_connect(
            &self.exchange_url,
            &exchange_body(code, code_verifier, redirect_uri),
            "Devin PKCE exchange",
            true,
        )
        .await
    }

    async fn user_status(
        &self,
        api_key: &str,
        api_server_url: &str,
    ) -> Result<UserStatusResponse, ApiFailure> {
        let base = api_server_url.trim_end_matches('/');
        let url = reqwest::Url::parse(base)
            .map_err(|_| ApiFailure::protocol("Devin API server URL is not a valid URL"))?;
        if !is_secure_or_loopback(&url) {
            return Err(ApiFailure::protocol(
                "Devin API server URL must use HTTPS or loopback HTTP",
            ));
        }
        self.post_connect(
            &format!("{base}{USER_STATUS_PATH}"),
            &user_status_body(api_key),
            "Devin user status",
            false,
        )
        .await
    }
}

async fn decode_response<T: DeserializeOwned>(
    response: reqwest::Response,
    operation: &str,
    bad_request_is_authentication: bool,
) -> Result<T, ApiFailure> {
    let status = response.status();
    let retry_after_seconds = response
        .headers()
        .get(reqwest::header::RETRY_AFTER)
        .and_then(|value| value.to_str().ok())
        .and_then(|value| value.parse().ok());
    if let Some(error) = classify_status(
        status,
        operation,
        bad_request_is_authentication,
        retry_after_seconds,
    ) {
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
    bad_request_is_authentication: bool,
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
    if bad_request_is_authentication && status == StatusCode::BAD_REQUEST {
        return Some(ApiFailure::authentication(format!(
            "{operation} rejected the credential"
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

fn exchange_body(code: &str, code_verifier: &str, redirect_uri: &str) -> serde_json::Value {
    json!({
        "code": code,
        "code_verifier": code_verifier,
        "redirect_uri": redirect_uri,
    })
}

/// The metadata block is telemetry the endpoint expects; `ideName` and the
/// versions identify this client, not the vendor's own.
fn user_status_body(api_key: &str) -> serde_json::Value {
    let version = env!("CARGO_PKG_VERSION");
    json!({
        "metadata": {
            "apiKey": api_key,
            "ideName": "ullage",
            "ideVersion": version,
            "extensionVersion": version,
            "locale": "en",
        }
    })
}

fn network_failure(_: reqwest::Error) -> ApiFailure {
    ApiFailure::network("Devin request failed before a response was received")
}

#[cfg(test)]
mod tests {
    use serde_json::json;

    use super::*;

    #[test]
    fn exchange_request_body_carries_code_verifier_and_redirect() {
        assert_eq!(
            exchange_body("code-1", "verifier-1", "http://127.0.0.1:4567/callback"),
            json!({
                "code": "code-1",
                "code_verifier": "verifier-1",
                "redirect_uri": "http://127.0.0.1:4567/callback",
            })
        );
    }

    #[test]
    fn user_status_request_body_carries_metadata() {
        let body = user_status_body("key-1");
        assert_eq!(
            body,
            json!({
                "metadata": {
                    "apiKey": "key-1",
                    "ideName": "ullage",
                    "ideVersion": env!("CARGO_PKG_VERSION"),
                    "extensionVersion": env!("CARGO_PKG_VERSION"),
                    "locale": "en",
                }
            })
        );
    }

    #[test]
    fn exchange_response_tolerates_absent_optional_fields() {
        let response: ExchangeResponse = serde_json::from_str(r#"{"apiKey":"key-1"}"#).unwrap();
        assert_eq!(response.api_key, "key-1");
        assert!(response.api_server_url.is_none());
        assert!(response.session_token.is_none());
    }

    #[test]
    fn classifies_http_status() {
        assert_eq!(
            classify_status(StatusCode::UNAUTHORIZED, "status", false, None)
                .unwrap()
                .kind,
            ApiFailureKind::Authentication
        );
        assert_eq!(
            classify_status(StatusCode::FORBIDDEN, "status", false, None)
                .unwrap()
                .kind,
            ApiFailureKind::Authentication
        );
        assert_eq!(
            classify_status(StatusCode::REQUEST_TIMEOUT, "status", false, None)
                .unwrap()
                .kind,
            ApiFailureKind::Network
        );
        assert_eq!(
            classify_status(StatusCode::SERVICE_UNAVAILABLE, "status", false, None)
                .unwrap()
                .kind,
            ApiFailureKind::Network
        );
        assert_eq!(
            classify_status(StatusCode::NOT_FOUND, "status", false, None)
                .unwrap()
                .kind,
            ApiFailureKind::Protocol
        );
        assert_eq!(
            classify_status(StatusCode::BAD_REQUEST, "status", false, None)
                .unwrap()
                .kind,
            ApiFailureKind::Protocol
        );
        assert_eq!(
            classify_status(StatusCode::BAD_REQUEST, "exchange", true, None)
                .unwrap()
                .kind,
            ApiFailureKind::Authentication
        );
        let limited =
            classify_status(StatusCode::TOO_MANY_REQUESTS, "status", false, Some(9)).unwrap();
        assert_eq!(limited.kind, ApiFailureKind::RateLimit);
        assert_eq!(limited.retry_after_seconds, Some(9));
        assert!(classify_status(StatusCode::OK, "status", false, None).is_none());
    }

    #[test]
    fn rejects_insecure_endpoints() {
        assert!(HttpDevinApi::with_exchange_url("https://api.devin.ai/x").is_ok());
        assert!(HttpDevinApi::with_exchange_url("http://api.devin.ai/x").is_err());
        assert!(HttpDevinApi::with_exchange_url("http://127.0.0.1:8080/x").is_ok());
        assert!(HttpDevinApi::with_exchange_url("not a url").is_err());
    }
}
