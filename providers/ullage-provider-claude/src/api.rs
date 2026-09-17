use async_trait::async_trait;
use reqwest::header::{ACCEPT, AUTHORIZATION, CONTENT_TYPE, HeaderValue, RETRY_AFTER, USER_AGENT};
use serde::de::DeserializeOwned;
use serde::{Deserialize, Serialize};
use std::time::SystemTime;
use ullage_core::{ProviderError, ProviderResult};

use crate::{ClaudeProfile, ClaudeUsageResponse};

const API_BASE: &str = "https://api.anthropic.com";
const TOKEN_ENDPOINT: &str = "https://platform.claude.com/v1/oauth/token";
const REVOKE_ENDPOINT: &str = "https://platform.claude.com/v1/oauth/token/revoke";
const OAUTH_BETA: &str = "oauth-2025-04-20";
const USER_AGENT_VALUE: &str = concat!("ullage/", env!("CARGO_PKG_VERSION"));
const MAX_RESPONSE_BYTES: usize = 1024 * 1024;

/// Endpoints the HTTP adapter talks to. `Default` is production; tests point
/// the same adapter at a local mock so the wire shape is what gets checked.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ClaudeHttpConfig {
    pub api_base: String,
    pub token_endpoint: String,
    pub revoke_endpoint: String,
}

impl Default for ClaudeHttpConfig {
    fn default() -> Self {
        Self {
            api_base: API_BASE.into(),
            token_endpoint: TOKEN_ENDPOINT.into(),
            revoke_endpoint: REVOKE_ENDPOINT.into(),
        }
    }
}

#[derive(Clone, PartialEq, Eq)]
pub struct AuthorizationCodeExchange {
    pub code: String,
    pub state: String,
    pub redirect_uri: String,
    pub code_verifier: String,
}

impl std::fmt::Debug for AuthorizationCodeExchange {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter
            .debug_struct("AuthorizationCodeExchange")
            .field("code", &"[REDACTED]")
            .field("state", &"[REDACTED]")
            .field("redirect_uri", &self.redirect_uri)
            .field("code_verifier", &"[REDACTED]")
            .finish()
    }
}

#[derive(Clone, PartialEq, Eq, Deserialize)]
pub struct ClaudeTokenResponse {
    pub access_token: String,
    #[serde(default)]
    pub refresh_token: Option<String>,
    #[serde(default)]
    pub expires_in: Option<i64>,
    #[serde(default)]
    pub token_type: Option<String>,
}

impl std::fmt::Debug for ClaudeTokenResponse {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter
            .debug_struct("ClaudeTokenResponse")
            .field("access_token", &"[REDACTED]")
            .field(
                "refresh_token",
                &self.refresh_token.as_ref().map(|_| "[REDACTED]"),
            )
            .field("expires_in", &self.expires_in)
            .field("token_type", &self.token_type)
            .finish()
    }
}

#[async_trait]
pub trait ClaudeApi: Send + Sync {
    async fn exchange_code(
        &self,
        request: AuthorizationCodeExchange,
    ) -> ProviderResult<ClaudeTokenResponse>;
    async fn refresh_token(&self, refresh_token: &str) -> ProviderResult<ClaudeTokenResponse>;
    async fn revoke_token(&self, token: &str, token_type_hint: &str) -> ProviderResult<()>;
    async fn profile(&self, access_token: &str) -> ProviderResult<ClaudeProfile>;
    async fn usage(&self, access_token: &str) -> ProviderResult<ClaudeUsageResponse>;
}

#[derive(Clone)]
pub struct HttpClaudeApi {
    client: reqwest::Client,
    config: ClaudeHttpConfig,
}

impl HttpClaudeApi {
    pub fn new() -> ProviderResult<Self> {
        Self::with_config(ClaudeHttpConfig::default())
    }

    pub fn with_config(config: ClaudeHttpConfig) -> ProviderResult<Self> {
        let client = reqwest::Client::builder()
            .redirect(reqwest::redirect::Policy::none())
            .timeout(std::time::Duration::from_secs(30))
            .build()
            .map_err(|_| ProviderError::Network {
                message: "failed to initialize the Claude HTTP client".into(),
            })?;
        Ok(Self { client, config })
    }

    async fn authenticated_get<T: DeserializeOwned>(
        &self,
        path: &'static str,
        token: &str,
    ) -> ProviderResult<T> {
        let response = self
            .client
            .get(format!("{}{path}", self.config.api_base))
            .header(AUTHORIZATION, bearer(token)?)
            .header("anthropic-beta", OAUTH_BETA)
            .header(ACCEPT, "application/json")
            .header(USER_AGENT, USER_AGENT_VALUE)
            .send()
            .await
            .map_err(network_error)?;
        decode_json(response).await
    }
}

#[async_trait]
impl ClaudeApi for HttpClaudeApi {
    async fn exchange_code(
        &self,
        request: AuthorizationCodeExchange,
    ) -> ProviderResult<ClaudeTokenResponse> {
        #[derive(Serialize)]
        struct Body<'a> {
            grant_type: &'static str,
            client_id: &'static str,
            code: &'a str,
            state: &'a str,
            redirect_uri: &'a str,
            code_verifier: &'a str,
        }

        let response = self
            .client
            .post(&self.config.token_endpoint)
            .header(CONTENT_TYPE, "application/json")
            .header(ACCEPT, "application/json")
            .header(USER_AGENT, USER_AGENT_VALUE)
            .json(&Body {
                grant_type: "authorization_code",
                client_id: crate::CLAUDE_CLIENT_ID,
                code: &request.code,
                state: &request.state,
                redirect_uri: &request.redirect_uri,
                code_verifier: &request.code_verifier,
            })
            .send()
            .await
            .map_err(network_error)?;
        decode_json(response).await
    }

    async fn refresh_token(&self, refresh_token: &str) -> ProviderResult<ClaudeTokenResponse> {
        #[derive(Serialize)]
        struct Body<'a> {
            grant_type: &'static str,
            client_id: &'static str,
            refresh_token: &'a str,
            scope: &'static str,
        }

        let response = self
            .client
            .post(&self.config.token_endpoint)
            .header(CONTENT_TYPE, "application/json")
            .header(ACCEPT, "application/json")
            .header(USER_AGENT, USER_AGENT_VALUE)
            .json(&Body {
                grant_type: "refresh_token",
                client_id: crate::CLAUDE_CLIENT_ID,
                refresh_token,
                scope: crate::OAUTH_SCOPES,
            })
            .send()
            .await
            .map_err(network_error)?;
        decode_json(response).await
    }

    async fn revoke_token(&self, token: &str, token_type_hint: &str) -> ProviderResult<()> {
        #[derive(Serialize)]
        struct Body<'a> {
            token: &'a str,
            token_type_hint: &'a str,
            client_id: &'static str,
        }

        let response = self
            .client
            .post(&self.config.revoke_endpoint)
            .header(CONTENT_TYPE, "application/json")
            .header(ACCEPT, "application/json")
            .header(USER_AGENT, USER_AGENT_VALUE)
            .json(&Body {
                token,
                token_type_hint,
                client_id: crate::CLAUDE_CLIENT_ID,
            })
            .send()
            .await
            .map_err(network_error)?;
        decode_empty(response).await
    }

    async fn profile(&self, access_token: &str) -> ProviderResult<ClaudeProfile> {
        self.authenticated_get("/api/oauth/profile", access_token)
            .await
    }

    async fn usage(&self, access_token: &str) -> ProviderResult<ClaudeUsageResponse> {
        self.authenticated_get("/api/oauth/usage", access_token)
            .await
    }
}

fn bearer(token: &str) -> ProviderResult<HeaderValue> {
    let value = format!("Bearer {token}");
    HeaderValue::from_str(&value).map_err(|_| ProviderError::AuthenticationInvalid {
        message: "stored Claude access token contains invalid header characters".into(),
    })
}

fn network_error(_: reqwest::Error) -> ProviderError {
    ProviderError::Network {
        message: "request to Claude failed".into(),
    }
}

async fn decode_json<T: DeserializeOwned>(response: reqwest::Response) -> ProviderResult<T> {
    if !response.status().is_success() {
        return Err(error_response(response).await);
    }
    let body = bounded_body(response).await?;
    serde_json::from_slice(&body).map_err(|_| ProviderError::ProtocolIncompatible {
        message: "Claude returned an incompatible JSON response".into(),
    })
}

async fn decode_empty(response: reqwest::Response) -> ProviderResult<()> {
    if response.status().is_success() {
        Ok(())
    } else {
        Err(error_response(response).await)
    }
}

/// Classifies a failed response. The body still goes through the bounded
/// reader so an OAuth error code (not its free-text description, which is
/// never echoed) can refine the classification.
async fn error_response(response: reqwest::Response) -> ProviderError {
    let status = response.status();
    let retry_after_seconds = retry_after_seconds(&response);
    let body = match bounded_body(response).await {
        Ok(body) => body,
        Err(error) => return error,
    };
    status_error(status.as_u16(), retry_after_seconds, &body)
}

fn retry_after_seconds(response: &reqwest::Response) -> Option<u64> {
    response
        .headers()
        .get(RETRY_AFTER)
        .and_then(|value| value.to_str().ok())
        .and_then(|value| parse_retry_after(value, SystemTime::now()))
}

fn parse_retry_after(value: &str, now: SystemTime) -> Option<u64> {
    value.parse().ok().or_else(|| {
        httpdate::parse_http_date(value).ok().map(|deadline| {
            deadline.duration_since(now).map_or(0, |duration| {
                duration
                    .as_secs()
                    .saturating_add(u64::from(duration.subsec_nanos() != 0))
            })
        })
    })
}

async fn bounded_body(mut response: reqwest::Response) -> ProviderResult<Vec<u8>> {
    let mut body = Vec::new();
    while let Some(chunk) = response.chunk().await.map_err(network_error)? {
        if body.len().saturating_add(chunk.len()) > MAX_RESPONSE_BYTES {
            return Err(ProviderError::ProtocolIncompatible {
                message: "Claude response exceeded the 1 MiB safety limit".into(),
            });
        }
        body.extend_from_slice(&chunk);
    }
    Ok(body)
}

#[derive(Deserialize)]
struct OAuthErrorBody {
    error: String,
}

/// Reads the RFC 6749 `error` member out of an error response body. Anything
/// that is not a JSON object carrying a string `error` is simply no code.
fn oauth_error_code(body: &[u8]) -> Option<String> {
    serde_json::from_slice::<OAuthErrorBody>(body)
        .ok()
        .map(|parsed| parsed.error)
}

pub(crate) fn status_error(
    status: u16,
    retry_after_seconds: Option<u64>,
    body: &[u8],
) -> ProviderError {
    match status {
        401 => ProviderError::AuthenticationInvalid {
            message: "Claude returned HTTP 401 (unauthorized)".into(),
        },
        403 => ProviderError::AuthenticationInvalid {
            message: "Claude returned HTTP 403 (forbidden)".into(),
        },
        408 => ProviderError::Network {
            message: "Claude returned HTTP 408 (request timeout)".into(),
        },
        429 => ProviderError::RateLimited {
            message: "Claude returned HTTP 429 (rate limited)".into(),
            retry_after_seconds,
        },
        _ if status >= 500 => ProviderError::Network {
            message: format!("Claude returned server error HTTP {status}"),
        },
        // The grant itself was rejected: the credential is dead and needs
        // re-authentication, not a retry. Every other client-side failure —
        // invalid_client, a malformed error body, an unknown code — stays a
        // protocol failure.
        _ if oauth_error_code(body).as_deref() == Some("invalid_grant") => {
            ProviderError::AuthenticationInvalid {
                message: "Claude rejected the stored OAuth credential (invalid_grant)".into(),
            }
        }
        _ => ProviderError::ProtocolIncompatible {
            message: format!("Claude returned unexpected HTTP status {status}"),
        },
    }
}

#[cfg(test)]
mod tests {
    use std::time::{Duration, UNIX_EPOCH};

    use super::*;

    #[test]
    fn distinguishes_security_and_rate_limit_statuses() {
        assert!(matches!(
            status_error(401, None, b""),
            ProviderError::AuthenticationInvalid { message } if message.contains("401")
        ));
        assert!(matches!(
            status_error(403, None, b""),
            ProviderError::AuthenticationInvalid { message } if message.contains("403")
        ));
        assert_eq!(
            status_error(429, Some(17), b""),
            ProviderError::RateLimited {
                message: "Claude returned HTTP 429 (rate limited)".into(),
                retry_after_seconds: Some(17),
            }
        );
    }

    #[test]
    fn maps_oauth_invalid_grant_to_invalid_authentication() {
        assert!(matches!(
            status_error(400, None, br#"{"error":"invalid_grant"}"#),
            ProviderError::AuthenticationInvalid { .. }
        ));
        // A credential error still wins on an unexpected status.
        assert!(matches!(
            status_error(422, None, br#"{"error":"invalid_grant"}"#),
            ProviderError::AuthenticationInvalid { .. }
        ));
    }

    #[test]
    fn keeps_other_oauth_errors_as_protocol_failures() {
        for body in [
            &br#"{"error":"invalid_client"}"#[..],
            &br#"{"error":"unknown_error"}"#[..],
            b"not json",
            br#"{"error":{"nested":"object"}}"#,
            b"",
        ] {
            assert!(matches!(
                status_error(400, None, body),
                ProviderError::ProtocolIncompatible { .. }
            ));
        }
    }

    #[test]
    fn maps_timeouts_and_server_errors_to_network() {
        for status in [408, 500, 502, 503, 599] {
            assert!(matches!(
                status_error(status, None, br#"{"error":"invalid_grant"}"#),
                ProviderError::Network { .. }
            ));
        }
    }

    #[test]
    fn parses_retry_after_delta_seconds_and_http_dates() {
        let now = UNIX_EPOCH + Duration::from_secs(2_000_000_000);
        assert_eq!(parse_retry_after("17", now), Some(17));
        assert_eq!(
            parse_retry_after(&httpdate::fmt_http_date(now + Duration::from_secs(23)), now,),
            Some(23)
        );
        let half_second_later = now + Duration::from_millis(500);
        assert_eq!(
            parse_retry_after(
                &httpdate::fmt_http_date(now + Duration::from_secs(1)),
                half_second_later,
            ),
            Some(1)
        );
        assert_eq!(
            parse_retry_after(&httpdate::fmt_http_date(now - Duration::from_secs(1)), now,),
            Some(0)
        );
        assert_eq!(parse_retry_after("not-a-delay", now), None);
    }

    #[test]
    fn redacts_authorization_exchange_debug_output() {
        let request = AuthorizationCodeExchange {
            code: "secret-code".into(),
            state: "secret-state".into(),
            redirect_uri: "https://example.invalid/callback".into(),
            code_verifier: "secret-verifier".into(),
        };
        let output = format!("{request:?}");
        assert!(!output.contains("secret-code"));
        assert!(!output.contains("secret-state"));
        assert!(!output.contains("secret-verifier"));
    }
}
