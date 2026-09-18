//! codex2api gateway upstream-account usage polling.
//!
//! codex2api is a self-hosted gateway that fronts many upstream Codex/Claude
//! accounts behind one admin API. One Ullage account maps to one upstream
//! account inside the gateway: sign-in pastes
//! `base_url admin_key upstream_ref`, where `upstream_ref` is the gateway
//! account id or email (names may contain spaces, so they are not accepted).
//! The admin key is the whole credential; there is no exchange or refresh
//! token.
//!
//! Every query reads `GET /api/admin/accounts` for the account's embedded
//! quota fields and plan data, then overlays the realtime window values from
//! `POST /api/admin/accounts/:id/usage/refresh`. The daemon's periodic and
//! manual probes share this single path, so a manual `probe` is realtime and
//! the snapshots `show` prints stay one poll behind it.

mod api;
mod dto;

use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex, MutexGuard};

use async_trait::async_trait;
use chrono::{DateTime, Duration, Utc};
use ullage_auth::{
    AuthChallenge, AuthCompleteRequest, AuthInputRequest, AuthMethod, AuthStartRequest, AuthState,
    Credential, CredentialError, CredentialKey, CredentialStore, LogoutRequest, SecretValue,
};
use ullage_core::{
    Capability, PartialFailure, Provider, ProviderDescriptor, ProviderError, ProviderId,
    ProviderResult, QueryOutcome, SubscriptionUsage, UsageQuery,
};
use zeroize::Zeroizing;

pub use api::{ApiFailure, ApiFailureKind, Codex2apiApi, HttpCodex2apiApi};
pub use dto::{
    AccountsResponse, Codex2apiUsage, GatewayAccount, QuotaWindow, UsageRefreshResponse,
};

/// How long a started paste flow stays completable. The gateway does not date
/// the flow, so this is Ullage's own window.
const FLOW_LIFETIME_MINUTES: i64 = 10;
static FLOW_SEQUENCE: AtomicU64 = AtomicU64::new(1);

#[derive(Default)]
struct ProviderState {
    generation: u64,
    pending_flow: Option<PendingFlow>,
    /// The stored or freshly pasted credential: gateway base URL, admin key,
    /// the upstream reference the user gave, and the resolved account
    /// identity. It is the whole session.
    credentials: Option<SessionCredentials>,
    invalid_reason: Option<String>,
    /// The upstream account id of the credential that went invalid, kept
    /// after the credential itself is dropped so `auth_status` can still name
    /// whose sign-in expired.
    invalid_account_key: Option<String>,
    /// A stored record that will not decode is corrupt, not absent: the flag
    /// keeps `auth_status` able to report Invalid and logout able to delete
    /// the record even though no key can be read out of it.
    stored_credential_corrupt: bool,
    credentials_loaded: bool,
}

/// The session material derived from the pasted `base_url admin_key
/// upstream_ref` triple.
#[derive(Clone)]
struct SessionCredentials {
    base_url: String,
    admin_key: Zeroizing<String>,
    /// The upstream reference exactly as pasted: a gateway account id or an
    /// email.
    upstream_ref: String,
    /// The gateway account id resolved at sign-in, when known. Queries match
    /// on it first so a renamed account keeps binding.
    upstream_id: Option<i64>,
    /// Display label resolved at sign-in (email preferred, then name).
    account_label: Option<String>,
}

/// A paste-the-triple sign-in in progress. It expires so an abandoned flow
/// cannot wedge re-authentication forever.
struct PendingFlow {
    flow_id: String,
    expires_at: DateTime<Utc>,
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
            .is_some_and(|pending| pending.expires_at <= Utc::now())
        {
            self.pending_flow = None;
            return true;
        }
        false
    }
}

pub struct Codex2apiProvider {
    api: Arc<dyn Codex2apiApi>,
    state: Mutex<ProviderState>,
    credentials: Option<(Arc<CredentialStore>, CredentialKey)>,
    /// Serializes store writes and the state commits that adopt them so a
    /// racing sign-in, logout, or expiry cannot interleave between them.
    credential_gate: tokio::sync::Mutex<()>,
}

impl Codex2apiProvider {
    pub fn new() -> ProviderResult<Self> {
        Ok(Self::with_api(Arc::new(
            HttpCodex2apiApi::new().map_err(ApiFailure::into_provider_error)?,
        )))
    }

    pub fn new_with_store(credentials: Arc<CredentialStore>) -> ProviderResult<Self> {
        Self::new_with_store_for_account(credentials, "active")
    }

    pub fn new_with_store_for_account(
        credentials: Arc<CredentialStore>,
        account_id: &str,
    ) -> ProviderResult<Self> {
        let api = Arc::new(HttpCodex2apiApi::new().map_err(ApiFailure::into_provider_error)?);
        Self::with_api_and_store_for_account(api, credentials, account_id)
    }

    pub fn with_api(api: Arc<dyn Codex2apiApi>) -> Self {
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
        api: Arc<dyn Codex2apiApi>,
        credentials: Arc<CredentialStore>,
    ) -> ProviderResult<Self> {
        Self::with_api_and_store_for_account(api, credentials, "active")
    }

    pub fn with_api_and_store_for_account(
        api: Arc<dyn Codex2apiApi>,
        credentials: Arc<CredentialStore>,
        account_id: &str,
    ) -> ProviderResult<Self> {
        let key = CredentialKey::new("codex2api", account_id).map_err(credential_error)?;
        Ok(Self {
            api,
            state: Mutex::new(ProviderState::default()),
            credentials: Some((credentials, key)),
            credential_gate: tokio::sync::Mutex::new(()),
        })
    }

    /// Expire an abandoned pending flow under `credential_gate`, serialized
    /// against `install_credentials`'s persist+commit window: a flow cleared
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
                state.credentials_loaded = true;
                return Ok(());
            }
            Err(error) => return Err(credential_error(error)),
        };
        match session_credentials(stored.credential()) {
            Ok(session) => {
                let mut state = self.lock_state()?;
                state.generation = state.generation.wrapping_add(1);
                state.credentials = Some(session);
                state.credentials_loaded = true;
            }
            // The record exists but does not decode into a usable credential:
            // it is corrupt, not absent. Invalid lets sign-in repair it and
            // logout delete it.
            Err(_) => {
                let mut state = self.lock_state()?;
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
    async fn install_credentials(
        &self,
        session: SessionCredentials,
        expected_generation: u64,
        expected_flow: &str,
    ) -> ProviderResult<AuthState> {
        let _persist_gate = self.credential_gate.lock().await;
        {
            let state = self.lock_state()?;
            if !Self::flow_is_current(&state, expected_generation, expected_flow) {
                return Err(ProviderError::ProtocolIncompatible {
                    message: "codex2api authentication operation was superseded".into(),
                });
            }
        }
        if let Some((store, key)) = &self.credentials {
            let mut credential = Credential::new();
            credential
                .insert("base_url", SecretValue::new(session.base_url.as_bytes()))
                .map_err(credential_error)?;
            credential
                .insert("admin_key", SecretValue::new(session.admin_key.as_bytes()))
                .map_err(credential_error)?;
            credential
                .insert(
                    "upstream_ref",
                    SecretValue::new(session.upstream_ref.as_bytes()),
                )
                .map_err(credential_error)?;
            if let Some(upstream_id) = session.upstream_id {
                credential
                    .insert(
                        "upstream_id",
                        SecretValue::new(upstream_id.to_string().into_bytes()),
                    )
                    .map_err(credential_error)?;
            }
            if let Some(label) = &session.account_label {
                credential
                    .insert("account_label", SecretValue::new(label.as_bytes()))
                    .map_err(credential_error)?;
            }
            // A completed sign-in supersedes whatever the store holds.
            store.set(key, credential).map_err(credential_error)?;
        }
        let account_label = session.account_label.clone();
        let account_key = session.upstream_id.map(|id| id.to_string());
        let mut state = self.lock_state()?;
        if !Self::flow_is_current(&state, expected_generation, expected_flow) {
            return Err(ProviderError::ProtocolIncompatible {
                message: "codex2api authentication operation was superseded".into(),
            });
        }
        state.generation = state.generation.wrapping_add(1);
        state.pending_flow = None;
        state.invalid_reason = None;
        state.invalid_account_key = None;
        state.stored_credential_corrupt = false;
        state.credentials = Some(session);
        Ok(AuthState::Authenticated {
            account_label,
            account_key,
            expires_at: None,
        })
    }

    /// Applies a failed sign-in validation or a rejected credential seen
    /// while querying. Under the persist gate so the writes cannot interleave
    /// with `install_credentials`.
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
                    state
                        .pending_flow
                        .as_ref()
                        .map(|pending| pending.flow_id.as_str())
                        == Some(flow_id)
                }
                None => state.credentials.is_some() && state.pending_flow.is_none(),
            };
        if !operation_is_current {
            return ProviderError::ProtocolIncompatible {
                message: "codex2api authentication operation was superseded".into(),
            };
        }
        if error.kind == ApiFailureKind::Authentication {
            if expected_flow.is_some() {
                // The failure belongs to the pending flow alone: dropping the
                // live session behind it would downgrade a healthy sign-in.
                state.pending_flow = None;
                if state.credentials.is_none() {
                    state.invalid_reason = Some(error.message.clone());
                }
            } else {
                state.generation = state.generation.wrapping_add(1);
                state.pending_flow = None;
                state.invalid_account_key = state
                    .credentials
                    .as_ref()
                    .and_then(|session| session.upstream_id.map(|id| id.to_string()));
                state.credentials = None;
                state.invalid_reason = Some(error.message.clone());
            }
        }
        error.into_provider_error()
    }

    fn flow_is_current(state: &ProviderState, generation: u64, flow_id: &str) -> bool {
        state.generation == generation
            && state
                .pending_flow
                .as_ref()
                .map(|pending| pending.flow_id.as_str())
                == Some(flow_id)
    }

    fn lock_state(&self) -> ProviderResult<MutexGuard<'_, ProviderState>> {
        self.state
            .lock()
            .map_err(|_| ProviderError::ProtocolIncompatible {
                message: "codex2api authentication state is unavailable".into(),
            })
    }

    /// Finds the bound upstream account inside the admin list. The resolved
    /// id wins so a renamed account keeps binding; otherwise the pasted
    /// reference is matched as an id when it is numeric and as an email
    /// otherwise.
    fn find_account<'a>(
        accounts: &'a [GatewayAccount],
        upstream_id: Option<i64>,
        upstream_ref: &str,
    ) -> Option<&'a GatewayAccount> {
        if let Some(id) = upstream_id {
            if let Some(account) = accounts.iter().find(|account| account.id == id) {
                return Some(account);
            }
        }
        if let Ok(id) = upstream_ref.parse::<i64>() {
            if let Some(account) = accounts.iter().find(|account| account.id == id) {
                return Some(account);
            }
        }
        accounts.iter().find(|account| {
            account
                .email
                .as_deref()
                .is_some_and(|email| email.eq_ignore_ascii_case(upstream_ref))
        })
    }
}

/// Splits the pasted `base_url admin_key upstream_ref` triple. Whitespace
/// cannot appear inside any of the three values, so a simple split keeps the
/// format unambiguous. A malformed paste is reported as an invalid credential
/// without touching the gateway: the flow stays pending so the user can paste
/// again inside its lifetime.
fn parse_pasted_credentials(pasted: &str) -> ProviderResult<(String, String, String)> {
    let invalid = |message: &str| ProviderError::AuthenticationInvalid {
        message: message.to_owned(),
    };
    let parts: Vec<&str> = pasted.split_whitespace().collect();
    let [base_url, admin_key, upstream_ref]: [&str; 3] = parts.try_into().map_err(|_| {
        invalid(
            "paste the codex2api gateway base URL, admin key, and upstream account id or email, separated by spaces",
        )
    })?;
    HttpCodex2apiApi::check_base_url(base_url).map_err(|error| invalid(&error.message))?;
    if upstream_ref.contains('@') || upstream_ref.parse::<i64>().is_ok() {
        Ok((
            base_url.trim_end_matches('/').to_owned(),
            admin_key.to_owned(),
            upstream_ref.to_owned(),
        ))
    } else {
        Err(invalid(
            "the codex2api upstream reference must be the gateway account id or email",
        ))
    }
}

#[async_trait]
impl Provider for Codex2apiProvider {
    type VendorUsage = Codex2apiUsage;

    fn descriptor(&self) -> ProviderDescriptor {
        ProviderDescriptor {
            id: ProviderId::new("codex2api"),
            display_name: "Codex2API".into(),
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
        match request.method {
            None | Some(AuthMethod::ApiToken) => {}
            Some(method) => {
                return Err(ProviderError::UnsupportedCapability {
                    capability: format!("codex2api authentication method {method:?}"),
                });
            }
        }
        let _credential_gate = self.credential_gate.lock().await;

        let flow_id = new_flow_id()?;
        // The flow waits for user input, so it gets a lifetime: abandoning it
        // must not wedge the pending slot forever.
        let expires_at = Utc::now() + Duration::minutes(FLOW_LIFETIME_MINUTES);
        let challenge = AuthChallenge {
            flow_id: flow_id.clone(),
            method: AuthMethod::ApiToken,
            verification_uri: None,
            user_code: None,
            expires_at: Some(expires_at),
            input: Some(AuthInputRequest::secret(
                "the codex2api gateway base URL, admin key, and upstream account id or email, separated by spaces",
            )),
        };

        let mut state = self.lock_state()?;
        // Starting a replacement flow deliberately supersedes any persisted
        // credential without first validating that potentially invalid key.
        state.credentials_loaded = true;
        state.generation = state.generation.wrapping_add(1);
        state.pending_flow = Some(PendingFlow {
            flow_id,
            expires_at,
        });
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
        let (generation, expired) = {
            let state = self.lock_state()?;
            let pending = state
                .pending_flow
                .as_ref()
                .filter(|pending| pending.flow_id == flow_id)
                .ok_or_else(|| ProviderError::ProtocolIncompatible {
                    message: "codex2api authentication flow ID is not active".into(),
                })?;
            (state.generation, pending.expires_at <= Utc::now())
        };
        if expired {
            // The abandoned flow must release the pending slot, under the
            // persist gate so the clear cannot interleave with a concurrent
            // install_credentials's store write and commit.
            let _persist_gate = self.credential_gate.lock().await;
            let mut state = self.lock_state()?;
            if state
                .pending_flow
                .as_ref()
                .map(|pending| pending.flow_id.as_str())
                == Some(flow_id.as_str())
            {
                state.pending_flow = None;
            }
            return Err(ProviderError::AuthenticationInvalid {
                message: "codex2api gateway sign-in expired".into(),
            });
        }
        // `authorization_code` arrives as protocol plaintext; from here on the
        // only held copies are Zeroizing.
        let pasted = Zeroizing::new(authorization_code.unwrap_or_default());
        let (base_url, admin_key, upstream_ref) = parse_pasted_credentials(&pasted)?;
        let admin_key = Zeroizing::new(admin_key);
        // Listing the accounts once validates the admin key and resolves the
        // upstream reference to a concrete account in a single round trip.
        let accounts = match self.api.list_accounts(&base_url, &admin_key).await {
            Ok(response) => response.accounts,
            Err(error) => {
                return Err(self
                    .resolve_api_failure(generation, Some(&flow_id), error)
                    .await);
            }
        };
        let Some(account) = Self::find_account(&accounts, None, &upstream_ref) else {
            return Err(self
                .resolve_api_failure(
                    generation,
                    Some(&flow_id),
                    ApiFailure::authentication(
                        "no codex2api upstream account matches the given id or email",
                    ),
                )
                .await);
        };
        let session = SessionCredentials {
            base_url,
            admin_key,
            upstream_ref,
            upstream_id: Some(account.id),
            account_label: dto::account_label(account),
        };
        self.install_credentials(session, generation, &flow_id)
            .await
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
                account_key: state.invalid_account_key.clone().or_else(|| {
                    state
                        .credentials
                        .as_ref()
                        .and_then(|session| session.upstream_id.map(|id| id.to_string()))
                }),
            });
        }
        if let Some(session) = &state.credentials {
            return Ok(AuthState::Authenticated {
                account_label: session.account_label.clone(),
                account_key: session.upstream_id.map(|id| id.to_string()),
                expires_at: None,
            });
        }
        if let Some(pending) = &state.pending_flow {
            return Ok(AuthState::Pending {
                flow_id: pending.flow_id.clone(),
                expires_at: Some(pending.expires_at),
            });
        }
        if state.stored_credential_corrupt {
            return Ok(AuthState::Invalid {
                reason: "the stored codex2api credential is unreadable; sign in again".into(),
                account_key: None,
            });
        }
        Ok(AuthState::NotAuthenticated)
    }

    async fn logout(&self, request: LogoutRequest) -> ProviderResult<()> {
        // The gateway exposes no admin-key revocation endpoint, so logout is
        // local-only: the stored credential is deleted and the key's own
        // lifetime bounds any residual server-side validity.
        if request.account_label.is_some() {
            self.ensure_credentials_loaded().await?;
        }
        let _credential_gate = self.credential_gate.lock().await;
        let mut state = self.lock_state()?;
        // A label-scoped logout can only proceed when the label matches the
        // resolved account, or when a corrupt record makes the guard
        // unverifiable; deleting it is the recovery path.
        let label_matches = request.account_label.as_ref().is_some_and(|label| {
            state
                .credentials
                .as_ref()
                .is_some_and(|session| session.account_label.as_ref() == Some(label))
        });
        let unverifiable = state.stored_credential_corrupt && state.credentials.is_none();
        if request.account_label.is_some() && !label_matches && !unverifiable {
            return Err(ProviderError::AuthenticationInvalid {
                message: "requested codex2api account is not authenticated".into(),
            });
        }
        if let Some((store, key)) = &self.credentials {
            match store.delete(key) {
                Ok(()) | Err(CredentialError::NotFound) => {}
                Err(error) => return Err(credential_error(error)),
            }
        }
        state.generation = state.generation.wrapping_add(1);
        state.pending_flow = None;
        state.credentials = None;
        state.invalid_reason = None;
        state.invalid_account_key = None;
        state.stored_credential_corrupt = false;
        Ok(())
    }

    async fn query(&self, _query: UsageQuery) -> ProviderResult<QueryOutcome<Self::VendorUsage>> {
        self.ensure_credentials_loaded().await?;
        self.expire_pending_flow_gated().await?;
        let (session, generation) =
            {
                let state = self.lock_state()?;
                let session = state.credentials.clone().ok_or_else(|| {
                    ProviderError::AuthenticationInvalid {
                        message: "codex2api is not authenticated".into(),
                    }
                })?;
                (session, state.generation)
            };
        let accounts = match self
            .api
            .list_accounts(&session.base_url, &session.admin_key)
            .await
        {
            Ok(response) => response.accounts,
            Err(error) => {
                return Err(self.resolve_api_failure(generation, None, error).await);
            }
        };
        let Some(account) =
            Self::find_account(&accounts, session.upstream_id, &session.upstream_ref)
        else {
            return Err(self
                .resolve_api_failure(
                    generation,
                    None,
                    ApiFailure::authentication(
                        "the bound codex2api upstream account is not listed by the gateway",
                    ),
                )
                .await);
        };
        let account = account.clone();
        // The refresh call probes just this one upstream account; its values
        // overlay the list-embedded ones, which may lag a gateway probe cycle.
        let refresh = self
            .api
            .refresh_usage(&session.base_url, &session.admin_key, account.id)
            .await;
        let refresh = match refresh {
            Ok(refresh) => Some(refresh),
            Err(error) if error.kind == ApiFailureKind::Authentication => {
                return Err(self.resolve_api_failure(generation, None, error).await);
            }
            Err(_) => None,
        };
        let usage = Codex2apiUsage {
            account_label: dto::account_label(&account).or(session.account_label.clone()),
            account_key: Some(account.id.to_string()),
            plan_type: account.plan_type.clone(),
            subscription_expires_at: dto::parse_gateway_time(
                account.subscription_expires_at.as_deref(),
            ),
            window_7d_kind: account.usage_window_7d_kind.clone(),
            five_hours: merge_window(
                account.usage_percent_5h,
                account.reset_5h_at.as_deref(),
                refresh
                    .as_ref()
                    .and_then(|refresh| refresh.usage_percent_5h),
                refresh
                    .as_ref()
                    .and_then(|refresh| refresh.reset_5h_at.as_deref()),
                account.billed_5h,
            ),
            long: merge_window(
                account.usage_percent_7d,
                account.reset_7d_at.as_deref(),
                refresh
                    .as_ref()
                    .and_then(|refresh| refresh.usage_percent_7d),
                refresh
                    .as_ref()
                    .and_then(|refresh| refresh.reset_7d_at.as_deref()),
                account.billed_7d,
            ),
            spark: merge_window(
                account.usage_percent_spark,
                account.reset_spark_at.as_deref(),
                refresh
                    .as_ref()
                    .and_then(|refresh| refresh.usage_percent_spark),
                refresh
                    .as_ref()
                    .and_then(|refresh| refresh.reset_spark_at.as_deref()),
                None,
            ),
            observed_at: Utc::now(),
        };
        if refresh.is_none() {
            // The list row still carries the last probed values, so the query
            // delivers data but flags that it is not the realtime reading a
            // manual probe asked for.
            return Ok(QueryOutcome::Partial {
                data: usage,
                failures: vec![PartialFailure {
                    scope: "usage_refresh".into(),
                    message: "the realtime usage refresh failed; values come from the account list"
                        .into(),
                }],
            });
        }
        Ok(QueryOutcome::Complete { data: usage })
    }

    fn normalize(&self, vendor_usage: Self::VendorUsage) -> ProviderResult<SubscriptionUsage> {
        dto::normalize(vendor_usage)
    }
}

/// Merges one quota window: realtime refresh values win over the list-embedded
/// ones; billing data exists on the list row only.
fn merge_window(
    listed_percent: Option<f64>,
    listed_reset: Option<&str>,
    refreshed_percent: Option<f64>,
    refreshed_reset: Option<&str>,
    billed: Option<f64>,
) -> QuotaWindow {
    QuotaWindow {
        percent: refreshed_percent.or(listed_percent),
        resets_at: dto::parse_gateway_time(refreshed_reset)
            .or(dto::parse_gateway_time(listed_reset)),
        billed,
    }
}

fn new_flow_id() -> ProviderResult<String> {
    // A timestamp plus sequence is guessable, so the id carries 256 bits of
    // randomness: it is the bearer for completing a pending flow.
    Ok(format!(
        "codex2api-key-{}-{}",
        FLOW_SEQUENCE.fetch_add(1, Ordering::Relaxed),
        ullage_auth::random_url_token().map_err(|_| ProviderError::Network {
            message: "operating system randomness is unavailable".into(),
        })?
    ))
}

/// Decodes the stored credential record into the live session material. Any
/// missing or unreadable field makes the record corrupt, which surfaces as
/// Invalid rather than silently unauthenticated.
fn session_credentials(credential: &Credential) -> ProviderResult<SessionCredentials> {
    let base_url = credential_string(credential, "base_url")?;
    let admin_key = credential_string(credential, "admin_key")?;
    let upstream_ref = credential_string(credential, "upstream_ref")?;
    let upstream_id = credential_string_optional(credential, "upstream_id")?
        .map(|value| value.parse::<i64>())
        .transpose()
        .map_err(|_| credential_error(CredentialError::CorruptCredential))?;
    let account_label = credential_string_optional(credential, "account_label")?;
    Ok(SessionCredentials {
        base_url: base_url.trim_end_matches('/').to_owned(),
        admin_key: Zeroizing::new(admin_key),
        upstream_ref,
        upstream_id,
        account_label,
    })
}

fn credential_string_optional(
    credential: &Credential,
    field: &str,
) -> ProviderResult<Option<String>> {
    let Some(value) = credential.get(field) else {
        return Ok(None);
    };
    String::from_utf8(value.expose().to_vec())
        .map(Some)
        .map_err(|_| credential_error(CredentialError::CorruptCredential))
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
        message: error.provider_message("codex2api credential storage is unavailable or invalid"),
    }
}

#[cfg(test)]
#[path = "lib_tests.rs"]
mod tests;
