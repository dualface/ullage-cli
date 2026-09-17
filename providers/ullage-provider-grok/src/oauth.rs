use std::collections::HashMap;
use std::fmt;
use std::sync::Mutex;

use async_trait::async_trait;
use base64::Engine;
use base64::engine::general_purpose::URL_SAFE_NO_PAD;
use chrono::{DateTime, Utc};
use reqwest::{Client, StatusCode, Url};
use serde::{Deserialize, Serialize};
use serde_json::Value;
use ullage_auth::{AuthChallenge, AuthInputRequest, AuthMethod, AuthState, account_identity};
use ullage_core::{ProviderError, ProviderResult};

#[derive(Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct OAuthToken {
    pub access_token: String,
    #[serde(default)]
    pub refresh_token: Option<String>,
    #[serde(default)]
    pub expires_at: Option<DateTime<Utc>>,
    #[serde(default)]
    pub account_label: Option<String>,
}

impl fmt::Debug for OAuthToken {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter
            .debug_struct("OAuthToken")
            .field("access_token", &"[REDACTED]")
            .field(
                "refresh_token",
                &self.refresh_token.as_ref().map(|_| "[REDACTED]"),
            )
            .field("expires_at", &self.expires_at)
            .field("account_label", &self.account_label)
            .finish()
    }
}

impl OAuthToken {
    pub(crate) fn validate_stored(&self) -> ProviderResult<()> {
        if self.access_token.trim().is_empty()
            || self
                .refresh_token
                .as_deref()
                .is_some_and(|token| token.trim().is_empty())
            || self
                .account_label
                .as_deref()
                .is_none_or(|label| label.trim().is_empty())
        {
            return Err(ProviderError::ProtocolIncompatible {
                message: "Grok returned malformed token contents".into(),
            });
        }
        Ok(())
    }

    pub(crate) fn validate(&self) -> ProviderResult<()> {
        self.validate_stored()?;
        if self.expires_at.is_some_and(|expiry| expiry <= Utc::now()) {
            return Err(ProviderError::AuthenticationInvalid {
                message: "Grok returned an expired access token".into(),
            });
        }
        Ok(())
    }

    pub(crate) fn auth_state(&self) -> AuthState {
        AuthState::Authenticated {
            account_label: self.account_label.clone(),
            // x.ai reports an email or subject, which names the signed-in user
            // rather than anything the user chose.
            account_key: self.account_label.as_deref().and_then(account_identity),
            expires_at: self.expires_at,
        }
    }
}

#[derive(Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct DeviceAuthorization {
    pub flow_id: String,
    pub device_code: String,
    pub user_code: String,
    pub verification_uri: String,
    #[serde(default)]
    pub expires_at: Option<DateTime<Utc>>,
    /// RFC 8628 `interval` in seconds. Carried for completeness; the client
    /// cannot receive it because `AuthChallenge` has no poll-hint field —
    /// that is a v11 protocol candidate, so the client keeps its own interval.
    #[serde(default)]
    pub poll_interval_seconds: Option<u64>,
}

impl fmt::Debug for DeviceAuthorization {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter
            .debug_struct("DeviceAuthorization")
            .field("flow_id", &"[REDACTED]")
            .field("device_code", &"[REDACTED]")
            .field("user_code", &"[REDACTED]")
            .field("verification_uri", &"[REDACTED]")
            .field("expires_at", &self.expires_at)
            .field("poll_interval_seconds", &self.poll_interval_seconds)
            .finish()
    }
}

impl DeviceAuthorization {
    pub(crate) fn validate(&self) -> ProviderResult<()> {
        validate_flow(&self.flow_id, &self.verification_uri)?;
        let expires_at = self
            .expires_at
            .ok_or_else(|| ProviderError::ProtocolIncompatible {
                message: "Grok device authorization has no expiry".into(),
            })?;
        validate_expiry(Some(expires_at))?;
        if self.device_code.trim().is_empty() || self.user_code.trim().is_empty() {
            return Err(ProviderError::ProtocolIncompatible {
                message: "Grok returned an incomplete device authorization".into(),
            });
        }
        Ok(())
    }

    pub(crate) fn challenge(&self) -> AuthChallenge {
        AuthChallenge {
            flow_id: self.flow_id.clone(),
            method: AuthMethod::DeviceCode,
            verification_uri: Some(self.verification_uri.clone()),
            user_code: Some(self.user_code.clone()),
            expires_at: self.expires_at,
            input: None,
        }
    }
}

#[derive(Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct BrowserAuthorization {
    pub flow_id: String,
    pub authorization_uri: String,
    #[serde(default)]
    pub expires_at: Option<DateTime<Utc>>,
}

impl fmt::Debug for BrowserAuthorization {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter
            .debug_struct("BrowserAuthorization")
            .field("flow_id", &"[REDACTED]")
            .field("authorization_uri", &"[REDACTED]")
            .field("expires_at", &self.expires_at)
            .finish()
    }
}

impl BrowserAuthorization {
    pub(crate) fn validate(&self) -> ProviderResult<()> {
        validate_flow(&self.flow_id, &self.authorization_uri)?;
        validate_expiry(self.expires_at)
    }

    pub(crate) fn challenge(&self) -> AuthChallenge {
        AuthChallenge {
            flow_id: self.flow_id.clone(),
            method: AuthMethod::BrowserOAuth,
            verification_uri: Some(self.authorization_uri.clone()),
            user_code: None,
            expires_at: self.expires_at,
            input: Some(AuthInputRequest::visible(
                "the full callback URL from the browser",
            )),
        }
    }
}

fn validate_flow(flow_id: &str, uri: &str) -> ProviderResult<()> {
    let valid_uri = Url::parse(uri).is_ok_and(|uri| is_secure_or_loopback(&uri));
    if flow_id.trim().is_empty() || !valid_uri {
        return Err(ProviderError::ProtocolIncompatible {
            message: "Grok returned an invalid OAuth challenge".into(),
        });
    }
    Ok(())
}

fn validate_expiry(expires_at: Option<DateTime<Utc>>) -> ProviderResult<()> {
    if expires_at.is_some_and(|expiry| expiry <= Utc::now()) {
        return Err(ProviderError::ProtocolIncompatible {
            message: "Grok returned an expired OAuth challenge".into(),
        });
    }
    Ok(())
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub enum OAuthPoll {
    Pending,
    Authorized(OAuthToken),
    Denied,
    Expired,
}

#[derive(Clone, PartialEq, Eq)]
pub enum GrokApiError {
    AuthenticationInvalid(String),
    RateLimited {
        message: String,
        retry_after_seconds: Option<u64>,
    },
    Network(String),
    ProtocolIncompatible(String),
}

impl fmt::Debug for GrokApiError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::AuthenticationInvalid(_) => formatter
                .debug_tuple("AuthenticationInvalid")
                .field(&"[REDACTED]")
                .finish(),
            Self::RateLimited {
                retry_after_seconds,
                ..
            } => formatter
                .debug_struct("RateLimited")
                .field("message", &"[REDACTED]")
                .field("retry_after_seconds", retry_after_seconds)
                .finish(),
            Self::Network(_) => formatter
                .debug_tuple("Network")
                .field(&"[REDACTED]")
                .finish(),
            Self::ProtocolIncompatible(_) => formatter
                .debug_tuple("ProtocolIncompatible")
                .field(&"[REDACTED]")
                .finish(),
        }
    }
}

impl fmt::Display for GrokApiError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::AuthenticationInvalid(message)
            | Self::Network(message)
            | Self::ProtocolIncompatible(message) => formatter.write_str(message),
            Self::RateLimited { message, .. } => formatter.write_str(message),
        }
    }
}

impl std::error::Error for GrokApiError {}

impl From<GrokApiError> for ProviderError {
    fn from(error: GrokApiError) -> Self {
        match error {
            GrokApiError::AuthenticationInvalid(message) => Self::AuthenticationInvalid { message },
            GrokApiError::RateLimited {
                message,
                retry_after_seconds,
            } => Self::RateLimited {
                message,
                retry_after_seconds,
            },
            GrokApiError::Network(message) => Self::Network { message },
            GrokApiError::ProtocolIncompatible(message) => Self::ProtocolIncompatible { message },
        }
    }
}

/// Network boundary for the undocumented consumer endpoints.
///
/// Implementations must generate an unguessable OAuth state/flow id, keep PKCE verifiers and
/// device codes out of logs, reject redirects outside their configured callback and apply finite
/// request timeouts. Returning raw JSON lets the parser tolerate additive billing schema changes.
#[async_trait]
pub trait GrokTransport: Send + Sync {
    /// Drops transport-private state for a pending flow that was superseded or cancelled.
    fn cancel_authorization(&self, _flow_id: &str) {}

    /// Default browser OAuth redirect URI when the caller omits one.
    fn browser_redirect_uri(&self) -> &str;

    async fn start_device_authorization(&self) -> Result<DeviceAuthorization, GrokApiError>;
    async fn poll_device_authorization(&self, device_code: &str)
    -> Result<OAuthPoll, GrokApiError>;
    async fn start_browser_authorization(
        &self,
        redirect_uri: &str,
    ) -> Result<BrowserAuthorization, GrokApiError>;
    async fn complete_browser_authorization(
        &self,
        flow_id: &str,
        authorization_code: &str,
        redirect_uri: &str,
    ) -> Result<OAuthToken, GrokApiError>;
    async fn refresh(&self, refresh_token: &str) -> Result<OAuthToken, GrokApiError>;
    async fn revoke(&self, token: &OAuthToken) -> Result<(), GrokApiError>;
    async fn fetch_billing(&self, access_token: &str) -> Result<Value, GrokApiError>;
    async fn fetch_settings(&self, access_token: &str) -> Result<Value, GrokApiError>;
}

#[derive(Clone, PartialEq, Eq)]
pub struct HttpGrokConfig {
    pub client_id: String,
    pub scope: String,
    pub redirect_uri: String,
    pub device_authorization_url: String,
    pub authorization_url: String,
    pub token_url: String,
    pub revoke_url: String,
    pub billing_url: String,
    pub settings_url: String,
}

/// Concrete HTTP adapter for configured Grok consumer endpoints.
///
/// Endpoint URLs are configuration because the consumer API is not a stable public contract. They
/// must be HTTPS, except that loopback HTTP is allowed for local callbacks and mock servers.
pub struct HttpGrokTransport {
    client: Client,
    config: HttpGrokConfig,
    browser_flows: Mutex<HashMap<String, BrowserFlow>>,
}

#[derive(Clone)]
struct BrowserFlow {
    verifier: String,
    redirect_uri: String,
    expires_at: DateTime<Utc>,
}

impl HttpGrokTransport {
    pub fn new(config: HttpGrokConfig) -> Result<Self, GrokApiError> {
        validate_config(&config)?;
        let client = Client::builder()
            .timeout(std::time::Duration::from_secs(30))
            .redirect(reqwest::redirect::Policy::none())
            .build()
            .map_err(network_error)?;
        Ok(Self {
            client,
            config,
            browser_flows: Mutex::new(HashMap::new()),
        })
    }

    fn lock_flows(&self) -> std::sync::MutexGuard<'_, HashMap<String, BrowserFlow>> {
        // The map only holds opaque flow records, so a poisoned lock is
        // recovered rather than reported, matching `cancel_authorization`.
        self.browser_flows
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
    }

    async fn post_token(&self, form: &[(&str, &str)]) -> Result<OAuthToken, GrokApiError> {
        let response = self
            .client
            .post(&self.config.token_url)
            .form(form)
            .send()
            .await
            .map_err(network_error)?;
        parse_token_response(response).await
    }

    async fn fetch_authenticated_json(
        &self,
        url: &str,
        access_token: &str,
        context: &str,
    ) -> Result<Value, GrokApiError> {
        let response = self
            .client
            .get(url)
            .bearer_auth(access_token)
            .send()
            .await
            .map_err(network_error)?;
        json_response(
            checked_response(response, RequestAuthContext::CredentialPresent).await?,
            context,
        )
        .await
    }
}

#[async_trait]
impl GrokTransport for HttpGrokTransport {
    fn browser_redirect_uri(&self) -> &str {
        &self.config.redirect_uri
    }

    fn cancel_authorization(&self, flow_id: &str) {
        match self.browser_flows.lock() {
            Ok(mut flows) => {
                flows.remove(flow_id);
            }
            Err(poisoned) => {
                poisoned.into_inner().remove(flow_id);
            }
        }
    }

    async fn start_device_authorization(&self) -> Result<DeviceAuthorization, GrokApiError> {
        let response = self
            .client
            .post(&self.config.device_authorization_url)
            .form(&[
                ("client_id", self.config.client_id.as_str()),
                ("scope", self.config.scope.as_str()),
            ])
            .send()
            .await
            .map_err(network_error)?;
        let value = json_response(
            checked_response(response, RequestAuthContext::Unauthenticated).await?,
            "device",
        )
        .await?;
        parse_device_authorization(value)
    }

    async fn poll_device_authorization(
        &self,
        device_code: &str,
    ) -> Result<OAuthPoll, GrokApiError> {
        let response = self
            .client
            .post(&self.config.token_url)
            .form(&[
                ("grant_type", "urn:ietf:params:oauth:grant-type:device_code"),
                ("device_code", device_code),
                ("client_id", self.config.client_id.as_str()),
            ])
            .send()
            .await
            .map_err(network_error)?;
        if response.status() == StatusCode::BAD_REQUEST {
            ensure_oauth_json_content_type(&response, "device poll error")?;
            let value = json_response(response, "device poll error").await?;
            return match value.get("error").and_then(Value::as_str) {
                Some("authorization_pending") => Ok(OAuthPoll::Pending),
                // RFC 8628 slow_down means "add 5s to the poll interval"; the
                // server's own interval never reaches the client, so the hint
                // is approximated by the client's default poll interval.
                Some("slow_down") => Err(GrokApiError::RateLimited {
                    message: "Grok device polling must slow down".into(),
                    retry_after_seconds: Some(5),
                }),
                Some("access_denied") => Ok(OAuthPoll::Denied),
                Some("expired_token") => Ok(OAuthPoll::Expired),
                Some("invalid_grant") => Err(GrokApiError::AuthenticationInvalid(
                    "Grok rejected the device authorization".into(),
                )),
                _ => Err(GrokApiError::ProtocolIncompatible(
                    "Grok returned an unknown device authorization error".into(),
                )),
            };
        }
        let value = json_response(
            checked_response(response, RequestAuthContext::CredentialPresent).await?,
            "token",
        )
        .await?;
        parse_token(value).map(OAuthPoll::Authorized)
    }

    async fn start_browser_authorization(
        &self,
        redirect_uri: &str,
    ) -> Result<BrowserAuthorization, GrokApiError> {
        let redirect = Url::parse(redirect_uri).map_err(|_| {
            GrokApiError::AuthenticationInvalid("Grok OAuth redirect URI is invalid".into())
        })?;
        if !is_secure_or_loopback(&redirect) {
            return Err(GrokApiError::AuthenticationInvalid(
                "Grok OAuth redirect URI must use HTTPS or loopback HTTP".into(),
            ));
        }
        let state = random_secret()?;
        let verifier = random_secret()?;
        let challenge = ullage_auth::pkce_s256_challenge(&verifier);
        let mut url = Url::parse(&self.config.authorization_url)
            .map_err(|_| GrokApiError::ProtocolIncompatible("invalid authorization URL".into()))?;
        url.query_pairs_mut()
            .append_pair("response_type", "code")
            .append_pair("client_id", &self.config.client_id)
            .append_pair("redirect_uri", redirect_uri)
            .append_pair("scope", &self.config.scope)
            .append_pair("state", &state)
            .append_pair("code_challenge", &challenge)
            .append_pair("code_challenge_method", "S256");
        let expires_at = Utc::now() + chrono::Duration::minutes(10);
        let mut flows = self.lock_flows();
        flows.retain(|_, flow| flow.expires_at > Utc::now());
        flows.insert(
            state.clone(),
            BrowserFlow {
                verifier,
                redirect_uri: redirect_uri.to_owned(),
                expires_at,
            },
        );
        drop(flows);
        Ok(BrowserAuthorization {
            flow_id: state,
            authorization_uri: url.into(),
            expires_at: Some(expires_at),
        })
    }

    async fn complete_browser_authorization(
        &self,
        flow_id: &str,
        authorization_code: &str,
        redirect_uri: &str,
    ) -> Result<OAuthToken, GrokApiError> {
        let flow = self.lock_flows().get(flow_id).cloned().ok_or_else(|| {
            GrokApiError::AuthenticationInvalid("unknown Grok OAuth state".into())
        })?;
        if redirect_uri != flow.redirect_uri {
            return Err(GrokApiError::AuthenticationInvalid(
                "Grok OAuth redirect URI does not match".into(),
            ));
        }
        if flow.expires_at <= Utc::now() {
            self.cancel_authorization(flow_id);
            return Err(GrokApiError::AuthenticationInvalid(
                "Grok OAuth state expired".into(),
            ));
        }
        let token = self
            .post_token(&[
                ("grant_type", "authorization_code"),
                ("code", authorization_code),
                ("redirect_uri", redirect_uri),
                ("client_id", self.config.client_id.as_str()),
                ("code_verifier", flow.verifier.as_str()),
            ])
            .await?;
        self.lock_flows().remove(flow_id);
        Ok(token)
    }

    async fn refresh(&self, refresh_token: &str) -> Result<OAuthToken, GrokApiError> {
        self.post_token(&[
            ("grant_type", "refresh_token"),
            ("refresh_token", refresh_token),
            ("client_id", self.config.client_id.as_str()),
        ])
        .await
    }

    async fn revoke(&self, token: &OAuthToken) -> Result<(), GrokApiError> {
        let credential = token
            .refresh_token
            .as_deref()
            .unwrap_or(&token.access_token);
        let response = self
            .client
            .post(&self.config.revoke_url)
            .form(&[
                ("token", credential),
                ("client_id", self.config.client_id.as_str()),
            ])
            .send()
            .await
            .map_err(network_error)?;
        checked_response(response, RequestAuthContext::CredentialPresent).await?;
        Ok(())
    }

    async fn fetch_billing(&self, access_token: &str) -> Result<Value, GrokApiError> {
        self.fetch_authenticated_json(&self.config.billing_url, access_token, "billing")
            .await
    }

    async fn fetch_settings(&self, access_token: &str) -> Result<Value, GrokApiError> {
        self.fetch_authenticated_json(&self.config.settings_url, access_token, "settings")
            .await
    }
}

fn validate_config(config: &HttpGrokConfig) -> Result<(), GrokApiError> {
    if config.client_id.trim().is_empty() {
        return Err(GrokApiError::ProtocolIncompatible(
            "Grok client id is empty".into(),
        ));
    }
    for endpoint in [
        &config.device_authorization_url,
        &config.authorization_url,
        &config.token_url,
        &config.revoke_url,
        &config.billing_url,
        &config.settings_url,
    ] {
        let url = Url::parse(endpoint)
            .map_err(|_| GrokApiError::ProtocolIncompatible("invalid Grok endpoint URL".into()))?;
        if !is_secure_or_loopback(&url) {
            return Err(GrokApiError::ProtocolIncompatible(
                "Grok endpoints must use HTTPS or loopback HTTP".into(),
            ));
        }
    }
    let redirect = Url::parse(&config.redirect_uri)
        .map_err(|_| GrokApiError::ProtocolIncompatible("invalid Grok redirect URI".into()))?;
    if !is_secure_or_loopback(&redirect) {
        return Err(GrokApiError::ProtocolIncompatible(
            "Grok redirect URI must be HTTPS or loopback HTTP".into(),
        ));
    }
    Ok(())
}

fn is_secure_or_loopback(url: &Url) -> bool {
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

async fn checked_response(
    response: reqwest::Response,
    auth_context: RequestAuthContext,
) -> Result<reqwest::Response, GrokApiError> {
    let status = response.status();
    if status.is_success() {
        return Ok(response);
    }
    let not_json = !response_content_type_is_json(&response);
    if status == StatusCode::UNAUTHORIZED || status == StatusCode::FORBIDDEN {
        return Err(match auth_context {
            RequestAuthContext::CredentialPresent => {
                GrokApiError::AuthenticationInvalid("Grok rejected the credential".into())
            }
            RequestAuthContext::Unauthenticated => GrokApiError::ProtocolIncompatible(
                unauthenticated_access_denied_message(status, not_json),
            ),
        });
    }
    if status == StatusCode::TOO_MANY_REQUESTS {
        let retry_after_seconds = response
            .headers()
            .get(reqwest::header::RETRY_AFTER)
            .and_then(|value| value.to_str().ok())
            .and_then(|value| value.parse().ok());
        return Err(GrokApiError::RateLimited {
            message: "Grok rate limit exceeded".into(),
            retry_after_seconds,
        });
    }
    if status.is_server_error() {
        return Err(GrokApiError::Network(format!(
            "Grok service returned HTTP {status}"
        )));
    }
    Err(GrokApiError::ProtocolIncompatible(
        unexpected_status_message(status, not_json),
    ))
}

async fn parse_token_response(response: reqwest::Response) -> Result<OAuthToken, GrokApiError> {
    if response.status() == StatusCode::BAD_REQUEST {
        ensure_oauth_json_content_type(&response, "token error")?;
        let value = json_response(response, "token error").await?;
        return match value.get("error").and_then(Value::as_str) {
            Some("invalid_grant") | Some("invalid_token") => Err(
                GrokApiError::AuthenticationInvalid("Grok rejected the OAuth credential".into()),
            ),
            // invalid_client faults the configured client, not the user's
            // credential, so re-authenticating cannot fix it.
            Some("invalid_client") => Err(GrokApiError::ProtocolIncompatible(
                "Grok rejected the OAuth client".into(),
            )),
            _ => Err(GrokApiError::ProtocolIncompatible(
                "Grok returned an unknown token error".into(),
            )),
        };
    }
    let value = json_response(
        checked_response(response, RequestAuthContext::CredentialPresent).await?,
        "token",
    )
    .await?;
    parse_token(value)
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum RequestAuthContext {
    Unauthenticated,
    CredentialPresent,
}

fn response_content_type_is_json(response: &reqwest::Response) -> bool {
    response
        .headers()
        .get(reqwest::header::CONTENT_TYPE)
        .and_then(|value| value.to_str().ok())
        .and_then(|content_type| content_type.split(';').next())
        .map(str::trim)
        .is_some_and(|media_type| {
            let media_type = media_type.to_ascii_lowercase();
            media_type == "application/json"
                || (media_type.starts_with("application/") && media_type.ends_with("+json"))
        })
}

fn unauthenticated_access_denied_message(status: StatusCode, not_json: bool) -> String {
    if not_json {
        format!(
            "Grok OAuth endpoint returned HTTP {status} with a non-JSON response; \
             the endpoint may be unavailable or misconfigured"
        )
    } else {
        format!(
            "Grok OAuth endpoint rejected the client request with HTTP {status}; \
             the client id may be invalid or the endpoint unavailable"
        )
    }
}

fn unexpected_status_message(status: StatusCode, not_json: bool) -> String {
    if not_json {
        format!(
            "Grok endpoint returned HTTP {status} with a non-JSON response; \
             expected OAuth JSON from the configured host"
        )
    } else {
        format!("Grok endpoint returned unexpected HTTP {status}")
    }
}

fn ensure_oauth_json_content_type(
    response: &reqwest::Response,
    context: &str,
) -> Result<(), GrokApiError> {
    if response_content_type_is_json(response) {
        Ok(())
    } else {
        let status = response.status();
        Err(GrokApiError::ProtocolIncompatible(format!(
            "Grok {context} response returned HTTP {status} with a non-JSON Content-Type; \
             expected OAuth JSON from the configured host"
        )))
    }
}

async fn json_response(
    mut response: reqwest::Response,
    context: &str,
) -> Result<Value, GrokApiError> {
    const MAX_RESPONSE_BYTES: usize = 2 * 1024 * 1024;
    if response
        .content_length()
        .is_some_and(|length| length > MAX_RESPONSE_BYTES as u64)
    {
        return Err(GrokApiError::ProtocolIncompatible(format!(
            "Grok {context} response exceeds the size limit"
        )));
    }
    let mut body = Vec::new();
    while let Some(chunk) = response.chunk().await.map_err(network_error)? {
        if body.len().saturating_add(chunk.len()) > MAX_RESPONSE_BYTES {
            return Err(GrokApiError::ProtocolIncompatible(format!(
                "Grok {context} response exceeds the size limit"
            )));
        }
        body.extend_from_slice(&chunk);
    }
    serde_json::from_slice(&body).map_err(|_| {
        GrokApiError::ProtocolIncompatible(format!("Grok {context} response is not valid JSON"))
    })
}

fn parse_device_authorization(value: Value) -> Result<DeviceAuthorization, GrokApiError> {
    let text = |name: &str| value.get(name).and_then(Value::as_str).map(str::to_owned);
    let expires_in = value
        .get("expires_in")
        .and_then(|value| value.as_i64().or_else(|| value.as_str()?.parse().ok()))
        .filter(|seconds| *seconds > 0)
        .ok_or_else(|| {
            GrokApiError::ProtocolIncompatible(
                "Grok device authorization expiry is missing or invalid".into(),
            )
        })?;
    let duration = chrono::Duration::try_seconds(expires_in).ok_or_else(|| {
        GrokApiError::ProtocolIncompatible("Grok device authorization expiry overflowed".into())
    })?;
    let expires_at = Utc::now().checked_add_signed(duration).ok_or_else(|| {
        GrokApiError::ProtocolIncompatible("Grok device authorization expiry overflowed".into())
    })?;
    // RFC 8628 `interval` is a poll hint in seconds; invalid values fall back
    // to absent rather than failing the flow.
    let poll_interval_seconds = value
        .get("interval")
        .and_then(|value| value.as_u64().or_else(|| value.as_str()?.parse().ok()));
    Ok(DeviceAuthorization {
        flow_id: random_secret()?,
        poll_interval_seconds,
        device_code: text("device_code").ok_or_else(|| {
            GrokApiError::ProtocolIncompatible("Grok device code is missing".into())
        })?,
        user_code: text("user_code").ok_or_else(|| {
            GrokApiError::ProtocolIncompatible("Grok user code is missing".into())
        })?,
        verification_uri: text("verification_uri_complete")
            .or_else(|| text("verification_uri"))
            .ok_or_else(|| {
                GrokApiError::ProtocolIncompatible("Grok verification URI is missing".into())
            })?,
        expires_at: Some(expires_at),
    })
}

const MAX_JWT_BYTES: usize = 16 * 1024;

fn parse_token(value: Value) -> Result<OAuthToken, GrokApiError> {
    let access_token = value
        .get("access_token")
        .and_then(Value::as_str)
        .filter(|token| !token.trim().is_empty())
        .ok_or_else(|| GrokApiError::ProtocolIncompatible("Grok access token is missing".into()))?
        .to_owned();
    let expires_at = match value.get("expires_in") {
        None => None,
        Some(value) => {
            let seconds = value
                .as_i64()
                .or_else(|| value.as_str()?.parse().ok())
                .filter(|seconds| *seconds > 0)
                .ok_or_else(|| {
                    GrokApiError::ProtocolIncompatible(
                        "Grok token expiry is missing or invalid".into(),
                    )
                })?;
            let duration = chrono::Duration::try_seconds(seconds).ok_or_else(|| {
                GrokApiError::ProtocolIncompatible("Grok token expiry overflowed".into())
            })?;
            Some(Utc::now().checked_add_signed(duration).ok_or_else(|| {
                GrokApiError::ProtocolIncompatible("Grok token expiry overflowed".into())
            })?)
        }
    };
    Ok(OAuthToken {
        access_token,
        refresh_token: value
            .get("refresh_token")
            .and_then(Value::as_str)
            .map(str::to_owned),
        expires_at,
        account_label: account_label_from_token_response(&value),
    })
}

/// Reads display-only identity claims from an OIDC token response.
///
/// Signature verification is intentionally omitted; claims are used only to label the stored
/// account, not as an authentication assertion.
fn account_label_from_token_response(value: &Value) -> Option<String> {
    for name in ["account_label", "email", "sub"] {
        if let Some(label) = value
            .get(name)
            .and_then(Value::as_str)
            .filter(|label| !label.trim().is_empty())
        {
            return Some(label.to_owned());
        }
    }
    value
        .get("id_token")
        .and_then(Value::as_str)
        .and_then(|token| jwt_claims(token).ok())
        .and_then(|claims| {
            // Prefer sub over email so refresh responses that omit email keep the same label.
            ["sub", "email"]
                .iter()
                .find_map(|name| {
                    claims
                        .get(*name)
                        .and_then(Value::as_str)
                        .filter(|label| !label.trim().is_empty())
                })
                .map(str::to_owned)
        })
}

fn jwt_claims(token: &str) -> Result<serde_json::Map<String, Value>, GrokApiError> {
    if token.len() > MAX_JWT_BYTES {
        return Err(GrokApiError::ProtocolIncompatible(
            "Grok identity token exceeds the supported size".into(),
        ));
    }
    let mut parts = token.split('.');
    let (_header, payload, signature) = (parts.next(), parts.next(), parts.next());
    if payload.is_none() || signature.is_none() || parts.next().is_some() {
        return Err(GrokApiError::ProtocolIncompatible(
            "Grok identity token is not a three-part JWT".into(),
        ));
    }
    let decoded = URL_SAFE_NO_PAD
        .decode(payload.unwrap_or_default())
        .map_err(|_| {
            GrokApiError::ProtocolIncompatible(
                "Grok identity token payload is not base64url".into(),
            )
        })?;
    if decoded.len() > MAX_JWT_BYTES {
        return Err(GrokApiError::ProtocolIncompatible(
            "Grok identity token claims exceed the supported size".into(),
        ));
    }
    serde_json::from_slice::<Value>(&decoded)
        .map_err(|_| {
            GrokApiError::ProtocolIncompatible(
                "Grok identity token payload is not valid JSON".into(),
            )
        })?
        .as_object()
        .cloned()
        .ok_or_else(|| {
            GrokApiError::ProtocolIncompatible(
                "Grok identity token payload is not an object".into(),
            )
        })
}

fn random_secret() -> Result<String, GrokApiError> {
    ullage_auth::random_url_token()
        .map_err(|_| GrokApiError::Network("secure random generation failed".into()))
}

fn network_error(_: reqwest::Error) -> GrokApiError {
    GrokApiError::Network("Grok network request failed".into())
}

#[cfg(test)]
mod tests {
    use serde_json::json;

    use super::*;

    #[test]
    fn rejects_extreme_oauth_expiries_without_panicking() {
        let device = parse_device_authorization(json!({
            "device_code": "device",
            "user_code": "user",
            "verification_uri": "https://accounts.example.invalid/device",
            "expires_in": i64::MAX
        }));
        assert!(matches!(device, Err(GrokApiError::ProtocolIncompatible(_))));

        let token = parse_token(json!({
            "access_token": "token",
            "expires_in": i64::MAX.to_string()
        }));
        assert!(matches!(token, Err(GrokApiError::ProtocolIncompatible(_))));
    }

    #[test]
    fn rejects_an_oauth_token_without_a_stable_account_identity() {
        let token = parse_token(json!({
            "access_token": "token",
            "refresh_token": "refresh",
            "expires_in": 3600
        }))
        .unwrap();

        assert!(matches!(
            token.validate(),
            Err(ProviderError::ProtocolIncompatible { .. })
        ));
    }

    #[test]
    fn prefers_sub_over_email_in_id_token_for_stable_refresh_identity() {
        let id_token = test_jwt(json!({
            "email": "user@example.com",
            "sub": "subject-fallback"
        }));
        let token = parse_token(json!({
            "access_token": "token",
            "refresh_token": "refresh",
            "expires_in": 3600,
            "id_token": id_token
        }))
        .unwrap();

        assert_eq!(token.account_label.as_deref(), Some("subject-fallback"));
        assert!(token.validate_stored().is_ok());
    }

    #[test]
    fn uses_email_from_id_token_when_sub_is_missing() {
        let id_token = test_jwt(json!({ "email": "user@example.com" }));
        let token = parse_token(json!({
            "access_token": "token",
            "refresh_token": "refresh",
            "expires_in": 3600,
            "id_token": id_token
        }))
        .unwrap();

        assert_eq!(token.account_label.as_deref(), Some("user@example.com"));
    }

    #[test]
    fn falls_back_to_id_token_sub_when_email_is_missing() {
        let id_token = test_jwt(json!({ "sub": "subject-only" }));
        let token = parse_token(json!({
            "access_token": "token",
            "refresh_token": "refresh",
            "expires_in": 3600,
            "id_token": id_token
        }))
        .unwrap();

        assert_eq!(token.account_label.as_deref(), Some("subject-only"));
    }

    #[test]
    fn rejects_id_token_without_identity_claims() {
        let id_token = test_jwt(json!({ "aud": "client" }));
        let token = parse_token(json!({
            "access_token": "token",
            "refresh_token": "refresh",
            "expires_in": 3600,
            "id_token": id_token
        }))
        .unwrap();

        assert!(token.account_label.is_none());
        assert!(matches!(
            token.validate_stored(),
            Err(ProviderError::ProtocolIncompatible { .. })
        ));
    }

    #[test]
    fn refresh_id_token_with_sub_only_keeps_the_same_account_label() {
        let authorize = parse_token(json!({
            "access_token": "token",
            "refresh_token": "refresh",
            "expires_in": 3600,
            "id_token": test_jwt(json!({
                "email": "user@example.com",
                "sub": "subject-1"
            }))
        }))
        .unwrap();
        let refreshed = parse_token(json!({
            "access_token": "token-2",
            "refresh_token": "refresh-2",
            "expires_in": 3600,
            "id_token": test_jwt(json!({ "sub": "subject-1" }))
        }))
        .unwrap();

        assert_eq!(
            authorize.account_label.as_deref(),
            refreshed.account_label.as_deref()
        );
        assert_eq!(authorize.account_label.as_deref(), Some("subject-1"));
    }

    fn test_jwt(claims: Value) -> String {
        let header = URL_SAFE_NO_PAD.encode(br#"{"alg":"none","typ":"JWT"}"#);
        let payload = URL_SAFE_NO_PAD.encode(serde_json::to_vec(&claims).unwrap());
        format!("{header}.{payload}.sig")
    }
}
