#![allow(dead_code)]
use std::collections::VecDeque;
use std::io::{ErrorKind, Read, Write};
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
use ullage_http::{HttpBindConfig, HttpServer};
use ullage_protocol::{
    AuthChallenge, AuthCompleteRequest, AuthStartRequest, AuthState, LogoutRequest,
};

#[derive(Clone)]
pub struct MockProvider {
    inner: Arc<MockInner>,
}

struct MockInner {
    id: ProviderId,
    results: Mutex<VecDeque<ProviderResult<QueryOutcome<SubscriptionUsage>>>>,
}

impl MockProvider {
    pub fn new(
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

pub struct Harness {
    pub addr: SocketAddr,
    pub token: String,
    pub _directory: TempDir,
    pub engine: DaemonEngine,
    pub service: ControlService,
    pub task: tokio::task::JoinHandle<Result<(), String>>,
}

impl Harness {
    pub async fn start(allowed_origins: Vec<String>, probe_min_interval: Duration) -> Self {
        Self::start_with_pairing(allowed_origins, probe_min_interval, true).await
    }

    pub async fn start_unpaired() -> Self {
        Self::start_with_pairing(Vec::new(), Duration::from_secs(60), false).await
    }

    pub async fn start_with_pairing(
        allowed_origins: Vec<String>,
        probe_min_interval: Duration,
        pair_device: bool,
    ) -> Self {
        Self::start_with_pairing_and_query(allowed_origins, probe_min_interval, pair_device, None)
            .await
    }

    /// Starts a harness whose provider answers the first probe with `first`.
    pub async fn start_with_first_query(
        first: ProviderResult<QueryOutcome<SubscriptionUsage>>,
        probe_min_interval: Duration,
    ) -> Self {
        Self::start_with_pairing_and_query(Vec::new(), probe_min_interval, true, Some(first)).await
    }

    pub async fn start_with_pairing_and_query(
        allowed_origins: Vec<String>,
        probe_min_interval: Duration,
        pair_device: bool,
        first_query: Option<ProviderResult<QueryOutcome<SubscriptionUsage>>>,
    ) -> Self {
        let first_query = first_query.unwrap_or_else(|| {
            Ok(QueryOutcome::Complete {
                data: SubscriptionUsage {
                    provider: ProviderId::new("claude"),
                    account_label: Some("primary".into()),
                    plan: None,
                    subscription_expires_at: None,
                    observed_at: chrono::Utc::now(),
                    windows: Vec::new(),
                },
            })
        });
        let mut registry = ProviderRegistry::default();
        registry
            .register(MockProvider::new(
                "claude",
                [
                    first_query,
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
                    metrics: Vec::new(),
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
        let device_store_path = directory.path().join("devices.json");
        let service = ControlService::new(engine.clone());
        let server = HttpServer::bind(
            HttpBindConfig {
                binds: vec!["127.0.0.1:0".parse().unwrap()],
                allowed_origins,
                probe_min_interval,
                device_store_path,
            },
            service.clone(),
        )
        .await
        .unwrap();
        let token = if pair_device {
            let pair_code = service.create_pair_code().unwrap();
            service
                .pair_device(&pair_code.code, "test-device")
                .unwrap()
                .device_token
        } else {
            String::new()
        };
        let addr = server.local_addrs()[0];
        let task = tokio::spawn(server.run());
        Self {
            addr,
            token,
            _directory: directory,
            engine,
            service,
            task,
        }
    }

    pub async fn shutdown(self) {
        self.engine.shutdown();
        let _ = tokio::time::timeout(Duration::from_secs(2), self.task).await;
    }
}

pub struct RawResponse {
    pub status: u16,
    pub headers: Vec<(String, String)>,
    pub body: String,
}

impl RawResponse {
    pub fn header(&self, name: &str) -> Option<&str> {
        let name = name.to_ascii_lowercase();
        self.headers
            .iter()
            .find(|(key, _)| key.to_ascii_lowercase() == name)
            .map(|(_, value)| value.as_str())
    }
}

pub fn exchange(addr: SocketAddr, request: &str) -> RawResponse {
    let mut stream = TcpStream::connect(addr).unwrap();
    stream
        .set_read_timeout(Some(Duration::from_secs(2)))
        .unwrap();
    if let Err(error) = stream.write_all(request.as_bytes()) {
        assert!(
            matches!(
                error.kind(),
                ErrorKind::BrokenPipe | ErrorKind::ConnectionReset
            ),
            "request write failed: {error}"
        );
    }
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
                        if let Some(length) = content_length(headers) {
                            if bytes.len() >= end + 4 + length {
                                break;
                            }
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

pub fn content_length(headers: &[u8]) -> Option<usize> {
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

pub fn length_or_zero(headers: &[u8]) -> usize {
    content_length(headers).unwrap_or(0)
}

pub fn parse_response(bytes: &[u8]) -> RawResponse {
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

pub fn get(addr: SocketAddr, path: &str, token: Option<&str>, extra: &str) -> RawResponse {
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

pub fn post(addr: SocketAddr, path: &str, token: &str, extra: &str) -> RawResponse {
    exchange(
        addr,
        &format!(
            "POST {path} HTTP/1.1\r\nHost: 127.0.0.1:{}\r\nAuthorization: Bearer {token}\r\n{extra}Content-Length: 0\r\nConnection: close\r\n\r\n",
            addr.port()
        ),
    )
}

pub fn post_pair(addr: SocketAddr, body: &str, content_type: &str) -> RawResponse {
    post_pair_at(addr, "/v1/pair", body, content_type)
}

pub fn post_pair_at(addr: SocketAddr, path: &str, body: &str, content_type: &str) -> RawResponse {
    exchange(
        addr,
        &format!(
            "POST {path} HTTP/1.1\r\nHost: 127.0.0.1:{}\r\nContent-Type: {content_type}\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{body}",
            addr.port(),
            body.len(),
        ),
    )
}
