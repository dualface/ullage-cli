use std::time::Duration as StdDuration;

use async_trait::async_trait;
use base64::Engine;
use base64::engine::general_purpose::URL_SAFE_NO_PAD;
use chrono::{Duration, Utc};
use reqwest::header::{AUTHORIZATION, CONTENT_TYPE, HeaderMap, HeaderValue, RETRY_AFTER};
use reqwest::{Client, StatusCode, Url};
use serde::Deserialize;
use serde::de::DeserializeOwned;

use crate::{
    ChatGptApi, ChatGptApiError, ChatGptApiErrorKind, ChatGptUsageResponse, ChatGptWorkspace,
    OAuthTokenSet,
};

const MAX_JWT_BYTES: usize = 64 * 1024;
const MAX_RESPONSE_BYTES: usize = 1024 * 1024;
const MAX_TOKEN_BYTES: usize = 64 * 1024;
// Official Codex CLI originator. Ullage uses the official Codex OAuth client
// and must send the same identity on usage requests or Cloudflare challenges
// the request at the edge.
const USAGE_ORIGINATOR: &str = "codex_cli_rs";

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ChatGptHttpConfig {
    pub client_id: String,
    pub token_endpoint: String,
    pub revoke_endpoint: String,
    pub usage_endpoint: String,
}

impl ChatGptHttpConfig {
    pub fn openai(client_id: impl Into<String>) -> Self {
        Self {
            client_id: client_id.into(),
            token_endpoint: "https://auth.openai.com/oauth/token".into(),
            revoke_endpoint: "https://auth.openai.com/oauth/revoke".into(),
            usage_endpoint: "https://chatgpt.com/backend-api/codex/usage".into(),
        }
    }
}

pub struct ReqwestChatGptApi {
    config: ChatGptHttpConfig,
    client: Client,
}

impl ReqwestChatGptApi {
    pub fn new(config: ChatGptHttpConfig) -> Result<Self, ChatGptApiError> {
        validate_config(&config)?;
        let client = Client::builder()
            .redirect(reqwest::redirect::Policy::none())
            .timeout(StdDuration::from_secs(30))
            .user_agent(concat!("ullage/", env!("CARGO_PKG_VERSION")))
            .build()
            .map_err(|error| network_error("build HTTP client", error))?;
        Ok(Self { config, client })
    }

    async fn request_tokens(
        &self,
        form: &[(&str, &str)],
        require_refresh_token: bool,
    ) -> Result<OAuthTokenSet, ChatGptApiError> {
        let response = self
            .client
            .post(&self.config.token_endpoint)
            .form(form)
            .send()
            .await
            .map_err(|error| network_error("send token request", error))?;
        if !response.status().is_success() {
            return Err(classify_status(
                "OAuth token request",
                response.status(),
                response.headers(),
                true,
            ));
        }
        let response = decode_json::<TokenResponse>(response, "OAuth token response").await?;
        token_set(response, require_refresh_token)
    }
}

#[async_trait]
impl ChatGptApi for ReqwestChatGptApi {
    async fn exchange_code(
        &self,
        authorization_code: &str,
        pkce_verifier: &str,
        redirect_uri: &str,
    ) -> Result<OAuthTokenSet, ChatGptApiError> {
        self.request_tokens(
            &[
                ("grant_type", "authorization_code"),
                ("client_id", &self.config.client_id),
                ("code", authorization_code),
                ("redirect_uri", redirect_uri),
                ("code_verifier", pkce_verifier),
            ],
            true,
        )
        .await
    }

    async fn refresh_token(&self, refresh_token: &str) -> Result<OAuthTokenSet, ChatGptApiError> {
        self.request_tokens(
            &[
                ("grant_type", "refresh_token"),
                ("client_id", &self.config.client_id),
                ("refresh_token", refresh_token),
            ],
            false,
        )
        .await
    }

    async fn list_workspaces(
        &self,
        tokens: &OAuthTokenSet,
    ) -> Result<Vec<ChatGptWorkspace>, ChatGptApiError> {
        let claims = tokens
            .identity_token()
            .and_then(|token| jwt_claims(token).ok())
            .or_else(|| jwt_claims(tokens.access_token()).ok())
            .ok_or_else(|| protocol_error("OAuth tokens do not contain readable JWT claims"))?;
        let auth_claims = claims
            .get("https://api.openai.com/auth")
            .and_then(serde_json::Value::as_object)
            .ok_or_else(|| protocol_error("JWT omitted OpenAI auth claims"))?;
        let account_id = auth_claims
            .get("chatgpt_account_id")
            .and_then(serde_json::Value::as_str)
            .filter(|value| !value.is_empty())
            .ok_or_else(|| protocol_error("JWT omitted ChatGPT account/workspace ID"))?;
        let label = auth_claims
            .get("organizations")
            .and_then(serde_json::Value::as_array)
            .and_then(|organizations| {
                organizations.iter().find(|organization| {
                    organization.get("id").and_then(serde_json::Value::as_str) == Some(account_id)
                })
            })
            .and_then(|organization| {
                organization
                    .get("title")
                    .or_else(|| organization.get("name"))
                    .and_then(serde_json::Value::as_str)
            })
            .map(str::to_owned);
        Ok(vec![ChatGptWorkspace {
            id: account_id.to_owned(),
            label,
        }])
    }

    async fn query_usage(
        &self,
        tokens: &OAuthTokenSet,
        workspace_id: &str,
    ) -> Result<ChatGptUsageResponse, ChatGptApiError> {
        let workspace_header = HeaderValue::from_str(workspace_id)
            .map_err(|_| protocol_error("workspace ID cannot be represented as an HTTP header"))?;
        let response = self
            .client
            .get(&self.config.usage_endpoint)
            .header(AUTHORIZATION, format!("Bearer {}", tokens.access_token()))
            .header("ChatGPT-Account-ID", workspace_header)
            .header("originator", USAGE_ORIGINATOR)
            .header("version", env!("CARGO_PKG_VERSION"))
            .send()
            .await
            .map_err(|error| network_error("query ChatGPT usage", error))?;
        if !response.status().is_success() {
            return Err(classify_status(
                "ChatGPT usage request",
                response.status(),
                response.headers(),
                false,
            ));
        }
        decode_json(response, "ChatGPT usage response").await
    }

    async fn revoke(&self, tokens: &OAuthTokenSet) -> Result<(), ChatGptApiError> {
        let (token, token_type_hint) = tokens
            .refresh_token()
            .map(|token| (token, "refresh_token"))
            .unwrap_or_else(|| (tokens.access_token(), "access_token"));
        let response = self
            .client
            .post(&self.config.revoke_endpoint)
            .json(&serde_json::json!({
                "token": token,
                "token_type_hint": token_type_hint,
                "client_id": self.config.client_id,
            }))
            .send()
            .await
            .map_err(|error| network_error("revoke OAuth token", error))?;
        if response.status().is_success() {
            Ok(())
        } else {
            Err(classify_status(
                "OAuth revoke request",
                response.status(),
                response.headers(),
                true,
            ))
        }
    }
}

#[derive(Deserialize)]
struct TokenResponse {
    access_token: String,
    #[serde(default)]
    refresh_token: Option<String>,
    #[serde(default)]
    id_token: Option<String>,
    #[serde(default)]
    expires_in: Option<i64>,
}

fn token_set(
    response: TokenResponse,
    require_refresh_token: bool,
) -> Result<OAuthTokenSet, ChatGptApiError> {
    if response.access_token.len() > MAX_TOKEN_BYTES
        || response
            .refresh_token
            .as_ref()
            .is_some_and(|token| token.len() > MAX_TOKEN_BYTES)
        || response
            .id_token
            .as_ref()
            .is_some_and(|token| token.len() > MAX_TOKEN_BYTES)
    {
        return Err(protocol_error("OAuth token exceeds the supported size"));
    }
    HeaderValue::from_str(&response.access_token)
        .map_err(|_| protocol_error("OAuth access token cannot be used as an HTTP header"))?;
    if require_refresh_token && response.refresh_token.as_deref().is_none_or(str::is_empty) {
        return Err(protocol_error("OAuth token response omitted refresh token"));
    }
    if response.expires_in.is_some_and(|seconds| seconds < 0) {
        return Err(protocol_error("OAuth expires_in cannot be negative"));
    }
    let expires_at = response
        .expires_in
        .map(|seconds| {
            Duration::try_seconds(seconds)
                .and_then(|duration| Utc::now().checked_add_signed(duration))
                .ok_or_else(|| protocol_error("OAuth expires_in is outside supported range"))
        })
        .transpose()?;
    OAuthTokenSet::new(response.access_token, response.refresh_token, expires_at)
        .and_then(|tokens| tokens.with_identity_token(response.id_token))
        .map_err(|_| protocol_error("OAuth token response omitted access token"))
}

async fn decode_json<T: DeserializeOwned>(
    mut response: reqwest::Response,
    context: &str,
) -> Result<T, ChatGptApiError> {
    if response
        .content_length()
        .is_some_and(|length| length > MAX_RESPONSE_BYTES as u64)
    {
        return Err(protocol_error(format!(
            "{context} exceeds the supported size"
        )));
    }
    let mut body = Vec::new();
    while let Some(chunk) = response
        .chunk()
        .await
        .map_err(|error| network_error("read HTTP response", error))?
    {
        if body.len().saturating_add(chunk.len()) > MAX_RESPONSE_BYTES {
            return Err(protocol_error(format!(
                "{context} exceeds the supported size"
            )));
        }
        body.extend_from_slice(&chunk);
    }
    serde_json::from_slice(&body)
        .map_err(|_| protocol_error(format!("{context} is not compatible JSON")))
}

fn jwt_claims(token: &str) -> Result<serde_json::Map<String, serde_json::Value>, ChatGptApiError> {
    if token.len() > MAX_JWT_BYTES {
        return Err(protocol_error("JWT exceeds the supported size"));
    }
    let mut parts = token.split('.');
    let (_header, payload, signature) = (parts.next(), parts.next(), parts.next());
    if payload.is_none() || signature.is_none() || parts.next().is_some() {
        return Err(protocol_error("OAuth token is not a three-part JWT"));
    }
    let decoded = URL_SAFE_NO_PAD
        .decode(payload.unwrap_or_default())
        .map_err(|_| protocol_error("JWT payload is not base64url"))?;
    if decoded.len() > MAX_JWT_BYTES {
        return Err(protocol_error("JWT claims exceed the supported size"));
    }
    serde_json::from_slice::<serde_json::Value>(&decoded)
        .map_err(|_| protocol_error("JWT payload is not compatible JSON"))?
        .as_object()
        .cloned()
        .ok_or_else(|| protocol_error("JWT payload is not an object"))
}

fn validate_config(config: &ChatGptHttpConfig) -> Result<(), ChatGptApiError> {
    if config.client_id.trim().is_empty() {
        return Err(protocol_error("OAuth client ID is required"));
    }
    for (name, value) in [
        ("token", &config.token_endpoint),
        ("revoke", &config.revoke_endpoint),
        ("usage", &config.usage_endpoint),
    ] {
        let url = Url::parse(value)
            .map_err(|_| protocol_error(format!("{name} endpoint is not a valid URL")))?;
        let local_http = url.scheme() == "http"
            && url.host_str().is_some_and(|host| {
                host == "localhost"
                    || host
                        .parse::<std::net::IpAddr>()
                        .is_ok_and(|ip| ip.is_loopback())
            });
        if url.scheme() != "https" && !local_http {
            return Err(protocol_error(format!(
                "{name} endpoint must use HTTPS (loopback HTTP is allowed for tests)"
            )));
        }
        if url.fragment().is_some() || url.username() != "" || url.password().is_some() {
            return Err(protocol_error(format!(
                "{name} endpoint cannot contain credentials or a fragment"
            )));
        }
    }
    Ok(())
}

fn classify_status(
    operation: &str,
    status: StatusCode,
    headers: &HeaderMap,
    oauth_endpoint: bool,
) -> ChatGptApiError {
    if is_edge_challenge(status, headers) {
        return edge_challenge_error(operation, status, headers);
    }
    let message = format!("{operation} returned HTTP {}", status.as_u16());
    match status {
        StatusCode::UNAUTHORIZED => {
            ChatGptApiError::new(ChatGptApiErrorKind::AuthenticationInvalid, message)
        }
        StatusCode::BAD_REQUEST if oauth_endpoint => {
            ChatGptApiError::new(ChatGptApiErrorKind::AuthenticationInvalid, message)
        }
        StatusCode::FORBIDDEN if oauth_endpoint => {
            ChatGptApiError::new(ChatGptApiErrorKind::AuthenticationInvalid, message)
        }
        StatusCode::FORBIDDEN => {
            ChatGptApiError::new(ChatGptApiErrorKind::WorkspaceAccessDenied, message)
        }
        StatusCode::TOO_MANY_REQUESTS => ChatGptApiError::rate_limited(
            message,
            headers
                .get(RETRY_AFTER)
                .and_then(|value| value.to_str().ok())
                .and_then(|value| value.parse().ok()),
        ),
        status if status.is_server_error() => {
            ChatGptApiError::new(ChatGptApiErrorKind::Network, message)
        }
        _ => ChatGptApiError::new(ChatGptApiErrorKind::ProtocolIncompatible, message),
    }
}

fn is_edge_challenge(status: StatusCode, headers: &HeaderMap) -> bool {
    has_cf_mitigated(headers)
        || (matches!(
            status,
            StatusCode::FORBIDDEN | StatusCode::SERVICE_UNAVAILABLE
        ) && !is_json_content_type(headers))
}

fn has_cf_mitigated(headers: &HeaderMap) -> bool {
    headers
        .get("cf-mitigated")
        .and_then(|value| value.to_str().ok())
        .is_some_and(|value| !value.trim().is_empty())
}

fn is_json_content_type(headers: &HeaderMap) -> bool {
    headers
        .get(CONTENT_TYPE)
        .and_then(|value| value.to_str().ok())
        .and_then(|value| value.split(';').next())
        .map(str::trim)
        .is_some_and(|media_type| {
            let media_type = media_type.to_ascii_lowercase();
            media_type == "application/json"
                || (media_type.starts_with("application/") && media_type.ends_with("+json"))
        })
}

fn edge_challenge_error(
    operation: &str,
    status: StatusCode,
    headers: &HeaderMap,
) -> ChatGptApiError {
    let mut message = format!(
        "{operation} returned HTTP {} (Cloudflare edge challenge)",
        status.as_u16()
    );
    if let Some(cf_ray) = headers
        .get("cf-ray")
        .and_then(|value| value.to_str().ok())
        .map(str::trim)
        .filter(|value| !value.is_empty())
    {
        message.push_str("; cf-ray ");
        message.push_str(cf_ray);
    }
    ChatGptApiError::new(ChatGptApiErrorKind::Network, message)
}

fn network_error(operation: &str, _: reqwest::Error) -> ChatGptApiError {
    ChatGptApiError::new(ChatGptApiErrorKind::Network, format!("{operation} failed"))
}

fn protocol_error(message: impl Into<String>) -> ChatGptApiError {
    ChatGptApiError::new(ChatGptApiErrorKind::ProtocolIncompatible, message)
}
