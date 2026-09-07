mod api;
mod dto;

use std::sync::{Arc, Mutex};

use async_trait::async_trait;
use base64::Engine;
use base64::engine::general_purpose::URL_SAFE_NO_PAD;
use chrono::{DateTime, Duration, Utc};
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};
use tokio::sync::Mutex as AsyncMutex;
use ullage_auth::{
    AuthChallenge, AuthCompleteRequest, AuthInputRequest, AuthMethod, AuthStartRequest, AuthState,
    LogoutRequest, validate_loopback_http_redirect_uri,
};
use ullage_core::{
    Capability, MeasurementUnit, PartialFailure, Provider, ProviderDescriptor, ProviderError,
    ProviderId, ProviderResult, QueryOutcome, SubscriptionUsage, UsageMeasurement, UsageQuery,
    UsageWindow, UsageWindowKind,
};
use url::Url;

pub use api::{AuthorizationCodeExchange, ClaudeApi, ClaudeTokenResponse, HttpClaudeApi};
pub use dto::{
    ClaudeAccount, ClaudeExtraUsage, ClaudeLimit, ClaudeLimitScope, ClaudeOrganization,
    ClaudeProfile, ClaudeScopeLabel, ClaudeUsageResponse, ClaudeUsageWindow,
};

pub(crate) const CLAUDE_CLIENT_ID: &str = "9d1c250a-e61b-44d9-88ed-5944d1962f5e";
const AUTHORIZE_ENDPOINT: &str = "https://claude.com/cai/oauth/authorize";
const REDIRECT_URI: &str = "https://platform.claude.com/oauth/code/callback";
pub(crate) const OAUTH_SCOPES: &str =
    "user:profile user:inference user:sessions:claude_code user:mcp_servers user:file_upload";
const AUTH_FLOW_LIFETIME_MINUTES: i64 = 10;
const REFRESH_EARLY_SECONDS: i64 = 300;

#[derive(Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ClaudeCredential {
    pub access_token: String,
    pub refresh_token: Option<String>,
    pub expires_at: Option<DateTime<Utc>>,
    /// Email of the signed-in account, read from the profile once at sign-in.
    /// Absent on credentials stored before this was recorded, and on any account
    /// whose profile does not carry one.
    #[serde(default)]
    pub account_label: Option<String>,
}

impl ClaudeCredential {
    pub fn validate(&self) -> ProviderResult<()> {
        if self.access_token.trim().is_empty() {
            return Err(ProviderError::ProtocolIncompatible {
                message: "Claude credential has an empty access token".into(),
            });
        }
        if self
            .refresh_token
            .as_deref()
            .is_none_or(|token| token.trim().is_empty())
        {
            return Err(ProviderError::ProtocolIncompatible {
                message: "Claude credential has no usable refresh token".into(),
            });
        }
        Ok(())
    }
}

impl std::fmt::Debug for ClaudeCredential {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter
            .debug_struct("ClaudeCredential")
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

pub trait ClaudeCredentialStore: Send + Sync {
    fn load(&self) -> ProviderResult<Option<ClaudeCredential>>;
    fn save(&self, credential: &ClaudeCredential) -> ProviderResult<()>;
    fn clear(&self) -> ProviderResult<()>;
}

#[derive(Clone, Debug, PartialEq)]
pub struct ClaudeUsage {
    pub profile: Option<ClaudeProfile>,
    pub usage: Option<ClaudeUsageResponse>,
    pub observed_at: DateTime<Utc>,
}

#[derive(Clone)]
struct PendingAuth {
    flow_id: String,
    verifier: String,
    redirect_uri: String,
    expires_at: DateTime<Utc>,
}

pub struct ClaudeProvider {
    api: Arc<dyn ClaudeApi>,
    credentials: Arc<dyn ClaudeCredentialStore>,
    pending_auth: Mutex<Option<PendingAuth>>,
    lifecycle_lock: AsyncMutex<()>,
}

struct CredentialClearGuard {
    credentials: Arc<dyn ClaudeCredentialStore>,
    armed: bool,
}

impl CredentialClearGuard {
    fn new(credentials: Arc<dyn ClaudeCredentialStore>) -> Self {
        Self {
            credentials,
            armed: true,
        }
    }

    fn clear(mut self) -> ProviderResult<()> {
        self.credentials.clear()?;
        self.armed = false;
        Ok(())
    }

    fn preserve(mut self) {
        self.armed = false;
    }
}

impl Drop for CredentialClearGuard {
    fn drop(&mut self) {
        if self.armed {
            let _ = self.credentials.clear();
        }
    }
}

impl ClaudeProvider {
    pub fn new(credentials: Arc<dyn ClaudeCredentialStore>) -> ProviderResult<Self> {
        Ok(Self::with_api(Arc::new(HttpClaudeApi::new()?), credentials))
    }

    pub fn with_api(api: Arc<dyn ClaudeApi>, credentials: Arc<dyn ClaudeCredentialStore>) -> Self {
        Self {
            api,
            credentials,
            pending_auth: Mutex::new(None),
            lifecycle_lock: AsyncMutex::new(()),
        }
    }

    pub async fn refresh_auth(&self) -> ProviderResult<AuthState> {
        let _guard = self.lifecycle_lock.lock().await;
        self.refresh_auth_locked().await
    }

    async fn refresh_auth_locked(&self) -> ProviderResult<AuthState> {
        let current =
            self.credentials
                .load()?
                .ok_or_else(|| ProviderError::AuthenticationInvalid {
                    message: "Claude is not authenticated".into(),
                })?;
        let refresh_token = current.refresh_token.as_deref().ok_or_else(|| {
            ProviderError::AuthenticationInvalid {
                message: "stored Claude credential has no refresh token".into(),
            }
        })?;
        let response = self.api.refresh_token(refresh_token).await?;
        let mut credential = credential_from_response(response, current.refresh_token)?;
        // A refresh does not re-read the profile: the account cannot change
        // under a refresh token, so the identity recorded at sign-in stands.
        credential.account_label = current.account_label;
        self.credentials.save(&credential)?;
        Ok(authenticated_state(&credential))
    }

    async fn usable_credential_locked(&self) -> ProviderResult<ClaudeCredential> {
        let credential =
            self.credentials
                .load()?
                .ok_or_else(|| ProviderError::AuthenticationInvalid {
                    message: "Claude is not authenticated".into(),
                })?;
        let refresh_at = Utc::now() + Duration::seconds(REFRESH_EARLY_SECONDS);
        if credential
            .expires_at
            .is_some_and(|expires_at| expires_at <= refresh_at)
        {
            self.refresh_auth_locked().await?;
            self.credentials
                .load()?
                .ok_or_else(|| ProviderError::AuthenticationInvalid {
                    message: "Claude credential disappeared after refresh".into(),
                })
        } else {
            Ok(credential)
        }
    }
}

#[async_trait]
impl Provider for ClaudeProvider {
    type VendorUsage = ClaudeUsage;

    fn descriptor(&self) -> ProviderDescriptor {
        ProviderDescriptor {
            id: ProviderId::new("claude"),
            display_name: "Claude".into(),
            capabilities: vec![
                Capability::Authentication,
                Capability::AuthenticationStatus,
                Capability::Logout,
                Capability::UsageQuery,
                Capability::SubscriptionExpiry,
            ],
        }
    }

    async fn start_auth(&self, request: AuthStartRequest) -> ProviderResult<AuthChallenge> {
        let _guard = self.lifecycle_lock.lock().await;
        if request
            .method
            .as_ref()
            .is_some_and(|method| method != &AuthMethod::BrowserOAuth)
        {
            return Err(ProviderError::UnsupportedCapability {
                capability: "Claude supports browser OAuth only".into(),
            });
        }

        let verifier = random_url_token()?;
        let flow_id = random_url_token()?;
        let challenge = URL_SAFE_NO_PAD.encode(Sha256::digest(verifier.as_bytes()));
        let expires_at = Utc::now() + Duration::minutes(AUTH_FLOW_LIFETIME_MINUTES);
        let redirect_uri = match request.redirect_uri {
            None => REDIRECT_URI.to_owned(),
            Some(uri) if uri == REDIRECT_URI => uri,
            // Anthropic accepts any loopback callback for this client, which is
            // what lets a desktop client finish sign-in without a manual paste.
            Some(uri) if validate_loopback_http_redirect_uri(&uri).is_ok() => uri,
            Some(_) => {
                return Err(ProviderError::AuthenticationInvalid {
                    message: "Claude OAuth needs the registered remote callback or a loopback one"
                        .into(),
                });
            }
        };
        let authorization_url = authorization_url(&redirect_uri, &flow_id, &challenge)?;
        *lock(&self.pending_auth)? = Some(PendingAuth {
            flow_id: flow_id.clone(),
            verifier,
            redirect_uri,
            expires_at,
        });

        Ok(AuthChallenge {
            flow_id,
            method: AuthMethod::BrowserOAuth,
            verification_uri: Some(authorization_url),
            user_code: None,
            expires_at: Some(expires_at),
            input: Some(AuthInputRequest::visible(
                "the full callback URL from the browser, or code#state",
            )),
        })
    }

    async fn complete_auth(&self, request: AuthCompleteRequest) -> ProviderResult<AuthState> {
        let _guard = self.lifecycle_lock.lock().await;
        let pending = lock(&self.pending_auth)?.clone().ok_or_else(|| {
            ProviderError::AuthenticationInvalid {
                message: "no Claude OAuth flow is pending".into(),
            }
        })?;
        if request.flow_id != pending.flow_id {
            return Err(ProviderError::AuthenticationInvalid {
                message: "Claude OAuth flow identifier does not match".into(),
            });
        }
        if pending.expires_at <= Utc::now() {
            *lock(&self.pending_auth)? = None;
            return Err(ProviderError::AuthenticationInvalid {
                message: "Claude OAuth flow has expired".into(),
            });
        }
        if request
            .redirect_uri
            .as_deref()
            .is_some_and(|redirect| redirect != pending.redirect_uri)
        {
            return Err(ProviderError::AuthenticationInvalid {
                message: "Claude OAuth redirect URI does not match the initiated flow".into(),
            });
        }
        let input = request.authorization_code.as_deref().ok_or_else(|| {
            ProviderError::AuthenticationInvalid {
                message: "Claude OAuth authorization code is missing".into(),
            }
        })?;
        let (code, supplied_state) = parse_authorization_input(input, &pending.redirect_uri)?;
        let supplied_state =
            supplied_state.ok_or_else(|| ProviderError::AuthenticationInvalid {
                message: "Claude OAuth callback omitted the state".into(),
            })?;
        if supplied_state != pending.flow_id {
            return Err(ProviderError::AuthenticationInvalid {
                message: "Claude OAuth state does not match".into(),
            });
        }

        let response = self
            .api
            .exchange_code(AuthorizationCodeExchange {
                code,
                state: pending.flow_id.clone(),
                redirect_uri: pending.redirect_uri.clone(),
                code_verifier: pending.verifier,
            })
            .await?;
        let mut credential = credential_from_response(response, None)?;
        // The profile is what names the account, so it is read once here rather
        // than on every status check. A profile Anthropic will not serve leaves
        // the account unnamed instead of failing a sign-in that did work.
        credential.account_label = self
            .api
            .profile(&credential.access_token)
            .await
            .ok()
            .and_then(|profile| profile.account_label());
        self.credentials.save(&credential)?;
        *lock(&self.pending_auth)? = None;
        Ok(authenticated_state(&credential))
    }

    async fn auth_status(&self) -> ProviderResult<AuthState> {
        let _guard = self.lifecycle_lock.lock().await;
        {
            let mut pending = lock(&self.pending_auth)?;
            match pending.as_ref() {
                Some(flow) if flow.expires_at > Utc::now() => {
                    return Ok(AuthState::Pending {
                        flow_id: flow.flow_id.clone(),
                        expires_at: Some(flow.expires_at),
                    });
                }
                Some(_) => {
                    *pending = None;
                }
                None => {}
            }
        }
        let Some(credential) = self.credentials.load()? else {
            return Ok(AuthState::NotAuthenticated);
        };
        if credential
            .expires_at
            .is_some_and(|expires_at| expires_at <= Utc::now())
        {
            return match self.refresh_auth_locked().await {
                Ok(state) => Ok(state),
                Err(ProviderError::AuthenticationInvalid { message }) => {
                    Ok(AuthState::Invalid { reason: message })
                }
                Err(error) => Err(error),
            };
        }
        Ok(authenticated_state(&credential))
    }

    async fn logout(&self, _: LogoutRequest) -> ProviderResult<()> {
        let _guard = self.lifecycle_lock.lock().await;
        *lock(&self.pending_auth)? = None;
        let Some(credential) = self.credentials.load()? else {
            return Ok(());
        };
        let revoke_token = credential
            .refresh_token
            .as_deref()
            .unwrap_or(&credential.access_token)
            .to_owned();
        let clear_guard = CredentialClearGuard::new(self.credentials.clone());
        match self.api.revoke_token(&revoke_token).await {
            Ok(()) => clear_guard.clear(),
            Err(error) => {
                clear_guard.preserve();
                Err(error)
            }
        }
    }

    async fn query(&self, _request: UsageQuery) -> ProviderResult<QueryOutcome<Self::VendorUsage>> {
        let _guard = self.lifecycle_lock.lock().await;
        let credential = self.usable_credential_locked().await?;
        let profile = self.api.profile(&credential.access_token).await;
        let usage = self.api.usage(&credential.access_token).await;

        if let (Err(profile_error), Err(usage_error)) = (&profile, &usage) {
            return Err(preferred_error(profile_error.clone(), usage_error.clone()));
        }

        let mut failures = Vec::new();
        let profile = match profile {
            Ok(value) => Some(value),
            Err(error) => {
                failures.push(partial_failure("profile", &error));
                None
            }
        };
        let usage = match usage {
            Ok(value) => Some(value),
            Err(error) => {
                failures.push(partial_failure("usage", &error));
                None
            }
        };
        let data = ClaudeUsage {
            profile,
            usage,
            observed_at: Utc::now(),
        };
        if failures.is_empty() {
            Ok(QueryOutcome::Complete { data })
        } else {
            Ok(QueryOutcome::Partial { data, failures })
        }
    }

    fn normalize(&self, vendor_usage: Self::VendorUsage) -> ProviderResult<SubscriptionUsage> {
        normalize(vendor_usage)
    }
}

pub fn normalize(value: ClaudeUsage) -> ProviderResult<SubscriptionUsage> {
    if value.profile.is_none() && value.usage.is_none() {
        return Err(ProviderError::ProtocolIncompatible {
            message: "Claude query contained neither profile nor usage data".into(),
        });
    }

    let profile = value.profile.as_ref();
    let usage = value.usage.as_ref();
    let plan = profile.and_then(ClaudeProfile::plan).or_else(|| {
        usage.and_then(|item| {
            item.subscription_type
                .as_deref()
                .map(|plan| dto::normalize_plan(plan, None))
        })
    });
    let subscription_expires_at = profile.and_then(ClaudeProfile::trial_ends_at);
    let account_label = profile.and_then(ClaudeProfile::account_label);
    let mut windows = Vec::new();

    if let Some(usage) = usage {
        push_percent_window(
            &mut windows,
            UsageWindowKind::FiveHours,
            "included_usage",
            usage.five_hour.as_ref(),
        )?;
        push_percent_window(
            &mut windows,
            UsageWindowKind::Weekly,
            "included_usage",
            usage.seven_day.as_ref(),
        )?;
        for (id, label, window) in [
            (
                "seven_day_opus",
                "Weekly Opus",
                usage.seven_day_opus.as_ref(),
            ),
            (
                "seven_day_sonnet",
                "Weekly Sonnet",
                usage.seven_day_sonnet.as_ref(),
            ),
            (
                "seven_day_oauth_apps",
                "Weekly OAuth apps",
                usage.seven_day_oauth_apps.as_ref(),
            ),
        ] {
            push_percent_window(
                &mut windows,
                UsageWindowKind::Other {
                    id: id.into(),
                    label: label.into(),
                },
                "included_usage",
                window,
            )?;
        }
        for limit in &usage.limits {
            let Some(model) = limit.scope.as_ref().and_then(|scope| scope.model.as_ref()) else {
                continue;
            };
            validate_measurement(limit.percent, "model usage utilization")?;
            windows.push(UsageWindow {
                window: UsageWindowKind::Other {
                    id: format!(
                        "model_{}_{}_{}",
                        slug(&limit.kind),
                        slug(&limit.group),
                        slug(&model.display_name)
                    ),
                    label: format!("{} ({})", model.display_name, limit.kind),
                },
                resets_at: limit.resets_at,
                measurements: vec![UsageMeasurement {
                    name: "included_usage".into(),
                    used: limit.percent,
                    limit: Some(100.0),
                    unit: MeasurementUnit::Percent,
                }],
            });
        }
        if let Some(extra) = usage.extra_usage.as_ref().filter(|extra| extra.is_enabled) {
            if let Some(used) = extra.used_credits {
                validate_measurement(used, "extra usage credits")?;
                if let Some(limit) = extra.monthly_limit {
                    validate_measurement(limit, "extra usage limit")?;
                }
                windows.push(UsageWindow {
                    window: UsageWindowKind::Monthly,
                    resets_at: None,
                    measurements: vec![UsageMeasurement {
                        name: "extra_usage".into(),
                        used,
                        limit: extra.monthly_limit,
                        unit: extra
                            .currency
                            .as_ref()
                            .filter(|currency| !currency.trim().is_empty())
                            .map(|currency| MeasurementUnit::Currency {
                                code: currency.to_ascii_uppercase(),
                            })
                            .unwrap_or(MeasurementUnit::Credits),
                    }],
                });
            } else if let Some(utilization) = extra.utilization {
                validate_measurement(utilization, "extra usage utilization")?;
                windows.push(UsageWindow {
                    window: UsageWindowKind::Monthly,
                    resets_at: None,
                    measurements: vec![UsageMeasurement {
                        name: "extra_usage".into(),
                        used: utilization,
                        limit: Some(100.0),
                        unit: MeasurementUnit::Percent,
                    }],
                });
            }
        }
    }

    Ok(SubscriptionUsage {
        provider: ProviderId::new("claude"),
        account_label,
        plan,
        subscription_expires_at,
        observed_at: value.observed_at,
        windows,
    })
}

fn push_percent_window(
    target: &mut Vec<UsageWindow>,
    kind: UsageWindowKind,
    name: &str,
    source: Option<&ClaudeUsageWindow>,
) -> ProviderResult<()> {
    let Some(source) = source else {
        return Ok(());
    };
    let Some(utilization) = source.utilization else {
        return Ok(());
    };
    validate_measurement(utilization, name)?;
    target.push(UsageWindow {
        window: kind,
        resets_at: source.resets_at,
        measurements: vec![UsageMeasurement {
            name: name.into(),
            used: utilization,
            limit: Some(100.0),
            unit: MeasurementUnit::Percent,
        }],
    });
    Ok(())
}

fn validate_measurement(value: f64, name: &str) -> ProviderResult<()> {
    if value.is_finite() && value >= 0.0 {
        Ok(())
    } else {
        Err(ProviderError::ProtocolIncompatible {
            message: format!("Claude returned invalid {name}"),
        })
    }
}

fn slug(value: &str) -> String {
    let value = value
        .chars()
        .map(|character| {
            if character.is_ascii_alphanumeric() {
                character.to_ascii_lowercase()
            } else {
                '_'
            }
        })
        .collect::<String>();
    value.trim_matches('_').to_owned()
}

fn credential_from_response(
    response: ClaudeTokenResponse,
    previous_refresh_token: Option<String>,
) -> ProviderResult<ClaudeCredential> {
    if response.access_token.trim().is_empty() {
        return Err(ProviderError::ProtocolIncompatible {
            message: "Claude token response omitted the access token".into(),
        });
    }
    if response
        .token_type
        .as_deref()
        .is_some_and(|token_type| !token_type.eq_ignore_ascii_case("bearer"))
    {
        return Err(ProviderError::ProtocolIncompatible {
            message: "Claude token response used an unsupported token type".into(),
        });
    }
    if response
        .refresh_token
        .as_deref()
        .is_some_and(|token| token.trim().is_empty())
    {
        return Err(ProviderError::ProtocolIncompatible {
            message: "Claude token response contained an empty refresh token".into(),
        });
    }
    let expires_in = response
        .expires_in
        .filter(|seconds| *seconds > 0)
        .ok_or_else(|| ProviderError::ProtocolIncompatible {
            message: "Claude token response omitted a valid expiry".into(),
        })?;
    if response.refresh_token.is_none() && previous_refresh_token.is_none() {
        return Err(ProviderError::ProtocolIncompatible {
            message: "Claude token response omitted the refresh token".into(),
        });
    }
    let duration =
        Duration::try_seconds(expires_in).ok_or_else(|| ProviderError::ProtocolIncompatible {
            message: "Claude token response contained an out-of-range expiry".into(),
        })?;
    let expires_at = Utc::now().checked_add_signed(duration).ok_or_else(|| {
        ProviderError::ProtocolIncompatible {
            message: "Claude token response contained an out-of-range expiry".into(),
        }
    })?;
    let credential = ClaudeCredential {
        access_token: response.access_token,
        refresh_token: response.refresh_token.or(previous_refresh_token),
        expires_at: Some(expires_at),
        account_label: None,
    };
    credential.validate()?;
    Ok(credential)
}

fn authenticated_state(credential: &ClaudeCredential) -> AuthState {
    AuthState::Authenticated {
        account_label: credential.account_label.clone(),
        // Anthropic gives no identifier beyond the profile email, so the label
        // and the identity are the same value here. It is still read from the
        // profile rather than chosen by the user.
        account_key: credential.account_label.clone(),
        expires_at: credential.expires_at,
    }
}

fn authorization_url(redirect_uri: &str, state: &str, challenge: &str) -> ProviderResult<String> {
    let mut url =
        Url::parse(AUTHORIZE_ENDPOINT).map_err(|_| ProviderError::ProtocolIncompatible {
            message: "built-in Claude authorization endpoint is invalid".into(),
        })?;
    url.query_pairs_mut()
        .append_pair("code", "true")
        .append_pair("client_id", CLAUDE_CLIENT_ID)
        .append_pair("response_type", "code")
        .append_pair("redirect_uri", redirect_uri)
        .append_pair("scope", OAUTH_SCOPES)
        .append_pair("code_challenge", challenge)
        .append_pair("code_challenge_method", "S256")
        .append_pair("state", state);
    Ok(url.into())
}

fn parse_authorization_input(
    input: &str,
    expected_redirect_uri: &str,
) -> ProviderResult<(String, Option<String>)> {
    let input = input.trim();
    if input.is_empty() || input.len() > 8192 {
        return Err(ProviderError::AuthenticationInvalid {
            message: "Claude OAuth authorization input is empty or too long".into(),
        });
    }
    if input.contains("://") {
        let url = Url::parse(input).map_err(|_| ProviderError::AuthenticationInvalid {
            message: "Claude OAuth callback URL is invalid".into(),
        })?;
        let expected =
            Url::parse(expected_redirect_uri).map_err(|_| ProviderError::ProtocolIncompatible {
                message: "Claude OAuth expected redirect URI is invalid".into(),
            })?;
        if url.scheme() != expected.scheme()
            || url.host_str() != expected.host_str()
            || url.port_or_known_default() != expected.port_or_known_default()
            || url.path() != expected.path()
        {
            return Err(ProviderError::AuthenticationInvalid {
                message: "Claude OAuth callback URL has an unexpected origin or path".into(),
            });
        }
        let code = url
            .query_pairs()
            .find(|(key, _)| key == "code")
            .map(|(_, value)| value.into_owned())
            .filter(|value| !value.is_empty())
            .ok_or_else(|| ProviderError::AuthenticationInvalid {
                message: "Claude OAuth callback omitted the authorization code".into(),
            })?;
        let state = url
            .query_pairs()
            .find(|(key, _)| key == "state")
            .map(|(_, value)| value.into_owned());
        return Ok((code, state));
    }
    if let Some((code, state)) = input.split_once('#') {
        if code.is_empty() || state.is_empty() {
            return Err(ProviderError::AuthenticationInvalid {
                message: "Claude OAuth code or state is empty".into(),
            });
        }
        return Ok((code.into(), Some(state.into())));
    }
    Ok((input.into(), None))
}

fn random_url_token() -> ProviderResult<String> {
    let mut bytes = [0_u8; 32];
    getrandom::fill(&mut bytes).map_err(|_| ProviderError::Network {
        message: "operating system randomness is unavailable".into(),
    })?;
    Ok(URL_SAFE_NO_PAD.encode(bytes))
}

fn lock<T>(mutex: &Mutex<T>) -> ProviderResult<std::sync::MutexGuard<'_, T>> {
    mutex.lock().map_err(|_| ProviderError::Network {
        message: "Claude provider state lock is poisoned".into(),
    })
}

fn partial_failure(scope: &str, error: &ProviderError) -> PartialFailure {
    PartialFailure::from_error(scope, error)
}

fn preferred_error(first: ProviderError, second: ProviderError) -> ProviderError {
    if let (
        ProviderError::RateLimited {
            message,
            retry_after_seconds: first_delay,
        },
        ProviderError::RateLimited {
            retry_after_seconds: second_delay,
            ..
        },
    ) = (&first, &second)
    {
        return ProviderError::RateLimited {
            message: message.clone(),
            retry_after_seconds: match (first_delay, second_delay) {
                (Some(first), Some(second)) => Some((*first).max(*second)),
                (Some(delay), None) | (None, Some(delay)) => Some(*delay),
                (None, None) => None,
            },
        };
    }

    fn priority(error: &ProviderError) -> u8 {
        match error {
            ProviderError::AuthenticationInvalid { .. } => 4,
            ProviderError::RateLimited { .. } => 3,
            ProviderError::Network { .. } => 2,
            ProviderError::ProtocolIncompatible { .. }
            | ProviderError::UnsupportedCapability { .. } => 1,
        }
    }
    if priority(&first) >= priority(&second) {
        first
    } else {
        second
    }
}

#[cfg(test)]
mod tests {
    use std::future::Future;
    use std::sync::atomic::{AtomicUsize, Ordering};
    use std::task::{Context, Poll, Waker};

    use super::*;

    #[derive(Default)]
    struct MemoryStore(Mutex<Option<ClaudeCredential>>);

    impl ClaudeCredentialStore for MemoryStore {
        fn load(&self) -> ProviderResult<Option<ClaudeCredential>> {
            Ok(lock(&self.0)?.clone())
        }

        fn save(&self, credential: &ClaudeCredential) -> ProviderResult<()> {
            *lock(&self.0)? = Some(credential.clone());
            Ok(())
        }

        fn clear(&self) -> ProviderResult<()> {
            *lock(&self.0)? = None;
            Ok(())
        }
    }

    struct FakeApi {
        profile: ProviderResult<ClaudeProfile>,
        usage: ProviderResult<ClaudeUsageResponse>,
    }

    #[async_trait]
    impl ClaudeApi for FakeApi {
        async fn exchange_code(
            &self,
            _: AuthorizationCodeExchange,
        ) -> ProviderResult<ClaudeTokenResponse> {
            Ok(token_response("access", Some("refresh")))
        }

        async fn refresh_token(&self, _: &str) -> ProviderResult<ClaudeTokenResponse> {
            Ok(token_response("refreshed", None))
        }

        async fn revoke_token(&self, _: &str) -> ProviderResult<()> {
            Ok(())
        }

        async fn profile(&self, _: &str) -> ProviderResult<ClaudeProfile> {
            self.profile.clone()
        }

        async fn usage(&self, _: &str) -> ProviderResult<ClaudeUsageResponse> {
            self.usage.clone()
        }
    }

    fn token_response(access: &str, refresh: Option<&str>) -> ClaudeTokenResponse {
        ClaudeTokenResponse {
            access_token: access.into(),
            refresh_token: refresh.map(str::to_owned),
            expires_in: Some(3600),
            token_type: Some("Bearer".into()),
        }
    }

    fn provider(api: FakeApi, store: Arc<MemoryStore>) -> ClaudeProvider {
        ClaudeProvider::with_api(Arc::new(api), store)
    }

    fn run_ready<F: Future>(future: F) -> F::Output {
        let mut context = Context::from_waker(Waker::noop());
        let mut future = Box::pin(future);
        match future.as_mut().poll(&mut context) {
            Poll::Ready(output) => output,
            Poll::Pending => panic!("test future unexpectedly yielded"),
        }
    }

    #[test]
    fn completes_refreshes_and_clears_authentication() {
        let store = Arc::new(MemoryStore::default());
        let provider = provider(
            FakeApi {
                profile: Ok(ClaudeProfile::default()),
                usage: Ok(ClaudeUsageResponse::default()),
            },
            store.clone(),
        );
        let challenge = run_ready(provider.start_auth(AuthStartRequest {
            method: Some(AuthMethod::BrowserOAuth),
            redirect_uri: None,
        }))
        .unwrap();
        assert!(
            challenge
                .verification_uri
                .as_deref()
                .unwrap()
                .starts_with(AUTHORIZE_ENDPOINT)
        );
        assert!(matches!(
            run_ready(provider.auth_status()).unwrap(),
            AuthState::Pending { flow_id, .. } if flow_id == challenge.flow_id
        ));
        let authorization_input = format!("safe-code#{}", challenge.flow_id);
        let state = run_ready(provider.complete_auth(AuthCompleteRequest {
            flow_id: challenge.flow_id,
            authorization_code: Some(authorization_input),
            redirect_uri: Some(REDIRECT_URI.into()),
        }))
        .unwrap();
        assert!(matches!(state, AuthState::Authenticated { .. }));
        assert_eq!(store.load().unwrap().unwrap().access_token, "access");

        run_ready(provider.refresh_auth()).unwrap();
        let refreshed = store.load().unwrap().unwrap();
        assert_eq!(refreshed.access_token, "refreshed");
        assert_eq!(refreshed.refresh_token.as_deref(), Some("refresh"));

        run_ready(provider.logout(LogoutRequest::default())).unwrap();
        assert_eq!(store.load().unwrap(), None);
    }

    #[test]
    fn pending_reauthentication_takes_precedence_over_stored_credentials() {
        let store = Arc::new(MemoryStore(Mutex::new(Some(ClaudeCredential {
            access_token: "old-access".into(),
            refresh_token: Some("old-refresh".into()),
            expires_at: Some(Utc::now() + Duration::hours(1)),
            account_label: None,
        }))));
        let provider = provider(
            FakeApi {
                profile: Ok(ClaudeProfile::default()),
                usage: Ok(ClaudeUsageResponse::default()),
            },
            store,
        );

        let challenge = run_ready(provider.start_auth(AuthStartRequest {
            method: None,
            redirect_uri: None,
        }))
        .unwrap();
        assert!(matches!(
            run_ready(provider.auth_status()).unwrap(),
            AuthState::Pending { flow_id, .. } if flow_id == challenge.flow_id
        ));
    }

    #[test]
    fn completes_a_loopback_callback_and_still_rejects_other_redirects() {
        let store = Arc::new(MemoryStore::default());
        let provider = provider(
            FakeApi {
                profile: Ok(ClaudeProfile::default()),
                usage: Ok(ClaudeUsageResponse::default()),
            },
            store.clone(),
        );
        let loopback = "http://localhost:54545/callback";
        let challenge = run_ready(provider.start_auth(AuthStartRequest {
            method: Some(AuthMethod::BrowserOAuth),
            redirect_uri: Some(loopback.into()),
        }))
        .unwrap();
        assert!(
            challenge
                .verification_uri
                .as_deref()
                .unwrap()
                .contains("redirect_uri=http%3A%2F%2Flocalhost%3A54545%2Fcallback")
        );
        let state = run_ready(provider.complete_auth(AuthCompleteRequest {
            flow_id: challenge.flow_id.clone(),
            authorization_code: Some(format!(
                "{loopback}?code=safe-code&state={}",
                challenge.flow_id
            )),
            redirect_uri: Some(loopback.into()),
        }))
        .unwrap();
        assert!(matches!(state, AuthState::Authenticated { .. }));
        assert_eq!(store.load().unwrap().unwrap().access_token, "access");

        let error = run_ready(provider.start_auth(AuthStartRequest {
            method: Some(AuthMethod::BrowserOAuth),
            redirect_uri: Some("https://attacker.invalid/callback".into()),
        }))
        .unwrap_err();
        assert!(matches!(error, ProviderError::AuthenticationInvalid { .. }));
    }

    #[test]
    fn rejects_redirect_and_state_injection() {
        let store = Arc::new(MemoryStore::default());
        let provider = provider(
            FakeApi {
                profile: Ok(ClaudeProfile::default()),
                usage: Ok(ClaudeUsageResponse::default()),
            },
            store,
        );
        let challenge = run_ready(provider.start_auth(AuthStartRequest {
            method: None,
            redirect_uri: None,
        }))
        .unwrap();
        let error = run_ready(provider.complete_auth(AuthCompleteRequest {
            flow_id: challenge.flow_id.clone(),
            authorization_code: Some(format!("code#{}-attacker", challenge.flow_id)),
            redirect_uri: Some(REDIRECT_URI.into()),
        }))
        .unwrap_err();
        assert!(matches!(error, ProviderError::AuthenticationInvalid { .. }));

        let error = run_ready(provider.complete_auth(AuthCompleteRequest {
            flow_id: challenge.flow_id.clone(),
            authorization_code: Some("code".into()),
            redirect_uri: Some(REDIRECT_URI.into()),
        }))
        .unwrap_err();
        assert!(matches!(
            error,
            ProviderError::AuthenticationInvalid { message }
                if message == "Claude OAuth callback omitted the state"
        ));

        let error = run_ready(provider.complete_auth(AuthCompleteRequest {
            flow_id: challenge.flow_id,
            authorization_code: Some("code".into()),
            redirect_uri: Some("https://attacker.invalid/callback".into()),
        }))
        .unwrap_err();
        assert!(matches!(error, ProviderError::AuthenticationInvalid { .. }));
    }

    #[test]
    fn parses_missing_windows_and_unknown_fields() {
        let response: ClaudeUsageResponse =
            serde_json::from_str(include_str!("../tests/fixtures/missing-windows.json")).unwrap();
        let snapshot = normalize(ClaudeUsage {
            profile: None,
            usage: Some(response),
            observed_at: DateTime::parse_from_rfc3339("2026-08-27T12:00:00Z")
                .unwrap()
                .with_timezone(&Utc),
        })
        .unwrap();
        assert!(snapshot.windows.is_empty());
        assert_eq!(snapshot.subscription_expires_at, None);
    }

    #[test]
    fn maps_windows_extra_usage_and_trial_expiry() {
        let profile: ClaudeProfile =
            serde_json::from_str(include_str!("../tests/fixtures/profile-trial.json")).unwrap();
        let response: ClaudeUsageResponse =
            serde_json::from_str(include_str!("../tests/fixtures/extra-usage.json")).unwrap();
        let snapshot = normalize(ClaudeUsage {
            profile: Some(profile),
            usage: Some(response),
            observed_at: Utc::now(),
        })
        .unwrap();

        assert_eq!(snapshot.plan.as_deref(), Some("max_20x"));
        assert_eq!(
            snapshot.account_label.as_deref(),
            Some("user@example.invalid")
        );
        assert!(snapshot.subscription_expires_at.is_some());
        assert!(snapshot.windows.iter().any(|item| {
            item.window == UsageWindowKind::FiveHours
                && item.measurements[0].unit == MeasurementUnit::Percent
        }));
        assert!(snapshot.windows.iter().any(|item| {
            matches!(
                &item.window,
                UsageWindowKind::Other { id, .. } if id == "seven_day_opus"
            )
        }));
        assert!(snapshot.windows.iter().any(|item| {
            item.window == UsageWindowKind::Monthly
                && item.measurements[0].unit == MeasurementUnit::Currency { code: "USD".into() }
        }));
        assert!(snapshot.windows.iter().any(|item| {
            matches!(
                &item.window,
                UsageWindowKind::Other { id, .. } if id == "model_seven_day_model_future_model"
            )
        }));
    }

    #[test]
    fn returns_partial_data_when_one_endpoint_fails() {
        let store = Arc::new(MemoryStore(Mutex::new(Some(ClaudeCredential {
            access_token: "access".into(),
            refresh_token: Some("refresh".into()),
            expires_at: Some(Utc::now() + Duration::hours(1)),
            account_label: None,
        }))));
        let provider = provider(
            FakeApi {
                profile: Err(ProviderError::Network {
                    message: "profile unavailable".into(),
                }),
                usage: Ok(ClaudeUsageResponse::default()),
            },
            store,
        );
        let outcome = run_ready(provider.query(UsageQuery {
            account_label: Some("configured@example.test".into()),
        }))
        .unwrap();
        assert!(matches!(
            outcome,
            QueryOutcome::Partial { data, failures }
                if data.profile.is_none()
                    && data.usage.is_some()
                    && failures[0].scope == "profile"
        ));
    }

    #[test]
    fn query_succeeds_when_local_account_label_differs_from_profile() {
        let store = Arc::new(MemoryStore(Mutex::new(Some(ClaudeCredential {
            access_token: "access".into(),
            refresh_token: Some("refresh".into()),
            expires_at: Some(Utc::now() + Duration::hours(1)),
            account_label: None,
        }))));
        let provider = provider(
            FakeApi {
                profile: Ok(ClaudeProfile {
                    account: Some(ClaudeAccount {
                        email_address: Some("actual@example.test".into()),
                        ..ClaudeAccount::default()
                    }),
                    ..ClaudeProfile::default()
                }),
                usage: Ok(ClaudeUsageResponse::default()),
            },
            store,
        );
        let outcome = run_ready(provider.query(UsageQuery {
            account_label: Some("claude-1".into()),
        }))
        .unwrap();
        let QueryOutcome::Complete { data } = outcome else {
            panic!("expected complete outcome, got {outcome:?}");
        };
        let snapshot = normalize(data).unwrap();
        assert_eq!(
            snapshot.account_label.as_deref(),
            Some("actual@example.test")
        );
    }

    #[test]
    fn partial_rate_limit_preserves_retry_delay() {
        let store = Arc::new(MemoryStore(Mutex::new(Some(ClaudeCredential {
            access_token: "access".into(),
            refresh_token: Some("refresh".into()),
            expires_at: Some(Utc::now() + Duration::hours(1)),
            account_label: None,
        }))));
        let provider = provider(
            FakeApi {
                profile: Err(ProviderError::RateLimited {
                    message: "profile rate limited".into(),
                    retry_after_seconds: Some(17),
                }),
                usage: Ok(ClaudeUsageResponse::default()),
            },
            store,
        );

        let outcome = run_ready(provider.query(UsageQuery::default())).unwrap();
        assert!(matches!(
            outcome,
            QueryOutcome::Partial { failures, .. }
                if failures[0].scope == "profile"
                    && failures[0].message == "provider rate limited the request"
        ));
    }

    #[test]
    fn dual_rate_limits_preserve_the_longest_retry_delay() {
        let store = Arc::new(MemoryStore(Mutex::new(Some(ClaudeCredential {
            access_token: "access".into(),
            refresh_token: Some("refresh".into()),
            expires_at: Some(Utc::now() + Duration::hours(1)),
            account_label: None,
        }))));
        let provider = provider(
            FakeApi {
                profile: Err(ProviderError::RateLimited {
                    message: "profile rate limited".into(),
                    retry_after_seconds: None,
                }),
                usage: Err(ProviderError::RateLimited {
                    message: "usage rate limited".into(),
                    retry_after_seconds: Some(17),
                }),
            },
            store,
        );

        assert_eq!(
            run_ready(provider.query(UsageQuery::default())).unwrap_err(),
            ProviderError::RateLimited {
                message: "profile rate limited".into(),
                retry_after_seconds: Some(17),
            }
        );
    }

    #[test]
    fn redacts_tokens_from_debug_output() {
        let credential = ClaudeCredential {
            access_token: "access-secret".into(),
            refresh_token: Some("refresh-secret".into()),
            expires_at: None,
            account_label: None,
        };
        let output = format!("{credential:?}");
        assert!(!output.contains("access-secret"));
        assert!(!output.contains("refresh-secret"));
    }

    #[test]
    fn rejects_out_of_range_token_expiry() {
        let response = ClaudeTokenResponse {
            access_token: "access".into(),
            refresh_token: Some("refresh".into()),
            expires_in: Some(i64::MAX),
            token_type: Some("Bearer".into()),
        };
        assert!(matches!(
            credential_from_response(response, None),
            Err(ProviderError::ProtocolIncompatible { message })
                if message.contains("out-of-range expiry")
        ));
    }

    #[test]
    fn rejects_empty_refresh_tokens_in_exchanges_and_refreshes() {
        let response = ClaudeTokenResponse {
            access_token: "access".into(),
            refresh_token: Some(String::new()),
            expires_in: Some(3600),
            token_type: Some("Bearer".into()),
        };
        assert!(matches!(
            credential_from_response(response.clone(), None),
            Err(ProviderError::ProtocolIncompatible { message })
                if message.contains("empty refresh token")
        ));
        assert!(matches!(
            credential_from_response(response, Some("previous-refresh".into())),
            Err(ProviderError::ProtocolIncompatible { message })
                if message.contains("empty refresh token")
        ));
    }

    struct BlockingExchangeApi {
        entered: Arc<tokio::sync::Notify>,
        release: Arc<tokio::sync::Notify>,
    }

    struct BlockingRevokeApi {
        entered: Arc<tokio::sync::Notify>,
    }

    struct RetryRevokeApi {
        revoke_calls: Arc<AtomicUsize>,
    }

    struct BlockingQueryApi {
        refresh_calls: Arc<AtomicUsize>,
        profile_calls: AtomicUsize,
        profile_entered: Arc<tokio::sync::Notify>,
        release_profile: Arc<tokio::sync::Notify>,
    }

    #[async_trait]
    impl ClaudeApi for BlockingQueryApi {
        async fn exchange_code(
            &self,
            _: AuthorizationCodeExchange,
        ) -> ProviderResult<ClaudeTokenResponse> {
            Ok(token_response("access", Some("refresh")))
        }

        async fn refresh_token(&self, _: &str) -> ProviderResult<ClaudeTokenResponse> {
            self.refresh_calls.fetch_add(1, Ordering::SeqCst);
            Ok(token_response("refreshed", None))
        }

        async fn revoke_token(&self, _: &str) -> ProviderResult<()> {
            Ok(())
        }

        async fn profile(&self, _: &str) -> ProviderResult<ClaudeProfile> {
            if self.profile_calls.fetch_add(1, Ordering::SeqCst) == 0 {
                self.profile_entered.notify_one();
                self.release_profile.notified().await;
            }
            Ok(ClaudeProfile::default())
        }

        async fn usage(&self, _: &str) -> ProviderResult<ClaudeUsageResponse> {
            Ok(ClaudeUsageResponse::default())
        }
    }

    #[async_trait]
    impl ClaudeApi for BlockingExchangeApi {
        async fn exchange_code(
            &self,
            _: AuthorizationCodeExchange,
        ) -> ProviderResult<ClaudeTokenResponse> {
            self.entered.notify_one();
            self.release.notified().await;
            Ok(token_response("access", Some("refresh")))
        }

        async fn refresh_token(&self, _: &str) -> ProviderResult<ClaudeTokenResponse> {
            Ok(token_response("refreshed", None))
        }

        async fn revoke_token(&self, _: &str) -> ProviderResult<()> {
            Ok(())
        }

        async fn profile(&self, _: &str) -> ProviderResult<ClaudeProfile> {
            Ok(ClaudeProfile::default())
        }

        async fn usage(&self, _: &str) -> ProviderResult<ClaudeUsageResponse> {
            Ok(ClaudeUsageResponse::default())
        }
    }

    #[async_trait]
    impl ClaudeApi for BlockingRevokeApi {
        async fn exchange_code(
            &self,
            _: AuthorizationCodeExchange,
        ) -> ProviderResult<ClaudeTokenResponse> {
            Ok(token_response("access", Some("refresh")))
        }

        async fn refresh_token(&self, _: &str) -> ProviderResult<ClaudeTokenResponse> {
            Ok(token_response("refreshed", None))
        }

        async fn revoke_token(&self, _: &str) -> ProviderResult<()> {
            self.entered.notify_one();
            std::future::pending().await
        }

        async fn profile(&self, _: &str) -> ProviderResult<ClaudeProfile> {
            Ok(ClaudeProfile::default())
        }

        async fn usage(&self, _: &str) -> ProviderResult<ClaudeUsageResponse> {
            Ok(ClaudeUsageResponse::default())
        }
    }

    #[async_trait]
    impl ClaudeApi for RetryRevokeApi {
        async fn exchange_code(
            &self,
            _: AuthorizationCodeExchange,
        ) -> ProviderResult<ClaudeTokenResponse> {
            Ok(token_response("access", Some("refresh")))
        }

        async fn refresh_token(&self, _: &str) -> ProviderResult<ClaudeTokenResponse> {
            Ok(token_response("refreshed", None))
        }

        async fn revoke_token(&self, _: &str) -> ProviderResult<()> {
            if self.revoke_calls.fetch_add(1, Ordering::SeqCst) == 0 {
                Err(ProviderError::RateLimited {
                    message: "revoke rate limited".into(),
                    retry_after_seconds: Some(17),
                })
            } else {
                Ok(())
            }
        }

        async fn profile(&self, _: &str) -> ProviderResult<ClaudeProfile> {
            Ok(ClaudeProfile::default())
        }

        async fn usage(&self, _: &str) -> ProviderResult<ClaudeUsageResponse> {
            Ok(ClaudeUsageResponse::default())
        }
    }

    #[tokio::test]
    async fn logout_cannot_be_undone_by_inflight_auth_completion() {
        let store = Arc::new(MemoryStore::default());
        let entered = Arc::new(tokio::sync::Notify::new());
        let release = Arc::new(tokio::sync::Notify::new());
        let provider = Arc::new(ClaudeProvider::with_api(
            Arc::new(BlockingExchangeApi {
                entered: entered.clone(),
                release: release.clone(),
            }),
            store.clone(),
        ));
        let challenge = provider
            .start_auth(AuthStartRequest {
                method: None,
                redirect_uri: None,
            })
            .await
            .unwrap();
        let flow_id = challenge.flow_id;
        let authorization_input = format!("safe-code#{flow_id}");
        let complete_provider = provider.clone();
        let completion = tokio::spawn(async move {
            complete_provider
                .complete_auth(AuthCompleteRequest {
                    flow_id,
                    authorization_code: Some(authorization_input),
                    redirect_uri: Some(REDIRECT_URI.into()),
                })
                .await
        });
        entered.notified().await;

        let logout_started = Arc::new(tokio::sync::Notify::new());
        let logout_started_signal = logout_started.clone();
        let logout_provider = provider.clone();
        let logout = tokio::spawn(async move {
            logout_started_signal.notify_one();
            logout_provider.logout(LogoutRequest::default()).await
        });
        logout_started.notified().await;
        tokio::task::yield_now().await;
        assert!(!logout.is_finished());
        release.notify_one();

        completion.await.unwrap().unwrap();
        logout.await.unwrap().unwrap();
        assert_eq!(store.load().unwrap(), None);
    }

    #[tokio::test]
    async fn cancelled_logout_still_clears_local_credentials() {
        let store = Arc::new(MemoryStore(Mutex::new(Some(ClaudeCredential {
            access_token: "access".into(),
            refresh_token: Some("refresh".into()),
            expires_at: Some(Utc::now() + Duration::hours(1)),
            account_label: None,
        }))));
        let entered = Arc::new(tokio::sync::Notify::new());
        let provider = Arc::new(ClaudeProvider::with_api(
            Arc::new(BlockingRevokeApi {
                entered: entered.clone(),
            }),
            store.clone(),
        ));

        let logout_provider = provider.clone();
        let logout =
            tokio::spawn(async move { logout_provider.logout(LogoutRequest::default()).await });
        entered.notified().await;
        assert!(store.load().unwrap().is_some());

        logout.abort();
        assert!(logout.await.unwrap_err().is_cancelled());
        assert_eq!(store.load().unwrap(), None);
    }

    #[tokio::test]
    async fn failed_revoke_preserves_credentials_for_logout_retry() {
        let store = Arc::new(MemoryStore(Mutex::new(Some(ClaudeCredential {
            access_token: "access".into(),
            refresh_token: Some("refresh".into()),
            expires_at: Some(Utc::now() + Duration::hours(1)),
            account_label: None,
        }))));
        let revoke_calls = Arc::new(AtomicUsize::new(0));
        let provider = ClaudeProvider::with_api(
            Arc::new(RetryRevokeApi {
                revoke_calls: revoke_calls.clone(),
            }),
            store.clone(),
        );

        assert!(matches!(
            provider.logout(LogoutRequest::default()).await,
            Err(ProviderError::RateLimited {
                retry_after_seconds: Some(17),
                ..
            })
        ));
        assert!(store.load().unwrap().is_some());

        provider.logout(LogoutRequest::default()).await.unwrap();
        assert_eq!(store.load().unwrap(), None);
        assert_eq!(revoke_calls.load(Ordering::SeqCst), 2);
    }

    #[tokio::test]
    async fn logout_waits_for_an_inflight_usage_query() {
        let store = Arc::new(MemoryStore(Mutex::new(Some(ClaudeCredential {
            access_token: "access".into(),
            refresh_token: Some("refresh".into()),
            expires_at: Some(Utc::now() + Duration::hours(1)),
            account_label: None,
        }))));
        let profile_entered = Arc::new(tokio::sync::Notify::new());
        let release_profile = Arc::new(tokio::sync::Notify::new());
        let provider = Arc::new(ClaudeProvider::with_api(
            Arc::new(BlockingQueryApi {
                refresh_calls: Arc::new(AtomicUsize::new(0)),
                profile_calls: AtomicUsize::new(0),
                profile_entered: profile_entered.clone(),
                release_profile: release_profile.clone(),
            }),
            store.clone(),
        ));

        let query_provider = provider.clone();
        let query = tokio::spawn(async move { query_provider.query(UsageQuery::default()).await });
        profile_entered.notified().await;
        let logout_provider = provider.clone();
        let logout =
            tokio::spawn(async move { logout_provider.logout(LogoutRequest::default()).await });
        tokio::task::yield_now().await;
        assert!(!logout.is_finished());

        release_profile.notify_one();
        query.await.unwrap().unwrap();
        logout.await.unwrap().unwrap();
        assert_eq!(store.load().unwrap(), None);
    }

    #[tokio::test]
    async fn concurrent_queries_refresh_expiring_credentials_once() {
        let store = Arc::new(MemoryStore(Mutex::new(Some(ClaudeCredential {
            access_token: "expiring".into(),
            refresh_token: Some("refresh".into()),
            expires_at: Some(Utc::now() + Duration::seconds(1)),
            account_label: None,
        }))));
        let refresh_calls = Arc::new(AtomicUsize::new(0));
        let profile_entered = Arc::new(tokio::sync::Notify::new());
        let release_profile = Arc::new(tokio::sync::Notify::new());
        let provider = Arc::new(ClaudeProvider::with_api(
            Arc::new(BlockingQueryApi {
                refresh_calls: refresh_calls.clone(),
                profile_calls: AtomicUsize::new(0),
                profile_entered: profile_entered.clone(),
                release_profile: release_profile.clone(),
            }),
            store,
        ));

        let first_provider = provider.clone();
        let first = tokio::spawn(async move { first_provider.query(UsageQuery::default()).await });
        profile_entered.notified().await;
        let second_provider = provider.clone();
        let second =
            tokio::spawn(async move { second_provider.query(UsageQuery::default()).await });
        tokio::task::yield_now().await;
        assert!(!second.is_finished());

        release_profile.notify_one();
        first.await.unwrap().unwrap();
        second.await.unwrap().unwrap();
        assert_eq!(refresh_calls.load(Ordering::SeqCst), 1);
    }
}
