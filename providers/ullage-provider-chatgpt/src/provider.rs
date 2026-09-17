use std::collections::BTreeMap;
use std::fmt;
use std::sync::{Arc, Mutex};

use async_trait::async_trait;
use base64::Engine;
use base64::engine::general_purpose::URL_SAFE_NO_PAD;
use chrono::{DateTime, Duration, Utc};
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};
use ullage_auth::{
    AuthChallenge, AuthCompleteRequest, AuthInputRequest, AuthMethod, AuthStartRequest, AuthState,
    CredentialVersion, LogoutRequest, account_identity, validate_loopback_http_redirect_uri,
};
use ullage_core::{
    Capability, Provider, ProviderDescriptor, ProviderError, ProviderId, ProviderResult,
    ProviderWorkspace, QueryOutcome, SubscriptionUsage, UsageQuery,
};

use crate::{ChatGptApiError, ChatGptUsage, ChatGptUsageResponse, ChatGptWorkspace};

const OAUTH_FLOW_LIFETIME_MINUTES: i64 = 10;
/// Same lead time as the Claude provider: refresh early enough to cover
/// request latency and clock skew before the stored token actually expires.
const REFRESH_SKEW_SECONDS: i64 = 300;
const MAX_AUTHORIZATION_CODE_BYTES: usize = 16 * 1024;

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ChatGptConfig {
    pub authorization_endpoint: String,
    pub client_id: String,
    pub redirect_uri: String,
    pub scopes: Vec<String>,
}

impl ChatGptConfig {
    pub fn openai(client_id: impl Into<String>, redirect_uri: impl Into<String>) -> Self {
        Self {
            authorization_endpoint: "https://auth.openai.com/oauth/authorize".into(),
            client_id: client_id.into(),
            redirect_uri: redirect_uri.into(),
            scopes: vec![
                "openid".into(),
                "profile".into(),
                "email".into(),
                "offline_access".into(),
            ],
        }
    }
}

#[derive(Clone, PartialEq, Eq, Serialize)]
pub struct OAuthTokenSet {
    access_token: String,
    #[serde(default)]
    refresh_token: Option<String>,
    #[serde(default)]
    identity_token: Option<String>,
    #[serde(default)]
    expires_at: Option<DateTime<Utc>>,
}

impl<'de> Deserialize<'de> for OAuthTokenSet {
    /// Persisted tokens go back through the constructor's semantic validation
    /// (non-empty tokens, header safety, expiry sanity): a stored record is
    /// untrusted input once it leaves this process. Wire-only checks — size
    /// caps, `token_type`, a required positive `expires_in` — apply to fresh
    /// OAuth responses, so a legacy record keeps its optional expiry.
    fn deserialize<D>(deserializer: D) -> Result<Self, D::Error>
    where
        D: serde::Deserializer<'de>,
    {
        #[derive(Deserialize)]
        struct Raw {
            access_token: String,
            #[serde(default)]
            refresh_token: Option<String>,
            #[serde(default)]
            identity_token: Option<String>,
            #[serde(default)]
            expires_at: Option<DateTime<Utc>>,
        }
        let raw = Raw::deserialize(deserializer)?;
        Self::new(raw.access_token, raw.refresh_token, raw.expires_at)
            .and_then(|tokens| tokens.with_identity_token(raw.identity_token))
            .map_err(serde::de::Error::custom)
    }
}

impl OAuthTokenSet {
    pub fn new(
        access_token: impl Into<String>,
        refresh_token: Option<String>,
        expires_at: Option<DateTime<Utc>>,
    ) -> ProviderResult<Self> {
        let access_token = access_token.into();
        if access_token.trim().is_empty()
            || refresh_token
                .as_deref()
                .is_some_and(|token| !token.is_empty() && token.trim().is_empty())
        {
            return Err(ProviderError::ProtocolIncompatible {
                message: "OAuth response contained unusable token contents".into(),
            });
        }
        // The access token is sent as an HTTP header value; reject bytes that
        // cannot be represented as one here so every caller, including the
        // credential-store reload path, is covered.
        reqwest::header::HeaderValue::from_str(&access_token).map_err(|_| {
            ProviderError::ProtocolIncompatible {
                message: "OAuth access token cannot be used as an HTTP header".into(),
            }
        })?;
        Ok(Self {
            access_token,
            refresh_token: refresh_token.filter(|token| !token.is_empty()),
            identity_token: None,
            expires_at,
        })
    }

    pub fn access_token(&self) -> &str {
        &self.access_token
    }

    pub fn expires_at(&self) -> Option<DateTime<Utc>> {
        self.expires_at
    }

    pub fn refresh_token(&self) -> Option<&str> {
        self.refresh_token.as_deref()
    }

    pub fn identity_token(&self) -> Option<&str> {
        self.identity_token.as_deref()
    }

    pub fn with_identity_token(mut self, identity_token: Option<String>) -> ProviderResult<Self> {
        if identity_token
            .as_deref()
            .is_some_and(|token| !token.is_empty() && token.trim().is_empty())
        {
            return Err(ProviderError::ProtocolIncompatible {
                message: "OAuth response contained an unusable identity token".into(),
            });
        }
        self.identity_token = identity_token.filter(|token| !token.is_empty());
        Ok(self)
    }
}

impl fmt::Debug for OAuthTokenSet {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter
            .debug_struct("OAuthTokenSet")
            .field("access_token", &"[REDACTED]")
            .field(
                "refresh_token",
                &self.refresh_token.as_ref().map(|_| "[REDACTED]"),
            )
            .field(
                "identity_token",
                &self.identity_token.as_ref().map(|_| "[REDACTED]"),
            )
            .field("expires_at", &self.expires_at)
            .finish()
    }
}

/// Serialized as the flat persisted-session shape: the token fields sit at
/// the top level next to `workspaces` and friends, matching what the CLI
/// credential vault has always written.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct ChatGptSession {
    #[serde(flatten)]
    pub tokens: OAuthTokenSet,
    #[serde(default)]
    pub workspaces: Vec<ChatGptWorkspace>,
    #[serde(default)]
    pub selected_workspace_id: Option<String>,
    #[serde(default)]
    pub invalid_reason: Option<String>,
}

#[async_trait]
pub trait ChatGptApi: Send + Sync {
    async fn exchange_code(
        &self,
        authorization_code: &str,
        pkce_verifier: &str,
        redirect_uri: &str,
    ) -> Result<OAuthTokenSet, ChatGptApiError>;

    async fn refresh_token(&self, refresh_token: &str) -> Result<OAuthTokenSet, ChatGptApiError>;

    async fn list_workspaces(
        &self,
        tokens: &OAuthTokenSet,
    ) -> Result<Vec<ChatGptWorkspace>, ChatGptApiError>;

    async fn query_usage(
        &self,
        tokens: &OAuthTokenSet,
        workspace_id: &str,
    ) -> Result<ChatGptUsageResponse, ChatGptApiError>;

    async fn revoke(&self, tokens: &OAuthTokenSet) -> Result<(), ChatGptApiError>;
}

pub trait ChatGptSessionStore: Send + Sync {
    /// Loads the session with the store version it was observed at; that
    /// version must be passed to `replace` for any update derived from it.
    fn load(&self) -> Result<Option<(ChatGptSession, CredentialVersion)>, ChatGptApiError>;
    /// Unconditional write, valid only for a first sign-in or an explicit
    /// re-login. Returns the version the record now carries.
    fn save(&self, session: &ChatGptSession) -> Result<CredentialVersion, ChatGptApiError>;
    /// Compare-and-swap update for a session derived from a `load`.
    /// `Ok(None)` means the stored record changed or was removed since then;
    /// the caller must reload rather than overwrite.
    fn replace(
        &self,
        expected: CredentialVersion,
        session: &ChatGptSession,
    ) -> Result<Option<CredentialVersion>, ChatGptApiError>;
    fn clear(&self) -> Result<(), ChatGptApiError>;
}

#[derive(Clone)]
struct PendingOAuth {
    pkce_verifier: String,
    redirect_uri: String,
    expires_at: DateTime<Utc>,
}

struct OAuthGrantGuard<A: ChatGptApi + 'static> {
    api: Arc<A>,
    tokens: Option<OAuthTokenSet>,
}

impl<A: ChatGptApi + 'static> OAuthGrantGuard<A> {
    fn new(api: Arc<A>, tokens: OAuthTokenSet) -> Self {
        Self {
            api,
            tokens: Some(tokens),
        }
    }

    fn tokens(&self) -> &OAuthTokenSet {
        self.tokens
            .as_ref()
            .expect("OAuth grant guard must be armed")
    }

    fn disarm(&mut self) {
        self.tokens = None;
    }

    async fn revoke_now(&mut self) {
        if let Some(tokens) = self.tokens.take() {
            let _ = self.api.revoke(&tokens).await;
        }
    }
}

/// Clears the session store on drop unless disarmed: `clear` on a successful
/// revoke, `preserve` on a failed one, drop (still clearing) on cancellation.
struct SessionClearGuard<S: ChatGptSessionStore + 'static> {
    store: Arc<S>,
    armed: bool,
}

impl<S: ChatGptSessionStore + 'static> SessionClearGuard<S> {
    fn new(store: Arc<S>) -> Self {
        Self { store, armed: true }
    }

    fn clear(mut self) -> Result<(), ChatGptApiError> {
        self.armed = false;
        self.store.clear()
    }

    fn preserve(mut self) {
        self.armed = false;
    }
}

impl<S: ChatGptSessionStore + 'static> Drop for SessionClearGuard<S> {
    fn drop(&mut self) {
        if self.armed {
            let _ = self.store.clear();
        }
    }
}

impl<A: ChatGptApi + 'static> Drop for OAuthGrantGuard<A> {
    fn drop(&mut self) {
        let Some(tokens) = self.tokens.take() else {
            return;
        };
        let api = self.api.clone();
        if let Ok(runtime) = tokio::runtime::Handle::try_current() {
            runtime.spawn(async move {
                let _ = api.revoke(&tokens).await;
            });
        }
    }
}

pub struct ChatGptProvider<A, S> {
    config: ChatGptConfig,
    api: Arc<A>,
    store: Arc<S>,
    pending: Mutex<BTreeMap<String, PendingOAuth>>,
    session_gate: tokio::sync::Mutex<()>,
}

impl<A, S> ChatGptProvider<A, S>
where
    A: ChatGptApi,
    S: ChatGptSessionStore,
{
    pub fn new(config: ChatGptConfig, api: Arc<A>, store: Arc<S>) -> ProviderResult<Self> {
        if !valid_authorization_endpoint(&config.authorization_endpoint) {
            return Err(ProviderError::ProtocolIncompatible {
                message: "authorization endpoint must be a valid HTTPS URL without a fragment"
                    .into(),
            });
        }
        if !valid_redirect_uri(&config.redirect_uri) {
            return Err(ProviderError::ProtocolIncompatible {
                message: "OAuth redirect URI must use HTTPS or loopback HTTP".into(),
            });
        }
        if config.client_id.is_empty() || config.redirect_uri.is_empty() {
            return Err(ProviderError::ProtocolIncompatible {
                message: "OAuth client ID and redirect URI are required".into(),
            });
        }
        Ok(Self {
            config,
            api,
            store,
            pending: Mutex::new(BTreeMap::new()),
            session_gate: tokio::sync::Mutex::new(()),
        })
    }

    pub async fn refresh_auth(&self) -> ProviderResult<AuthState> {
        let _session_guard = self.session_gate.lock().await;
        let (session, version) = self.load_session()?;
        let (session, _) = self.refresh_session(session, version, true).await?;
        Ok(authenticated_state(&session))
    }

    pub async fn workspaces(&self) -> ProviderResult<Vec<ChatGptWorkspace>> {
        let _session_guard = self.session_gate.lock().await;
        self.workspaces_locked().await
    }

    async fn workspaces_locked(&self) -> ProviderResult<Vec<ChatGptWorkspace>> {
        let (mut session, mut version) = self.load_refreshed_session().await?;
        let workspaces = match self.api.list_workspaces(&session.tokens).await {
            Ok(workspaces) => workspaces,
            Err(error) if error.kind == crate::ChatGptApiErrorKind::AuthenticationInvalid => {
                (session, version) = self.refresh_session(session, version, true).await?;
                match self.api.list_workspaces(&session.tokens).await {
                    Ok(workspaces) => workspaces,
                    Err(error)
                        if error.kind == crate::ChatGptApiErrorKind::AuthenticationInvalid =>
                    {
                        session.invalid_reason = Some(error.message.clone());
                        self.persist_invalid_reason(version, &session)?;
                        return Err(error.into());
                    }
                    Err(error) => return Err(error.into()),
                }
            }
            Err(error) => return Err(error.into()),
        };
        validate_workspaces(&workspaces)?;
        if session
            .selected_workspace_id
            .as_ref()
            .is_some_and(|selected| !workspaces.iter().any(|item| &item.id == selected))
        {
            session.selected_workspace_id = None;
        }
        session.workspaces = workspaces.clone();
        self.save_observed(version, &session)?;
        Ok(workspaces)
    }

    pub async fn select_workspace(&self, workspace_id: &str) -> ProviderResult<ChatGptWorkspace> {
        if workspace_id.is_empty() {
            return Err(workspace_access_denied("workspace ID is empty"));
        }
        let _session_guard = self.session_gate.lock().await;
        let workspaces = self.workspaces_locked().await?;
        let selected = workspaces
            .iter()
            .find(|workspace| workspace.id == workspace_id)
            .cloned()
            .ok_or_else(|| workspace_access_denied("workspace is not available to this account"))?;
        let (mut session, version) = self.load_session()?;
        session.selected_workspace_id = Some(selected.id.clone());
        self.save_observed(version, &session)?;
        Ok(selected)
    }

    fn load_session(&self) -> ProviderResult<(ChatGptSession, CredentialVersion)> {
        let (session, version) =
            self.store
                .load()
                .map_err(ProviderError::from)?
                .ok_or_else(|| ProviderError::AuthenticationInvalid {
                    message: "ChatGPT is not authenticated".into(),
                })?;
        if let Some(reason) = &session.invalid_reason {
            return Err(ProviderError::AuthenticationInvalid {
                message: reason.clone(),
            });
        }
        Ok((session, version))
    }

    async fn load_refreshed_session(&self) -> ProviderResult<(ChatGptSession, CredentialVersion)> {
        let (session, version) = self.load_session()?;
        self.refresh_session(session, version, false).await
    }

    /// Persists an update derived from the session observed at `version`. A
    /// conflict means a sign-in or logout landed in between, so the update is
    /// dropped and reported as an authentication change.
    fn save_observed(
        &self,
        version: CredentialVersion,
        session: &ChatGptSession,
    ) -> ProviderResult<CredentialVersion> {
        self.store
            .replace(version, session)
            .map_err(ProviderError::from)?
            .ok_or_else(|| ProviderError::AuthenticationInvalid {
                message: "stored ChatGPT session changed during the update".into(),
            })
    }

    /// Marks the observed record invalid. A conflicting write means the record
    /// no longer belongs to this session, so the original error stands alone.
    fn persist_invalid_reason(
        &self,
        version: CredentialVersion,
        session: &ChatGptSession,
    ) -> ProviderResult<()> {
        self.store
            .replace(version, session)
            .map_err(ProviderError::from)?;
        Ok(())
    }

    async fn refresh_session(
        &self,
        session: ChatGptSession,
        version: CredentialVersion,
        force: bool,
    ) -> ProviderResult<(ChatGptSession, CredentialVersion)> {
        let should_refresh = force
            || session.tokens.expires_at().is_some_and(|expires_at| {
                expires_at <= Utc::now() + Duration::seconds(REFRESH_SKEW_SECONDS)
            });
        if !should_refresh {
            return Ok((session, version));
        }
        let mut session = session;
        let refresh_token =
            session
                .tokens
                .refresh_token()
                .ok_or_else(|| ProviderError::AuthenticationInvalid {
                    message: "OAuth token expired without a refresh token".into(),
                })?;
        let mut refreshed = match self.api.refresh_token(refresh_token).await {
            Ok(refreshed) => refreshed,
            Err(error) if error.kind == crate::ChatGptApiErrorKind::AuthenticationInvalid => {
                session.invalid_reason = Some(error.message.clone());
                self.persist_invalid_reason(version, &session)?;
                return Err(error.into());
            }
            Err(error) => return Err(error.into()),
        };
        if refreshed.refresh_token.is_none() {
            refreshed.refresh_token = session.tokens.refresh_token.clone();
        }
        if refreshed.identity_token.is_none() {
            refreshed.identity_token = session.tokens.identity_token.clone();
        }
        session.tokens = refreshed;
        session.invalid_reason = None;
        let version = self.save_observed(version, &session)?;
        Ok((session, version))
    }

    fn selected_workspace(
        &self,
        session: &ChatGptSession,
        requested: Option<&str>,
    ) -> ProviderResult<ChatGptWorkspace> {
        let requested = requested.or(session.selected_workspace_id.as_deref());
        if let Some(requested) = requested {
            return session
                .workspaces
                .iter()
                .find(|workspace| workspace.id == requested)
                .cloned()
                .ok_or_else(|| workspace_access_denied("selected workspace is unavailable"));
        }
        match session.workspaces.as_slice() {
            [only] => Ok(only.clone()),
            [] => Err(workspace_access_denied(
                "account has no available workspace",
            )),
            _ => Err(workspace_access_denied(
                "multiple workspaces are available; select one before querying usage",
            )),
        }
    }
}

impl<S> ChatGptProvider<crate::ReqwestChatGptApi, S>
where
    S: ChatGptSessionStore,
{
    pub fn openai(config: ChatGptConfig, store: Arc<S>) -> ProviderResult<Self> {
        let api = crate::ReqwestChatGptApi::new(crate::ChatGptHttpConfig::openai(
            config.client_id.clone(),
        ))
        .map_err(ProviderError::from)?;
        Self::new(config, Arc::new(api), store)
    }
}

#[async_trait]
impl<A, S> Provider for ChatGptProvider<A, S>
where
    A: ChatGptApi + 'static,
    S: ChatGptSessionStore + 'static,
{
    type VendorUsage = ChatGptUsage;

    fn descriptor(&self) -> ProviderDescriptor {
        ProviderDescriptor {
            id: ProviderId::new("chatgpt"),
            display_name: "ChatGPT".into(),
            capabilities: vec![
                Capability::Authentication,
                Capability::AuthenticationStatus,
                Capability::Logout,
                Capability::UsageQuery,
                Capability::WorkspaceSelection,
            ],
        }
    }

    async fn start_auth(&self, request: AuthStartRequest) -> ProviderResult<AuthChallenge> {
        if !matches!(request.method, None | Some(AuthMethod::BrowserOAuth)) {
            return Err(ProviderError::UnsupportedCapability {
                capability: "requested authentication method".into(),
            });
        }
        let flow_id = random_url_safe(32)?;
        let pkce_verifier = random_url_safe(64)?;
        let pkce_challenge = URL_SAFE_NO_PAD.encode(Sha256::digest(pkce_verifier.as_bytes()));
        let expires_at = Utc::now() + Duration::minutes(OAUTH_FLOW_LIFETIME_MINUTES);
        let redirect_uri = match request.redirect_uri.as_deref() {
            None => self.config.redirect_uri.clone(),
            Some(uri) if uri == self.config.redirect_uri => uri.to_owned(),
            // OpenAI registers loopback callbacks for this client, so a
            // desktop client may finish sign-in on a local listener.
            Some(uri) if validate_loopback_http_redirect_uri(uri).is_ok() => uri.to_owned(),
            Some(_) => {
                return Err(ProviderError::AuthenticationInvalid {
                    message: "OAuth redirect URI must be the registered one or a loopback HTTP URI"
                        .into(),
                });
            }
        };
        let authorization_url =
            oauth_authorization_url(&self.config, &redirect_uri, &flow_id, &pkce_challenge);
        let _session_guard = self.session_gate.lock().await;
        let mut pending = self.pending.lock().map_err(|_| ProviderError::Network {
            message: "ChatGPT provider state lock is poisoned".into(),
        })?;
        pending.clear();
        pending.insert(
            flow_id.clone(),
            PendingOAuth {
                pkce_verifier,
                redirect_uri,
                expires_at,
            },
        );
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
        let _session_guard = self.session_gate.lock().await;
        // A malformed callback must not consume the flow: the entry stays
        // until the exchange succeeds or the flow expires, so a mistyped
        // paste can be retried.
        let pending = {
            let mut flows = self.pending.lock().map_err(|_| ProviderError::Network {
                message: "ChatGPT provider state lock is poisoned".into(),
            })?;
            match flows.get(&request.flow_id) {
                None => {
                    return Err(ProviderError::AuthenticationInvalid {
                        message: "OAuth state is missing, invalid, or already used".into(),
                    });
                }
                Some(flow) if flow.expires_at <= Utc::now() => {
                    flows.remove(&request.flow_id);
                    return Err(ProviderError::AuthenticationInvalid {
                        message: "OAuth flow expired".into(),
                    });
                }
                Some(flow) => flow.clone(),
            }
        };
        if request
            .redirect_uri
            .as_deref()
            .is_some_and(|redirect_uri| redirect_uri != pending.redirect_uri)
        {
            return Err(ProviderError::AuthenticationInvalid {
                message: "OAuth redirect URI does not match the initiated flow".into(),
            });
        }
        let input = request.authorization_code.as_deref().ok_or_else(|| {
            ProviderError::AuthenticationInvalid {
                message: "OAuth callback omitted authorization code".into(),
            }
        })?;
        if input.len() > MAX_AUTHORIZATION_CODE_BYTES {
            return Err(ProviderError::AuthenticationInvalid {
                message: "OAuth authorization code has an invalid size".into(),
            });
        }
        let code = browser_authorization_code(input, &request.flow_id, &pending.redirect_uri)?;
        let tokens = self
            .api
            .exchange_code(&code, &pending.pkce_verifier, &pending.redirect_uri)
            .await
            .map_err(ProviderError::from)?;
        // The exchange consumed the one-shot code, so the flow is done: only
        // failures before this point leave it retryable.
        self.pending
            .lock()
            .map_err(|_| ProviderError::Network {
                message: "ChatGPT provider state lock is poisoned".into(),
            })?
            .remove(&request.flow_id);
        let mut grant = OAuthGrantGuard::new(self.api.clone(), tokens);
        let initialization = async {
            let workspaces = self
                .api
                .list_workspaces(grant.tokens())
                .await
                .map_err(ProviderError::from)?;
            validate_workspaces(&workspaces)?;
            let previous_session = self.store.load().map_err(ProviderError::from)?;
            let previous_selection = previous_session
                .as_ref()
                .and_then(|(session, _)| session.selected_workspace_id.clone());
            let selected_workspace_id = previous_selection
                .filter(|selected| workspaces.iter().any(|workspace| &workspace.id == selected))
                .or_else(|| (workspaces.len() == 1).then(|| workspaces[0].id.clone()));
            let session = ChatGptSession {
                tokens: grant.tokens().clone(),
                workspaces,
                selected_workspace_id,
                invalid_reason: None,
            };
            // A completed sign-in is an explicit re-login: the unconditional
            // save supersedes whatever session was stored before.
            self.store.save(&session).map_err(ProviderError::from)?;
            Ok::<_, ProviderError>((session, previous_session))
        };
        let (session, previous_session) = match initialization.await {
            Ok(initialized) => initialized,
            Err(error) => {
                grant.revoke_now().await;
                return Err(error);
            }
        };
        grant.disarm();
        if let Some(previous_tokens) = previous_session.map(|(session, _)| session.tokens) {
            if revocation_token(&previous_tokens) != revocation_token(&session.tokens) {
                let _ = self.api.revoke(&previous_tokens).await;
            }
        }
        Ok(authenticated_state(&session))
    }

    async fn auth_status(&self) -> ProviderResult<AuthState> {
        let _session_guard = self.session_gate.lock().await;
        let pending_state = {
            let mut pending = self.pending.lock().map_err(|_| ProviderError::Network {
                message: "ChatGPT provider state lock is poisoned".into(),
            })?;
            pending.retain(|_, flow| flow.expires_at > Utc::now());
            pending
                .iter()
                .next()
                .map(|(flow_id, flow)| AuthState::Pending {
                    flow_id: flow_id.clone(),
                    expires_at: Some(flow.expires_at),
                })
        };
        if let Some(state) = pending_state {
            return Ok(state);
        }
        let Some((session, version)) = self.store.load().map_err(ProviderError::from)? else {
            return Ok(AuthState::NotAuthenticated);
        };
        let account_key = session
            .selected_workspace_id
            .as_deref()
            .and_then(account_identity);
        if let Some(reason) = session.invalid_reason.clone() {
            return Ok(AuthState::Invalid {
                reason,
                account_key,
            });
        }
        match self.refresh_session(session, version, false).await {
            Ok((session, _)) => Ok(authenticated_state(&session)),
            Err(ProviderError::AuthenticationInvalid { message }) => Ok(AuthState::Invalid {
                reason: message,
                account_key,
            }),
            Err(error) => Err(error),
        }
    }

    async fn logout(&self, _: LogoutRequest) -> ProviderResult<()> {
        let _session_guard = self.session_gate.lock().await;
        self.pending
            .lock()
            .map_err(|_| ProviderError::Network {
                message: "ChatGPT provider state lock is poisoned".into(),
            })?
            .clear();
        let tokens = self
            .store
            .load()
            .map_err(ProviderError::from)?
            .map(|(session, _)| session.tokens);
        let Some(tokens) = tokens else {
            return Ok(());
        };
        // Revoke before clearing: a failed revocation keeps the stored session
        // so the logout can be retried with the same tokens. The guard clears
        // the store on drop so a cancelled revoke still completes the local
        // half of the logout.
        let clear_guard = SessionClearGuard::new(self.store.clone());
        match self.api.revoke(&tokens).await {
            Ok(()) => clear_guard.clear().map_err(ProviderError::from),
            Err(error) => {
                clear_guard.preserve();
                Err(error.into())
            }
        }
    }

    async fn list_workspaces(&self) -> ProviderResult<Vec<ProviderWorkspace>> {
        Ok(ChatGptProvider::workspaces(self)
            .await?
            .into_iter()
            .map(|workspace| ProviderWorkspace {
                id: workspace.id,
                label: workspace.label,
            })
            .collect())
    }

    async fn select_workspace(&self, workspace_id: &str) -> ProviderResult<ProviderWorkspace> {
        let workspace = ChatGptProvider::select_workspace(self, workspace_id).await?;
        Ok(ProviderWorkspace {
            id: workspace.id,
            label: workspace.label,
        })
    }

    async fn query(&self, request: UsageQuery) -> ProviderResult<QueryOutcome<Self::VendorUsage>> {
        let _session_guard = self.session_gate.lock().await;
        let (mut session, mut version) = self.load_refreshed_session().await?;
        let workspace_override = workspace_id_override(&session, request.account_label.as_deref());
        let mut workspace = self.selected_workspace(&session, workspace_override)?;
        if workspace_override.is_none()
            && session.selected_workspace_id.as_deref() != Some(&workspace.id)
        {
            session.selected_workspace_id = Some(workspace.id.clone());
            version = self.save_observed(version, &session)?;
        }
        let response = match self.api.query_usage(&session.tokens, &workspace.id).await {
            Ok(response) => response,
            Err(error) if error.kind == crate::ChatGptApiErrorKind::AuthenticationInvalid => {
                (session, version) = self.refresh_session(session, version, true).await?;
                let refreshed_workspaces = match self.api.list_workspaces(&session.tokens).await {
                    Ok(workspaces) => workspaces,
                    Err(error)
                        if error.kind == crate::ChatGptApiErrorKind::AuthenticationInvalid =>
                    {
                        session.invalid_reason = Some(error.message.clone());
                        self.persist_invalid_reason(version, &session)?;
                        return Err(error.into());
                    }
                    Err(error) => return Err(error.into()),
                };
                validate_workspaces(&refreshed_workspaces)?;
                let refreshed_workspace = refreshed_workspaces
                    .iter()
                    .find(|candidate| candidate.id == workspace.id)
                    .cloned();
                if session
                    .selected_workspace_id
                    .as_ref()
                    .is_some_and(|selected| {
                        !refreshed_workspaces
                            .iter()
                            .any(|candidate| &candidate.id == selected)
                    })
                {
                    session.selected_workspace_id = None;
                }
                session.workspaces = refreshed_workspaces;
                version = self.save_observed(version, &session)?;
                let Some(refreshed_workspace) = refreshed_workspace else {
                    return Err(workspace_access_denied(
                        "selected workspace is unavailable after token refresh",
                    ));
                };
                workspace = refreshed_workspace;
                match self.api.query_usage(&session.tokens, &workspace.id).await {
                    Ok(response) => response,
                    Err(error)
                        if error.kind == crate::ChatGptApiErrorKind::AuthenticationInvalid =>
                    {
                        session.invalid_reason = Some(error.message.clone());
                        self.persist_invalid_reason(version, &session)?;
                        return Err(error.into());
                    }
                    Err(error) => return Err(error.into()),
                }
            }
            Err(error) => return Err(error.into()),
        };
        Ok(QueryOutcome::Complete {
            data: ChatGptUsage {
                workspace,
                observed_at: Utc::now(),
                response,
            },
        })
    }

    fn normalize(&self, vendor_usage: Self::VendorUsage) -> ProviderResult<SubscriptionUsage> {
        vendor_usage.normalize()
    }
}

fn authenticated_state(session: &ChatGptSession) -> AuthState {
    let account_label = session.selected_workspace_id.as_ref().and_then(|selected| {
        session
            .workspaces
            .iter()
            .find(|workspace| &workspace.id == selected)
            .and_then(|workspace| {
                workspace
                    .label
                    .clone()
                    .filter(|label| !label.trim().is_empty())
                    .or_else(|| Some(workspace.id.clone()))
            })
    });
    AuthState::Authenticated {
        account_label,
        // The label above is a workspace name, which two different accounts can
        // share; the workspace ID is what actually identifies this sign-in.
        account_key: session
            .selected_workspace_id
            .as_deref()
            .and_then(account_identity),
        expires_at: session.tokens.expires_at(),
    }
}

fn revocation_token(tokens: &OAuthTokenSet) -> &str {
    tokens
        .refresh_token()
        .unwrap_or_else(|| tokens.access_token())
}

fn validate_workspaces(workspaces: &[ChatGptWorkspace]) -> ProviderResult<()> {
    if workspaces.iter().any(|workspace| workspace.id.is_empty()) {
        return Err(ProviderError::ProtocolIncompatible {
            message: "workspace response contains an empty ID".into(),
        });
    }
    let mut ids = workspaces
        .iter()
        .map(|workspace| workspace.id.as_str())
        .collect::<Vec<_>>();
    ids.sort_unstable();
    if ids.windows(2).any(|pair| pair[0] == pair[1]) {
        return Err(ProviderError::ProtocolIncompatible {
            message: "workspace response contains duplicate IDs".into(),
        });
    }
    Ok(())
}

fn workspace_access_denied(message: impl Into<String>) -> ProviderError {
    ProviderError::AuthenticationInvalid {
        message: format!("workspace access denied: {}", message.into()),
    }
}

fn random_url_safe(byte_count: usize) -> ProviderResult<String> {
    let mut bytes = vec![0_u8; byte_count];
    getrandom::fill(&mut bytes).map_err(|error| ProviderError::ProtocolIncompatible {
        message: format!("secure random source unavailable: {error}"),
    })?;
    Ok(URL_SAFE_NO_PAD.encode(bytes))
}

fn valid_authorization_endpoint(endpoint: &str) -> bool {
    reqwest::Url::parse(endpoint).is_ok_and(|url| {
        url.scheme() == "https"
            && url.host_str().is_some()
            && url.username().is_empty()
            && url.password().is_none()
            && url.fragment().is_none()
    })
}

/// The authorization code a pasted callback carries, checked against the
/// pending flow. Accepted shapes: the whole callback URL, whose origin and
/// path must equal the flow's `redirect_uri`, or `code#state`. Both carry
/// the `state`, which must equal the flow ID; a bare code proves nothing
/// about which flow it belongs to and is rejected.
fn browser_authorization_code(
    input: &str,
    flow_id: &str,
    expected_redirect_uri: &str,
) -> ProviderResult<String> {
    let input = input.trim();
    if input.is_empty() {
        return Err(ProviderError::AuthenticationInvalid {
            message: "OAuth callback omitted authorization code".into(),
        });
    }
    let (code, state) = if input.contains("://") {
        let url = reqwest::Url::parse(input).map_err(|_| ProviderError::AuthenticationInvalid {
            message: "OAuth callback URL is invalid".into(),
        })?;
        let expected = reqwest::Url::parse(expected_redirect_uri).map_err(|_| {
            ProviderError::ProtocolIncompatible {
                message: "OAuth expected redirect URI is invalid".into(),
            }
        })?;
        if url.scheme() != expected.scheme()
            || url.host_str() != expected.host_str()
            || url.port_or_known_default() != expected.port_or_known_default()
            || url.path() != expected.path()
        {
            return Err(ProviderError::AuthenticationInvalid {
                message: "OAuth callback URL has an unexpected origin or path".into(),
            });
        }
        let code = url
            .query_pairs()
            .find(|(key, _)| key == "code")
            .map(|(_, value)| value.into_owned())
            .filter(|value| !value.is_empty())
            .ok_or_else(|| ProviderError::AuthenticationInvalid {
                message: "OAuth callback omitted authorization code".into(),
            })?;
        let state = url
            .query_pairs()
            .find(|(key, _)| key == "state")
            .map(|(_, value)| value.into_owned())
            .filter(|value| !value.is_empty())
            .ok_or_else(|| ProviderError::AuthenticationInvalid {
                message: "OAuth callback omitted the state".into(),
            })?;
        (code, state)
    } else if let Some((code, state)) = input.split_once('#') {
        if code.is_empty() || state.is_empty() {
            return Err(ProviderError::AuthenticationInvalid {
                message: "OAuth callback code or state is empty".into(),
            });
        }
        (code.to_owned(), state.to_owned())
    } else {
        return Err(ProviderError::AuthenticationInvalid {
            message: "OAuth callback must be the full URL or code#state".into(),
        });
    };
    if state != flow_id {
        return Err(ProviderError::AuthenticationInvalid {
            message: "OAuth callback state does not match".into(),
        });
    }
    Ok(code)
}

fn valid_redirect_uri(redirect_uri: &str) -> bool {
    reqwest::Url::parse(redirect_uri).is_ok_and(|url| {
        let local_http = url.scheme() == "http"
            && url.host_str().is_some_and(|host| {
                host == "localhost"
                    || host
                        .parse::<std::net::IpAddr>()
                        .is_ok_and(|ip| ip.is_loopback())
            });
        (url.scheme() == "https" || local_http)
            && url.username().is_empty()
            && url.password().is_none()
            && url.fragment().is_none()
    })
}

// Daemon UsageQuery.account_label is the account display name. Treat it as a
// workspace override only when it matches a known workspace ID.
fn workspace_id_override<'a>(
    session: &ChatGptSession,
    requested: Option<&'a str>,
) -> Option<&'a str> {
    requested.filter(|requested| {
        session
            .workspaces
            .iter()
            .any(|workspace| workspace.id == *requested)
    })
}

fn oauth_authorization_url(
    config: &ChatGptConfig,
    redirect_uri: &str,
    state: &str,
    challenge: &str,
) -> String {
    let separator = if config.authorization_endpoint.contains('?') {
        '&'
    } else {
        '?'
    };
    let parameters = [
        ("response_type", "code".to_owned()),
        ("client_id", config.client_id.clone()),
        ("redirect_uri", redirect_uri.to_owned()),
        ("scope", config.scopes.join(" ")),
        ("state", state.to_owned()),
        ("code_challenge", challenge.to_owned()),
        ("code_challenge_method", "S256".to_owned()),
        ("id_token_add_organizations", "true".to_owned()),
        ("codex_cli_simplified_flow", "true".to_owned()),
        ("originator", "ullage".to_owned()),
    ];
    let query = parameters
        .into_iter()
        .map(|(name, value)| format!("{name}={}", encode_query_component(&value)))
        .collect::<Vec<_>>()
        .join("&");
    format!("{}{separator}{query}", config.authorization_endpoint)
}

fn encode_query_component(value: &str) -> String {
    const HEX: &[u8; 16] = b"0123456789ABCDEF";
    let mut encoded = String::with_capacity(value.len());
    for byte in value.bytes() {
        if byte.is_ascii_alphanumeric() || matches!(byte, b'-' | b'.' | b'_' | b'~') {
            encoded.push(char::from(byte));
        } else {
            encoded.push('%');
            encoded.push(char::from(HEX[usize::from(byte >> 4)]));
            encoded.push(char::from(HEX[usize::from(byte & 0x0f)]));
        }
    }
    encoded
}
