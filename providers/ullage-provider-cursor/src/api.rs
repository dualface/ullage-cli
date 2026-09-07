use std::fmt;
use std::time::Duration;

use async_trait::async_trait;
use reqwest::{Client, StatusCode};
use serde::de::DeserializeOwned;
use serde::{Deserialize, Deserializer};
use serde_json::json;
use ullage_core::ProviderError;
use zeroize::{Zeroize, Zeroizing};

use crate::dto::{CreditGrantsBalance, CurrentPeriodUsage, HardLimit, PlanInfoResponse};

const DEFAULT_API_BASE: &str = "https://api2.cursor.sh";

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

    pub fn scope_suffix(&self) -> &'static str {
        match self.kind {
            ApiFailureKind::Authentication => "authentication",
            ApiFailureKind::RateLimit => "rate_limit",
            ApiFailureKind::Network => "network",
            ApiFailureKind::Protocol => "protocol",
        }
    }
}

#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct ExchangeTokens {
    pub access_token: SecretString,
    pub refresh_token: Option<SecretString>,
    pub email: Option<String>,
}

pub struct SecretString(String);

impl fmt::Debug for SecretString {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str("SecretString([REDACTED])")
    }
}

impl SecretString {
    pub fn new(value: impl Into<String>) -> Self {
        Self(value.into())
    }

    pub fn expose_secret(&self) -> &str {
        &self.0
    }

    pub(crate) fn take(&mut self) -> Zeroizing<String> {
        Zeroizing::new(std::mem::take(&mut self.0))
    }
}

impl Drop for SecretString {
    fn drop(&mut self) {
        self.0.zeroize();
    }
}

impl<'de> Deserialize<'de> for SecretString {
    fn deserialize<D>(deserializer: D) -> Result<Self, D::Error>
    where
        D: Deserializer<'de>,
    {
        String::deserialize(deserializer).map(Self)
    }
}

#[async_trait]
pub trait CursorApi: Send + Sync {
    async fn exchange_user_api_key(&self, api_key: &str) -> Result<ExchangeTokens, ApiFailure>;
    /// Polls the browser sign-in started at `loginDeepControl`. `Ok(None)` means
    /// the browser has not finished yet, which is the normal answer until the
    /// user approves. Test doubles that only cover the API key path inherit the
    /// default, which reports the flow as unavailable rather than hanging.
    async fn poll_login(
        &self,
        _uuid: &str,
        _verifier: &str,
    ) -> Result<Option<ExchangeTokens>, ApiFailure> {
        Err(ApiFailure::protocol(
            "this Cursor API does not serve browser sign-in",
        ))
    }
    async fn current_period(&self, access_token: &str) -> Result<CurrentPeriodUsage, ApiFailure>;
    async fn plan_info(&self, access_token: &str) -> Result<PlanInfoResponse, ApiFailure>;
    async fn credit_grants(&self, access_token: &str) -> Result<CreditGrantsBalance, ApiFailure>;
    async fn hard_limit(&self, access_token: &str) -> Result<HardLimit, ApiFailure>;
}

#[derive(Clone)]
pub struct HttpCursorApi {
    client: Client,
    api_base: String,
}

impl HttpCursorApi {
    pub fn new() -> Result<Self, ApiFailure> {
        Self::with_base_url(DEFAULT_API_BASE)
    }

    pub fn with_base_url(api_base: impl Into<String>) -> Result<Self, ApiFailure> {
        let client = Client::builder()
            .timeout(Duration::from_secs(30))
            .build()
            .map_err(|_| ApiFailure {
                kind: ApiFailureKind::Network,
                message: "failed to construct Cursor HTTP client".into(),
                retry_after_seconds: None,
            })?;
        Ok(Self {
            client,
            api_base: api_base.into().trim_end_matches('/').to_owned(),
        })
    }

    async fn dashboard<T: DeserializeOwned>(
        &self,
        access_token: &str,
        method: &str,
    ) -> Result<T, ApiFailure> {
        let url = format!("{}/aiserver.v1.DashboardService/{method}", self.api_base);
        let response = self
            .client
            .post(url)
            .bearer_auth(access_token)
            .header("Connect-Protocol-Version", "1")
            .json(&json!({}))
            .send()
            .await
            .map_err(network_failure)?;
        decode_response(response, "Cursor DashboardService", false).await
    }
}

#[async_trait]
impl CursorApi for HttpCursorApi {
    async fn exchange_user_api_key(&self, api_key: &str) -> Result<ExchangeTokens, ApiFailure> {
        let response = self
            .client
            .post(format!("{}/auth/exchange_user_api_key", self.api_base))
            .bearer_auth(api_key)
            .json(&json!({}))
            .send()
            .await
            .map_err(network_failure)?;
        let tokens: ExchangeTokens =
            decode_response(response, "Cursor API key exchange", true).await?;
        if tokens.access_token.expose_secret().trim().is_empty() {
            return Err(ApiFailure::protocol(
                "Cursor API key exchange returned an empty access token",
            ));
        }
        Ok(tokens)
    }

    async fn poll_login(
        &self,
        uuid: &str,
        verifier: &str,
    ) -> Result<Option<ExchangeTokens>, ApiFailure> {
        let response = self
            .client
            .get(format!("{}/auth/poll", self.api_base))
            .query(&[("uuid", uuid), ("verifier", verifier)])
            .send()
            .await
            .map_err(network_failure)?;
        // Cursor answers a sign-in nobody has approved yet with 404, so this is
        // the pending case rather than a failure.
        if response.status() == reqwest::StatusCode::NOT_FOUND {
            return Ok(None);
        }
        let tokens: ExchangeTokens =
            decode_response(response, "Cursor browser sign-in", true).await?;
        if tokens.access_token.expose_secret().trim().is_empty() {
            return Err(ApiFailure::protocol(
                "Cursor browser sign-in returned an empty access token",
            ));
        }
        Ok(Some(tokens))
    }

    async fn current_period(&self, access_token: &str) -> Result<CurrentPeriodUsage, ApiFailure> {
        self.dashboard(access_token, "GetCurrentPeriodUsage").await
    }

    async fn plan_info(&self, access_token: &str) -> Result<PlanInfoResponse, ApiFailure> {
        self.dashboard(access_token, "GetPlanInfo").await
    }

    async fn credit_grants(&self, access_token: &str) -> Result<CreditGrantsBalance, ApiFailure> {
        self.dashboard(access_token, "GetCreditGrantsBalance").await
    }

    async fn hard_limit(&self, access_token: &str) -> Result<HardLimit, ApiFailure> {
        self.dashboard(access_token, "GetHardLimit").await
    }
}

async fn decode_response<T: DeserializeOwned>(
    response: reqwest::Response,
    operation: &str,
    client_error_is_authentication: bool,
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
        client_error_is_authentication,
        retry_after_seconds,
    ) {
        return Err(error);
    }
    response
        .json()
        .await
        .map_err(|_| ApiFailure::protocol(format!("{operation} returned incompatible JSON")))
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
        return Some(ApiFailure {
            kind: ApiFailureKind::Network,
            message: format!("{operation} returned HTTP {status}"),
            retry_after_seconds: None,
        });
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

fn network_failure(_: reqwest::Error) -> ApiFailure {
    ApiFailure {
        kind: ApiFailureKind::Network,
        message: "Cursor request failed before a response was received".into(),
        retry_after_seconds: None,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn classifies_exchange_http_status_without_invalidating_on_protocol_or_network_errors() {
        assert_eq!(
            classify_status(StatusCode::BAD_REQUEST, "exchange", true, None)
                .unwrap()
                .kind,
            ApiFailureKind::Authentication
        );
        assert_eq!(
            classify_status(StatusCode::REQUEST_TIMEOUT, "exchange", true, None)
                .unwrap()
                .kind,
            ApiFailureKind::Network
        );
        assert_eq!(
            classify_status(StatusCode::NOT_FOUND, "exchange", true, None)
                .unwrap()
                .kind,
            ApiFailureKind::Protocol
        );
        let limited =
            classify_status(StatusCode::TOO_MANY_REQUESTS, "exchange", true, Some(9)).unwrap();
        assert_eq!(limited.kind, ApiFailureKind::RateLimit);
        assert_eq!(limited.retry_after_seconds, Some(9));
        assert_eq!(
            classify_status(StatusCode::SERVICE_UNAVAILABLE, "exchange", true, None)
                .unwrap()
                .kind,
            ApiFailureKind::Network
        );
    }
}
