use std::collections::VecDeque;
use std::future::Future;
use std::task::{Context, Poll, Waker};

use super::*;

struct StubApi {
    exchanges: Mutex<VecDeque<Result<ExchangeResponse, ApiFailure>>>,
    statuses: Mutex<VecDeque<Result<UserStatusResponse, ApiFailure>>>,
    last_status_url: Mutex<Option<String>>,
    /// The code of every exchange attempt, in order.
    exchanged_codes: Mutex<Vec<String>>,
}

impl StubApi {
    fn queue(
        exchanges: Vec<Result<ExchangeResponse, ApiFailure>>,
        statuses: Vec<Result<UserStatusResponse, ApiFailure>>,
    ) -> Arc<Self> {
        Arc::new(Self {
            exchanges: Mutex::new(VecDeque::from_iter(exchanges)),
            statuses: Mutex::new(VecDeque::from_iter(statuses)),
            last_status_url: Mutex::new(None),
            exchanged_codes: Mutex::new(Vec::new()),
        })
    }
}

#[async_trait]
impl DevinApi for StubApi {
    async fn exchange_pkce_code(
        &self,
        code: &str,
        _: &str,
    ) -> Result<ExchangeResponse, ApiFailure> {
        self.exchanged_codes.lock().unwrap().push(code.to_owned());
        self.exchanges.lock().unwrap().pop_front().unwrap()
    }

    async fn user_status(
        &self,
        _: &str,
        api_server_url: &str,
    ) -> Result<UserStatusResponse, ApiFailure> {
        *self.last_status_url.lock().unwrap() = Some(api_server_url.to_owned());
        self.statuses.lock().unwrap().pop_front().unwrap()
    }
}

/// A stub whose one exchange succeeds, for browser flows expected to finish.
fn exchanging_api() -> Arc<StubApi> {
    StubApi::queue(
        vec![Ok(ExchangeResponse {
            api_key: "exchanged-key".into(),
            api_server_url: Some("https://server.codeium.com".into()),
            ..ExchangeResponse::default()
        })],
        vec![],
    )
}

fn authenticated() -> AuthState {
    AuthState::Authenticated {
        account_label: None,
        account_key: None,
        expires_at: None,
    }
}

fn start_browser(provider: &DevinProvider) -> AuthChallenge {
    ready(provider.start_auth(AuthStartRequest::default())).unwrap()
}

fn paste(
    provider: &DevinProvider,
    flow_id: &str,
    input: Option<&str>,
    redirect_uri: Option<&str>,
) -> ProviderResult<AuthState> {
    ready(provider.complete_auth(AuthCompleteRequest {
        flow_id: flow_id.to_owned(),
        authorization_code: input.map(str::to_owned),
        redirect_uri: redirect_uri.map(str::to_owned),
    }))
}

fn stub_status() -> UserStatusResponse {
    UserStatusResponse {
        user_status: Some(UserStatus {
            plan_status: Some(PlanStatus {
                plan_info: Some(PlanInfo {
                    plan_name: Some("Pro".into()),
                }),
                daily_quota_remaining_percent: Some(67.0),
                weekly_quota_remaining_percent: Some(83.0),
                daily_quota_reset_at_unix: Some(1780000000),
                weekly_quota_reset_at_unix: Some(1780600000),
                plan_start: None,
                plan_end: None,
                available_prompt_credits: Some(-1.0),
                acu_consumed: None,
                acu_limit: None,
            }),
        }),
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

#[test]
fn browser_start_offers_the_paste_code_authorization_url() {
    let provider = DevinProvider::with_api(StubApi::queue(vec![], vec![]));
    let challenge = start_browser(&provider);
    assert_eq!(challenge.method, AuthMethod::BrowserOAuth);
    let parsed = Url::parse(&challenge.verification_uri.unwrap()).unwrap();
    assert_eq!(parsed.scheme(), "https");
    assert_eq!(parsed.host_str(), Some("app.devin.ai"));
    assert_eq!(parsed.path(), "/auth/cli/continue");
    let pairs: Vec<(String, String)> = parsed.query_pairs().into_owned().collect();
    let keys: Vec<&str> = pairs.iter().map(|(key, _)| key.as_str()).collect();
    assert_eq!(
        keys,
        [
            "state",
            "prompt",
            "code_challenge",
            "code_challenge_method",
            "cli_pkce_marker"
        ]
    );
    assert_eq!(pairs[0].1, challenge.flow_id);
    assert_eq!(pairs[1].1, "select_account");
    assert!(!pairs[2].1.is_empty());
    assert_eq!(pairs[3].1, "S256");
    assert_eq!(pairs[4].1, "1");
    let input = challenge.input.unwrap();
    assert!(!input.secret);
    assert!(input.prompt.contains("code"));
    assert!(!input.prompt.contains("callback"));
}

#[test]
fn a_client_supplied_redirect_stays_out_of_the_authorization_url() {
    let provider = DevinProvider::with_api(StubApi::queue(vec![], vec![]));
    let challenge = ready(provider.start_auth(AuthStartRequest {
        method: Some(AuthMethod::BrowserOAuth),
        redirect_uri: Some("http://localhost:7777/callback".into()),
    }))
    .unwrap();
    let url = Url::parse(&challenge.verification_uri.unwrap()).unwrap();
    assert!(url.query_pairs().all(|(key, _)| key != "redirect_uri"));
}

#[test]
fn manual_token_flow_presents_the_token_page_and_secret_input() {
    let provider = DevinProvider::with_api(StubApi::queue(vec![], vec![]));
    let challenge = ready(provider.start_auth(AuthStartRequest {
        method: Some(AuthMethod::ApiToken),
        redirect_uri: None,
    }))
    .unwrap();
    assert_eq!(challenge.method, AuthMethod::ApiToken);
    assert_eq!(
        challenge.verification_uri.as_deref(),
        Some(MANUAL_TOKEN_PAGE)
    );
    assert!(challenge.input.unwrap().secret);
}

#[test]
fn pasted_key_sign_in_validates_and_authenticates() {
    let provider = DevinProvider::with_api(StubApi::queue(vec![], vec![Ok(stub_status())]));
    let challenge = ready(provider.start_auth(AuthStartRequest {
        method: Some(AuthMethod::ApiToken),
        redirect_uri: None,
    }))
    .unwrap();
    assert_eq!(
        ready(provider.complete_auth(AuthCompleteRequest {
            flow_id: challenge.flow_id,
            authorization_code: Some("redacted-devin-key".into()),
            redirect_uri: None,
        }))
        .unwrap(),
        AuthState::Authenticated {
            account_label: None,
            account_key: None,
            expires_at: None,
        }
    );
}

#[test]
fn a_rejected_key_fails_sign_in_and_releases_the_flow() {
    let provider = DevinProvider::with_api(StubApi::queue(
        vec![],
        vec![
            Err(ApiFailure::authentication("rejected")),
            Ok(stub_status()),
        ],
    ));
    let challenge = ready(provider.start_auth(AuthStartRequest {
        method: Some(AuthMethod::ApiToken),
        redirect_uri: None,
    }))
    .unwrap();
    assert!(matches!(
        ready(provider.complete_auth(AuthCompleteRequest {
            flow_id: challenge.flow_id,
            authorization_code: Some("bad-key".into()),
            redirect_uri: None,
        })),
        Err(ProviderError::AuthenticationInvalid { .. })
    ));
    assert!(matches!(
        ready(provider.auth_status()).unwrap(),
        AuthState::Invalid { .. }
    ));
    let retry = ready(provider.start_auth(AuthStartRequest {
        method: Some(AuthMethod::ApiToken),
        redirect_uri: None,
    }))
    .unwrap();
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
fn a_pasted_bare_code_completes_the_browser_flow() {
    let api = exchanging_api();
    let provider = DevinProvider::with_api(api.clone());
    let challenge = start_browser(&provider);
    assert_eq!(
        paste(&provider, &challenge.flow_id, Some(" code-1 "), None).unwrap(),
        authenticated()
    );
    assert_eq!(*api.exchanged_codes.lock().unwrap(), ["code-1"]);
}

#[test]
fn a_redirect_uri_on_completion_is_ignored() {
    let provider = DevinProvider::with_api(exchanging_api());
    let challenge = start_browser(&provider);
    assert_eq!(
        paste(
            &provider,
            &challenge.flow_id,
            Some("code-1"),
            Some("http://localhost:7777/callback"),
        )
        .unwrap(),
        authenticated()
    );
}

#[test]
fn a_pasted_code_with_the_flow_state_completes_the_browser_flow() {
    let api = exchanging_api();
    let provider = DevinProvider::with_api(api.clone());
    let challenge = start_browser(&provider);
    let input = format!("code-1#{}", challenge.flow_id);
    assert_eq!(
        paste(&provider, &challenge.flow_id, Some(&input), None).unwrap(),
        authenticated()
    );
    assert_eq!(*api.exchanged_codes.lock().unwrap(), ["code-1"]);
}

#[test]
fn a_wrong_pasted_state_is_rejected_before_the_exchange() {
    let api = StubApi::queue(vec![], vec![]);
    let provider = DevinProvider::with_api(api.clone());
    let challenge = start_browser(&provider);
    assert!(matches!(
        paste(&provider, &challenge.flow_id, Some("code-1#wrong"), None),
        Err(ProviderError::AuthenticationInvalid { .. })
    ));
    assert!(api.exchanged_codes.lock().unwrap().is_empty());
}

#[test]
fn a_pasted_url_is_rejected_before_the_exchange() {
    let api = StubApi::queue(vec![], vec![]);
    let provider = DevinProvider::with_api(api.clone());
    let challenge = start_browser(&provider);
    let input = format!(
        "http://localhost:7777/callback?code=code-1&state={}",
        challenge.flow_id
    );
    assert!(matches!(
        paste(&provider, &challenge.flow_id, Some(&input), None),
        Err(ProviderError::AuthenticationInvalid { .. })
    ));
    assert!(api.exchanged_codes.lock().unwrap().is_empty());
}

#[test]
fn an_empty_paste_fails_instead_of_pending() {
    let api = StubApi::queue(vec![], vec![]);
    let provider = DevinProvider::with_api(api.clone());
    let challenge = start_browser(&provider);
    for input in [None, Some(""), Some("   ")] {
        assert!(
            matches!(
                paste(&provider, &challenge.flow_id, input, None),
                Err(ProviderError::AuthenticationInvalid { .. })
            ),
            "{input:?}"
        );
    }
    assert!(api.exchanged_codes.lock().unwrap().is_empty());
}

#[test]
fn query_reports_normalized_usage() {
    let provider = DevinProvider::with_api(StubApi::queue(
        vec![],
        vec![Ok(stub_status()), Ok(stub_status())],
    ));
    let challenge = ready(provider.start_auth(AuthStartRequest {
        method: Some(AuthMethod::ApiToken),
        redirect_uri: None,
    }))
    .unwrap();
    ready(provider.complete_auth(AuthCompleteRequest {
        flow_id: challenge.flow_id,
        authorization_code: Some("redacted-devin-key".into()),
        redirect_uri: None,
    }))
    .unwrap();
    let outcome = ready(provider.query(UsageQuery::default())).unwrap();
    let QueryOutcome::Complete { data } = outcome else {
        panic!("expected a complete outcome");
    };
    let normalized = provider.normalize(data).unwrap();
    assert_eq!(normalized.plan.as_deref(), Some("Pro"));
    assert_eq!(normalized.windows.len(), 2);
    assert_eq!(normalized.windows[0].measurements[0].used, 33.0);
}

#[test]
fn a_rejected_key_during_query_invalidates_the_session() {
    let provider = DevinProvider::with_api(StubApi::queue(
        vec![],
        vec![
            Ok(stub_status()),
            Err(ApiFailure::authentication("revoked")),
        ],
    ));
    let challenge = ready(provider.start_auth(AuthStartRequest {
        method: Some(AuthMethod::ApiToken),
        redirect_uri: None,
    }))
    .unwrap();
    ready(provider.complete_auth(AuthCompleteRequest {
        flow_id: challenge.flow_id,
        authorization_code: Some("redacted-devin-key".into()),
        redirect_uri: None,
    }))
    .unwrap();
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
fn logout_clears_the_session() {
    let provider = DevinProvider::with_api(StubApi::queue(vec![], vec![Ok(stub_status())]));
    let challenge = ready(provider.start_auth(AuthStartRequest {
        method: Some(AuthMethod::ApiToken),
        redirect_uri: None,
    }))
    .unwrap();
    ready(provider.complete_auth(AuthCompleteRequest {
        flow_id: challenge.flow_id,
        authorization_code: Some("redacted-devin-key".into()),
        redirect_uri: None,
    }))
    .unwrap();
    ready(provider.logout(LogoutRequest::default())).unwrap();
    assert_eq!(
        ready(provider.auth_status()).unwrap(),
        AuthState::NotAuthenticated
    );
}
