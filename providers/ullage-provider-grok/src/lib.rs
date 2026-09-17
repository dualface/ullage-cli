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
    CredentialError, CredentialKey, CredentialStore, CredentialVersion, LogoutRequest,
    ReplaceOutcome, SecretValue, account_identity,
};
use ullage_core::{
    Capability, PartialFailure, Provider, ProviderDescriptor, ProviderError, ProviderId,
    ProviderResult, QueryOutcome, SubscriptionUsage, UsageQuery,
};

/// Cap for the settings fetch, which overlaps billing so a hang cannot add a
/// second wait after a slow billing response. A billing failure drops the
/// in-flight request instead of waiting out this deadline.
const SETTINGS_FETCH_DEADLINE: Duration = Duration::from_secs(8);

/// Tokens this close to expiry are refreshed before use, so a token cannot
/// die while a query is in flight. Matches the early-refresh margin the other
/// OAuth providers use.
const REFRESH_SKEW_SECONDS: i64 = 300;

#[derive(Default)]
struct Session {
    generation: u64,
    pending: Option<PendingAuthorization>,
    token: Option<OAuthToken>,
    /// Store version `token` was observed at; required for CAS updates.
    stored_version: Option<CredentialVersion>,
    /// The stored record loaded but was not a usable token. Kept distinct
    /// from "no credential" so `auth_status` can report `Invalid` while
    /// `start_auth` and `logout` still repair or delete the record.
    stored_credential_corrupt: bool,
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
        let loaded = match credentials.get(key) {
            Ok(stored) => {
                let version = stored.version();
                let parsed = stored
                    .credential()
                    .get("session")
                    .ok_or(CredentialError::CorruptCredential)
                    .and_then(|payload| {
                        serde_json::from_slice::<OAuthToken>(payload.expose())
                            .map_err(|_| CredentialError::CorruptCredential)
                    })
                    .and_then(|token| {
                        token
                            .validate_stored()
                            .map_err(|_| CredentialError::CorruptCredential)
                            .map(|_| token)
                    });
                Some((parsed, version))
            }
            Err(CredentialError::NotFound) => None,
            Err(error) => return Err(credential_error(error)),
        };
        let mut session = self.lock_session()?;
        if !session.credentials_loaded {
            match loaded {
                Some((Ok(token), version)) => {
                    session.stored_version = Some(version);
                    session.token = Some(token);
                }
                // A record that will not parse is corrupt, not absent: the
                // flag keeps every entry point able to repair or delete it.
                // Its store version is still kept so logout can CAS-delete it.
                Some((Err(_), version)) => {
                    session.stored_credential_corrupt = true;
                    session.stored_version = Some(version);
                }
                None => {}
            }
            session.credentials_loaded = true;
        }
        Ok(())
    }

    /// The "no usable credential" error for entry points that cannot proceed.
    /// A corrupt record is reported as corrupt, not as a missing sign-in.
    fn unauthenticated_error(session: &Session) -> ProviderError {
        if session.stored_credential_corrupt {
            ProviderError::AuthenticationInvalid {
                message: "the stored Grok credential is unreadable; sign in again".into(),
            }
        } else {
            ProviderError::AuthenticationInvalid {
                message: "Grok is not authenticated".into(),
            }
        }
    }

    fn encode_token(token: &OAuthToken) -> ProviderResult<Credential> {
        let encoded =
            serde_json::to_vec(token).map_err(|_| ProviderError::ProtocolIncompatible {
                message: "Grok credential serialization failed".into(),
            })?;
        let mut credential = Credential::new();
        credential
            .insert("session", SecretValue::new(encoded))
            .map_err(credential_error)?;
        Ok(credential)
    }

    /// Unconditional write, valid only when installing a freshly completed
    /// sign-in. Returns the version the record now carries.
    fn persist_created_token(
        &self,
        token: &OAuthToken,
    ) -> ProviderResult<Option<CredentialVersion>> {
        let Some((credentials, key)) = &self.credentials else {
            return Ok(None);
        };
        let stored = credentials
            .set(key, Self::encode_token(token)?)
            .map_err(credential_error)?;
        Ok(Some(stored.version()))
    }

    /// CAS write for a token rotated from the record observed at `expected`.
    /// A conflict or a deleted record means the session is stale and the
    /// rotation is dropped instead of overwriting it.
    fn persist_rotated_token(
        &self,
        expected: Option<CredentialVersion>,
        token: &OAuthToken,
    ) -> ProviderResult<Option<CredentialVersion>> {
        let Some((credentials, key)) = &self.credentials else {
            return Ok(None);
        };
        let expected = expected.ok_or_else(|| ProviderError::AuthenticationInvalid {
            message: "Grok credential has no observed store version".into(),
        })?;
        match credentials.replace(key, expected, Self::encode_token(token)?) {
            Ok(ReplaceOutcome::Replaced(stored)) => Ok(Some(stored.version())),
            Ok(ReplaceOutcome::VersionConflict) | Err(CredentialError::NotFound) => {
                Err(ProviderError::AuthenticationInvalid {
                    message: "stored Grok credential changed during refresh".into(),
                })
            }
            Err(error) => Err(credential_error(error)),
        }
    }

    fn persist_cleared(&self) -> ProviderResult<()> {
        let Some((credentials, key)) = &self.credentials else {
            return Ok(());
        };
        match credentials.delete(key) {
            Ok(()) | Err(CredentialError::NotFound) => Ok(()),
            Err(error) => Err(credential_error(error)),
        }
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
            let previous = session
                .token
                .clone()
                .ok_or_else(|| Self::unauthenticated_error(&session))?;
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
        let expected_version = {
            let session = self.lock_session()?;
            if session.generation != generation || session.token.as_ref() != Some(&previous) {
                return Err(ProviderError::AuthenticationInvalid {
                    message: "Grok authentication state changed during refresh".into(),
                });
            }
            session.stored_version
        };
        // Store I/O runs outside the session lock; the lifecycle lock held by
        // the caller serializes this against a sign-in or logout persist.
        let stored_version = self.persist_rotated_token(expected_version, &token)?;
        let mut session = self.lock_session()?;
        if session.generation != generation || session.token.as_ref() != Some(&previous) {
            return Err(ProviderError::AuthenticationInvalid {
                message: "Grok authentication state changed during refresh".into(),
            });
        }
        session.stored_version = stored_version;
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
        {
            let session = self.lock_session()?;
            if session.generation != generation
                || session.pending.as_ref().map(PendingAuthorization::flow_id) != Some(flow_id)
            {
                return Err(ProviderError::AuthenticationInvalid {
                    message: "Grok OAuth flow is no longer current".into(),
                });
            }
        }
        // Installing a completed sign-in supersedes any stored record. The
        // write runs outside the session lock; the caller's lifecycle lock
        // keeps it serialized against refresh and logout.
        let stored_version = self.persist_created_token(&token)?;
        let mut session = self.lock_session()?;
        session.stored_version = stored_version;
        session.stored_credential_corrupt = false;
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
        // Held across the provider call on purpose: `install_token` persists
        // outside the session lock, and this is what serializes that write
        // against a concurrent refresh or logout.
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
        let expires_soon = Utc::now() + chrono::Duration::seconds(REFRESH_SKEW_SECONDS);
        let expired = self.lock_session()?.token.as_ref().is_some_and(|token| {
            token
                .expires_at
                .is_some_and(|expiry| expiry <= expires_soon)
        });
        if expired {
            // A refresh the server refuses leaves the account unusable, but it
            // is still that account: reporting the identity is what lets a fresh
            // sign-in as the same user replace this row. The key is hashed the
            // same way `Authenticated` hashes it, or the two states could never
            // name the same account.
            let account_key = self
                .lock_session()?
                .token
                .as_ref()
                .and_then(|token| token.account_label.as_deref().and_then(account_identity));
            return match self.refresh_auth().await {
                Ok(state) => Ok(state),
                Err(ProviderError::AuthenticationInvalid { message }) => Ok(AuthState::Invalid {
                    reason: message,
                    account_key,
                }),
                Err(error) => Err(error),
            };
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
        if session.stored_credential_corrupt {
            return Ok(AuthState::Invalid {
                reason: "the stored Grok credential is unreadable; sign in again".into(),
                // The corrupt record cannot be decoded, so it names no account.
                account_key: None,
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
                // A corrupt record names no account, so the label guard cannot
                // apply; deleting it is the recovery path.
                let unverifiable = session.stored_credential_corrupt && token.is_none();
                if !unverifiable {
                    match token
                        .as_ref()
                        .and_then(|token| token.account_label.as_ref())
                    {
                        Some(actual) if actual == expected => {}
                        _ => {
                            return Err(ProviderError::AuthenticationInvalid {
                                message:
                                    "Grok logout account does not match the authenticated account"
                                        .into(),
                            });
                        }
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
            let still_current = {
                let session = self.lock_session()?;
                session.token.as_ref() == Some(&token)
            };
            if still_current {
                // Store I/O outside the session lock; the lifecycle lock
                // serializes it against refresh and sign-in.
                self.persist_cleared()?;
                let mut session = self.lock_session()?;
                session.stored_version = None;
                session.token = None;
                session.stored_credential_corrupt = false;
            }
        } else {
            self.persist_cleared()?;
            let mut session = self.lock_session()?;
            session.stored_version = None;
            session.stored_credential_corrupt = false;
        }
        Ok(())
    }

    async fn query(&self, _request: UsageQuery) -> ProviderResult<QueryOutcome<Self::VendorUsage>> {
        self.ensure_credentials_loaded()?;
        let expires_soon = Utc::now() + chrono::Duration::seconds(REFRESH_SKEW_SECONDS);
        let expired = self.lock_session()?.token.as_ref().is_some_and(|token| {
            token
                .expires_at
                .is_some_and(|expiry| expiry <= expires_soon)
        });
        if expired {
            self.refresh_auth().await?;
        }
        let (mut generation, mut token) = {
            let session = self.lock_session()?;
            let token = session
                .token
                .clone()
                .ok_or_else(|| Self::unauthenticated_error(&session))?;
            (session.generation, token)
        };

        // Settings overlaps billing so a hang cannot add a second wait after a
        // slow billing response. It is only driven while billing is still in
        // flight; a billing failure drops the in-flight request instead of
        // waiting out its deadline.
        let mut retried = false;
        let (response, settings_result) = loop {
            let access_token = token.access_token.clone();
            let settings = tokio::time::timeout(
                SETTINGS_FETCH_DEADLINE,
                self.transport.fetch_settings(&access_token),
            );
            let mut settings = std::pin::pin!(settings);
            let mut billing = std::pin::pin!(self.transport.fetch_billing(&access_token));
            let mut settings_result = None;
            let billing_result = std::future::poll_fn(|context| {
                use std::task::Poll;
                if let Poll::Ready(result) = billing.as_mut().poll(context) {
                    return Poll::Ready(result);
                }
                if settings_result.is_none() {
                    if let Poll::Ready(result) = settings.as_mut().poll(context) {
                        settings_result = Some(result);
                    }
                }
                Poll::Pending
            })
            .await;
            match billing_result {
                // An access token rejected mid-flight gets one refresh and
                // retry. A missing refresh token, a refused refresh, or a
                // second rejection is a real sign-out and surfaces as
                // AuthenticationInvalid.
                Err(GrokApiError::AuthenticationInvalid(_)) if !retried => {
                    self.refresh_auth().await?;
                    let session = self.lock_session()?;
                    token = session
                        .token
                        .clone()
                        .ok_or_else(|| Self::unauthenticated_error(&session))?;
                    generation = session.generation;
                    retried = true;
                }
                Err(error) => return Err(error.into()),
                Ok(response) => {
                    let settings_result = match settings_result {
                        Some(result) => result,
                        None => settings.await,
                    };
                    let settings_result = match settings_result {
                        // Settings rejecting the same token gets the same
                        // one-shot refresh and retry billing gets; a second
                        // rejection is a real sign-out and surfaces as
                        // AuthenticationInvalid rather than Partial.
                        Ok(Err(GrokApiError::AuthenticationInvalid(message))) => {
                            if retried {
                                return Err(GrokApiError::AuthenticationInvalid(message).into());
                            }
                            self.refresh_auth().await?;
                            {
                                let session = self.lock_session()?;
                                token = session
                                    .token
                                    .clone()
                                    .ok_or_else(|| Self::unauthenticated_error(&session))?;
                                generation = session.generation;
                            }
                            let access_token = token.access_token.clone();
                            match tokio::time::timeout(
                                SETTINGS_FETCH_DEADLINE,
                                self.transport.fetch_settings(&access_token),
                            )
                            .await
                            {
                                Ok(Err(GrokApiError::AuthenticationInvalid(message))) => {
                                    return Err(GrokApiError::AuthenticationInvalid(message).into());
                                }
                                retried_result => retried_result,
                            }
                        }
                        other => other,
                    };
                    break (response, settings_result);
                }
            }
        };
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
