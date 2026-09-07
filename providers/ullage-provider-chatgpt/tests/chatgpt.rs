use std::collections::VecDeque;
use std::future::Future;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};
use std::task::{Context, Poll, Waker};

use async_trait::async_trait;
use chrono::{Duration, TimeZone, Utc};
use ullage_auth::{AuthCompleteRequest, AuthMethod, AuthStartRequest, AuthState, LogoutRequest};
use ullage_core::{Provider, ProviderError, QueryOutcome, UsageQuery, UsageWindowKind};
use ullage_provider_chatgpt::{
    ChatGptApi, ChatGptApiError, ChatGptApiErrorKind, ChatGptConfig, ChatGptProvider, ChatGptUsage,
    ChatGptUsageResponse, ChatGptWorkspace, MemorySessionStore, OAuthTokenSet,
};

#[derive(Default)]
struct ApiCalls {
    exchanges: usize,
    exchange_redirect_uris: Vec<String>,
    refreshes: usize,
    queries: Vec<String>,
    revocations: usize,
    revoked_tokens: Vec<String>,
}

struct FakeApi {
    issued_tokens: Mutex<OAuthTokenSet>,
    refreshed_tokens: Mutex<OAuthTokenSet>,
    workspaces: Mutex<Vec<ChatGptWorkspace>>,
    response: ChatGptUsageResponse,
    workspace_errors: Mutex<VecDeque<ChatGptApiError>>,
    query_errors: Mutex<VecDeque<ChatGptApiError>>,
    pause_workspace: AtomicBool,
    workspace_started: tokio::sync::Notify,
    pause_refresh: AtomicBool,
    refresh_started: tokio::sync::Notify,
    release_refresh: tokio::sync::Notify,
    pause_revoke: AtomicBool,
    revoke_started: tokio::sync::Notify,
    revoke_called: tokio::sync::Notify,
    calls: Mutex<ApiCalls>,
}

#[async_trait]
impl ChatGptApi for FakeApi {
    async fn exchange_code(
        &self,
        authorization_code: &str,
        pkce_verifier: &str,
        redirect_uri: &str,
    ) -> Result<OAuthTokenSet, ChatGptApiError> {
        assert_eq!(authorization_code, "authorization-code");
        assert!(pkce_verifier.len() >= 43);
        let mut calls = self.calls.lock().unwrap();
        calls.exchanges += 1;
        calls.exchange_redirect_uris.push(redirect_uri.to_owned());
        Ok(self.issued_tokens.lock().unwrap().clone())
    }

    async fn refresh_token(&self, refresh_token: &str) -> Result<OAuthTokenSet, ChatGptApiError> {
        assert_eq!(refresh_token, "refresh-secret");
        self.calls.lock().unwrap().refreshes += 1;
        if self.pause_refresh.swap(false, Ordering::SeqCst) {
            self.refresh_started.notify_one();
            self.release_refresh.notified().await;
        }
        Ok(self.refreshed_tokens.lock().unwrap().clone())
    }

    async fn list_workspaces(
        &self,
        _: &OAuthTokenSet,
    ) -> Result<Vec<ChatGptWorkspace>, ChatGptApiError> {
        if self.pause_workspace.swap(false, Ordering::SeqCst) {
            self.workspace_started.notify_one();
            std::future::pending::<()>().await;
        }
        if let Some(error) = self.workspace_errors.lock().unwrap().pop_front() {
            return Err(error);
        }
        Ok(self.workspaces.lock().unwrap().clone())
    }

    async fn query_usage(
        &self,
        _: &OAuthTokenSet,
        workspace_id: &str,
    ) -> Result<ChatGptUsageResponse, ChatGptApiError> {
        self.calls
            .lock()
            .unwrap()
            .queries
            .push(workspace_id.to_owned());
        if let Some(error) = self.query_errors.lock().unwrap().pop_front() {
            return Err(error);
        }
        Ok(self.response.clone())
    }

    async fn revoke(&self, tokens: &OAuthTokenSet) -> Result<(), ChatGptApiError> {
        {
            let mut calls = self.calls.lock().unwrap();
            calls.revocations += 1;
            calls.revoked_tokens.push(
                tokens
                    .refresh_token()
                    .unwrap_or_else(|| tokens.access_token())
                    .to_owned(),
            );
        }
        self.revoke_called.notify_one();
        if self.pause_revoke.swap(false, Ordering::SeqCst) {
            self.revoke_started.notify_one();
            std::future::pending::<()>().await;
        }
        Ok(())
    }
}

fn run_ready<F: Future>(future: F) -> F::Output {
    let mut context = Context::from_waker(Waker::noop());
    let mut future = Box::pin(future);
    match future.as_mut().poll(&mut context) {
        Poll::Ready(output) => output,
        Poll::Pending => panic!("fake API future unexpectedly yielded"),
    }
}

fn fixture(name: &str) -> ChatGptUsageResponse {
    let json = match name {
        "single" => include_str!("fixtures/single-window.json"),
        "multi" => include_str!("fixtures/multi-window.json"),
        "unknown" => include_str!("fixtures/unknown-window.json"),
        "missing-plan" => include_str!("fixtures/missing-plan.json"),
        _ => panic!("unknown fixture"),
    };
    serde_json::from_str(json).unwrap()
}

fn tokens(expires_at: Option<chrono::DateTime<Utc>>) -> OAuthTokenSet {
    OAuthTokenSet::new("access-secret", Some("refresh-secret".into()), expires_at).unwrap()
}

fn provider(
    workspaces: Vec<ChatGptWorkspace>,
    response: ChatGptUsageResponse,
    expires_at: Option<chrono::DateTime<Utc>>,
) -> (ChatGptProvider<FakeApi, MemorySessionStore>, Arc<FakeApi>) {
    let api = Arc::new(FakeApi {
        issued_tokens: Mutex::new(tokens(expires_at)),
        refreshed_tokens: Mutex::new(tokens(Some(Utc::now() + Duration::hours(1)))),
        workspaces: Mutex::new(workspaces),
        response,
        workspace_errors: Mutex::new(VecDeque::new()),
        query_errors: Mutex::new(VecDeque::new()),
        pause_workspace: AtomicBool::new(false),
        workspace_started: tokio::sync::Notify::new(),
        pause_refresh: AtomicBool::new(false),
        refresh_started: tokio::sync::Notify::new(),
        release_refresh: tokio::sync::Notify::new(),
        pause_revoke: AtomicBool::new(false),
        revoke_started: tokio::sync::Notify::new(),
        revoke_called: tokio::sync::Notify::new(),
        calls: Mutex::new(ApiCalls::default()),
    });
    let provider = ChatGptProvider::new(
        ChatGptConfig {
            authorization_endpoint: "https://auth.example.test/authorize".into(),
            client_id: "public client".into(),
            redirect_uri: "http://127.0.0.1:1455/callback".into(),
            scopes: vec!["openid".into(), "profile".into()],
        },
        api.clone(),
        Arc::new(MemorySessionStore::default()),
    )
    .unwrap();
    (provider, api)
}

fn complete_auth(provider: &ChatGptProvider<FakeApi, MemorySessionStore>) -> AuthState {
    let challenge = run_ready(provider.start_auth(AuthStartRequest {
        method: Some(AuthMethod::BrowserOAuth),
        redirect_uri: None,
    }))
    .unwrap();
    let uri = challenge.verification_uri.as_ref().unwrap();
    assert!(uri.contains("client_id=public%20client"));
    assert!(uri.contains(&format!("state={}", challenge.flow_id)));
    assert!(uri.contains("code_challenge_method=S256"));
    run_ready(provider.complete_auth(AuthCompleteRequest {
        flow_id: challenge.flow_id,
        authorization_code: Some("authorization-code".into()),
        redirect_uri: Some("http://127.0.0.1:1455/callback".into()),
    }))
    .unwrap()
}

#[test]
fn browser_oauth_uses_per_flow_redirect_uri() {
    let (provider, api) = provider(
        vec![workspace("ws-one", "Personal")],
        fixture("single"),
        Some(Utc::now() + Duration::hours(1)),
    );
    let custom = "http://127.0.0.1:54321/auth/callback";
    let challenge = run_ready(provider.start_auth(AuthStartRequest {
        method: Some(AuthMethod::BrowserOAuth),
        redirect_uri: Some(custom.into()),
    }))
    .unwrap();
    let uri = challenge.verification_uri.as_ref().unwrap();
    assert!(uri.contains("redirect_uri=http%3A%2F%2F127.0.0.1%3A54321%2Fauth%2Fcallback"));
    run_ready(provider.complete_auth(AuthCompleteRequest {
        flow_id: challenge.flow_id,
        authorization_code: Some("authorization-code".into()),
        redirect_uri: Some(custom.into()),
    }))
    .unwrap();
    assert_eq!(api.calls.lock().unwrap().exchange_redirect_uris.len(), 1);
    assert_eq!(api.calls.lock().unwrap().exchange_redirect_uris[0], custom);
}

fn workspace(id: &str, label: &str) -> ChatGptWorkspace {
    ChatGptWorkspace {
        id: id.into(),
        label: Some(label.into()),
    }
}

#[test]
fn oauth_is_single_use_and_persists_single_workspace() {
    let (provider, api) = provider(
        vec![workspace("ws-one", "Personal")],
        fixture("single"),
        Some(Utc::now() + Duration::hours(1)),
    );
    let challenge = run_ready(provider.start_auth(AuthStartRequest {
        method: None,
        redirect_uri: None,
    }))
    .unwrap();
    let request = AuthCompleteRequest {
        flow_id: challenge.flow_id,
        authorization_code: Some("authorization-code".into()),
        redirect_uri: None,
    };
    let state = run_ready(provider.complete_auth(request.clone())).unwrap();
    assert!(matches!(
        state,
        AuthState::Authenticated { account_label: Some(label), .. } if label == "Personal"
    ));
    assert!(matches!(
        run_ready(provider.complete_auth(request)),
        Err(ProviderError::AuthenticationInvalid { .. })
    ));
    assert_eq!(api.calls.lock().unwrap().exchanges, 1);
}

#[test]
fn reports_the_single_pending_oauth_flow() {
    let (provider, _) = provider(
        vec![workspace("ws-one", "Personal")],
        fixture("single"),
        Some(Utc::now() + Duration::hours(1)),
    );
    let first = run_ready(provider.start_auth(AuthStartRequest {
        method: None,
        redirect_uri: None,
    }))
    .unwrap();
    assert!(matches!(
        run_ready(provider.auth_status()).unwrap(),
        AuthState::Pending { flow_id, .. } if flow_id == first.flow_id
    ));
    let second = run_ready(provider.start_auth(AuthStartRequest {
        method: None,
        redirect_uri: None,
    }))
    .unwrap();
    assert!(matches!(
        run_ready(provider.auth_status()).unwrap(),
        AuthState::Pending { flow_id, .. } if flow_id == second.flow_id
    ));
    assert!(matches!(
        run_ready(provider.complete_auth(AuthCompleteRequest {
            flow_id: first.flow_id,
            authorization_code: Some("authorization-code".into()),
            redirect_uri: None,
        })),
        Err(ProviderError::AuthenticationInvalid { .. })
    ));
    run_ready(provider.complete_auth(AuthCompleteRequest {
        flow_id: second.flow_id,
        authorization_code: Some("authorization-code".into()),
        redirect_uri: None,
    }))
    .unwrap();
    let relogin = run_ready(provider.start_auth(AuthStartRequest {
        method: None,
        redirect_uri: None,
    }))
    .unwrap();
    assert!(matches!(
        run_ready(provider.auth_status()).unwrap(),
        AuthState::Pending { flow_id, .. } if flow_id == relogin.flow_id
    ));
}

#[test]
fn blank_workspace_label_falls_back_to_workspace_id() {
    let (provider, _) = provider(
        vec![workspace("ws-one", "   ")],
        fixture("single"),
        Some(Utc::now() + Duration::hours(1)),
    );
    assert!(matches!(
        complete_auth(&provider),
        AuthState::Authenticated { account_label: Some(label), .. } if label == "ws-one"
    ));
}

#[test]
fn rejects_mismatched_callback_redirect_before_exchange() {
    let (provider, api) = provider(
        vec![workspace("ws-one", "Personal")],
        fixture("single"),
        Some(Utc::now() + Duration::hours(1)),
    );
    let challenge = run_ready(provider.start_auth(AuthStartRequest {
        method: None,
        redirect_uri: None,
    }))
    .unwrap();
    let result = run_ready(provider.complete_auth(AuthCompleteRequest {
        flow_id: challenge.flow_id,
        authorization_code: Some("authorization-code".into()),
        redirect_uri: Some("http://attacker.invalid/callback".into()),
    }));
    assert!(matches!(
        result,
        Err(ProviderError::AuthenticationInvalid { .. })
    ));
    assert_eq!(api.calls.lock().unwrap().exchanges, 0);
}

#[test]
fn revokes_new_token_when_workspace_initialization_fails() {
    let (provider, api) = provider(
        vec![workspace("ws-one", "Personal")],
        fixture("single"),
        Some(Utc::now() + Duration::hours(1)),
    );
    let challenge = run_ready(provider.start_auth(AuthStartRequest {
        method: None,
        redirect_uri: None,
    }))
    .unwrap();
    api.workspace_errors
        .lock()
        .unwrap()
        .push_back(ChatGptApiError::new(
            ChatGptApiErrorKind::ProtocolIncompatible,
            "workspace response is incompatible",
        ));

    assert!(matches!(
        run_ready(provider.complete_auth(AuthCompleteRequest {
            flow_id: challenge.flow_id,
            authorization_code: Some("authorization-code".into()),
            redirect_uri: None,
        })),
        Err(ProviderError::ProtocolIncompatible { .. })
    ));
    let calls = api.calls.lock().unwrap();
    assert_eq!(calls.revocations, 1);
    assert_eq!(calls.revoked_tokens, vec!["refresh-secret"]);
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn cancelled_workspace_initialization_revokes_the_new_token() {
    let (provider, api) = provider(
        vec![workspace("ws-one", "Personal")],
        fixture("single"),
        Some(Utc::now() + Duration::hours(1)),
    );
    let challenge = provider
        .start_auth(AuthStartRequest {
            method: None,
            redirect_uri: None,
        })
        .await
        .unwrap();
    let provider = Arc::new(provider);
    api.pause_workspace.store(true, Ordering::SeqCst);

    let authenticating_provider = provider.clone();
    let login = tokio::spawn(async move {
        authenticating_provider
            .complete_auth(AuthCompleteRequest {
                flow_id: challenge.flow_id,
                authorization_code: Some("authorization-code".into()),
                redirect_uri: None,
            })
            .await
    });
    api.workspace_started.notified().await;
    login.abort();
    assert!(login.await.unwrap_err().is_cancelled());
    api.revoke_called.notified().await;

    assert_eq!(
        api.calls.lock().unwrap().revoked_tokens,
        vec!["refresh-secret"]
    );
    assert_eq!(
        provider.auth_status().await.unwrap(),
        AuthState::NotAuthenticated
    );
}

#[test]
fn requires_selection_for_multiple_workspaces_and_persists_it() {
    let (provider, api) = provider(
        vec![workspace("ws-a", "A"), workspace("ws-b", "B")],
        fixture("single"),
        Some(Utc::now() + Duration::hours(1)),
    );
    complete_auth(&provider);
    assert!(matches!(
        run_ready(provider.query(UsageQuery::default())),
        Err(ProviderError::AuthenticationInvalid { message })
            if message.contains("workspace access denied")
    ));
    run_ready(provider.select_workspace("ws-b")).unwrap();
    let outcome = run_ready(provider.query(UsageQuery::default())).unwrap();
    assert!(matches!(outcome, QueryOutcome::Complete { .. }));
    assert_eq!(api.calls.lock().unwrap().queries, vec!["ws-b"]);
}

#[test]
fn workspace_query_override_accepts_ids_only() {
    let (provider, api) = provider(
        vec![workspace("ws-a", "Shared"), workspace("ws-b", "Shared")],
        fixture("single"),
        Some(Utc::now() + Duration::hours(1)),
    );
    complete_auth(&provider);
    run_ready(provider.select_workspace("ws-a")).unwrap();
    run_ready(provider.query(UsageQuery {
        account_label: Some("Shared".into()),
    }))
    .unwrap();
    run_ready(provider.query(UsageQuery {
        account_label: Some("ws-b".into()),
    }))
    .unwrap();
    run_ready(provider.query(UsageQuery::default())).unwrap();
    assert_eq!(
        api.calls.lock().unwrap().queries,
        vec!["ws-a", "ws-b", "ws-a"]
    );
}

#[test]
fn query_ignores_account_display_labels_that_are_not_workspace_ids() {
    let (provider, api) = provider(
        vec![workspace("ws-one", "Personal")],
        fixture("single"),
        Some(Utc::now() + Duration::hours(1)),
    );
    complete_auth(&provider);
    run_ready(provider.query(UsageQuery {
        account_label: Some("account-2".into()),
    }))
    .unwrap();
    assert_eq!(api.calls.lock().unwrap().queries, vec!["ws-one"]);
}

#[test]
fn refreshes_expiring_tokens_without_treating_expiry_as_subscription_expiry() {
    let (provider, api) = provider(
        vec![workspace("ws-one", "Personal")],
        fixture("missing-plan"),
        Some(Utc::now() - Duration::seconds(1)),
    );
    complete_auth(&provider);
    let outcome = run_ready(provider.query(UsageQuery::default())).unwrap();
    let QueryOutcome::Complete { data } = outcome else {
        panic!("expected complete usage");
    };
    let normalized = provider.normalize(data).unwrap();
    assert_eq!(normalized.subscription_expires_at, None);
    assert_eq!(normalized.plan, None);
    assert_eq!(api.calls.lock().unwrap().refreshes, 1);
}

#[test]
fn refreshes_and_retries_once_after_unexpected_authentication_failure() {
    let (provider, api) = provider(
        vec![workspace("ws-one", "Personal")],
        fixture("single"),
        None,
    );
    complete_auth(&provider);
    api.query_errors
        .lock()
        .unwrap()
        .push_back(ChatGptApiError::new(
            ChatGptApiErrorKind::AuthenticationInvalid,
            "access token rejected",
        ));
    run_ready(provider.query(UsageQuery::default())).unwrap();
    let calls = api.calls.lock().unwrap();
    assert_eq!(calls.refreshes, 1);
    assert_eq!(calls.queries, vec!["ws-one", "ws-one"]);
}

#[test]
fn marks_session_invalid_when_refreshed_token_is_also_rejected() {
    let (provider, api) = provider(
        vec![workspace("ws-one", "Personal")],
        fixture("single"),
        None,
    );
    complete_auth(&provider);
    api.query_errors.lock().unwrap().extend([
        ChatGptApiError::new(
            ChatGptApiErrorKind::AuthenticationInvalid,
            "old access token rejected",
        ),
        ChatGptApiError::new(
            ChatGptApiErrorKind::AuthenticationInvalid,
            "refreshed access token rejected",
        ),
    ]);
    assert!(matches!(
        run_ready(provider.query(UsageQuery::default())),
        Err(ProviderError::AuthenticationInvalid { .. })
    ));
    assert!(matches!(
        run_ready(provider.auth_status()).unwrap(),
        AuthState::Invalid { reason, .. } if reason == "refreshed access token rejected"
    ));
}

#[test]
fn marks_session_invalid_when_refreshed_workspace_request_is_rejected() {
    let (provider, api) = provider(
        vec![workspace("ws-one", "Personal")],
        fixture("single"),
        None,
    );
    complete_auth(&provider);
    api.query_errors
        .lock()
        .unwrap()
        .push_back(ChatGptApiError::new(
            ChatGptApiErrorKind::AuthenticationInvalid,
            "old access token rejected",
        ));
    api.workspace_errors
        .lock()
        .unwrap()
        .push_back(ChatGptApiError::new(
            ChatGptApiErrorKind::AuthenticationInvalid,
            "refreshed workspace token rejected",
        ));
    assert!(matches!(
        run_ready(provider.query(UsageQuery::default())),
        Err(ProviderError::AuthenticationInvalid { .. })
    ));
    assert!(matches!(
        run_ready(provider.auth_status()).unwrap(),
        AuthState::Invalid { reason, .. } if reason == "refreshed workspace token rejected"
    ));
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn logout_cannot_be_overwritten_by_an_in_flight_refresh() {
    let (provider, api) = provider(
        vec![workspace("ws-one", "Personal")],
        fixture("single"),
        None,
    );
    complete_auth(&provider);
    let provider = Arc::new(provider);
    api.pause_refresh.store(true, Ordering::SeqCst);

    let refreshing_provider = provider.clone();
    let refresh = tokio::spawn(async move { refreshing_provider.refresh_auth().await });
    api.refresh_started.notified().await;
    let logging_out_provider = provider.clone();
    let logout =
        tokio::spawn(async move { logging_out_provider.logout(LogoutRequest::default()).await });
    tokio::task::yield_now().await;
    api.release_refresh.notify_one();

    refresh.await.unwrap().unwrap();
    logout.await.unwrap().unwrap();
    assert_eq!(api.calls.lock().unwrap().refreshes, 1);
    assert_eq!(api.calls.lock().unwrap().revocations, 1);
    assert_eq!(
        provider.auth_status().await.unwrap(),
        AuthState::NotAuthenticated
    );
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn new_login_cannot_be_overwritten_by_an_in_flight_refresh() {
    let (provider, api) = provider(vec![workspace("ws-old", "Old")], fixture("single"), None);
    complete_auth(&provider);
    let challenge = provider
        .start_auth(AuthStartRequest {
            method: None,
            redirect_uri: None,
        })
        .await
        .unwrap();
    let provider = Arc::new(provider);
    api.pause_refresh.store(true, Ordering::SeqCst);

    let refreshing_provider = provider.clone();
    let refresh = tokio::spawn(async move { refreshing_provider.refresh_auth().await });
    api.refresh_started.notified().await;
    *api.workspaces.lock().unwrap() = vec![workspace("ws-new", "New")];
    let authenticating_provider = provider.clone();
    let login = tokio::spawn(async move {
        authenticating_provider
            .complete_auth(AuthCompleteRequest {
                flow_id: challenge.flow_id,
                authorization_code: Some("authorization-code".into()),
                redirect_uri: None,
            })
            .await
    });
    tokio::task::yield_now().await;
    api.release_refresh.notify_one();

    refresh.await.unwrap().unwrap();
    assert!(matches!(
        login.await.unwrap().unwrap(),
        AuthState::Authenticated { account_label: Some(label), .. } if label == "New"
    ));
    assert!(matches!(
        provider.auth_status().await.unwrap(),
        AuthState::Authenticated { account_label: Some(label), .. } if label == "New"
    ));
    assert_eq!(api.calls.lock().unwrap().revocations, 0);
}

#[test]
fn new_login_best_effort_revokes_a_different_old_token() {
    let (provider, api) = provider(
        vec![workspace("ws-one", "Personal")],
        fixture("single"),
        Some(Utc::now() + Duration::hours(1)),
    );
    complete_auth(&provider);
    *api.issued_tokens.lock().unwrap() = OAuthTokenSet::new(
        "new-access",
        Some("new-refresh".into()),
        Some(Utc::now() + Duration::hours(1)),
    )
    .unwrap();

    complete_auth(&provider);
    let calls = api.calls.lock().unwrap();
    assert_eq!(calls.revocations, 1);
    assert_eq!(calls.revoked_tokens, vec!["refresh-secret"]);
}

#[test]
fn empty_rotated_refresh_token_preserves_the_previous_token() {
    let (provider, api) = provider(
        vec![workspace("ws-one", "Personal")],
        fixture("single"),
        None,
    );
    complete_auth(&provider);
    *api.refreshed_tokens.lock().unwrap() = OAuthTokenSet::new(
        "rotated-access",
        Some(String::new()),
        Some(Utc::now() + Duration::hours(1)),
    )
    .unwrap();

    run_ready(provider.refresh_auth()).unwrap();
    run_ready(provider.refresh_auth()).unwrap();
    assert_eq!(api.calls.lock().unwrap().refreshes, 2);
}

#[test]
fn clears_removed_workspace_after_usage_refresh() {
    let (provider, api) = provider(vec![workspace("ws-old", "Old")], fixture("single"), None);
    complete_auth(&provider);
    *api.workspaces.lock().unwrap() = vec![workspace("ws-new", "New")];
    api.query_errors
        .lock()
        .unwrap()
        .push_back(ChatGptApiError::new(
            ChatGptApiErrorKind::AuthenticationInvalid,
            "old access token rejected",
        ));

    assert!(matches!(
        run_ready(provider.query(UsageQuery::default())),
        Err(ProviderError::AuthenticationInvalid { message })
            if message.contains("workspace access denied")
    ));
    assert!(matches!(
        run_ready(provider.auth_status()).unwrap(),
        AuthState::Authenticated {
            account_label: None,
            ..
        }
    ));
    run_ready(provider.query(UsageQuery::default())).unwrap();
    assert_eq!(api.calls.lock().unwrap().queries, vec!["ws-old", "ws-new"]);
}

#[test]
fn returns_refreshed_workspace_metadata_after_usage_retry() {
    let (provider, api) = provider(vec![workspace("ws-one", "Old")], fixture("single"), None);
    complete_auth(&provider);
    *api.workspaces.lock().unwrap() = vec![workspace("ws-one", "New")];
    api.query_errors
        .lock()
        .unwrap()
        .push_back(ChatGptApiError::new(
            ChatGptApiErrorKind::AuthenticationInvalid,
            "old access token rejected",
        ));

    let outcome = run_ready(provider.query(UsageQuery::default())).unwrap();
    let QueryOutcome::Complete { data } = outcome else {
        panic!("expected complete usage");
    };
    assert_eq!(
        provider.normalize(data).unwrap().account_label.as_deref(),
        Some("New")
    );
    assert!(matches!(
        run_ready(provider.auth_status()).unwrap(),
        AuthState::Authenticated { account_label: Some(label), .. } if label == "New"
    ));
    assert_eq!(api.calls.lock().unwrap().queries, vec!["ws-one", "ws-one"]);
}

#[test]
fn classifies_api_failures_without_exposing_tokens() {
    let (provider, api) = provider(
        vec![workspace("ws-one", "Personal")],
        fixture("single"),
        Some(Utc::now() + Duration::hours(1)),
    );
    complete_auth(&provider);
    api.query_errors
        .lock()
        .unwrap()
        .push_back(ChatGptApiError::rate_limited("slow down", Some(30)));
    assert!(matches!(
        run_ready(provider.query(UsageQuery::default())),
        Err(ProviderError::RateLimited {
            retry_after_seconds: Some(30),
            ..
        })
    ));
    let token_debug = format!("{:?}", tokens(Some(Utc::now())));
    assert!(!token_debug.contains("access-secret"));
    assert!(!token_debug.contains("refresh-secret"));

    let denied: ProviderError =
        ChatGptApiError::new(ChatGptApiErrorKind::WorkspaceAccessDenied, "not a member").into();
    assert!(matches!(
        denied,
        ProviderError::AuthenticationInvalid { message }
            if message.contains("workspace access denied")
    ));

    let authentication: ProviderError =
        ChatGptApiError::new(ChatGptApiErrorKind::AuthenticationInvalid, "expired").into();
    assert!(matches!(
        authentication,
        ProviderError::AuthenticationInvalid { .. }
    ));
    let network: ProviderError =
        ChatGptApiError::new(ChatGptApiErrorKind::Network, "connection failed").into();
    assert!(matches!(network, ProviderError::Network { .. }));
    let protocol: ProviderError =
        ChatGptApiError::new(ChatGptApiErrorKind::ProtocolIncompatible, "unexpected body").into();
    assert!(matches!(
        protocol,
        ProviderError::ProtocolIncompatible { .. }
    ));
}

#[test]
fn logout_revokes_and_clears_local_auth() {
    let (provider, api) = provider(
        vec![workspace("ws-one", "Personal")],
        fixture("single"),
        Some(Utc::now() + Duration::hours(1)),
    );
    complete_auth(&provider);
    run_ready(provider.logout(LogoutRequest::default())).unwrap();
    assert_eq!(api.calls.lock().unwrap().revocations, 1);
    assert_eq!(
        run_ready(provider.auth_status()).unwrap(),
        AuthState::NotAuthenticated
    );
}

#[test]
fn logout_cancels_pending_oauth_callback() {
    let (provider, api) = provider(
        vec![workspace("ws-one", "Personal")],
        fixture("single"),
        Some(Utc::now() + Duration::hours(1)),
    );
    let challenge = run_ready(provider.start_auth(AuthStartRequest {
        method: None,
        redirect_uri: None,
    }))
    .unwrap();
    run_ready(provider.logout(LogoutRequest::default())).unwrap();
    assert!(matches!(
        run_ready(provider.complete_auth(AuthCompleteRequest {
            flow_id: challenge.flow_id,
            authorization_code: Some("authorization-code".into()),
            redirect_uri: None,
        })),
        Err(ProviderError::AuthenticationInvalid { .. })
    ));
    assert_eq!(api.calls.lock().unwrap().exchanges, 0);
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn cancelled_remote_revoke_still_leaves_local_auth_cleared() {
    let (provider, api) = provider(
        vec![workspace("ws-one", "Personal")],
        fixture("single"),
        Some(Utc::now() + Duration::hours(1)),
    );
    complete_auth(&provider);
    let provider = Arc::new(provider);
    api.pause_revoke.store(true, Ordering::SeqCst);

    let logging_out_provider = provider.clone();
    let logout =
        tokio::spawn(async move { logging_out_provider.logout(LogoutRequest::default()).await });
    api.revoke_started.notified().await;
    logout.abort();
    assert!(logout.await.unwrap_err().is_cancelled());

    assert_eq!(
        provider.auth_status().await.unwrap(),
        AuthState::NotAuthenticated
    );
}

#[test]
fn normalizes_windows_by_duration_not_vendor_position() {
    let usage = ChatGptUsage {
        workspace: workspace("ws-one", "Personal"),
        observed_at: Utc.with_ymd_and_hms(2026, 8, 27, 12, 0, 0).unwrap(),
        response: fixture("multi"),
    }
    .normalize()
    .unwrap();
    assert_eq!(usage.plan.as_deref(), Some("team"));
    assert!(matches!(
        usage.windows[0].window,
        UsageWindowKind::FiveHours
    ));
    assert!(matches!(usage.windows[1].window, UsageWindowKind::Weekly));
    assert!(matches!(usage.windows[2].window, UsageWindowKind::Weekly));
}

#[test]
fn preserves_unknown_and_additional_windows_and_optional_plan() {
    let unknown = ChatGptUsage {
        workspace: workspace("ws-one", "Personal"),
        observed_at: Utc::now(),
        response: fixture("unknown"),
    }
    .normalize()
    .unwrap();
    assert!(unknown.windows.iter().any(|window| matches!(
        &window.window,
        UsageWindowKind::Other { id, .. } if id == "3600s"
    )));
    assert!(unknown.windows.iter().any(|window| matches!(
        &window.window,
        UsageWindowKind::Other { id, .. } if id == "credits"
    )));
    assert!(unknown.windows.iter().any(|window| matches!(
        &window.window,
        UsageWindowKind::Other { id, .. } if id == "rate_limit_reset_credits"
    )));
    assert!(
        unknown
            .windows
            .iter()
            .any(|window| window.measurements[0].name == "burst_usage")
    );

    let missing_plan = ChatGptUsage {
        workspace: workspace("ws-one", "Personal"),
        observed_at: Utc::now(),
        response: fixture("missing-plan"),
    }
    .normalize()
    .unwrap();
    assert_eq!(missing_plan.plan, None);
}

#[test]
fn preserves_boolean_limit_and_credit_states() {
    let usage = ChatGptUsage {
        workspace: workspace("ws-one", "Personal"),
        observed_at: Utc::now(),
        response: serde_json::from_str(
            r#"{
                "rate_limit": {"allowed": false, "limit_reached": true},
                "credits": {"has_credits": false, "unlimited": true, "balance": null}
            }"#,
        )
        .unwrap(),
    }
    .normalize()
    .unwrap();
    let measurements = usage
        .windows
        .iter()
        .flat_map(|window| &window.measurements)
        .map(|measurement| (measurement.name.as_str(), measurement.used))
        .collect::<Vec<_>>();
    assert!(measurements.contains(&("allowed", 0.0)));
    assert!(measurements.contains(&("limit_reached", 1.0)));
    assert!(measurements.contains(&("has_credits", 0.0)));
    assert!(measurements.contains(&("unlimited", 1.0)));
}

#[test]
fn accepts_a_pasted_callback_url() {
    let (provider, api) = provider(
        vec![workspace("ws-one", "Personal")],
        fixture("single"),
        Some(Utc::now() + Duration::hours(1)),
    );
    let challenge = run_ready(provider.start_auth(AuthStartRequest {
        method: None,
        redirect_uri: None,
    }))
    .unwrap();
    let callback = format!(
        "http://127.0.0.1:1455/callback?code=authorization-code&state={}",
        challenge.flow_id
    );
    let state = run_ready(provider.complete_auth(AuthCompleteRequest {
        flow_id: challenge.flow_id,
        authorization_code: Some(callback),
        redirect_uri: Some("http://127.0.0.1:1455/callback".into()),
    }))
    .unwrap();
    assert!(matches!(state, AuthState::Authenticated { .. }));
    assert_eq!(api.calls.lock().unwrap().exchanges, 1);
}

#[test]
fn rejects_a_callback_url_whose_state_does_not_match() {
    let (provider, api) = provider(
        vec![workspace("ws-one", "Personal")],
        fixture("single"),
        Some(Utc::now() + Duration::hours(1)),
    );
    let challenge = run_ready(provider.start_auth(AuthStartRequest {
        method: None,
        redirect_uri: None,
    }))
    .unwrap();
    let callback = format!(
        "http://127.0.0.1:1455/callback?code=authorization-code&state={}-attacker",
        challenge.flow_id
    );
    assert!(matches!(
        run_ready(provider.complete_auth(AuthCompleteRequest {
            flow_id: challenge.flow_id,
            authorization_code: Some(callback),
            redirect_uri: Some("http://127.0.0.1:1455/callback".into()),
        })),
        Err(ProviderError::AuthenticationInvalid { .. })
    ));
    assert_eq!(api.calls.lock().unwrap().exchanges, 0);
}

#[test]
fn rejects_a_callback_url_without_an_authorization_code() {
    let (provider, api) = provider(
        vec![workspace("ws-one", "Personal")],
        fixture("single"),
        Some(Utc::now() + Duration::hours(1)),
    );
    let challenge = run_ready(provider.start_auth(AuthStartRequest {
        method: None,
        redirect_uri: None,
    }))
    .unwrap();
    let callback = format!(
        "http://127.0.0.1:1455/callback?error=access_denied&state={}",
        challenge.flow_id
    );
    assert!(matches!(
        run_ready(provider.complete_auth(AuthCompleteRequest {
            flow_id: challenge.flow_id,
            authorization_code: Some(callback),
            redirect_uri: Some("http://127.0.0.1:1455/callback".into()),
        })),
        Err(ProviderError::AuthenticationInvalid { .. })
    ));
    assert_eq!(api.calls.lock().unwrap().exchanges, 0);
}
