mod config;
mod credential_backend;
mod credentials;

use std::future::Future;
use std::net::SocketAddr;
use std::path::PathBuf;
use std::sync::Arc;
use std::time::Duration;

pub use config::{
    AccountSettings, AppConfig, CONFIG_VERSION, CredentialsSettings, DaemonSettings, HttpSettings,
    ProviderLimitSettings, ProviderSettings, default_config_path, load,
};
use credential_backend::assemble_credential_store;
use credentials::{ChatGptVault, ClaudeVault};
use ullage_auth::CredentialStore;
use ullage_cli::{ClientError, ControlClient, ServiceAction, SystemClient};
use ullage_core::{
    Capability, ProviderDescriptor, ProviderError, ProviderId, ProviderRegistry, RegisteredProvider,
};
use ullage_daemon::{ControlService, DaemonEngine, JsonSnapshotStore, SystemClock};
use ullage_http::{
    BindAddressClass, HttpBindConfig, HttpBindTarget, HttpServer, classify_bind_address,
    discover_bind_addresses, parse_http_bind,
};
use ullage_protocol::{ControlRequest, ControlResponse, CredentialBackendId};
use ullage_provider_chatgpt::{
    ChatGptConfig, ChatGptHttpConfig, ChatGptProvider, ReqwestChatGptApi,
};
use ullage_provider_grok::{GrokProvider, HttpGrokConfig, HttpGrokTransport};

/// The public OAuth client the Codex CLI registers with `auth.openai.com`.
/// ChatGPT sign-in only accepts clients that OpenAI knows about, so the value
/// has to be a registered identifier rather than a name of our own choosing.
const CHATGPT_CLIENT_ID: &str = "app_EMoamEEZ73f0CkXaXp7hrann";
/// The callback registered for that client. OpenAI compares redirect URIs
/// verbatim, so `localhost` cannot be spelled `127.0.0.1` here.
const CHATGPT_REDIRECT_URI: &str = "http://localhost:1455/auth/callback";
const HTTP_BIND_RETRY_INTERVAL: Duration = Duration::from_secs(2);
const HTTP_BIND_RETRY_TIMEOUT: Duration = Duration::from_secs(60);

/// The compiled-in provider ids, shared by the registry construction and
/// `config` validation so a new provider cannot silently skip either.
pub(crate) const PROVIDER_IDS: [&str; 4] = ["claude", "chatgpt", "grok", "cursor"];

pub fn production_registry(config: &AppConfig) -> Result<ProviderRegistry, String> {
    Ok(production_components(config)?.0)
}

fn production_components(
    config: &AppConfig,
) -> Result<(ProviderRegistry, CredentialBackendId), String> {
    let (store, backend) = assemble_credential_store(&config.credentials)?;
    Ok((registry_with_credentials(store)?, backend))
}

pub fn registry_with_credentials(
    credentials: Arc<CredentialStore>,
) -> Result<ProviderRegistry, String> {
    let mut registry = ProviderRegistry::default();
    let claude_api = Arc::new(
        ullage_provider_claude::HttpClaudeApi::new()
            .map_err(|_| "Claude provider initialization failed")?,
    );
    let claude_credentials = credentials.clone();
    registry
        .register_factory(descriptor("claude", "Claude", true), move |account_id| {
            let store = Arc::new(
                ClaudeVault::new(claude_credentials.clone(), account_id)
                    .map_err(|_| credential_init_error())?,
            );
            Ok(Arc::new(ullage_provider_claude::ClaudeProvider::with_api(
                claude_api.clone(),
                store,
            )) as Arc<dyn RegisteredProvider>)
        })
        .map_err(|_| "Claude provider registration failed")?;

    let chatgpt_config = ChatGptConfig::openai(CHATGPT_CLIENT_ID, CHATGPT_REDIRECT_URI);
    let chatgpt_api = Arc::new(
        ReqwestChatGptApi::new(ChatGptHttpConfig::openai(CHATGPT_CLIENT_ID))
            .map_err(|_| "ChatGPT provider initialization failed")?,
    );
    let chatgpt_credentials = credentials.clone();
    let mut chatgpt_descriptor = descriptor("chatgpt", "ChatGPT", false);
    chatgpt_descriptor
        .capabilities
        .push(Capability::WorkspaceSelection);
    registry
        .register_factory(chatgpt_descriptor, move |account_id| {
            let store = Arc::new(
                ChatGptVault::new(chatgpt_credentials.clone(), account_id)
                    .map_err(|_| credential_init_error())?,
            );
            Ok(Arc::new(ChatGptProvider::new(
                chatgpt_config.clone(),
                chatgpt_api.clone(),
                store,
            )?) as Arc<dyn RegisteredProvider>)
        })
        .map_err(|_| "ChatGPT provider registration failed")?;

    let grok_transport = Arc::new(
        HttpGrokTransport::new(HttpGrokConfig {
            client_id: "b1a00492-073a-47ea-816f-4c329264a828".into(),
            scope: "openid profile email offline_access grok-cli:access api:access \
                     conversations:read conversations:write workspaces:read workspaces:write"
                .into(),
            // Kept for an explicit browser OAuth request. The macOS app no
            // longer drives that path: auth.x.ai does not bounce back, so Grok
            // defaults to device code.
            redirect_uri: "http://127.0.0.1:1456/callback".into(),
            device_authorization_url: "https://auth.x.ai/oauth2/device/code".into(),
            authorization_url: "https://auth.x.ai/oauth2/authorize".into(),
            token_url: "https://auth.x.ai/oauth2/token".into(),
            revoke_url: "https://auth.x.ai/oauth2/revoke".into(),
            billing_url: "https://cli-chat-proxy.grok.com/v1/billing?format=credits".into(),
            settings_url: "https://cli-chat-proxy.grok.com/v1/settings".into(),
        })
        .map_err(|_| "Grok provider initialization failed")?,
    );
    let grok_credentials = credentials.clone();
    registry
        .register_factory(descriptor("grok", "Grok", false), move |account_id| {
            Ok(Arc::new(GrokProvider::with_transport_and_store_for_account(
                grok_transport.clone(),
                grok_credentials.clone(),
                account_id,
            )?) as Arc<dyn RegisteredProvider>)
        })
        .map_err(|_| "Grok provider registration failed")?;

    let cursor_api: Arc<dyn ullage_provider_cursor::CursorApi> = Arc::new(
        ullage_provider_cursor::HttpCursorApi::new()
            .map_err(|_| "Cursor provider initialization failed")?,
    );
    registry
        .register_factory(descriptor("cursor", "Cursor", false), move |account_id| {
            Ok(Arc::new(
                ullage_provider_cursor::CursorProvider::with_api_and_store_for_account(
                    cursor_api.clone(),
                    credentials.clone(),
                    account_id,
                )?,
            ) as Arc<dyn RegisteredProvider>)
        })
        .map_err(|_| "Cursor provider registration failed")?;
    Ok(registry)
}

fn descriptor(id: &str, display_name: &str, subscription_expiry: bool) -> ProviderDescriptor {
    let mut capabilities = vec![
        Capability::Authentication,
        Capability::AuthenticationStatus,
        Capability::Logout,
        Capability::UsageQuery,
    ];
    if subscription_expiry {
        capabilities.push(Capability::SubscriptionExpiry);
    }
    ProviderDescriptor {
        id: ProviderId::new(id),
        display_name: display_name.into(),
        capabilities,
    }
}

fn credential_init_error() -> ProviderError {
    ProviderError::ProtocolIncompatible {
        message: "credential account identity is invalid".into(),
    }
}

pub async fn run_daemon() -> Result<(), String> {
    let config = config::load(&config::default_config_path()?).await?;
    let (registry, backend) = production_components(&config)?;
    run_daemon_with(config, registry, backend).await
}

pub async fn run_daemon_with(
    config: AppConfig,
    registry: ProviderRegistry,
    credential_backend: CredentialBackendId,
) -> Result<(), String> {
    run_daemon_with_discovery(
        config,
        registry,
        credential_backend,
        |port| {
            discover_bind_addresses(port)
                .map_err(|error| format!("http.bind could not enumerate local addresses: {error}"))
        },
        async {
            let _ = tokio::signal::ctrl_c().await;
        },
    )
    .await
}

async fn run_daemon_with_discovery<F, S>(
    config: AppConfig,
    registry: ProviderRegistry,
    credential_backend: CredentialBackendId,
    discover: F,
    shutdown: S,
) -> Result<(), String>
where
    F: FnMut(u16) -> Result<Vec<SocketAddr>, String> + Send + 'static,
    S: Future<Output = ()> + Send + 'static,
{
    let engine = DaemonEngine::new(
        config.daemon.build(),
        Arc::new(registry),
        Arc::new(SystemClock),
        Arc::new(JsonSnapshotStore::new(state_path()?)),
    )
    .await
    .map_err(|_| "daemon engine initialization failed")?;
    merge_configured_accounts(&engine, &config.accounts).await?;
    let service = ControlService::new(engine.clone()).with_credential_backend(credential_backend);
    service.configure_device_store(devices_path()?)?;
    // Status answers stay not-ready until the control plane is up: every step
    // before it can still terminate the process. HTTP setup runs afterwards
    // as non-fatal background work, so it is not part of the gate.
    service.defer_readiness();

    #[cfg(unix)]
    let server =
        ullage_daemon::UnixControlServer::bind(control_endpoint(), service.clone()).await?;
    #[cfg(windows)]
    let server = ullage_daemon::WindowsControlServer::bind(control_endpoint()?, service.clone())?;

    let running_engine = engine.clone();
    let engine_task = tokio::spawn(async move { running_engine.run().await });
    let signal_engine = engine.clone();
    let signal_task = tokio::spawn(async move {
        shutdown.await;
        signal_engine.shutdown();
    });
    let mut control_task = tokio::spawn(server.run());
    service.mark_initialized();

    // HTTP setup can wait up to a minute for a non-loopback address under
    // `http.bind = auto`. It must neither delay readiness nor hold shutdown
    // hostage, and once the control plane answers it can no longer be fatal:
    // a launcher may already have reported success. A failed bind therefore
    // leaves the daemon running control-only with the error on stderr.
    let mut http_setup = tokio::spawn(http_server(config.http.clone(), service.clone(), discover));
    let result = tokio::select! {
        control_result = &mut control_task => {
            http_setup.abort();
            control_result.map_err(|_| "control server task failed".to_owned())?
        }
        setup_result = &mut http_setup => match setup_result {
            Ok(Ok(Some(http))) => {
                run_control_and_http(control_task, http, engine.clone()).await
            }
            Ok(Ok(None)) => control_task
                .await
                .map_err(|_| "control server task failed".to_owned())?,
            Ok(Err(error)) => {
                eprintln!("http setup failed; daemon continues without http: {error}");
                control_task
                    .await
                    .map_err(|_| "control server task failed".to_owned())?
            }
            Err(_) => {
                eprintln!("http setup task failed; daemon continues without http");
                control_task
                    .await
                    .map_err(|_| "control server task failed".to_owned())?
            }
        }
    };
    engine.shutdown();
    let _ = engine_task.await;
    signal_task.abort();
    result
}

async fn http_server<F>(
    http: HttpSettings,
    service: ControlService,
    discover: F,
) -> Result<Option<HttpServer>, String>
where
    F: FnMut(u16) -> Result<Vec<SocketAddr>, String>,
{
    if !http.enabled {
        return Ok(None);
    }
    let target = parse_http_bind(&http.bind)?;
    Ok(Some(match target {
        HttpBindTarget::Explicit(bind) => {
            let bind_config = http_bind_config(&http, vec![bind])?;
            retry_http_bind(
                bind,
                HTTP_BIND_RETRY_INTERVAL,
                HTTP_BIND_RETRY_TIMEOUT,
                || HttpServer::bind(bind_config.clone(), service.clone()),
                |error| error.io_kind() == Some(std::io::ErrorKind::AddrNotAvailable),
            )
            .await
            .map_err(String::from)?
        }
        HttpBindTarget::Auto(port) => {
            let binds = wait_for_auto_bind_addresses(
                port,
                HTTP_BIND_RETRY_INTERVAL,
                HTTP_BIND_RETRY_TIMEOUT,
                discover,
            )
            .await?;
            HttpServer::bind(http_bind_config(&http, binds)?, service.clone())
                .await
                .map_err(String::from)?
        }
    }))
}

async fn retry_http_bind<T, E, F, Fut, P>(
    bind: SocketAddr,
    interval: Duration,
    timeout: Duration,
    mut attempt: F,
    retryable: P,
) -> Result<T, E>
where
    F: FnMut() -> Fut,
    Fut: Future<Output = Result<T, E>>,
    P: Fn(&E) -> bool,
{
    let started = tokio::time::Instant::now();
    loop {
        match attempt().await {
            Ok(value) => return Ok(value),
            Err(error)
                if classify_bind_address(bind.ip()).is_some()
                    && !bind.ip().is_loopback()
                    && retryable(&error)
                    && started.elapsed() < timeout =>
            {
                let remaining = timeout.saturating_sub(started.elapsed());
                let delay = interval.min(remaining);
                eprintln!(
                    "http.bind address is not available; retrying in {} seconds",
                    delay.as_secs_f64()
                );
                tokio::time::sleep(delay).await;
            }
            Err(error) => return Err(error),
        }
    }
}

async fn wait_for_auto_bind_addresses<F>(
    port: u16,
    interval: Duration,
    timeout: Duration,
    mut discover: F,
) -> Result<Vec<SocketAddr>, String>
where
    F: FnMut(u16) -> Result<Vec<SocketAddr>, String>,
{
    let started = tokio::time::Instant::now();
    loop {
        let addresses = discover(port)?;
        let has_remote_address = addresses.iter().any(|address| {
            matches!(
                classify_bind_address(address.ip()),
                Some(BindAddressClass::Tailnet | BindAddressClass::Lan)
            )
        });
        if has_remote_address || started.elapsed() >= timeout {
            return Ok(addresses);
        }
        let remaining = timeout.saturating_sub(started.elapsed());
        let delay = interval.min(remaining);
        eprintln!(
            "http.bind auto discovery found only loopback; retrying in {} seconds",
            delay.as_secs_f64()
        );
        tokio::time::sleep(delay).await;
    }
}

async fn run_control_and_http(
    mut control_task: tokio::task::JoinHandle<Result<(), String>>,
    http: HttpServer,
    engine: DaemonEngine,
) -> Result<(), String> {
    let mut http_task = tokio::spawn(http.run());
    tokio::select! {
        control_result = &mut control_task => {
            engine.shutdown();
            merge_server_results(control_result, http_task.await)
        }
        http_result = &mut http_task => {
            engine.shutdown();
            merge_server_results(control_task.await, http_result)
        }
    }
}

fn merge_server_results(
    unix: Result<Result<(), String>, tokio::task::JoinError>,
    http: Result<Result<(), String>, tokio::task::JoinError>,
) -> Result<(), String> {
    let unix = unix.map_err(|_| "control server task failed".to_owned())?;
    let http = http.map_err(|_| "http server task failed".to_owned())?;
    match (unix, http) {
        (Ok(()), Ok(())) => Ok(()),
        (Err(error), Ok(())) | (Ok(()), Err(error)) => Err(error),
        (Err(left), Err(right)) => Err(format!("{left}; {right}")),
    }
}

fn http_bind_config(http: &HttpSettings, binds: Vec<SocketAddr>) -> Result<HttpBindConfig, String> {
    // `config::validate` enforces this for file-loaded configuration; the
    // check is repeated here because `run_daemon_with` also accepts
    // programmatically built `AppConfig` values.
    if http.allowed_origins.iter().any(|origin| origin == "*") {
        return Err("http.allowed_origins must not contain *".into());
    }
    Ok(HttpBindConfig {
        binds,
        allowed_origins: http.allowed_origins.clone(),
        probe_min_interval: Duration::from_secs(http.probe_min_interval_seconds),
        device_store_path: devices_path()?,
    })
}

pub fn devices_path() -> Result<PathBuf, String> {
    let state = state_path()?;
    let parent = state
        .parent()
        .ok_or_else(|| "state path has no parent directory".to_owned())?;
    Ok(parent.join("devices.json"))
}

pub struct ProductionClient {
    inner: SystemClient,
}

impl ProductionClient {
    pub fn from_environment() -> Self {
        Self {
            inner: SystemClient::from_environment(),
        }
    }
}

impl ControlClient for ProductionClient {
    fn send(&self, request: &ControlRequest) -> Result<ControlResponse, ClientError> {
        self.inner.send(request)
    }

    fn run_daemon(&self) -> Result<(), ClientError> {
        self.inner.run_daemon()
    }

    fn manage_daemon(&self, action: ServiceAction) -> Result<(), ClientError> {
        self.inner.manage_daemon(action)
    }

    fn daemon_service_installed(&self) -> Result<bool, ClientError> {
        self.inner.daemon_service_installed()
    }
}

async fn merge_configured_accounts(
    engine: &DaemonEngine,
    configured_accounts: &[AccountSettings],
) -> Result<(), String> {
    for account in configured_accounts {
        let account = account.build()?;
        if engine.account_config(&account.id).await.is_none()
            && !engine.account_was_removed(&account.id).await
        {
            engine
                .add_account(account)
                .await
                .map_err(|_| "configured account initialization failed")?;
        }
    }
    Ok(())
}

#[cfg(unix)]
fn control_endpoint() -> PathBuf {
    ullage_cli::control_endpoint_from_environment()
}

#[cfg(windows)]
fn control_endpoint() -> Result<PathBuf, String> {
    ullage_cli::control_endpoint_from_environment()
        .ok_or_else(|| "current Windows user identity unavailable".to_owned())
}

fn state_path() -> Result<PathBuf, String> {
    if let Some(path) = std::env::var_os("ULLAGE_STATE_FILE") {
        return Ok(path.into());
    }
    #[cfg(target_os = "macos")]
    if let Some(home) = std::env::var_os("HOME") {
        return Ok(PathBuf::from(home).join("Library/Application Support/Ullage/state.json"));
    }
    #[cfg(all(unix, not(target_os = "macos")))]
    {
        if let Some(root) = std::env::var_os("XDG_STATE_HOME") {
            return Ok(PathBuf::from(root).join("ullage/state.json"));
        }
        if let Some(home) = std::env::var_os("HOME") {
            return Ok(PathBuf::from(home).join(".local/state/ullage/state.json"));
        }
    }
    #[cfg(windows)]
    if let Some(root) = std::env::var_os("LOCALAPPDATA") {
        return Ok(PathBuf::from(root).join("Ullage/state.json"));
    }
    Err("state path unavailable".into())
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::atomic::{AtomicUsize, Ordering};

    #[tokio::test]
    async fn retries_only_unavailable_allowed_non_loopback_binds() {
        let attempts = AtomicUsize::new(0);
        let result = retry_http_bind(
            "100.64.0.1:7878".parse().unwrap(),
            Duration::ZERO,
            Duration::from_secs(1),
            || {
                let attempt = attempts.fetch_add(1, Ordering::SeqCst);
                async move {
                    if attempt < 2 {
                        Err(std::io::Error::from(std::io::ErrorKind::AddrNotAvailable))
                    } else {
                        Ok(42)
                    }
                }
            },
            |error| error.kind() == std::io::ErrorKind::AddrNotAvailable,
        )
        .await
        .unwrap();
        assert_eq!(result, 42);
        assert_eq!(attempts.load(Ordering::SeqCst), 3);

        for (bind, kind) in [
            ("100.64.0.1:7878", std::io::ErrorKind::AddrInUse),
            ("127.0.0.1:7878", std::io::ErrorKind::AddrNotAvailable),
        ] {
            let attempts = AtomicUsize::new(0);
            let error = retry_http_bind(
                bind.parse().unwrap(),
                Duration::ZERO,
                Duration::from_secs(1),
                || {
                    attempts.fetch_add(1, Ordering::SeqCst);
                    async { Err::<(), _>(std::io::Error::from(kind)) }
                },
                |error| error.kind() == std::io::ErrorKind::AddrNotAvailable,
            )
            .await
            .unwrap_err();
            assert_eq!(error.kind(), kind);
            assert_eq!(attempts.load(Ordering::SeqCst), 1);
        }
    }

    #[tokio::test]
    async fn unavailable_tailscale_bind_stops_after_timeout() {
        let attempts = AtomicUsize::new(0);
        let error = retry_http_bind(
            "100.64.0.1:7878".parse().unwrap(),
            Duration::from_millis(1),
            Duration::from_millis(3),
            || {
                attempts.fetch_add(1, Ordering::SeqCst);
                async { Err::<(), _>(std::io::Error::from(std::io::ErrorKind::AddrNotAvailable)) }
            },
            |error| error.kind() == std::io::ErrorKind::AddrNotAvailable,
        )
        .await
        .unwrap_err();
        assert_eq!(error.kind(), std::io::ErrorKind::AddrNotAvailable);
        assert!(attempts.load(Ordering::SeqCst) >= 2);
    }

    #[tokio::test]
    async fn auto_bind_discovery_retries_until_a_remote_address_exists() {
        let attempts = AtomicUsize::new(0);
        let addresses =
            wait_for_auto_bind_addresses(7878, Duration::ZERO, Duration::from_secs(1), |_| {
                let attempt = attempts.fetch_add(1, Ordering::SeqCst);
                if attempt == 0 {
                    Ok(vec!["127.0.0.1:7878".parse().unwrap()])
                } else {
                    Ok(vec![
                        "127.0.0.1:7878".parse().unwrap(),
                        "192.168.50.10:7878".parse().unwrap(),
                    ])
                }
            })
            .await
            .unwrap();
        assert_eq!(addresses.len(), 2);
        assert_eq!(attempts.load(Ordering::SeqCst), 2);
    }

    #[tokio::test]
    async fn auto_bind_discovery_uses_loopback_after_timeout() {
        let attempts = AtomicUsize::new(0);
        let addresses = wait_for_auto_bind_addresses(
            7878,
            Duration::from_millis(1),
            Duration::from_millis(3),
            |_| {
                attempts.fetch_add(1, Ordering::SeqCst);
                Ok(vec!["127.0.0.1:7878".parse().unwrap()])
            },
        )
        .await
        .unwrap();
        assert_eq!(addresses, vec!["127.0.0.1:7878".parse().unwrap()]);
        assert!(attempts.load(Ordering::SeqCst) >= 2);
    }

    #[tokio::test]
    async fn removed_config_seed_stays_removed_after_engine_restart() {
        let store = Arc::new(ullage_daemon::MemorySnapshotStore::default());
        let configured = vec![AccountSettings {
            id: "configured-claude".into(),
            provider: "claude".into(),
            label: Some("seed@example.test".into()),
            ..AccountSettings::default()
        }];
        let engine = DaemonEngine::new(
            ullage_daemon::DaemonConfig::default(),
            Arc::new(ProviderRegistry::default()),
            Arc::new(SystemClock),
            store.clone(),
        )
        .await
        .unwrap();
        merge_configured_accounts(&engine, &configured)
            .await
            .unwrap();
        let id = ullage_daemon::AccountId::new("configured-claude");
        assert!(engine.remove_account(&id).await.unwrap().is_some());
        drop(engine);

        let restarted = DaemonEngine::new(
            ullage_daemon::DaemonConfig::default(),
            Arc::new(ProviderRegistry::default()),
            Arc::new(SystemClock),
            store,
        )
        .await
        .unwrap();
        merge_configured_accounts(&restarted, &configured)
            .await
            .unwrap();
        assert!(restarted.account_config(&id).await.is_none());
        assert!(restarted.account_was_removed(&id).await);
    }

    #[tokio::test]
    async fn configured_metrics_seed_new_accounts_once() {
        let store = Arc::new(ullage_daemon::MemorySnapshotStore::default());
        let configured = vec![AccountSettings {
            id: "seeded-claude".into(),
            provider: "claude".into(),
            metrics: vec!["Usage".into(), "Codex".into()],
            ..AccountSettings::default()
        }];
        let engine = DaemonEngine::new(
            ullage_daemon::DaemonConfig::default(),
            Arc::new(ProviderRegistry::default()),
            Arc::new(SystemClock),
            store.clone(),
        )
        .await
        .unwrap();
        merge_configured_accounts(&engine, &configured)
            .await
            .unwrap();
        let id = ullage_daemon::AccountId::new("seeded-claude");
        assert_eq!(
            engine.account_config(&id).await.unwrap().metrics,
            vec!["Usage", "Codex"]
        );

        // The stored value wins over the seed on the next merge.
        engine
            .set_account_metrics(&id, vec!["Credits".into()])
            .await
            .unwrap()
            .unwrap();
        merge_configured_accounts(&engine, &configured)
            .await
            .unwrap();
        assert_eq!(
            engine.account_config(&id).await.unwrap().metrics,
            vec!["Credits"]
        );
    }

    #[cfg(unix)]
    static CONTROL_ENV_LOCK: std::sync::Mutex<()> = std::sync::Mutex::new(());

    // The env lock guard is held across awaits; on the current-thread test
    // runtime both contenders always progress, so no deadlock is possible.
    #[cfg(unix)]
    #[tokio::test]
    #[allow(clippy::await_holding_lock)]
    async fn control_endpoint_answers_while_auto_bind_waits_for_a_remote_address() {
        use tokio::io::{AsyncBufReadExt, AsyncWriteExt};

        use std::os::unix::fs::DirBuilderExt;

        // These tests set process-wide endpoint env vars; serialize them.
        let _guard = CONTROL_ENV_LOCK
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        static SEQUENCE: AtomicUsize = AtomicUsize::new(0);
        let directory = std::env::temp_dir().join(format!(
            "ullage-cli-autobind-{}-{}",
            std::process::id(),
            SEQUENCE.fetch_add(1, Ordering::Relaxed)
        ));
        // The control socket parent must be owner-only, like the runtime dir.
        std::fs::DirBuilder::new()
            .recursive(true)
            .mode(0o700)
            .create(&directory)
            .unwrap();
        let socket = directory.join("control.sock");
        // SAFETY: no other test in this process reads these variables.
        unsafe {
            std::env::set_var("ULLAGE_CONTROL_SOCKET", &socket);
            std::env::set_var("ULLAGE_STATE_FILE", directory.join("state.json"));
        }

        let mut config = AppConfig::default();
        config.http.enabled = true;
        config.http.bind = "auto:7878".into();
        let daemon = tokio::spawn(run_daemon_with_discovery(
            config,
            ProviderRegistry::default(),
            CredentialBackendId::native(),
            |_| Ok(vec!["127.0.0.1:7878".parse().unwrap()]),
            std::future::pending(),
        ));

        let deadline = tokio::time::Instant::now() + Duration::from_secs(10);
        while !socket.exists() {
            assert!(!daemon.is_finished(), "daemon task exited before binding");
            assert!(
                tokio::time::Instant::now() < deadline,
                "control socket was not bound"
            );
            tokio::time::sleep(Duration::from_millis(10)).await;
        }

        let mut stream = tokio::net::UnixStream::connect(&socket).await.unwrap();
        let request = ControlRequest::new(
            "auto-bind-window",
            ullage_protocol::ControlCommand::DaemonStatus,
        );
        let mut encoded = serde_json::to_string(&request).unwrap();
        encoded.push('\n');
        stream.write_all(encoded.as_bytes()).await.unwrap();
        let mut reader = tokio::io::BufReader::new(stream);
        let mut line = String::new();
        tokio::time::timeout(Duration::from_secs(5), reader.read_line(&mut line))
            .await
            .expect("control endpoint did not answer during http auto-discovery")
            .unwrap();
        let response: ControlResponse = serde_json::from_str(&line).unwrap();
        let ullage_protocol::ControlResult::DaemonStatus(status) = response.result else {
            panic!("expected DaemonStatus, got {:?}", response.result);
        };
        // HTTP setup is non-fatal background work, so the control plane may
        // report ready while auto discovery is still pending.
        assert!(!status.shutting_down, "endpoint stayed not-ready");

        daemon.abort();
        // SAFETY: the daemon task is aborted and no other test reads these.
        unsafe {
            std::env::remove_var("ULLAGE_CONTROL_SOCKET");
            std::env::remove_var("ULLAGE_STATE_FILE");
        }
        std::fs::remove_dir_all(directory).unwrap();
    }

    #[cfg(unix)]
    #[tokio::test]
    #[allow(clippy::await_holding_lock)]
    async fn control_endpoint_reports_ready_once_fatal_init_completes() {
        use tokio::io::{AsyncBufReadExt, AsyncWriteExt};

        use std::os::unix::fs::DirBuilderExt;

        let _guard = CONTROL_ENV_LOCK
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        static SEQUENCE: AtomicUsize = AtomicUsize::new(0);
        let directory = std::env::temp_dir().join(format!(
            "ullage-cli-ready-{}-{}",
            std::process::id(),
            SEQUENCE.fetch_add(1, Ordering::Relaxed)
        ));
        std::fs::DirBuilder::new()
            .recursive(true)
            .mode(0o700)
            .create(&directory)
            .unwrap();
        let socket = directory.join("control.sock");
        // SAFETY: no other test in this process reads these variables.
        unsafe {
            std::env::set_var("ULLAGE_CONTROL_SOCKET", &socket);
            std::env::set_var("ULLAGE_STATE_FILE", directory.join("state.json"));
        }

        // HTTP disabled: control-plane setup completes immediately, then
        // status must go ready — otherwise launchers could never declare
        // success.
        let daemon = tokio::spawn(run_daemon_with_discovery(
            AppConfig::default(),
            ProviderRegistry::default(),
            CredentialBackendId::native(),
            |_| Ok(Vec::new()),
            std::future::pending(),
        ));

        let deadline = tokio::time::Instant::now() + Duration::from_secs(10);
        let ready = loop {
            assert!(!daemon.is_finished(), "daemon task exited early");
            assert!(
                tokio::time::Instant::now() < deadline,
                "control endpoint never reported ready"
            );
            if !socket.exists() {
                tokio::time::sleep(Duration::from_millis(10)).await;
                continue;
            }
            let Ok(mut stream) = tokio::net::UnixStream::connect(&socket).await else {
                tokio::time::sleep(Duration::from_millis(10)).await;
                continue;
            };
            let request =
                ControlRequest::new("ready-poll", ullage_protocol::ControlCommand::DaemonStatus);
            let mut encoded = serde_json::to_string(&request).unwrap();
            encoded.push('\n');
            if stream.write_all(encoded.as_bytes()).await.is_err() {
                continue;
            }
            let mut reader = tokio::io::BufReader::new(stream);
            let mut line = String::new();
            let Ok(Ok(_)) =
                tokio::time::timeout(Duration::from_secs(2), reader.read_line(&mut line)).await
            else {
                continue;
            };
            let Ok(response) = serde_json::from_str::<ControlResponse>(&line) else {
                continue;
            };
            match response.result {
                ullage_protocol::ControlResult::DaemonStatus(status) if !status.shutting_down => {
                    break true;
                }
                _ => {}
            }
            tokio::time::sleep(Duration::from_millis(25)).await;
        };
        assert!(ready);

        daemon.abort();
        // SAFETY: the daemon task is aborted and no other test reads these.
        unsafe {
            std::env::remove_var("ULLAGE_CONTROL_SOCKET");
            std::env::remove_var("ULLAGE_STATE_FILE");
        }
        std::fs::remove_dir_all(directory).unwrap();
    }

    #[cfg(unix)]
    #[tokio::test]
    #[allow(clippy::await_holding_lock)]
    async fn shutdown_during_auto_bind_discovery_exits_promptly() {
        use std::os::unix::fs::DirBuilderExt;

        let _guard = CONTROL_ENV_LOCK
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        static SEQUENCE: AtomicUsize = AtomicUsize::new(0);
        let directory = std::env::temp_dir().join(format!(
            "ullage-cli-shutdown-{}-{}",
            std::process::id(),
            SEQUENCE.fetch_add(1, Ordering::Relaxed)
        ));
        std::fs::DirBuilder::new()
            .recursive(true)
            .mode(0o700)
            .create(&directory)
            .unwrap();
        let socket = directory.join("control.sock");
        // SAFETY: no other test in this process reads these variables.
        unsafe {
            std::env::set_var("ULLAGE_CONTROL_SOCKET", &socket);
            std::env::set_var("ULLAGE_STATE_FILE", directory.join("state.json"));
        }

        let mut config = AppConfig::default();
        config.http.enabled = true;
        config.http.bind = "auto:7878".into();
        let (released, shutdown) = tokio::sync::oneshot::channel::<()>();
        let daemon = tokio::spawn(run_daemon_with_discovery(
            config,
            ProviderRegistry::default(),
            CredentialBackendId::native(),
            // Discovery only ever sees loopback, so HTTP setup would wait the
            // full retry window without the shutdown race.
            |_| Ok(vec!["127.0.0.1:7878".parse().unwrap()]),
            async move {
                let _ = shutdown.await;
            },
        ));

        let deadline = tokio::time::Instant::now() + Duration::from_secs(10);
        while !socket.exists() {
            assert!(!daemon.is_finished(), "daemon task exited before binding");
            assert!(
                tokio::time::Instant::now() < deadline,
                "control socket was not bound"
            );
            tokio::time::sleep(Duration::from_millis(10)).await;
        }

        let _ = released.send(());
        tokio::time::timeout(Duration::from_secs(10), daemon)
            .await
            .expect("daemon ignored shutdown during http discovery")
            .expect("daemon task panicked")
            .unwrap();

        // SAFETY: the daemon task has ended and no other test reads these.
        unsafe {
            std::env::remove_var("ULLAGE_CONTROL_SOCKET");
            std::env::remove_var("ULLAGE_STATE_FILE");
        }
        std::fs::remove_dir_all(directory).unwrap();
    }
}
