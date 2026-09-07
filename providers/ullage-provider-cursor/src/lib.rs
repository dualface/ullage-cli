//! Cursor personal-account authentication and monthly DashboardService usage.

mod api;
mod dto;

use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex, MutexGuard};

use async_trait::async_trait;
use base64::Engine;
use base64::engine::general_purpose::URL_SAFE_NO_PAD;
use chrono::{DateTime, Duration, Utc};
use sha2::{Digest, Sha256};
use ullage_auth::{
    AuthChallenge, AuthCompleteRequest, AuthInputRequest, AuthMethod, AuthStartRequest, AuthState,
    Credential, CredentialError, CredentialKey, CredentialStore, LogoutRequest, SecretValue,
};
use ullage_core::{
    Capability, PartialFailure, Provider, ProviderDescriptor, ProviderError, ProviderId,
    ProviderResult, QueryOutcome, SubscriptionUsage, UsageQuery,
};
use zeroize::{Zeroize, Zeroizing};

pub use api::{ApiFailure, ApiFailureKind, CursorApi, ExchangeTokens, HttpCursorApi, SecretString};
pub use dto::{
    CreditGrantsBalance, CurrentPeriodUsage, CursorUsage, HardLimit, PlanInfo, PlanInfoResponse,
    PlanUsage, SpendLimitUsage,
};

const API_KEY_DASHBOARD: &str = "https://cursor.com/dashboard";
const LOGIN_DEEP_LINK: &str = "https://cursor.com/loginDeepControl";
/// How long a started browser sign-in stays pollable. Cursor does not date the
/// flow, so this is Ullage's own window.
const BROWSER_FLOW_LIFETIME_MINUTES: i64 = 10;
static FLOW_SEQUENCE: AtomicU64 = AtomicU64::new(1);

#[derive(Default)]
struct ProviderState {
    generation: u64,
    session_id: u64,
    pending_flow: Option<PendingFlow>,
    auth: Option<AuthMaterial>,
    invalid_reason: Option<String>,
    credentials_loaded: bool,
}

/// An authentication in progress. The API key variant waits for the user to
/// paste a key; the browser variant is polled until Cursor hands over a session.
enum PendingFlow {
    ApiKey {
        flow_id: String,
    },
    /// `verifier` is the secret half of the deep link's challenge and never
    /// leaves the daemon: only the challenge derived from it reaches the browser.
    Browser {
        flow_id: String,
        uuid: String,
        verifier: String,
        expires_at: DateTime<Utc>,
    },
}

impl PendingFlow {
    fn flow_id(&self) -> &str {
        match self {
            Self::ApiKey { flow_id } | Self::Browser { flow_id, .. } => flow_id,
        }
    }
}

struct AuthMaterial {
    /// Present only for an API key sign-in. A key never expires and can be
    /// re-exchanged for a fresh access token; a browser sign-in has no such
    /// credential, so its session stands until `expires_at` and is then redone.
    api_key: Option<Zeroizing<String>>,
    access_token: Zeroizing<String>,
    account_label: Option<String>,
    expires_at: Option<DateTime<Utc>>,
}

enum ExchangeFailureResolution {
    Shared {
        access_token: Zeroizing<String>,
        status: AuthState,
        generation: u64,
    },
    Failed(ProviderError),
}

pub struct CursorProvider {
    api: Arc<dyn CursorApi>,
    state: Mutex<ProviderState>,
    credentials: Option<(Arc<CredentialStore>, CredentialKey)>,
    credential_gate: tokio::sync::Mutex<()>,
}

impl CursorProvider {
    pub fn new() -> ProviderResult<Self> {
        Ok(Self::with_api(Arc::new(
            HttpCursorApi::new().map_err(ApiFailure::into_provider_error)?,
        )))
    }

    pub fn new_with_store(credentials: Arc<CredentialStore>) -> ProviderResult<Self> {
        Self::new_with_store_for_account(credentials, "active")
    }

    pub fn new_with_store_for_account(
        credentials: Arc<CredentialStore>,
        account_id: &str,
    ) -> ProviderResult<Self> {
        let api = Arc::new(HttpCursorApi::new().map_err(ApiFailure::into_provider_error)?);
        Self::with_api_and_store_for_account(api, credentials, account_id)
    }

    pub fn with_api(api: Arc<dyn CursorApi>) -> Self {
        Self {
            api,
            state: Mutex::new(ProviderState {
                credentials_loaded: true,
                ..ProviderState::default()
            }),
            credentials: None,
            credential_gate: tokio::sync::Mutex::new(()),
        }
    }

    pub fn with_api_and_store(
        api: Arc<dyn CursorApi>,
        credentials: Arc<CredentialStore>,
    ) -> ProviderResult<Self> {
        Self::with_api_and_store_for_account(api, credentials, "active")
    }

    pub fn with_api_and_store_for_account(
        api: Arc<dyn CursorApi>,
        credentials: Arc<CredentialStore>,
        account_id: &str,
    ) -> ProviderResult<Self> {
        let key = CredentialKey::new("cursor", account_id).map_err(credential_error)?;
        Ok(Self {
            api,
            state: Mutex::new(ProviderState::default()),
            credentials: Some((credentials, key)),
            credential_gate: tokio::sync::Mutex::new(()),
        })
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
                self.lock_state()?.credentials_loaded = true;
                return Ok(());
            }
            Err(error) => return Err(credential_error(error)),
        };
        let saved_label = stored
            .credential()
            .get("account_label")
            .map(|value| String::from_utf8(value.expose().to_vec()))
            .transpose()
            .map_err(|_| credential_error(CredentialError::CorruptCredential))?;
        let material = match stored.credential().get("api_key") {
            Some(_) => {
                let api_key = credential_string(stored.credential(), "api_key")?;
                let mut exchange = self
                    .api
                    .exchange_user_api_key(&Zeroizing::new(api_key.clone()))
                    .await
                    .map_err(ApiFailure::into_provider_error)?;
                let exchanged_label = exchange
                    .email
                    .as_ref()
                    .map(|email| email.trim().to_lowercase());
                if saved_label != exchanged_label {
                    return Err(ProviderError::ProtocolIncompatible {
                        message: "stored Cursor identity does not match the token exchange".into(),
                    });
                }
                let access_token = exchange.access_token.take();
                AuthMaterial {
                    expires_at: token_expiry(&access_token),
                    api_key: Some(Zeroizing::new(api_key)),
                    access_token,
                    account_label: saved_label,
                }
            }
            // A browser sign-in stores the session itself: there is nothing to
            // exchange, so the stored token is used until Cursor rejects it.
            None => {
                let access_token =
                    Zeroizing::new(credential_string(stored.credential(), "access_token")?);
                AuthMaterial {
                    expires_at: token_expiry(&access_token),
                    api_key: None,
                    access_token,
                    account_label: saved_label,
                }
            }
        };
        let mut state = self.lock_state()?;
        state.generation = state.generation.wrapping_add(1);
        state.session_id = state.session_id.wrapping_add(1);
        state.auth = Some(material);
        state.credentials_loaded = true;
        Ok(())
    }

    pub async fn refresh_auth(&self) -> ProviderResult<AuthState> {
        self.ensure_credentials_loaded().await?;
        let (api_key, generation, session_id) = {
            let state = self.lock_state()?;
            if state.pending_flow.is_some() {
                return Err(ProviderError::ProtocolIncompatible {
                    message: "Cursor authentication replacement is pending".into(),
                });
            }
            let auth = state
                .auth
                .as_ref()
                .ok_or_else(|| ProviderError::AuthenticationInvalid {
                    message: "Cursor is not authenticated".into(),
                })?;
            // A browser sign-in has nothing to spend on a refresh: Cursor issues
            // the session and its refresh token with the same expiry and offers
            // no renewal, so the only move left is signing in again.
            let Some(api_key) = auth.api_key.clone() else {
                return Ok(session_auth_state(auth));
            };
            (api_key, state.generation, state.session_id)
        };
        let exchange = match self.api.exchange_user_api_key(&api_key).await {
            Ok(exchange) => exchange,
            Err(error) => match self.resolve_exchange_failure(
                generation,
                Some(session_id),
                None,
                error,
                true,
            )? {
                ExchangeFailureResolution::Shared { status, .. } => return Ok(status),
                ExchangeFailureResolution::Failed(error) => return Err(error),
            },
        };
        match self.install_exchange(Some(api_key), exchange, generation, None) {
            Ok((status, _)) => Ok(status),
            Err(error) => self
                .refreshed_auth_since(generation, session_id)?
                .map(|(_, status, _)| status)
                .ok_or(error),
        }
    }

    async fn endpoint(
        &self,
        endpoint: Endpoint,
        expected_session_id: u64,
    ) -> Result<EndpointData, ApiFailure> {
        let (api_key, access_token, generation) = self
            .auth_tokens(expected_session_id)
            .map_err(provider_as_api_failure)?;
        let first = self.call_endpoint(endpoint, &access_token).await;
        self.ensure_session_current(expected_session_id)
            .map_err(provider_as_api_failure)?;
        if !matches!(
            first,
            Err(ApiFailure {
                kind: ApiFailureKind::Authentication,
                ..
            })
        ) {
            return first;
        }

        // Without an API key there is no second attempt to make, so record the
        // rejection as an invalid credential and let the user sign in again.
        let Some(api_key) = api_key else {
            let error = match first {
                Err(error) => error,
                Ok(_) => unreachable!("authentication failure was matched above"),
            };
            return match self
                .resolve_exchange_failure(generation, Some(expected_session_id), None, error, false)
                .map_err(provider_as_api_failure)?
            {
                ExchangeFailureResolution::Shared { .. } => {
                    unreachable!("a shared refresh was not allowed")
                }
                ExchangeFailureResolution::Failed(error) => Err(provider_as_api_failure(error)),
            };
        };

        let exchange = match self.api.exchange_user_api_key(&api_key).await {
            Ok(exchange) => exchange,
            Err(error) => match self
                .resolve_exchange_failure(generation, Some(expected_session_id), None, error, true)
                .map_err(provider_as_api_failure)?
            {
                ExchangeFailureResolution::Shared {
                    access_token,
                    generation,
                    ..
                } => {
                    let retried = self.call_endpoint(endpoint, &access_token).await;
                    return self.coordinate_retry_result(expected_session_id, generation, retried);
                }
                ExchangeFailureResolution::Failed(error) => {
                    return Err(provider_as_api_failure(error));
                }
            },
        };
        let refreshed_access_token =
            Zeroizing::new(exchange.access_token.expose_secret().to_owned());
        let (refreshed_access_token, refreshed_generation) =
            match self.install_exchange(Some(api_key), exchange, generation, None) {
                Ok((_, refreshed_generation)) => (refreshed_access_token, refreshed_generation),
                Err(error) => match self
                    .refreshed_auth_since(generation, expected_session_id)
                    .map_err(provider_as_api_failure)?
                {
                    Some((access_token, _, refreshed_generation)) => {
                        (access_token, refreshed_generation)
                    }
                    None => return Err(provider_as_api_failure(error)),
                },
            };
        let retried = self.call_endpoint(endpoint, &refreshed_access_token).await;
        self.coordinate_retry_result(expected_session_id, refreshed_generation, retried)
    }

    async fn call_endpoint(
        &self,
        endpoint: Endpoint,
        access_token: &str,
    ) -> Result<EndpointData, ApiFailure> {
        match endpoint {
            Endpoint::CurrentPeriod => self
                .api
                .current_period(access_token)
                .await
                .map(EndpointData::CurrentPeriod),
            Endpoint::PlanInfo => self
                .api
                .plan_info(access_token)
                .await
                .map(EndpointData::PlanInfo),
            Endpoint::CreditGrants => self
                .api
                .credit_grants(access_token)
                .await
                .map(EndpointData::CreditGrants),
            Endpoint::HardLimit => self
                .api
                .hard_limit(access_token)
                .await
                .map(EndpointData::HardLimit),
        }
    }

    /// One poll of a browser sign-in. Returns `Pending` until the user has
    /// approved in the browser, which is what lets a client poll this the way it
    /// polls a device-code flow.
    async fn complete_browser_auth(
        &self,
        flow_id: &str,
        uuid: &str,
        verifier: &str,
        expires_at: DateTime<Utc>,
        generation: u64,
    ) -> ProviderResult<AuthState> {
        if expires_at <= Utc::now() {
            let mut state = self.lock_state()?;
            if state.pending_flow.as_ref().map(PendingFlow::flow_id) == Some(flow_id) {
                state.pending_flow = None;
            }
            return Err(ProviderError::AuthenticationInvalid {
                message: "Cursor browser sign-in expired".into(),
            });
        }
        let polled = match self.api.poll_login(uuid, verifier).await {
            Ok(polled) => polled,
            Err(error) => {
                match self.resolve_exchange_failure(
                    generation,
                    None,
                    Some(flow_id),
                    error,
                    false,
                )? {
                    ExchangeFailureResolution::Failed(error) => return Err(error),
                    ExchangeFailureResolution::Shared { .. } => {
                        unreachable!("a shared refresh was not allowed")
                    }
                }
            }
        };
        let Some(exchange) = polled else {
            return Ok(AuthState::Pending {
                flow_id: flow_id.to_owned(),
                expires_at: Some(expires_at),
            });
        };
        self.install_exchange(None, exchange, generation, Some(flow_id))
            .map(|(status, _)| status)
    }

    fn install_exchange(
        &self,
        api_key: Option<Zeroizing<String>>,
        exchange: ExchangeTokens,
        expected_generation: u64,
        expected_flow: Option<&str>,
    ) -> ProviderResult<(AuthState, u64)> {
        let ExchangeTokens {
            mut access_token,
            refresh_token,
            email,
        } = exchange;
        let access_token = access_token.take();
        drop(refresh_token);
        let exchanged_account_label = email.map(|email| email.trim().to_lowercase());
        let mut state = self.lock_state()?;
        let expected_state_is_current = state.generation == expected_generation
            && match expected_flow {
                Some(flow_id) => {
                    state.pending_flow.as_ref().map(PendingFlow::flow_id) == Some(flow_id)
                }
                None => state.auth.is_some() && state.pending_flow.is_none(),
            };
        if !expected_state_is_current {
            return Err(ProviderError::ProtocolIncompatible {
                message: "Cursor authentication operation was superseded".into(),
            });
        }
        let account_label = if expected_flow.is_some() {
            exchanged_account_label
        } else {
            let existing_label = state
                .auth
                .as_ref()
                .and_then(|auth| auth.account_label.clone());
            if let (Some(existing), Some(exchanged)) = (
                existing_label.as_deref(),
                exchanged_account_label.as_deref(),
            ) {
                if existing != exchanged {
                    return Err(ProviderError::ProtocolIncompatible {
                        message: "Cursor token exchange returned a different account identity"
                            .into(),
                    });
                }
            }
            existing_label
        };
        if let Some((store, key)) = &self.credentials {
            let mut credential = Credential::new();
            match &api_key {
                Some(api_key) => credential
                    .insert("api_key", SecretValue::new(api_key.as_bytes()))
                    .map_err(credential_error)?,
                None => credential
                    .insert("access_token", SecretValue::new(access_token.as_bytes()))
                    .map_err(credential_error)?,
            };
            if let Some(label) = &account_label {
                credential
                    .insert("account_label", SecretValue::new(label.as_bytes()))
                    .map_err(credential_error)?;
            }
            store.set(key, credential).map_err(credential_error)?;
        }
        state.generation = state.generation.wrapping_add(1);
        if expected_flow.is_some() {
            state.session_id = state.session_id.wrapping_add(1);
        }
        state.pending_flow = None;
        state.invalid_reason = None;
        let expires_at = token_expiry(&access_token);
        state.auth = Some(AuthMaterial {
            api_key,
            access_token,
            account_label: account_label.clone(),
            expires_at,
        });
        Ok((
            AuthState::Authenticated {
                account_label,
                expires_at,
            },
            state.generation,
        ))
    }

    fn auth_tokens(
        &self,
        expected_session_id: u64,
    ) -> ProviderResult<(Option<Zeroizing<String>>, Zeroizing<String>, u64)> {
        let state = self.lock_state()?;
        if state.session_id != expected_session_id {
            return Err(ProviderError::ProtocolIncompatible {
                message: "Cursor authentication session changed during the operation".into(),
            });
        }
        let auth = state
            .auth
            .as_ref()
            .ok_or_else(|| ProviderError::AuthenticationInvalid {
                message: "Cursor is not authenticated".into(),
            })?;
        Ok((
            auth.api_key.clone(),
            auth.access_token.clone(),
            state.generation,
        ))
    }

    fn refreshed_auth_since(
        &self,
        expected_generation: u64,
        expected_session_id: u64,
    ) -> ProviderResult<Option<(Zeroizing<String>, AuthState, u64)>> {
        let state = self.lock_state()?;
        if state.session_id != expected_session_id
            || state.generation != expected_generation.wrapping_add(1)
            || state.pending_flow.is_some()
            || state.invalid_reason.is_some()
        {
            return Ok(None);
        }
        Ok(state.auth.as_ref().map(|auth| {
            (
                auth.access_token.clone(),
                session_auth_state(auth),
                state.generation,
            )
        }))
    }

    fn resolve_exchange_failure(
        &self,
        expected_generation: u64,
        expected_session_id: Option<u64>,
        expected_flow: Option<&str>,
        error: ApiFailure,
        allow_shared_refresh: bool,
    ) -> ProviderResult<ExchangeFailureResolution> {
        let mut state = self.lock_state()?;
        let session_is_current = expected_session_id
            .map(|session_id| state.session_id == session_id)
            .unwrap_or(true);
        let operation_is_current = state.generation == expected_generation
            && session_is_current
            && match expected_flow {
                Some(flow_id) => {
                    state.pending_flow.as_ref().map(PendingFlow::flow_id) == Some(flow_id)
                }
                None => state.auth.is_some() && state.pending_flow.is_none(),
            };

        if operation_is_current {
            if error.kind == ApiFailureKind::Authentication {
                state.generation = state.generation.wrapping_add(1);
                state.session_id = state.session_id.wrapping_add(1);
                state.pending_flow = None;
                state.auth = None;
                state.invalid_reason = Some(error.message.clone());
            }
            return Ok(ExchangeFailureResolution::Failed(
                error.into_provider_error(),
            ));
        }

        if allow_shared_refresh
            && session_is_current
            && state.generation == expected_generation.wrapping_add(1)
            && state.pending_flow.is_none()
            && state.invalid_reason.is_none()
        {
            if let Some(auth) = &state.auth {
                return Ok(ExchangeFailureResolution::Shared {
                    access_token: auth.access_token.clone(),
                    status: session_auth_state(auth),
                    generation: state.generation,
                });
            }
        }

        Ok(ExchangeFailureResolution::Failed(
            ProviderError::ProtocolIncompatible {
                message: "Cursor authentication operation was superseded".into(),
            },
        ))
    }

    fn coordinate_retry_result(
        &self,
        expected_session_id: u64,
        expected_generation: u64,
        result: Result<EndpointData, ApiFailure>,
    ) -> Result<EndpointData, ApiFailure> {
        if !matches!(
            result,
            Err(ApiFailure {
                kind: ApiFailureKind::Authentication,
                ..
            })
        ) {
            self.ensure_session_current(expected_session_id)
                .map_err(provider_as_api_failure)?;
            return result;
        }

        let mut state = self.lock_state().map_err(provider_as_api_failure)?;
        if state.session_id != expected_session_id || state.generation != expected_generation {
            return Err(ApiFailure::protocol(
                "Cursor authentication retry was superseded",
            ));
        }
        let error = match result {
            Err(error) => error,
            Ok(_) => unreachable!("authentication failure was matched above"),
        };
        state.generation = state.generation.wrapping_add(1);
        state.session_id = state.session_id.wrapping_add(1);
        state.pending_flow = None;
        state.auth = None;
        state.invalid_reason = Some(error.message.clone());
        Err(error)
    }

    fn query_identity(&self) -> ProviderResult<(u64, Option<String>)> {
        let state = self.lock_state()?;
        let auth = state
            .auth
            .as_ref()
            .ok_or_else(|| ProviderError::AuthenticationInvalid {
                message: "Cursor is not authenticated".into(),
            })?;
        Ok((state.session_id, auth.account_label.clone()))
    }

    fn session_is_current(&self, expected_session_id: u64) -> ProviderResult<bool> {
        Ok(self.lock_state()?.session_id == expected_session_id)
    }

    fn ensure_session_current(&self, expected_session_id: u64) -> ProviderResult<()> {
        if self.session_is_current(expected_session_id)? {
            Ok(())
        } else {
            Err(ProviderError::ProtocolIncompatible {
                message: "Cursor authentication session changed during the operation".into(),
            })
        }
    }

    fn lock_state(&self) -> ProviderResult<MutexGuard<'_, ProviderState>> {
        self.state
            .lock()
            .map_err(|_| ProviderError::ProtocolIncompatible {
                message: "Cursor authentication state is unavailable".into(),
            })
    }
}

impl Default for CursorProvider {
    fn default() -> Self {
        Self::new().expect("the default Cursor HTTP client should be constructible")
    }
}

#[derive(Clone, Copy)]
enum Endpoint {
    CurrentPeriod,
    PlanInfo,
    CreditGrants,
    HardLimit,
}

enum EndpointData {
    CurrentPeriod(CurrentPeriodUsage),
    PlanInfo(PlanInfoResponse),
    CreditGrants(CreditGrantsBalance),
    HardLimit(HardLimit),
}

#[async_trait]
impl Provider for CursorProvider {
    type VendorUsage = CursorUsage;

    fn descriptor(&self) -> ProviderDescriptor {
        ProviderDescriptor {
            id: ProviderId::new("cursor"),
            display_name: "Cursor".into(),
            capabilities: vec![
                Capability::Authentication,
                Capability::AuthenticationStatus,
                Capability::Logout,
                Capability::UsageQuery,
            ],
        }
    }

    async fn start_auth(&self, request: AuthStartRequest) -> ProviderResult<AuthChallenge> {
        let api_key_flow = match request.method {
            Some(AuthMethod::ApiToken) => true,
            // Cursor's browser sign-in is polled rather than redirected back, so
            // it is served for both browser and device-code requests.
            None | Some(AuthMethod::BrowserOAuth) | Some(AuthMethod::DeviceCode) => false,
            Some(method) => {
                return Err(ProviderError::UnsupportedCapability {
                    capability: format!("Cursor authentication method {method:?}"),
                });
            }
        };
        let _credential_gate = self.credential_gate.lock().await;

        let (pending, challenge) = if api_key_flow {
            let flow_id = new_flow_id("api-key");
            let challenge = AuthChallenge {
                flow_id: flow_id.clone(),
                method: AuthMethod::ApiToken,
                verification_uri: Some(API_KEY_DASHBOARD.into()),
                user_code: None,
                expires_at: None,
                input: Some(AuthInputRequest::secret("the Cursor API key")),
            };
            (PendingFlow::ApiKey { flow_id }, challenge)
        } else {
            let flow_id = new_flow_id("browser");
            let verifier = random_url_token()?;
            let uuid = random_uuid()?;
            let expires_at = Utc::now() + Duration::minutes(BROWSER_FLOW_LIFETIME_MINUTES);
            let challenge = AuthChallenge {
                flow_id: flow_id.clone(),
                // The browser never comes back to this process; the daemon polls
                // Cursor instead, which is the device-code shape clients expect.
                method: AuthMethod::DeviceCode,
                verification_uri: Some(login_deep_link(&uuid, &verifier)),
                user_code: None,
                expires_at: Some(expires_at),
                input: None,
            };
            (
                PendingFlow::Browser {
                    flow_id,
                    uuid,
                    verifier,
                    expires_at,
                },
                challenge,
            )
        };

        let mut state = self.lock_state()?;
        // Starting a replacement flow deliberately supersedes any persisted
        // credential without first exchanging that potentially invalid key.
        state.credentials_loaded = true;
        state.generation = state.generation.wrapping_add(1);
        state.session_id = state.session_id.wrapping_add(1);
        state.pending_flow = Some(pending);
        state.invalid_reason = None;
        Ok(challenge)
    }

    async fn complete_auth(&self, request: AuthCompleteRequest) -> ProviderResult<AuthState> {
        self.ensure_credentials_loaded().await?;
        let AuthCompleteRequest {
            flow_id,
            authorization_code,
            redirect_uri: _,
        } = request;
        let (generation, browser_flow) = {
            let state = self.lock_state()?;
            let pending = state
                .pending_flow
                .as_ref()
                .filter(|pending| pending.flow_id() == flow_id)
                .ok_or_else(|| ProviderError::ProtocolIncompatible {
                    message: "Cursor authentication flow ID is not active".into(),
                })?;
            let browser_flow = match pending {
                PendingFlow::ApiKey { .. } => None,
                PendingFlow::Browser {
                    uuid,
                    verifier,
                    expires_at,
                    ..
                } => Some((uuid.clone(), verifier.clone(), *expires_at)),
            };
            (state.generation, browser_flow)
        };
        if let Some((uuid, verifier, expires_at)) = browser_flow {
            return self
                .complete_browser_auth(&flow_id, &uuid, &verifier, expires_at, generation)
                .await;
        }
        let mut api_key = Zeroizing::new(authorization_code.unwrap_or_default());
        if api_key.trim().is_empty() {
            return Err(ProviderError::AuthenticationInvalid {
                message: "Cursor User API Key is required".into(),
            });
        }
        let trimmed_api_key = Zeroizing::new(api_key.trim().to_owned());
        api_key.zeroize();
        let exchange = match self.api.exchange_user_api_key(&trimmed_api_key).await {
            Ok(exchange) => exchange,
            Err(error) => match self.resolve_exchange_failure(
                generation,
                None,
                Some(&flow_id),
                error,
                false,
            )? {
                ExchangeFailureResolution::Failed(error) => return Err(error),
                ExchangeFailureResolution::Shared { .. } => {
                    unreachable!("authentication completion never shares a refresh")
                }
            },
        };
        self.install_exchange(Some(trimmed_api_key), exchange, generation, Some(&flow_id))
            .map(|(status, _)| status)
    }

    async fn auth_status(&self) -> ProviderResult<AuthState> {
        self.ensure_credentials_loaded().await?;
        let state = self.lock_state()?;
        if let Some(reason) = &state.invalid_reason {
            return Ok(AuthState::Invalid {
                reason: reason.clone(),
            });
        }
        if let Some(auth) = &state.auth {
            return Ok(session_auth_state(auth));
        }
        if let Some(pending) = &state.pending_flow {
            let expires_at = match pending {
                PendingFlow::ApiKey { .. } => None,
                PendingFlow::Browser { expires_at, .. } => Some(*expires_at),
            };
            return Ok(AuthState::Pending {
                flow_id: pending.flow_id().to_owned(),
                expires_at,
            });
        }
        Ok(AuthState::NotAuthenticated)
    }

    async fn logout(&self, request: LogoutRequest) -> ProviderResult<()> {
        if request.account_label.is_some() {
            self.ensure_credentials_loaded().await?;
        }
        let _credential_gate = self.credential_gate.lock().await;
        let mut state = self.lock_state()?;
        if request.account_label.as_ref().is_some_and(|requested| {
            state
                .auth
                .as_ref()
                .and_then(|auth| auth.account_label.as_ref())
                != Some(requested)
        }) {
            return Err(ProviderError::AuthenticationInvalid {
                message: "requested Cursor account is not authenticated".into(),
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
        state.auth = None;
        state.invalid_reason = None;
        Ok(())
    }

    async fn query(&self, _query: UsageQuery) -> ProviderResult<QueryOutcome<Self::VendorUsage>> {
        self.ensure_credentials_loaded().await?;
        let (session_id, account_label) = self.query_identity()?;
        let current_period = match self.endpoint(Endpoint::CurrentPeriod, session_id).await {
            Ok(EndpointData::CurrentPeriod(value)) => value,
            Ok(_) => unreachable!("endpoint response variant is fixed"),
            Err(error) => return Err(error.into_provider_error()),
        };

        let mut failures = Vec::new();
        if current_period.plan_usage.is_none() {
            failures.push(PartialFailure::from_error(
                "current_period.plan_usage",
                &ProviderError::ProtocolIncompatible {
                    message: String::new(),
                },
            ));
        }

        let plan = optional_endpoint(
            self.endpoint(Endpoint::PlanInfo, session_id).await,
            "plan_info",
            &mut failures,
        )?
        .map(|data| match data {
            EndpointData::PlanInfo(value) => value,
            _ => unreachable!("endpoint response variant is fixed"),
        });
        let credit_grants = optional_endpoint(
            self.endpoint(Endpoint::CreditGrants, session_id).await,
            "credit_grants",
            &mut failures,
        )?
        .map(|data| match data {
            EndpointData::CreditGrants(value) => value,
            _ => unreachable!("endpoint response variant is fixed"),
        });
        let hard_limit = optional_endpoint(
            self.endpoint(Endpoint::HardLimit, session_id).await,
            "hard_limit",
            &mut failures,
        )?
        .map(|data| match data {
            EndpointData::HardLimit(value) => value,
            _ => unreachable!("endpoint response variant is fixed"),
        });

        let data = CursorUsage {
            account_label,
            current_period,
            plan,
            credit_grants,
            hard_limit,
            observed_at: Utc::now(),
        };
        if !self.session_is_current(session_id)? {
            return Err(ProviderError::ProtocolIncompatible {
                message: "Cursor authentication session changed during the query".into(),
            });
        }
        if failures.is_empty() {
            Ok(QueryOutcome::Complete { data })
        } else {
            Ok(QueryOutcome::Partial { data, failures })
        }
    }

    fn normalize(&self, vendor_usage: Self::VendorUsage) -> ProviderResult<SubscriptionUsage> {
        dto::normalize(vendor_usage)
    }
}

/// The state a stored session reports. A browser sign-in that has run out its
/// expiry is Invalid rather than Authenticated: nothing can renew it, so saying
/// so is what tells the user to sign in again.
fn session_auth_state(auth: &AuthMaterial) -> AuthState {
    if auth.api_key.is_none() && auth.expires_at.is_some_and(|expiry| expiry <= Utc::now()) {
        return AuthState::Invalid {
            reason: "the Cursor browser sign-in expired".into(),
        };
    }
    AuthState::Authenticated {
        account_label: auth.account_label.clone(),
        expires_at: auth.expires_at,
    }
}

fn new_flow_id(kind: &str) -> String {
    format!(
        "cursor-{kind}-{}-{}",
        Utc::now().timestamp_millis(),
        FLOW_SEQUENCE.fetch_add(1, Ordering::Relaxed)
    )
}

/// Both values are URL-safe by construction: the challenge is base64url of a
/// digest and the UUID is hex with dashes, so neither needs escaping.
fn login_deep_link(uuid: &str, verifier: &str) -> String {
    let challenge = URL_SAFE_NO_PAD.encode(Sha256::digest(verifier.as_bytes()));
    format!("{LOGIN_DEEP_LINK}?challenge={challenge}&uuid={uuid}&mode=login&redirectTarget=cli")
}

/// Reads the `exp` claim out of a Cursor session token. Cursor issues these with
/// a fixed lifetime and no way to renew one, so this is what tells a client when
/// signing in again becomes necessary.
fn token_expiry(access_token: &str) -> Option<DateTime<Utc>> {
    let payload = access_token.split('.').nth(1)?;
    let decoded = URL_SAFE_NO_PAD.decode(payload).ok()?;
    let claims: serde_json::Value = serde_json::from_slice(&decoded).ok()?;
    DateTime::from_timestamp(claims.get("exp")?.as_i64()?, 0)
}

fn random_url_token() -> ProviderResult<String> {
    let mut bytes = [0_u8; 32];
    getrandom::fill(&mut bytes).map_err(|_| ProviderError::Network {
        message: "operating system randomness is unavailable".into(),
    })?;
    Ok(URL_SAFE_NO_PAD.encode(bytes))
}

fn random_uuid() -> ProviderResult<String> {
    let mut bytes = [0_u8; 16];
    getrandom::fill(&mut bytes).map_err(|_| ProviderError::Network {
        message: "operating system randomness is unavailable".into(),
    })?;
    bytes[6] = (bytes[6] & 0x0f) | 0x40;
    bytes[8] = (bytes[8] & 0x3f) | 0x80;
    let hex: String = bytes.iter().map(|byte| format!("{byte:02x}")).collect();
    Ok(format!(
        "{}-{}-{}-{}-{}",
        &hex[0..8],
        &hex[8..12],
        &hex[12..16],
        &hex[16..20],
        &hex[20..32]
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
        message: error.provider_message("Cursor credential storage is unavailable or invalid"),
    }
}

fn optional_endpoint(
    result: Result<EndpointData, ApiFailure>,
    scope: &str,
    failures: &mut Vec<PartialFailure>,
) -> ProviderResult<Option<EndpointData>> {
    match result {
        Ok(value) => Ok(Some(value)),
        Err(error) if error.kind == ApiFailureKind::Authentication => {
            Err(error.into_provider_error())
        }
        Err(error) => {
            let provider_error = error.clone().into_provider_error();
            failures.push(PartialFailure::from_error(
                format!("{scope}.{}", error.scope_suffix()),
                &provider_error,
            ));
            Ok(None)
        }
    }
}

fn provider_as_api_failure(error: ProviderError) -> ApiFailure {
    match error {
        ProviderError::AuthenticationInvalid { message } => ApiFailure::authentication(message),
        ProviderError::RateLimited {
            message,
            retry_after_seconds,
        } => ApiFailure {
            kind: ApiFailureKind::RateLimit,
            message,
            retry_after_seconds,
        },
        ProviderError::Network { message } => ApiFailure {
            kind: ApiFailureKind::Network,
            message,
            retry_after_seconds: None,
        },
        ProviderError::ProtocolIncompatible { message } => ApiFailure::protocol(message),
        ProviderError::UnsupportedCapability { capability } => {
            ApiFailure::protocol(format!("unsupported capability: {capability}"))
        }
    }
}
