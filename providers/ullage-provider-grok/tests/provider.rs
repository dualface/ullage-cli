use std::future::Future;
use std::io::{Read, Write};
use std::net::TcpListener;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::{Arc, Mutex};
use std::task::{Context, Poll, Waker};

use async_trait::async_trait;
use chrono::{DateTime, TimeZone, Utc};
use serde_json::{Value, json};
use ullage_auth::{
    AuthCompleteRequest, AuthMethod, AuthStartRequest, AuthState, Availability, BackendKind,
    BackendScope, Credential, CredentialBackend, CredentialError, CredentialKey, CredentialStore,
    LogoutRequest, SecretValue,
};
use ullage_core::{
    PartialFailure, Provider, ProviderError, QueryOutcome, UsageQuery, UsageWindowKind,
};
use ullage_provider_grok::{
    BrowserAuthorization, DeviceAuthorization, GrokApiError, GrokProvider, GrokTransport,
    HttpGrokConfig, HttpGrokTransport, NormalizedTier, OAuthPoll, OAuthToken, parse_billing,
};

fn fixture(contents: &str) -> Value {
    serde_json::from_str(contents).unwrap()
}

fn run_ready<F: Future>(future: F) -> F::Output {
    let mut context = Context::from_waker(Waker::noop());
    let mut future = Box::pin(future);
    match future.as_mut().poll(&mut context) {
        Poll::Ready(output) => output,
        Poll::Pending => panic!("mock future unexpectedly yielded"),
    }
}

#[derive(Default)]
struct MockTransport {
    polls: Mutex<usize>,
    revoked: Mutex<bool>,
    revoke_failures: Mutex<usize>,
    refreshes: AtomicUsize,
    browser_codes: Mutex<Vec<String>>,
    refresh_account: Option<String>,
    initial_token_expires_soon: bool,
    hang_settings: bool,
    delay_billing: Option<std::time::Duration>,
    billing: Mutex<Option<Value>>,
    settings: Mutex<Option<Result<Value, GrokApiError>>>,
}

#[derive(Default)]
struct MemoryCredentialBackend {
    values: Mutex<std::collections::BTreeMap<String, Vec<u8>>>,
    write_failures: Arc<AtomicUsize>,
}

impl CredentialBackend for MemoryCredentialBackend {
    fn kind(&self) -> BackendKind {
        BackendKind::ExplicitFileFallback
    }

    fn coordination_scope(&self) -> BackendScope {
        BackendScope::new(b"grok-provider-persistence-test")
    }

    fn probe(&self) -> Result<Availability, CredentialError> {
        Ok(Availability::Available)
    }

    fn read(&self, key: &CredentialKey) -> Result<Vec<u8>, CredentialError> {
        let identity = format!("{}:{}", key.service_name(), key.entry_name());
        self.values
            .lock()
            .unwrap()
            .get(&identity)
            .cloned()
            .ok_or(CredentialError::NotFound)
    }

    fn write(&self, key: &CredentialKey, value: &[u8]) -> Result<(), CredentialError> {
        if self
            .write_failures
            .fetch_update(Ordering::SeqCst, Ordering::SeqCst, |remaining| {
                remaining.checked_sub(1)
            })
            .is_ok()
        {
            return Err(CredentialError::BackendFailure);
        }
        let identity = format!("{}:{}", key.service_name(), key.entry_name());
        self.values.lock().unwrap().insert(identity, value.to_vec());
        Ok(())
    }
}

fn token(label: &str) -> OAuthToken {
    OAuthToken {
        access_token: "access-token-never-logged".into(),
        refresh_token: Some("refresh-token-never-logged".into()),
        expires_at: Some(Utc.with_ymd_and_hms(2099, 1, 1, 0, 0, 0).unwrap()),
        account_label: Some(label.into()),
    }
}

#[async_trait]
impl GrokTransport for MockTransport {
    fn browser_redirect_uri(&self) -> &str {
        "http://127.0.0.1/callback"
    }

    async fn start_device_authorization(&self) -> Result<DeviceAuthorization, GrokApiError> {
        Ok(DeviceAuthorization {
            flow_id: "device-flow".into(),
            device_code: "private-device-code".into(),
            user_code: "SAFE-CODE".into(),
            verification_uri: "https://accounts.example.invalid/device".into(),
            expires_at: Some(Utc::now() + chrono::Duration::minutes(10)),
        })
    }

    async fn poll_device_authorization(&self, _: &str) -> Result<OAuthPoll, GrokApiError> {
        let mut polls = self.polls.lock().unwrap();
        *polls += 1;
        if *polls == 1 {
            Ok(OAuthPoll::Pending)
        } else {
            let mut authorized = token("device-account");
            if self.initial_token_expires_soon {
                authorized.expires_at = Some(Utc::now() + chrono::Duration::milliseconds(20));
            }
            Ok(OAuthPoll::Authorized(authorized))
        }
    }

    async fn start_browser_authorization(
        &self,
        _: &str,
    ) -> Result<BrowserAuthorization, GrokApiError> {
        Ok(BrowserAuthorization {
            flow_id: "browser-state".into(),
            authorization_uri: "https://accounts.example.invalid/oauth?state=browser-state".into(),
            expires_at: Some(Utc::now() + chrono::Duration::minutes(10)),
        })
    }

    async fn complete_browser_authorization(
        &self,
        _: &str,
        authorization_code: &str,
        _: &str,
    ) -> Result<OAuthToken, GrokApiError> {
        self.browser_codes
            .lock()
            .unwrap()
            .push(authorization_code.to_owned());
        Ok(token("browser-account"))
    }

    async fn refresh(&self, _: &str) -> Result<OAuthToken, GrokApiError> {
        self.refreshes.fetch_add(1, Ordering::SeqCst);
        let mut refreshed = token("device-account");
        refreshed.refresh_token = None;
        refreshed.account_label = self.refresh_account.clone();
        Ok(refreshed)
    }

    async fn revoke(&self, _: &OAuthToken) -> Result<(), GrokApiError> {
        let mut failures = self.revoke_failures.lock().unwrap();
        if *failures > 0 {
            *failures -= 1;
            return Err(GrokApiError::Network("mock revoke failure".into()));
        }
        *self.revoked.lock().unwrap() = true;
        Ok(())
    }

    async fn fetch_billing(&self, _: &str) -> Result<Value, GrokApiError> {
        if let Some(delay) = self.delay_billing {
            tokio::time::sleep(delay).await;
        }
        Ok(self
            .billing
            .lock()
            .unwrap()
            .clone()
            .unwrap_or_else(|| json!({"usage_percent": 1, "products": []})))
    }

    async fn fetch_settings(&self, _: &str) -> Result<Value, GrokApiError> {
        if self.hang_settings {
            return std::future::pending().await;
        }
        match self.settings.lock().unwrap().clone() {
            Some(result) => result,
            None => Ok(json!({
                "subscription_tier_display": "SuperGrok",
                "allow_access": true
            })),
        }
    }
}

#[tokio::test]
async fn persistent_credentials_restore_and_logout_through_the_shared_store() {
    let credentials = Arc::new(CredentialStore::new(MemoryCredentialBackend::default()));
    let provider = GrokProvider::with_transport_and_store_for_account(
        Arc::new(MockTransport::default()),
        credentials.clone(),
        "grok-a",
    )
    .unwrap();
    let challenge = provider
        .start_auth(AuthStartRequest {
            method: Some(AuthMethod::DeviceCode),
            redirect_uri: None,
        })
        .await
        .unwrap();
    let completion = AuthCompleteRequest {
        flow_id: challenge.flow_id,
        authorization_code: None,
        redirect_uri: None,
    };
    assert!(matches!(
        provider.complete_auth(completion.clone()).await.unwrap(),
        AuthState::Pending { .. }
    ));
    assert!(matches!(
        provider.complete_auth(completion).await.unwrap(),
        AuthState::Authenticated { .. }
    ));
    drop(provider);

    let isolated = GrokProvider::with_transport_and_store_for_account(
        Arc::new(MockTransport::default()),
        credentials.clone(),
        "grok-b",
    )
    .unwrap();
    assert_eq!(
        isolated.auth_status().await.unwrap(),
        AuthState::NotAuthenticated
    );

    let restored = GrokProvider::with_transport_and_store_for_account(
        Arc::new(MockTransport::default()),
        credentials.clone(),
        "grok-a",
    )
    .unwrap();
    assert!(matches!(
        restored.auth_status().await.unwrap(),
        AuthState::Authenticated { .. }
    ));
    restored.logout(LogoutRequest::default()).await.unwrap();
    drop(restored);

    let cleared = GrokProvider::with_transport_and_store_for_account(
        Arc::new(MockTransport::default()),
        credentials,
        "grok-a",
    )
    .unwrap();
    assert_eq!(
        cleared.auth_status().await.unwrap(),
        AuthState::NotAuthenticated
    );
}

#[tokio::test]
async fn persisted_grok_tokens_require_valid_secret_contents() {
    let credentials = Arc::new(CredentialStore::new(MemoryCredentialBackend::default()));
    for (account, token) in [
        (
            "empty-access",
            OAuthToken {
                access_token: " ".into(),
                refresh_token: Some("refresh".into()),
                expires_at: None,
                account_label: Some("account".into()),
            },
        ),
        (
            "empty-refresh",
            OAuthToken {
                access_token: "access".into(),
                refresh_token: Some(" ".into()),
                expires_at: None,
                account_label: Some("account".into()),
            },
        ),
    ] {
        let key = CredentialKey::new("grok", account).unwrap();
        let mut stored = Credential::new();
        stored
            .insert(
                "session",
                SecretValue::new(serde_json::to_vec(&token).unwrap()),
            )
            .unwrap();
        credentials.set(&key, stored).unwrap();
        let provider = GrokProvider::with_transport_and_store_for_account(
            Arc::new(MockTransport::default()),
            credentials.clone(),
            account,
        )
        .unwrap();
        assert!(matches!(
            provider.auth_status().await,
            Err(ProviderError::ProtocolIncompatible { .. })
        ));
    }
}

#[tokio::test]
async fn expired_persisted_token_refreshes_through_the_normal_query_path() {
    let credentials = Arc::new(CredentialStore::new(MemoryCredentialBackend::default()));
    let transport = Arc::new(MockTransport {
        initial_token_expires_soon: true,
        ..MockTransport::default()
    });
    let provider = GrokProvider::with_transport_and_store_for_account(
        transport.clone(),
        credentials.clone(),
        "grok-expired-query",
    )
    .unwrap();
    let challenge = provider
        .start_auth(AuthStartRequest {
            method: Some(AuthMethod::DeviceCode),
            redirect_uri: None,
        })
        .await
        .unwrap();
    let completion = AuthCompleteRequest {
        flow_id: challenge.flow_id,
        authorization_code: None,
        redirect_uri: None,
    };
    assert!(matches!(
        provider.complete_auth(completion.clone()).await.unwrap(),
        AuthState::Pending { .. }
    ));
    provider.complete_auth(completion).await.unwrap();
    tokio::time::sleep(std::time::Duration::from_millis(30)).await;

    let outcome = provider
        .query(UsageQuery {
            account_label: Some("device-account".into()),
        })
        .await
        .unwrap();
    assert!(matches!(outcome, QueryOutcome::Complete { .. }));
    assert_eq!(transport.refreshes.load(Ordering::SeqCst), 1);

    drop(provider);
    let restored = GrokProvider::with_transport_and_store_for_account(
        Arc::new(MockTransport::default()),
        credentials,
        "grok-expired-query",
    )
    .unwrap();
    assert!(matches!(
        restored.auth_status().await.unwrap(),
        AuthState::Authenticated { .. }
    ));
}

#[tokio::test]
async fn auth_status_refreshes_an_expired_persisted_token() {
    let credentials = Arc::new(CredentialStore::new(MemoryCredentialBackend::default()));
    let transport = Arc::new(MockTransport {
        initial_token_expires_soon: true,
        ..MockTransport::default()
    });
    let provider = GrokProvider::with_transport_and_store_for_account(
        transport.clone(),
        credentials,
        "grok-expired-status",
    )
    .unwrap();
    let challenge = provider
        .start_auth(AuthStartRequest {
            method: Some(AuthMethod::DeviceCode),
            redirect_uri: None,
        })
        .await
        .unwrap();
    let completion = AuthCompleteRequest {
        flow_id: challenge.flow_id,
        authorization_code: None,
        redirect_uri: None,
    };
    provider.complete_auth(completion.clone()).await.unwrap();
    provider.complete_auth(completion).await.unwrap();
    tokio::time::sleep(std::time::Duration::from_millis(30)).await;

    assert!(matches!(
        provider.auth_status().await.unwrap(),
        AuthState::Authenticated { .. }
    ));
    assert_eq!(transport.refreshes.load(Ordering::SeqCst), 1);
}

#[tokio::test]
async fn failed_credential_delete_keeps_logout_retryable_and_restart_stays_cleared() {
    let write_failures = Arc::new(AtomicUsize::new(0));
    let credentials = Arc::new(CredentialStore::new(MemoryCredentialBackend {
        write_failures: write_failures.clone(),
        ..MemoryCredentialBackend::default()
    }));
    let provider = GrokProvider::with_transport_and_store_for_account(
        Arc::new(MockTransport::default()),
        credentials.clone(),
        "grok-delete-retry",
    )
    .unwrap();
    let challenge = provider
        .start_auth(AuthStartRequest {
            method: Some(AuthMethod::DeviceCode),
            redirect_uri: None,
        })
        .await
        .unwrap();
    let completion = AuthCompleteRequest {
        flow_id: challenge.flow_id,
        authorization_code: None,
        redirect_uri: None,
    };
    provider.complete_auth(completion.clone()).await.unwrap();
    provider.complete_auth(completion).await.unwrap();

    write_failures.store(1, Ordering::SeqCst);
    assert!(matches!(
        provider.logout(LogoutRequest::default()).await,
        Err(ProviderError::ProtocolIncompatible { .. })
    ));
    assert!(matches!(
        provider.auth_status().await.unwrap(),
        AuthState::Authenticated { .. }
    ));
    provider.logout(LogoutRequest::default()).await.unwrap();

    drop(provider);
    let restored = GrokProvider::with_transport_and_store_for_account(
        Arc::new(MockTransport::default()),
        credentials,
        "grok-delete-retry",
    )
    .unwrap();
    assert_eq!(
        restored.auth_status().await.unwrap(),
        AuthState::NotAuthenticated
    );
}

#[test]
fn device_flow_polls_refreshes_and_logs_out() {
    let provider = GrokProvider::new(MockTransport::default());
    let challenge = run_ready(provider.start_auth(AuthStartRequest {
        method: Some(AuthMethod::DeviceCode),
        redirect_uri: None,
    }))
    .unwrap();
    assert_eq!(challenge.flow_id, "device-flow");
    assert_eq!(challenge.user_code.as_deref(), Some("SAFE-CODE"));

    let request = AuthCompleteRequest {
        flow_id: challenge.flow_id,
        authorization_code: None,
        redirect_uri: None,
    };
    assert!(matches!(
        run_ready(provider.complete_auth(request.clone())).unwrap(),
        AuthState::Pending { .. }
    ));
    assert!(matches!(
        run_ready(provider.complete_auth(request)).unwrap(),
        AuthState::Authenticated { account_label: Some(label), .. } if label == "device-account"
    ));
    assert!(matches!(
        run_ready(provider.refresh_auth()).unwrap(),
        AuthState::Authenticated { account_label: Some(label), .. } if label == "device-account"
    ));
    run_ready(provider.logout(LogoutRequest::default())).unwrap();
    assert_eq!(
        run_ready(provider.auth_status()).unwrap(),
        AuthState::NotAuthenticated
    );
}

#[test]
fn browser_flow_rejects_state_confusion_then_completes() {
    let provider = GrokProvider::new(MockTransport::default());
    let challenge = run_ready(provider.start_auth(AuthStartRequest {
        method: Some(AuthMethod::BrowserOAuth),
        redirect_uri: None,
    }))
    .unwrap();
    let mismatch = run_ready(provider.complete_auth(AuthCompleteRequest {
        flow_id: "attacker-state".into(),
        authorization_code: Some("code".into()),
        redirect_uri: Some("http://127.0.0.1/callback".into()),
    }))
    .unwrap_err();
    assert!(matches!(
        mismatch,
        ProviderError::AuthenticationInvalid { .. }
    ));

    let authenticated = run_ready(provider.complete_auth(AuthCompleteRequest {
        flow_id: challenge.flow_id,
        authorization_code: Some("code".into()),
        redirect_uri: Some("http://127.0.0.1/callback".into()),
    }))
    .unwrap();
    assert!(matches!(authenticated, AuthState::Authenticated { .. }));
}

#[test]
fn browser_flow_uses_per_flow_redirect_uri() {
    let provider = GrokProvider::new(MockTransport::default());
    let custom = "http://127.0.0.1:54321/auth/callback";
    let challenge = run_ready(provider.start_auth(AuthStartRequest {
        method: Some(AuthMethod::BrowserOAuth),
        redirect_uri: Some(custom.into()),
    }))
    .unwrap();
    let mismatch = run_ready(provider.complete_auth(AuthCompleteRequest {
        flow_id: challenge.flow_id.clone(),
        authorization_code: Some("code".into()),
        redirect_uri: Some("http://127.0.0.1/callback".into()),
    }))
    .unwrap_err();
    assert!(matches!(
        mismatch,
        ProviderError::AuthenticationInvalid { .. }
    ));
    run_ready(provider.complete_auth(AuthCompleteRequest {
        flow_id: challenge.flow_id,
        authorization_code: Some("code".into()),
        redirect_uri: Some(custom.into()),
    }))
    .unwrap();
}

#[test]
fn browser_flow_extracts_the_code_from_a_callback_url() {
    let transport = Arc::new(MockTransport::default());
    let provider = GrokProvider::with_transport(transport.clone());
    let challenge = run_ready(provider.start_auth(AuthStartRequest {
        method: Some(AuthMethod::BrowserOAuth),
        redirect_uri: None,
    }))
    .unwrap();
    let callback = format!(
        "http://127.0.0.1/callback?code=raw-code&state={}",
        challenge.flow_id
    );
    run_ready(provider.complete_auth(AuthCompleteRequest {
        flow_id: challenge.flow_id.clone(),
        authorization_code: Some(callback),
        redirect_uri: Some("http://127.0.0.1/callback".into()),
    }))
    .unwrap();
    assert_eq!(
        transport.browser_codes.lock().unwrap().as_slice(),
        ["raw-code"]
    );

    let mismatched = run_ready(provider.start_auth(AuthStartRequest {
        method: Some(AuthMethod::BrowserOAuth),
        redirect_uri: None,
    }))
    .unwrap();
    let mismatch = run_ready(provider.complete_auth(AuthCompleteRequest {
        flow_id: mismatched.flow_id.clone(),
        authorization_code: Some("http://127.0.0.1/callback?code=raw-code&state=attacker".into()),
        redirect_uri: Some("http://127.0.0.1/callback".into()),
    }))
    .unwrap_err();
    assert!(matches!(
        mismatch,
        ProviderError::AuthenticationInvalid { .. }
    ));

    let missing_state = run_ready(provider.start_auth(AuthStartRequest {
        method: Some(AuthMethod::BrowserOAuth),
        redirect_uri: None,
    }))
    .unwrap();
    let omitted = run_ready(provider.complete_auth(AuthCompleteRequest {
        flow_id: missing_state.flow_id.clone(),
        authorization_code: Some("http://127.0.0.1/callback?code=raw-code".into()),
        redirect_uri: Some("http://127.0.0.1/callback".into()),
    }))
    .unwrap_err();
    assert!(matches!(
        omitted,
        ProviderError::AuthenticationInvalid { .. }
    ));
}

#[test]
fn refresh_and_logout_remain_bound_to_the_authenticated_account() {
    let provider = GrokProvider::new(MockTransport {
        refresh_account: Some("different-account".into()),
        ..MockTransport::default()
    });
    let challenge = run_ready(provider.start_auth(AuthStartRequest {
        method: Some(AuthMethod::BrowserOAuth),
        redirect_uri: None,
    }))
    .unwrap();
    run_ready(provider.complete_auth(AuthCompleteRequest {
        flow_id: challenge.flow_id,
        authorization_code: Some("code".into()),
        redirect_uri: Some("http://127.0.0.1/callback".into()),
    }))
    .unwrap();

    assert!(matches!(
        run_ready(provider.refresh_auth()),
        Err(ProviderError::AuthenticationInvalid { .. })
    ));
    assert!(matches!(
        run_ready(provider.logout(LogoutRequest {
            account_label: Some("different-account".into()),
        })),
        Err(ProviderError::AuthenticationInvalid { .. })
    ));
    assert!(matches!(
        run_ready(provider.auth_status()).unwrap(),
        AuthState::Authenticated { account_label: Some(label), .. } if label == "browser-account"
    ));
}

#[tokio::test]
async fn query_succeeds_when_local_account_label_differs_from_token_label() {
    let provider = GrokProvider::new(MockTransport::default());
    let challenge = provider
        .start_auth(AuthStartRequest {
            method: Some(AuthMethod::BrowserOAuth),
            redirect_uri: None,
        })
        .await
        .unwrap();
    provider
        .complete_auth(AuthCompleteRequest {
            flow_id: challenge.flow_id,
            authorization_code: Some("code".into()),
            redirect_uri: Some("http://127.0.0.1/callback".into()),
        })
        .await
        .unwrap();

    let outcome = provider
        .query(UsageQuery {
            account_label: Some("grok-1".into()),
        })
        .await
        .unwrap();
    assert!(matches!(outcome, QueryOutcome::Complete { .. }));
}

async fn query_with_settings(
    billing: Value,
    settings: Result<Value, GrokApiError>,
) -> (
    GrokProvider<MockTransport>,
    QueryOutcome<ullage_provider_grok::GrokBillingUsage>,
) {
    let provider = GrokProvider::new(MockTransport {
        billing: Mutex::new(Some(billing)),
        settings: Mutex::new(Some(settings)),
        ..MockTransport::default()
    });
    let challenge = provider
        .start_auth(AuthStartRequest {
            method: Some(AuthMethod::BrowserOAuth),
            redirect_uri: None,
        })
        .await
        .unwrap();
    provider
        .complete_auth(AuthCompleteRequest {
            flow_id: challenge.flow_id,
            authorization_code: Some("code".into()),
            redirect_uri: Some("http://127.0.0.1/callback".into()),
        })
        .await
        .unwrap();
    let outcome = provider.query(UsageQuery::default()).await.unwrap();
    (provider, outcome)
}

fn billing_with_tier() -> Value {
    json!({
        "usage_percent": 1,
        "products": [],
        "tier": "SuperGrok"
    })
}

fn outcome_data(
    outcome: &QueryOutcome<ullage_provider_grok::GrokBillingUsage>,
) -> &ullage_provider_grok::GrokBillingUsage {
    match outcome {
        QueryOutcome::Complete { data } | QueryOutcome::Partial { data, .. } => data,
    }
}

fn outcome_failures(
    outcome: &QueryOutcome<ullage_provider_grok::GrokBillingUsage>,
) -> &[PartialFailure] {
    match outcome {
        QueryOutcome::Complete { .. } => &[],
        QueryOutcome::Partial { failures, .. } => failures,
    }
}

#[tokio::test]
async fn settings_display_tier_overrides_billing_plan() {
    let (provider, outcome) = query_with_settings(
        billing_with_tier(),
        Ok(json!({
            "subscription_tier_display": "SuperGrok Heavy",
            "allow_access": true,
            "gate_message": null,
            "unused_vendor_field": {"nested": true}
        })),
    )
    .await;
    assert!(matches!(outcome, QueryOutcome::Complete { .. }));
    let data = outcome_data(&outcome);
    assert_eq!(
        data.tier.as_ref().map(|tier| tier.raw.as_str()),
        Some("SuperGrok Heavy")
    );
    assert!(!data.access_restricted);
    let normalized = provider.normalize(data.clone()).unwrap();
    assert_eq!(normalized.plan.as_deref(), Some("SuperGrok Heavy"));
}

#[tokio::test]
async fn missing_settings_display_keeps_billing_plan_and_is_partial() {
    let (_, outcome) = query_with_settings(
        billing_with_tier(),
        Ok(json!({
            "allow_access": true,
            "unused_vendor_field": 1
        })),
    )
    .await;
    let QueryOutcome::Partial { failures, .. } = &outcome else {
        panic!("missing display must be partial");
    };
    assert!(
        failures
            .iter()
            .any(|failure| failure.scope == "subscription_tier_display")
    );
    assert_eq!(
        outcome_data(&outcome)
            .tier
            .as_ref()
            .map(|tier| tier.raw.as_str()),
        Some("SuperGrok")
    );
}

#[tokio::test]
async fn settings_request_failure_keeps_billing_usage_and_is_partial() {
    let (_, outcome) = query_with_settings(
        billing_with_tier(),
        Err(GrokApiError::Network("upstream settings failed".into())),
    )
    .await;
    assert!(
        outcome_failures(&outcome)
            .iter()
            .any(|failure| failure.scope == "settings"
                && failure.message == "provider network request failed")
    );
    assert!(
        !format!("{:?}", outcome_failures(&outcome)).contains("upstream settings failed"),
        "settings error details must not leak into the query outcome"
    );
    let data = outcome_data(&outcome);
    assert_eq!(data.usage_percent, Some(1.0));
    assert_eq!(
        data.tier.as_ref().map(|tier| tier.raw.as_str()),
        Some("SuperGrok")
    );
}

#[tokio::test]
async fn settings_authentication_failure_keeps_billing_usage_and_is_partial() {
    let (_, outcome) = query_with_settings(
        billing_with_tier(),
        Err(GrokApiError::AuthenticationInvalid(
            "settings token rejected".into(),
        )),
    )
    .await;
    assert!(
        outcome_failures(&outcome).iter().any(|failure| {
            failure.scope == "settings" && failure.message == "provider authentication is invalid"
        }),
        "{:?}",
        outcome_failures(&outcome)
    );
    assert!(
        !format!("{:?}", outcome_failures(&outcome)).contains("settings token rejected"),
        "settings error details must not leak into the query outcome"
    );
}

#[tokio::test(start_paused = true)]
async fn hanging_settings_degrades_to_partial_without_failing_the_query() {
    let provider = GrokProvider::new(MockTransport {
        billing: Mutex::new(Some(billing_with_tier())),
        hang_settings: true,
        ..MockTransport::default()
    });
    let challenge = provider
        .start_auth(AuthStartRequest {
            method: Some(AuthMethod::BrowserOAuth),
            redirect_uri: None,
        })
        .await
        .unwrap();
    provider
        .complete_auth(AuthCompleteRequest {
            flow_id: challenge.flow_id,
            authorization_code: Some("code".into()),
            redirect_uri: Some("http://127.0.0.1/callback".into()),
        })
        .await
        .unwrap();

    let query = tokio::spawn(async move { provider.query(UsageQuery::default()).await });
    tokio::task::yield_now().await;
    tokio::time::advance(std::time::Duration::from_secs(60)).await;
    let outcome = query.await.unwrap().unwrap();
    assert!(
        outcome_failures(&outcome)
            .iter()
            .any(|failure| failure.scope == "settings")
    );
    assert_eq!(outcome_data(&outcome).usage_percent, Some(1.0));
    assert_eq!(
        outcome_data(&outcome)
            .tier
            .as_ref()
            .map(|tier| tier.raw.as_str()),
        Some("SuperGrok")
    );
}

#[tokio::test(start_paused = true)]
async fn slow_billing_with_hung_settings_returns_partial_before_daemon_budget() {
    let provider = GrokProvider::new(MockTransport {
        billing: Mutex::new(Some(billing_with_tier())),
        hang_settings: true,
        delay_billing: Some(std::time::Duration::from_secs(23)),
        ..MockTransport::default()
    });
    let challenge = provider
        .start_auth(AuthStartRequest {
            method: Some(AuthMethod::BrowserOAuth),
            redirect_uri: None,
        })
        .await
        .unwrap();
    provider
        .complete_auth(AuthCompleteRequest {
            flow_id: challenge.flow_id,
            authorization_code: Some("code".into()),
            redirect_uri: Some("http://127.0.0.1/callback".into()),
        })
        .await
        .unwrap();

    let query = tokio::spawn(async move { provider.query(UsageQuery::default()).await });
    tokio::task::yield_now().await;
    let raced = tokio::time::timeout(std::time::Duration::from_secs(30), async {
        tokio::time::advance(std::time::Duration::from_secs(23)).await;
        query.await.unwrap()
    })
    .await
    .expect("settings must not extend a 23s billing fetch past the 30s probe budget")
    .unwrap();
    assert!(
        outcome_failures(&raced)
            .iter()
            .any(|failure| failure.scope == "settings")
    );
    assert_eq!(outcome_data(&raced).usage_percent, Some(1.0));
}

#[tokio::test]
async fn settings_access_denied_marks_limit_reached() {
    let (provider, outcome) = query_with_settings(
        billing_with_tier(),
        Ok(json!({
            "subscription_tier_display": "SuperGrok Heavy",
            "allow_access": false,
            "gate_message": "\u{1b}[31mgated\u{1b}[0m"
        })),
    )
    .await;
    assert!(matches!(outcome, QueryOutcome::Complete { .. }));
    let data = outcome_data(&outcome);
    assert!(data.access_restricted);
    assert!(
        format!("{data:?}").find("gated").is_none(),
        "vendor gate_message must not be stored in the usage model"
    );
    let normalized = provider.normalize(data.clone()).unwrap();
    assert_eq!(normalized.plan.as_deref(), Some("SuperGrok Heavy"));
    let access = normalized
        .windows
        .iter()
        .find(|window| {
            matches!(
                window.window,
                UsageWindowKind::Other { ref id, .. } if id == "access"
            )
        })
        .expect("restricted access window");
    assert!(
        access
            .measurements
            .iter()
            .any(|measurement| measurement.name == "limit_reached" && measurement.used == 1.0)
    );
    assert!(
        access
            .measurements
            .iter()
            .any(|measurement| measurement.name == "allowed" && measurement.used == 0.0)
    );
}

#[test]
fn failed_revocation_retains_the_token_for_logout_retry() {
    let provider = GrokProvider::new(MockTransport {
        revoke_failures: Mutex::new(1),
        ..MockTransport::default()
    });
    let challenge = run_ready(provider.start_auth(AuthStartRequest {
        method: Some(AuthMethod::BrowserOAuth),
        redirect_uri: None,
    }))
    .unwrap();
    run_ready(provider.complete_auth(AuthCompleteRequest {
        flow_id: challenge.flow_id,
        authorization_code: Some("code".into()),
        redirect_uri: Some("http://127.0.0.1/callback".into()),
    }))
    .unwrap();

    assert!(matches!(
        run_ready(provider.logout(LogoutRequest::default())),
        Err(ProviderError::Network { .. })
    ));
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

struct SlowRefreshTransport {
    entered: Arc<tokio::sync::Notify>,
    release: Arc<tokio::sync::Notify>,
}

#[async_trait]
impl GrokTransport for SlowRefreshTransport {
    fn browser_redirect_uri(&self) -> &str {
        "http://127.0.0.1/callback"
    }

    async fn start_device_authorization(&self) -> Result<DeviceAuthorization, GrokApiError> {
        unreachable!()
    }

    async fn poll_device_authorization(&self, _: &str) -> Result<OAuthPoll, GrokApiError> {
        unreachable!()
    }

    async fn start_browser_authorization(
        &self,
        _: &str,
    ) -> Result<BrowserAuthorization, GrokApiError> {
        Ok(BrowserAuthorization {
            flow_id: "slow-refresh-flow".into(),
            authorization_uri: "https://accounts.example.invalid/authorize".into(),
            expires_at: Some(Utc::now() + chrono::Duration::minutes(10)),
        })
    }

    async fn complete_browser_authorization(
        &self,
        _: &str,
        _: &str,
        _: &str,
    ) -> Result<OAuthToken, GrokApiError> {
        let mut initial = token("slow-refresh-account");
        initial.expires_at = Some(Utc::now() + chrono::Duration::milliseconds(20));
        Ok(initial)
    }

    async fn refresh(&self, _: &str) -> Result<OAuthToken, GrokApiError> {
        self.entered.notify_one();
        self.release.notified().await;
        Ok(token("slow-refresh-account"))
    }

    async fn revoke(&self, _: &OAuthToken) -> Result<(), GrokApiError> {
        Ok(())
    }

    async fn fetch_billing(&self, _: &str) -> Result<Value, GrokApiError> {
        Ok(json!({"usage_percent": 1, "products": []}))
    }

    async fn fetch_settings(&self, _: &str) -> Result<Value, GrokApiError> {
        Ok(json!({
            "subscription_tier_display": "SuperGrok",
            "allow_access": true
        }))
    }
}

#[tokio::test]
async fn logout_waits_for_automatic_refresh_and_clears_the_rotated_token() {
    let entered = Arc::new(tokio::sync::Notify::new());
    let release = Arc::new(tokio::sync::Notify::new());
    let transport = Arc::new(SlowRefreshTransport {
        entered: entered.clone(),
        release: release.clone(),
    });
    let credentials = Arc::new(CredentialStore::new(MemoryCredentialBackend::default()));
    let provider = Arc::new(
        GrokProvider::with_transport_and_store_for_account(
            transport.clone(),
            credentials.clone(),
            "grok-refresh-logout",
        )
        .unwrap(),
    );
    let challenge = provider
        .start_auth(AuthStartRequest {
            method: Some(AuthMethod::BrowserOAuth),
            redirect_uri: None,
        })
        .await
        .unwrap();
    provider
        .complete_auth(AuthCompleteRequest {
            flow_id: challenge.flow_id,
            authorization_code: Some("code".into()),
            redirect_uri: Some("http://127.0.0.1/callback".into()),
        })
        .await
        .unwrap();
    tokio::time::sleep(std::time::Duration::from_millis(30)).await;

    let querying_provider = provider.clone();
    let query = tokio::spawn(async move {
        querying_provider
            .query(UsageQuery {
                account_label: Some("slow-refresh-account".into()),
            })
            .await
    });
    entered.notified().await;
    let logging_out_provider = provider.clone();
    let logout =
        tokio::spawn(async move { logging_out_provider.logout(LogoutRequest::default()).await });
    release.notify_one();

    assert!(matches!(
        query.await.unwrap().unwrap(),
        QueryOutcome::Complete { .. }
    ));
    logout.await.unwrap().unwrap();
    assert_eq!(
        provider.auth_status().await.unwrap(),
        AuthState::NotAuthenticated
    );
    drop(provider);

    let restored = GrokProvider::with_transport_and_store_for_account(
        transport,
        credentials,
        "grok-refresh-logout",
    )
    .unwrap();
    assert_eq!(
        restored.auth_status().await.unwrap(),
        AuthState::NotAuthenticated
    );
}

struct SlowStartTransport {
    calls: AtomicUsize,
    entered: Arc<tokio::sync::Notify>,
    release: Arc<tokio::sync::Notify>,
}

#[async_trait]
impl GrokTransport for SlowStartTransport {
    fn browser_redirect_uri(&self) -> &str {
        "http://127.0.0.1/callback"
    }

    async fn start_device_authorization(&self) -> Result<DeviceAuthorization, GrokApiError> {
        let call = self.calls.fetch_add(1, Ordering::SeqCst);
        if call == 0 {
            self.entered.notify_one();
            self.release.notified().await;
        }
        Ok(DeviceAuthorization {
            flow_id: format!("flow-{call}"),
            device_code: format!("device-{call}"),
            user_code: format!("USER-{call}"),
            verification_uri: "https://accounts.example.invalid/device".into(),
            expires_at: Some(Utc::now() + chrono::Duration::minutes(10)),
        })
    }

    async fn poll_device_authorization(&self, _: &str) -> Result<OAuthPoll, GrokApiError> {
        Ok(OAuthPoll::Pending)
    }

    async fn start_browser_authorization(
        &self,
        _: &str,
    ) -> Result<BrowserAuthorization, GrokApiError> {
        unreachable!()
    }

    async fn complete_browser_authorization(
        &self,
        _: &str,
        _: &str,
        _: &str,
    ) -> Result<OAuthToken, GrokApiError> {
        unreachable!()
    }

    async fn refresh(&self, _: &str) -> Result<OAuthToken, GrokApiError> {
        unreachable!()
    }

    async fn revoke(&self, _: &OAuthToken) -> Result<(), GrokApiError> {
        Ok(())
    }

    async fn fetch_billing(&self, _: &str) -> Result<Value, GrokApiError> {
        unreachable!()
    }

    async fn fetch_settings(&self, _: &str) -> Result<Value, GrokApiError> {
        unreachable!()
    }
}

#[tokio::test]
async fn concurrent_oauth_starts_serialize_and_publish_the_latest_flow() {
    let entered = Arc::new(tokio::sync::Notify::new());
    let release = Arc::new(tokio::sync::Notify::new());
    let provider = Arc::new(GrokProvider::new(SlowStartTransport {
        calls: AtomicUsize::new(0),
        entered: entered.clone(),
        release: release.clone(),
    }));
    let older_provider = provider.clone();
    let older = tokio::spawn(async move {
        older_provider
            .start_auth(AuthStartRequest {
                method: Some(AuthMethod::DeviceCode),
                redirect_uri: None,
            })
            .await
    });
    entered.notified().await;
    let newer_provider = provider.clone();
    let newer = tokio::spawn(async move {
        newer_provider
            .start_auth(AuthStartRequest {
                method: Some(AuthMethod::DeviceCode),
                redirect_uri: None,
            })
            .await
    });
    release.notify_one();
    assert_eq!(older.await.unwrap().unwrap().flow_id, "flow-0");
    assert_eq!(newer.await.unwrap().unwrap().flow_id, "flow-1");
    assert!(matches!(
        provider.auth_status().await.unwrap(),
        AuthState::Pending { flow_id, .. } if flow_id == "flow-1"
    ));
}

struct SlowBillingTransport {
    entered: Arc<tokio::sync::Notify>,
    release: Arc<tokio::sync::Notify>,
}

#[async_trait]
impl GrokTransport for SlowBillingTransport {
    fn browser_redirect_uri(&self) -> &str {
        "http://127.0.0.1/callback"
    }

    async fn start_device_authorization(&self) -> Result<DeviceAuthorization, GrokApiError> {
        unreachable!()
    }

    async fn poll_device_authorization(&self, _: &str) -> Result<OAuthPoll, GrokApiError> {
        unreachable!()
    }

    async fn start_browser_authorization(
        &self,
        _: &str,
    ) -> Result<BrowserAuthorization, GrokApiError> {
        Ok(BrowserAuthorization {
            flow_id: "billing-flow".into(),
            authorization_uri: "https://accounts.example.invalid/authorize".into(),
            expires_at: Some(Utc::now() + chrono::Duration::minutes(10)),
        })
    }

    async fn complete_browser_authorization(
        &self,
        _: &str,
        _: &str,
        _: &str,
    ) -> Result<OAuthToken, GrokApiError> {
        Ok(token("billing-account"))
    }

    async fn refresh(&self, _: &str) -> Result<OAuthToken, GrokApiError> {
        unreachable!()
    }

    async fn revoke(&self, _: &OAuthToken) -> Result<(), GrokApiError> {
        Ok(())
    }

    async fn fetch_billing(&self, _: &str) -> Result<Value, GrokApiError> {
        self.entered.notify_one();
        self.release.notified().await;
        Ok(json!({"usagePercent": 3, "products": []}))
    }

    async fn fetch_settings(&self, _: &str) -> Result<Value, GrokApiError> {
        Ok(json!({
            "subscription_tier_display": "SuperGrok",
            "allow_access": true
        }))
    }
}

#[tokio::test]
async fn usage_query_discards_results_after_logout() {
    let entered = Arc::new(tokio::sync::Notify::new());
    let release = Arc::new(tokio::sync::Notify::new());
    let provider = Arc::new(GrokProvider::new(SlowBillingTransport {
        entered: entered.clone(),
        release: release.clone(),
    }));
    let challenge = provider
        .start_auth(AuthStartRequest {
            method: Some(AuthMethod::BrowserOAuth),
            redirect_uri: None,
        })
        .await
        .unwrap();
    provider
        .complete_auth(AuthCompleteRequest {
            flow_id: challenge.flow_id,
            authorization_code: Some("code".into()),
            redirect_uri: Some("http://127.0.0.1/callback".into()),
        })
        .await
        .unwrap();

    let querying_provider = provider.clone();
    let query = tokio::spawn(async move { querying_provider.query(UsageQuery::default()).await });
    entered.notified().await;
    provider.logout(LogoutRequest::default()).await.unwrap();
    release.notify_one();
    assert!(matches!(
        query.await.unwrap(),
        Err(ProviderError::AuthenticationInvalid { .. })
    ));
}

#[test]
fn later_valid_monthly_limit_aliases_survive_invalid_preferred_aliases() {
    let outcome = parse_billing(
        json!({
            "config": {
                "used": {"val": 10},
                "monthlyLimit": {"val": -1},
                "monthly_limit": {"val": 100},
                "billingPeriodEnd": "2026-09-01T00:00:00Z"
            }
        }),
        None,
        Utc::now(),
    )
    .unwrap();
    let QueryOutcome::Complete { data } = outcome else {
        panic!("valid monthly limit alias should produce complete data");
    };
    assert_eq!(data.monthly_used, Some(10.0));
    assert_eq!(data.monthly_limit, Some(100.0));
    assert_eq!(data.usage_percent, Some(10.0));
}

#[test]
fn malformed_optional_monthly_fields_return_partial_data() {
    let outcome = parse_billing(
        json!({
            "config": {
                "used": {"val": "not-a-number"},
                "monthlyLimit": {"val": -1},
                "billingPeriodStart": "2026-08-01T00:00:00+00:00",
                "billingPeriodEnd": "2026-09-01T00:00:00+00:00"
            }
        }),
        None,
        Utc::now(),
    )
    .unwrap();
    assert!(matches!(
        outcome,
        QueryOutcome::Partial { data, failures }
            if data.monthly_used.is_none()
                && data.monthly_limit.is_none()
                && data.current_period.is_some()
                && failures.iter().any(|failure| failure.scope == "used")
                && failures.iter().any(|failure| failure.scope == "monthly_limit")
    ));
}

#[test]
fn malformed_on_demand_cap_returns_partial_data() {
    let outcome = parse_billing(
        json!({
            "config": {
                "used": {"val": 10},
                "onDemandCap": {"val": "not-a-number"},
                "billingPeriodEnd": "2026-09-01T00:00:00Z"
            }
        }),
        None,
        Utc::now(),
    )
    .unwrap();
    assert!(matches!(
        outcome,
        QueryOutcome::Partial { data, failures }
            if data.monthly_used == Some(10.0)
                && data.on_demand.is_none()
                && failures.iter().any(|failure| failure.scope == "on_demand_cap")
    ));
}

#[test]
fn later_valid_on_demand_cap_aliases_survive_invalid_preferred_aliases() {
    let outcome = parse_billing(
        json!({
            "config": {
                "used": {"val": 10},
                "onDemandCap": {"val": -1},
                "on_demand_cap": {"val": 25},
                "billingPeriodEnd": "2026-09-01T00:00:00Z"
            }
        }),
        None,
        Utc::now(),
    )
    .unwrap();
    let QueryOutcome::Complete { data } = outcome else {
        panic!("valid on-demand cap alias should produce complete data");
    };
    let on_demand = data.on_demand.expect("on-demand cap");
    assert!(on_demand.enabled);
    assert_eq!(on_demand.limit, Some(25.0));
}

/// cli-chat-proxy reports `monthlyLimit.val = 0` when the account has no
/// published monthly cap, and `onDemandCap.val = 0` when on-demand allowance
/// is absent. The former becomes `limit: None` on the monthly measurement
/// (unlimited quota is `limit: null`, not a sentinel number). The latter
/// omits on-demand data entirely rather than synthesizing a window.
#[test]
fn parses_cli_chat_proxy_config_billing_schema() {
    let observed_at = Utc.with_ymd_and_hms(2026, 8, 30, 3, 0, 0).unwrap();
    let outcome = parse_billing(
        fixture(include_str!("fixtures/cli_chat_proxy_billing.json")),
        Some("account".into()),
        observed_at,
    )
    .unwrap();
    let QueryOutcome::Complete { data } = outcome else {
        panic!("expected complete billing data for cli-chat-proxy schema");
    };
    assert_eq!(data.monthly_used, Some(285.0));
    assert_eq!(data.monthly_limit, None);
    assert!(data.usage_percent.is_none());
    assert!(
        data.on_demand.is_none(),
        "onDemandCap=0 must not synthesize an on-demand window"
    );
    let period = data.current_period.as_ref().expect("billing period");
    assert_eq!(
        period.starts_at,
        Some(Utc.with_ymd_and_hms(2026, 8, 1, 0, 0, 0).unwrap())
    );
    assert_eq!(
        period.ends_at,
        Some(Utc.with_ymd_and_hms(2026, 9, 1, 0, 0, 0).unwrap())
    );

    let normalized = ullage_provider_grok::GrokProvider::new(MockTransport::default())
        .normalize(data)
        .unwrap();
    assert_eq!(normalized.windows.len(), 1);
    assert_eq!(normalized.windows[0].window, UsageWindowKind::Monthly);
    assert_eq!(normalized.windows[0].measurements[0].used, 285.0);
    assert!(normalized.windows[0].measurements[0].limit.is_none());
    assert_eq!(
        normalized.windows[0].measurements[0].unit,
        ullage_core::MeasurementUnit::Credits
    );
    assert_eq!(
        normalized.windows[0].resets_at,
        Some(Utc.with_ymd_and_hms(2026, 9, 1, 0, 0, 0).unwrap())
    );
}

/// Live `GET /v1/billing?format=credits` body captured 2026-08-30. The server
/// returns a rolling weekly window and a pre-computed `creditUsagePercent`.
#[test]
fn parses_format_credits_schema_and_keeps_percent_window() {
    let observed_at = Utc.with_ymd_and_hms(2026, 8, 30, 6, 0, 0).unwrap();
    let outcome = parse_billing(
        json!({"config":{
          "currentPeriod":{"type":"USAGE_PERIOD_TYPE_WEEKLY",
                           "start":"2026-08-28T01:18:04.090314+00:00",
                           "end":"2026-09-04T01:18:04.090314+00:00"},
          "creditUsagePercent":61.0,
          "productUsage":[{"product":"GrokBuild","usagePercent":61.0}],
          "onDemandCap":{"val":0},
          "onDemandUsed":{"val":0},
          "prepaidBalance":{"val":0},
          "isUnifiedBillingUser":true,
          "topUpMethod":"TOP_UP_METHOD_SAVED_PAYMENT_METHOD",
          "billingPeriodStart":"2026-08-28T01:18:04.090314+00:00",
          "billingPeriodEnd":"2026-09-04T01:18:04.090314+00:00"
        }}),
        Some("account".into()),
        observed_at,
    )
    .unwrap();
    let QueryOutcome::Complete { data } = outcome else {
        panic!("format=credits sample should parse completely");
    };
    assert_eq!(data.usage_percent, Some(61.0));
    assert_eq!(
        data.current_period
            .as_ref()
            .and_then(|period| period.kind.clone()),
        Some(UsageWindowKind::Weekly)
    );
    assert_eq!(
        data.current_period
            .as_ref()
            .and_then(|period| period.ends_at),
        Some(
            DateTime::parse_from_rfc3339("2026-09-04T01:18:04.090314Z")
                .unwrap()
                .with_timezone(&Utc)
        )
    );
    assert!(
        data.products
            .iter()
            .any(|product| product.product == "GrokBuild" && product.usage_percent == 61.0)
    );
    assert_eq!(
        data.prepaid.as_ref().map(|prepaid| prepaid.remaining),
        Some(0.0)
    );
    assert!(
        data.on_demand.is_none(),
        "onDemandCap=0 must not synthesize an on-demand window"
    );
    assert_eq!(
        data.top_up_method.as_deref(),
        Some("TOP_UP_METHOD_SAVED_PAYMENT_METHOD")
    );

    let normalized = ullage_provider_grok::GrokProvider::new(MockTransport::default())
        .normalize(data)
        .unwrap();
    assert_eq!(normalized.windows.len(), 1, "{normalized:?}");
    assert_eq!(normalized.windows[0].window, UsageWindowKind::Weekly);
    assert_eq!(normalized.windows[0].measurements[0].used, 61.0);
    assert_eq!(
        normalized.windows[0].resets_at,
        Some(
            DateTime::parse_from_rfc3339("2026-09-04T01:18:04.090314Z")
                .unwrap()
                .with_timezone(&Utc)
        )
    );
    assert!(
        normalized.windows[0]
            .measurements
            .iter()
            .any(|measurement| measurement.name == "product:GrokBuild")
    );
}

#[test]
fn parses_format_credits_money_fields_when_all_nonzero() {
    let outcome = parse_billing(
        json!({
            "config": {
                "creditUsagePercent": 10.0,
                "currentPeriod": { "type": "USAGE_PERIOD_TYPE_WEEKLY" },
                "prepaidBalance": { "val": 12.5 },
                "onDemandCap": { "val": 40.0 },
                "onDemandUsed": { "val": 7.25 },
                "topUpMethod": "TOP_UP_METHOD_SAVED_PAYMENT_METHOD"
            }
        }),
        None,
        Utc::now(),
    )
    .unwrap();
    let QueryOutcome::Complete { data } = outcome else {
        panic!("nonzero money fields should parse completely");
    };
    assert_eq!(
        data.prepaid.as_ref().map(|prepaid| prepaid.remaining),
        Some(12.5)
    );
    let on_demand = data.on_demand.as_ref().expect("on-demand window");
    assert!(on_demand.enabled);
    assert_eq!(on_demand.used, Some(7.25));
    assert_eq!(on_demand.limit, Some(40.0));
    assert_eq!(
        data.top_up_method.as_deref(),
        Some("TOP_UP_METHOD_SAVED_PAYMENT_METHOD")
    );

    let normalized = ullage_provider_grok::GrokProvider::new(MockTransport::default())
        .normalize(data)
        .unwrap();
    assert!(normalized.windows.iter().any(|window| {
        window.window
            == UsageWindowKind::Other {
                id: "prepaid".into(),
                label: "Extra Usage Credits".into(),
            }
            && window
                .measurements
                .iter()
                .any(|measurement| measurement.name == "remaining" && measurement.used == 12.5)
    }));
    assert!(normalized.windows.iter().any(|window| {
        window.window
            == UsageWindowKind::Other {
                id: "on_demand".into(),
                label: "On-demand usage".into(),
            }
            && window
                .measurements
                .iter()
                .any(|measurement| measurement.name == "spent" && measurement.used == 7.25)
            && window
                .measurements
                .iter()
                .any(|measurement| measurement.name == "spent" && measurement.limit == Some(40.0))
    }));
    assert!(
        !normalized.windows.iter().any(|window| window.window
            == UsageWindowKind::Other {
                id: "top_up_method".into(),
                label: "top_up_method".into(),
            }),
        "topUpMethod is a payment-method identifier, not a usage window"
    );
}

#[test]
fn malformed_format_credits_money_fields_return_partial_data() {
    let outcome = parse_billing(
        json!({
            "config": {
                "creditUsagePercent": 10.0,
                "currentPeriod": { "type": "USAGE_PERIOD_TYPE_WEEKLY" },
                "prepaidBalance": { "val": -3 },
                "onDemandCap": { "val": 25 },
                "onDemandUsed": "not-a-number",
                "topUpMethod": { "unexpected": true }
            }
        }),
        None,
        Utc::now(),
    )
    .unwrap();
    let QueryOutcome::Partial { data, failures } = outcome else {
        panic!("malformed money fields should be partial, got {outcome:?}");
    };
    assert!(data.prepaid.is_none());
    assert_eq!(data.on_demand.as_ref().and_then(|item| item.used), None);
    assert_eq!(
        data.on_demand.as_ref().and_then(|item| item.limit),
        Some(25.0)
    );
    assert!(data.top_up_method.is_none());
    assert!(
        failures
            .iter()
            .any(|failure| failure.scope == "prepaid.remaining")
    );
    assert!(
        failures
            .iter()
            .any(|failure| failure.scope == "on_demand_used")
    );
    assert!(
        failures
            .iter()
            .any(|failure| failure.scope == "top_up_method")
    );
}

#[test]
fn missing_format_credits_money_fields_leave_optional_slots_empty() {
    let outcome = parse_billing(
        json!({
            "config": {
                "creditUsagePercent": 10.0,
                "currentPeriod": { "type": "USAGE_PERIOD_TYPE_WEEKLY" }
            }
        }),
        None,
        Utc::now(),
    )
    .unwrap();
    let QueryOutcome::Complete { data } = outcome else {
        panic!("missing money fields should stay complete");
    };
    assert!(data.prepaid.is_none());
    assert!(data.on_demand.is_none());
    assert!(data.top_up_method.is_none());
}

#[test]
fn prepaid_balance_does_not_override_earlier_prepaid_aliases() {
    let outcome = parse_billing(
        json!({
            "prepaid": { "remaining": 8.5 },
            "prepaidBalance": { "val": 99.0 },
            "usagePercent": 1.0
        }),
        None,
        Utc::now(),
    )
    .unwrap();
    let QueryOutcome::Complete { data } = outcome else {
        panic!("preferred prepaid alias should produce complete data");
    };
    assert_eq!(data.prepaid.unwrap().remaining, 8.5);
}

#[test]
fn nested_on_demand_object_is_not_overwritten_by_flat_used() {
    let outcome = parse_billing(
        json!({
            "onDemand": {
                "enabled": true,
                "used": 2.0,
                "limit": 10.0
            },
            "onDemandCap": { "val": 40.0 },
            "onDemandUsed": { "val": 99.0 },
            "usagePercent": 1.0
        }),
        None,
        Utc::now(),
    )
    .unwrap();
    let QueryOutcome::Complete { data } = outcome else {
        panic!("nested on-demand object should produce complete data");
    };
    let on_demand = data.on_demand.unwrap();
    assert!(on_demand.enabled);
    assert_eq!(on_demand.used, Some(2.0));
    assert_eq!(on_demand.limit, Some(10.0));
}

#[test]
fn unused_money_fields_do_not_capture_a_later_usable_object() {
    let outcome = parse_billing(
        json!({
            "data": {
                "prepaidBalance": { "val": 0 },
                "onDemandCap": { "val": 0 },
                "onDemandUsed": { "val": 7 }
            },
            "config": {
                "creditUsagePercent": 61.0,
                "currentPeriod": { "type": "USAGE_PERIOD_TYPE_WEEKLY" }
            }
        }),
        None,
        Utc::now(),
    )
    .unwrap();
    let QueryOutcome::Complete { data } = outcome else {
        panic!("later config with weekly usage should still parse, got {outcome:?}");
    };
    assert_eq!(data.usage_percent, Some(61.0));
    assert!(data.prepaid.is_none());
    assert!(data.on_demand.is_none());
    let normalized = ullage_provider_grok::GrokProvider::new(MockTransport::default())
        .normalize(data)
        .unwrap();
    assert_eq!(normalized.windows.len(), 1);
    assert_eq!(normalized.windows[0].window, UsageWindowKind::Weekly);
}

#[test]
fn supplemental_money_fields_do_not_hide_a_later_percent_object() {
    let outcome = parse_billing(
        json!({
            "data": {
                "prepaidBalance": { "val": 12.5 },
                "onDemandCap": { "val": 40.0 },
                "onDemandUsed": { "val": 7.25 }
            },
            "config": {
                "creditUsagePercent": 61.0,
                "currentPeriod": { "type": "USAGE_PERIOD_TYPE_WEEKLY" }
            }
        }),
        None,
        Utc::now(),
    )
    .unwrap();
    let QueryOutcome::Complete { data } = outcome else {
        panic!("later config with weekly usage should still parse, got {outcome:?}");
    };
    assert_eq!(data.usage_percent, Some(61.0));
    assert!(
        data.prepaid.is_none() && data.on_demand.is_none(),
        "sibling-envelope money is not merged across billing objects"
    );
    let normalized = ullage_provider_grok::GrokProvider::new(MockTransport::default())
        .normalize(data)
        .unwrap();
    assert_eq!(normalized.windows[0].window, UsageWindowKind::Weekly);
    assert_eq!(normalized.windows[0].measurements[0].used, 61.0);
}

#[test]
fn derived_monthly_percent_keeps_a_monthly_window_label() {
    let outcome = parse_billing(
        json!({
            "config": {
                "used": {"val": 10},
                "monthlyLimit": {"val": 100},
                "billingPeriodEnd": "2026-09-01T00:00:00Z"
            }
        }),
        None,
        Utc::now(),
    )
    .unwrap();
    let QueryOutcome::Complete { data } = outcome else {
        panic!("positive monthly quota should stay complete");
    };
    assert_eq!(data.monthly_used, Some(10.0));
    assert_eq!(data.usage_percent, Some(10.0));
    assert!(data.usage_percent_derived);
    let normalized = ullage_provider_grok::GrokProvider::new(MockTransport::default())
        .normalize(data)
        .unwrap();
    assert_eq!(normalized.windows.len(), 1);
    assert_eq!(normalized.windows[0].window, UsageWindowKind::Monthly);
    assert_eq!(normalized.windows[0].measurements[0].name, "weekly_pool");
    assert_eq!(normalized.windows[0].measurements[0].used, 10.0);
}

#[test]
fn percent_window_survives_when_monthly_used_is_also_present() {
    let outcome = parse_billing(
        json!({
            "config": {
                "used": {"val": 285},
                "monthlyLimit": {"val": 0},
                "creditUsagePercent": 61.0,
                "currentPeriod": {
                    "type": "USAGE_PERIOD_TYPE_WEEKLY",
                    "start": "2026-08-28T01:18:04.090314+00:00",
                    "end": "2026-09-04T01:18:04.090314+00:00"
                }
            }
        }),
        None,
        Utc::now(),
    )
    .unwrap();
    let QueryOutcome::Complete { data } = outcome else {
        panic!("mixed monthly + percent fields should stay complete");
    };
    assert_eq!(data.monthly_used, Some(285.0));
    assert_eq!(data.usage_percent, Some(61.0));

    let normalized = ullage_provider_grok::GrokProvider::new(MockTransport::default())
        .normalize(data)
        .unwrap();
    assert_eq!(normalized.windows.len(), 1);
    assert_eq!(normalized.windows[0].window, UsageWindowKind::Weekly);
    assert_eq!(normalized.windows[0].measurements[0].name, "weekly_pool");
    assert_eq!(normalized.windows[0].measurements[0].used, 61.0);
}

#[test]
fn unrecognized_period_type_falls_back_to_weekly_with_partial_failure() {
    let outcome = parse_billing(
        json!({
            "currentPeriod": {
                "type": "USAGE_PERIOD_TYPE_DAILY",
                "start": "2026-08-28T00:00:00Z",
                "end": "2026-08-29T00:00:00Z"
            },
            "creditUsagePercent": 12.0
        }),
        None,
        Utc::now(),
    )
    .unwrap();
    let QueryOutcome::Partial { data, failures } = outcome else {
        panic!("unrecognized period type must be partial");
    };
    assert!(
        failures
            .iter()
            .any(|failure| failure.scope == "current_period.type")
    );
    assert_eq!(
        data.current_period
            .as_ref()
            .and_then(|period| period.kind.clone()),
        None
    );

    let normalized = ullage_provider_grok::GrokProvider::new(MockTransport::default())
        .normalize(data)
        .unwrap();
    assert_eq!(normalized.windows[0].window, UsageWindowKind::Weekly);
}

#[test]
fn missing_current_period_type_falls_back_to_weekly_with_partial_failure() {
    let outcome = parse_billing(
        json!({
            "currentPeriod": {
                "start": "2026-08-28T00:00:00Z",
                "end": "2026-08-29T00:00:00Z"
            },
            "creditUsagePercent": 12.0
        }),
        None,
        Utc::now(),
    )
    .unwrap();
    let QueryOutcome::Partial { data, failures } = outcome else {
        panic!("missing currentPeriod.type must be partial");
    };
    assert!(failures.iter().any(|failure| {
        failure.scope == "current_period.type"
            && failure.message == "provider protocol response is incompatible"
    }));
    let normalized = ullage_provider_grok::GrokProvider::new(MockTransport::default())
        .normalize(data)
        .unwrap();
    assert_eq!(normalized.windows[0].window, UsageWindowKind::Weekly);
}

#[test]
fn monthly_period_type_maps_to_monthly_window() {
    let outcome = parse_billing(
        json!({
            "currentPeriod": {
                "type": "USAGE_PERIOD_TYPE_MONTHLY",
                "start": "2026-08-01T00:00:00Z",
                "end": "2026-09-01T00:00:00Z"
            },
            "creditUsagePercent": 20.0
        }),
        None,
        Utc::now(),
    )
    .unwrap();
    let QueryOutcome::Complete { data } = outcome else {
        panic!("recognized monthly period type should be complete");
    };
    let normalized = ullage_provider_grok::GrokProvider::new(MockTransport::default())
        .normalize(data)
        .unwrap();
    assert_eq!(normalized.windows[0].window, UsageWindowKind::Monthly);
}

#[test]
fn monthly_period_type_survives_when_only_flat_dates_are_present() {
    let outcome = parse_billing(
        json!({
            "currentPeriod": {
                "type": "USAGE_PERIOD_TYPE_MONTHLY"
            },
            "creditUsagePercent": 20.0,
            "billingPeriodStart": "2026-08-01T00:00:00Z",
            "billingPeriodEnd": "2026-09-01T00:00:00Z"
        }),
        None,
        Utc::now(),
    )
    .unwrap();
    let QueryOutcome::Complete { data } = outcome else {
        panic!("monthly type with flat dates should stay complete");
    };
    assert_eq!(
        data.current_period
            .as_ref()
            .and_then(|period| period.kind.clone()),
        Some(UsageWindowKind::Monthly)
    );
    assert_eq!(
        data.current_period
            .as_ref()
            .and_then(|period| period.ends_at),
        Some(Utc.with_ymd_and_hms(2026, 9, 1, 0, 0, 0).unwrap())
    );
    let normalized = ullage_provider_grok::GrokProvider::new(MockTransport::default())
        .normalize(data)
        .unwrap();
    assert_eq!(normalized.windows[0].window, UsageWindowKind::Monthly);
    assert_eq!(
        normalized.windows[0].resets_at,
        Some(Utc.with_ymd_and_hms(2026, 9, 1, 0, 0, 0).unwrap())
    );
}

#[test]
fn flat_end_fills_missing_nested_end_without_overwriting_start() {
    let outcome = parse_billing(
        json!({
            "currentPeriod": {
                "type": "USAGE_PERIOD_TYPE_WEEKLY",
                "start": "2026-08-28T01:18:04.090314+00:00"
            },
            "creditUsagePercent": 61.0,
            "billingPeriodEnd": "2026-09-04T01:18:04.090314+00:00"
        }),
        None,
        Utc::now(),
    )
    .unwrap();
    let QueryOutcome::Complete { data } = outcome else {
        panic!("partial nested dates should merge flat end");
    };
    let period = data.current_period.as_ref().expect("period");
    assert_eq!(
        period.starts_at,
        Some(
            DateTime::parse_from_rfc3339("2026-08-28T01:18:04.090314Z")
                .unwrap()
                .with_timezone(&Utc)
        )
    );
    assert_eq!(
        period.ends_at,
        Some(
            DateTime::parse_from_rfc3339("2026-09-04T01:18:04.090314Z")
                .unwrap()
                .with_timezone(&Utc)
        )
    );
    let normalized = ullage_provider_grok::GrokProvider::new(MockTransport::default())
        .normalize(data)
        .unwrap();
    assert_eq!(
        normalized.windows[0].resets_at,
        Some(
            DateTime::parse_from_rfc3339("2026-09-04T01:18:04.090314Z")
                .unwrap()
                .with_timezone(&Utc)
        )
    );
}

#[test]
fn credit_usage_without_current_period_reports_missing_type() {
    let outcome = parse_billing(
        json!({
            "creditUsagePercent": 15.0,
            "billingPeriodEnd": "2026-09-04T00:00:00Z"
        }),
        None,
        Utc::now(),
    )
    .unwrap();
    let QueryOutcome::Partial { data, failures } = outcome else {
        panic!("creditUsagePercent without currentPeriod must be partial");
    };
    assert!(failures.iter().any(|failure| {
        failure.scope == "current_period.type"
            && failure.message == "provider protocol response is incompatible"
    }));
    let normalized = ullage_provider_grok::GrokProvider::new(MockTransport::default())
        .normalize(data)
        .unwrap();
    assert_eq!(normalized.windows[0].window, UsageWindowKind::Weekly);
}

#[test]
fn explicit_credit_percent_with_monthly_used_stays_weekly_when_type_missing() {
    let outcome = parse_billing(
        json!({
            "creditUsagePercent": 61.0,
            "used": {"val": 285},
            "billingPeriodEnd": "2026-09-04T00:00:00Z"
        }),
        None,
        Utc::now(),
    )
    .unwrap();
    let QueryOutcome::Partial { data, failures } = outcome else {
        panic!("credits percent without period type must be partial");
    };
    assert!(
        failures
            .iter()
            .any(|failure| failure.scope == "current_period.type")
    );
    assert_eq!(data.monthly_used, Some(285.0));
    assert_eq!(data.usage_percent, Some(61.0));
    assert!(!data.usage_percent_derived);
    let normalized = ullage_provider_grok::GrokProvider::new(MockTransport::default())
        .normalize(data)
        .unwrap();
    assert_eq!(normalized.windows[0].window, UsageWindowKind::Weekly);
    assert_eq!(normalized.windows[0].measurements[0].used, 61.0);
}

#[test]
fn unknown_canonical_period_type_ignores_sibling_monthly_type() {
    let outcome = parse_billing(
        json!({
            "currentPeriod": {
                "type": "USAGE_PERIOD_TYPE_DAILY",
                "start": "2026-08-28T00:00:00Z",
                "end": "2026-09-04T00:00:00Z"
            },
            "current_period": {
                "type": "USAGE_PERIOD_TYPE_MONTHLY"
            },
            "creditUsagePercent": 61.0
        }),
        None,
        Utc::now(),
    )
    .unwrap();
    let QueryOutcome::Partial { data, failures } = outcome else {
        panic!("unknown canonical type must stay partial: {outcome:?}");
    };
    assert!(
        failures
            .iter()
            .any(|failure| failure.scope == "current_period.type")
    );
    assert_eq!(
        data.current_period
            .as_ref()
            .and_then(|period| period.kind.clone()),
        None
    );
    let normalized = ullage_provider_grok::GrokProvider::new(MockTransport::default())
        .normalize(data)
        .unwrap();
    assert_eq!(normalized.windows[0].window, UsageWindowKind::Weekly);
}

#[test]
fn derived_percent_with_untyped_current_period_falls_back_to_weekly() {
    let outcome = parse_billing(
        json!({
            "config": {
                "currentPeriod": {
                    "start": "2026-08-28T00:00:00Z",
                    "end": "2026-09-04T00:00:00Z"
                },
                "used": {"val": 10},
                "monthlyLimit": {"val": 100}
            }
        }),
        None,
        Utc::now(),
    )
    .unwrap();
    let QueryOutcome::Partial { data, failures } = outcome else {
        panic!("missing currentPeriod.type must be partial: {outcome:?}");
    };
    assert!(
        failures
            .iter()
            .any(|failure| failure.scope == "current_period.type")
    );
    assert!(data.usage_percent_derived);
    assert!(data.prefer_weekly_type_fallback);
    let normalized = ullage_provider_grok::GrokProvider::new(MockTransport::default())
        .normalize(data)
        .unwrap();
    assert_eq!(normalized.windows[0].window, UsageWindowKind::Weekly);
    assert_eq!(normalized.windows[0].measurements[0].used, 10.0);
}

#[test]
fn malformed_sibling_period_end_is_reported_as_partial() {
    let outcome = parse_billing(
        json!({
            "currentPeriod": {
                "type": "USAGE_PERIOD_TYPE_WEEKLY"
            },
            "current_period": {
                "end": "not-a-date"
            },
            "creditUsagePercent": 61.0
        }),
        None,
        Utc::now(),
    )
    .unwrap();
    let QueryOutcome::Partial { data, failures } = outcome else {
        panic!("malformed sibling end must be partial: {outcome:?}");
    };
    assert!(
        failures
            .iter()
            .any(|failure| failure.scope.contains("ends_at")),
        "{failures:?}"
    );
    assert_eq!(
        data.current_period
            .as_ref()
            .and_then(|period| period.kind.clone()),
        Some(UsageWindowKind::Weekly)
    );
    assert!(
        data.current_period
            .as_ref()
            .and_then(|period| period.ends_at)
            .is_none()
    );
}

#[test]
fn camel_case_period_type_wins_over_untyped_snake_case_alias() {
    let outcome = parse_billing(
        json!({
            "creditUsagePercent": 20.0,
            "current_period": {
                "start": "2026-08-01T00:00:00Z",
                "end": "2026-09-01T00:00:00Z"
            },
            "currentPeriod": {
                "type": "USAGE_PERIOD_TYPE_MONTHLY"
            }
        }),
        None,
        Utc::now(),
    )
    .unwrap();
    let QueryOutcome::Complete { data } = outcome else {
        panic!("canonical currentPeriod.type should keep the outcome complete: {outcome:?}");
    };
    assert_eq!(
        data.current_period
            .as_ref()
            .and_then(|period| period.kind.clone()),
        Some(UsageWindowKind::Monthly)
    );
    assert_eq!(
        data.current_period
            .as_ref()
            .and_then(|period| period.ends_at),
        Some(Utc.with_ymd_and_hms(2026, 9, 1, 0, 0, 0).unwrap())
    );
    let normalized = ullage_provider_grok::GrokProvider::new(MockTransport::default())
        .normalize(data)
        .unwrap();
    assert_eq!(normalized.windows[0].window, UsageWindowKind::Monthly);
}

#[test]
fn product_usage_with_untyped_current_period_reports_missing_type() {
    let outcome = parse_billing(
        json!({
            "currentPeriod": {
                "end": "2026-09-04T00:00:00Z"
            },
            "productUsage": [{"product": "GrokBuild", "usagePercent": 15.0}]
        }),
        None,
        Utc::now(),
    )
    .unwrap();
    let QueryOutcome::Partial { data, failures } = outcome else {
        panic!("productUsage under untyped currentPeriod must be partial");
    };
    assert!(failures.iter().any(|failure| {
        failure.scope == "current_period.type"
            && failure.message == "provider protocol response is incompatible"
    }));
    let normalized = ullage_provider_grok::GrokProvider::new(MockTransport::default())
        .normalize(data)
        .unwrap();
    assert_eq!(normalized.windows[0].window, UsageWindowKind::Weekly);
    assert_eq!(
        normalized.windows[0].measurements[0].name,
        "product:GrokBuild"
    );
}

#[test]
fn product_usage_without_current_period_reports_missing_type() {
    let outcome = parse_billing(
        json!({
            "productUsage": [{"product": "GrokBuild", "usagePercent": 15.0}],
            "billingPeriodEnd": "2026-09-04T00:00:00Z"
        }),
        None,
        Utc::now(),
    )
    .unwrap();
    let QueryOutcome::Partial { data, failures } = outcome else {
        panic!("productUsage without currentPeriod must be partial");
    };
    assert!(failures.iter().any(|failure| {
        failure.scope == "current_period.type"
            && failure.message == "provider protocol response is incompatible"
    }));
    let normalized = ullage_provider_grok::GrokProvider::new(MockTransport::default())
        .normalize(data)
        .unwrap();
    assert_eq!(normalized.windows[0].window, UsageWindowKind::Weekly);
    assert_eq!(
        normalized.windows[0].measurements[0].name,
        "product:GrokBuild"
    );
}

#[test]
fn parses_new_schema_and_normalizes_only_a_weekly_pool() {
    let observed_at = Utc.with_ymd_and_hms(2026, 8, 27, 12, 0, 0).unwrap();
    let outcome = parse_billing(
        fixture(include_str!("fixtures/new_billing.json")),
        Some("account".into()),
        observed_at,
    )
    .unwrap();
    let QueryOutcome::Complete { data } = outcome else {
        panic!("expected complete billing data");
    };
    assert_eq!(
        data.tier.as_ref().unwrap().normalized,
        NormalizedTier::SuperGrok
    );
    assert_eq!(data.products.len(), 2);

    let normalized = ullage_provider_grok::GrokProvider::new(MockTransport::default())
        .normalize(data)
        .unwrap();
    assert_eq!(normalized.plan.as_deref(), Some("SuperGrok"));
    assert_eq!(normalized.subscription_expires_at, None);
    assert_eq!(normalized.windows[0].window, UsageWindowKind::Weekly);
    assert_eq!(normalized.windows[0].measurements[1].name, "product:chat");
    assert_eq!(normalized.windows.len(), 3);
    assert!(
        normalized
            .windows
            .iter()
            .all(|window| window.window != UsageWindowKind::FiveHours)
    );
}

#[test]
fn parses_old_schema_and_preserves_unknown_products() {
    let QueryOutcome::Complete { data } = parse_billing(
        fixture(include_str!("fixtures/old_billing.json")),
        None,
        Utc::now(),
    )
    .unwrap() else {
        panic!("expected complete legacy data");
    };
    assert_eq!(
        data.tier.as_ref().unwrap().normalized,
        NormalizedTier::SuperGrokHeavy
    );
    assert!(
        data.products
            .iter()
            .any(|product| product.product == "legacy-labs")
    );
    assert_eq!(data.prepaid.unwrap().remaining, 8.5);
    assert!(!data.on_demand.unwrap().enabled);
}

#[test]
fn accepts_empty_products_and_preserves_unknown_tier() {
    let QueryOutcome::Complete { data: empty } = parse_billing(
        fixture(include_str!("fixtures/empty_products.json")),
        None,
        Utc::now(),
    )
    .unwrap() else {
        panic!("empty products are valid");
    };
    assert!(empty.products.is_empty());

    let QueryOutcome::Complete { data: unknown } = parse_billing(
        fixture(include_str!("fixtures/unknown_tier.json")),
        None,
        Utc::now(),
    )
    .unwrap() else {
        panic!("unknown tiers are valid");
    };
    let tier = unknown.tier.unwrap();
    assert_eq!(tier.raw, " QuantumMax-Preview ");
    assert_eq!(tier.normalized, NormalizedTier::Unknown);
    assert_eq!(unknown.products[0].product, "future-product");
}

#[test]
fn object_product_failures_use_positional_scopes_not_vendor_keys() {
    const SECRET_KEY: &str = "secret-account@example.test";
    let outcome = parse_billing(
        json!({
            "usagePercent": 1,
            "products": {
                "chat": 4,
                "secret-account@example.test": "invalid"
            }
        }),
        None,
        Utc::now(),
    )
    .unwrap();
    let QueryOutcome::Partial { data, failures } = outcome else {
        panic!("malformed product map must be partial");
    };
    assert_eq!(data.products.len(), 1);
    assert_eq!(data.products[0].product, "chat");
    assert!(
        failures
            .iter()
            .any(|failure| failure.scope == "products[1]"),
        "{failures:?}"
    );
    assert!(
        failures.iter().all(|failure| {
            !failure.scope.contains(SECRET_KEY) && !failure.message.contains(SECRET_KEY)
        }),
        "{failures:?}"
    );
}

#[test]
fn malformed_optional_fields_return_partial_data() {
    let outcome = parse_billing(
        json!({
            "usagePercent": "not-a-number",
            "products": [{"product": "chat", "usagePercent": 4}],
            "prepaid": {"remaining": "unknown"}
        }),
        None,
        Utc::now(),
    )
    .unwrap();
    assert!(matches!(
        outcome,
        QueryOutcome::Partial { data, failures }
            if data.products.len() == 1
                && failures.iter().any(|failure| failure.scope == "weekly")
                && failures.iter().any(|failure| failure.scope == "prepaid.remaining")
    ));
}

#[test]
fn later_valid_aliases_survive_invalid_preferred_aliases() {
    let outcome = parse_billing(
        json!({
            "tier": null,
            "plan": {"tier": null, "name": "SuperGrok"},
            "current_period": null,
            "currentPeriod": {
                "type": "USAGE_PERIOD_TYPE_WEEKLY",
                "ends_at": null,
                "endAt": "2026-09-14T00:00:00Z"
            },
            "usage_percent": null,
            "usagePercent": 42,
            "products": null,
            "productUsage": [{
                "product": null,
                "name": "chat",
                "usage_percent": null,
                "usagePercent": 42
            }],
            "prepaid": {
                "remaining": null,
                "balance": "5",
                "currency": null,
                "currencyCode": "USD"
            },
            "on_demand": {
                "enabled": null,
                "isEnabled": true,
                "used": null,
                "spend": "2",
                "limit": null,
                "cap": 10,
                "currency": null,
                "currencyCode": "USD"
            }
        }),
        None,
        Utc::now(),
    )
    .unwrap();
    let QueryOutcome::Complete { data } = outcome else {
        panic!("valid compatibility aliases should produce complete data");
    };
    assert_eq!(data.tier.unwrap().normalized, NormalizedTier::SuperGrok);
    assert_eq!(data.usage_percent, Some(42.0));
    assert_eq!(data.products[0].product, "chat");
    assert_eq!(data.products[0].usage_percent, 42.0);
    assert_eq!(data.prepaid.unwrap().remaining, 5.0);
    let on_demand = data.on_demand.unwrap();
    assert!(on_demand.enabled);
    assert_eq!(on_demand.used, Some(2.0));
    assert_eq!(on_demand.limit, Some(10.0));
    assert_eq!(on_demand.currency.as_deref(), Some("USD"));
}

#[test]
fn falls_back_past_placeholder_envelopes_and_reports_nested_loss() {
    let outcome = parse_billing(
        json!({
            "data": {"billing": {"usagePercent": "invalid"}},
            "billing": {
                "currentPeriod": {
                    "type": "USAGE_PERIOD_TYPE_WEEKLY",
                    "startAt": "not-a-date",
                    "endAt": "2026-09-14T00:00:00Z"
                },
                "usagePercent": 9,
                "onDemand": {
                    "enabled": true,
                    "used": "not-a-number",
                    "limit": 20
                }
            }
        }),
        None,
        Utc::now(),
    )
    .unwrap();
    assert!(matches!(
        outcome,
        QueryOutcome::Partial { data, failures }
            if data.usage_percent == Some(9.0)
                && data.current_period.as_ref().and_then(|period| period.ends_at).is_some()
                && failures.iter().any(|failure| failure.scope == "current_period.starts_at")
                && failures.iter().any(|failure| failure.scope == "on_demand.used")
    ));
}

#[test]
fn rejects_recognized_but_entirely_unusable_billing_data() {
    let error = parse_billing(
        json!({"usagePercent": "invalid", "products": ["invalid"]}),
        None,
        Utc::now(),
    )
    .unwrap_err();
    assert!(matches!(error, ProviderError::ProtocolIncompatible { .. }));
}

#[test]
fn extreme_billing_timestamps_fail_without_panicking() {
    let outcome = parse_billing(
        json!({
            "period": {"start": i64::MIN, "end": i64::MAX.to_string()},
            "usagePercent": 1
        }),
        None,
        Utc::now(),
    )
    .unwrap();
    assert!(matches!(
        outcome,
        QueryOutcome::Partial { data, failures }
            if data.usage_percent == Some(1.0)
                && data.current_period.is_none()
                && failures.iter().any(|failure| failure.scope == "current_period.starts_at")
                && failures.iter().any(|failure| failure.scope == "current_period.ends_at")
    ));
}

#[test]
fn transport_errors_keep_distinct_provider_categories() {
    assert!(matches!(
        ProviderError::from(GrokApiError::AuthenticationInvalid("expired".into())),
        ProviderError::AuthenticationInvalid { .. }
    ));
    assert!(matches!(
        ProviderError::from(GrokApiError::RateLimited {
            message: "slow down".into(),
            retry_after_seconds: Some(30),
        }),
        ProviderError::RateLimited {
            retry_after_seconds: Some(30),
            ..
        }
    ));
    assert!(matches!(
        ProviderError::from(GrokApiError::Network("offline".into())),
        ProviderError::Network { .. }
    ));
    assert!(matches!(
        ProviderError::from(GrokApiError::ProtocolIncompatible("shape".into())),
        ProviderError::ProtocolIncompatible { .. }
    ));
}

#[test]
fn token_debug_output_is_redacted() {
    let debug = format!("{:?}", token("account"));
    assert!(!debug.contains("access-token-never-logged"));
    assert!(!debug.contains("refresh-token-never-logged"));
    assert!(debug.contains("[REDACTED]"));

    let device = DeviceAuthorization {
        flow_id: "private-flow".into(),
        device_code: "private-device".into(),
        user_code: "private-user".into(),
        verification_uri:
            "https://accounts.example.invalid/device?user_code=private-user&device_code=private-device"
                .into(),
        expires_at: None,
    };
    let debug = format!("{device:?}");
    assert!(!debug.contains("private-flow"));
    assert!(!debug.contains("private-device"));
    assert!(!debug.contains("private-user"));
    assert!(!debug.contains("accounts.example.invalid"));
}

fn mock_http_server(expected_requests: usize) -> (String, std::thread::JoinHandle<Vec<String>>) {
    let listener = TcpListener::bind("127.0.0.1:0").unwrap();
    let address = format!("http://{}", listener.local_addr().unwrap());
    let handle = std::thread::spawn(move || {
        let mut requests = Vec::new();
        for _ in 0..expected_requests {
            let (mut stream, _) = listener.accept().unwrap();
            let mut bytes = Vec::new();
            let mut buffer = [0_u8; 4096];
            loop {
                let read = stream.read(&mut buffer).unwrap();
                if read == 0 {
                    break;
                }
                bytes.extend_from_slice(&buffer[..read]);
                let Some(headers_end) = bytes.windows(4).position(|window| window == b"\r\n\r\n")
                else {
                    continue;
                };
                let headers = String::from_utf8_lossy(&bytes[..headers_end]);
                let content_length = headers
                    .lines()
                    .find_map(|line| {
                        let (name, value) = line.split_once(':')?;
                        name.eq_ignore_ascii_case("content-length")
                            .then(|| value.trim().parse::<usize>().ok())
                            .flatten()
                    })
                    .unwrap_or(0);
                if bytes.len() >= headers_end + 4 + content_length {
                    break;
                }
            }
            let request = String::from_utf8(bytes).unwrap();
            let path = request.split_whitespace().nth(1).unwrap_or_default();
            let body = request.split("\r\n\r\n").nth(1).unwrap_or_default();
            let response_body = if path == "/device" {
                r#"{"flow_id":"device-secret","device_code":"device-secret","user_code":"SHOW-ME","verification_uri":"https://accounts.example.invalid/device","expires_in":600}"#
            } else if path == "/token" && body.contains("grant_type=refresh_token") {
                r#"{"access_token":"refreshed","refresh_token":"refresh","expires_in":3600,"account_label":"http-account"}"#
            } else if path == "/token" {
                r#"{"access_token":"access","refresh_token":"refresh","expires_in":3600,"account_label":"http-account"}"#
            } else if path == "/billing" {
                r#"{"usagePercent":11,"products":[{"product":"chat","usagePercent":11}],"tier":"SuperGrok"}"#
            } else if path == "/settings" {
                r#"{"subscription_tier_display":"SuperGrok Heavy","allow_access":true,"gate_message":null}"#
            } else {
                "{}"
            };
            let response = format!(
                "HTTP/1.1 200 OK\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{}",
                response_body.len(),
                response_body
            );
            stream.write_all(response.as_bytes()).unwrap();
            requests.push(request);
        }
        requests
    });
    (address, handle)
}

fn http_config(base: &str) -> HttpGrokConfig {
    HttpGrokConfig {
        client_id: "public-client-id".into(),
        scope: "openid profile offline_access".into(),
        redirect_uri: format!("{base}/callback"),
        device_authorization_url: format!("{base}/device"),
        authorization_url: format!("{base}/authorize"),
        token_url: format!("{base}/token"),
        revoke_url: format!("{base}/revoke"),
        billing_url: format!("{base}/billing"),
        settings_url: format!("{base}/settings"),
    }
}

fn single_response_server(response: impl Into<String>) -> String {
    let response = response.into();
    let listener = TcpListener::bind("127.0.0.1:0").unwrap();
    let address = format!("http://{}", listener.local_addr().unwrap());
    std::thread::spawn(move || {
        let (mut stream, _) = listener.accept().unwrap();
        let mut buffer = [0_u8; 4096];
        let _ = stream.read(&mut buffer).unwrap();
        stream.write_all(response.as_bytes()).unwrap();
    });
    address
}

fn mock_http_response(status: &str, content_type: &str, body: &str) -> String {
    format!(
        "HTTP/1.1 {status}\r\nContent-Type: {content_type}\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{body}",
        body.len()
    )
}

#[tokio::test]
async fn concrete_http_transport_runs_oauth_billing_refresh_and_revoke() {
    let (base, server) = mock_http_server(8);
    let provider = GrokProvider::new(HttpGrokTransport::new(http_config(&base)).unwrap());

    let device = provider
        .start_auth(AuthStartRequest {
            method: Some(AuthMethod::DeviceCode),
            redirect_uri: None,
        })
        .await
        .unwrap();
    assert_ne!(device.flow_id, "device-secret");
    provider
        .complete_auth(AuthCompleteRequest {
            flow_id: device.flow_id,
            authorization_code: None,
            redirect_uri: None,
        })
        .await
        .unwrap();
    assert!(matches!(
        provider.query(UsageQuery::default()).await.unwrap(),
        QueryOutcome::Complete { data } if data.usage_percent == Some(11.0)
    ));
    provider.refresh_auth().await.unwrap();
    provider.logout(LogoutRequest::default()).await.unwrap();

    let browser = provider
        .start_auth(AuthStartRequest {
            method: Some(AuthMethod::BrowserOAuth),
            redirect_uri: None,
        })
        .await
        .unwrap();
    let authorization_uri = browser.verification_uri.as_ref().unwrap();
    assert!(authorization_uri.contains("code_challenge="));
    assert!(authorization_uri.contains("state="));
    provider
        .complete_auth(AuthCompleteRequest {
            flow_id: browser.flow_id,
            authorization_code: Some("browser-code".into()),
            redirect_uri: Some(format!("{base}/callback")),
        })
        .await
        .unwrap();
    provider.logout(LogoutRequest::default()).await.unwrap();

    let requests = server.join().unwrap();
    assert!(
        requests
            .iter()
            .any(|request| request.starts_with("POST /device "))
    );
    assert!(
        requests
            .iter()
            .any(|request| request.contains("device_code=device-secret"))
    );
    assert!(
        requests
            .iter()
            .any(|request| request.contains("code_verifier="))
    );
    assert!(
        requests
            .iter()
            .any(|request| request.starts_with("GET /billing "))
    );
    assert!(requests.iter().any(|request| {
        request.starts_with("GET /settings ")
            && request
                .to_ascii_lowercase()
                .contains("authorization: bearer access")
    }));
    assert_eq!(
        requests
            .iter()
            .filter(|request| request.starts_with("POST /revoke "))
            .count(),
        2
    );
}

#[tokio::test]
async fn superseded_browser_flow_removes_its_pkce_verifier() {
    let config = http_config("http://127.0.0.1:9");
    let transport = Arc::new(HttpGrokTransport::new(config.clone()).unwrap());
    let provider = GrokProvider::with_transport(transport.clone());
    let first = provider
        .start_auth(AuthStartRequest {
            method: Some(AuthMethod::BrowserOAuth),
            redirect_uri: None,
        })
        .await
        .unwrap();
    provider
        .start_auth(AuthStartRequest {
            method: Some(AuthMethod::BrowserOAuth),
            redirect_uri: None,
        })
        .await
        .unwrap();

    let error = transport
        .complete_browser_authorization(&first.flow_id, "stale-code", config.redirect_uri.as_str())
        .await
        .unwrap_err();
    assert!(matches!(error, GrokApiError::AuthenticationInvalid(_)));
}

#[test]
fn concrete_transport_rejects_non_tls_remote_endpoints() {
    let error = HttpGrokTransport::new(HttpGrokConfig {
        client_id: "client".into(),
        scope: "openid".into(),
        redirect_uri: "http://127.0.0.1/callback".into(),
        device_authorization_url: "http://example.com/device".into(),
        authorization_url: "https://example.com/authorize".into(),
        token_url: "https://example.com/token".into(),
        revoke_url: "https://example.com/revoke".into(),
        billing_url: "https://example.com/billing".into(),
        settings_url: "https://example.com/settings".into(),
    })
    .err()
    .expect("remote HTTP must be rejected");
    assert!(matches!(error, GrokApiError::ProtocolIncompatible(_)));
}

#[test]
fn concrete_transport_accepts_ipv6_loopback_configuration() {
    let base = "http://[::1]";
    assert!(HttpGrokTransport::new(http_config(base)).is_ok());
}

#[tokio::test]
async fn concrete_device_flow_rejects_a_missing_expiry() {
    let base = single_response_server(
        "HTTP/1.1 200 OK\r\nContent-Type: application/json\r\nConnection: close\r\n\r\n{\"device_code\":\"device-secret\",\"user_code\":\"SHOW-ME\",\"verification_uri\":\"https://accounts.example.invalid/device\"}",
    );
    let transport = HttpGrokTransport::new(http_config(&base)).unwrap();
    let error = transport.start_device_authorization().await.unwrap_err();
    assert!(matches!(error, GrokApiError::ProtocolIncompatible(_)));
}

#[tokio::test]
async fn concrete_transport_maps_http_rate_limits_without_exposing_body() {
    let base = single_response_server(
        "HTTP/1.1 429 Too Many Requests\r\nRetry-After: 17\r\nContent-Length: 18\r\nConnection: close\r\n\r\nprivate error body",
    );
    let transport = HttpGrokTransport::new(http_config(&base)).unwrap();
    let error = transport.fetch_billing("private-token").await.unwrap_err();
    assert_eq!(
        error,
        GrokApiError::RateLimited {
            message: "Grok rate limit exceeded".into(),
            retry_after_seconds: Some(17),
        }
    );
}

#[tokio::test]
async fn unauthenticated_device_authorization_403_is_protocol_incompatible() {
    let base = single_response_server(mock_http_response(
        "403 Forbidden",
        "text/html",
        "<html>blocked by cloudflare</html>",
    ));
    let transport = HttpGrokTransport::new(http_config(&base)).unwrap();
    let error = transport.start_device_authorization().await.unwrap_err();
    assert!(matches!(error, GrokApiError::ProtocolIncompatible(_)));
    if let GrokApiError::ProtocolIncompatible(message) = error {
        assert!(message.contains("non-JSON"));
        assert!(message.contains("403"));
        assert!(!message.contains("cloudflare"));
        assert!(!message.to_ascii_lowercase().contains("credential"));
    } else {
        panic!("expected ProtocolIncompatible");
    }
}

#[tokio::test]
async fn credential_bearing_billing_403_is_authentication_invalid() {
    let base = single_response_server(mock_http_response(
        "403 Forbidden",
        "text/html",
        "<html>blocked by cloudflare</html>",
    ));
    let transport = HttpGrokTransport::new(http_config(&base)).unwrap();
    let error = transport.fetch_billing("private-token").await.unwrap_err();
    assert_eq!(
        error,
        GrokApiError::AuthenticationInvalid("Grok rejected the credential".into())
    );
}

#[tokio::test]
async fn unauthenticated_device_authorization_403_json_is_still_protocol_incompatible() {
    let base = single_response_server(mock_http_response(
        "403 Forbidden",
        "application/json",
        r#"{"error":"forbidden"}"#,
    ));
    let transport = HttpGrokTransport::new(http_config(&base)).unwrap();
    let error = transport.start_device_authorization().await.unwrap_err();
    assert!(matches!(error, GrokApiError::ProtocolIncompatible(_)));
    if let GrokApiError::ProtocolIncompatible(message) = error {
        assert!(message.contains("client request"));
        assert!(!message.to_ascii_lowercase().contains("credential"));
        assert!(!message.contains("forbidden"));
    } else {
        panic!("expected ProtocolIncompatible");
    }
}

#[tokio::test]
async fn device_poll_403_with_device_code_is_authentication_invalid() {
    let base = single_response_server(mock_http_response(
        "403 Forbidden",
        "text/html",
        "<html>blocked by cloudflare</html>",
    ));
    let transport = HttpGrokTransport::new(http_config(&base)).unwrap();
    let error = transport
        .poll_device_authorization("device-secret")
        .await
        .unwrap_err();
    assert_eq!(
        error,
        GrokApiError::AuthenticationInvalid("Grok rejected the credential".into())
    );
}

#[tokio::test]
async fn unauthenticated_device_authorization_problem_json_is_not_reported_as_non_json() {
    let base = single_response_server(mock_http_response(
        "403 Forbidden",
        "application/problem+json",
        r#"{"title":"forbidden"}"#,
    ));
    let transport = HttpGrokTransport::new(http_config(&base)).unwrap();
    let error = transport.start_device_authorization().await.unwrap_err();
    assert!(matches!(error, GrokApiError::ProtocolIncompatible(_)));
    if let GrokApiError::ProtocolIncompatible(message) = error {
        assert!(message.contains("client request"));
        assert!(!message.contains("non-JSON"));
        assert!(!message.contains("forbidden"));
    } else {
        panic!("expected ProtocolIncompatible");
    }
}

#[tokio::test]
async fn device_poll_400_html_content_type_rejects_json_shaped_body() {
    let base = single_response_server(mock_http_response(
        "400 Bad Request",
        "text/html",
        r#"{"error":"authorization_pending"}"#,
    ));
    let transport = HttpGrokTransport::new(http_config(&base)).unwrap();
    let error = transport
        .poll_device_authorization("device-secret")
        .await
        .unwrap_err();
    assert!(matches!(error, GrokApiError::ProtocolIncompatible(_)));
    if let GrokApiError::ProtocolIncompatible(message) = error {
        assert!(message.contains("non-JSON Content-Type"));
        assert!(message.contains("400"));
        assert!(!message.contains("authorization_pending"));
    } else {
        panic!("expected ProtocolIncompatible");
    }
}

#[tokio::test]
async fn token_refresh_400_html_content_type_rejects_json_shaped_body() {
    let base = single_response_server(mock_http_response(
        "400 Bad Request",
        "text/html",
        r#"{"error":"invalid_grant"}"#,
    ));
    let transport = HttpGrokTransport::new(http_config(&base)).unwrap();
    let error = transport.refresh("refresh-token").await.unwrap_err();
    assert!(matches!(error, GrokApiError::ProtocolIncompatible(_)));
    if let GrokApiError::ProtocolIncompatible(message) = error {
        assert!(message.contains("non-JSON Content-Type"));
        assert!(message.contains("400"));
        assert!(!message.contains("invalid_grant"));
    } else {
        panic!("expected ProtocolIncompatible");
    }
}
