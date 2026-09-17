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
    Credential, CredentialError, CredentialKey, CredentialStore, CredentialVersion, LogoutRequest,
    ReplaceOutcome, SecretValue, account_identity,
};
use ullage_core::{
    Capability, PartialFailure, Provider, ProviderDescriptor, ProviderError, ProviderId,
    ProviderResult, QueryOutcome, SubscriptionUsage, UsageQuery,
};
use zeroize::Zeroizing;

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
    /// Identity of the account whose credential was rejected. Rejection drops
    /// the auth material, but the account it belonged to is still the account a
    /// fresh sign-in as that identity supersedes.
    invalid_account_key: Option<String>,
    /// Store version `auth` was observed at; required for CAS updates.
    stored_version: Option<CredentialVersion>,
    /// A stored record that will not decode is corrupt, not absent: the flag
    /// keeps `auth_status` able to report Invalid and logout able to delete
    /// the record even though no account can be read out of it.
    stored_credential_corrupt: bool,
    credentials_loaded: bool,
    /// Ticked when a provider exchange starts under `exchange_gate` and
    /// again when its outcome is published; comparing it against a snapshot
    /// only establishes ordering, not whether the caller was queued.
    exchange_epoch: u64,
    /// The most recent failed exchange, shared with callers whose auth
    /// snapshot predates it so a failed refresh stays a single provider
    /// call.
    exchange_verdict: Option<ExchangeVerdict>,
}

/// The outcome of a provider exchange attempted on an auth snapshot. Only
/// failures are stored: success is observable through `generation`
/// advancing, while a failure either leaves the snapshot unchanged (a
/// transient error) or invalidates it (an authentication error that bumps
/// generation and session); both kinds are shared through this verdict.
struct ExchangeVerdict {
    /// (generation, session_id) the failed exchange was attempted on.
    observed_generation: u64,
    observed_session_id: u64,
    /// `exchange_epoch` tick at which the failure was published; a snapshot
    /// taken before this verdict compares older than it.
    epoch: u64,
    failure: ProviderError,
}

/// An authentication in progress. The API key variant waits for the user to
/// paste a key; the browser variant is polled until Cursor hands over a session.
/// Both expire: an abandoned flow must not wedge re-authentication forever.
enum PendingFlow {
    ApiKey {
        flow_id: String,
        expires_at: DateTime<Utc>,
    },
    /// `verifier` is the secret half of the deep link's challenge. It stays out
    /// of the browser URL — only the derived challenge is linked — but it is
    /// sent back to `api2.cursor.sh` by `poll_login` when completing the flow.
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
            Self::ApiKey { flow_id, .. } | Self::Browser { flow_id, .. } => flow_id,
        }
    }

    fn expires_at(&self) -> DateTime<Utc> {
        match self {
            Self::ApiKey { expires_at, .. } | Self::Browser { expires_at, .. } => *expires_at,
        }
    }
}

impl ProviderState {
    /// Releases a pending flow whose lifetime ran out, returning whether one
    /// expired. Called at every entry point that `pending_flow` would
    /// otherwise block, so an abandoned flow cannot wedge refresh,
    /// re-exchange, or re-authentication forever.
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

struct AuthMaterial {
    /// Present only for an API key sign-in. A key never expires and can be
    /// re-exchanged for a fresh access token; a browser sign-in has no such
    /// credential, so its session stands until `expires_at` and is then redone.
    api_key: Option<Zeroizing<String>>,
    access_token: Zeroizing<String>,
    account_label: Option<String>,
    /// Identity of the signed-in account, which unlike the label above the
    /// browser sign-in can produce: its tokens carry no email.
    account_key: Option<String>,
    expires_at: Option<DateTime<Utc>>,
}

/// A consistent snapshot of the live session's exchange inputs, taken
/// before waiting on `exchange_gate`.
struct AuthSnapshot {
    api_key: Option<Zeroizing<String>>,
    access_token: Zeroizing<String>,
    generation: u64,
    epoch: u64,
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
    /// Serializes the provider token exchange across `refresh_auth` and the
    /// query retry path so concurrent refreshes share one provider call.
    /// Sign-in flows intentionally do not take it: they must still be able
    /// to supersede an in-flight refresh.
    exchange_gate: tokio::sync::Mutex<()>,
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
            exchange_gate: tokio::sync::Mutex::new(()),
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
            exchange_gate: tokio::sync::Mutex::new(()),
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
                let mut state = self.lock_state()?;
                state.stored_version = None;
                state.credentials_loaded = true;
                return Ok(());
            }
            Err(error) => return Err(credential_error(error)),
        };
        let stored_version = stored.version();
        enum Parsed {
            Material(AuthMaterial),
            /// The record exists but no field combination decodes into usable
            /// auth material. Surfaced as Invalid so sign-in can repair it
            /// and logout can delete it.
            Corrupt,
            /// The stored API key was rejected; the record is still that
            /// account's, so Invalid names the identity for the repair path.
            Rejected {
                message: String,
                label: Option<String>,
            },
        }
        let parsed: Parsed = async {
            let saved_label = match stored
                .credential()
                .get("account_label")
                .map(|value| String::from_utf8(value.expose().to_vec()))
                .transpose()
            {
                Ok(saved_label) => saved_label,
                Err(_) => return Ok(Parsed::Corrupt),
            };
            let material = match stored.credential().get("api_key") {
                Some(_) => {
                    let api_key = match credential_string(stored.credential(), "api_key") {
                        Ok(api_key) => api_key,
                        Err(_) => return Ok(Parsed::Corrupt),
                    };
                    let mut exchange = match self
                        .api
                        .exchange_user_api_key(&Zeroizing::new(api_key.clone()))
                        .await
                    {
                        Ok(exchange) => exchange,
                        Err(error) if error.kind == ApiFailureKind::Authentication => {
                            return Ok(Parsed::Rejected {
                                message: error.message,
                                label: saved_label,
                            });
                        }
                        Err(error) => return Err(ApiFailure::into_provider_error(error)),
                    };
                    let exchanged_label = exchange
                        .email
                        .as_ref()
                        .map(|email| email.trim().to_lowercase());
                    // An absent side cannot contradict the other: only two
                    // known labels that disagree prove an identity swap.
                    if let (Some(saved), Some(exchanged)) =
                        (saved_label.as_deref(), exchanged_label.as_deref())
                    {
                        if saved != exchanged {
                            return Err(ProviderError::ProtocolIncompatible {
                                message: "stored Cursor identity does not match the token exchange"
                                    .into(),
                            });
                        }
                    }
                    let access_token = exchange.access_token.take();
                    AuthMaterial {
                        expires_at: token_expiry(&access_token),
                        account_key: token_subject(&access_token, saved_label.as_deref()),
                        api_key: Some(Zeroizing::new(api_key)),
                        access_token,
                        account_label: saved_label,
                    }
                }
                // A browser sign-in stores the session itself: there is
                // nothing to exchange, so the stored token is used until
                // Cursor rejects it.
                None => match credential_string(stored.credential(), "access_token") {
                    Ok(access_token) => {
                        let access_token = Zeroizing::new(access_token);
                        AuthMaterial {
                            expires_at: token_expiry(&access_token),
                            account_key: token_subject(&access_token, saved_label.as_deref()),
                            api_key: None,
                            access_token,
                            account_label: saved_label,
                        }
                    }
                    Err(_) => return Ok(Parsed::Corrupt),
                },
            };
            Ok(Parsed::Material(material))
        }
        .await?;
        let material = match parsed {
            Parsed::Material(material) => material,
            Parsed::Rejected { message, label } => {
                // A stored key the server refuses is dead, but it is still
                // that account's record: report Invalid so a fresh sign-in
                // can replace it instead of surfacing a bare error.
                let mut state = self.lock_state()?;
                state.generation = state.generation.wrapping_add(1);
                state.session_id = state.session_id.wrapping_add(1);
                state.stored_version = Some(stored_version);
                state.invalid_account_key = label;
                state.invalid_reason = Some(message);
                state.credentials_loaded = true;
                return Ok(());
            }
            Parsed::Corrupt => {
                let mut state = self.lock_state()?;
                state.stored_version = Some(stored_version);
                state.stored_credential_corrupt = true;
                state.credentials_loaded = true;
                return Ok(());
            }
        };
        let mut state = self.lock_state()?;
        state.generation = state.generation.wrapping_add(1);
        state.session_id = state.session_id.wrapping_add(1);
        state.auth = Some(material);
        state.stored_version = Some(stored_version);
        state.credentials_loaded = true;
        Ok(())
    }

    pub async fn refresh_auth(&self) -> ProviderResult<AuthState> {
        self.ensure_credentials_loaded().await?;
        let (api_key, generation, session_id, epoch) = {
            let mut state = self.lock_state()?;
            state.expire_pending_flow();
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
            (
                api_key,
                state.generation,
                state.session_id,
                state.exchange_epoch,
            )
        };
        // The gate makes the provider exchange single-flight per account: a
        // refresh that completed while this call waited is shared instead of
        // repeated, so concurrent callers never trigger two exchanges.
        let _gate = self.exchange_gate.lock().await;
        if let Some((_, status, _)) = self.refreshed_auth_since(generation, session_id)? {
            return Ok(status);
        }
        if let Some(failure) = self.failed_exchange_since(generation, session_id, epoch)? {
            return Err(failure);
        }
        if !self.operation_is_current(generation, session_id)? {
            return Err(ProviderError::ProtocolIncompatible {
                message: "Cursor authentication operation was superseded".into(),
            });
        }
        self.begin_exchange()?;
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
                ExchangeFailureResolution::Failed(error) => {
                    self.record_exchange_failure(generation, session_id, &error)?;
                    return Err(error);
                }
            },
        };
        match self
            .install_exchange(Some(api_key), exchange, generation, None)
            .await
        {
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
        let snapshot = self
            .auth_tokens(expected_session_id)
            .map_err(provider_as_api_failure)?;
        let generation = snapshot.generation;
        let first = self.call_endpoint(endpoint, &snapshot.access_token).await;
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
        let Some(api_key) = snapshot.api_key.clone() else {
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

        // The gate makes the provider exchange single-flight per account: a
        // refresh that landed while this retry waited is reused instead of
        // repeated.
        let gate = self.exchange_gate.lock().await;
        if let Some((access_token, _, refreshed_generation)) = self
            .refreshed_auth_since(generation, expected_session_id)
            .map_err(provider_as_api_failure)?
        {
            drop(gate);
            let retried = self.call_endpoint(endpoint, &access_token).await;
            return self.coordinate_retry_result(
                expected_session_id,
                refreshed_generation,
                retried,
            );
        }
        if let Some(failure) = self
            .failed_exchange_since(generation, expected_session_id, snapshot.epoch)
            .map_err(provider_as_api_failure)?
        {
            drop(gate);
            return Err(provider_as_api_failure(failure));
        }
        if !self
            .operation_is_current(generation, expected_session_id)
            .map_err(provider_as_api_failure)?
        {
            drop(gate);
            return Err(ApiFailure::protocol(
                "Cursor authentication retry was superseded",
            ));
        }
        self.begin_exchange().map_err(provider_as_api_failure)?;
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
                    drop(gate);
                    let retried = self.call_endpoint(endpoint, &access_token).await;
                    return self.coordinate_retry_result(expected_session_id, generation, retried);
                }
                ExchangeFailureResolution::Failed(error) => {
                    self.record_exchange_failure(generation, expected_session_id, &error)
                        .map_err(provider_as_api_failure)?;
                    return Err(provider_as_api_failure(error));
                }
            },
        };
        let refreshed_access_token =
            Zeroizing::new(exchange.access_token.expose_secret().to_owned());
        let (refreshed_access_token, refreshed_generation) = match self
            .install_exchange(Some(api_key), exchange, generation, None)
            .await
        {
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
        drop(gate);
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
            // A poll that started before another one installed the session
            // would otherwise report this flow as still pending after it has
            // already finished.
            let state = self.lock_state()?;
            if state.generation != generation
                || state.pending_flow.as_ref().map(PendingFlow::flow_id) != Some(flow_id)
            {
                return Err(ProviderError::ProtocolIncompatible {
                    message: "Cursor authentication operation was superseded".into(),
                });
            }
            return Ok(AuthState::Pending {
                flow_id: flow_id.to_owned(),
                expires_at: Some(expires_at),
            });
        };
        self.install_exchange(None, exchange, generation, Some(flow_id))
            .await
            .map(|(status, _)| status)
    }

    async fn install_exchange(
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
        // The refresh token goes unused on purpose: Cursor issues session and
        // refresh with the same expiry and offers no renewal endpoint, so the
        // only credential that can re-exchange is the API key.
        drop(refresh_token);
        let exchanged_account_label = email.map(|email| email.trim().to_lowercase());
        let account_label = {
            let state = self.lock_state()?;
            if !Self::expected_state_is_current(&state, expected_generation, expected_flow) {
                return Err(ProviderError::ProtocolIncompatible {
                    message: "Cursor authentication operation was superseded".into(),
                });
            }
            if expected_flow.is_some() {
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
            }
        };
        // The persist gate serializes the store write and the state commit so
        // a racing sign-in or logout cannot interleave between them. Store I/O
        // itself stays outside the state lock.
        let _persist_gate = self.credential_gate.lock().await;
        {
            let state = self.lock_state()?;
            if !Self::expected_state_is_current(&state, expected_generation, expected_flow) {
                return Err(ProviderError::ProtocolIncompatible {
                    message: "Cursor authentication operation was superseded".into(),
                });
            }
        }
        let stored_version = if let Some((store, key)) = &self.credentials {
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
            if expected_flow.is_some() {
                // A completed sign-in supersedes whatever the store holds.
                Some(
                    store
                        .set(key, credential)
                        .map_err(credential_error)?
                        .version(),
                )
            } else {
                // A refresh updates the record it observed; a concurrent write
                // (logout, another sign-in) makes the rotation stale.
                let expected = self.lock_state()?.stored_version.ok_or_else(|| {
                    ProviderError::ProtocolIncompatible {
                        message: "Cursor credential has no observed store version".into(),
                    }
                })?;
                match store.replace(key, expected, credential) {
                    Ok(ReplaceOutcome::Replaced(stored)) => Some(stored.version()),
                    Ok(ReplaceOutcome::VersionConflict) | Err(CredentialError::NotFound) => {
                        return Err(ProviderError::ProtocolIncompatible {
                            message: "stored Cursor credential changed during the operation".into(),
                        });
                    }
                    Err(error) => return Err(credential_error(error)),
                }
            }
        } else {
            None
        };
        let mut state = self.lock_state()?;
        if !Self::expected_state_is_current(&state, expected_generation, expected_flow) {
            return Err(ProviderError::ProtocolIncompatible {
                message: "Cursor authentication operation was superseded".into(),
            });
        }
        state.stored_version = stored_version;
        state.generation = state.generation.wrapping_add(1);
        if expected_flow.is_some() {
            state.session_id = state.session_id.wrapping_add(1);
        }
        state.pending_flow = None;
        state.invalid_reason = None;
        state.invalid_account_key = None;
        state.stored_credential_corrupt = false;
        let expires_at = token_expiry(&access_token);
        let account_key = token_subject(&access_token, account_label.as_deref());
        state.auth = Some(AuthMaterial {
            api_key,
            access_token,
            account_label: account_label.clone(),
            account_key: account_key.clone(),
            expires_at,
        });
        Ok((
            AuthState::Authenticated {
                account_key: account_key.as_deref().and_then(account_identity),
                account_label,
                expires_at,
            },
            state.generation,
        ))
    }

    fn expected_state_is_current(
        state: &ProviderState,
        expected_generation: u64,
        expected_flow: Option<&str>,
    ) -> bool {
        state.generation == expected_generation
            && match expected_flow {
                Some(flow_id) => {
                    state.pending_flow.as_ref().map(PendingFlow::flow_id) == Some(flow_id)
                }
                None => state.auth.is_some() && state.pending_flow.is_none(),
            }
    }

    fn auth_tokens(&self, expected_session_id: u64) -> ProviderResult<AuthSnapshot> {
        let mut state = self.lock_state()?;
        state.expire_pending_flow();
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
        Ok(AuthSnapshot {
            api_key: auth.api_key.clone(),
            access_token: auth.access_token.clone(),
            generation: state.generation,
            epoch: state.exchange_epoch,
        })
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
                if expected_flow.is_some() {
                    // The failure belongs to the pending flow alone: clearing
                    // `state.auth` or setting `invalid_reason` here would
                    // downgrade a healthy session behind it.
                    state.pending_flow = None;
                    if state.auth.is_none() {
                        state.invalid_reason = Some(error.message.clone());
                    }
                } else {
                    state.generation = state.generation.wrapping_add(1);
                    state.session_id = state.session_id.wrapping_add(1);
                    state.pending_flow = None;
                    state.invalid_account_key = state.auth.take().and_then(|auth| auth.account_key);
                    state.invalid_reason = Some(error.message.clone());
                }
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
        state.invalid_account_key = state.auth.take().and_then(|auth| auth.account_key);
        state.invalid_reason = Some(error.message.clone());
        Err(error)
    }

    /// Marks the start of a provider exchange attempt under `exchange_gate`.
    fn begin_exchange(&self) -> ProviderResult<u64> {
        let mut state = self.lock_state()?;
        state.exchange_epoch = state.exchange_epoch.wrapping_add(1);
        Ok(state.exchange_epoch)
    }

    /// Shares the failed exchange another caller completed for the same
    /// observed auth state. `observed_epoch` only orders the snapshot
    /// against the verdict: an older value means the snapshot predates the
    /// published failure, while an equal value means the caller arrived
    /// afterwards and becomes the next leader instead of inheriting a stale
    /// failure forever.
    fn failed_exchange_since(
        &self,
        expected_generation: u64,
        expected_session_id: u64,
        observed_epoch: u64,
    ) -> ProviderResult<Option<ProviderError>> {
        let state = self.lock_state()?;
        Ok(match &state.exchange_verdict {
            Some(verdict)
                if verdict.observed_generation == expected_generation
                    && verdict.observed_session_id == expected_session_id
                    && verdict.epoch > observed_epoch =>
            {
                Some(verdict.failure.clone())
            }
            _ => None,
        })
    }

    /// Publishes the failure an exchange attempt produced so callers that
    /// observed the same auth snapshot before it was published receive it
    /// instead of calling the provider. The epoch is ticked again here so a
    /// snapshot taken while the attempt was in flight compares older than
    /// the verdict, while one taken after compares equal.
    fn record_exchange_failure(
        &self,
        expected_generation: u64,
        expected_session_id: u64,
        failure: &ProviderError,
    ) -> ProviderResult<()> {
        let mut state = self.lock_state()?;
        state.exchange_epoch = state.exchange_epoch.wrapping_add(1);
        state.exchange_verdict = Some(ExchangeVerdict {
            observed_generation: expected_generation,
            observed_session_id: expected_session_id,
            epoch: state.exchange_epoch,
            failure: failure.clone(),
        });
        Ok(())
    }

    /// True while the snapshot taken before waiting on `exchange_gate`
    /// still names the live session; anything else means the operation was
    /// superseded by a refresh, sign-in, or invalidation.
    fn operation_is_current(
        &self,
        expected_generation: u64,
        expected_session_id: u64,
    ) -> ProviderResult<bool> {
        let state = self.lock_state()?;
        Ok(state.generation == expected_generation
            && state.session_id == expected_session_id
            && state.pending_flow.is_none()
            && state.auth.is_some())
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
            let flow_id = new_flow_id("api-key")?;
            // An API-key flow waits for user input, so it gets the same
            // lifetime as the browser flow: abandoning it must not wedge the
            // pending slot forever.
            let expires_at = Utc::now() + Duration::minutes(BROWSER_FLOW_LIFETIME_MINUTES);
            let challenge = AuthChallenge {
                flow_id: flow_id.clone(),
                method: AuthMethod::ApiToken,
                verification_uri: Some(API_KEY_DASHBOARD.into()),
                user_code: None,
                expires_at: Some(expires_at),
                input: Some(AuthInputRequest::secret("the Cursor API key")),
            };
            (
                PendingFlow::ApiKey {
                    flow_id,
                    expires_at,
                },
                challenge,
            )
        } else {
            let flow_id = new_flow_id("browser")?;
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
        // The identity outlives the replacement flow. Clearing it here would
        // lose it for as long as that flow is pending, and an abandoned flow is
        // pending until something else ends it.
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
            let mut state = self.lock_state()?;
            let pending = state
                .pending_flow
                .as_ref()
                .filter(|pending| pending.flow_id() == flow_id)
                .ok_or_else(|| ProviderError::ProtocolIncompatible {
                    message: "Cursor authentication flow ID is not active".into(),
                })?;
            let (api_key_expires_at, browser_flow) = match pending {
                PendingFlow::ApiKey { expires_at, .. } => (Some(*expires_at), None),
                PendingFlow::Browser {
                    uuid,
                    verifier,
                    expires_at,
                    ..
                } => (None, Some((uuid.clone(), verifier.clone(), *expires_at))),
            };
            if api_key_expires_at.is_some_and(|expires_at| expires_at <= Utc::now()) {
                // The abandoned flow must release the pending slot.
                state.pending_flow = None;
                return Err(ProviderError::AuthenticationInvalid {
                    message: "Cursor API key sign-in expired".into(),
                });
            }
            (state.generation, browser_flow)
        };
        if let Some((uuid, verifier, expires_at)) = browser_flow {
            return self
                .complete_browser_auth(&flow_id, &uuid, &verifier, expires_at, generation)
                .await;
        }
        // `authorization_code` arrives as protocol plaintext; from here on the
        // only held copies are Zeroizing.
        let api_key = Zeroizing::new(authorization_code.unwrap_or_default());
        if api_key.trim().is_empty() {
            return Err(ProviderError::AuthenticationInvalid {
                message: "Cursor User API Key is required".into(),
            });
        }
        let trimmed_api_key = Zeroizing::new(api_key.trim().to_owned());
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
            .await
            .map(|(status, _)| status)
    }

    async fn auth_status(&self) -> ProviderResult<AuthState> {
        self.ensure_credentials_loaded().await?;
        let mut state = self.lock_state()?;
        // A flow nobody finished would stay pending forever otherwise, and a
        // pending flow names no account, so the identity behind it would stay
        // invisible to anything comparing accounts.
        if state.expire_pending_flow()
            && state.auth.is_none()
            && state.invalid_account_key.is_some()
        {
            state.invalid_reason = Some("the Cursor sign-in was not completed".into());
        }
        let state = state;
        if let Some(reason) = &state.invalid_reason {
            return Ok(AuthState::Invalid {
                reason: reason.clone(),
                account_key: state
                    .invalid_account_key
                    .as_deref()
                    .and_then(account_identity),
            });
        }
        if let Some(auth) = &state.auth {
            return Ok(session_auth_state(auth));
        }
        if let Some(pending) = &state.pending_flow {
            return Ok(AuthState::Pending {
                flow_id: pending.flow_id().to_owned(),
                expires_at: Some(pending.expires_at()),
            });
        }
        if state.stored_credential_corrupt {
            return Ok(AuthState::Invalid {
                reason: "the stored Cursor credential is unreadable; sign in again".into(),
                // The corrupt record cannot be decoded, so it names no account.
                account_key: None,
            });
        }
        Ok(AuthState::NotAuthenticated)
    }

    async fn logout(&self, request: LogoutRequest) -> ProviderResult<()> {
        // Cursor exposes no documented token-revocation endpoint, so logout is
        // local-only: the stored credential is deleted and the session's own
        // expiry bounds any residual server-side validity.
        if request.account_label.is_some() {
            self.ensure_credentials_loaded().await?;
        }
        let _credential_gate = self.credential_gate.lock().await;
        let mut state = self.lock_state()?;
        // A corrupt record names no account, so the label guard cannot verify
        // it; deleting it is the recovery path.
        let unverifiable = state.stored_credential_corrupt && state.auth.is_none();
        if !unverifiable
            && request.account_label.as_ref().is_some_and(|requested| {
                state
                    .auth
                    .as_ref()
                    .and_then(|auth| auth.account_label.as_ref())
                    != Some(requested)
            })
        {
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
        state.stored_version = None;
        state.invalid_reason = None;
        state.invalid_account_key = None;
        state.stored_credential_corrupt = false;
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
        for field in &current_period.malformed_fields {
            let scope = match field.as_str() {
                "billingCycleStart" => "billing_cycle_start",
                "billingCycleEnd" => "billing_cycle_end",
                "planUsage" => "plan_usage",
                "spendLimitUsage" => "spend_limit_usage",
                "autoBucketModels" => "auto_bucket_models",
                other => other,
            };
            failures.push(PartialFailure::from_error(
                format!("current_period.{scope}"),
                &ProviderError::ProtocolIncompatible {
                    message: String::new(),
                },
            ));
        }
        if current_period.plan_usage.is_none()
            && !current_period
                .malformed_fields
                .iter()
                .any(|field| field == "planUsage")
        {
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
            // The session expired, but it is still this account's session, so a
            // fresh sign-in as the same identity supersedes it.
            account_key: auth.account_key.as_deref().and_then(account_identity),
        };
    }
    AuthState::Authenticated {
        account_label: auth.account_label.clone(),
        account_key: auth.account_key.as_deref().and_then(account_identity),
        expires_at: auth.expires_at,
    }
}

fn new_flow_id(kind: &str) -> ProviderResult<String> {
    // A timestamp plus sequence is guessable, so the id carries 256 bits of
    // randomness: it is the bearer for completing a pending flow.
    Ok(format!(
        "cursor-{kind}-{}-{}",
        FLOW_SEQUENCE.fetch_add(1, Ordering::Relaxed),
        random_url_token()?
    ))
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
    DateTime::from_timestamp(token_claim(access_token, "exp")?.as_i64()?, 0)
}

/// Identity of the signed-in account, taken from the session token's `sub`
/// claim. The browser sign-in returns tokens without an email, so the exchanged
/// address cannot be the identity; `sub` is issued for both paths, which also
/// lets an API key account and a browser account of one person match. The email
/// remains the fallback for a token that carries no subject.
fn token_subject(access_token: &str, email: Option<&str>) -> Option<String> {
    token_claim(access_token, "sub")
        .and_then(|claim| claim.as_str().map(str::to_owned))
        .or_else(|| email.map(str::to_owned))
        .map(|value| value.trim().to_owned())
        .filter(|value| !value.is_empty())
}

fn token_claim(access_token: &str, name: &str) -> Option<serde_json::Value> {
    let payload = access_token.split('.').nth(1)?;
    let decoded = URL_SAFE_NO_PAD.decode(payload).ok()?;
    let claims: serde_json::Value = serde_json::from_slice(&decoded).ok()?;
    claims.get(name).cloned()
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
    let mut hex = String::with_capacity(bytes.len() * 2);
    for byte in bytes {
        use std::fmt::Write as _;
        let _ = write!(hex, "{byte:02x}");
    }
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

#[cfg(test)]
mod tests {
    use std::collections::VecDeque;
    use std::future::Future;
    use std::task::{Context, Poll, Waker};

    use super::*;

    struct StubApi {
        exchanges: Mutex<VecDeque<Result<ExchangeTokens, ApiFailure>>>,
    }

    fn stub_exchange() -> ExchangeTokens {
        ExchangeTokens {
            access_token: SecretString::new(format!(
                "header.{}.signature",
                base64::engine::general_purpose::URL_SAFE_NO_PAD.encode(format!(
                    r#"{{"exp":{},"sub":"user|abc123"}}"#,
                    Utc::now().timestamp() + 3600
                ))
            )),
            refresh_token: None,
            email: Some("user@example.com".into()),
        }
    }

    #[async_trait]
    impl CursorApi for StubApi {
        async fn exchange_user_api_key(&self, _: &str) -> Result<ExchangeTokens, ApiFailure> {
            self.exchanges.lock().unwrap().pop_front().unwrap()
        }

        async fn current_period(&self, _: &str) -> Result<CurrentPeriodUsage, ApiFailure> {
            Ok(CurrentPeriodUsage::default())
        }

        async fn plan_info(&self, _: &str) -> Result<PlanInfoResponse, ApiFailure> {
            Err(ApiFailure::protocol("unimplemented"))
        }

        async fn credit_grants(&self, _: &str) -> Result<CreditGrantsBalance, ApiFailure> {
            Err(ApiFailure::protocol("unimplemented"))
        }

        async fn hard_limit(&self, _: &str) -> Result<HardLimit, ApiFailure> {
            Err(ApiFailure::protocol("unimplemented"))
        }
    }

    fn ready<F: Future>(future: F) -> F::Output {
        let mut context = Context::from_waker(Waker::noop());
        let mut future = Box::pin(future);
        match future.as_mut().poll(&mut context) {
            Poll::Ready(output) => output,
            Poll::Pending => panic!("stub future unexpectedly yielded"),
        }
    }

    fn provider_with_exchanges(count: usize) -> CursorProvider {
        CursorProvider::with_api(Arc::new(StubApi {
            exchanges: Mutex::new(VecDeque::from_iter((0..count).map(|_| Ok(stub_exchange())))),
        }))
    }

    fn authenticate(provider: &CursorProvider) {
        let challenge = ready(provider.start_auth(AuthStartRequest {
            method: Some(AuthMethod::ApiToken),
            redirect_uri: None,
        }))
        .unwrap();
        ready(provider.complete_auth(AuthCompleteRequest {
            flow_id: challenge.flow_id,
            authorization_code: Some("redacted-user-api-key".into()),
            redirect_uri: None,
        }))
        .unwrap();
    }

    fn expire_pending_flow(provider: &CursorProvider) {
        let mut state = provider.lock_state().unwrap();
        let pending = state.pending_flow.take().expect("a pending flow");
        let flow_id = pending.flow_id().to_owned();
        state.pending_flow = Some(PendingFlow::ApiKey {
            flow_id,
            expires_at: Utc::now() - Duration::minutes(1),
        });
    }

    #[test]
    fn expired_api_key_flow_cannot_complete_but_releases_the_slot() {
        let provider = provider_with_exchanges(2);
        let challenge = ready(provider.start_auth(AuthStartRequest {
            method: Some(AuthMethod::ApiToken),
            redirect_uri: None,
        }))
        .unwrap();
        expire_pending_flow(&provider);
        assert!(matches!(
            ready(provider.complete_auth(AuthCompleteRequest {
                flow_id: challenge.flow_id,
                authorization_code: Some("redacted-user-api-key".into()),
                redirect_uri: None,
            })),
            Err(ProviderError::AuthenticationInvalid { .. })
        ));
        // The slot is free, so a fresh flow starts and completes.
        authenticate(&provider);
        assert!(matches!(
            ready(provider.auth_status()).unwrap(),
            AuthState::Authenticated { .. }
        ));
    }

    #[test]
    fn an_abandoned_api_key_flow_does_not_block_refresh() {
        let provider = provider_with_exchanges(2);
        authenticate(&provider);
        ready(provider.start_auth(AuthStartRequest {
            method: Some(AuthMethod::ApiToken),
            redirect_uri: None,
        }))
        .unwrap();
        expire_pending_flow(&provider);
        assert!(matches!(
            ready(provider.refresh_auth()).unwrap(),
            AuthState::Authenticated { .. }
        ));
    }

    #[test]
    fn an_abandoned_api_key_flow_does_not_block_queries() {
        let provider = provider_with_exchanges(1);
        authenticate(&provider);
        provider.lock_state().unwrap().pending_flow = Some(PendingFlow::ApiKey {
            flow_id: "stale-flow".into(),
            expires_at: Utc::now() - Duration::minutes(1),
        });
        assert!(matches!(
            ready(provider.query(UsageQuery::default())).unwrap(),
            QueryOutcome::Partial { .. } | QueryOutcome::Complete { .. }
        ));
    }
}
