use std::collections::{BTreeSet, HashMap};
use std::convert::Infallible;
use std::net::SocketAddr;
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
use percent_encoding::percent_decode_str;
use serde::Serialize;
use tokio::net::TcpListener;
use ullage_daemon::ControlService;
use ullage_protocol::{
    ControlCommand, ControlError, ControlRequest, ControlResponse, ControlResult, ProviderError,
};

use crate::bind::INVALID_BIND_MESSAGE;
use crate::token::{constant_time_eq, load_or_create_token, load_token};
use crate::{BindAddressClass, classify_bind_address};

const MAXIMUM_REQUEST_BYTES: usize = 1024 * 1024;
const READ_TIMEOUT: Duration = Duration::from_secs(10);
const UNAUTHORIZED_BODY: &str = "{\"error\":\"unauthorized\"}";

#[derive(Clone, Debug)]
pub struct HttpBindConfig {
    pub binds: Vec<SocketAddr>,
    pub allowed_origins: Vec<String>,
    pub probe_min_interval: Duration,
    pub token_path: PathBuf,
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
    allowed_origins: Vec<String>,
    probe_min_interval: Duration,
    token_path: PathBuf,
    last_probe: Mutex<HashMap<String, Instant>>,
}

#[derive(Clone, Debug, PartialEq, Eq)]
enum Route {
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
        load_or_create_token(&config.token_path).map_err(HttpBindError::message)?;

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
        Ok(Self {
            listeners,
            state: Arc::new(HttpState {
                service,
                binds,
                allowed_origins: config.allowed_origins,
                probe_min_interval: config.probe_min_interval,
                token_path: config.token_path,
                last_probe: Mutex::new(HashMap::new()),
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
    let mut connections = tokio::task::JoinSet::new();
    let result = loop {
        tokio::select! {
            accepted = listener.accept() => {
                let (stream, _) = match accepted {
                    Ok(accepted) => accepted,
                    Err(error) => break Err(error.to_string()),
                };
                let state = state.clone();
                connections.spawn(async move {
                    let io = TokioIo::new(stream);
                    let service = service_fn(move |request| {
                        let state = state.clone();
                        async move { handle_connection(state, request).await }
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
    connections.abort_all();
    while connections.join_next().await.is_some() {}
    result
}

async fn handle_connection(
    state: Arc<HttpState>,
    request: Request<Incoming>,
) -> Result<Response<Full<Bytes>>, Infallible> {
    Ok(handle_request(state, request).await)
}

async fn handle_request(
    state: Arc<HttpState>,
    request: Request<Incoming>,
) -> Response<Full<Bytes>> {
    let method = request.method().clone();
    let path = request.uri().path().to_owned();
    let query = request.uri().query().unwrap_or_default().to_owned();
    let headers = request.headers().clone();
    let origin = header_str(&headers, &header::ORIGIN).map(str::to_owned);

    if !host_is_allowed(header_str(&headers, &header::HOST), &state.binds) {
        return finish(
            json_status(
                StatusCode::FORBIDDEN,
                serde_json::json!({"error":"forbidden"}),
            ),
            origin.as_deref(),
            &state.allowed_origins,
        );
    }

    if method == Method::OPTIONS {
        return match parse_route(&method, &path) {
            Ok(_) | Err(RouteError::MethodNotAllowed) => finish(
                empty_status(StatusCode::NO_CONTENT),
                origin.as_deref(),
                &state.allowed_origins,
            ),
            Err(RouteError::NotFound) | Err(RouteError::BadRequest) => finish(
                json_status(
                    StatusCode::NOT_FOUND,
                    serde_json::json!({"error":"not_found"}),
                ),
                origin.as_deref(),
                &state.allowed_origins,
            ),
        };
    }

    let expected = match load_token(&state.token_path) {
        Ok(token) => token,
        Err(_) => {
            return finish(
                json_status(
                    StatusCode::INTERNAL_SERVER_ERROR,
                    serde_json::json!({"error":"storage"}),
                ),
                origin.as_deref(),
                &state.allowed_origins,
            );
        }
    };
    if !bearer_matches(header_str(&headers, &header::AUTHORIZATION), &expected) {
        return finish(unauthorized(), origin.as_deref(), &state.allowed_origins);
    }

    match collect_body(request).await {
        Ok(()) => {}
        Err(status) => {
            let error = if status == StatusCode::PAYLOAD_TOO_LARGE {
                "payload_too_large"
            } else {
                "request_timeout"
            };
            return finish(
                json_status(status, serde_json::json!({"error": error})),
                origin.as_deref(),
                &state.allowed_origins,
            );
        }
    }

    let route = match parse_route(&method, &path) {
        Ok(route) => route,
        Err(RouteError::NotFound) => {
            return finish(
                json_status(
                    StatusCode::NOT_FOUND,
                    serde_json::json!({"error":"not_found"}),
                ),
                origin.as_deref(),
                &state.allowed_origins,
            );
        }
        Err(RouteError::MethodNotAllowed) => {
            return finish(
                json_status(
                    StatusCode::METHOD_NOT_ALLOWED,
                    serde_json::json!({"error":"method_not_allowed"}),
                ),
                origin.as_deref(),
                &state.allowed_origins,
            );
        }
        Err(RouteError::BadRequest) => {
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

    if let Route::Probe { id } = &route
        && let Some(retry_after) = probe_retry_after(&state, id)
    {
        return finish(
            rate_limited(retry_after),
            origin.as_deref(),
            &state.allowed_origins,
        );
    }

    let command = route_command(route, &params);

    let request_id = next_request_id();
    let response = state
        .service
        .handle(ControlRequest::new(request_id, command).with_diagnostics(params.diagnose))
        .await;
    finish(
        map_control_response(response, params.diagnose),
        origin.as_deref(),
        &state.allowed_origins,
    )
}

async fn collect_body(request: Request<Incoming>) -> Result<(), StatusCode> {
    match tokio::time::timeout(
        READ_TIMEOUT,
        Limited::new(request.into_body(), MAXIMUM_REQUEST_BYTES).collect(),
    )
    .await
    {
        Ok(Ok(_)) => Ok(()),
        Ok(Err(_)) => Err(StatusCode::PAYLOAD_TOO_LARGE),
        Err(_) => Err(StatusCode::REQUEST_TIMEOUT),
    }
}

fn route_command(route: Route, params: &QueryParams) -> ControlCommand {
    match route {
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

#[derive(Clone, Debug)]
struct QueryParams {
    account: Option<String>,
    wait: bool,
    diagnose: bool,
    keys: Vec<String>,
}

impl QueryParams {
    fn parse(query: &str) -> Result<Self, ()> {
        let mut account = None;
        let mut wait = None;
        let mut diagnose = None;
        let mut keys = Vec::new();
        if query.is_empty() {
            return Ok(Self {
                account: None,
                wait: true,
                diagnose: false,
                keys,
            });
        }
        for pair in query.split('&') {
            let (key, value) = pair.split_once('=').ok_or(())?;
            let key = decode_component(key)?;
            let value = decode_component(value)?;
            match key.as_str() {
                "account" => {
                    if account.is_some() || value.is_empty() {
                        return Err(());
                    }
                    account = Some(value);
                }
                "wait" => {
                    if wait.is_some() {
                        return Err(());
                    }
                    wait = Some(match value.as_str() {
                        "true" => true,
                        "false" => false,
                        _ => return Err(()),
                    });
                }
                "diagnose" => {
                    if diagnose.is_some() {
                        return Err(());
                    }
                    diagnose = Some(match value.as_str() {
                        "1" => true,
                        "0" => false,
                        _ => return Err(()),
                    });
                }
                _ => return Err(()),
            }
            keys.push(key);
        }
        Ok(Self {
            account,
            wait: wait.unwrap_or(true),
            diagnose: diagnose.unwrap_or(false),
            keys,
        })
    }

    fn validate_for(&self, route: &Route) -> Result<(), ()> {
        let allowed = match route {
            Route::Usage => ["account", "diagnose"].as_slice(),
            Route::Probe { .. } => ["wait", "diagnose"].as_slice(),
            _ => ["diagnose"].as_slice(),
        };
        if self.keys.iter().all(|key| allowed.contains(&key.as_str())) {
            Ok(())
        } else {
            Err(())
        }
    }
}

enum RouteError {
    NotFound,
    MethodNotAllowed,
    BadRequest,
}

fn parse_route(method: &Method, path: &str) -> Result<Route, RouteError> {
    let mut segments = Vec::new();
    for segment in path.split('/') {
        if segment.is_empty() {
            continue;
        }
        segments.push(decode_component(segment).map_err(|()| RouteError::BadRequest)?);
    }
    let matched = match segments
        .iter()
        .map(String::as_str)
        .collect::<Vec<_>>()
        .as_slice()
    {
        ["v1", "status"] => Some(Route::Status),
        ["v1", "providers"] => Some(Route::Providers),
        ["v1", "accounts"] => Some(Route::Accounts),
        ["v1", "accounts", id] if !id.is_empty() => Some(Route::Account {
            id: (*id).to_owned(),
        }),
        ["v1", "usage"] => Some(Route::Usage),
        ["v1", "accounts", id, "probe"] if !id.is_empty() => Some(Route::Probe {
            id: (*id).to_owned(),
        }),
        _ => None,
    };
    match (method, matched) {
        (_, None) => Err(RouteError::NotFound),
        (&Method::OPTIONS, Some(_)) => Ok(Route::Status),
        (&Method::GET, Some(Route::Probe { .. })) | (&Method::POST, Some(Route::Status)) => {
            Err(RouteError::MethodNotAllowed)
        }
        (&Method::GET, Some(route)) if !matches!(route, Route::Probe { .. }) => Ok(route),
        (&Method::POST, Some(route @ Route::Probe { .. })) => Ok(route),
        (_, Some(_)) => Err(RouteError::MethodNotAllowed),
    }
}

fn decode_component(value: &str) -> Result<String, ()> {
    let decoded = percent_decode_str(value).decode_utf8().map_err(|_| ())?;
    if decoded.contains('\0') {
        return Err(());
    }
    Ok(decoded.into_owned())
}

fn host_is_allowed(host: Option<&str>, binds: &[SocketAddr]) -> bool {
    let Some(host) = host else {
        return false;
    };
    let Some(port) = binds.first().map(SocketAddr::port) else {
        return false;
    };
    let normalized = host.trim().to_ascii_lowercase();
    let mut allowed = vec![format!("127.0.0.1:{port}"), format!("localhost:{port}")];
    allowed.extend(
        binds
            .iter()
            .map(|bind| bind.to_string().to_ascii_lowercase()),
    );
    if binds.iter().any(SocketAddr::is_ipv6) {
        allowed.push(format!("[::1]:{port}"));
    }
    allowed.iter().any(|candidate| candidate == &normalized)
}

fn bearer_matches(authorization: Option<&str>, expected: &str) -> bool {
    let Some(authorization) = authorization else {
        return constant_time_eq(expected.as_bytes(), b"");
    };
    let Some(presented) = authorization.strip_prefix("Bearer ") else {
        return constant_time_eq(expected.as_bytes(), b"");
    };
    constant_time_eq(presented.as_bytes(), expected.as_bytes())
}

fn probe_retry_after(state: &HttpState, account_id: &str) -> Option<u64> {
    if state.probe_min_interval.is_zero() {
        return None;
    }
    let mut last_probe = state
        .last_probe
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner);
    let now = Instant::now();
    if let Some(previous) = last_probe.get(account_id)
        && now.saturating_duration_since(*previous) < state.probe_min_interval
    {
        let remaining = state.probe_min_interval - now.saturating_duration_since(*previous);
        return Some(remaining.as_secs().max(1));
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
        ),
        ControlResult::ProtocolMismatch { supported_version } => json_status(
            StatusCode::BAD_REQUEST,
            serde_json::json!({
                "version": response.version,
                "error": "protocol_mismatch",
                "supported_version": supported_version,
            }),
        ),
        result => json_status(
            success_status(&result),
            with_protocol_version(response.version, result),
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
) -> Response<Full<Bytes>> {
    let (status, retry_after) = control_error_status(&error);
    let mut response = json_error_payload(status, error, diagnostic, version);
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
) -> Response<Full<Bytes>> {
    json_status(
        status,
        ErrorDocument {
            version,
            error,
            diagnostic,
        },
    )
}

fn with_protocol_version(version: u16, value: impl Serialize) -> serde_json::Value {
    let mut encoded =
        serde_json::to_value(value).unwrap_or_else(|_| serde_json::json!({"error":"storage"}));
    if let serde_json::Value::Object(map) = &mut encoded {
        map.insert("version".into(), serde_json::json!(version));
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
    mut response: Response<Full<Bytes>>,
    origin: Option<&str>,
    allowed_origins: &[String],
) -> Response<Full<Bytes>> {
    apply_cors(&mut response, origin, allowed_origins);
    response
}

fn apply_cors(
    response: &mut Response<Full<Bytes>>,
    origin: Option<&str>,
    allowed_origins: &[String],
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
        set_header(
            response,
            header::ACCESS_CONTROL_ALLOW_METHODS,
            HeaderValue::from_static("GET, POST, OPTIONS"),
        );
        set_header(
            response,
            header::ACCESS_CONTROL_ALLOW_HEADERS,
            HeaderValue::from_static("Authorization"),
        );
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
        assert!(host_is_allowed(Some("127.0.0.1:7878"), &[bind]));
        assert!(host_is_allowed(Some("localhost:7878"), &[bind]));
        assert!(host_is_allowed(Some("LOCALHOST:7878"), &[bind]));
        assert!(!host_is_allowed(Some("example.com"), &[bind]));
        assert!(!host_is_allowed(Some("127.0.0.1"), &[bind]));
        assert!(!host_is_allowed(None, &[bind]));
    }

    #[test]
    fn host_whitelist_accepts_every_bound_address() {
        let binds = [
            "127.0.0.1:7878".parse().unwrap(),
            "100.64.0.1:7878".parse().unwrap(),
            "192.168.50.10:7878".parse().unwrap(),
        ];
        assert!(host_is_allowed(Some("100.64.0.1:7878"), &binds));
        assert!(host_is_allowed(Some("192.168.50.10:7878"), &binds));
        assert!(!host_is_allowed(Some("100.64.0.2:7878"), &binds));
        assert!(!host_is_allowed(Some("evil.example:7878"), &binds));
    }

    #[test]
    fn bearer_comparison_does_not_treat_missing_and_wrong_tokens_differently() {
        let expected = "secret-token-value";
        assert!(bearer_matches(Some("Bearer secret-token-value"), expected));
        assert!(!bearer_matches(None, expected));
        assert!(!bearer_matches(Some("Bearer other"), expected));
        assert!(!bearer_matches(Some("Basic secret-token-value"), expected));
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
