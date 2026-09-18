//! OpenCode Go subscription authentication and usage polling.
//!
//! OpenCode Go has no OAuth or device-code flow: keys are issued manually at
//! `https://opencode.ai/auth`. Sign-in therefore asks the user to paste the
//! key, the same shape as Cursor's API-key path, and the key itself is the
//! bearer credential for `GET /zen/go/v1/usage`. There is no token exchange
//! and nothing to refresh: a key stands until the server rejects it.

mod api;
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
use zeroize::Zeroizing;

pub use api::{ApiFailure, ApiFailureKind, HttpOpencodeApi, OpencodeApi};
pub use dto::{OpencodeUsage, UsageResponse, UsageWindows, WindowUsage};

/// The official key page the OpenCode docs send users to (`/connect`, pick
/// OpenCode Go, sign in at opencode.ai/auth, copy the API key).
const API_KEY_PAGE: &str = "https://opencode.ai/auth";
/// How long a started paste-the-key flow stays completable. OpenCode does not
/// date the flow, so this is Ullage's own window.
const FLOW_LIFETIME_MINUTES: i64 = 10;
static FLOW_SEQUENCE: AtomicU64 = AtomicU64::new(1);

#[derive(Default)]
struct ProviderState {
    generation: u64,
    session_id: u64,
    pending_flow: Option<PendingFlow>,
    /// The stored or freshly pasted key. It is the whole session: OpenCode
    /// issues no secondary token and reports no account identity or expiry.
    api_key: Option<Zeroizing<String>>,
    invalid_reason: Option<String>,
    /// Store version `api_key` was observed at.
    stored_version: Option<CredentialVersion>,
    /// A stored record that will not decode is corrupt, not absent: the flag
    /// keeps `auth_status` able to report Invalid and logout able to delete
    /// the record even though no key can be read out of it.
    stored_credential_corrupt: bool,
    credentials_loaded: bool,
}

/// A paste-the-key sign-in in progress. It expires so an abandoned flow
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

pub struct OpencodeProvider {
    api: Arc<dyn OpencodeApi>,
    state: Mutex<ProviderState>,
    credentials: Option<(Arc<CredentialStore>, CredentialKey)>,
    /// Serializes store writes and the state commits that adopt them so a
    /// racing sign-in, logout, or expiry cannot interleave between them.
    credential_gate: tokio::sync::Mutex<()>,
}

impl OpencodeProvider {
    pub fn new() -> ProviderResult<Self> {
        Ok(Self::with_api(Arc::new(
            HttpOpencodeApi::new().map_err(ApiFailure::into_provider_error)?,
        )))
    }

    pub fn new_with_store(credentials: Arc<CredentialStore>) -> ProviderResult<Self> {
        Self::new_with_store_for_account(credentials, "active")
    }

    pub fn new_with_store_for_account(
        credentials: Arc<CredentialStore>,
        account_id: &str,
    ) -> ProviderResult<Self> {
        let api = Arc::new(HttpOpencodeApi::new().map_err(ApiFailure::into_provider_error)?);
        Self::with_api_and_store_for_account(api, credentials, account_id)
    }

    pub fn with_api(api: Arc<dyn OpencodeApi>) -> Self {
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
        api: Arc<dyn OpencodeApi>,
        credentials: Arc<CredentialStore>,
    ) -> ProviderResult<Self> {
        Self::with_api_and_store_for_account(api, credentials, "active")
    }

    pub fn with_api_and_store_for_account(
        api: Arc<dyn OpencodeApi>,
        credentials: Arc<CredentialStore>,
        account_id: &str,
    ) -> ProviderResult<Self> {
        let key = CredentialKey::new("opencode", account_id).map_err(credential_error)?;
        Ok(Self {
            api,
            state: Mutex::new(ProviderState::default()),
            credentials: Some((credentials, key)),
            credential_gate: tokio::sync::Mutex::new(()),
        })
    }

    /// Expire an abandoned pending flow under `credential_gate`, serialized
    /// against `install_api_key`'s persist+commit window: a flow cleared
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
                let mut state = self.lock_state()?;
                state.generation = state.generation.wrapping_add(1);
                state.session_id = state.session_id.wrapping_add(1);
                state.api_key = Some(Zeroizing::new(api_key));
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

    /// Persists the validated key, then commits it as the live session. The
    /// persist gate spans the store write and the commit so a racing sign-in
    /// or logout cannot interleave between them; store I/O itself stays
    /// outside the state lock.
    async fn install_api_key(
        &self,
        api_key: Zeroizing<String>,
        expected_generation: u64,
        expected_flow: &str,
    ) -> ProviderResult<AuthState> {
        let _persist_gate = self.credential_gate.lock().await;
        {
            let state = self.lock_state()?;
            if !Self::flow_is_current(&state, expected_generation, expected_flow) {
                return Err(ProviderError::ProtocolIncompatible {
                    message: "OpenCode authentication operation was superseded".into(),
                });
            }
        }
        let stored_version = if let Some((store, key)) = &self.credentials {
            let mut credential = Credential::new();
            credential
                .insert("api_key", SecretValue::new(api_key.as_bytes()))
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
                message: "OpenCode authentication operation was superseded".into(),
            });
        }
        state.stored_version = stored_version;
        state.generation = state.generation.wrapping_add(1);
        state.session_id = state.session_id.wrapping_add(1);
        state.pending_flow = None;
        state.invalid_reason = None;
        state.stored_credential_corrupt = false;
        state.api_key = Some(api_key);
        Ok(AuthState::Authenticated {
            account_label: None,
            account_key: None,
            expires_at: None,
        })
    }

    /// Applies a failed sign-in validation or a rejected key seen while
    /// querying. Under the persist gate so the writes cannot interleave with
    /// `install_api_key`.
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
                None => state.api_key.is_some() && state.pending_flow.is_none(),
            };
        if !operation_is_current {
            return ProviderError::ProtocolIncompatible {
                message: "OpenCode authentication operation was superseded".into(),
            };
        }
        if error.kind == ApiFailureKind::Authentication {
            if expected_flow.is_some() {
                // The failure belongs to the pending flow alone: dropping the
                // live session behind it would downgrade a healthy sign-in.
                state.pending_flow = None;
                if state.api_key.is_none() {
                    state.invalid_reason = Some(error.message.clone());
                }
            } else {
                state.generation = state.generation.wrapping_add(1);
                state.session_id = state.session_id.wrapping_add(1);
                state.pending_flow = None;
                state.api_key = None;
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
                message: "OpenCode authentication state is unavailable".into(),
            })
    }
}

#[async_trait]
impl Provider for OpencodeProvider {
    type VendorUsage = OpencodeUsage;

    fn descriptor(&self) -> ProviderDescriptor {
        ProviderDescriptor {
            id: ProviderId::new("opencode"),
            display_name: "OpenCode".into(),
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
                    capability: format!("OpenCode authentication method {method:?}"),
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
            verification_uri: Some(API_KEY_PAGE.into()),
            user_code: None,
            expires_at: Some(expires_at),
            input: Some(AuthInputRequest::secret("the OpenCode Go API key")),
        };

        let mut state = self.lock_state()?;
        // Starting a replacement flow deliberately supersedes any persisted
        // credential without first validating that potentially invalid key.
        state.credentials_loaded = true;
        state.generation = state.generation.wrapping_add(1);
        state.session_id = state.session_id.wrapping_add(1);
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
                    message: "OpenCode authentication flow ID is not active".into(),
                })?;
            (state.generation, pending.expires_at <= Utc::now())
        };
        if expired {
            // The abandoned flow must release the pending slot, under the
            // persist gate so the clear cannot interleave with a concurrent
            // install_api_key's store write and commit.
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
                message: "OpenCode API key sign-in expired".into(),
            });
        }
        // `authorization_code` arrives as protocol plaintext; from here on the
        // only held copies are Zeroizing.
        let api_key = Zeroizing::new(authorization_code.unwrap_or_default());
        if api_key.trim().is_empty() {
            return Err(ProviderError::AuthenticationInvalid {
                message: "an OpenCode Go API key is required".into(),
            });
        }
        let api_key = Zeroizing::new(api_key.trim().to_owned());
        // The key is also the usage credential: validating it once at sign-in
        // proves it works before it is persisted.
        if let Err(error) = self.api.usage(&api_key).await {
            return Err(self
                .resolve_api_failure(generation, Some(&flow_id), error)
                .await);
        }
        self.install_api_key(api_key, generation, &flow_id).await
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
                // OpenCode reports no account identity, so nothing can name
                // the account whose credential went invalid.
                account_key: None,
            });
        }
        if state.api_key.is_some() {
            return Ok(AuthState::Authenticated {
                account_label: None,
                account_key: None,
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
                reason: "the stored OpenCode credential is unreadable; sign in again".into(),
                account_key: None,
            });
        }
        Ok(AuthState::NotAuthenticated)
    }

    async fn logout(&self, request: LogoutRequest) -> ProviderResult<()> {
        // OpenCode exposes no key-revocation endpoint, so logout is
        // local-only: the stored credential is deleted and the key's own
        // lifetime bounds any residual server-side validity.
        if request.account_label.is_some() {
            self.ensure_credentials_loaded().await?;
        }
        let _credential_gate = self.credential_gate.lock().await;
        let mut state = self.lock_state()?;
        // OpenCode never learns an account label, so a label-scoped logout can
        // only proceed when a corrupt record makes the guard unverifiable;
        // deleting it is the recovery path.
        let unverifiable = state.stored_credential_corrupt && state.api_key.is_none();
        if !unverifiable && request.account_label.is_some() {
            return Err(ProviderError::AuthenticationInvalid {
                message: "requested OpenCode account is not authenticated".into(),
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
        state.api_key = None;
        state.stored_version = None;
        state.invalid_reason = None;
        state.stored_credential_corrupt = false;
        Ok(())
    }

    async fn query(&self, _query: UsageQuery) -> ProviderResult<QueryOutcome<Self::VendorUsage>> {
        self.ensure_credentials_loaded().await?;
        self.expire_pending_flow_gated().await?;
        let (api_key, generation) = {
            let state = self.lock_state()?;
            let api_key =
                state
                    .api_key
                    .clone()
                    .ok_or_else(|| ProviderError::AuthenticationInvalid {
                        message: "OpenCode is not authenticated".into(),
                    })?;
            (api_key, state.generation)
        };
        let response = match self.api.usage(&api_key).await {
            Ok(response) => response,
            Err(error) => {
                return Err(self.resolve_api_failure(generation, None, error).await);
            }
        };
        Ok(QueryOutcome::Complete {
            data: OpencodeUsage {
                account_label: None,
                usage: response.usage,
                observed_at: Utc::now(),
            },
        })
    }

    fn normalize(&self, vendor_usage: Self::VendorUsage) -> ProviderResult<SubscriptionUsage> {
        dto::normalize(vendor_usage)
    }
}

fn new_flow_id() -> ProviderResult<String> {
    // A timestamp plus sequence is guessable, so the id carries 256 bits of
    // randomness: it is the bearer for completing a pending flow.
    Ok(format!(
        "opencode-key-{}-{}",
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
        message: error.provider_message("OpenCode credential storage is unavailable or invalid"),
    }
}

#[cfg(test)]
mod tests {
    use std::collections::VecDeque;
    use std::future::Future;
    use std::task::{Context, Poll, Waker};

    use super::*;

    struct StubApi {
        responses: Mutex<VecDeque<Result<UsageResponse, ApiFailure>>>,
    }

    impl StubApi {
        fn queue(responses: Vec<Result<UsageResponse, ApiFailure>>) -> Arc<Self> {
            Arc::new(Self {
                responses: Mutex::new(VecDeque::from_iter(responses)),
            })
        }
    }

    #[async_trait]
    impl OpencodeApi for StubApi {
        async fn usage(&self, _: &str) -> Result<UsageResponse, ApiFailure> {
            self.responses.lock().unwrap().pop_front().unwrap()
        }
    }

    fn stub_usage() -> UsageResponse {
        UsageResponse {
            usage: UsageWindows {
                rolling: Some(WindowUsage {
                    status: Some("ok".into()),
                    percent: Some(6.0),
                    resets_at: Some(Utc::now()),
                }),
                weekly: None,
                monthly: None,
            },
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

    fn authenticate(provider: &OpencodeProvider) {
        let challenge = ready(provider.start_auth(AuthStartRequest {
            method: Some(AuthMethod::ApiToken),
            redirect_uri: None,
        }))
        .unwrap();
        ready(provider.complete_auth(AuthCompleteRequest {
            flow_id: challenge.flow_id,
            authorization_code: Some("redacted-opencode-key".into()),
            redirect_uri: None,
        }))
        .unwrap();
    }

    #[test]
    fn api_key_sign_in_presents_the_key_page_and_secret_input() {
        let provider = OpencodeProvider::with_api(StubApi::queue(vec![Ok(stub_usage())]));
        let challenge = ready(provider.start_auth(AuthStartRequest::default())).unwrap();
        assert_eq!(challenge.method, AuthMethod::ApiToken);
        assert_eq!(challenge.verification_uri.as_deref(), Some(API_KEY_PAGE));
        let input = challenge.input.unwrap();
        assert!(input.secret);
    }

    #[test]
    fn complete_auth_validates_the_key_and_authenticates() {
        let provider = OpencodeProvider::with_api(StubApi::queue(vec![Ok(stub_usage())]));
        authenticate(&provider);
        assert_eq!(
            ready(provider.auth_status()).unwrap(),
            AuthState::Authenticated {
                account_label: None,
                account_key: None,
                expires_at: None,
            }
        );
    }

    #[test]
    fn a_rejected_key_fails_sign_in_and_releases_the_flow() {
        let provider = OpencodeProvider::with_api(StubApi::queue(vec![
            Err(ApiFailure::authentication("rejected")),
            Ok(stub_usage()),
        ]));
        let challenge = ready(provider.start_auth(AuthStartRequest::default())).unwrap();
        assert!(matches!(
            ready(provider.complete_auth(AuthCompleteRequest {
                flow_id: challenge.flow_id,
                authorization_code: Some("bad-key".into()),
                redirect_uri: None,
            })),
            Err(ProviderError::AuthenticationInvalid { .. })
        ));
        // With no session behind the flow the rejection is remembered, and a
        // fresh flow can still start.
        assert!(matches!(
            ready(provider.auth_status()).unwrap(),
            AuthState::Invalid { .. }
        ));
        let retry = ready(provider.start_auth(AuthStartRequest::default())).unwrap();
        assert!(
            ready(provider.complete_auth(AuthCompleteRequest {
                flow_id: retry.flow_id,
                authorization_code: Some("good-key".into()),
                redirect_uri: None,
            }))
            .is_ok()
        );
    }

    #[test]
    fn an_expired_flow_cannot_complete_but_releases_the_slot() {
        let provider = OpencodeProvider::with_api(StubApi::queue(vec![Ok(stub_usage())]));
        let challenge = ready(provider.start_auth(AuthStartRequest::default())).unwrap();
        provider.lock_state().unwrap().pending_flow = Some(PendingFlow {
            flow_id: challenge.flow_id.clone(),
            expires_at: Utc::now() - Duration::minutes(1),
        });
        assert!(matches!(
            ready(provider.complete_auth(AuthCompleteRequest {
                flow_id: challenge.flow_id,
                authorization_code: Some("key".into()),
                redirect_uri: None,
            })),
            Err(ProviderError::AuthenticationInvalid { .. })
        ));
    }

    #[test]
    fn query_reports_usage_windows() {
        let provider =
            OpencodeProvider::with_api(StubApi::queue(vec![Ok(stub_usage()), Ok(stub_usage())]));
        authenticate(&provider);
        let outcome = ready(provider.query(UsageQuery::default())).unwrap();
        let QueryOutcome::Complete { data } = outcome else {
            panic!("expected a complete outcome");
        };
        let normalized = provider.normalize(data).unwrap();
        assert_eq!(normalized.windows.len(), 1);
        assert_eq!(
            normalized.windows[0].window,
            ullage_core::UsageWindowKind::FiveHours
        );
    }

    #[test]
    fn a_rejected_key_during_query_invalidates_the_session() {
        let provider = OpencodeProvider::with_api(StubApi::queue(vec![
            Ok(stub_usage()),
            Err(ApiFailure::authentication("revoked")),
        ]));
        authenticate(&provider);
        assert!(matches!(
            ready(provider.query(UsageQuery::default())),
            Err(ProviderError::AuthenticationInvalid { .. })
        ));
        assert!(matches!(
            ready(provider.auth_status()).unwrap(),
            AuthState::Invalid { .. }
        ));
    }

    #[test]
    fn a_key_without_subscription_maps_to_unsupported_capability() {
        let provider = OpencodeProvider::with_api(StubApi::queue(vec![
            Ok(stub_usage()),
            Err(ApiFailure {
                kind: ApiFailureKind::NoSubscription,
                message: "no Go subscription".into(),
                retry_after_seconds: None,
            }),
        ]));
        authenticate(&provider);
        assert!(matches!(
            ready(provider.query(UsageQuery::default())),
            Err(ProviderError::UnsupportedCapability { capability })
                if capability == "opencode-go subscription"
        ));
        // The key itself stays valid: no subscription is not a credential failure.
        assert!(matches!(
            ready(provider.auth_status()).unwrap(),
            AuthState::Authenticated { .. }
        ));
    }

    #[test]
    fn logout_clears_the_session() {
        let provider = OpencodeProvider::with_api(StubApi::queue(vec![Ok(stub_usage())]));
        authenticate(&provider);
        ready(provider.logout(LogoutRequest::default())).unwrap();
        assert_eq!(
            ready(provider.auth_status()).unwrap(),
            AuthState::NotAuthenticated
        );
    }
}
