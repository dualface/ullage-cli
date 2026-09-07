//! Grok consumer provider.
//!
//! The consumer billing API is not a stable public API. Network details therefore live behind
//! [`GrokTransport`], while this crate owns OAuth state, tolerant vendor DTO parsing and conversion
//! into Ullage's stable usage model.

mod billing;
mod oauth;

pub use billing::{
    GrokBillingUsage, GrokOnDemand, GrokPeriod, GrokPrepaid, GrokProductUsage, GrokTier,
    NormalizedTier, parse_billing,
};
pub use oauth::{
    BrowserAuthorization, DeviceAuthorization, GrokApiError, GrokTransport, HttpGrokConfig,
    HttpGrokTransport, OAuthPoll, OAuthToken,
};

use std::sync::{Arc, Mutex};
use std::time::Duration;

use async_trait::async_trait;
use chrono::Utc;
use ullage_auth::{
    AuthChallenge, AuthCompleteRequest, AuthMethod, AuthStartRequest, AuthState, Credential,
    CredentialError, CredentialKey, CredentialStore, LogoutRequest, SecretValue,
};
use ullage_core::{
    Capability, PartialFailure, Provider, ProviderDescriptor, ProviderError, ProviderId,
    ProviderResult, QueryOutcome, SubscriptionUsage, UsageQuery,
};

/// Cap for the overlapping settings fetch. Started with billing so a hang cannot
/// add a second wait after a slow billing response and exhaust the daemon budget.
const SETTINGS_FETCH_DEADLINE: Duration = Duration::from_secs(8);

#[derive(Default)]
struct Session {
    generation: u64,
    pending: Option<PendingAuthorization>,
    token: Option<OAuthToken>,
    credentials_loaded: bool,
}

enum PendingAuthorization {
    Browser {
        flow_id: String,
        redirect_uri: String,
        expires_at: Option<chrono::DateTime<Utc>>,
    },
    Device {
        flow_id: String,
        device_code: String,
        expires_at: Option<chrono::DateTime<Utc>>,
    },
}

impl PendingAuthorization {
    fn flow_id(&self) -> &str {
        match self {
            Self::Browser { flow_id, .. } | Self::Device { flow_id, .. } => flow_id,
        }
    }

    fn expires_at(&self) -> Option<chrono::DateTime<Utc>> {
        match self {
            Self::Browser { expires_at, .. } | Self::Device { expires_at, .. } => *expires_at,
        }
    }
}

/// Stateful Grok provider whose tokens are always omitted from `Debug`.
///
/// `new` and `with_transport` keep tokens only in memory. The production
/// `with_transport_and_store_for_account` constructor persists them in the shared credential store.
pub struct GrokProvider<T> {
    transport: Arc<T>,
    lifecycle: tokio::sync::Mutex<()>,
    session: Mutex<Session>,
    credentials: Option<(Arc<CredentialStore>, CredentialKey)>,
}

impl<T> GrokProvider<T> {
    pub fn new(transport: T) -> Self {
        Self {
            transport: Arc::new(transport),
            lifecycle: tokio::sync::Mutex::new(()),
            session: Mutex::new(Session {
                credentials_loaded: true,
                ..Session::default()
            }),
            credentials: None,
        }
    }

    pub fn with_transport(transport: Arc<T>) -> Self {
        Self {
            transport,
            lifecycle: tokio::sync::Mutex::new(()),
            session: Mutex::new(Session {
                credentials_loaded: true,
                ..Session::default()
            }),
            credentials: None,
        }
    }

    pub fn with_transport_and_store(
        transport: Arc<T>,
        credentials: Arc<CredentialStore>,
    ) -> ProviderResult<Self> {
        Self::with_transport_and_store_for_account(transport, credentials, "active")
    }

    pub fn with_transport_and_store_for_account(
        transport: Arc<T>,
        credentials: Arc<CredentialStore>,
        account_id: &str,
    ) -> ProviderResult<Self> {
        let key = CredentialKey::new("grok", account_id).map_err(credential_error)?;
        Ok(Self {
            transport,
            lifecycle: tokio::sync::Mutex::new(()),
            session: Mutex::new(Session::default()),
            credentials: Some((credentials, key)),
        })
    }

    fn lock_session(&self) -> ProviderResult<std::sync::MutexGuard<'_, Session>> {
        self.session
            .lock()
            .map_err(|_| ProviderError::ProtocolIncompatible {
                message: "Grok session state is unavailable".into(),
            })
    }

    fn ensure_query_session(&self, generation: u64, token: &OAuthToken) -> ProviderResult<()> {
        let session = self.lock_session()?;
        if session.generation != generation || session.token.as_ref() != Some(token) {
            return Err(ProviderError::AuthenticationInvalid {
                message: "Grok authentication state changed during usage query".into(),
            });
        }
        Ok(())
    }

    fn next_generation(session: &mut Session) -> ProviderResult<u64> {
        session.generation = session.generation.checked_add(1).ok_or_else(|| {
            ProviderError::ProtocolIncompatible {
                message: "Grok session generation is exhausted".into(),
            }
        })?;
        Ok(session.generation)
    }

    fn ensure_credentials_loaded(&self) -> ProviderResult<()> {
        let Some((credentials, key)) = &self.credentials else {
            return Ok(());
        };
        if self.lock_session()?.credentials_loaded {
            return Ok(());
        }
        let token = match credentials.get(key) {
            Ok(stored) => {
                let payload = stored
                    .credential()
                    .get("session")
                    .ok_or_else(|| credential_error(CredentialError::CorruptCredential))?;
                let token: OAuthToken = serde_json::from_slice(payload.expose())
                    .map_err(|_| credential_error(CredentialError::CorruptCredential))?;
                token.validate_stored()?;
                Some(token)
            }
            Err(CredentialError::NotFound) => None,
            Err(error) => return Err(credential_error(error)),
        };
        let mut session = self.lock_session()?;
        if !session.credentials_loaded {
            session.token = token;
            session.credentials_loaded = true;
        }
        Ok(())
    }

    fn persist_token(&self, token: Option<&OAuthToken>) -> ProviderResult<()> {
        let Some((credentials, key)) = &self.credentials else {
            return Ok(());
        };
        match token {
            Some(token) => {
                let encoded =
                    serde_json::to_vec(token).map_err(|_| ProviderError::ProtocolIncompatible {
                        message: "Grok credential serialization failed".into(),
                    })?;
                let mut credential = Credential::new();
                credential
                    .insert("session", SecretValue::new(encoded))
                    .map_err(credential_error)?;
                credentials.set(key, credential).map_err(credential_error)?;
            }
            None => match credentials.delete(key) {
                Ok(()) | Err(CredentialError::NotFound) => {}
                Err(error) => return Err(credential_error(error)),
            },
        }
        Ok(())
    }
}

fn credential_error(error: CredentialError) -> ProviderError {
    ProviderError::ProtocolIncompatible {
        message: error.provider_message("Grok credential store operation failed"),
    }
}

impl<T: GrokTransport> GrokProvider<T> {
    /// Refreshes the current access token without interpreting its expiry as subscription expiry.
    pub async fn refresh_auth(&self) -> ProviderResult<AuthState> {
        let _lifecycle = self.lifecycle.lock().await;
        self.ensure_credentials_loaded()?;
        let (generation, previous, refresh_token) = {
            let mut session = self.lock_session()?;
            let previous =
                session
                    .token
                    .clone()
                    .ok_or_else(|| ProviderError::AuthenticationInvalid {
                        message: "Grok is not authenticated".into(),
                    })?;
            let refresh_token = previous.refresh_token.clone().ok_or_else(|| {
                ProviderError::AuthenticationInvalid {
                    message: "Grok refresh token is unavailable".into(),
                }
            })?;
            let generation = Self::next_generation(&mut session)?;
            (generation, previous, refresh_token)
        };

        let mut token = self
            .transport
            .refresh(&refresh_token)
            .await
            .map_err(ProviderError::from)?;
        if token.refresh_token.is_none() {
            token.refresh_token = previous.refresh_token.clone();
        }
        if token.account_label.is_none() {
            token.account_label = previous.account_label.clone();
        } else if previous.account_label.is_some() && token.account_label != previous.account_label
        {
            return Err(ProviderError::AuthenticationInvalid {
                message: "Grok refresh response changed the authenticated account".into(),
            });
        }
        token.validate()?;
        let state = token.auth_state();
        let mut session = self.lock_session()?;
        if session.generation != generation || session.token.as_ref() != Some(&previous) {
            return Err(ProviderError::AuthenticationInvalid {
                message: "Grok authentication state changed during refresh".into(),
            });
        }
        self.persist_token(Some(&token))?;
        session.token = Some(token);
        Ok(state)
    }

    fn install_token(
        &self,
        generation: u64,
        flow_id: &str,
        token: OAuthToken,
    ) -> ProviderResult<AuthState> {
        token.validate()?;
        let state = token.auth_state();
        let mut session = self.lock_session()?;
        if session.generation != generation
            || session.pending.as_ref().map(PendingAuthorization::flow_id) != Some(flow_id)
        {
            return Err(ProviderError::AuthenticationInvalid {
                message: "Grok OAuth flow is no longer current".into(),
            });
        }
        self.persist_token(Some(&token))?;
        session.pending = None;
        session.token = Some(token);
        Ok(state)
    }
}

#[async_trait]
impl<T> Provider for GrokProvider<T>
where
    T: GrokTransport + 'static,
{
    type VendorUsage = GrokBillingUsage;

    fn descriptor(&self) -> ProviderDescriptor {
        ProviderDescriptor {
            id: ProviderId::new("grok"),
            display_name: "Grok".into(),
            capabilities: vec![
                Capability::Authentication,
                Capability::AuthenticationStatus,
                Capability::Logout,
                Capability::UsageQuery,
            ],
        }
    }

    async fn start_auth(&self, request: AuthStartRequest) -> ProviderResult<AuthChallenge> {
        let _lifecycle = self.lifecycle.lock().await;
        self.ensure_credentials_loaded()?;
        let method = request.method.unwrap_or(AuthMethod::DeviceCode);
        if !matches!(method, AuthMethod::DeviceCode | AuthMethod::BrowserOAuth) {
            return Err(ProviderError::UnsupportedCapability {
                capability: "requested Grok authentication method".into(),
            });
        }
        let (generation, superseded_flow) = {
            let mut session = self.lock_session()?;
            let generation = Self::next_generation(&mut session)?;
            let superseded_flow = session
                .pending
                .take()
                .map(|pending| pending.flow_id().to_owned());
            (generation, superseded_flow)
        };
        if let Some(flow_id) = superseded_flow {
            self.transport.cancel_authorization(&flow_id);
        }
        let (challenge, pending) = match method {
            AuthMethod::DeviceCode => {
                let authorization = self
                    .transport
                    .start_device_authorization()
                    .await
                    .map_err(ProviderError::from)?;
                authorization.validate()?;
                let challenge = authorization.challenge();
                let pending = PendingAuthorization::Device {
                    flow_id: authorization.flow_id,
                    device_code: authorization.device_code,
                    expires_at: authorization.expires_at,
                };
                (challenge, pending)
            }
            AuthMethod::BrowserOAuth => {
                let redirect_uri = request
                    .redirect_uri
                    .clone()
                    .unwrap_or_else(|| self.transport.browser_redirect_uri().to_owned());
                let authorization = self
                    .transport
                    .start_browser_authorization(&redirect_uri)
                    .await
                    .map_err(ProviderError::from)?;
                authorization.validate()?;
                let challenge = authorization.challenge();
                let pending = PendingAuthorization::Browser {
                    flow_id: authorization.flow_id,
                    redirect_uri,
                    expires_at: authorization.expires_at,
                };
                (challenge, pending)
            }
            _ => unreachable!("authentication method was validated"),
        };

        let mut session = self.lock_session()?;
        if session.generation != generation {
            drop(session);
            self.transport
                .cancel_authorization(challenge.flow_id.as_str());
            return Err(ProviderError::AuthenticationInvalid {
                message: "Grok OAuth start was superseded".into(),
            });
        }
        session.pending = Some(pending);
        Ok(challenge)
    }

    async fn complete_auth(&self, request: AuthCompleteRequest) -> ProviderResult<AuthState> {
        let _lifecycle = self.lifecycle.lock().await;
        self.ensure_credentials_loaded()?;
        enum Completion {
            Browser { code: String, redirect_uri: String },
            Device { device_code: String },
        }

        let (generation, completion) = {
            let session = self.lock_session()?;
            let pending =
                session
                    .pending
                    .as_ref()
                    .ok_or_else(|| ProviderError::AuthenticationInvalid {
                        message: "no Grok authorization is pending".into(),
                    })?;
            if pending.flow_id() != request.flow_id {
                return Err(ProviderError::AuthenticationInvalid {
                    message: "Grok OAuth flow identifier does not match".into(),
                });
            }
            if pending
                .expires_at()
                .is_some_and(|expiry| expiry <= Utc::now())
            {
                return Err(ProviderError::AuthenticationInvalid {
                    message: "Grok OAuth flow expired".into(),
                });
            }
            let completion = match pending {
                PendingAuthorization::Browser {
                    redirect_uri: expected_redirect_uri,
                    ..
                } => {
                    let redirect_uri = request.redirect_uri.ok_or_else(|| {
                        ProviderError::AuthenticationInvalid {
                            message: "Grok browser redirect URI is missing".into(),
                        }
                    })?;
                    if redirect_uri != *expected_redirect_uri {
                        return Err(ProviderError::AuthenticationInvalid {
                            message: "Grok OAuth redirect URI does not match the initiated flow"
                                .into(),
                        });
                    }
                    Completion::Browser {
                        code: browser_authorization_code(
                            request.authorization_code.as_deref().ok_or_else(|| {
                                ProviderError::AuthenticationInvalid {
                                    message: "Grok browser authorization code is missing".into(),
                                }
                            })?,
                            &request.flow_id,
                        )?,
                        redirect_uri,
                    }
                }
                PendingAuthorization::Device { device_code, .. } => Completion::Device {
                    device_code: device_code.clone(),
                },
            };
            (session.generation, completion)
        };

        match completion {
            Completion::Browser { code, redirect_uri } => {
                let token = self
                    .transport
                    .complete_browser_authorization(&request.flow_id, &code, &redirect_uri)
                    .await
                    .map_err(ProviderError::from)?;
                self.install_token(generation, &request.flow_id, token)
            }
            Completion::Device { device_code } => match self
                .transport
                .poll_device_authorization(&device_code)
                .await
                .map_err(ProviderError::from)?
            {
                OAuthPoll::Pending => {
                    let session = self.lock_session()?;
                    let pending = session.pending.as_ref();
                    if session.generation != generation
                        || pending.map(PendingAuthorization::flow_id)
                            != Some(request.flow_id.as_str())
                    {
                        return Err(ProviderError::AuthenticationInvalid {
                            message: "Grok OAuth flow is no longer current".into(),
                        });
                    }
                    Ok(AuthState::Pending {
                        flow_id: request.flow_id,
                        expires_at: pending.and_then(PendingAuthorization::expires_at),
                    })
                }
                OAuthPoll::Authorized(token) => {
                    self.install_token(generation, &request.flow_id, token)
                }
                OAuthPoll::Denied => {
                    let mut session = self.lock_session()?;
                    if session.generation == generation
                        && session.pending.as_ref().map(PendingAuthorization::flow_id)
                            == Some(request.flow_id.as_str())
                    {
                        session.pending = None;
                    }
                    Err(ProviderError::AuthenticationInvalid {
                        message: "Grok device authorization was denied".into(),
                    })
                }
                OAuthPoll::Expired => {
                    let mut session = self.lock_session()?;
                    if session.generation == generation
                        && session.pending.as_ref().map(PendingAuthorization::flow_id)
                            == Some(request.flow_id.as_str())
                    {
                        session.pending = None;
                    }
                    Err(ProviderError::AuthenticationInvalid {
                        message: "Grok device authorization expired".into(),
                    })
                }
            },
        }
    }

    async fn auth_status(&self) -> ProviderResult<AuthState> {
        self.ensure_credentials_loaded()?;
        let expired = self
            .lock_session()?
            .token
            .as_ref()
            .is_some_and(|token| token.expires_at.is_some_and(|expiry| expiry <= Utc::now()));
        if expired {
            return self.refresh_auth().await;
        }
        let session = self.lock_session()?;
        if let Some(token) = &session.token {
            return Ok(token.auth_state());
        }
        if let Some(pending) = &session.pending {
            if pending
                .expires_at()
                .is_some_and(|expiry| expiry <= Utc::now())
            {
                return Ok(AuthState::Invalid {
                    reason: "Grok OAuth flow expired".into(),
                    // An abandoned flow never named an account.
                    account_key: None,
                });
            }
            return Ok(AuthState::Pending {
                flow_id: pending.flow_id().into(),
                expires_at: pending.expires_at(),
            });
        }
        Ok(AuthState::NotAuthenticated)
    }

    async fn logout(&self, request: LogoutRequest) -> ProviderResult<()> {
        let _lifecycle = self.lifecycle.lock().await;
        self.ensure_credentials_loaded()?;
        let (token, pending_flow) = {
            let mut session = self.lock_session()?;
            let token = session.token.clone();
            if let Some(expected) = &request.account_label {
                match token
                    .as_ref()
                    .and_then(|token| token.account_label.as_ref())
                {
                    Some(actual) if actual == expected => {}
                    _ => {
                        return Err(ProviderError::AuthenticationInvalid {
                            message: "Grok logout account does not match the authenticated account"
                                .into(),
                        });
                    }
                }
            }
            Self::next_generation(&mut session)?;
            let pending_flow = session
                .pending
                .take()
                .map(|pending| pending.flow_id().to_owned());
            (token, pending_flow)
        };
        if let Some(flow_id) = pending_flow {
            self.transport.cancel_authorization(&flow_id);
        }
        if let Some(token) = token {
            self.transport
                .revoke(&token)
                .await
                .map_err(ProviderError::from)?;
            let mut session = self.lock_session()?;
            if session.token.as_ref() == Some(&token) {
                self.persist_token(None)?;
                session.token = None;
            }
        } else {
            self.persist_token(None)?;
        }
        Ok(())
    }

    async fn query(&self, _request: UsageQuery) -> ProviderResult<QueryOutcome<Self::VendorUsage>> {
        self.ensure_credentials_loaded()?;
        let expired = self
            .lock_session()?
            .token
            .as_ref()
            .is_some_and(|token| token.expires_at.is_some_and(|expiry| expiry <= Utc::now()));
        if expired {
            self.refresh_auth().await?;
        }
        let (generation, token) = {
            let session = self.lock_session()?;
            let token =
                session
                    .token
                    .clone()
                    .ok_or_else(|| ProviderError::AuthenticationInvalid {
                        message: "Grok is not authenticated".into(),
                    })?;
            (session.generation, token)
        };

        let billing = self.transport.fetch_billing(&token.access_token);
        let settings = tokio::time::timeout(
            SETTINGS_FETCH_DEADLINE,
            self.transport.fetch_settings(&token.access_token),
        );
        let (billing_result, settings_result) = tokio::join!(billing, settings);
        let response = billing_result.map_err(ProviderError::from)?;
        self.ensure_query_session(generation, &token)?;
        let mut outcome = parse_billing(response, token.account_label.clone(), Utc::now())?;

        let settings_failures = match settings_result {
            Ok(Ok(settings)) => {
                let (parsed, failures) = billing::parse_settings(&settings);
                billing::apply_settings_to_outcome(&mut outcome, parsed);
                failures
            }
            Ok(Err(error)) => vec![PartialFailure::from_error("settings", &error.into())],
            Err(_) => vec![PartialFailure::from_error(
                "settings",
                &ProviderError::Network {
                    message: String::new(),
                },
            )],
        };
        self.ensure_query_session(generation, &token)?;
        Ok(billing::with_failures(outcome, settings_failures))
    }

    fn normalize(&self, vendor_usage: Self::VendorUsage) -> ProviderResult<SubscriptionUsage> {
        billing::normalize(vendor_usage)
    }
}

/// Interactive login pastes the full callback URL; the scripted path still
/// sends a raw authorization code.
fn browser_authorization_code(input: &str, flow_id: &str) -> ProviderResult<String> {
    let input = input.trim();
    if input.is_empty() {
        return Err(ProviderError::AuthenticationInvalid {
            message: "Grok browser authorization code is missing".into(),
        });
    }
    if !input.contains("://") {
        return Ok(input.to_owned());
    }
    let url = reqwest::Url::parse(input).map_err(|_| ProviderError::AuthenticationInvalid {
        message: "Grok OAuth callback URL is invalid".into(),
    })?;
    let state = url
        .query_pairs()
        .find(|(key, _)| key == "state")
        .map(|(_, value)| value.into_owned())
        .filter(|value| !value.is_empty())
        .ok_or_else(|| ProviderError::AuthenticationInvalid {
            message: "Grok OAuth callback omitted the state".into(),
        })?;
    if state != flow_id {
        return Err(ProviderError::AuthenticationInvalid {
            message: "Grok OAuth callback state does not match".into(),
        });
    }
    url.query_pairs()
        .find(|(key, _)| key == "code")
        .map(|(_, value)| value.into_owned())
        .filter(|value| !value.is_empty())
        .ok_or_else(|| ProviderError::AuthenticationInvalid {
            message: "Grok OAuth callback omitted the authorization code".into(),
        })
}
