use std::collections::VecDeque;
use std::future::Future;
use std::task::{Context, Poll, Waker};

use super::*;

enum StubCall {
    List,
    Refresh(i64),
}

struct StubApi {
    calls: Mutex<Vec<StubCall>>,
    lists: Mutex<VecDeque<Result<AccountsResponse, ApiFailure>>>,
    refreshes: Mutex<VecDeque<Result<UsageRefreshResponse, ApiFailure>>>,
}

impl StubApi {
    fn queue(
        lists: Vec<Result<AccountsResponse, ApiFailure>>,
        refreshes: Vec<Result<UsageRefreshResponse, ApiFailure>>,
    ) -> Arc<Self> {
        Arc::new(Self {
            calls: Mutex::new(Vec::new()),
            lists: Mutex::new(VecDeque::from_iter(lists)),
            refreshes: Mutex::new(VecDeque::from_iter(refreshes)),
        })
    }

    fn calls(&self) -> Vec<String> {
        self.calls
            .lock()
            .unwrap()
            .iter()
            .map(|call| match call {
                StubCall::List => "list".to_owned(),
                StubCall::Refresh(id) => format!("refresh:{id}"),
            })
            .collect()
    }
}

#[async_trait]
impl Codex2apiApi for StubApi {
    async fn list_accounts(&self, _: &str, _: &str) -> Result<AccountsResponse, ApiFailure> {
        self.calls.lock().unwrap().push(StubCall::List);
        self.lists.lock().unwrap().pop_front().unwrap()
    }

    async fn refresh_usage(
        &self,
        _: &str,
        _: &str,
        account_id: i64,
    ) -> Result<UsageRefreshResponse, ApiFailure> {
        self.calls
            .lock()
            .unwrap()
            .push(StubCall::Refresh(account_id));
        self.refreshes.lock().unwrap().pop_front().unwrap()
    }
}

fn stub_account() -> GatewayAccount {
    GatewayAccount {
        id: 7,
        name: Some("ops".into()),
        email: Some("ops@example.com".into()),
        plan_type: Some("pro".into()),
        subscription_expires_at: Some("2026-10-01T00:00:00Z".into()),
        status: Some("active".into()),
        usage_percent_5h: Some(12.5),
        usage_percent_7d: Some(40.0),
        usage_percent_spark: None,
        reset_5h_at: Some("2026-09-19T05:00:00Z".into()),
        reset_7d_at: Some("2026-09-26T00:00:00Z".into()),
        reset_spark_at: None,
        usage_window_7d_kind: None,
        billed_5h: Some(1.25),
        billed_7d: Some(9.5),
    }
}

fn stub_list() -> AccountsResponse {
    AccountsResponse {
        accounts: vec![stub_account()],
    }
}

fn stub_refresh() -> UsageRefreshResponse {
    UsageRefreshResponse {
        refreshed: Some(true),
        usage_percent_5h: Some(60.0),
        usage_percent_7d: Some(41.0),
        usage_percent_spark: Some(3.0),
        reset_5h_at: Some("2026-09-19T06:00:00Z".into()),
        reset_7d_at: Some("2026-09-26T00:00:00Z".into()),
        reset_spark_at: Some("2026-09-20T00:00:00Z".into()),
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

fn authenticate(provider: &Codex2apiProvider) {
    let challenge = ready(provider.start_auth(AuthStartRequest {
        method: Some(AuthMethod::ApiToken),
        redirect_uri: None,
    }))
    .unwrap();
    ready(provider.complete_auth(AuthCompleteRequest {
        flow_id: challenge.flow_id,
        authorization_code: Some("http://localhost:8317 admin-secret ops@example.com".into()),
        redirect_uri: None,
    }))
    .unwrap();
}

#[test]
fn api_token_sign_in_asks_for_the_secret_triple() {
    let provider = Codex2apiProvider::with_api(StubApi::queue(vec![], vec![]));
    let challenge = ready(provider.start_auth(AuthStartRequest::default())).unwrap();
    assert_eq!(challenge.method, AuthMethod::ApiToken);
    let input = challenge.input.unwrap();
    assert!(input.secret);
    assert!(input.prompt.contains("admin key"));
}

#[test]
fn complete_auth_validates_the_triple_and_reports_identity() {
    let provider = Codex2apiProvider::with_api(StubApi::queue(vec![Ok(stub_list())], vec![]));
    let state = {
        let challenge = ready(provider.start_auth(AuthStartRequest::default())).unwrap();
        ready(provider.complete_auth(AuthCompleteRequest {
            flow_id: challenge.flow_id,
            authorization_code: Some("http://localhost:8317 admin-secret ops@example.com".into()),
            redirect_uri: None,
        }))
        .unwrap()
    };
    assert_eq!(
        state,
        AuthState::Authenticated {
            account_label: Some("ops@example.com".into()),
            account_key: ullage_auth::account_identity("http://localhost:8317#7"),
            expires_at: None,
        }
    );
}

#[test]
fn complete_auth_accepts_a_numeric_upstream_id() {
    let provider = Codex2apiProvider::with_api(StubApi::queue(vec![Ok(stub_list())], vec![]));
    let challenge = ready(provider.start_auth(AuthStartRequest::default())).unwrap();
    assert!(
        ready(provider.complete_auth(AuthCompleteRequest {
            flow_id: challenge.flow_id,
            authorization_code: Some("https://gw.example.com admin-secret 7".into()),
            redirect_uri: None,
        }))
        .is_ok()
    );
}

#[test]
fn complete_auth_rejects_malformed_input_without_calling_the_api() {
    let api = StubApi::queue(vec![], vec![]);
    let provider = Codex2apiProvider::with_api(api.clone());
    let challenge = ready(provider.start_auth(AuthStartRequest::default())).unwrap();
    for pasted in [
        "only-two parts",
        "http://lan-host:8317 admin-secret ops@example.com",
        "http://localhost:8317 admin-secret a name with spaces",
        "http://localhost:8317 admin-secret not-an-email",
    ] {
        assert!(matches!(
            ready(provider.complete_auth(AuthCompleteRequest {
                flow_id: challenge.flow_id.clone(),
                authorization_code: Some(pasted.to_owned()),
                redirect_uri: None,
            })),
            Err(ProviderError::AuthenticationInvalid { .. })
        ));
    }
    assert!(api.calls().is_empty());
}

#[test]
fn complete_auth_fails_when_no_upstream_account_matches() {
    let provider = Codex2apiProvider::with_api(StubApi::queue(vec![Ok(stub_list())], vec![]));
    let challenge = ready(provider.start_auth(AuthStartRequest::default())).unwrap();
    assert!(matches!(
        ready(provider.complete_auth(AuthCompleteRequest {
            flow_id: challenge.flow_id,
            authorization_code: Some(
                "http://localhost:8317 admin-secret absent@example.com".into(),
            ),
            redirect_uri: None,
        })),
        Err(ProviderError::AuthenticationInvalid { .. })
    ));
    assert!(matches!(
        ready(provider.auth_status()).unwrap(),
        AuthState::Invalid { .. }
    ));
}

#[test]
fn query_lists_then_refreshes_the_bound_account() {
    let api = StubApi::queue(
        vec![Ok(stub_list()), Ok(stub_list())],
        vec![Ok(stub_refresh())],
    );
    let provider = Codex2apiProvider::with_api(api.clone());
    authenticate(&provider);
    let outcome = ready(provider.query(UsageQuery::default())).unwrap();
    let QueryOutcome::Complete { data } = outcome else {
        panic!("expected a complete outcome");
    };
    assert_eq!(api.calls(), vec!["list", "list", "refresh:7"]);
    // The refresh overlay wins over the list-embedded values.
    assert_eq!(data.five_hours.percent, Some(60.0));
    assert_eq!(data.spark.percent, Some(3.0));
    // Plan and billing stay list-sourced.
    assert_eq!(data.plan_type.as_deref(), Some("pro"));
    assert_eq!(data.five_hours.billed, Some(1.25));
    let normalized = provider.normalize(data).unwrap();
    assert_eq!(normalized.windows.len(), 3);
    assert_eq!(
        normalized.windows[2].window,
        ullage_core::UsageWindowKind::Other {
            id: "spark".into(),
            label: "spark".into()
        }
    );
}

#[test]
fn query_reports_partial_data_when_the_refresh_fails() {
    let provider = Codex2apiProvider::with_api(StubApi::queue(
        vec![Ok(stub_list()), Ok(stub_list())],
        vec![Err(ApiFailure::network("boom"))],
    ));
    authenticate(&provider);
    let outcome = ready(provider.query(UsageQuery::default())).unwrap();
    let QueryOutcome::Partial { data, failures } = outcome else {
        panic!("expected a partial outcome");
    };
    assert_eq!(failures[0].scope, "usage_refresh");
    assert_eq!(data.five_hours.percent, Some(12.5));
}

#[test]
fn a_rejected_key_during_query_invalidates_the_session() {
    let provider = Codex2apiProvider::with_api(StubApi::queue(
        vec![Ok(stub_list()), Err(ApiFailure::authentication("revoked"))],
        vec![],
    ));
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
fn a_missing_upstream_account_invalidates_the_session() {
    let provider = Codex2apiProvider::with_api(StubApi::queue(
        vec![Ok(stub_list()), Ok(AccountsResponse { accounts: vec![] })],
        vec![],
    ));
    authenticate(&provider);
    assert!(matches!(
        ready(provider.query(UsageQuery::default())),
        Err(ProviderError::AuthenticationInvalid { .. })
    ));
    assert!(matches!(
        ready(provider.auth_status()).unwrap(),
        AuthState::Invalid {
            account_key: Some(_),
            ..
        }
    ));
}

#[test]
fn a_rejected_key_fails_sign_in_and_releases_the_flow() {
    let provider = Codex2apiProvider::with_api(StubApi::queue(
        vec![Err(ApiFailure::authentication("rejected")), Ok(stub_list())],
        vec![],
    ));
    let challenge = ready(provider.start_auth(AuthStartRequest::default())).unwrap();
    assert!(matches!(
        ready(provider.complete_auth(AuthCompleteRequest {
            flow_id: challenge.flow_id,
            authorization_code: Some("http://localhost:8317 bad-key ops@example.com".into(),),
            redirect_uri: None,
        })),
        Err(ProviderError::AuthenticationInvalid { .. })
    ));
    assert!(matches!(
        ready(provider.auth_status()).unwrap(),
        AuthState::Invalid { .. }
    ));
    let retry = ready(provider.start_auth(AuthStartRequest::default())).unwrap();
    assert!(
        ready(provider.complete_auth(AuthCompleteRequest {
            flow_id: retry.flow_id,
            authorization_code: Some("http://localhost:8317 good-key ops@example.com".into(),),
            redirect_uri: None,
        }))
        .is_ok()
    );
}

#[test]
fn an_expired_flow_cannot_complete_but_releases_the_slot() {
    let provider = Codex2apiProvider::with_api(StubApi::queue(vec![], vec![]));
    let challenge = ready(provider.start_auth(AuthStartRequest::default())).unwrap();
    provider.lock_state().unwrap().pending_flow = Some(PendingFlow {
        flow_id: challenge.flow_id.clone(),
        expires_at: Utc::now() - Duration::minutes(1),
    });
    assert!(matches!(
        ready(provider.complete_auth(AuthCompleteRequest {
            flow_id: challenge.flow_id,
            authorization_code: Some("http://localhost:8317 k 7".into()),
            redirect_uri: None,
        })),
        Err(ProviderError::AuthenticationInvalid { .. })
    ));
}

#[test]
fn logout_clears_the_session() {
    let provider = Codex2apiProvider::with_api(StubApi::queue(vec![Ok(stub_list())], vec![]));
    authenticate(&provider);
    ready(provider.logout(LogoutRequest::default())).unwrap();
    assert_eq!(
        ready(provider.auth_status()).unwrap(),
        AuthState::NotAuthenticated
    );
}

#[test]
fn pasted_triple_parsing_is_strict() {
    assert_eq!(
        parse_pasted_credentials("http://localhost:8317 key ops@example.com").unwrap(),
        (
            "http://localhost:8317".to_owned(),
            "key".to_owned(),
            "ops@example.com".to_owned()
        )
    );
    assert_eq!(
        parse_pasted_credentials("https://gw.example.com/ key 42")
            .unwrap()
            .0,
        "https://gw.example.com"
    );
    assert!(parse_pasted_credentials("http://localhost k display name").is_err());
    assert!(parse_pasted_credentials("http://localhost").is_err());
    assert!(parse_pasted_credentials("http://10.0.0.2 k 7").is_err());
}

#[test]
fn account_key_is_scoped_to_the_gateway_and_hashed() {
    let first = upstream_account_key("http://localhost:8317", 7);
    let second = upstream_account_key("http://localhost:9000", 7);
    assert!(first.is_some());
    assert_ne!(first, second);
    assert_eq!(
        first,
        ullage_auth::account_identity("http://localhost:8317#7")
    );
}

#[test]
fn gateway_free_text_is_sanitized_at_the_boundary() {
    let mut account = stub_account();
    account.email = Some("ops\u{202e}@example.com".into());
    account.name = Some("na\u{07}me".into());
    let label = dto::account_label(&account).unwrap();
    assert_eq!(label, "ops@example.com");
    account.email = None;
    assert_eq!(dto::account_label(&account).unwrap(), "name");
    let usage = Codex2apiUsage {
        account_label: None,
        plan_type: Some("pro\u{1b}[2J".into()),
        subscription_expires_at: None,
        window_7d_kind: None,
        five_hours: QuotaWindow::default(),
        long: QuotaWindow::default(),
        spark: QuotaWindow::default(),
        observed_at: Utc::now(),
    };
    let normalized = dto::normalize(usage).unwrap();
    assert_eq!(normalized.plan.as_deref(), Some("pro[2J"));
}
