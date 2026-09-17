use std::collections::{BTreeSet, HashMap};
use std::convert::Infallible;
use std::net::{IpAddr, SocketAddr};
use std::path::PathBuf;
use std::sync::Arc;
use std::sync::Mutex;
use std::time::{Duration, Instant};

use bytes::Bytes;
use http_body_util::{BodyExt, Full, Limited};
use hyper::body::Incoming;
use hyper::header::{HeaderName, HeaderValue};
use hyper::server::conn::http1;
use hyper::service::service_fn;
use hyper::{Method, Request, Response, StatusCode, header};
use hyper_util::rt::{TokioIo, TokioTimer};
use serde::{Deserialize, Serialize};
use tokio::net::TcpListener;
use tokio::sync::Semaphore;
use ullage_daemon::{AccountId, ControlService, PairDeviceError};
use ullage_protocol::{
    ControlCommand, ControlError, ControlRequest, ControlResponse, ControlResult, ProviderError,
};

use crate::bind::INVALID_BIND_MESSAGE;
use crate::query::{QueryParams, decode_component};
use crate::{BindAddressClass, classify_bind_address};

const MAXIMUM_REQUEST_BYTES: usize = 1024 * 1024;
const MAXIMUM_PAIR_REQUEST_BYTES: usize = 4 * 1024;
const MAXIMUM_CONNECTIONS: usize = 256;
const PAIR_MIN_INTERVAL: Duration = Duration::from_secs(1);
const PAIR_RATE_LIMIT_CAPACITY: usize = 4096;
const PROBE_RATE_LIMIT_CAPACITY: usize = 4096;
const READ_TIMEOUT: Duration = Duration::from_secs(10);
/// Accept errors classified as transient back off exponentially from this
/// delay so an exhausted fd table cannot spin the accept loop.
const ACCEPT_BACKOFF_INITIAL: Duration = Duration::from_millis(10);
const ACCEPT_BACKOFF_MAXIMUM: Duration = Duration::from_secs(1);
/// A transient accept error that outlives this many consecutive attempts is
/// treated as fatal: the socket itself is almost certainly broken.
const ACCEPT_FAILURE_LIMIT: u32 = 32;
const UNAUTHORIZED_BODY: &str = "{\"error\":\"unauthorized\"}";

#[derive(Clone, Debug)]
pub struct HttpBindConfig {
    pub binds: Vec<SocketAddr>,
    pub allowed_origins: Vec<String>,
    pub probe_min_interval: Duration,
    pub device_store_path: PathBuf,
}

pub struct HttpServer {
    listeners: Vec<TcpListener>,
    state: Arc<HttpState>,
}

#[derive(Debug)]
pub struct HttpBindError {
    message: String,
    io_kind: Option<std::io::ErrorKind>,
}

impl HttpBindError {
    fn message(message: impl Into<String>) -> Self {
        Self {
            message: message.into(),
            io_kind: None,
        }
    }

    fn io(message: impl Into<String>, error: &std::io::Error) -> Self {
        Self {
            message: message.into(),
            io_kind: Some(error.kind()),
        }
    }

    pub fn io_kind(&self) -> Option<std::io::ErrorKind> {
        self.io_kind
    }
}

impl std::fmt::Display for HttpBindError {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter.write_str(&self.message)
    }
}

impl std::error::Error for HttpBindError {}

impl From<HttpBindError> for String {
    fn from(error: HttpBindError) -> Self {
        error.message
    }
}

struct HttpState {
    service: ControlService,
    binds: Vec<SocketAddr>,
    /// Precomputed `Host` header whitelist; rebuilt once at bind so requests
    /// never allocate it.
    allowed_hosts: Vec<String>,
    allowed_origins: Vec<String>,
    probe_min_interval: Duration,
    last_probe: Mutex<HashMap<String, Instant>>,
    last_pair_attempt: Mutex<HashMap<IpAddr, Instant>>,
}

#[derive(Clone, Debug, PartialEq, Eq)]
enum Route {
    Pair,
    Status,
    Providers,
    Accounts,
    Account { id: String },
    Usage,
    Probe { id: String },
}

impl HttpServer {
    pub async fn bind(
        config: HttpBindConfig,
        service: ControlService,
    ) -> Result<Self, HttpBindError> {
        // The authoritative `*` rejection lives here so library callers that
        // skip the CLI layers stay safe; the config loader and
        // `http_bind_config` repeat the check to fail earlier for CLI users.
        if config.allowed_origins.iter().any(|origin| origin == "*") {
            return Err(HttpBindError::message(
                "http.allowed_origins must not contain *",
            ));
        }
        let requested = config.binds.into_iter().collect::<BTreeSet<_>>();
        let port = requested
            .first()
            .map(SocketAddr::port)
            .ok_or_else(|| HttpBindError::message("http.bind did not resolve to any address"))?;
        if requested.iter().any(|address| address.port() != port) {
            return Err(HttpBindError::message(
                "http.bind addresses must use the same port",
            ));
        }
        if port == 0 && requested.len() > 1 {
            return Err(HttpBindError::message(
                "http.bind port 0 requires a single address",
            ));
        }
        let requested = requested
            .into_iter()
            .map(|address| {
                classify_bind_address(address.ip())
                    .map(|class| (address, class))
                    .ok_or_else(|| HttpBindError::message(INVALID_BIND_MESSAGE))
            })
            .collect::<Result<Vec<_>, _>>()?;
        // `run_daemon_with` configures the store once up front; this call
        // covers direct library users and is idempotent for the same path, so
        // the explicit-bind retry loop can re-enter `bind` freely.
        service
            .configure_device_store(&config.device_store_path)
            .map_err(HttpBindError::message)?;

        let mut listeners = Vec::new();
        let mut binds = Vec::new();
        let mut last_error = None;
        for (requested_address, class) in requested {
            let listener = match TcpListener::bind(requested_address).await {
                Ok(listener) => listener,
                Err(error) if class != BindAddressClass::Loopback => {
                    eprintln!(
                        "warning: http.bind could not listen on {requested_address} ({class}); skipping: {error}"
                    );
                    last_error = Some(error);
                    continue;
                }
                Err(error) => {
                    return Err(HttpBindError::io(
                        format!("http.bind could not listen on {requested_address}: {error}"),
                        &error,
                    ));
                }
            };
            let address = listener.local_addr().map_err(|error| {
                HttpBindError::io(format!("http.bind address is unavailable: {error}"), &error)
            })?;
            eprintln!("http.bind listening {address} ({class})");
            binds.push(address);
            listeners.push(listener);
        }
        if listeners.is_empty() {
            return Err(match last_error {
                Some(error) => HttpBindError::io(
                    format!("http.bind could not listen on any address: {error}"),
                    &error,
                ),
                None => HttpBindError::message("http.bind did not resolve to any address"),
            });
        }
        let allowed_hosts = allowed_hosts(&binds);
        Ok(Self {
            listeners,
            state: Arc::new(HttpState {
                service,
                binds,
                allowed_hosts,
                allowed_origins: config.allowed_origins,
                probe_min_interval: config.probe_min_interval,
                last_probe: Mutex::new(HashMap::new()),
                last_pair_attempt: Mutex::new(HashMap::new()),
            }),
        })
    }

    pub fn local_addrs(&self) -> Vec<SocketAddr> {
        self.state.binds.clone()
    }

    pub async fn run(self) -> Result<(), String> {
        let mut tasks = tokio::task::JoinSet::new();
        for listener in self.listeners {
            tasks.spawn(run_listener(listener, self.state.clone()));
        }
        let result = match tasks.join_next().await {
            Some(Ok(result)) => result,
            Some(Err(_)) => Err("http listener task failed".to_owned()),
            None => Err("http.bind did not create a listener".to_owned()),
        };
        tasks.abort_all();
        while tasks.join_next().await.is_some() {}
        result
    }
}

async fn run_listener(listener: TcpListener, state: Arc<HttpState>) -> Result<(), String> {
    let connection_limit = Arc::new(Semaphore::new(MAXIMUM_CONNECTIONS));
    let mut connections = tokio::task::JoinSet::new();
    let mut consecutive_failures = 0u32;
    let result = loop {
        tokio::select! {
            accepted = listener.accept() => {
                let (stream, remote_address) = match accepted {
                    Ok(accepted) => {
                        consecutive_failures = 0;
                        accepted
                    }
                    Err(error) => match classify_accept_error(&error) {
                        // An aborted or network-failed pending connection
                        // says nothing about listener health: drop it without
                        // touching the fatal counter so a peer cannot RST the
                        // daemon offline.
                        AcceptFailure::Peer => {
                            tokio::task::yield_now().await;
                            continue;
                        }
                        AcceptFailure::Resource => {
                            consecutive_failures += 1;
                            if consecutive_failures >= ACCEPT_FAILURE_LIMIT {
                                break Err(format!(
                                    "http accept failed {consecutive_failures} consecutive times; last error: {error}"
                                ));
                            }
                            let shift = (consecutive_failures - 1).min(7);
                            let delay = (ACCEPT_BACKOFF_INITIAL * 2u32.pow(shift))
                                .min(ACCEPT_BACKOFF_MAXIMUM);
                            eprintln!(
                                "warning: http accept failed ({error}); retrying in {} seconds",
                                delay.as_secs_f64()
                            );
                            // The sleep stays select-able so shutdown is not
                            // held back by the transient-failure backoff.
                            tokio::select! {
                                _ = tokio::time::sleep(delay) => continue,
                                _ = state.service.wait_for_shutdown() => break Ok(()),
                            }
                        }
                        AcceptFailure::Fatal => {
                            break Err(format!("http accept failed permanently: {error}"));
                        }
                    },
                };
                let state = state.clone();
                // A saturated server closes the accepted socket at once: the
                // permit must cover the whole connection task, so it is moved
                // in rather than dropped when the handler finishes a request.
                let Ok(permit) = connection_limit.clone().try_acquire_owned() else {
                    drop(stream);
                    continue;
                };
                connections.spawn(async move {
                    let _permit = permit;
                    let io = TokioIo::new(stream);
                    let service = service_fn(move |request| {
                        let state = state.clone();
                        async move { Ok::<_, Infallible>(handle_request(state, remote_address.ip(), request).await) }
                    });
                    let _ = http1::Builder::new()
                        .timer(TokioTimer::new())
                        .header_read_timeout(READ_TIMEOUT)
                        .serve_connection(io, service)
                        .await;
                });
            }
            _ = state.service.wait_for_shutdown() => break Ok(()),
            _ = connections.join_next(), if !connections.is_empty() => {}
        }
    };
    // In-flight connections are aborted rather than drained: keep-alive
    // sessions have no natural end point, and engine-level work is already
    // awaited by `wait_for_idle` in the Unix control server that shares this
    // shutdown path (both tasks are awaited by `run_control_and_http`).
    connections.abort_all();
    while connections.join_next().await.is_some() {}
    result
}

/// How an `accept()` failure should be treated.
#[derive(Debug, PartialEq, Eq)]
enum AcceptFailure {
    /// The peer or the already-accepted pending connection caused the error:
    /// aborted handshakes, plus the pending network errors Linux passes
    /// through accept(2) on the new socket. Drop the connection; it is not
    /// evidence of listener damage and must not feed the fatal counter.
    Peer,
    /// The process or host is in a recoverable degraded state: fd-table or
    /// kernel buffer exhaustion, or a local network-subsystem failure
    /// (`WSAENETDOWN`). Back off and count toward the fatal threshold.
    Resource,
    /// The listener itself is broken (a closed or invalid socket).
    Fatal,
}

/// Errors a peer can trigger are `Peer`: `ECONNABORTED`, the pending-network
/// errno accept(2) tells Linux callers to retry like `EAGAIN`, and
/// `WSAECONNRESET`. `Resource` covers process-level exhaustion plus local
/// network-subsystem failure (`WSAENETDOWN`), which needs the bounded
/// backoff and failure threshold rather than a spin. Everything else (a
/// closed or invalid socket) is `Fatal`.
fn classify_accept_error(error: &std::io::Error) -> AcceptFailure {
    if error.kind() == std::io::ErrorKind::ConnectionAborted {
        return AcceptFailure::Peer;
    }
    #[cfg(unix)]
    {
        if matches!(
            error.raw_os_error(),
            Some(libc::EMFILE | libc::ENFILE | libc::ENOBUFS | libc::ENOMEM)
        ) {
            return AcceptFailure::Resource;
        }
        // These errno do not exist on every Unix (Apple has no ENONET), so
        // the Linux-only set stays under its own cfg.
        #[cfg(target_os = "linux")]
        {
            match error.raw_os_error() {
                Some(
                    libc::ENETDOWN
                        | libc::EPROTO
                        | libc::ENOPROTOOPT
                        | libc::EHOSTDOWN
                        | libc::ENONET
                        | libc::EHOSTUNREACH
                        | libc::EOPNOTSUPP
                        | libc::ENETUNREACH,
                ) => AcceptFailure::Peer,
                _ => AcceptFailure::Fatal,
            }
        }
        #[cfg(not(target_os = "linux"))]
        {
            AcceptFailure::Fatal
        }
    }
    #[cfg(windows)]
    {
        use windows_sys::Win32::Networking::WinSock;
        match error.raw_os_error() {
            Some(WinSock::WSAEMFILE | WinSock::WSAENOBUFS | WinSock::WSAENETDOWN) => {
                AcceptFailure::Resource
            }
            Some(WinSock::WSAECONNRESET) => AcceptFailure::Peer,
            _ => AcceptFailure::Fatal,
        }
    }
    #[cfg(not(any(unix, windows)))]
    {
        AcceptFailure::Fatal
    }
}

async fn handle_request(
    state: Arc<HttpState>,
    remote_ip: IpAddr,
    request: Request<Incoming>,
) -> Response<Full<Bytes>> {
    let method = request.method().clone();
    let path = request.uri().path().to_owned();
    let query = request.uri().query().unwrap_or_default().to_owned();
    let headers = request.headers().clone();
    let origin = header_str(&headers, &header::ORIGIN).map(str::to_owned);

    if !host_is_allowed(header_str(&headers, &header::HOST), &state.allowed_hosts) {
        return finish(
            json_status(
                StatusCode::FORBIDDEN,
                serde_json::json!({"error":"forbidden"}),
            ),
            origin.as_deref(),
            &state.allowed_origins,
        );
    }

    // The path is decoded once here; `Err` means bad percent-encoding or a
    // NUL byte. Preflight answers before auth like the rest of CORS, so it
    // reports the decode failure directly as 400; other methods keep the
    // existing order and see it after authentication and the body read.
    let matched = match_path(&path);

    if method == Method::OPTIONS {
        return match &matched {
            Ok(Some(_)) => finish_preflight(
                empty_status(StatusCode::NO_CONTENT),
                origin.as_deref(),
                &state.allowed_origins,
            ),
            Ok(None) => finish(
                json_status(
                    StatusCode::NOT_FOUND,
                    serde_json::json!({"error":"not_found"}),
                ),
                origin.as_deref(),
                &state.allowed_origins,
            ),
            Err(()) => finish(
                json_status(
                    StatusCode::BAD_REQUEST,
                    serde_json::json!({"error":"bad_request"}),
                ),
                origin.as_deref(),
                &state.allowed_origins,
            ),
        };
    }

    // Pairing answers before bearer auth because it exists to mint that
    // token; `matched` is reused so the path is never decoded twice.
    if matches!(matched, Ok(Some(Route::Pair))) {
        let response = if method == Method::POST {
            match QueryParams::parse(&query).and_then(|params| params.validate_for(&Route::Pair)) {
                Ok(()) => handle_pair_request(&state, remote_ip, &headers, request).await,
                Err(()) => json_status(
                    StatusCode::BAD_REQUEST,
                    serde_json::json!({"error":"bad_request"}),
                ),
            }
        } else {
            method_not_allowed(&Route::Pair)
        };
        return finish(response, origin.as_deref(), &state.allowed_origins);
    }

    let presented = bearer_token(header_str(&headers, &header::AUTHORIZATION));
    let has_token = presented.is_some();
    // `authenticate` can persist `last_seen_at` under the store mutex; keep
    // that blocking file I/O off the async executor.
    let service = state.service.clone();
    let token = presented.unwrap_or_default().to_owned();
    match tokio::task::spawn_blocking(move || service.authenticate_device(&token)).await {
        Ok(Ok(true)) if has_token => {}
        Ok(Ok(_)) => {
            return finish(unauthorized(), origin.as_deref(), &state.allowed_origins);
        }
        Ok(Err(_)) | Err(_) => {
            return finish(
                json_status(
                    StatusCode::INTERNAL_SERVER_ERROR,
                    serde_json::json!({"error":"storage"}),
                ),
                origin.as_deref(),
                &state.allowed_origins,
            );
        }
    }

    match collect_body(request, MAXIMUM_REQUEST_BYTES).await {
        Ok(_) => {}
        Err(status) => {
            return finish(
                request_body_error(status),
                origin.as_deref(),
                &state.allowed_origins,
            );
        }
    }

    let route = match matched {
        Ok(Some(route)) if method_allowed(&route, &method) => route,
        Ok(Some(route)) => {
            return finish(
                method_not_allowed(&route),
                origin.as_deref(),
                &state.allowed_origins,
            );
        }
        Ok(None) => {
            return finish(
                json_status(
                    StatusCode::NOT_FOUND,
                    serde_json::json!({"error":"not_found"}),
                ),
                origin.as_deref(),
                &state.allowed_origins,
            );
        }
        Err(()) => {
            return finish(
                json_status(
                    StatusCode::BAD_REQUEST,
                    serde_json::json!({"error":"bad_request"}),
                ),
                origin.as_deref(),
                &state.allowed_origins,
            );
        }
    };

    let params = match QueryParams::parse(&query).and_then(|params| {
        params.validate_for(&route)?;
        Ok(params)
    }) {
        Ok(params) => params,
        Err(()) => {
            return finish(
                json_status(
                    StatusCode::BAD_REQUEST,
                    serde_json::json!({"error":"bad_request"}),
                ),
                origin.as_deref(),
                &state.allowed_origins,
            );
        }
    };

    // Invalid metric names are their own 400 so a client can tell an illegal
    // filter from an unknown query key; a valid but unknown name is not an
    // error and filters to empty measurements.
    let metric_filter = match crate::metric::parse_metric_filter(&params.metric) {
        Ok(filter) => filter,
        Err(_) => {
            return finish(
                json_status(
                    StatusCode::BAD_REQUEST,
                    serde_json::json!({"error":"invalid_metric"}),
                ),
                origin.as_deref(),
                &state.allowed_origins,
            );
        }
    };

    if let Route::Probe { id } = &route {
        // Cooldown state is only spent on accounts that exist: recording
        // attempts for arbitrary names would grow the table unboundedly and
        // turn a 404 into a 429. The account is confirmed first, then the
        // check-and-record runs atomically under the cooldown lock, so two
        // concurrent probes for one account cannot both pass.
        if state
            .service
            .account_exists(&AccountId::new(id.as_str()))
            .await
        {
            if let Some(retry_after) = probe_retry_after(&state, id, Instant::now()) {
                return finish(
                    rate_limited(retry_after),
                    origin.as_deref(),
                    &state.allowed_origins,
                );
            }
        }
    }

    let command = route_command(route, &params);

    let request_id = next_request_id();
    let mut response = state
        .service
        .handle_with_transport(
            ControlRequest::new(request_id, command).with_diagnostics(params.diagnose),
            ullage_daemon::ControlTransport::RemoteHttp,
        )
        .await;
    if metric_filter.is_active() {
        if let ControlResult::Snapshots(snapshots) = &mut response.result {
            for snapshot in snapshots {
                crate::metric::filter_usage_outcome(&mut snapshot.usage, &metric_filter);
            }
        }
    }
    finish(
        map_control_response(response, params.diagnose),
        origin.as_deref(),
        &state.allowed_origins,
    )
}

async fn collect_body(
    request: Request<Incoming>,
    maximum_bytes: usize,
) -> Result<Bytes, StatusCode> {
    match tokio::time::timeout(
        READ_TIMEOUT,
        Limited::new(request.into_body(), maximum_bytes).collect(),
    )
    .await
    {
        Ok(Ok(body)) => Ok(body.to_bytes()),
        // Only the declared length cap is a 413; a truncated or malformed
        // stream is a client error, not an oversized payload.
        Ok(Err(error)) if error.is::<http_body_util::LengthLimitError>() => {
            Err(StatusCode::PAYLOAD_TOO_LARGE)
        }
        Ok(Err(_)) => Err(StatusCode::BAD_REQUEST),
        Err(_) => Err(StatusCode::REQUEST_TIMEOUT),
    }
}

#[derive(Deserialize)]
struct PairRequest {
    pair_code: String,
    device_name: String,
}

async fn handle_pair_request(
    state: &HttpState,
    remote_ip: IpAddr,
    headers: &hyper::HeaderMap,
    request: Request<Incoming>,
) -> Response<Full<Bytes>> {
    // The attempt is recorded before the request is validated on purpose: the
    // limiter throttles requests that reach the handler, so flooding with
    // malformed bodies still costs the sender its per-IP interval. Query
    // errors rejected in `handle_request` never reach here and stay free.
    if let Some(retry_after) = pair_retry_after(state, remote_ip, Instant::now()) {
        return rate_limited(retry_after);
    }
    if !content_type_is_json(header_str(headers, &header::CONTENT_TYPE)) {
        return json_status(
            StatusCode::BAD_REQUEST,
            serde_json::json!({"error":"bad_request"}),
        );
    }
    let body = match collect_body(request, MAXIMUM_PAIR_REQUEST_BYTES).await {
        Ok(body) => body,
        Err(status) => return request_body_error(status),
    };
    let request = match serde_json::from_slice::<PairRequest>(&body) {
        Ok(request) => request,
        Err(_) => {
            return json_status(
                StatusCode::BAD_REQUEST,
                serde_json::json!({"error":"bad_request"}),
            );
        }
    };
    // `pair` persists the new device under the store mutex; keep that
    // blocking file I/O off the async executor.
    let service = state.service.clone();
    let pairing = tokio::task::spawn_blocking(move || {
        service.pair_device(&request.pair_code, &request.device_name)
    })
    .await;
    match pairing.unwrap_or(Err(PairDeviceError::Storage("pairing task failed".into()))) {
        Ok(credential) => json_status(
            StatusCode::OK,
            serde_json::json!({
                "device_id": credential.device_id,
                "device_token": credential.device_token,
                "device_name": credential.device_name,
            }),
        ),
        Err(PairDeviceError::InvalidCode) => json_status(
            StatusCode::UNAUTHORIZED,
            serde_json::json!({"error":"pair_code_invalid"}),
        ),
        Err(PairDeviceError::InvalidName) => json_status(
            StatusCode::BAD_REQUEST,
            serde_json::json!({"error":"bad_request"}),
        ),
        Err(PairDeviceError::Storage(_)) => json_status(
            StatusCode::INTERNAL_SERVER_ERROR,
            serde_json::json!({"error":"storage"}),
        ),
    }
}

fn request_body_error(status: StatusCode) -> Response<Full<Bytes>> {
    let error = match status {
        StatusCode::PAYLOAD_TOO_LARGE => "payload_too_large",
        StatusCode::REQUEST_TIMEOUT => "request_timeout",
        _ => "bad_request",
    };
    json_status(status, serde_json::json!({"error": error}))
}

fn content_type_is_json(content_type: Option<&str>) -> bool {
    content_type
        .and_then(|value| value.split(';').next())
        .is_some_and(|value| value.trim().eq_ignore_ascii_case("application/json"))
}

fn route_command(route: Route, params: &QueryParams) -> ControlCommand {
    match route {
        Route::Pair => unreachable!("pairing is handled before control routing"),
        Route::Status => ControlCommand::DaemonStatus,
        Route::Providers => ControlCommand::ListProviders,
        Route::Accounts => ControlCommand::ListAccounts,
        Route::Account { id } => ControlCommand::ShowAccount {
            account: ullage_protocol::AccountId::new(id),
        },
        Route::Usage => ControlCommand::Show {
            account_id: params.account.clone(),
        },
        Route::Probe { id } => ControlCommand::Probe {
            account_id: id,
            wait: params.wait,
        },
    }
}

impl QueryParams {
    fn validate_for(&self, route: &Route) -> Result<(), ()> {
        let allowed = match route {
            Route::Usage => ["account", "diagnose", "metric"].as_slice(),
            Route::Probe { .. } => ["wait", "diagnose"].as_slice(),
            Route::Pair => [].as_slice(),
            _ => ["diagnose"].as_slice(),
        };
        if self.keys.iter().all(|key| allowed.contains(&key.as_str())) {
            Ok(())
        } else {
            Err(())
        }
    }
}

/// Decodes the path once and maps it to a route. `Ok(None)` is a legal path
/// that names no route (404); `Err` is a decode failure (400). Method checks
/// live in [`method_allowed`] so the split never decodes a segment twice.
fn match_path(path: &str) -> Result<Option<Route>, ()> {
    let mut segments = Vec::new();
    for segment in path.split('/') {
        if segment.is_empty() {
            continue;
        }
        segments.push(decode_component(segment)?);
    }
    // `split('/')` drops no characters, and empty segments are skipped above,
    // so a matched `id` segment is never empty.
    let matched = match segments
        .iter()
        .map(String::as_str)
        .collect::<Vec<_>>()
        .as_slice()
    {
        ["v1", "pair"] => Some(Route::Pair),
        ["v1", "status"] => Some(Route::Status),
        ["v1", "providers"] => Some(Route::Providers),
        ["v1", "accounts"] => Some(Route::Accounts),
        ["v1", "accounts", id] => Some(Route::Account {
            id: (*id).to_owned(),
        }),
        ["v1", "usage"] => Some(Route::Usage),
        ["v1", "accounts", id, "probe"] => Some(Route::Probe {
            id: (*id).to_owned(),
        }),
        _ => None,
    };
    Ok(matched)
}

fn method_allowed(route: &Route, method: &Method) -> bool {
    match route {
        Route::Pair | Route::Probe { .. } => *method == Method::POST,
        _ => *method == Method::GET,
    }
}

fn allowed_methods(route: &Route) -> &'static str {
    match route {
        Route::Pair | Route::Probe { .. } => "POST, OPTIONS",
        _ => "GET, OPTIONS",
    }
}

fn method_not_allowed(route: &Route) -> Response<Full<Bytes>> {
    let mut response = json_status(
        StatusCode::METHOD_NOT_ALLOWED,
        serde_json::json!({"error":"method_not_allowed"}),
    );
    set_header(
        &mut response,
        header::ALLOW,
        HeaderValue::from_static(allowed_methods(route)),
    );
    response
}

/// Builds the `Host` whitelist once at bind time. A `Host` without an
/// explicit port (`localhost` alone) is deliberately not matched: the port
/// binds the origin check to this server instance.
fn allowed_hosts(binds: &[SocketAddr]) -> Vec<String> {
    let Some(port) = binds.first().map(SocketAddr::port) else {
        return Vec::new();
    };
    let mut allowed = vec![format!("127.0.0.1:{port}"), format!("localhost:{port}")];
    allowed.extend(
        binds
            .iter()
            .map(|bind| bind.to_string().to_ascii_lowercase()),
    );
    if binds.iter().any(SocketAddr::is_ipv6) {
        allowed.push(format!("[::1]:{port}"));
    }
    allowed
}

fn host_is_allowed(host: Option<&str>, allowed_hosts: &[String]) -> bool {
    let Some(host) = host else {
        return false;
    };
    let normalized = host.trim().to_ascii_lowercase();
    allowed_hosts.iter().any(|candidate| candidate == &normalized)
}

fn bearer_token(authorization: Option<&str>) -> Option<&str> {
    authorization?
        .strip_prefix("Bearer ")
        .filter(|token| !token.is_empty())
}

fn pair_retry_after(state: &HttpState, remote_ip: IpAddr, now: Instant) -> Option<u64> {
    let mut attempts = state
        .last_pair_attempt
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner);
    attempts.retain(|_, attempted| now.saturating_duration_since(*attempted) < PAIR_MIN_INTERVAL);
    if let Some(previous) = attempts.get(&remote_ip) {
        let remaining = PAIR_MIN_INTERVAL - now.saturating_duration_since(*previous);
        return Some(remaining.as_secs().max(1));
    }
    if attempts.len() >= PAIR_RATE_LIMIT_CAPACITY {
        return Some(PAIR_MIN_INTERVAL.as_secs().max(1));
    }
    attempts.insert(remote_ip, now);
    None
}

fn probe_retry_after(state: &HttpState, account_id: &str, now: Instant) -> Option<u64> {
    if state.probe_min_interval.is_zero() {
        return None;
    }
    let mut last_probe = state
        .last_probe
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner);
    last_probe
        .retain(|_, probed| now.saturating_duration_since(*probed) < state.probe_min_interval);
    if let Some(previous) = last_probe.get(account_id) {
        let remaining = state.probe_min_interval - now.saturating_duration_since(*previous);
        return Some(remaining.as_secs().max(1));
    }
    if last_probe.len() >= PROBE_RATE_LIMIT_CAPACITY {
        return Some(state.probe_min_interval.as_secs().max(1));
    }
    last_probe.insert(account_id.to_owned(), now);
    None
}

fn map_control_response(response: ControlResponse, diagnose: bool) -> Response<Full<Bytes>> {
    match response.result {
        ControlResult::Error(error) => map_control_error(
            error,
            response.diagnostic.filter(|_| diagnose),
            response.version,
            response.request_id,
        ),
        ControlResult::ProtocolMismatch { supported_version } => json_status(
            StatusCode::BAD_REQUEST,
            serde_json::json!({
                "version": response.version,
                "request_id": response.request_id,
                "error": "protocol_mismatch",
                "supported_version": supported_version,
            }),
        ),
        result => json_status(
            success_status(&result),
            response_document(response.version, response.request_id, result),
        ),
    }
}

fn success_status(result: &ControlResult) -> StatusCode {
    match result {
        ControlResult::Ack => StatusCode::ACCEPTED,
        _ => StatusCode::OK,
    }
}

fn map_control_error(
    error: ControlError,
    diagnostic: Option<String>,
    version: u16,
    request_id: String,
) -> Response<Full<Bytes>> {
    let (status, retry_after) = control_error_status(&error);
    let mut response = json_error_payload(status, error, diagnostic, version, request_id);
    if status == StatusCode::TOO_MANY_REQUESTS {
        set_header(
            &mut response,
            header::RETRY_AFTER,
            HeaderValue::from_str(&retry_after.to_string())
                .unwrap_or(HeaderValue::from_static("1")),
        );
    }
    response
}

fn control_error_status(error: &ControlError) -> (StatusCode, u64) {
    match error {
        ControlError::AccountNotFound { .. }
        | ControlError::AccountSelectorNotFound { .. }
        | ControlError::Account(ullage_protocol::AccountError::NotFound(_))
        | ControlError::Registry(ullage_protocol::RegistryError::NotFound(_)) => {
            (StatusCode::NOT_FOUND, 0)
        }
        ControlError::Provider(ProviderError::AuthenticationInvalid { .. }) => {
            (StatusCode::CONFLICT, 0)
        }
        ControlError::Provider(ProviderError::RateLimited {
            retry_after_seconds,
            ..
        }) => (
            StatusCode::TOO_MANY_REQUESTS,
            retry_after_seconds.unwrap_or(1),
        ),
        ControlError::Timeout => (StatusCode::GATEWAY_TIMEOUT, 0),
        ControlError::Storage => (StatusCode::INTERNAL_SERVER_ERROR, 0),
        ControlError::UnsupportedCommand => (StatusCode::BAD_REQUEST, 0),
        _ => (StatusCode::INTERNAL_SERVER_ERROR, 0),
    }
}

#[derive(Serialize)]
struct ErrorDocument<T: Serialize> {
    version: u16,
    request_id: String,
    #[serde(flatten)]
    error: T,
    #[serde(skip_serializing_if = "Option::is_none")]
    diagnostic: Option<String>,
}

fn json_error_payload(
    status: StatusCode,
    error: ControlError,
    diagnostic: Option<String>,
    version: u16,
    request_id: String,
) -> Response<Full<Bytes>> {
    json_status(
        status,
        ErrorDocument {
            version,
            request_id,
            error,
            diagnostic,
        },
    )
}

fn response_document(
    version: u16,
    request_id: String,
    value: impl Serialize,
) -> serde_json::Value {
    let mut encoded =
        serde_json::to_value(value).unwrap_or_else(|_| serde_json::json!({"error":"storage"}));
    if let serde_json::Value::Object(map) = &mut encoded {
        map.insert("version".into(), serde_json::json!(version));
        map.insert("request_id".into(), serde_json::json!(request_id));
    }
    encoded
}

fn json_status(status: StatusCode, value: impl Serialize) -> Response<Full<Bytes>> {
    let bytes = serde_json::to_vec(&value).unwrap_or_else(|_| b"{\"error\":\"storage\"}".to_vec());
    let mut response = Response::new(Full::new(Bytes::from(bytes)));
    *response.status_mut() = status;
    set_header(
        &mut response,
        header::CONTENT_TYPE,
        HeaderValue::from_static("application/json"),
    );
    apply_security_headers(&mut response);
    response
}

fn empty_status(status: StatusCode) -> Response<Full<Bytes>> {
    let mut response = Response::new(Full::new(Bytes::new()));
    *response.status_mut() = status;
    apply_security_headers(&mut response);
    response
}

fn unauthorized() -> Response<Full<Bytes>> {
    let mut response = Response::new(Full::new(Bytes::from_static(UNAUTHORIZED_BODY.as_bytes())));
    *response.status_mut() = StatusCode::UNAUTHORIZED;
    set_header(
        &mut response,
        header::CONTENT_TYPE,
        HeaderValue::from_static("application/json"),
    );
    set_header(
        &mut response,
        header::WWW_AUTHENTICATE,
        HeaderValue::from_static("Bearer"),
    );
    apply_security_headers(&mut response);
    response
}

fn rate_limited(retry_after: u64) -> Response<Full<Bytes>> {
    let mut response = json_status(
        StatusCode::TOO_MANY_REQUESTS,
        serde_json::json!({"error":"rate_limited"}),
    );
    set_header(
        &mut response,
        header::RETRY_AFTER,
        HeaderValue::from_str(&retry_after.to_string()).unwrap_or(HeaderValue::from_static("1")),
    );
    response
}

fn finish(
    response: Response<Full<Bytes>>,
    origin: Option<&str>,
    allowed_origins: &[String],
) -> Response<Full<Bytes>> {
    finish_with_cors(response, origin, allowed_origins, false)
}

fn finish_preflight(
    response: Response<Full<Bytes>>,
    origin: Option<&str>,
    allowed_origins: &[String],
) -> Response<Full<Bytes>> {
    finish_with_cors(response, origin, allowed_origins, true)
}

fn finish_with_cors(
    mut response: Response<Full<Bytes>>,
    origin: Option<&str>,
    allowed_origins: &[String],
    preflight: bool,
) -> Response<Full<Bytes>> {
    apply_cors(&mut response, origin, allowed_origins, preflight);
    response
}

/// `Access-Control-Allow-Methods`/`Headers` answer the preflight question and
/// are only meaningful there; ordinary responses get just the echoed origin
/// and the `Vary` marker.
fn apply_cors(
    response: &mut Response<Full<Bytes>>,
    origin: Option<&str>,
    allowed_origins: &[String],
    preflight: bool,
) {
    let Some(origin) = origin else {
        return;
    };
    if origin == "*"
        || !allowed_origins
            .iter()
            .any(|allowed| allowed != "*" && allowed == origin)
    {
        return;
    }
    if let Ok(value) = HeaderValue::from_str(origin) {
        set_header(response, header::ACCESS_CONTROL_ALLOW_ORIGIN, value);
        set_header(response, header::VARY, HeaderValue::from_static("Origin"));
        if preflight {
            set_header(
                response,
                header::ACCESS_CONTROL_ALLOW_METHODS,
                HeaderValue::from_static("GET, POST, OPTIONS"),
            );
            set_header(
                response,
                header::ACCESS_CONTROL_ALLOW_HEADERS,
                HeaderValue::from_static("Authorization, Content-Type"),
            );
        }
    }
}

fn apply_security_headers(response: &mut Response<Full<Bytes>>) {
    set_header(
        response,
        header::CACHE_CONTROL,
        HeaderValue::from_static("no-store"),
    );
    set_header(
        response,
        HeaderName::from_static("x-content-type-options"),
        HeaderValue::from_static("nosniff"),
    );
    set_header(
        response,
        header::REFERRER_POLICY,
        HeaderValue::from_static("no-referrer"),
    );
}

fn set_header(response: &mut Response<Full<Bytes>>, name: HeaderName, value: HeaderValue) {
    response.headers_mut().insert(name, value);
}

fn header_str<'a>(headers: &'a hyper::HeaderMap, name: &HeaderName) -> Option<&'a str> {
    headers.get(name).and_then(|value| value.to_str().ok())
}

fn next_request_id() -> String {
    use std::sync::atomic::{AtomicU64, Ordering};
    static SEQUENCE: AtomicU64 = AtomicU64::new(1);
    format!("http-{}", SEQUENCE.fetch_add(1, Ordering::Relaxed))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn host_whitelist_accepts_loopback_spellings() {
        let bind = "127.0.0.1:7878".parse().unwrap();
        let allowed = allowed_hosts(&[bind]);
        assert!(host_is_allowed(Some("127.0.0.1:7878"), &allowed));
        assert!(host_is_allowed(Some("localhost:7878"), &allowed));
        assert!(host_is_allowed(Some("LOCALHOST:7878"), &allowed));
        assert!(!host_is_allowed(Some("example.com"), &allowed));
        assert!(!host_is_allowed(Some("127.0.0.1"), &allowed));
        assert!(!host_is_allowed(Some("localhost"), &allowed));
        assert!(!host_is_allowed(None, &allowed));
    }

    #[test]
    fn host_whitelist_accepts_every_bound_address() {
        let binds = [
            "127.0.0.1:7878".parse().unwrap(),
            "100.64.0.1:7878".parse().unwrap(),
            "192.168.50.10:7878".parse().unwrap(),
        ];
        let allowed = allowed_hosts(&binds);
        assert!(host_is_allowed(Some("100.64.0.1:7878"), &allowed));
        assert!(host_is_allowed(Some("192.168.50.10:7878"), &allowed));
        assert!(!host_is_allowed(Some("100.64.0.2:7878"), &allowed));
        assert!(!host_is_allowed(Some("evil.example:7878"), &allowed));
    }

    #[test]
    fn bearer_parser_requires_the_exact_scheme_and_a_value() {
        assert_eq!(
            bearer_token(Some("Bearer secret-token-value")),
            Some("secret-token-value")
        );
        assert_eq!(bearer_token(None), None);
        assert_eq!(bearer_token(Some("Basic secret-token-value")), None);
        assert_eq!(bearer_token(Some("bearer secret-token-value")), None);
        assert_eq!(bearer_token(Some("Bearer ")), None);
    }

    fn test_state(probe_min_interval: Duration) -> HttpState {
        let engine = tokio::runtime::Runtime::new().unwrap().block_on(async {
            ullage_daemon::DaemonEngine::new(
                ullage_daemon::DaemonConfig::default(),
                Arc::new(ullage_core::ProviderRegistry::default()),
                Arc::new(ullage_daemon::SystemClock),
                Arc::new(ullage_daemon::MemorySnapshotStore::default()),
            )
            .await
            .unwrap()
        });
        let binds = vec!["127.0.0.1:7878".parse().unwrap()];
        HttpState {
            service: ControlService::new(engine),
            allowed_hosts: allowed_hosts(&binds),
            binds,
            allowed_origins: Vec::new(),
            probe_min_interval,
            last_probe: Mutex::new(HashMap::new()),
            last_pair_attempt: Mutex::new(HashMap::new()),
        }
    }

    #[test]
    fn probe_limiter_is_per_account_bounded_and_expires() {
        let state = test_state(Duration::from_secs(60));
        let now = Instant::now();
        assert_eq!(probe_retry_after(&state, "alpha", now), None);
        assert!(probe_retry_after(&state, "alpha", now).is_some());
        assert_eq!(
            probe_retry_after(&state, "beta", now),
            None,
            "cooldown must not leak across accounts"
        );
        let later = now + Duration::from_secs(61);
        assert_eq!(
            probe_retry_after(&state, "alpha", later),
            None,
            "an expired entry frees its slot"
        );
    }

    #[test]
    fn probe_limiter_rejects_overflow_without_evicting_active_accounts() {
        let state = test_state(Duration::from_secs(60));
        let now = Instant::now();
        for index in 0..PROBE_RATE_LIMIT_CAPACITY {
            assert_eq!(probe_retry_after(&state, &format!("a{index}"), now), None);
        }
        assert_eq!(probe_retry_after(&state, "overflow", now), Some(60));
        assert_eq!(probe_retry_after(&state, "a0", now), Some(60));
        assert_eq!(
            state.last_probe.lock().unwrap().len(),
            PROBE_RATE_LIMIT_CAPACITY
        );
    }

    #[test]
    fn accept_errors_split_peer_resource_and_fatal() {
        assert_eq!(
            classify_accept_error(&std::io::Error::from(
                std::io::ErrorKind::ConnectionAborted
            )),
            AcceptFailure::Peer
        );
        #[cfg(unix)]
        {
            for code in [libc::EMFILE, libc::ENFILE, libc::ENOBUFS, libc::ENOMEM] {
                assert_eq!(
                    classify_accept_error(&std::io::Error::from_raw_os_error(code)),
                    AcceptFailure::Resource,
                    "errno {code}"
                );
            }
            #[cfg(target_os = "linux")]
            for code in [
                libc::ENETDOWN,
                libc::EPROTO,
                libc::ENOPROTOOPT,
                libc::EHOSTDOWN,
                libc::ENONET,
                libc::EHOSTUNREACH,
                libc::EOPNOTSUPP,
                libc::ENETUNREACH,
            ] {
                assert_eq!(
                    classify_accept_error(&std::io::Error::from_raw_os_error(code)),
                    AcceptFailure::Peer,
                    "errno {code}"
                );
            }
            assert_eq!(
                classify_accept_error(&std::io::Error::from_raw_os_error(libc::EINVAL)),
                AcceptFailure::Fatal
            );
        }
        #[cfg(windows)]
        {
            use windows_sys::Win32::Networking::WinSock;
            for code in [
                WinSock::WSAEMFILE,
                WinSock::WSAENOBUFS,
                WinSock::WSAENETDOWN,
            ] {
                assert_eq!(
                    classify_accept_error(&std::io::Error::from_raw_os_error(code)),
                    AcceptFailure::Resource,
                    "errno {code}"
                );
            }
            assert_eq!(
                classify_accept_error(&std::io::Error::from_raw_os_error(
                    WinSock::WSAECONNRESET
                )),
                AcceptFailure::Peer
            );
            assert_eq!(
                classify_accept_error(&std::io::Error::from_raw_os_error(
                    WinSock::WSAEINVAL
                )),
                AcceptFailure::Fatal
            );
        }
        assert_eq!(
            classify_accept_error(&std::io::Error::from(std::io::ErrorKind::PermissionDenied)),
            AcceptFailure::Fatal
        );
    }

    #[test]
    fn json_content_type_matches_case_and_parameters() {
        assert!(content_type_is_json(Some("application/json")));
        assert!(content_type_is_json(Some("APPLICATION/JSON")));
        assert!(content_type_is_json(Some("application/JSON; charset=utf-8")));
        assert!(!content_type_is_json(Some("text/json")));
        assert!(!content_type_is_json(Some("application/jsonx")));
        assert!(!content_type_is_json(None));
    }

    #[test]
    fn pair_rate_limiter_rejects_overflow_without_evicting_active_ips() {
        let state = test_state(Duration::ZERO);
        let now = Instant::now();
        let oldest = IpAddr::V6(std::net::Ipv6Addr::from(0_u128));
        for suffix in 0..PAIR_RATE_LIMIT_CAPACITY {
            let address = IpAddr::V6(std::net::Ipv6Addr::from(suffix as u128));
            assert_eq!(pair_retry_after(&state, address, now), None);
        }
        let overflow = IpAddr::V6(std::net::Ipv6Addr::from(PAIR_RATE_LIMIT_CAPACITY as u128));
        assert_eq!(pair_retry_after(&state, overflow, now), Some(1));
        assert_eq!(pair_retry_after(&state, oldest, now), Some(1));
        assert_eq!(
            state.last_pair_attempt.lock().unwrap().len(),
            PAIR_RATE_LIMIT_CAPACITY
        );
    }

    #[test]
    fn control_error_mapping_matches_the_http_contract() {
        assert_eq!(
            control_error_status(&ControlError::AccountNotFound {
                account_id: "missing".into()
            })
            .0,
            StatusCode::NOT_FOUND
        );
        assert_eq!(
            control_error_status(&ControlError::Provider(
                ProviderError::AuthenticationInvalid {
                    message: "invalid".into()
                }
            ))
            .0,
            StatusCode::CONFLICT
        );
        assert_eq!(
            control_error_status(&ControlError::Provider(ProviderError::RateLimited {
                message: "slow".into(),
                retry_after_seconds: Some(12),
            }))
            .1,
            12
        );
        assert_eq!(
            control_error_status(&ControlError::Provider(ProviderError::RateLimited {
                message: "slow".into(),
                retry_after_seconds: None,
            }))
            .1,
            1
        );
        assert_eq!(
            control_error_status(&ControlError::Timeout).0,
            StatusCode::GATEWAY_TIMEOUT
        );
        assert_eq!(
            control_error_status(&ControlError::Storage).0,
            StatusCode::INTERNAL_SERVER_ERROR
        );
        assert_eq!(
            control_error_status(&ControlError::UnsupportedCommand).0,
            StatusCode::BAD_REQUEST
        );
    }
}
