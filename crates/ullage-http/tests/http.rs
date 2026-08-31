use std::collections::VecDeque;
use std::io::{Read, Write};
use std::net::{SocketAddr, TcpStream};
use std::sync::Arc;
use std::sync::Mutex;
use std::time::Duration;

use async_trait::async_trait;
use tempfile::TempDir;
use ullage_core::{
    Capability, Provider, ProviderDescriptor, ProviderError, ProviderId, ProviderRegistry,
    ProviderResult, QueryOutcome, SubscriptionUsage, UsageQuery,
};
use ullage_daemon::{
    AccountConfig, AccountId, BackoffConfig, ControlService, DaemonEngine, MemorySnapshotStore,
    SystemClock,
};
use ullage_http::{HttpBindConfig, HttpServer, load_or_create_token};
use ullage_protocol::{
    AuthChallenge, AuthCompleteRequest, AuthStartRequest, AuthState, LogoutRequest,
};

#[derive(Clone)]
struct MockProvider {
    inner: Arc<MockInner>,
}

struct MockInner {
    id: ProviderId,
    results: Mutex<VecDeque<ProviderResult<QueryOutcome<SubscriptionUsage>>>>,
}

impl MockProvider {
    fn new(
        id: &str,
        results: impl IntoIterator<Item = ProviderResult<QueryOutcome<SubscriptionUsage>>>,
    ) -> Self {
        Self {
            inner: Arc::new(MockInner {
                id: ProviderId::new(id),
                results: Mutex::new(results.into_iter().collect()),
            }),
        }
    }
}

#[async_trait]
impl Provider for MockProvider {
    type VendorUsage = SubscriptionUsage;

    fn descriptor(&self) -> ProviderDescriptor {
        ProviderDescriptor {
            id: self.inner.id.clone(),
            display_name: self.inner.id.as_str().into(),
            capabilities: vec![Capability::UsageQuery],
        }
    }

    async fn start_auth(&self, _: AuthStartRequest) -> ProviderResult<AuthChallenge> {
        Err(ProviderError::UnsupportedCapability {
            capability: "authentication".into(),
        })
    }

    async fn complete_auth(&self, _: AuthCompleteRequest) -> ProviderResult<AuthState> {
        Err(ProviderError::UnsupportedCapability {
            capability: "authentication".into(),
        })
    }

    async fn auth_status(&self) -> ProviderResult<AuthState> {
        Err(ProviderError::UnsupportedCapability {
            capability: "authentication".into(),
        })
    }

    async fn logout(&self, _: LogoutRequest) -> ProviderResult<()> {
        Err(ProviderError::UnsupportedCapability {
            capability: "logout".into(),
        })
    }

    async fn query(&self, _: UsageQuery) -> ProviderResult<QueryOutcome<Self::VendorUsage>> {
        self.inner
            .results
            .lock()
            .unwrap()
            .pop_front()
            .unwrap_or_else(|| {
                Err(ProviderError::Network {
                    message: "no result".into(),
                })
            })
    }

    fn normalize(&self, usage: Self::VendorUsage) -> ProviderResult<SubscriptionUsage> {
        Ok(usage)
    }
}

struct Harness {
    addr: SocketAddr,
    token: String,
    _directory: TempDir,
    engine: DaemonEngine,
    task: tokio::task::JoinHandle<Result<(), String>>,
}

impl Harness {
    async fn start(allowed_origins: Vec<String>, probe_min_interval: Duration) -> Self {
        let mut registry = ProviderRegistry::default();
        registry
            .register(MockProvider::new(
                "claude",
                [
                    Ok(QueryOutcome::Complete {
                        data: SubscriptionUsage {
                            provider: ProviderId::new("claude"),
                            account_label: Some("primary".into()),
                            plan: None,
                            subscription_expires_at: None,
                            observed_at: chrono::Utc::now(),
                            windows: Vec::new(),
                        },
                    }),
                    Err(ProviderError::AuthenticationInvalid {
                        message: "bad credential".into(),
                    }),
                    Err(ProviderError::RateLimited {
                        message: "slow down".into(),
                        retry_after_seconds: Some(9),
                    }),
                    Err(ProviderError::Network {
                        message: "timeout-like".into(),
                    }),
                ],
            ))
            .unwrap();
        let engine = DaemonEngine::new(
            ullage_daemon::DaemonConfig::default(),
            Arc::new(registry),
            Arc::new(SystemClock),
            Arc::new(MemorySnapshotStore::default()),
        )
        .await
        .unwrap();
        for id in ["primary", "team/a"] {
            engine
                .add_account(AccountConfig {
                    id: AccountId::new(id),
                    provider: ProviderId::new("claude"),
                    query: UsageQuery {
                        account_label: Some(id.into()),
                    },
                    enabled: true,
                    interval: Duration::from_secs(60),
                    timeout: Duration::from_secs(5),
                    jitter: Duration::ZERO,
                    backoff: BackoffConfig::default(),
                })
                .await
                .unwrap();
        }
        let directory = tempfile::tempdir().unwrap();
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            std::fs::set_permissions(directory.path(), std::fs::Permissions::from_mode(0o700))
                .unwrap();
        }
        let token_path = directory.path().join("http-token");
        let token = load_or_create_token(&token_path).unwrap();
        let server = HttpServer::bind(
            HttpBindConfig {
                bind: "127.0.0.1:0".parse().unwrap(),
                allowed_origins,
                probe_min_interval,
                token_path,
            },
            ControlService::new(engine.clone()),
        )
        .await
        .unwrap();
        let addr = server.local_addr();
        let task = tokio::spawn(server.run());
        Self {
            addr,
            token,
            _directory: directory,
            engine,
            task,
        }
    }

    async fn shutdown(self) {
        self.engine.shutdown();
        let _ = tokio::time::timeout(Duration::from_secs(2), self.task).await;
    }
}

struct RawResponse {
    status: u16,
    headers: Vec<(String, String)>,
    body: String,
}

impl RawResponse {
    fn header(&self, name: &str) -> Option<&str> {
        let name = name.to_ascii_lowercase();
        self.headers
            .iter()
            .find(|(key, _)| key.to_ascii_lowercase() == name)
            .map(|(_, value)| value.as_str())
    }
}

fn exchange(addr: SocketAddr, request: &str) -> RawResponse {
    let mut stream = TcpStream::connect(addr).unwrap();
    stream
        .set_read_timeout(Some(Duration::from_secs(2)))
        .unwrap();
    stream.write_all(request.as_bytes()).unwrap();
    let mut bytes = Vec::new();
    let mut buffer = [0; 8192];
    loop {
        match stream.read(&mut buffer) {
            Ok(0) => break,
            Ok(read) => {
                bytes.extend_from_slice(&buffer[..read]);
                if bytes.windows(4).any(|window| window == b"\r\n\r\n") {
                    if let Some(end) = bytes.windows(4).position(|window| window == b"\r\n\r\n") {
                        let headers = &bytes[..end];
                        if let Some(length) = content_length(headers)
                            && bytes.len() >= end + 4 + length
                        {
                            break;
                        }
                        if headers
                            .windows(19)
                            .any(|window| window.eq_ignore_ascii_case(b"transfer-encoding:"))
                        {
                            if bytes.windows(5).any(|window| window == b"0\r\n\r\n") {
                                break;
                            }
                        } else if length_or_zero(headers) == 0 {
                            break;
                        }
                    }
                }
            }
            Err(_) => break,
        }
    }
    parse_response(&bytes)
}

fn content_length(headers: &[u8]) -> Option<usize> {
    let text = String::from_utf8_lossy(headers);
    text.lines().find_map(|line| {
        let (name, value) = line.split_once(':')?;
        if name.eq_ignore_ascii_case("content-length") {
            value.trim().parse().ok()
        } else {
            None
        }
    })
}

fn length_or_zero(headers: &[u8]) -> usize {
    content_length(headers).unwrap_or(0)
}

fn parse_response(bytes: &[u8]) -> RawResponse {
    let text = String::from_utf8_lossy(bytes);
    let (head, body) = text.split_once("\r\n\r\n").unwrap_or((&text, ""));
    let mut lines = head.split("\r\n");
    let status = lines
        .next()
        .and_then(|line| line.split_whitespace().nth(1))
        .and_then(|status| status.parse().ok())
        .unwrap_or(0);
    let headers = lines
        .filter_map(|line| {
            let (name, value) = line.split_once(':')?;
            Some((name.trim().to_owned(), value.trim().to_owned()))
        })
        .collect();
    RawResponse {
        status,
        headers,
        body: body.to_owned(),
    }
}

fn get(addr: SocketAddr, path: &str, token: Option<&str>, extra: &str) -> RawResponse {
    let authorization = token
        .map(|token| format!("Authorization: Bearer {token}\r\n"))
        .unwrap_or_default();
    exchange(
        addr,
        &format!(
            "GET {path} HTTP/1.1\r\nHost: 127.0.0.1:{}\r\n{authorization}{extra}Connection: close\r\n\r\n",
            addr.port()
        ),
    )
}

fn post(addr: SocketAddr, path: &str, token: &str, extra: &str) -> RawResponse {
    exchange(
        addr,
        &format!(
            "POST {path} HTTP/1.1\r\nHost: 127.0.0.1:{}\r\nAuthorization: Bearer {token}\r\n{extra}Content-Length: 0\r\nConnection: close\r\n\r\n",
            addr.port()
        ),
    )
}

#[tokio::test(flavor = "multi_thread")]
async fn rejects_disallowed_bind_and_names_the_setting() {
    let engine = DaemonEngine::new(
        ullage_daemon::DaemonConfig::default(),
        Arc::new(ProviderRegistry::default()),
        Arc::new(SystemClock),
        Arc::new(MemorySnapshotStore::default()),
    )
    .await
    .unwrap();
    let directory = tempfile::tempdir().unwrap();
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        std::fs::set_permissions(directory.path(), std::fs::Permissions::from_mode(0o700)).unwrap();
    }
    let error = match HttpServer::bind(
        HttpBindConfig {
            bind: "0.0.0.0:0".parse().unwrap(),
            allowed_origins: Vec::new(),
            probe_min_interval: Duration::from_secs(60),
            token_path: directory.path().join("http-token"),
        },
        ControlService::new(engine),
    )
    .await
    {
        Ok(_) => panic!("disallowed http.bind should fail"),
        Err(error) => error.to_string(),
    };
    assert!(error.contains("http.bind"), "{error}");
    assert!(error.contains("loopback"), "{error}");
}

#[tokio::test(flavor = "multi_thread")]
async fn authenticates_with_bearer_token_and_maps_control_errors() {
    let harness = Harness::start(Vec::new(), Duration::from_secs(60)).await;
    let missing = get(harness.addr, "/v1/status", None, "");
    assert_eq!(missing.status, 401);
    assert_eq!(missing.body, "{\"error\":\"unauthorized\"}");
    let unauthenticated_body = exchange(
        harness.addr,
        &format!(
            "POST /v1/accounts/primary/probe HTTP/1.1\r\nHost: 127.0.0.1:{}\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{}",
            harness.addr.port(),
            1024 * 1024 + 1,
            "x".repeat(1024 * 1024 + 1)
        ),
    );
    assert_eq!(unauthenticated_body.status, 401);
    let wrong = get(harness.addr, "/v1/status", Some("wrong-token"), "");
    assert_eq!(wrong.status, 401);
    assert_eq!(wrong.body, missing.body);
    let status = get(harness.addr, "/v1/status", Some(&harness.token), "");
    assert_eq!(status.status, 200);
    assert!(status.body.contains("daemon_status"), "{}", status.body);
    assert!(
        status.body.contains("\"version\":8") || status.body.contains("\"version\": 8"),
        "{}",
        status.body
    );
    let illegal = get(
        harness.addr,
        "/v1/status?account=primary",
        Some(&harness.token),
        "",
    );
    assert_eq!(illegal.status, 400);
    assert_eq!(status.header("cache-control"), Some("no-store"));
    let unknown = get(harness.addr, "/v1/missing", Some(&harness.token), "");
    assert_eq!(unknown.status, 404);
    let missing_account = get(
        harness.addr,
        "/v1/accounts/missing",
        Some(&harness.token),
        "",
    );
    assert_eq!(missing_account.status, 404);
    let probe = post(
        harness.addr,
        "/v1/accounts/primary/probe",
        &harness.token,
        "",
    );
    assert_eq!(probe.status, 200);
    assert!(probe.body.contains("\"probe\""), "{}", probe.body);
    let auth_invalid = post(
        harness.addr,
        "/v1/accounts/primary/probe?wait=true",
        &harness.token,
        "",
    );
    assert_eq!(auth_invalid.status, 429);
    assert!(auth_invalid.header("retry-after").is_some());
    harness.shutdown().await;
}

#[tokio::test(flavor = "multi_thread")]
async fn host_and_origin_checks_follow_the_whitelist() {
    let harness = Harness::start(
        vec!["https://gui.example.test".into()],
        Duration::from_secs(60),
    )
    .await;
    let bad_host = exchange(
        harness.addr,
        &format!(
            "GET /v1/status HTTP/1.1\r\nHost: evil.example:{}\r\nAuthorization: Bearer {}\r\nConnection: close\r\n\r\n",
            harness.addr.port(),
            harness.token
        ),
    );
    assert_eq!(bad_host.status, 403);
    assert!(bad_host.header("access-control-allow-origin").is_none());
    let denied_origin = get(
        harness.addr,
        "/v1/status",
        Some(&harness.token),
        "Origin: https://evil.example\r\n",
    );
    assert_eq!(denied_origin.status, 200);
    assert!(
        denied_origin
            .header("access-control-allow-origin")
            .is_none()
    );
    assert_ne!(
        denied_origin.header("access-control-allow-origin"),
        Some("*")
    );
    let allowed = get(
        harness.addr,
        "/v1/status",
        Some(&harness.token),
        "Origin: https://gui.example.test\r\n",
    );
    assert_eq!(allowed.status, 200);
    assert_eq!(
        allowed.header("access-control-allow-origin"),
        Some("https://gui.example.test")
    );
    assert_eq!(allowed.header("vary"), Some("Origin"));
    assert!(allowed.header("access-control-allow-credentials").is_none());
    let preflight = exchange(
        harness.addr,
        &format!(
            "OPTIONS /v1/status HTTP/1.1\r\nHost: 127.0.0.1:{}\r\nOrigin: https://gui.example.test\r\nAccess-Control-Request-Method: GET\r\nConnection: close\r\n\r\n",
            harness.addr.port()
        ),
    );
    assert_eq!(preflight.status, 204);
    assert_eq!(
        preflight.header("access-control-allow-origin"),
        Some("https://gui.example.test")
    );
    harness.shutdown().await;
}

#[tokio::test(flavor = "multi_thread")]
async fn probe_cooldown_returns_429_with_retry_after() {
    let harness = Harness::start(Vec::new(), Duration::from_secs(60)).await;
    let first = post(
        harness.addr,
        "/v1/accounts/primary/probe?wait=false",
        &harness.token,
        "",
    );
    assert!(
        first.status == 202 || first.status == 200,
        "{}",
        first.status
    );
    let second = post(
        harness.addr,
        "/v1/accounts/primary/probe?wait=false",
        &harness.token,
        "",
    );
    assert_eq!(second.status, 429);
    assert!(second.header("retry-after").is_some());
    assert!(second.body.contains("rate_limited"), "{}", second.body);
    harness.shutdown().await;
}

#[tokio::test(flavor = "multi_thread")]
async fn usage_reads_snapshots_without_calling_the_provider() {
    let harness = Harness::start(Vec::new(), Duration::from_secs(60)).await;
    let usage = get(harness.addr, "/v1/usage", Some(&harness.token), "");
    assert_eq!(usage.status, 200);
    assert!(usage.body.contains("snapshots"), "{}", usage.body);
    let accounts = get(harness.addr, "/v1/accounts", Some(&harness.token), "");
    assert_eq!(accounts.status, 200);
    assert!(accounts.body.contains("primary"), "{}", accounts.body);
    let encoded = get(
        harness.addr,
        "/v1/accounts/team%2Fa",
        Some(&harness.token),
        "",
    );
    assert_eq!(encoded.status, 200, "{}", encoded.body);
    assert!(encoded.body.contains("team/a"), "{}", encoded.body);
    harness.shutdown().await;
}

#[tokio::test(flavor = "multi_thread")]
async fn oversized_bodies_are_rejected_without_taking_the_listener_down() {
    let harness = Harness::start(Vec::new(), Duration::from_secs(60)).await;
    let body = "x".repeat(1024 * 1024 + 1);
    let oversized = exchange(
        harness.addr,
        &format!(
            "POST /v1/accounts/primary/probe HTTP/1.1\r\nHost: 127.0.0.1:{}\r\nAuthorization: Bearer {}\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{body}",
            harness.addr.port(),
            harness.token,
            body.len()
        ),
    );
    assert_eq!(oversized.status, 413);
    let status = get(harness.addr, "/v1/status", Some(&harness.token), "");
    assert_eq!(status.status, 200);
    harness.shutdown().await;
}

#[tokio::test(flavor = "multi_thread")]
async fn maps_timeout_and_does_not_attach_diagnostics_by_default() {
    let harness = Harness::start(Vec::new(), Duration::ZERO).await;
    let _ok = post(
        harness.addr,
        "/v1/accounts/primary/probe",
        &harness.token,
        "",
    );
    let invalid = post(
        harness.addr,
        "/v1/accounts/primary/probe",
        &harness.token,
        "",
    );
    assert_eq!(invalid.status, 409, "{}", invalid.body);
    assert!(!invalid.body.contains("bad credential"), "{}", invalid.body);
    let diagnosed = post(
        harness.addr,
        "/v1/accounts/primary/probe?diagnose=1",
        &harness.token,
        "",
    );
    assert_eq!(diagnosed.status, 429, "{}", diagnosed.body);
    harness.shutdown().await;
}
