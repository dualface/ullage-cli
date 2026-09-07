use std::collections::VecDeque;
use std::future::Future;
use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
use std::sync::{Arc, Mutex};
use std::task::{Context, Poll, Waker};

use async_trait::async_trait;
use base64::Engine;
use base64::engine::general_purpose::URL_SAFE_NO_PAD;
use chrono::Utc;
use ullage_auth::{
    AuthCompleteRequest, AuthMethod, AuthStartRequest, AuthState, Availability, BackendKind,
    BackendScope, Credential, CredentialBackend, CredentialError, CredentialKey, CredentialStore,
    LogoutRequest, SecretValue,
};
use ullage_core::{
    MeasurementUnit, Provider, ProviderError, QueryOutcome, UsageQuery, UsageWindowKind,
};
use ullage_provider_cursor::{
    ApiFailure, CreditGrantsBalance, CurrentPeriodUsage, CursorApi, CursorProvider, CursorUsage,
    ExchangeTokens, HardLimit, PlanInfo, PlanInfoResponse, SecretString,
};

struct FakeApi {
    exchanges: Mutex<VecDeque<Result<ExchangeTokens, ApiFailure>>>,
    polls: Mutex<VecDeque<Result<Option<ExchangeTokens>, ApiFailure>>>,
    periods: Mutex<VecDeque<Result<CurrentPeriodUsage, ApiFailure>>>,
    plan: Result<PlanInfoResponse, ApiFailure>,
    grants: Result<CreditGrantsBalance, ApiFailure>,
    hard_limit: Result<HardLimit, ApiFailure>,
}

#[derive(Clone, Default)]
struct MemoryCredentialBackend {
    value: Arc<Mutex<Option<Vec<u8>>>>,
}

impl CredentialBackend for MemoryCredentialBackend {
    fn kind(&self) -> BackendKind {
        BackendKind::OtherPlatform
    }

    fn coordination_scope(&self) -> BackendScope {
        BackendScope::new(b"cursor-provider-restart-test")
    }

    fn probe(&self) -> Result<Availability, CredentialError> {
        Ok(Availability::Available)
    }

    fn read(&self, _: &CredentialKey) -> Result<Vec<u8>, CredentialError> {
        self.value
            .lock()
            .map_err(|_| CredentialError::Synchronization)?
            .clone()
            .ok_or(CredentialError::NotFound)
    }

    fn write(&self, _: &CredentialKey, value: &[u8]) -> Result<(), CredentialError> {
        *self
            .value
            .lock()
            .map_err(|_| CredentialError::Synchronization)? = Some(value.to_vec());
        Ok(())
    }
}

struct DelayedApi {
    release_exchange: Arc<AtomicBool>,
    fail_exchange: bool,
}

struct ConcurrentRefreshApi {
    exchange_calls: AtomicUsize,
    release_refreshes: Arc<AtomicBool>,
}

struct DelayedRefreshFailureApi {
    exchange_calls: AtomicUsize,
    release_failure: Arc<AtomicBool>,
}

struct DelayedReexchangeFailureApi {
    exchange_calls: AtomicUsize,
    release_failure: Arc<AtomicBool>,
}

struct RetryRefreshRaceApi {
    exchange_calls: AtomicUsize,
    period_calls: AtomicUsize,
    release_retry: Arc<AtomicBool>,
}

struct SwitchingApi {
    exchange_calls: AtomicUsize,
    release_current_period: Arc<AtomicBool>,
    fail_current_period: bool,
}

#[async_trait]
impl CursorApi for FakeApi {
    async fn exchange_user_api_key(&self, _: &str) -> Result<ExchangeTokens, ApiFailure> {
        self.exchanges.lock().unwrap().pop_front().unwrap()
    }

    async fn poll_login(&self, _: &str, _: &str) -> Result<Option<ExchangeTokens>, ApiFailure> {
        self.polls.lock().unwrap().pop_front().unwrap()
    }

    async fn current_period(&self, _: &str) -> Result<CurrentPeriodUsage, ApiFailure> {
        self.periods.lock().unwrap().pop_front().unwrap()
    }

    async fn plan_info(&self, _: &str) -> Result<PlanInfoResponse, ApiFailure> {
        self.plan.clone()
    }

    async fn credit_grants(&self, _: &str) -> Result<CreditGrantsBalance, ApiFailure> {
        self.grants.clone()
    }

    async fn hard_limit(&self, _: &str) -> Result<HardLimit, ApiFailure> {
        self.hard_limit.clone()
    }
}

#[async_trait]
impl CursorApi for DelayedApi {
    async fn exchange_user_api_key(&self, _: &str) -> Result<ExchangeTokens, ApiFailure> {
        std::future::poll_fn(|_| {
            if self.release_exchange.load(Ordering::SeqCst) {
                Poll::Ready(())
            } else {
                Poll::Pending
            }
        })
        .await;
        if self.fail_exchange {
            Err(ApiFailure::authentication("delayed exchange rejection"))
        } else {
            Ok(exchange("delayed-access"))
        }
    }

    async fn current_period(&self, _: &str) -> Result<CurrentPeriodUsage, ApiFailure> {
        unreachable!("the concurrency test does not query usage")
    }

    async fn plan_info(&self, _: &str) -> Result<PlanInfoResponse, ApiFailure> {
        unreachable!("the concurrency test does not query usage")
    }

    async fn credit_grants(&self, _: &str) -> Result<CreditGrantsBalance, ApiFailure> {
        unreachable!("the concurrency test does not query usage")
    }

    async fn hard_limit(&self, _: &str) -> Result<HardLimit, ApiFailure> {
        unreachable!("the concurrency test does not query usage")
    }
}

#[async_trait]
impl CursorApi for ConcurrentRefreshApi {
    async fn exchange_user_api_key(&self, _: &str) -> Result<ExchangeTokens, ApiFailure> {
        let call = self.exchange_calls.fetch_add(1, Ordering::SeqCst);
        if call > 0 {
            std::future::poll_fn(|_| {
                if self.release_refreshes.load(Ordering::SeqCst) {
                    Poll::Ready(())
                } else {
                    Poll::Pending
                }
            })
            .await;
        }
        if call == 2 {
            Err(ApiFailure {
                kind: ullage_provider_cursor::ApiFailureKind::Network,
                message: "concurrent refresh failed".into(),
                retry_after_seconds: None,
            })
        } else {
            Ok(exchange(&format!("access-{call}")))
        }
    }

    async fn current_period(&self, _: &str) -> Result<CurrentPeriodUsage, ApiFailure> {
        unreachable!("the concurrent refresh test does not query usage")
    }

    async fn plan_info(&self, _: &str) -> Result<PlanInfoResponse, ApiFailure> {
        unreachable!("the concurrent refresh test does not query usage")
    }

    async fn credit_grants(&self, _: &str) -> Result<CreditGrantsBalance, ApiFailure> {
        unreachable!("the concurrent refresh test does not query usage")
    }

    async fn hard_limit(&self, _: &str) -> Result<HardLimit, ApiFailure> {
        unreachable!("the concurrent refresh test does not query usage")
    }
}

#[async_trait]
impl CursorApi for DelayedRefreshFailureApi {
    async fn exchange_user_api_key(&self, _: &str) -> Result<ExchangeTokens, ApiFailure> {
        let call = self.exchange_calls.fetch_add(1, Ordering::SeqCst);
        if call == 0 {
            return Ok(exchange("initial-access"));
        }
        std::future::poll_fn(|_| {
            if self.release_failure.load(Ordering::SeqCst) {
                Poll::Ready(())
            } else {
                Poll::Pending
            }
        })
        .await;
        Err(ApiFailure::authentication("delayed exchange rejection"))
    }

    async fn current_period(&self, _: &str) -> Result<CurrentPeriodUsage, ApiFailure> {
        unreachable!("the refresh concurrency test does not query usage")
    }

    async fn plan_info(&self, _: &str) -> Result<PlanInfoResponse, ApiFailure> {
        unreachable!("the refresh concurrency test does not query usage")
    }

    async fn credit_grants(&self, _: &str) -> Result<CreditGrantsBalance, ApiFailure> {
        unreachable!("the refresh concurrency test does not query usage")
    }

    async fn hard_limit(&self, _: &str) -> Result<HardLimit, ApiFailure> {
        unreachable!("the refresh concurrency test does not query usage")
    }
}

#[async_trait]
impl CursorApi for DelayedReexchangeFailureApi {
    async fn exchange_user_api_key(&self, _: &str) -> Result<ExchangeTokens, ApiFailure> {
        let call = self.exchange_calls.fetch_add(1, Ordering::SeqCst);
        if call == 0 {
            return Ok(exchange("initial-access"));
        }
        std::future::poll_fn(|_| {
            if self.release_failure.load(Ordering::SeqCst) {
                Poll::Ready(())
            } else {
                Poll::Pending
            }
        })
        .await;
        Err(ApiFailure::authentication("delayed reexchange rejection"))
    }

    async fn current_period(&self, _: &str) -> Result<CurrentPeriodUsage, ApiFailure> {
        Err(ApiFailure::authentication("expired access token"))
    }

    async fn plan_info(&self, _: &str) -> Result<PlanInfoResponse, ApiFailure> {
        unreachable!("the reexchange concurrency test stops at current period")
    }

    async fn credit_grants(&self, _: &str) -> Result<CreditGrantsBalance, ApiFailure> {
        unreachable!("the reexchange concurrency test stops at current period")
    }

    async fn hard_limit(&self, _: &str) -> Result<HardLimit, ApiFailure> {
        unreachable!("the reexchange concurrency test stops at current period")
    }
}

#[async_trait]
impl CursorApi for RetryRefreshRaceApi {
    async fn exchange_user_api_key(&self, _: &str) -> Result<ExchangeTokens, ApiFailure> {
        let call = self.exchange_calls.fetch_add(1, Ordering::SeqCst);
        Ok(exchange(&format!("race-access-{call}")))
    }

    async fn current_period(&self, _: &str) -> Result<CurrentPeriodUsage, ApiFailure> {
        let call = self.period_calls.fetch_add(1, Ordering::SeqCst);
        if call > 0 {
            std::future::poll_fn(|_| {
                if self.release_retry.load(Ordering::SeqCst) {
                    Poll::Ready(())
                } else {
                    Poll::Pending
                }
            })
            .await;
        }
        Err(ApiFailure::authentication(if call == 0 {
            "expired access token"
        } else {
            "retry token rejected"
        }))
    }

    async fn plan_info(&self, _: &str) -> Result<PlanInfoResponse, ApiFailure> {
        unreachable!("the retry race test stops at current period")
    }

    async fn credit_grants(&self, _: &str) -> Result<CreditGrantsBalance, ApiFailure> {
        unreachable!("the retry race test stops at current period")
    }

    async fn hard_limit(&self, _: &str) -> Result<HardLimit, ApiFailure> {
        unreachable!("the retry race test stops at current period")
    }
}

#[async_trait]
impl CursorApi for SwitchingApi {
    async fn exchange_user_api_key(&self, _: &str) -> Result<ExchangeTokens, ApiFailure> {
        let call = self.exchange_calls.fetch_add(1, Ordering::SeqCst);
        let mut tokens = exchange(&format!("switch-access-{call}"));
        tokens.email = Some(if call == 0 {
            "USER@EXAMPLE.COM".into()
        } else {
            format!("account-{call}@example.com")
        });
        Ok(tokens)
    }

    async fn current_period(&self, _: &str) -> Result<CurrentPeriodUsage, ApiFailure> {
        std::future::poll_fn(|_| {
            if self.release_current_period.load(Ordering::SeqCst) {
                Poll::Ready(())
            } else {
                Poll::Pending
            }
        })
        .await;
        if self.fail_current_period {
            return Err(ApiFailure {
                kind: ullage_provider_cursor::ApiFailureKind::Network,
                message: "delayed current period failure".into(),
                retry_after_seconds: None,
            });
        }
        Ok(serde_json::from_str(include_str!("fixtures/usage.json")).unwrap())
    }

    async fn plan_info(&self, _: &str) -> Result<PlanInfoResponse, ApiFailure> {
        Ok(PlanInfoResponse::default())
    }

    async fn credit_grants(&self, _: &str) -> Result<CreditGrantsBalance, ApiFailure> {
        Ok(CreditGrantsBalance::default())
    }

    async fn hard_limit(&self, _: &str) -> Result<HardLimit, ApiFailure> {
        Ok(HardLimit::default())
    }
}

fn run_ready<F: Future>(future: F) -> F::Output {
    let mut context = Context::from_waker(Waker::noop());
    let mut future = Box::pin(future);
    match future.as_mut().poll(&mut context) {
        Poll::Ready(output) => output,
        Poll::Pending => panic!("fake future unexpectedly yielded"),
    }
}

fn exchange(token: &str) -> ExchangeTokens {
    ExchangeTokens {
        access_token: SecretString::new(token),
        refresh_token: Some(SecretString::new("redacted-refresh-token")),
        email: Some("USER@EXAMPLE.COM".into()),
    }
}

fn fake_api(periods: Vec<Result<CurrentPeriodUsage, ApiFailure>>) -> Arc<FakeApi> {
    Arc::new(FakeApi {
        exchanges: Mutex::new(VecDeque::from([
            Ok(exchange("first-access")),
            Ok(exchange("second-access")),
        ])),
        polls: Mutex::new(VecDeque::new()),
        periods: Mutex::new(periods.into()),
        plan: Ok(PlanInfoResponse {
            plan_info: Some(PlanInfo {
                plan_name: Some("Pro+".into()),
                included_amount_cents: Some(7000.0),
                ..PlanInfo::default()
            }),
            ..PlanInfoResponse::default()
        }),
        grants: Ok(CreditGrantsBalance {
            balance_cents: Some(500.0),
            granted_cents: Some(1000.0),
            ..CreditGrantsBalance::default()
        }),
        hard_limit: Ok(
            serde_json::from_str(include_str!("fixtures/hard-limit-disabled.json")).unwrap(),
        ),
    })
}

fn authenticate(provider: &CursorProvider) {
    let challenge = run_ready(provider.start_auth(AuthStartRequest {
        method: Some(AuthMethod::ApiToken),
        redirect_uri: None,
    }))
    .unwrap();
    assert_eq!(challenge.method, AuthMethod::ApiToken);
    assert!(challenge.verification_uri.is_some());
    let state = run_ready(provider.complete_auth(AuthCompleteRequest {
        flow_id: challenge.flow_id,
        authorization_code: Some("redacted-user-api-key".into()),
        redirect_uri: None,
    }))
    .unwrap();
    assert!(matches!(
        state,
        AuthState::Authenticated {
            account_label: Some(ref label),
            ..
        } if label == "user@example.com"
    ));
}

#[test]
fn exchanges_reexchanges_and_logs_out_without_exposing_secrets() {
    let period: CurrentPeriodUsage =
        serde_json::from_str(include_str!("fixtures/usage.json")).unwrap();
    let api = fake_api(vec![
        Err(ApiFailure::authentication("expired access token")),
        Ok(period),
    ]);
    let provider = CursorProvider::with_api(api);
    authenticate(&provider);

    let outcome = run_ready(provider.query(UsageQuery::default())).unwrap();
    assert!(matches!(outcome, QueryOutcome::Complete { .. }));
    assert!(matches!(
        run_ready(provider.auth_status()).unwrap(),
        AuthState::Authenticated { .. }
    ));
    run_ready(provider.logout(LogoutRequest::default())).unwrap();
    assert_eq!(
        run_ready(provider.auth_status()).unwrap(),
        AuthState::NotAuthenticated
    );
}

#[test]
fn repeated_authentication_rejection_marks_the_session_invalid() {
    let api = fake_api(vec![
        Err(ApiFailure::authentication("expired access token")),
        Err(ApiFailure::authentication("revoked API key")),
    ]);
    let provider = CursorProvider::with_api(api);
    authenticate(&provider);

    assert!(matches!(
        run_ready(provider.query(UsageQuery::default())),
        Err(ProviderError::AuthenticationInvalid { .. })
    ));
    assert!(matches!(
        run_ready(provider.auth_status()).unwrap(),
        AuthState::Invalid { .. }
    ));
}

#[test]
fn query_ignores_a_custom_display_label() {
    let period: CurrentPeriodUsage =
        serde_json::from_str(include_str!("fixtures/usage.json")).unwrap();
    let provider = CursorProvider::with_api(fake_api(vec![Ok(period)]));
    authenticate(&provider);

    let outcome = run_ready(provider.query(UsageQuery {
        account_label: Some("work".into()),
    }))
    .unwrap();
    assert!(matches!(outcome, QueryOutcome::Complete { .. }));
    let QueryOutcome::Complete { data } = outcome else {
        unreachable!("query succeeded with complete outcome");
    };
    assert_eq!(data.account_label.as_deref(), Some("user@example.com"));
}

/// A Cursor session token with the given expiry. Only the payload is read, so
/// the header and signature are filler.
fn session_token(expires_in_seconds: i64) -> String {
    let exp = Utc::now().timestamp() + expires_in_seconds;
    let payload = URL_SAFE_NO_PAD.encode(format!(r#"{{"exp":{exp}}}"#));
    format!("header.{payload}.signature")
}

#[test]
fn browser_sign_in_polls_then_stores_a_session_that_needs_no_exchange() {
    let store = Arc::new(CredentialStore::new(MemoryCredentialBackend::default()));
    let api = fake_api(Vec::new());
    let token = session_token(60 * 60);
    api.polls
        .lock()
        .unwrap()
        .extend([Ok(None), Ok(Some(exchange(&token)))]);
    let provider = CursorProvider::with_api_and_store(api.clone(), store.clone()).unwrap();

    let challenge = run_ready(provider.start_auth(AuthStartRequest {
        method: None,
        redirect_uri: None,
    }))
    .unwrap();
    assert_eq!(challenge.method, AuthMethod::DeviceCode);
    assert!(challenge.input.is_none());
    let login_url = challenge.verification_uri.clone().unwrap();
    assert!(login_url.starts_with("https://cursor.com/loginDeepControl?challenge="));
    assert!(login_url.contains("&mode=login&redirectTarget=cli"));

    let complete = |flow_id: String| {
        run_ready(provider.complete_auth(AuthCompleteRequest {
            flow_id,
            authorization_code: None,
            redirect_uri: None,
        }))
    };
    assert!(matches!(
        complete(challenge.flow_id.clone()).unwrap(),
        AuthState::Pending { ref flow_id, .. } if *flow_id == challenge.flow_id
    ));
    let authenticated = complete(challenge.flow_id.clone()).unwrap();
    assert!(matches!(
        authenticated,
        AuthState::Authenticated {
            account_label: Some(ref label),
            expires_at: Some(_),
            ..
        } if label == "user@example.com"
    ));
    drop(provider);

    // The stored session is used as-is. The fake would hand out an API key
    // exchange here, so reaching for one would change the access token.
    let restored = CursorProvider::with_api_and_store(api.clone(), store.clone()).unwrap();
    assert!(matches!(
        run_ready(restored.auth_status()).unwrap(),
        AuthState::Authenticated {
            expires_at: Some(_),
            ..
        }
    ));
    assert_eq!(api.exchanges.lock().unwrap().len(), 2);
    // Nothing can renew a browser session, so an expired one asks for a new one.
    assert!(matches!(
        run_ready(restored.refresh_auth()).unwrap(),
        AuthState::Authenticated { .. }
    ));
    drop(restored);

    store
        .set(
            &CredentialKey::new("cursor", "active").unwrap(),
            expired_session_credential(),
        )
        .unwrap();
    let stale = CursorProvider::with_api_and_store(api, store).unwrap();
    assert!(matches!(
        run_ready(stale.auth_status()).unwrap(),
        AuthState::Invalid { .. }
    ));
}

fn expired_session_credential() -> Credential {
    let mut credential = Credential::new();
    credential
        .insert(
            "access_token",
            SecretValue::new(session_token(-60).as_bytes()),
        )
        .unwrap();
    credential
}

#[test]
fn authentication_restores_from_the_shared_credential_store() {
    let store = Arc::new(CredentialStore::new(MemoryCredentialBackend::default()));
    let api = fake_api(Vec::new());
    let first = CursorProvider::with_api_and_store(api.clone(), store.clone()).unwrap();
    authenticate(&first);
    drop(first);

    let restored = CursorProvider::with_api_and_store(api, store.clone()).unwrap();
    assert!(matches!(
        run_ready(restored.auth_status()).unwrap(),
        AuthState::Authenticated {
            account_label: Some(ref label),
            ..
        } if label == "user@example.com"
    ));
    run_ready(restored.logout(LogoutRequest {
        account_label: Some("user@example.com".into()),
    }))
    .unwrap();
    drop(restored);

    let unauthenticated = CursorProvider::with_api_and_store(fake_api(Vec::new()), store).unwrap();
    assert_eq!(
        run_ready(unauthenticated.auth_status()).unwrap(),
        AuthState::NotAuthenticated
    );
}

#[test]
fn mismatched_logout_selector_preserves_the_authenticated_session() {
    let provider = CursorProvider::with_api(fake_api(Vec::new()));
    authenticate(&provider);

    assert!(matches!(
        run_ready(provider.logout(LogoutRequest {
            account_label: Some("another@example.com".into()),
        })),
        Err(ProviderError::AuthenticationInvalid { .. })
    ));
    assert!(matches!(
        run_ready(provider.auth_status()).unwrap(),
        AuthState::Authenticated {
            account_label: Some(ref label),
            ..
        } if label == "user@example.com"
    ));
}

#[test]
fn stale_persisted_credentials_do_not_block_reauthentication_or_local_logout() {
    let login_store = Arc::new(CredentialStore::new(MemoryCredentialBackend::default()));
    let initial =
        CursorProvider::with_api_and_store(fake_api(Vec::new()), login_store.clone()).unwrap();
    authenticate(&initial);
    drop(initial);
    let rejected = fake_api(Vec::new());
    *rejected.exchanges.lock().unwrap() =
        VecDeque::from([Err(ApiFailure::authentication("revoked persisted API key"))]);
    let replacement = CursorProvider::with_api_and_store(rejected, login_store.clone()).unwrap();
    assert!(
        run_ready(replacement.start_auth(AuthStartRequest {
            method: Some(AuthMethod::ApiToken),
            redirect_uri: None,
        }))
        .is_ok()
    );

    let logout_store = Arc::new(CredentialStore::new(MemoryCredentialBackend::default()));
    let initial =
        CursorProvider::with_api_and_store(fake_api(Vec::new()), logout_store.clone()).unwrap();
    authenticate(&initial);
    drop(initial);
    let offline = fake_api(Vec::new());
    *offline.exchanges.lock().unwrap() = VecDeque::from([Err(ApiFailure {
        kind: ullage_provider_cursor::ApiFailureKind::Network,
        message: "offline while restoring persisted key".into(),
        retry_after_seconds: None,
    })]);
    let logout = CursorProvider::with_api_and_store(offline, logout_store.clone()).unwrap();
    run_ready(logout.logout(LogoutRequest::default())).unwrap();
    let cleared = CursorProvider::with_api_and_store(fake_api(Vec::new()), logout_store).unwrap();
    assert_eq!(
        run_ready(cleared.auth_status()).unwrap(),
        AuthState::NotAuthenticated
    );
}

#[test]
fn maps_monthly_usage_bonus_on_demand_and_vendor_categories() {
    let period: CurrentPeriodUsage =
        serde_json::from_str(include_str!("fixtures/usage.json")).unwrap();
    assert!(
        period
            .plan_usage
            .as_ref()
            .unwrap()
            .categories
            .contains_key("experimentalPercentUsed")
    );
    let provider = CursorProvider::with_api(fake_api(vec![Ok(period)]));
    authenticate(&provider);
    let vendor = match run_ready(provider.query(UsageQuery::default())).unwrap() {
        QueryOutcome::Complete { data } => data,
        other => panic!("unexpected outcome: {other:?}"),
    };
    let normalized = provider.normalize(vendor).unwrap();

    assert_eq!(normalized.plan.as_deref(), Some("Pro+"));
    assert_eq!(
        normalized.account_label.as_deref(),
        Some("user@example.com")
    );
    assert_eq!(normalized.subscription_expires_at, None);
    assert_eq!(normalized.windows.len(), 1);
    assert_eq!(normalized.windows[0].window, UsageWindowKind::Monthly);
    assert!(normalized.windows[0].resets_at.is_some());
    for name in [
        "total_spend",
        "included_spend",
        "bonus_spend",
        "bonus_balance",
        "on_demand_spend",
        "auto",
        "api",
        "total",
        "experimental",
        "on_demand_enabled",
        "on_demand_hard_limit",
    ] {
        assert!(
            normalized.windows[0]
                .measurements
                .iter()
                .any(|measurement| measurement.name == name),
            "missing measurement {name}"
        );
    }
    assert!(
        normalized.windows[0]
            .measurements
            .iter()
            .any(|measurement| {
                measurement.name == "auto" && measurement.unit == MeasurementUnit::Percent
            })
    );
    let on_demand_enabled = normalized.windows[0]
        .measurements
        .iter()
        .find(|measurement| measurement.name == "on_demand_enabled")
        .unwrap();
    assert_eq!(on_demand_enabled.used, 0.0);
    assert_eq!(on_demand_enabled.limit, Some(1.0));
    assert_eq!(
        on_demand_enabled.unit,
        MeasurementUnit::Other {
            id: "boolean".into(),
            label: "Enabled".into(),
        }
    );
    let hard_limit = normalized.windows[0]
        .measurements
        .iter()
        .find(|measurement| measurement.name == "on_demand_hard_limit")
        .unwrap();
    assert_eq!(hard_limit.limit, Some(0.0));
}

#[test]
fn reports_missing_plan_and_optional_network_failure_as_partial_data() {
    let period: CurrentPeriodUsage =
        serde_json::from_str(include_str!("fixtures/plan-missing.json")).unwrap();
    let mut api = Arc::try_unwrap(fake_api(vec![Ok(period)])).ok().unwrap();
    api.plan = Err(ApiFailure {
        kind: ullage_provider_cursor::ApiFailureKind::Network,
        message: "plan endpoint unavailable".into(),
        retry_after_seconds: None,
    });
    let provider = CursorProvider::with_api(Arc::new(api));
    authenticate(&provider);

    match run_ready(provider.query(UsageQuery::default())).unwrap() {
        QueryOutcome::Partial { data, failures } => {
            assert!(data.current_period.plan_usage.is_none());
            assert!(
                failures
                    .iter()
                    .any(|failure| failure.scope == "current_period.plan_usage")
            );
            assert!(
                failures
                    .iter()
                    .any(|failure| failure.scope == "plan_info.network")
            );
        }
        other => panic!("unexpected outcome: {other:?}"),
    }
}

#[test]
fn classifies_invalid_api_key_and_rejects_stale_flow() {
    let api = Arc::new(FakeApi {
        exchanges: Mutex::new(VecDeque::from([Err(ApiFailure::authentication(
            "invalid API key",
        ))])),
        polls: Mutex::new(VecDeque::new()),
        periods: Mutex::new(VecDeque::new()),
        plan: Ok(PlanInfoResponse::default()),
        grants: Ok(CreditGrantsBalance::default()),
        hard_limit: Ok(HardLimit::default()),
    });
    let provider = CursorProvider::with_api(api);
    let error = run_ready(provider.complete_auth(AuthCompleteRequest {
        flow_id: "stale".into(),
        authorization_code: Some("redacted".into()),
        redirect_uri: None,
    }))
    .unwrap_err();
    assert!(matches!(error, ProviderError::ProtocolIncompatible { .. }));

    let challenge = run_ready(provider.start_auth(AuthStartRequest {
        method: Some(AuthMethod::ApiToken),
        redirect_uri: None,
    }))
    .unwrap();
    let error = run_ready(provider.complete_auth(AuthCompleteRequest {
        flow_id: challenge.flow_id,
        authorization_code: Some("redacted".into()),
        redirect_uri: None,
    }))
    .unwrap_err();
    assert!(matches!(error, ProviderError::AuthenticationInvalid { .. }));
    assert!(matches!(
        run_ready(provider.auth_status()).unwrap(),
        AuthState::Invalid { .. }
    ));
}

#[test]
fn logout_supersedes_an_in_flight_exchange() {
    let release_exchange = Arc::new(AtomicBool::new(false));
    let provider = CursorProvider::with_api(Arc::new(DelayedApi {
        release_exchange: release_exchange.clone(),
        fail_exchange: false,
    }));
    let challenge = run_ready(provider.start_auth(AuthStartRequest {
        method: Some(AuthMethod::ApiToken),
        redirect_uri: None,
    }))
    .unwrap();
    let mut completion = Box::pin(provider.complete_auth(AuthCompleteRequest {
        flow_id: challenge.flow_id,
        authorization_code: Some("redacted-user-api-key".into()),
        redirect_uri: None,
    }));
    let mut context = Context::from_waker(Waker::noop());
    assert!(matches!(
        completion.as_mut().poll(&mut context),
        Poll::Pending
    ));

    run_ready(provider.logout(LogoutRequest::default())).unwrap();
    release_exchange.store(true, Ordering::SeqCst);
    assert!(matches!(
        completion.as_mut().poll(&mut context),
        Poll::Ready(Err(ProviderError::ProtocolIncompatible { .. }))
    ));
    drop(completion);
    assert_eq!(
        run_ready(provider.auth_status()).unwrap(),
        AuthState::NotAuthenticated
    );
}

#[test]
fn logout_supersedes_an_in_flight_authentication_exchange_failure() {
    let release_exchange = Arc::new(AtomicBool::new(false));
    let provider = CursorProvider::with_api(Arc::new(DelayedApi {
        release_exchange: release_exchange.clone(),
        fail_exchange: true,
    }));
    let challenge = run_ready(provider.start_auth(AuthStartRequest {
        method: Some(AuthMethod::ApiToken),
        redirect_uri: None,
    }))
    .unwrap();
    let mut completion = Box::pin(provider.complete_auth(AuthCompleteRequest {
        flow_id: challenge.flow_id,
        authorization_code: Some("redacted-user-api-key".into()),
        redirect_uri: None,
    }));
    let mut context = Context::from_waker(Waker::noop());
    assert!(matches!(
        completion.as_mut().poll(&mut context),
        Poll::Pending
    ));

    run_ready(provider.logout(LogoutRequest::default())).unwrap();
    release_exchange.store(true, Ordering::SeqCst);
    assert!(matches!(
        completion.as_mut().poll(&mut context),
        Poll::Ready(Err(ProviderError::ProtocolIncompatible { .. }))
    ));
}

#[test]
fn concurrent_refreshes_share_the_first_installed_session() {
    let release_refreshes = Arc::new(AtomicBool::new(false));
    let provider = CursorProvider::with_api(Arc::new(ConcurrentRefreshApi {
        exchange_calls: AtomicUsize::new(0),
        release_refreshes: release_refreshes.clone(),
    }));
    authenticate(&provider);

    let mut first = Box::pin(provider.refresh_auth());
    let mut second = Box::pin(provider.refresh_auth());
    let mut context = Context::from_waker(Waker::noop());
    assert!(matches!(first.as_mut().poll(&mut context), Poll::Pending));
    assert!(matches!(second.as_mut().poll(&mut context), Poll::Pending));

    release_refreshes.store(true, Ordering::SeqCst);
    assert!(matches!(
        first.as_mut().poll(&mut context),
        Poll::Ready(Ok(AuthState::Authenticated { .. }))
    ));
    assert!(matches!(
        second.as_mut().poll(&mut context),
        Poll::Ready(Ok(AuthState::Authenticated { .. }))
    ));
    assert!(matches!(
        run_ready(provider.auth_status()).unwrap(),
        AuthState::Authenticated { .. }
    ));
}

#[test]
fn replacement_flow_supersedes_an_in_flight_refresh_failure() {
    let release_failure = Arc::new(AtomicBool::new(false));
    let provider = CursorProvider::with_api(Arc::new(DelayedRefreshFailureApi {
        exchange_calls: AtomicUsize::new(0),
        release_failure: release_failure.clone(),
    }));
    authenticate(&provider);

    let mut refresh = Box::pin(provider.refresh_auth());
    let mut context = Context::from_waker(Waker::noop());
    assert!(matches!(refresh.as_mut().poll(&mut context), Poll::Pending));
    run_ready(provider.start_auth(AuthStartRequest {
        method: Some(AuthMethod::ApiToken),
        redirect_uri: None,
    }))
    .unwrap();
    release_failure.store(true, Ordering::SeqCst);

    assert!(matches!(
        refresh.as_mut().poll(&mut context),
        Poll::Ready(Err(ProviderError::ProtocolIncompatible { .. }))
    ));
    assert!(matches!(
        run_ready(provider.auth_status()).unwrap(),
        AuthState::Authenticated { .. }
    ));
}

#[test]
fn logout_supersedes_an_in_flight_query_reexchange_failure() {
    let release_failure = Arc::new(AtomicBool::new(false));
    let provider = CursorProvider::with_api(Arc::new(DelayedReexchangeFailureApi {
        exchange_calls: AtomicUsize::new(0),
        release_failure: release_failure.clone(),
    }));
    authenticate(&provider);

    let mut query = Box::pin(provider.query(UsageQuery::default()));
    let mut context = Context::from_waker(Waker::noop());
    assert!(matches!(query.as_mut().poll(&mut context), Poll::Pending));
    run_ready(provider.logout(LogoutRequest::default())).unwrap();
    release_failure.store(true, Ordering::SeqCst);

    assert!(matches!(
        query.as_mut().poll(&mut context),
        Poll::Ready(Err(ProviderError::ProtocolIncompatible { .. }))
    ));
}

#[test]
fn reexchange_rejects_a_changed_account_identity() {
    let period: CurrentPeriodUsage =
        serde_json::from_str(include_str!("fixtures/usage.json")).unwrap();
    let api = fake_api(vec![
        Err(ApiFailure::authentication("expired access token")),
        Ok(period),
    ]);
    api.exchanges.lock().unwrap()[1].as_mut().unwrap().email =
        Some("different-account@example.com".into());
    let provider = CursorProvider::with_api(api);
    authenticate(&provider);

    assert!(matches!(
        run_ready(provider.query(UsageQuery::default())),
        Err(ProviderError::ProtocolIncompatible { .. })
    ));
    assert!(matches!(
        run_ready(provider.auth_status()).unwrap(),
        AuthState::Authenticated {
            account_label: Some(ref label),
            ..
        } if label == "user@example.com"
    ));
}

#[test]
fn concurrent_refresh_supersedes_an_old_retry_authentication_failure() {
    let release_retry = Arc::new(AtomicBool::new(false));
    let provider = CursorProvider::with_api(Arc::new(RetryRefreshRaceApi {
        exchange_calls: AtomicUsize::new(0),
        period_calls: AtomicUsize::new(0),
        release_retry: release_retry.clone(),
    }));
    authenticate(&provider);

    let mut query = Box::pin(provider.query(UsageQuery::default()));
    let mut context = Context::from_waker(Waker::noop());
    assert!(matches!(query.as_mut().poll(&mut context), Poll::Pending));
    assert!(matches!(
        run_ready(provider.refresh_auth()),
        Ok(AuthState::Authenticated { .. })
    ));
    release_retry.store(true, Ordering::SeqCst);

    assert!(matches!(
        query.as_mut().poll(&mut context),
        Poll::Ready(Err(ProviderError::ProtocolIncompatible { .. }))
    ));
    assert!(matches!(
        run_ready(provider.auth_status()).unwrap(),
        AuthState::Authenticated { .. }
    ));
}

#[test]
fn pending_replacement_flow_rejects_old_session_refresh_without_cancelling_the_flow() {
    let provider = CursorProvider::with_api(fake_api(Vec::new()));
    authenticate(&provider);
    let replacement = run_ready(provider.start_auth(AuthStartRequest {
        method: Some(AuthMethod::ApiToken),
        redirect_uri: None,
    }))
    .unwrap();

    assert!(matches!(
        run_ready(provider.refresh_auth()),
        Err(ProviderError::ProtocolIncompatible { .. })
    ));
    assert!(matches!(
        run_ready(provider.complete_auth(AuthCompleteRequest {
            flow_id: replacement.flow_id,
            authorization_code: Some("redacted-replacement-key".into()),
            redirect_uri: None,
        })),
        Ok(AuthState::Authenticated { .. })
    ));
}

#[test]
fn account_switch_supersedes_an_in_flight_usage_query() {
    let release_current_period = Arc::new(AtomicBool::new(false));
    let provider = CursorProvider::with_api(Arc::new(SwitchingApi {
        exchange_calls: AtomicUsize::new(0),
        release_current_period: release_current_period.clone(),
        fail_current_period: false,
    }));
    authenticate(&provider);
    let mut query = Box::pin(provider.query(UsageQuery::default()));
    let mut context = Context::from_waker(Waker::noop());
    assert!(matches!(query.as_mut().poll(&mut context), Poll::Pending));

    let replacement = run_ready(provider.start_auth(AuthStartRequest {
        method: Some(AuthMethod::ApiToken),
        redirect_uri: None,
    }))
    .unwrap();
    run_ready(provider.complete_auth(AuthCompleteRequest {
        flow_id: replacement.flow_id,
        authorization_code: Some("redacted-replacement-key".into()),
        redirect_uri: None,
    }))
    .unwrap();
    release_current_period.store(true, Ordering::SeqCst);

    assert!(matches!(
        query.as_mut().poll(&mut context),
        Poll::Ready(Err(ProviderError::ProtocolIncompatible { .. }))
    ));
}

#[test]
fn account_switch_takes_precedence_over_an_in_flight_query_failure() {
    let release_current_period = Arc::new(AtomicBool::new(false));
    let provider = CursorProvider::with_api(Arc::new(SwitchingApi {
        exchange_calls: AtomicUsize::new(0),
        release_current_period: release_current_period.clone(),
        fail_current_period: true,
    }));
    authenticate(&provider);
    let mut query = Box::pin(provider.query(UsageQuery::default()));
    let mut context = Context::from_waker(Waker::noop());
    assert!(matches!(query.as_mut().poll(&mut context), Poll::Pending));

    run_ready(provider.logout(LogoutRequest::default())).unwrap();
    release_current_period.store(true, Ordering::SeqCst);
    assert!(matches!(
        query.as_mut().poll(&mut context),
        Poll::Ready(Err(ProviderError::ProtocolIncompatible { .. }))
    ));
}

#[test]
fn rejects_out_of_range_billing_cycle_end_values() {
    let provider = CursorProvider::with_api(fake_api(Vec::new()));
    for json in [
        r#"{"billingCycleEnd":"9223372036854775807"}"#,
        r#"{"billingCycleEnd":9223372036854775807}"#,
    ] {
        let current_period: CurrentPeriodUsage = serde_json::from_str(json).unwrap();
        let error = provider
            .normalize(CursorUsage {
                account_label: None,
                current_period,
                plan: None,
                credit_grants: None,
                hard_limit: None,
                observed_at: Utc::now(),
            })
            .unwrap_err();
        assert!(matches!(error, ProviderError::ProtocolIncompatible { .. }));
    }
}

#[test]
fn vendor_dto_round_trips_unknown_fields() {
    let current_period: CurrentPeriodUsage =
        serde_json::from_str(include_str!("fixtures/usage.json")).unwrap();
    let usage = CursorUsage {
        account_label: None,
        current_period,
        plan: None,
        credit_grants: None,
        hard_limit: None,
        observed_at: Utc::now(),
    };
    let encoded = serde_json::to_value(&usage).unwrap();
    assert_eq!(
        encoded["currentPeriod"]["planUsage"]["experimentalPercentUsed"],
        4
    );
}
