use std::collections::VecDeque;
use std::future::Future;
use std::task::{Context, Poll, Waker};

use super::*;

enum Call {
    Accounts {
        page: i64,
        result: Result<AccountsPage, ApiFailure>,
    },
    Account {
        id: i64,
        result: Result<AdminAccount, ApiFailure>,
    },
    Usage {
        id: i64,
        force: bool,
        result: Box<Result<UsageInfo, ApiFailure>>,
    },
}

struct StubApi {
    calls: Mutex<VecDeque<Call>>,
}

impl StubApi {
    fn queue(calls: Vec<Call>) -> Arc<Self> {
        Arc::new(Self {
            calls: Mutex::new(VecDeque::from_iter(calls)),
        })
    }
}

#[async_trait]
impl Sub2apiApi for StubApi {
    async fn accounts(&self, _: &str, _: &str, page: i64) -> Result<AccountsPage, ApiFailure> {
        match self.calls.lock().unwrap().pop_front().unwrap() {
            Call::Accounts {
                page: expected,
                result,
            } => {
                assert_eq!(page, expected);
                result
            }
            _ => panic!("unexpected call order"),
        }
    }

    async fn account(&self, _: &str, _: &str, account_id: i64) -> Result<AdminAccount, ApiFailure> {
        match self.calls.lock().unwrap().pop_front().unwrap() {
            Call::Account { id, result } => {
                assert_eq!(account_id, id);
                result
            }
            _ => panic!("unexpected call order"),
        }
    }

    async fn usage(
        &self,
        _: &str,
        _: &str,
        account_id: i64,
        force: bool,
    ) -> Result<UsageInfo, ApiFailure> {
        match self.calls.lock().unwrap().pop_front().unwrap() {
            Call::Usage {
                id,
                force: f,
                result,
            } => {
                assert_eq!(account_id, id);
                assert_eq!(force, f);
                *result
            }
            _ => panic!("unexpected call order"),
        }
    }
}

fn upstream_account() -> AdminAccount {
    AdminAccount {
        id: 7,
        name: Some("work".into()),
        platform: Some("openai".into()),
        ..AdminAccount::default()
    }
}

fn stub_usage() -> UsageInfo {
    UsageInfo {
        five_hour: Some(UsageProgress {
            utilization: Some(42.0),
            resets_at: Some(Utc::now()),
            ..UsageProgress::default()
        }),
        seven_day: Some(UsageProgress {
            utilization: Some(10.0),
            ..UsageProgress::default()
        }),
        subscription_tier: Some("PRO".into()),
        ..UsageInfo::default()
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

fn paste(id_or_name: &str) -> String {
    format!("http://127.0.0.1:55001 admin-deadbeef {id_or_name}")
}

fn authenticate_by_id(provider: &Sub2apiProvider) {
    let challenge = ready(provider.start_auth(AuthStartRequest {
        method: Some(AuthMethod::ApiToken),
        redirect_uri: None,
    }))
    .unwrap();
    ready(provider.complete_auth(AuthCompleteRequest {
        flow_id: challenge.flow_id,
        authorization_code: Some(paste("7")),
        redirect_uri: None,
    }))
    .unwrap();
}

#[test]
fn sign_in_presents_the_connection_prompt_and_secret_input() {
    let provider = Sub2apiProvider::with_api(StubApi::queue(vec![]));
    let challenge = ready(provider.start_auth(AuthStartRequest::default())).unwrap();
    assert_eq!(challenge.method, AuthMethod::ApiToken);
    assert_eq!(challenge.verification_uri, None);
    let input = challenge.input.unwrap();
    assert!(input.secret);
    assert!(input.prompt.contains("base_url admin_key upstream_ref"));
}

#[test]
fn complete_auth_resolves_an_id_reference_and_authenticates() {
    let provider = Sub2apiProvider::with_api(StubApi::queue(vec![Call::Account {
        id: 7,
        result: Ok(upstream_account()),
    }]));
    authenticate_by_id(&provider);
    assert_eq!(
        ready(provider.auth_status()).unwrap(),
        AuthState::Authenticated {
            account_label: Some("work".into()),
            account_key: ullage_auth::account_identity("http://127.0.0.1:55001#7"),
            expires_at: None,
        }
    );
}

#[test]
fn complete_auth_resolves_a_name_reference_with_spaces() {
    let provider = Sub2apiProvider::with_api(StubApi::queue(vec![Call::Accounts {
        page: 1,
        result: Ok(AccountsPage {
            items: vec![AdminAccount {
                id: 9,
                name: Some("my work account".into()),
                platform: Some("anthropic".into()),
                ..AdminAccount::default()
            }],
            total: 1,
            page: 1,
            page_size: 100,
            pages: 1,
        }),
    }]));
    let challenge = ready(provider.start_auth(AuthStartRequest::default())).unwrap();
    let state = ready(provider.complete_auth(AuthCompleteRequest {
        flow_id: challenge.flow_id,
        authorization_code: Some(paste("my work account")),
        redirect_uri: None,
    }))
    .unwrap();
    assert_eq!(
        state,
        AuthState::Authenticated {
            account_label: Some("my work account".into()),
            account_key: ullage_auth::account_identity("http://127.0.0.1:55001#9"),
            expires_at: None,
        }
    );
}

#[test]
fn complete_auth_searches_later_pages_for_a_name() {
    let filler = |id: i64| AdminAccount {
        id,
        name: Some(format!("other-{id}")),
        ..AdminAccount::default()
    };
    let provider = Sub2apiProvider::with_api(StubApi::queue(vec![
        Call::Accounts {
            page: 1,
            result: Ok(AccountsPage {
                items: (1..=100).map(filler).collect(),
                total: 101,
                page: 1,
                page_size: 100,
                pages: 2,
            }),
        },
        Call::Accounts {
            page: 2,
            result: Ok(AccountsPage {
                items: vec![upstream_account()],
                total: 101,
                page: 2,
                page_size: 100,
                pages: 2,
            }),
        },
    ]));
    let challenge = ready(provider.start_auth(AuthStartRequest::default())).unwrap();
    let state = ready(provider.complete_auth(AuthCompleteRequest {
        flow_id: challenge.flow_id,
        authorization_code: Some(paste("work")),
        redirect_uri: None,
    }))
    .unwrap();
    assert_eq!(
        state,
        AuthState::Authenticated {
            account_label: Some("work".into()),
            account_key: ullage_auth::account_identity("http://127.0.0.1:55001#7"),
            expires_at: None,
        }
    );
}

#[test]
fn complete_auth_continues_past_page_one_when_pages_is_absent() {
    // Older gateway responses document only items/total/page/page_size: a
    // missing `pages` field must not stop the lookup after page 1.
    let filler = |id: i64| AdminAccount {
        id,
        name: Some(format!("other-{id}")),
        ..AdminAccount::default()
    };
    let provider = Sub2apiProvider::with_api(StubApi::queue(vec![
        Call::Accounts {
            page: 1,
            result: Ok(AccountsPage {
                items: (1..=100).map(filler).collect(),
                total: 101,
                page: 1,
                page_size: 100,
                pages: 0,
            }),
        },
        Call::Accounts {
            page: 2,
            result: Ok(AccountsPage {
                items: vec![upstream_account()],
                total: 101,
                page: 2,
                page_size: 100,
                pages: 0,
            }),
        },
    ]));
    let challenge = ready(provider.start_auth(AuthStartRequest::default())).unwrap();
    let state = ready(provider.complete_auth(AuthCompleteRequest {
        flow_id: challenge.flow_id,
        authorization_code: Some(paste("work")),
        redirect_uri: None,
    }))
    .unwrap();
    assert_eq!(
        state,
        AuthState::Authenticated {
            account_label: Some("work".into()),
            account_key: ullage_auth::account_identity("http://127.0.0.1:55001#7"),
            expires_at: None,
        }
    );
}

#[test]
fn an_unknown_name_fails_sign_in() {
    let provider = Sub2apiProvider::with_api(StubApi::queue(vec![Call::Accounts {
        page: 1,
        result: Ok(AccountsPage {
            items: vec![upstream_account()],
            total: 1,
            page: 1,
            page_size: 100,
            pages: 1,
        }),
    }]));
    let challenge = ready(provider.start_auth(AuthStartRequest::default())).unwrap();
    assert!(matches!(
        ready(provider.complete_auth(AuthCompleteRequest {
            flow_id: challenge.flow_id,
            authorization_code: Some(paste("missing")),
            redirect_uri: None,
        })),
        Err(ProviderError::AuthenticationInvalid { .. })
    ));
}

#[test]
fn a_rejected_key_fails_sign_in_and_releases_the_flow() {
    let provider = Sub2apiProvider::with_api(StubApi::queue(vec![
        Call::Account {
            id: 7,
            result: Err(ApiFailure::authentication("rejected")),
        },
        Call::Account {
            id: 7,
            result: Ok(upstream_account()),
        },
    ]));
    let challenge = ready(provider.start_auth(AuthStartRequest::default())).unwrap();
    assert!(matches!(
        ready(provider.complete_auth(AuthCompleteRequest {
            flow_id: challenge.flow_id,
            authorization_code: Some(paste("7")),
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
            authorization_code: Some(paste("7")),
            redirect_uri: None,
        }))
        .is_ok()
    );
}

#[test]
fn a_malformed_paste_fails_before_any_call() {
    let provider = Sub2apiProvider::with_api(StubApi::queue(vec![]));
    let challenge = ready(provider.start_auth(AuthStartRequest::default())).unwrap();
    assert!(matches!(
        ready(provider.complete_auth(AuthCompleteRequest {
            flow_id: challenge.flow_id,
            authorization_code: Some("only-two fields".into()),
            redirect_uri: None,
        })),
        Err(ProviderError::AuthenticationInvalid { .. })
    ));
}

#[test]
fn an_expired_flow_cannot_complete_but_releases_the_slot() {
    let provider = Sub2apiProvider::with_api(StubApi::queue(vec![]));
    let challenge = ready(provider.start_auth(AuthStartRequest::default())).unwrap();
    provider.lock_state().unwrap().pending_flow = Some(PendingFlow {
        flow_id: challenge.flow_id.clone(),
        expires_at: Utc::now() - Duration::minutes(1),
    });
    assert!(matches!(
        ready(provider.complete_auth(AuthCompleteRequest {
            flow_id: challenge.flow_id,
            authorization_code: Some(paste("7")),
            redirect_uri: None,
        })),
        Err(ProviderError::AuthenticationInvalid { .. })
    ));
}

#[test]
fn query_forces_live_usage() {
    let provider = Sub2apiProvider::with_api(StubApi::queue(vec![
        Call::Account {
            id: 7,
            result: Ok(upstream_account()),
        },
        Call::Usage {
            id: 7,
            force: true,
            result: Box::new(Ok(stub_usage())),
        },
    ]));
    authenticate_by_id(&provider);
    let outcome = ready(provider.query(UsageQuery::default())).unwrap();
    let QueryOutcome::Complete { data } = outcome else {
        panic!("expected a complete outcome");
    };
    let normalized = provider.normalize(data).unwrap();
    assert_eq!(normalized.windows.len(), 2);
    assert_eq!(
        normalized.windows[0].window,
        ullage_core::UsageWindowKind::FiveHours
    );
    assert_eq!(normalized.plan.as_deref(), Some("PRO"));
}

#[test]
fn a_degraded_upstream_reports_a_partial_failure() {
    let provider = Sub2apiProvider::with_api(StubApi::queue(vec![
        Call::Account {
            id: 7,
            result: Ok(upstream_account()),
        },
        Call::Usage {
            id: 7,
            force: true,
            result: Box::new(Ok(UsageInfo {
                error_code: Some("unauthenticated".into()),
                error: Some("upstream token expired".into()),
                ..UsageInfo::default()
            })),
        },
    ]));
    authenticate_by_id(&provider);
    let outcome = ready(provider.query(UsageQuery::default())).unwrap();
    let QueryOutcome::Partial { failures, .. } = outcome else {
        panic!("expected a partial outcome");
    };
    assert_eq!(failures[0].scope, "unauthenticated");
    assert_eq!(failures[0].message, "upstream token expired");
    // The admin credential itself is fine: the session stays authenticated.
    assert!(matches!(
        ready(provider.auth_status()).unwrap(),
        AuthState::Authenticated { .. }
    ));
}

#[test]
fn a_deleted_upstream_account_invalidates_the_session() {
    let provider = Sub2apiProvider::with_api(StubApi::queue(vec![
        Call::Account {
            id: 7,
            result: Ok(upstream_account()),
        },
        Call::Usage {
            id: 7,
            force: true,
            result: Box::new(Err(ApiFailure::upstream_account_missing("gone"))),
        },
    ]));
    authenticate_by_id(&provider);
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
fn a_rejected_key_during_query_invalidates_the_session() {
    let provider = Sub2apiProvider::with_api(StubApi::queue(vec![
        Call::Account {
            id: 7,
            result: Ok(upstream_account()),
        },
        Call::Usage {
            id: 7,
            force: true,
            result: Box::new(Err(ApiFailure::authentication("revoked"))),
        },
    ]));
    authenticate_by_id(&provider);
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
fn the_account_key_binds_the_upstream_id_to_its_gateway() {
    // Two gateways may number their upstream accounts identically; binding
    // the key to the base URL stops duplicate-account retirement from ever
    // confusing a same-id account on another gateway.
    let session = |base_url: &str| GatewaySession {
        base_url: base_url.to_owned(),
        admin_key: Zeroizing::new("key".into()),
        upstream_id: 1,
        account_name: None,
    };
    let key = session("https://a.example").account_key().unwrap();
    assert_eq!(
        Some(key.clone()),
        ullage_auth::account_identity("https://a.example#1")
    );
    assert!(!key.contains("example"));
    assert_ne!(
        session("https://a.example").account_key(),
        session("https://b.example").account_key()
    );
}

#[test]
fn logout_clears_the_session() {
    let provider = Sub2apiProvider::with_api(StubApi::queue(vec![Call::Account {
        id: 7,
        result: Ok(upstream_account()),
    }]));
    authenticate_by_id(&provider);
    ready(provider.logout(LogoutRequest::default())).unwrap();
    assert_eq!(
        ready(provider.auth_status()).unwrap(),
        AuthState::NotAuthenticated
    );
}
