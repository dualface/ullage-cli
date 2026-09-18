//! sub2api gateway upstream account authentication and usage polling.
//!
//! sub2api (Wei-Shaw/sub2api) is a self-hosted subscription gateway that
//! hosts upstream provider accounts. Its admin API is protected by a single
//! admin API key (`admin-<64hex>`) sent as `x-api-key`, so sign-in asks the
//! user to paste `base_url admin_key upstream_ref`: the gateway base URL,
//! the admin key, and a reference (upstream account id, or its name) to the
//! one upstream account this Ullage account tracks. One Ullage account maps
//! to exactly one gateway upstream account.
//!
//! Usage comes from `GET /api/v1/admin/accounts/:id/usage`: every Ullage
//! probe passes `force=true` so the gateway reads the upstream live rather
//! than answering from its own cache. `ullage show` never reaches this
//! provider — it renders the daemon's cached snapshot — so each provider
//! query is by definition a probe and always forces.

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
    Capability, Provider, ProviderDescriptor, ProviderError, ProviderId, ProviderResult,
    QueryOutcome, SubscriptionUsage, UsageQuery,
};
use zeroize::Zeroizing;

pub use api::{ApiFailure, ApiFailureKind, HttpSub2apiApi, Sub2apiApi, validated_base_url};
pub use dto::{
    AccountsPage, AdminAccount, AiCredit, AntigravityModelQuota, QuotaWindow, Sub2apiUsage,
    UsageInfo, UsageProgress, WindowStats,
};

/// How long a started paste-the-connection flow stays completable. sub2api
/// does not date the flow, so this is Ullage's own window.
const FLOW_LIFETIME_MINUTES: i64 = 10;
/// The lookup scan for name references stops after this many listing pages
/// so a typo cannot turn sign-in into an unbounded crawl of a huge pool.
const MAX_LOOKUP_PAGES: i64 = 20;
static FLOW_SEQUENCE: AtomicU64 = AtomicU64::new(1);

/// The resolved upstream account identity the admin key and base URL point
/// at. Persisted per credential field; `upstream_id` is the stable identity
/// used for usage calls and duplicate-account detection.
#[derive(Clone)]
struct GatewaySession {
    base_url: String,
    admin_key: Zeroizing<String>,
    upstream_id: i64,
    /// Display name the gateway reports for the upstream account.
    account_name: Option<String>,
}

impl GatewaySession {
    /// The deduplication identity reported as `account_key`. An upstream id
    /// is unique only inside its own gateway — two gateways routinely share
    /// id 1 — so the key binds the id to the normalized gateway URL: the
    /// same upstream on the same gateway dedupes, the same numeric id on
    /// another gateway does not.
    fn account_key(&self) -> String {
        format!("{}#{}", self.base_url, self.upstream_id)
    }
}

#[derive(Default)]
struct ProviderState {
    generation: u64,
    pending_flow: Option<PendingFlow>,
    /// The stored or freshly pasted session. The admin key is the whole
    /// credential: sub2api issues no secondary token and reports no expiry.
    session: Option<GatewaySession>,
    invalid_reason: Option<String>,
    /// A stored record that will not decode is corrupt, not absent: the flag
    /// keeps `auth_status` able to report Invalid and logout able to delete
    /// the record even though no session can be read out of it.
    stored_credential_corrupt: bool,
    credentials_loaded: bool,
}

/// A paste-the-connection sign-in in progress. It expires so an abandoned
/// flow cannot wedge re-authentication forever.
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

pub struct Sub2apiProvider {
    api: Arc<dyn Sub2apiApi>,
    state: Mutex<ProviderState>,
    credentials: Option<(Arc<CredentialStore>, CredentialKey)>,
    /// Serializes store writes and the state commits that adopt them so a
    /// racing sign-in, logout, or expiry cannot interleave between them.
    credential_gate: tokio::sync::Mutex<()>,
}

impl Sub2apiProvider {
    pub fn new() -> ProviderResult<Self> {
        Ok(Self::with_api(Arc::new(
            HttpSub2apiApi::new().map_err(ApiFailure::into_provider_error)?,
        )))
    }

    pub fn new_with_store(credentials: Arc<CredentialStore>) -> ProviderResult<Self> {
        Self::new_with_store_for_account(credentials, "active")
    }

    pub fn new_with_store_for_account(
        credentials: Arc<CredentialStore>,
        account_id: &str,
    ) -> ProviderResult<Self> {
        let api = Arc::new(HttpSub2apiApi::new().map_err(ApiFailure::into_provider_error)?);
        Self::with_api_and_store_for_account(api, credentials, account_id)
    }

    pub fn with_api(api: Arc<dyn Sub2apiApi>) -> Self {
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
        api: Arc<dyn Sub2apiApi>,
        credentials: Arc<CredentialStore>,
    ) -> ProviderResult<Self> {
        Self::with_api_and_store_for_account(api, credentials, "active")
    }

    pub fn with_api_and_store_for_account(
        api: Arc<dyn Sub2apiApi>,
        credentials: Arc<CredentialStore>,
        account_id: &str,
    ) -> ProviderResult<Self> {
        let key = CredentialKey::new("sub2api", account_id).map_err(credential_error)?;
        Ok(Self {
            api,
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
                state.credentials_loaded = true;
                return Ok(());
            }
            Err(error) => return Err(credential_error(error)),
        };
        match stored_session(stored.credential()) {
            Ok(session) => {
                let mut state = self.lock_state()?;
                state.generation = state.generation.wrapping_add(1);
                state.session = Some(session);
                state.credentials_loaded = true;
            }
            // The record exists but does not decode into a usable session: it
            // is corrupt, not absent. Invalid lets sign-in repair it and
            // logout delete it.
            Err(_) => {
                let mut state = self.lock_state()?;
                state.stored_credential_corrupt = true;
                state.credentials_loaded = true;
            }
        }
        Ok(())
    }

    /// Persists the validated session, then commits it as the live session.
    /// The persist gate spans the store write and the commit so a racing
    /// sign-in or logout cannot interleave between them; store I/O itself
    /// stays outside the state lock.
    async fn install_session(
        &self,
        session: GatewaySession,
        expected_generation: u64,
        expected_flow: &str,
    ) -> ProviderResult<AuthState> {
        let _persist_gate = self.credential_gate.lock().await;
        {
            let state = self.lock_state()?;
            if !Self::flow_is_current(&state, expected_generation, expected_flow) {
                return Err(ProviderError::ProtocolIncompatible {
                    message: "sub2api authentication operation was superseded".into(),
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
                    "upstream_id",
                    SecretValue::new(session.upstream_id.to_string().into_bytes()),
                )
                .map_err(credential_error)?;
            if let Some(name) = &session.account_name {
                credential
                    .insert("account_name", SecretValue::new(name.as_bytes()))
                    .map_err(credential_error)?;
            }

            // A completed sign-in supersedes whatever the store holds.
            store.set(key, credential).map_err(credential_error)?;
        }
        let mut state = self.lock_state()?;
        if !Self::flow_is_current(&state, expected_generation, expected_flow) {
            return Err(ProviderError::ProtocolIncompatible {
                message: "sub2api authentication operation was superseded".into(),
            });
        }
        state.generation = state.generation.wrapping_add(1);
        state.pending_flow = None;
        state.invalid_reason = None;
        state.stored_credential_corrupt = false;
        let auth = AuthState::Authenticated {
            account_label: session.account_name.clone(),
            account_key: Some(session.account_key()),
            expires_at: None,
        };
        state.session = Some(session);
        Ok(auth)
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
                    state
                        .pending_flow
                        .as_ref()
                        .map(|pending| pending.flow_id.as_str())
                        == Some(flow_id)
                }
                None => state.session.is_some() && state.pending_flow.is_none(),
            };
        if !operation_is_current {
            return ProviderError::ProtocolIncompatible {
                message: "sub2api authentication operation was superseded".into(),
            };
        }
        if matches!(
            error.kind,
            ApiFailureKind::Authentication | ApiFailureKind::UpstreamAccountMissing
        ) {
            let account_key = state.session.as_ref().map(GatewaySession::account_key);
            if expected_flow.is_some() {
                // The failure belongs to the pending flow alone: dropping the
                // live session behind it would downgrade a healthy sign-in.
                state.pending_flow = None;
                if state.session.is_none() {
                    state.invalid_reason = Some(error.message.clone());
                }
            } else {
                state.generation = state.generation.wrapping_add(1);
                state.pending_flow = None;
                state.session = None;
                state.invalid_reason = Some(match account_key {
                    Some(key) => format!("{} (account {key})", error.message),
                    None => error.message.clone(),
                });
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
                message: "sub2api authentication state is unavailable".into(),
            })
    }

    /// Resolves a pasted `upstream_ref` to a concrete upstream account: a
    /// numeric reference is the account id directly; anything else is an
    /// account name looked up in the paged listing.
    async fn resolve_upstream(
        &self,
        base_url: &str,
        admin_key: &str,
        upstream_ref: &str,
    ) -> Result<AdminAccount, ApiFailure> {
        if let Ok(id) = upstream_ref.parse::<i64>() {
            return self.api.account(base_url, admin_key, id).await;
        }
        let mut page = 1;
        loop {
            let listing = self.api.accounts(base_url, admin_key, page).await?;
            if let Some(account) = listing
                .items
                .iter()
                .find(|account| account.name.as_deref() == Some(upstream_ref))
            {
                return Ok(account.clone());
            }
            if !has_next_page(&listing, page) {
                return Err(ApiFailure::upstream_account_missing(format!(
                    "no sub2api upstream account is named {upstream_ref}"
                )));
            }
            page += 1;
            if page > MAX_LOOKUP_PAGES {
                return Err(ApiFailure::upstream_account_missing(
                    "no sub2api upstream account matched within the lookup limit",
                ));
            }
        }
    }
}

#[async_trait]
impl Provider for Sub2apiProvider {
    type VendorUsage = Sub2apiUsage;

    fn descriptor(&self) -> ProviderDescriptor {
        ProviderDescriptor {
            id: ProviderId::new("sub2api"),
            display_name: "sub2api".into(),
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
            None | Some(AuthMethod::ApiToken) => {}
            Some(method) => {
                return Err(ProviderError::UnsupportedCapability {
                    capability: format!("sub2api authentication method {method:?}"),
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
                "the sub2api connection as `base_url admin_key upstream_ref` (upstream account \
                 id or name)",
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
                    message: "sub2api authentication flow ID is not active".into(),
                })?;
            (state.generation, pending.expires_at <= Utc::now())
        };
        if expired {
            // The abandoned flow must release the pending slot, under the
            // persist gate so the clear cannot interleave with a concurrent
            // install_session's store write and commit.
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
                message: "sub2api sign-in expired".into(),
            });
        }
        // `authorization_code` arrives as protocol plaintext; the paste and
        // the derived admin key are held as Zeroizing, while base_url and
        // upstream_ref are non-secret and stay plain.
        let pasted = Zeroizing::new(authorization_code.unwrap_or_default());
        let session = match parse_connection(&pasted) {
            Ok((base_url, admin_key, upstream_ref)) => {
                let base_url = validated_base_url(&base_url).map_err(|_| {
                    ProviderError::AuthenticationInvalid {
                        message: "the sub2api base URL must use HTTPS or loopback HTTP".into(),
                    }
                })?;
                let upstream = self
                    .resolve_upstream(&base_url, &admin_key, &upstream_ref)
                    .await;
                match upstream {
                    Ok(account) => GatewaySession {
                        base_url,
                        admin_key,
                        upstream_id: account.id,
                        account_name: account.name,
                    },
                    Err(error) => {
                        return Err(self
                            .resolve_api_failure(generation, Some(&flow_id), error)
                            .await);
                    }
                }
            }
            Err(error) => {
                return Err(error);
            }
        };
        self.install_session(session, generation, &flow_id).await
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
                account_key: state.session.as_ref().map(GatewaySession::account_key),
            });
        }
        if let Some(session) = &state.session {
            return Ok(AuthState::Authenticated {
                account_label: session.account_name.clone(),
                account_key: Some(session.account_key()),
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
                reason: "the stored sub2api credential is unreadable; sign in again".into(),
                account_key: None,
            });
        }
        Ok(AuthState::NotAuthenticated)
    }

    async fn logout(&self, request: LogoutRequest) -> ProviderResult<()> {
        // sub2api exposes no admin-key revocation endpoint, so logout is
        // local-only: the stored credential is deleted and the key's own
        // lifetime bounds any residual server-side validity.
        if request.account_label.is_some() {
            self.ensure_credentials_loaded().await?;
        }
        let _credential_gate = self.credential_gate.lock().await;
        let mut state = self.lock_state()?;
        // A corrupt record names no account, so the label guard cannot verify
        // it; deleting it is the recovery path.
        let unverifiable = state.stored_credential_corrupt && state.session.is_none();
        if !unverifiable
            && request.account_label.as_ref().is_some_and(|requested| {
                state
                    .session
                    .as_ref()
                    .and_then(|session| session.account_name.as_ref())
                    != Some(requested)
            })
        {
            return Err(ProviderError::AuthenticationInvalid {
                message: "requested sub2api account is not authenticated".into(),
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
        state.session = None;
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
                        message: "sub2api is not authenticated".into(),
                    })?;
            (session, state.generation)
        };
        // Every provider query is a probe: `show` reads the daemon snapshot
        // and never calls this, so `force=true` always applies here.
        let info = match self
            .api
            .usage(
                &session.base_url,
                &session.admin_key,
                session.upstream_id,
                true,
            )
            .await
        {
            Ok(info) => info,
            Err(error) => {
                return Err(self.resolve_api_failure(generation, None, error).await);
            }
        };
        let data = Sub2apiUsage {
            account_label: session.account_name.clone(),
            info,
            observed_at: Utc::now(),
        };
        // The gateway reports a degraded upstream inside `data` instead of
        // failing the request (for example `unauthenticated` when the
        // upstream token died): surface it as a partial failure so `show`
        // displays the state instead of an empty window list.
        let upstream_error = data
            .info
            .error_code
            .as_ref()
            .filter(|c| !c.is_empty())
            .map(|code| ullage_core::PartialFailure {
                scope: code.clone(),
                message: data
                    .info
                    .error
                    .clone()
                    .filter(|text| !text.is_empty())
                    .unwrap_or_else(|| "the gateway reported an upstream error".into()),
            });
        if let Some(failure) = upstream_error {
            return Ok(QueryOutcome::Partial {
                data,
                failures: vec![failure],
            });
        }
        Ok(QueryOutcome::Complete { data })
    }

    fn normalize(&self, vendor_usage: Self::VendorUsage) -> ProviderResult<SubscriptionUsage> {
        dto::normalize(vendor_usage)
    }
}

/// Splits the pasted connection string on its first two spaces: `base_url`
/// and `admin_key` never contain spaces, so the third field takes the whole
/// remainder and an upstream account name may itself contain spaces. Only
/// the admin key is secret; it leaves the split already wrapped.
fn parse_connection(pasted: &str) -> Result<(String, Zeroizing<String>, String), ProviderError> {
    let pasted = pasted.trim();
    let (base_url, rest) =
        pasted
            .split_once(' ')
            .ok_or_else(|| ProviderError::AuthenticationInvalid {
                message: "paste `base_url admin_key upstream_ref` separated by spaces".into(),
            })?;
    let (admin_key, upstream_ref) =
        rest.trim_start()
            .split_once(' ')
            .ok_or_else(|| ProviderError::AuthenticationInvalid {
                message: "paste `base_url admin_key upstream_ref` separated by spaces".into(),
            })?;
    let upstream_ref = upstream_ref.trim();
    if base_url.is_empty() || admin_key.is_empty() || upstream_ref.is_empty() {
        return Err(ProviderError::AuthenticationInvalid {
            message: "paste `base_url admin_key upstream_ref` separated by spaces".into(),
        });
    }
    Ok((
        base_url.to_owned(),
        Zeroizing::new(admin_key.to_owned()),
        upstream_ref.to_owned(),
    ))
}

/// Whether a fetched listing page promises another one. `pages` is the
/// gateway's own page count, but older responses only documented
/// `items`/`total`/`page`/`page_size`, so a missing or zero `pages` falls
/// back to `page * page_size < total`; either way a short page means the
/// list is done.
fn has_next_page(listing: &AccountsPage, page: i64) -> bool {
    if (listing.items.len() as i64) < api::ACCOUNTS_PAGE_SIZE {
        return false;
    }
    if listing.pages > 0 {
        return page < listing.pages;
    }
    page * api::ACCOUNTS_PAGE_SIZE < listing.total
}

fn new_flow_id() -> ProviderResult<String> {
    // A timestamp plus sequence is guessable, so the id carries 256 bits of
    // randomness: it is the bearer for completing a pending flow.
    Ok(format!(
        "sub2api-key-{}-{}",
        FLOW_SEQUENCE.fetch_add(1, Ordering::Relaxed),
        ullage_auth::random_url_token().map_err(|_| ProviderError::Network {
            message: "operating system randomness is unavailable".into(),
        })?
    ))
}

/// Decodes a stored credential record into the live session. Missing or
/// undecodable fields make the whole record corrupt.
fn stored_session(credential: &Credential) -> ProviderResult<GatewaySession> {
    let base_url = credential_string(credential, "base_url")?;
    let admin_key = credential_string(credential, "admin_key")?;
    let upstream_id = credential_string(credential, "upstream_id")?
        .parse::<i64>()
        .map_err(|_| credential_error(CredentialError::CorruptCredential))?;
    let account_name = credential
        .get("account_name")
        .and_then(|value| String::from_utf8(value.expose().to_vec()).ok());
    Ok(GatewaySession {
        base_url,
        admin_key: Zeroizing::new(admin_key),
        upstream_id,
        account_name,
    })
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
        message: error.provider_message("sub2api credential storage is unavailable or invalid"),
    }
}

#[cfg(test)]
#[path = "lib_tests.rs"]
mod tests;
