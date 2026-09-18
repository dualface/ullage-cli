use std::collections::VecDeque;
use std::future::Future;
use std::task::{Context, Poll, Waker};

use super::*;

struct StubApi {
    exchanges: Mutex<VecDeque<Result<ExchangeResponse, ApiFailure>>>,
    statuses: Mutex<VecDeque<Result<UserStatusResponse, ApiFailure>>>,
    last_status_url: Mutex<Option<String>>,
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
        })
    }
}

#[async_trait]
impl DevinApi for StubApi {
    async fn exchange_pkce_code(
        &self,
        _: &str,
        _: &str,
        _: &str,
    ) -> Result<ExchangeResponse, ApiFailure> {
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

/// A callback channel that never reports a redirect; the pasted path is
/// exercised instead.
struct SilentCallback;

impl CallbackSource for SilentCallback {
    fn redirect_uri(&self) -> String {
        "http://127.0.0.1:1/callback".into()
    }

    fn take(&self) -> Option<CallbackOutcome> {
        None
    }
}

fn silent_callbacks() -> Arc<CallbackFactory> {
    Arc::new(|_, _| Ok(Arc::new(SilentCallback)))
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
fn browser_start_offers_the_pkce_authorization_url() {
    let provider =
        DevinProvider::with_api_and_callbacks(StubApi::queue(vec![], vec![]), silent_callbacks());
    let challenge = ready(provider.start_auth(AuthStartRequest::default())).unwrap();
    assert_eq!(challenge.method, AuthMethod::BrowserOAuth);
    let url = challenge.verification_uri.unwrap();
    let parsed = Url::parse(&url).unwrap();
    assert_eq!(parsed.host_str(), Some("app.devin.ai"));
    assert_eq!(parsed.path(), "/auth/cli/continue");
    let pairs: std::collections::HashMap<_, _> = parsed.query_pairs().into_owned().collect();
    assert_eq!(pairs.get("state"), Some(&challenge.flow_id));
    assert_eq!(
        pairs.get("prompt").map(String::as_str),
        Some("select_account")
    );
    assert_eq!(
        pairs.get("code_challenge_method").map(String::as_str),
        Some("S256")
    );
    assert_eq!(pairs.get("cli_pkce_marker").map(String::as_str), Some("1"));
    assert!(pairs.contains_key("code_challenge"));
    // The provider's own listener path means nothing to paste back.
    assert!(challenge.input.is_none());
}

#[test]
fn a_client_supplied_redirect_asks_for_the_callback_url() {
    let provider =
        DevinProvider::with_api_and_callbacks(StubApi::queue(vec![], vec![]), silent_callbacks());
    let challenge = ready(provider.start_auth(AuthStartRequest {
        method: Some(AuthMethod::BrowserOAuth),
        redirect_uri: Some("http://localhost:7777/callback".into()),
    }))
    .unwrap();
    let url = Url::parse(&challenge.verification_uri.unwrap()).unwrap();
    let pairs: std::collections::HashMap<_, _> = url.query_pairs().into_owned().collect();
    assert_eq!(
        pairs.get("redirect_uri").map(String::as_str),
        Some("http://localhost:7777/callback")
    );
    let input = challenge.input.unwrap();
    assert!(!input.secret);
}

#[test]
fn a_non_loopback_redirect_is_rejected() {
    let provider =
        DevinProvider::with_api_and_callbacks(StubApi::queue(vec![], vec![]), silent_callbacks());
    assert!(matches!(
        ready(provider.start_auth(AuthStartRequest {
            method: Some(AuthMethod::BrowserOAuth),
            redirect_uri: Some("https://attacker.invalid/callback".into()),
        })),
        Err(ProviderError::AuthenticationInvalid { .. })
    ));
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
fn pasted_callback_url_completes_the_browser_flow() {
    let provider = DevinProvider::with_api_and_callbacks(
        StubApi::queue(
            vec![Ok(ExchangeResponse {
                api_key: "exchanged-key".into(),
                api_server_url: Some("https://server.codeium.com".into()),
                ..ExchangeResponse::default()
            })],
            vec![],
        ),
        silent_callbacks(),
    );
    let challenge = ready(provider.start_auth(AuthStartRequest {
        method: Some(AuthMethod::BrowserOAuth),
        redirect_uri: Some("http://localhost:7777/callback".into()),
    }))
    .unwrap();
    assert_eq!(
        ready(provider.complete_auth(AuthCompleteRequest {
            flow_id: challenge.flow_id.clone(),
            authorization_code: Some(format!(
                "http://localhost:7777/callback?code=code-1&state={}",
                challenge.flow_id
            )),
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
fn a_wrong_state_in_the_pasted_callback_is_rejected() {
    let provider =
        DevinProvider::with_api_and_callbacks(StubApi::queue(vec![], vec![]), silent_callbacks());
    let challenge = ready(provider.start_auth(AuthStartRequest {
        method: Some(AuthMethod::BrowserOAuth),
        redirect_uri: Some("http://localhost:7777/callback".into()),
    }))
    .unwrap();
    assert!(matches!(
        ready(provider.complete_auth(AuthCompleteRequest {
            flow_id: challenge.flow_id.clone(),
            authorization_code: Some(
                "http://localhost:7777/callback?code=code-1&state=wrong".into()
            ),
            redirect_uri: None,
        })),
        Err(ProviderError::AuthenticationInvalid { .. })
    ));
}

#[test]
fn a_listener_flow_polls_pending_until_the_callback_lands() {
    let provider =
        DevinProvider::with_api_and_callbacks(StubApi::queue(vec![], vec![]), silent_callbacks());
    // No client redirect URI: the SilentCallback stands in for the real
    // listener and never produces an outcome, so completion stays pending.
    let challenge = ready(provider.start_auth(AuthStartRequest::default())).unwrap();
    assert!(matches!(
        ready(provider.complete_auth(AuthCompleteRequest {
            flow_id: challenge.flow_id.clone(),
            authorization_code: None,
            redirect_uri: None,
        }))
        .unwrap(),
        AuthState::Pending { .. }
    ));
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

#[test]
fn callback_input_parsing_accepts_url_code_state_and_bare_code() {
    let redirect = "http://127.0.0.1:4567/callback";
    let (code, state) =
        parse_authorization_input("http://127.0.0.1:4567/callback?code=c&state=s", redirect)
            .unwrap();
    assert_eq!((code.as_str(), state.as_deref()), ("c", Some("s")));
    let (code, state) = parse_authorization_input("c#s", redirect).unwrap();
    assert_eq!((code.as_str(), state.as_deref()), ("c", Some("s")));
    let (code, state) = parse_authorization_input("c", redirect).unwrap();
    assert_eq!((code.as_str(), state), ("c", None));
    assert!(matches!(
        parse_authorization_input("http://127.0.0.1:4567/other?code=c&state=s", redirect),
        Err(ProviderError::AuthenticationInvalid { .. })
    ));
}
