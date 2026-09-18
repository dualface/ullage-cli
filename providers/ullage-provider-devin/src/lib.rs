//! Devin (devin.ai / Cognition) subscription authentication and usage polling.
//!
//! Sign-in mirrors the official CLI: a PKCE S256 browser flow against
//! `app.devin.ai/auth/cli/continue` that returns the authorization code on a
//! loopback `/callback`, exchanged through Connect-RPC
//! `ExchangePKCEAuthorizationCode` for an `api_key` and `api_server_url`. A
//! manual paste-the-key flow (`app.devin.ai/auth/cli/token`) covers headless
//! setups; the pasted value is the same `api_key` `GetUserStatus` expects in
//! its `metadata.apiKey`. There is nothing to refresh: the key stands until
//! the server rejects it.

mod api;
mod callback;
mod dto;

use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex, MutexGuard};

use async_trait::async_trait;
use chrono::{DateTime, Duration, Utc};
use ullage_auth::{
    AuthChallenge, AuthCompleteRequest, AuthInputRequest, AuthMethod, AuthStartRequest, AuthState,
    Credential, CredentialError, CredentialKey, CredentialStore, CredentialVersion, LogoutRequest,
    SecretValue,
};
use ullage_core::{
    Capability, Provider, ProviderDescriptor, ProviderError, ProviderId, ProviderResult,
    QueryOutcome, SubscriptionUsage, UsageQuery,
};
use url::Url;
use zeroize::Zeroizing;

pub use api::{
    ApiFailure, ApiFailureKind, DEFAULT_API_SERVER_URL, DevinApi, ExchangeResponse, HttpDevinApi,
};
pub use callback::{CallbackOutcome, LoopbackCallback};
pub use dto::{DevinUsage, PlanInfo, PlanStatus, UserStatus, UserStatusResponse};

/// The authorization page the official Devin CLI drives for PKCE sign-in.
const AUTHORIZE_URL: &str = "https://app.devin.ai/auth/cli/continue";
/// The page that issues a key for the manual paste flow, matching the
/// official `--force-manual-token-flow` hint.
const MANUAL_TOKEN_PAGE: &str = "https://app.devin.ai/auth/cli/token";
/// How long a started flow stays completable.
const FLOW_LIFETIME_MINUTES: i64 = 10;
static FLOW_SEQUENCE: AtomicU64 = AtomicU64::new(1);

/// What a pending browser flow needs from a loopback receiver, abstracted so
/// tests can drive `complete_auth` without a real socket.
pub trait CallbackSource: Send + Sync {
    /// The loopback URI this receiver answers on.
    fn redirect_uri(&self) -> String;
    fn take(&self) -> Option<CallbackOutcome>;
}

impl CallbackSource for LoopbackCallback {
    fn redirect_uri(&self) -> String {
        LoopbackCallback::redirect_uri(self).to_owned()
    }

    fn take(&self) -> Option<CallbackOutcome> {
        LoopbackCallback::take(self)
    }
}

/// Builds the loopback receiver for a browser flow. Swappable in tests;
/// production binds a real ephemeral port.
pub type CallbackFactory =
    dyn Fn(&str, DateTime<Utc>) -> Result<Arc<dyn CallbackSource>, ApiFailure> + Send + Sync;

#[derive(Default)]
struct ProviderState {
    generation: u64,
    session_id: u64,
    pending_flow: Option<PendingFlow>,
    /// The live session: `api_key` is the whole credential and
    /// `api_server_url` is where it reports. No expiry is known; a key
    /// stands until the server rejects it.
    session: Option<DevinSession>,
    invalid_reason: Option<String>,
    /// Store version `session` was observed at.
    stored_version: Option<CredentialVersion>,
    /// A stored record that will not decode is corrupt, not absent: the flag
    /// keeps `auth_status` able to report Invalid and logout able to delete
    /// the record even though no key can be read out of it.
    stored_credential_corrupt: bool,
    credentials_loaded: bool,
}

#[derive(Clone)]
struct DevinSession {
    api_key: Zeroizing<String>,
    api_server_url: String,
}

/// A sign-in in progress. It expires so an abandoned flow cannot wedge
/// re-authentication forever.
enum PendingFlow {
    /// PKCE browser flow. `flow_id` doubles as the OAuth `state`.
    Browser {
        flow_id: String,
        verifier: Zeroizing<String>,
        redirect_uri: String,
        expires_at: DateTime<Utc>,
        /// The provider-owned loopback receiver, present only when no client
        /// supplied its own `redirect_uri`.
        callback: Option<Arc<dyn CallbackSource>>,
    },
    /// Manual paste-the-key fallback for headless environments.
    ManualToken {
        flow_id: String,
        expires_at: DateTime<Utc>,
    },
}

impl PendingFlow {
    fn flow_id(&self) -> &str {
        match self {
            Self::Browser { flow_id, .. } | Self::ManualToken { flow_id, .. } => flow_id,
        }
    }

    fn expires_at(&self) -> DateTime<Utc> {
        match self {
            Self::Browser { expires_at, .. } | Self::ManualToken { expires_at, .. } => *expires_at,
        }
    }
}

impl ProviderState {
    /// Releases a pending flow whose lifetime ran out, returning whether one
    /// expired. Called at every entry point that `pending_flow` would
    /// otherwise block, so an abandoned flow cannot wedge queries or
    /// re-authentication forever.
    fn expire_pending_flow(&mut self) -> bool {
        if self
            .pending_flow
            .as_ref()
            .is_some_and(|pending| pending.expires_at() <= Utc::now())
        {
            self.pending_flow = None;
            return true;
        }
        false
    }
}

pub struct DevinProvider {
    api: Arc<dyn DevinApi>,
    callbacks: Arc<CallbackFactory>,
    state: Mutex<ProviderState>,
    credentials: Option<(Arc<CredentialStore>, CredentialKey)>,
    /// Serializes store writes and the state commits that adopt them so a
    /// racing sign-in, logout, or expiry cannot interleave between them.
    credential_gate: tokio::sync::Mutex<()>,
}

fn bind_loopback(
    state: &str,
    expires_at: DateTime<Utc>,
) -> Result<Arc<dyn CallbackSource>, ApiFailure> {
    Ok(Arc::new(
        LoopbackCallback::bind(state, expires_at).map_err(|_| {
            ApiFailure::network("the Devin OAuth loopback listener could not start")
        })?,
    ) as Arc<dyn CallbackSource>)
}

impl DevinProvider {
    pub fn new() -> ProviderResult<Self> {
        Ok(Self::with_api_and_callbacks(
            Arc::new(HttpDevinApi::new().map_err(ApiFailure::into_provider_error)?),
            Arc::new(bind_loopback),
        ))
    }

    pub fn new_with_store(credentials: Arc<CredentialStore>) -> ProviderResult<Self> {
        Self::new_with_store_for_account(credentials, "active")
    }

    pub fn new_with_store_for_account(
        credentials: Arc<CredentialStore>,
        account_id: &str,
    ) -> ProviderResult<Self> {
        let api = Arc::new(HttpDevinApi::new().map_err(ApiFailure::into_provider_error)?);
        Self::with_api_and_store_for_account(api, credentials, account_id)
    }

    pub fn with_api(api: Arc<dyn DevinApi>) -> Self {
        Self::with_api_and_callbacks(api, Arc::new(bind_loopback))
    }

    pub fn with_api_and_callbacks(api: Arc<dyn DevinApi>, callbacks: Arc<CallbackFactory>) -> Self {
        Self {
            api,
            callbacks,
            state: Mutex::new(ProviderState {
                credentials_loaded: true,
                ..ProviderState::default()
            }),
            credentials: None,
            credential_gate: tokio::sync::Mutex::new(()),
        }
    }

    pub fn with_api_and_store(
        api: Arc<dyn DevinApi>,
        credentials: Arc<CredentialStore>,
    ) -> ProviderResult<Self> {
        Self::with_api_and_store_for_account(api, credentials, "active")
    }

    pub fn with_api_and_store_for_account(
        api: Arc<dyn DevinApi>,
        credentials: Arc<CredentialStore>,
        account_id: &str,
    ) -> ProviderResult<Self> {
        Self::with_api_callbacks_and_store_for_account(
            api,
            Arc::new(bind_loopback),
            credentials,
            account_id,
        )
    }

    pub fn with_api_callbacks_and_store_for_account(
        api: Arc<dyn DevinApi>,
        callbacks: Arc<CallbackFactory>,
        credentials: Arc<CredentialStore>,
        account_id: &str,
    ) -> ProviderResult<Self> {
        let key = CredentialKey::new("devin", account_id).map_err(credential_error)?;
        Ok(Self {
            api,
            callbacks,
            state: Mutex::new(ProviderState::default()),
            credentials: Some((credentials, key)),
            credential_gate: tokio::sync::Mutex::new(()),
        })
    }

    /// Expire an abandoned pending flow under `credential_gate`, serialized
    /// against `install_session`'s persist+commit window: a flow cleared
    /// between a completed sign-in's store write and its state commit would
    /// leave the store holding a credential the session never adopted.
    async fn expire_pending_flow_gated(&self) -> ProviderResult<bool> {
        let _persist_gate = self.credential_gate.lock().await;
        Ok(self.lock_state()?.expire_pending_flow())
    }

    async fn ensure_credentials_loaded(&self) -> ProviderResult<()> {
        let Some((credentials, key)) = &self.credentials else {
            return Ok(());
        };
        let _gate = self.credential_gate.lock().await;
        if self.lock_state()?.credentials_loaded {
            return Ok(());
        }
        let stored = match credentials.get(key) {
            Ok(stored) => stored,
            Err(CredentialError::NotFound) => {
                let mut state = self.lock_state()?;
                state.stored_version = None;
                state.credentials_loaded = true;
                return Ok(());
            }
            Err(error) => return Err(credential_error(error)),
        };
        let stored_version = stored.version();
        match credential_string(stored.credential(), "api_key") {
            Ok(api_key) => {
                let api_server_url = credential_string(stored.credential(), "api_server_url")
                    .unwrap_or_else(|_| DEFAULT_API_SERVER_URL.to_owned());
                let mut state = self.lock_state()?;
                state.generation = state.generation.wrapping_add(1);
                state.session_id = state.session_id.wrapping_add(1);
                state.session = Some(DevinSession {
                    api_key: Zeroizing::new(api_key),
                    api_server_url,
                });
                state.stored_version = Some(stored_version);
                state.credentials_loaded = true;
            }
            // The record exists but does not decode into a usable key: it is
            // corrupt, not absent. Invalid lets sign-in repair it and logout
            // delete it.
            Err(_) => {
                let mut state = self.lock_state()?;
                state.stored_version = Some(stored_version);
                state.stored_credential_corrupt = true;
                state.credentials_loaded = true;
            }
        }
        Ok(())
    }

    /// Persists the validated credential, then commits it as the live
    /// session. The persist gate spans the store write and the commit so a
    /// racing sign-in or logout cannot interleave between them; store I/O
    /// itself stays outside the state lock.
    async fn install_session(
        &self,
        session: DevinSession,
        expected_generation: u64,
        expected_flow: &str,
    ) -> ProviderResult<AuthState> {
        let _persist_gate = self.credential_gate.lock().await;
        {
            let state = self.lock_state()?;
            if !Self::flow_is_current(&state, expected_generation, expected_flow) {
                return Err(ProviderError::ProtocolIncompatible {
                    message: "Devin authentication operation was superseded".into(),
                });
            }
        }
        let stored_version = if let Some((store, key)) = &self.credentials {
            let mut credential = Credential::new();
            credential
                .insert("api_key", SecretValue::new(session.api_key.as_bytes()))
                .map_err(credential_error)?;
            credential
                .insert(
                    "api_server_url",
                    SecretValue::new(session.api_server_url.as_bytes()),
                )
                .map_err(credential_error)?;
            // A completed sign-in supersedes whatever the store holds.
            Some(
                store
                    .set(key, credential)
                    .map_err(credential_error)?
                    .version(),
            )
        } else {
            None
        };
        let mut state = self.lock_state()?;
        if !Self::flow_is_current(&state, expected_generation, expected_flow) {
            return Err(ProviderError::ProtocolIncompatible {
                message: "Devin authentication operation was superseded".into(),
            });
        }
        state.stored_version = stored_version;
        state.generation = state.generation.wrapping_add(1);
        state.session_id = state.session_id.wrapping_add(1);
        state.pending_flow = None;
        state.invalid_reason = None;
        state.stored_credential_corrupt = false;
        state.session = Some(session);
        Ok(AuthState::Authenticated {
            // GetUserStatus reports no account identity, so nothing can name
            // the signed-in user.
            account_label: None,
            account_key: None,
            expires_at: None,
        })
    }

    /// Applies a failed sign-in validation or a rejected key seen while
    /// querying. Under the persist gate so the writes cannot interleave with
    /// `install_session`.
    async fn resolve_api_failure(
        &self,
        expected_generation: u64,
        expected_flow: Option<&str>,
        error: ApiFailure,
    ) -> ProviderError {
        let _persist_gate = self.credential_gate.lock().await;
        let mut state = match self.lock_state() {
            Ok(state) => state,
            Err(error) => return error,
        };
        let operation_is_current = state.generation == expected_generation
            && match expected_flow {
                Some(flow_id) => {
                    state.pending_flow.as_ref().map(PendingFlow::flow_id) == Some(flow_id)
                }
                None => state.session.is_some() && state.pending_flow.is_none(),
            };
        if !operation_is_current {
            return ProviderError::ProtocolIncompatible {
                message: "Devin authentication operation was superseded".into(),
            };
        }
        if error.kind == ApiFailureKind::Authentication {
            if expected_flow.is_some() {
                // The failure belongs to the pending flow alone: dropping the
                // live session behind it would downgrade a healthy sign-in.
                state.pending_flow = None;
                if state.session.is_none() {
                    state.invalid_reason = Some(error.message.clone());
                }
            } else {
                state.generation = state.generation.wrapping_add(1);
                state.session_id = state.session_id.wrapping_add(1);
                state.pending_flow = None;
                state.session = None;
                state.invalid_reason = Some(error.message.clone());
            }
        }
        error.into_provider_error()
    }

    fn flow_is_current(state: &ProviderState, generation: u64, flow_id: &str) -> bool {
        state.generation == generation
            && state.pending_flow.as_ref().map(PendingFlow::flow_id) == Some(flow_id)
    }

    fn lock_state(&self) -> ProviderResult<MutexGuard<'_, ProviderState>> {
        self.state
            .lock()
            .map_err(|_| ProviderError::ProtocolIncompatible {
                message: "Devin authentication state is unavailable".into(),
            })
    }

    async fn start_browser_auth(&self, request: AuthStartRequest) -> ProviderResult<AuthChallenge> {
        let flow_id = new_flow_id()?;
        let verifier = Zeroizing::new(ullage_auth::random_url_token().map_err(|_| {
            ProviderError::Network {
                message: "operating system randomness is unavailable".into(),
            }
        })?);
        let challenge_secret = ullage_auth::pkce_s256_challenge(&verifier);
        let expires_at = Utc::now() + Duration::minutes(FLOW_LIFETIME_MINUTES);
        // A client that runs its own loopback receiver (the macOS app) passes
        // the URI in; otherwise the provider listens itself and the flow
        // completes by polling instead of a paste.
        let (redirect_uri, callback, pasted_input) = match request.redirect_uri {
            Some(uri) => {
                ullage_auth::validate_loopback_http_redirect_uri(&uri).map_err(|reason| {
                    ProviderError::AuthenticationInvalid {
                        message: reason.into(),
                    }
                })?;
                (uri, None, true)
            }
            None => {
                let callback = (self.callbacks)(&flow_id, expires_at)
                    .map_err(ApiFailure::into_provider_error)?;
                (callback.redirect_uri(), Some(callback), false)
            }
        };
        let authorization_url = authorize_url(&redirect_uri, &flow_id, &challenge_secret)?;
        let challenge = AuthChallenge {
            flow_id: flow_id.clone(),
            method: AuthMethod::BrowserOAuth,
            verification_uri: Some(authorization_url),
            user_code: None,
            expires_at: Some(expires_at),
            input: pasted_input.then(|| {
                AuthInputRequest::visible("the full callback URL from the browser, or code#state")
            }),
        };
        let mut state = self.lock_state()?;
        // Starting a replacement flow deliberately supersedes any persisted
        // credential without first validating that potentially invalid key.
        state.credentials_loaded = true;
        state.generation = state.generation.wrapping_add(1);
        state.session_id = state.session_id.wrapping_add(1);
        state.pending_flow = Some(PendingFlow::Browser {
            flow_id,
            verifier,
            redirect_uri,
            expires_at,
            callback,
        });
        state.invalid_reason = None;
        Ok(challenge)
    }
}

#[async_trait]
impl Provider for DevinProvider {
    type VendorUsage = DevinUsage;

    fn descriptor(&self) -> ProviderDescriptor {
        ProviderDescriptor {
            id: ProviderId::new("devin"),
            display_name: "Devin".into(),
            capabilities: vec![
                Capability::Authentication,
                Capability::AuthenticationStatus,
                Capability::Logout,
                Capability::UsageQuery,
            ],
        }
    }

    async fn start_auth(&self, request: AuthStartRequest) -> ProviderResult<AuthChallenge> {
        match request.method {
            None | Some(AuthMethod::BrowserOAuth) => {
                let _credential_gate = self.credential_gate.lock().await;
                self.start_browser_auth(request).await
            }
            Some(AuthMethod::ApiToken) => {
                let _credential_gate = self.credential_gate.lock().await;
                let flow_id = new_flow_id()?;
                // The flow waits for user input, so it gets a lifetime:
                // abandoning it must not wedge the pending slot forever.
                let expires_at = Utc::now() + Duration::minutes(FLOW_LIFETIME_MINUTES);
                let challenge = AuthChallenge {
                    flow_id: flow_id.clone(),
                    method: AuthMethod::ApiToken,
                    verification_uri: Some(MANUAL_TOKEN_PAGE.into()),
                    user_code: None,
                    expires_at: Some(expires_at),
                    input: Some(AuthInputRequest::secret("the Devin API key")),
                };
                let mut state = self.lock_state()?;
                state.credentials_loaded = true;
                state.generation = state.generation.wrapping_add(1);
                state.session_id = state.session_id.wrapping_add(1);
                state.pending_flow = Some(PendingFlow::ManualToken {
                    flow_id,
                    expires_at,
                });
                state.invalid_reason = None;
                Ok(challenge)
            }
            Some(method) => Err(ProviderError::UnsupportedCapability {
                capability: format!("Devin authentication method {method:?}"),
            }),
        }
    }

    async fn complete_auth(&self, request: AuthCompleteRequest) -> ProviderResult<AuthState> {
        self.ensure_credentials_loaded().await?;
        let AuthCompleteRequest {
            flow_id,
            authorization_code,
            redirect_uri,
        } = request;
        enum PendingSnapshot {
            Browser {
                verifier: Zeroizing<String>,
                redirect_uri: String,
                expires_at: DateTime<Utc>,
                callback: Option<Arc<dyn CallbackSource>>,
            },
            ManualToken {
                expires_at: DateTime<Utc>,
            },
        }
        let (generation, pending) = {
            let state = self.lock_state()?;
            let pending = state
                .pending_flow
                .as_ref()
                .filter(|pending| pending.flow_id() == flow_id)
                .ok_or_else(|| ProviderError::ProtocolIncompatible {
                    message: "Devin authentication flow ID is not active".into(),
                })?;
            let snapshot = match pending {
                PendingFlow::Browser {
                    verifier,
                    redirect_uri,
                    expires_at,
                    callback,
                    ..
                } => PendingSnapshot::Browser {
                    verifier: verifier.clone(),
                    redirect_uri: redirect_uri.clone(),
                    expires_at: *expires_at,
                    callback: callback.clone(),
                },
                PendingFlow::ManualToken { expires_at, .. } => PendingSnapshot::ManualToken {
                    expires_at: *expires_at,
                },
            };
            (state.generation, snapshot)
        };
        let expires_at = match &pending {
            PendingSnapshot::Browser { expires_at, .. }
            | PendingSnapshot::ManualToken { expires_at, .. } => *expires_at,
        };
        if expires_at <= Utc::now() {
            // The abandoned flow must release the pending slot, under the
            // persist gate so the clear cannot interleave with a concurrent
            // install_session's store write and commit.
            let _persist_gate = self.credential_gate.lock().await;
            let mut state = self.lock_state()?;
            if state.pending_flow.as_ref().map(PendingFlow::flow_id) == Some(flow_id.as_str()) {
                state.pending_flow = None;
            }
            return Err(ProviderError::AuthenticationInvalid {
                message: "Devin sign-in expired".into(),
            });
        }

        match pending {
            PendingSnapshot::ManualToken { .. } => {
                // `authorization_code` arrives as protocol plaintext; from
                // here on the only held copies are Zeroizing.
                let api_key = Zeroizing::new(authorization_code.unwrap_or_default());
                if api_key.trim().is_empty() {
                    return Err(ProviderError::AuthenticationInvalid {
                        message: "a Devin API key is required".into(),
                    });
                }
                let api_key = Zeroizing::new(api_key.trim().to_owned());
                // The key is also the usage credential: validating it once at
                // sign-in proves it works before it is persisted.
                if let Err(error) = self.api.user_status(&api_key, DEFAULT_API_SERVER_URL).await {
                    return Err(self
                        .resolve_api_failure(generation, Some(&flow_id), error)
                        .await);
                }
                self.install_session(
                    DevinSession {
                        api_key,
                        api_server_url: DEFAULT_API_SERVER_URL.to_owned(),
                    },
                    generation,
                    &flow_id,
                )
                .await
            }
            PendingSnapshot::Browser {
                verifier,
                redirect_uri: expected_redirect,
                callback,
                ..
            } => {
                if redirect_uri
                    .as_deref()
                    .is_some_and(|redirect| redirect != expected_redirect)
                {
                    return Err(ProviderError::AuthenticationInvalid {
                        message: "Devin OAuth redirect URI does not match the initiated flow"
                            .into(),
                    });
                }
                let pasted = authorization_code
                    .as_deref()
                    .map(str::trim)
                    .filter(|value| !value.is_empty());
                let code = match pasted {
                    Some(input) => {
                        let (code, supplied_state) =
                            parse_authorization_input(input, &expected_redirect)?;
                        if supplied_state.as_deref() != Some(flow_id.as_str()) {
                            return Err(ProviderError::AuthenticationInvalid {
                                message: "Devin OAuth state does not match".into(),
                            });
                        }
                        code
                    }
                    None => {
                        let Some(callback) = &callback else {
                            return Err(ProviderError::AuthenticationInvalid {
                                message: "Devin OAuth authorization code is missing".into(),
                            });
                        };
                        match callback.take() {
                            Some(CallbackOutcome::Code(code)) => code,
                            Some(CallbackOutcome::Denied(_)) => {
                                return Err(self
                                    .resolve_api_failure(
                                        generation,
                                        Some(&flow_id),
                                        ApiFailure::authentication("Devin denied the sign-in"),
                                    )
                                    .await);
                            }
                            None => {
                                return Ok(AuthState::Pending {
                                    flow_id,
                                    expires_at: Some(expires_at),
                                });
                            }
                        }
                    }
                };
                let response = match self
                    .api
                    .exchange_pkce_code(&code, &verifier, &expected_redirect)
                    .await
                {
                    Ok(response) => response,
                    Err(error) => {
                        return Err(self
                            .resolve_api_failure(generation, Some(&flow_id), error)
                            .await);
                    }
                };
                if response.api_key.trim().is_empty() {
                    return Err(ProviderError::ProtocolIncompatible {
                        message: "Devin returned an empty API key".into(),
                    });
                }
                let api_server_url = match response.api_server_url.as_deref() {
                    None => DEFAULT_API_SERVER_URL.to_owned(),
                    Some(url) => {
                        let url = url.trim().trim_end_matches('/');
                        let parsed = Url::parse(url).ok().filter(|parsed| {
                            parsed.scheme() == "https" && parsed.host().is_some()
                                || parsed.scheme() == "http"
                                    && parsed.host_str().is_some_and(|host| {
                                        host == "localhost"
                                            || host
                                                .parse::<std::net::IpAddr>()
                                                .is_ok_and(|ip| ip.is_loopback())
                                    })
                        });
                        if parsed.is_none() {
                            return Err(ProviderError::ProtocolIncompatible {
                                message: "Devin returned an unusable API server URL".into(),
                            });
                        }
                        url.to_owned()
                    }
                };
                self.install_session(
                    DevinSession {
                        api_key: Zeroizing::new(response.api_key.trim().to_owned()),
                        api_server_url,
                    },
                    generation,
                    &flow_id,
                )
                .await
            }
        }
    }

    async fn auth_status(&self) -> ProviderResult<AuthState> {
        self.ensure_credentials_loaded().await?;
        // A flow nobody finished would stay pending forever otherwise. Expiry
        // and the snapshot share one credential_gate section: start_auth
        // installs its replacement flow under the same gate, so a fresh
        // challenge is never masked by a stale expiry result.
        let _persist_gate = self.credential_gate.lock().await;
        let mut state = self.lock_state()?;
        state.expire_pending_flow();
        if let Some(reason) = &state.invalid_reason {
            return Ok(AuthState::Invalid {
                reason: reason.clone(),
                account_key: None,
            });
        }
        if state.session.is_some() {
            return Ok(AuthState::Authenticated {
                account_label: None,
                account_key: None,
                expires_at: None,
            });
        }
        if let Some(pending) = &state.pending_flow {
            return Ok(AuthState::Pending {
                flow_id: pending.flow_id().to_owned(),
                expires_at: Some(pending.expires_at()),
            });
        }
        if state.stored_credential_corrupt {
            return Ok(AuthState::Invalid {
                reason: "the stored Devin credential is unreadable; sign in again".into(),
                account_key: None,
            });
        }
        Ok(AuthState::NotAuthenticated)
    }

    async fn logout(&self, request: LogoutRequest) -> ProviderResult<()> {
        // Devin exposes no key-revocation endpoint for this credential, so
        // logout is local-only: the stored record is deleted and the key's
        // own lifetime bounds any residual server-side validity.
        if request.account_label.is_some() {
            self.ensure_credentials_loaded().await?;
        }
        let _credential_gate = self.credential_gate.lock().await;
        let mut state = self.lock_state()?;
        // Devin never learns an account label, so a label-scoped logout can
        // only proceed when a corrupt record makes the guard unverifiable;
        // deleting it is the recovery path.
        let unverifiable = state.stored_credential_corrupt && state.session.is_none();
        if !unverifiable && request.account_label.is_some() {
            return Err(ProviderError::AuthenticationInvalid {
                message: "requested Devin account is not authenticated".into(),
            });
        }
        if let Some((store, key)) = &self.credentials {
            match store.delete(key) {
                Ok(()) | Err(CredentialError::NotFound) => {}
                Err(error) => return Err(credential_error(error)),
            }
        }
        state.generation = state.generation.wrapping_add(1);
        state.session_id = state.session_id.wrapping_add(1);
        state.pending_flow = None;
        state.session = None;
        state.stored_version = None;
        state.invalid_reason = None;
        state.stored_credential_corrupt = false;
        Ok(())
    }

    async fn query(&self, _query: UsageQuery) -> ProviderResult<QueryOutcome<Self::VendorUsage>> {
        self.ensure_credentials_loaded().await?;
        self.expire_pending_flow_gated().await?;
        let (session, generation) = {
            let state = self.lock_state()?;
            let session =
                state
                    .session
                    .clone()
                    .ok_or_else(|| ProviderError::AuthenticationInvalid {
                        message: "Devin is not authenticated".into(),
                    })?;
            (session, state.generation)
        };
        let response = match self
            .api
            .user_status(&session.api_key, &session.api_server_url)
            .await
        {
            Ok(response) => response,
            Err(error) => {
                return Err(self.resolve_api_failure(generation, None, error).await);
            }
        };
        let plan_status = response
            .user_status
            .and_then(|user_status| user_status.plan_status)
            .ok_or_else(|| ProviderError::ProtocolIncompatible {
                message: "Devin user status omitted the plan status".into(),
            })?;
        Ok(QueryOutcome::Complete {
            data: DevinUsage {
                plan_status,
                observed_at: Utc::now(),
            },
        })
    }

    fn normalize(&self, vendor_usage: Self::VendorUsage) -> ProviderResult<SubscriptionUsage> {
        dto::normalize(vendor_usage)
    }
}

fn authorize_url(redirect_uri: &str, state: &str, challenge: &str) -> ProviderResult<String> {
    let mut url = Url::parse(AUTHORIZE_URL).map_err(|_| ProviderError::ProtocolIncompatible {
        message: "built-in Devin authorization endpoint is invalid".into(),
    })?;
    // The parameter set mirrors the official CLI's authorization URL.
    url.query_pairs_mut()
        .append_pair("redirect_uri", redirect_uri)
        .append_pair("state", state)
        .append_pair("prompt", "select_account")
        .append_pair("code_challenge", challenge)
        .append_pair("code_challenge_method", "S256")
        .append_pair("cli_pkce_marker", "1");
    Ok(url.into())
}

/// The authorization code a pasted callback carries, checked against the
/// initiated redirect. Accepted shapes: the whole callback URL, whose origin
/// and path must equal the flow's `redirect_uri`, or `code#state`, or a bare
/// `code` (which leaves the state check to the caller).
fn parse_authorization_input(
    input: &str,
    expected_redirect_uri: &str,
) -> ProviderResult<(String, Option<String>)> {
    let input = input.trim();
    if input.is_empty() || input.len() > 8192 {
        return Err(ProviderError::AuthenticationInvalid {
            message: "Devin OAuth authorization input is empty or too long".into(),
        });
    }
    if input.contains("://") {
        let url = Url::parse(input).map_err(|_| ProviderError::AuthenticationInvalid {
            message: "Devin OAuth callback URL is invalid".into(),
        })?;
        let expected =
            Url::parse(expected_redirect_uri).map_err(|_| ProviderError::ProtocolIncompatible {
                message: "Devin OAuth expected redirect URI is invalid".into(),
            })?;
        if url.scheme() != expected.scheme()
            || url.host_str() != expected.host_str()
            || url.port_or_known_default() != expected.port_or_known_default()
            || url.path() != expected.path()
        {
            return Err(ProviderError::AuthenticationInvalid {
                message: "Devin OAuth callback URL has an unexpected origin or path".into(),
            });
        }
        let code = url
            .query_pairs()
            .find(|(key, _)| key == "code")
            .map(|(_, value)| value.into_owned())
            .filter(|value| !value.is_empty())
            .ok_or_else(|| ProviderError::AuthenticationInvalid {
                message: "Devin OAuth callback omitted the authorization code".into(),
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
                message: "Devin OAuth code or state is empty".into(),
            });
        }
        return Ok((code.into(), Some(state.into())));
    }
    Ok((input.into(), None))
}

fn new_flow_id() -> ProviderResult<String> {
    // A timestamp plus sequence is guessable, so the id carries 256 bits of
    // randomness: it is the bearer for completing a pending flow and doubles
    // as the OAuth `state`.
    Ok(format!(
        "devin-flow-{}-{}",
        FLOW_SEQUENCE.fetch_add(1, Ordering::Relaxed),
        ullage_auth::random_url_token().map_err(|_| ProviderError::Network {
            message: "operating system randomness is unavailable".into(),
        })?
    ))
}

fn credential_string(credential: &Credential, field: &str) -> ProviderResult<String> {
    let value = credential
        .get(field)
        .ok_or_else(|| credential_error(CredentialError::CorruptCredential))?;
    String::from_utf8(value.expose().to_vec())
        .map_err(|_| credential_error(CredentialError::CorruptCredential))
}

fn credential_error(error: CredentialError) -> ProviderError {
    ProviderError::ProtocolIncompatible {
        message: error.provider_message("Devin credential storage is unavailable or invalid"),
    }
}

#[cfg(test)]
#[path = "lib_tests.rs"]
mod tests;
