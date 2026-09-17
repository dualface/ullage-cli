use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex, RwLock};
use std::time::Duration;

use serde::Deserialize;
use ullage_auth::{AuthStartRequest, LogoutRequest, validate_loopback_http_redirect_uri};
use ullage_core::{RegisteredProvider, SubscriptionUsage, UsageQuery};
use ullage_protocol::{
    Account, AccountError, AccountId as ProtocolAccountId, CONTROL_PROTOCOL_VERSION,
    ControlCommand, ControlError, ControlRequest, ControlResponse, ControlResult,
    CredentialBackendId, ProbePayload,
};

use crate::model::sanitize_provider_error;
use crate::{
    AccountConfig, AccountId, BackoffConfig, DaemonEngine, DaemonError, DaemonStatus, ProbeError,
    ProbeTrigger, SnapshotRecord,
};

mod payload;

use self::payload::{
    account_control_error, account_metrics, account_payload, probe_control_error, snapshot_payload,
    status_payload,
};
use crate::{DeviceCredential, DeviceStore, PairDeviceError};

const CONTROL_ACCOUNT_INTERVAL: Duration = Duration::from_secs(5 * 60);
const CONTROL_ACCOUNT_TIMEOUT: Duration = Duration::from_secs(30);
const CONTROL_ACCOUNT_JITTER: Duration = Duration::from_secs(5);

/// Where a control request entered the daemon.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum ControlTransport {
    /// Local Unix socket or Windows named pipe with peer identity checks.
    Local,
    /// Paired-device HTTP API.
    RemoteHttp,
}

fn prepare_auth_start_request(
    mut request: AuthStartRequest,
    transport: ControlTransport,
) -> Result<AuthStartRequest, ControlError> {
    match transport {
        ControlTransport::RemoteHttp => {
            request.redirect_uri = None;
            Ok(request)
        }
        ControlTransport::Local => {
            if let Some(redirect_uri) = request.redirect_uri.as_deref() {
                validate_loopback_http_redirect_uri(redirect_uri).map_err(|reason| {
                    ControlError::Provider(ullage_core::ProviderError::AuthenticationInvalid {
                        message: reason.into(),
                    })
                })?;
            }
            Ok(request)
        }
    }
}

#[derive(Clone)]
pub struct ControlService {
    engine: DaemonEngine,
    credential_backend: CredentialBackendId,
    device_store: Arc<RwLock<DeviceStore>>,
    /// One gate per provider, held across a duplicate cleanup. Scoped per
    /// provider so a slow cleanup only delays cleanups for that same provider.
    auth_gates: Arc<Mutex<HashMap<ullage_core::ProviderId, Arc<tokio::sync::Mutex<()>>>>>,
}

impl ControlService {
    pub fn new(engine: DaemonEngine) -> Self {
        Self {
            engine,
            credential_backend: CredentialBackendId::native(),
            device_store: Arc::new(RwLock::new(DeviceStore::memory())),
            auth_gates: Arc::default(),
        }
    }

    pub fn configure_device_store(&self, path: impl Into<PathBuf>) -> Result<(), String> {
        let path = path.into();
        let mut store = self
            .device_store
            .write()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        if store.path().as_deref() == Some(path.as_path()) {
            return Ok(());
        }
        if let Some(existing) = store.path() {
            return Err(format!(
                "device store is already configured at {}",
                existing.display()
            ));
        }
        *store = DeviceStore::open(path)?;
        Ok(())
    }

    pub fn create_pair_code(&self) -> Result<ullage_protocol::PairCodePayload, String> {
        self.device_store().create_pair_code()
    }

    pub fn list_devices(&self) -> Vec<ullage_protocol::DevicePayload> {
        self.device_store().list_devices()
    }

    pub fn revoke_device(&self, device_id: &str) -> Result<bool, String> {
        self.device_store().revoke_device(device_id)
    }

    pub fn pair_device(
        &self,
        pair_code: &str,
        device_name: &str,
    ) -> Result<DeviceCredential, PairDeviceError> {
        self.device_store().pair(pair_code, device_name)
    }

    pub fn authenticate_device(&self, token: &str) -> Result<bool, String> {
        self.device_store().authenticate(token)
    }

    fn device_store(&self) -> DeviceStore {
        self.device_store
            .read()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .clone()
    }

    #[must_use]
    pub fn with_credential_backend(mut self, credential_backend: CredentialBackendId) -> Self {
        self.credential_backend = credential_backend;
        self
    }

    pub async fn daemon_status(&self) -> DaemonStatus {
        self.engine.status().await
    }

    pub async fn wait_for_shutdown(&self) {
        self.engine.wait_for_shutdown().await
    }

    pub async fn probe(
        &self,
        account_id: &AccountId,
    ) -> Result<ullage_core::QueryOutcome<SubscriptionUsage>, ProbeError> {
        self.engine.probe(account_id, ProbeTrigger::Manual).await
    }

    /// Reports whether `account_id` names a configured account. The HTTP
    /// probe cooldown uses this so attempts on names that do not exist never
    /// consume rate-limit state.
    pub async fn account_exists(&self, account_id: &AccountId) -> bool {
        self.engine.account_config(account_id).await.is_some()
    }

    pub async fn show(&self, account_id: &AccountId) -> Option<SnapshotRecord> {
        self.engine.show(account_id).await
    }

    pub async fn show_all(&self) -> Vec<SnapshotRecord> {
        self.engine.show_all().await
    }

    async fn provider_for_account(
        &self,
        provider: &ullage_core::ProviderId,
        account: &ProtocolAccountId,
    ) -> Result<Arc<dyn RegisteredProvider>, ControlError> {
        let account_id = AccountId::new(account.as_str());
        let Some(config) = self.engine.account_config(&account_id).await else {
            return Err(AccountError::NotFound(account.clone()).into());
        };
        if &config.provider != provider {
            return Err(AccountError::NotFound(account.clone()).into());
        }
        self.engine
            .registry()
            .get_for_account(provider, account.as_str())
            .map_err(Into::into)
    }

    /// Removes the accounts of `provider` signed in as the same identity as
    /// `account`, whose own state is `state`. Called once `account` is fully set
    /// up, so nothing is deleted before its replacement is known to work, and
    /// the sign-in the user just finished is the one that survives.
    ///
    /// A provider that reports no identity cannot be compared, and a sibling
    /// whose status cannot be read is not evidence of a duplicate, so both leave
    /// the accounts alone. Each account is signed out before it is removed; one
    /// that will not sign out keeps its row rather than leaving a secret behind
    /// with nothing pointing at it.
    async fn retire_duplicates(
        &self,
        provider: &ullage_core::ProviderId,
        account: &ProtocolAccountId,
        state: &ullage_auth::AuthState,
    ) -> Vec<Account> {
        let mut retired = Vec::new();
        // Nothing is removed on behalf of an account that is not itself signed
        // in: an expired row still names its account, which makes it something
        // to replace rather than something to replace others with.
        if !matches!(state, ullage_auth::AuthState::Authenticated { .. }) {
            return retired;
        }
        let Some(key) = identity_of(state) else {
            return retired;
        };
        for config in self.engine.account_configs().await {
            let other = ProtocolAccountId::new(config.id.to_string());
            if &config.provider != provider || other == *account {
                continue;
            }
            let Ok(instance) = self
                .engine
                .registry()
                .get_for_account(provider, other.as_str())
            else {
                continue;
            };
            let Ok(status) = instance.auth_status().await else {
                continue;
            };
            let Some(other_key) = identity_of(&status) else {
                continue;
            };
            if !other_key.eq_ignore_ascii_case(&key) {
                continue;
            }
            if instance.logout(LogoutRequest::default()).await.is_err() {
                continue;
            }
            if self.engine.remove_account(&config.id).await.is_ok() {
                retired.push(account_payload(config));
            }
        }
        retired
    }

    /// Serializes a provider's duplicate cleanups. Two cleanups for one identity
    /// that ran concurrently would each see the other account as the duplicate
    /// and remove it, leaving neither.
    async fn auth_gate(&self, provider: &ullage_core::ProviderId) -> Arc<tokio::sync::Mutex<()>> {
        let mut gates = self
            .auth_gates
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        gates.entry(provider.clone()).or_default().clone()
    }

    pub async fn handle(&self, request: ControlRequest) -> ControlResponse {
        self.handle_with_transport(request, ControlTransport::Local)
            .await
    }

    pub async fn handle_with_transport(
        &self,
        request: ControlRequest,
        transport: ControlTransport,
    ) -> ControlResponse {
        let request_id = request.request_id;
        let diagnostics = request.diagnostics;
        let mut diagnostic = None;
        if request.version != CONTROL_PROTOCOL_VERSION {
            return ControlResponse {
                version: CONTROL_PROTOCOL_VERSION,
                request_id,
                result: ControlResult::ProtocolMismatch {
                    supported_version: CONTROL_PROTOCOL_VERSION,
                },
                diagnostic: None,
            };
        }
        let result = match request.command {
            ControlCommand::DaemonStatus => ControlResult::DaemonStatus(status_payload(
                self.engine.status().await,
                self.credential_backend,
            )),
            ControlCommand::CreatePairCode => match self.create_pair_code() {
                Ok(code) => ControlResult::PairCode(code),
                Err(_) => ControlResult::Error(ControlError::Storage),
            },
            ControlCommand::ListDevices => ControlResult::Devices(self.list_devices()),
            ControlCommand::RevokeDevice { device_id } => match self.revoke_device(&device_id) {
                Ok(true) => ControlResult::Ack,
                Ok(false) => ControlResult::Error(ControlError::DeviceNotFound { device_id }),
                Err(_) => ControlResult::Error(ControlError::Storage),
            },
            ControlCommand::ListProviders => {
                ControlResult::Providers(self.engine.registry().descriptors())
            }
            ControlCommand::Probe { account_id, wait } => {
                let account_id = AccountId::new(account_id);
                if wait {
                    match self
                        .engine
                        .probe_unsanitized(&account_id, ProbeTrigger::Manual)
                        .await
                    {
                        Ok(usage) => ControlResult::Probe(ProbePayload {
                            account_id: account_id.to_string(),
                            usage,
                            metrics: account_metrics(&self.engine, &account_id).await,
                        }),
                        Err(error) => {
                            diagnostic = probe_error_diagnostic(diagnostics, &error);
                            ControlResult::Error(probe_control_error(error))
                        }
                    }
                } else {
                    match self
                        .engine
                        .start_probe(&account_id, ProbeTrigger::Manual)
                        .await
                    {
                        Ok(()) => ControlResult::Ack,
                        Err(error) => {
                            diagnostic = probe_error_diagnostic(diagnostics, &error);
                            ControlResult::Error(probe_control_error(error))
                        }
                    }
                }
            }
            ControlCommand::Show { account_id } => match account_id {
                Some(account_id) => {
                    let id = AccountId::new(&account_id);
                    match self.engine.show_configured_account(&id).await {
                        Err(()) => {
                            ControlResult::Error(ControlError::AccountNotFound { account_id })
                        }
                        Ok(snapshot) => ControlResult::Snapshots({
                            let metrics = account_metrics(&self.engine, &id).await;
                            snapshot
                                .into_iter()
                                .map(|snapshot| snapshot_payload(snapshot, &metrics))
                                .collect()
                        }),
                    }
                }
                None => {
                    let mut snapshots = Vec::new();
                    for config in self.engine.account_configs().await {
                        if let Some(snapshot) = self.engine.show(&config.id).await {
                            snapshots.push(snapshot_payload(snapshot, &config.metrics));
                        }
                    }
                    ControlResult::Snapshots(snapshots)
                }
            },
            ControlCommand::QueryUsage { provider, query } => {
                if !self.engine.registry().contains(&provider) {
                    let error = ullage_core::RegistryError::NotFound(provider.clone());
                    ControlResult::Error(error.into())
                } else {
                    match self
                        .engine
                        .find_account(&provider, query.account_label.as_deref())
                        .await
                    {
                        Some(account_id) => {
                            match self.engine.probe(&account_id, ProbeTrigger::Manual).await {
                                Ok(usage) => ControlResult::Usage(usage),
                                Err(error) => ControlResult::Error(probe_control_error(error)),
                            }
                        }
                        None => ControlResult::Error(ControlError::AccountSelectorNotFound {
                            provider,
                            account_label: query.account_label,
                        }),
                    }
                }
            }
            ControlCommand::StartAuth {
                provider,
                account,
                request,
            } => match prepare_auth_start_request(request, transport) {
                Ok(request) => match self.provider_for_account(&provider, &account).await {
                    Ok(provider) => match provider.start_auth(request).await {
                        Ok(challenge) => ControlResult::AuthChallenge(challenge),
                        Err(error) => {
                            diagnostic = provider_error_diagnostic(diagnostics, &error);
                            ControlResult::Error(sanitize_provider_error(error).into())
                        }
                    },
                    Err(error) => ControlResult::Error(error),
                },
                Err(error) => ControlResult::Error(error),
            },
            ControlCommand::CompleteAuth {
                provider,
                account,
                request,
            } => match self.provider_for_account(&provider, &account).await {
                Ok(instance) => match instance.complete_auth(request).await {
                    Ok(state) => ControlResult::AuthState(sanitize_auth_state(diagnostics, state)),
                    Err(error) => {
                        diagnostic = provider_error_diagnostic(diagnostics, &error);
                        ControlResult::Error(sanitize_provider_error(error).into())
                    }
                },
                Err(error) => ControlResult::Error(error),
            },
            ControlCommand::AuthStatus { provider, account } => {
                match self.provider_for_account(&provider, &account).await {
                    Ok(provider) => match provider.auth_status().await {
                        Ok(state) => {
                            ControlResult::AuthState(sanitize_auth_state(diagnostics, state))
                        }
                        Err(error) => {
                            diagnostic = provider_error_diagnostic(diagnostics, &error);
                            ControlResult::Error(sanitize_provider_error(error).into())
                        }
                    },
                    Err(error) => ControlResult::Error(error),
                }
            }
            ControlCommand::Logout {
                provider,
                account,
                request,
            } => match self.provider_for_account(&provider, &account).await {
                Ok(provider) => match provider.logout(request).await {
                    Ok(()) => ControlResult::Ack,
                    Err(error) => {
                        diagnostic = provider_error_diagnostic(diagnostics, &error);
                        ControlResult::Error(sanitize_provider_error(error).into())
                    }
                },
                Err(error) => ControlResult::Error(error),
            },
            ControlCommand::ListWorkspaces { provider, account } => {
                match self.provider_for_account(&provider, &account).await {
                    Ok(provider) => match provider.list_workspaces().await {
                        Ok(workspaces) => ControlResult::Workspaces(workspaces),
                        Err(error) => ControlResult::Error(sanitize_provider_error(error).into()),
                    },
                    Err(error) => ControlResult::Error(error),
                }
            }
            ControlCommand::SelectWorkspace {
                provider,
                account,
                workspace_id,
            } => match self.provider_for_account(&provider, &account).await {
                Ok(instance) => match instance.select_workspace(&workspace_id).await {
                    Ok(workspace) => ControlResult::Workspace(workspace),
                    Err(error) => ControlResult::Error(sanitize_provider_error(error).into()),
                },
                Err(error) => ControlResult::Error(error),
            },
            ControlCommand::RetireDuplicateAccounts { provider, account } => {
                let gate = self.auth_gate(&provider).await;
                let _gate = gate.lock().await;
                match self.provider_for_account(&provider, &account).await {
                    Ok(instance) => match instance.auth_status().await {
                        Ok(state) => ControlResult::Accounts(
                            self.retire_duplicates(&provider, &account, &state).await,
                        ),
                        Err(error) => ControlResult::Error(sanitize_provider_error(error).into()),
                    },
                    Err(error) => ControlResult::Error(error),
                }
            }
            ControlCommand::AddAccount { provider, label } => {
                if !self.engine.registry().contains(&provider) {
                    let error = ullage_core::RegistryError::NotFound(provider.clone());
                    ControlResult::Error(error.into())
                } else {
                    let id = self.engine.next_account_id().await;
                    let config = AccountConfig {
                        id: id.clone(),
                        provider,
                        query: UsageQuery {
                            account_label: label,
                        },
                        enabled: true,
                        interval: CONTROL_ACCOUNT_INTERVAL,
                        timeout: CONTROL_ACCOUNT_TIMEOUT,
                        jitter: CONTROL_ACCOUNT_JITTER,
                        backoff: BackoffConfig::default(),
                        metrics: Vec::new(),
                    };
                    match self.engine.add_account(config.clone()).await {
                        Ok(()) => ControlResult::Account(account_payload(config)),
                        Err(error) => ControlResult::Error(account_control_error(error, id)),
                    }
                }
            }
            ControlCommand::ListAccounts => ControlResult::Accounts(
                self.engine
                    .account_configs()
                    .await
                    .into_iter()
                    .map(account_payload)
                    .collect(),
            ),
            ControlCommand::ShowAccount { account } => {
                let id = AccountId::new(account.as_str());
                match self.engine.account_config(&id).await {
                    Some(config) => ControlResult::Account(account_payload(config)),
                    None => ControlResult::Error(AccountError::NotFound(account).into()),
                }
            }
            ControlCommand::SetAccountEnabled { account, enabled } => {
                let id = AccountId::new(account.as_str());
                match self.engine.set_account_enabled(&id, enabled).await {
                    Ok(Some(config)) => ControlResult::Account(account_payload(config)),
                    Ok(None) => ControlResult::Error(AccountError::NotFound(account).into()),
                    Err(DaemonError::Storage(_)) => ControlResult::Error(ControlError::Storage),
                    Err(error) => ControlResult::Error(account_control_error(error, id)),
                }
            }
            ControlCommand::SetAccountLabel { account, label } => {
                let id = AccountId::new(account.as_str());
                match self.engine.set_account_label(&id, label).await {
                    Ok(Some(config)) => ControlResult::Account(account_payload(config)),
                    Ok(None) => ControlResult::Error(AccountError::NotFound(account).into()),
                    Err(DaemonError::Storage(_)) => ControlResult::Error(ControlError::Storage),
                    Err(error) => ControlResult::Error(account_control_error(error, id)),
                }
            }
            ControlCommand::SetAccountMetrics { account, metrics } => {
                let id = AccountId::new(account.as_str());
                match self.engine.set_account_metrics(&id, metrics).await {
                    Ok(Some(config)) => ControlResult::Account(account_payload(config)),
                    Ok(None) => ControlResult::Error(AccountError::NotFound(account).into()),
                    Err(error) => ControlResult::Error(account_control_error(error, id)),
                }
            }
            ControlCommand::RemoveAccount { account } => {
                let id = AccountId::new(account.as_str());
                match self.engine.remove_account(&id).await {
                    Ok(Some(_)) => ControlResult::Ack,
                    Ok(None) => ControlResult::Error(AccountError::NotFound(account).into()),
                    Err(DaemonError::Storage(_)) => ControlResult::Error(ControlError::Storage),
                    Err(error) => ControlResult::Error(account_control_error(error, id)),
                }
            }
        };
        if !matches!(result, ControlResult::Error(_)) {
            diagnostic = None;
        }
        ControlResponse {
            version: CONTROL_PROTOCOL_VERSION,
            request_id,
            result,
            diagnostic,
        }
    }
}

/// Original provider error text, kept out of the response unless the caller
/// explicitly opted into diagnostics.
fn provider_error_diagnostic(
    diagnostics: bool,
    error: &ullage_core::ProviderError,
) -> Option<String> {
    diagnostics.then(|| error.to_string())
}

fn probe_error_diagnostic(diagnostics: bool, error: &ProbeError) -> Option<String> {
    match error {
        ProbeError::Provider(error) => provider_error_diagnostic(diagnostics, error),
        _ => None,
    }
}

/// The identity an authentication state names, trimmed and non-empty. An
/// expired or rejected credential still belongs to the account that created it,
/// so `Invalid` counts here as well as `Authenticated`.
fn identity_of(state: &ullage_auth::AuthState) -> Option<String> {
    let key = match state {
        ullage_auth::AuthState::Authenticated { account_key, .. }
        | ullage_auth::AuthState::Invalid { account_key, .. } => account_key.as_deref()?,
        _ => return None,
    };
    let key = key.trim();
    (!key.is_empty()).then(|| key.to_owned())
}

/// `AuthState::Invalid.reason` is provider text. Keep it only when this call
/// opted into diagnostics; otherwise replace it with the stable sanitized kind.
fn sanitize_auth_state(diagnostics: bool, state: ullage_auth::AuthState) -> ullage_auth::AuthState {
    match state {
        ullage_auth::AuthState::Invalid { .. } if !diagnostics => ullage_auth::AuthState::Invalid {
            reason: "provider authentication is invalid".into(),
            account_key: None,
        },
        state => state,
    }
}

#[cfg(unix)]
pub struct UnixControlServer {
    listener: tokio::net::UnixListener,
    path: PathBuf,
    identity: SocketIdentity,
    service: ControlService,
}

#[cfg(unix)]
#[derive(Clone, Copy)]
struct SocketIdentity {
    device: u64,
    inode: u64,
}

#[cfg(unix)]
impl UnixControlServer {
    pub async fn bind(path: impl Into<PathBuf>, service: ControlService) -> Result<Self, String> {
        use std::os::unix::fs::{MetadataExt, PermissionsExt};

        let path = path.into();
        let parent = path
            .parent()
            .ok_or_else(|| "control socket path has no parent directory".to_owned())?;
        ensure_private_directory(parent).await?;
        recover_stale_socket(&path).await?;
        let listener = tokio::net::UnixListener::bind(&path).map_err(|error| error.to_string())?;
        let metadata = tokio::fs::symlink_metadata(&path)
            .await
            .map_err(|error| error.to_string())?;
        let identity = SocketIdentity {
            device: metadata.dev(),
            inode: metadata.ino(),
        };
        if let Err(error) =
            tokio::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o600)).await
        {
            drop(listener);
            let _ = remove_owned_socket(&path, identity).await;
            return Err(error.to_string());
        }
        Ok(Self {
            listener,
            path,
            identity,
            service,
        })
    }

    pub async fn run(self) -> Result<(), String> {
        let mut tasks = tokio::task::JoinSet::new();
        let mut accept_error = None;
        loop {
            tokio::select! {
                accepted = self.listener.accept() => {
                    let (stream, _) = match accepted {
                        Ok(accepted) => accepted,
                        Err(error) => {
                            accept_error = Some(error.to_string());
                            break;
                        }
                    };
                    let service = self.service.clone();
                    tasks.spawn(async move {
                        let _ = handle_connection(stream, service).await;
                    });
                }
                _ = self.service.engine.wait_for_shutdown() => break,
                _ = tasks.join_next(), if !tasks.is_empty() => {}
            }
        }
        tasks.abort_all();
        while tasks.join_next().await.is_some() {}
        if accept_error.is_none() {
            self.service.engine.wait_for_idle().await;
        }
        let cleanup = remove_owned_socket(&self.path, self.identity).await;
        match (accept_error, cleanup) {
            (None, result) => result,
            (Some(error), Ok(())) => Err(error),
            (Some(error), Err(cleanup_error)) => {
                Err(format!("{error}; socket cleanup failed: {cleanup_error}"))
            }
        }
    }

    pub fn path(&self) -> &Path {
        &self.path
    }
}

#[cfg(unix)]
async fn handle_connection(
    stream: tokio::net::UnixStream,
    service: ControlService,
) -> Result<(), String> {
    handle_stream(stream, service).await
}

async fn handle_stream<S>(stream: S, service: ControlService) -> Result<(), String>
where
    S: tokio::io::AsyncRead + tokio::io::AsyncWrite + Unpin,
{
    use tokio::io::{AsyncBufReadExt, AsyncReadExt, AsyncWriteExt, BufReader};

    const MAXIMUM_REQUEST_BYTES: u64 = 1024 * 1024;
    let (reader, mut writer) = tokio::io::split(stream);
    let mut bytes = Vec::new();
    let read = BufReader::new(reader)
        .take(MAXIMUM_REQUEST_BYTES + 1)
        .read_until(b'\n', &mut bytes)
        .await
        .map_err(|error| error.to_string())?;
    if read == 0 || read as u64 > MAXIMUM_REQUEST_BYTES || !bytes.ends_with(b"\n") {
        return Err("invalid control request framing".into());
    }
    #[derive(Deserialize)]
    struct ControlEnvelope {
        version: u16,
        request_id: String,
    }

    let envelope: ControlEnvelope =
        serde_json::from_slice(&bytes).map_err(|error| error.to_string())?;
    let response = if envelope.version != CONTROL_PROTOCOL_VERSION {
        ControlResponse {
            version: CONTROL_PROTOCOL_VERSION,
            request_id: envelope.request_id,
            result: ControlResult::ProtocolMismatch {
                supported_version: CONTROL_PROTOCOL_VERSION,
            },
            diagnostic: None,
        }
    } else {
        let request: ControlRequest =
            serde_json::from_slice(&bytes).map_err(|error| error.to_string())?;
        service.handle(request).await
    };
    let mut encoded = serde_json::to_vec(&response).map_err(|error| error.to_string())?;
    encoded.push(b'\n');
    writer
        .write_all(&encoded)
        .await
        .map_err(|error| error.to_string())?;
    writer.shutdown().await.map_err(|error| error.to_string())
}

#[cfg(windows)]
pub struct WindowsControlServer {
    pipe_name: PathBuf,
    first: tokio::net::windows::named_pipe::NamedPipeServer,
    service: ControlService,
}

#[cfg(windows)]
impl WindowsControlServer {
    pub fn bind(pipe_name: impl Into<PathBuf>, service: ControlService) -> Result<Self, String> {
        let pipe_name = pipe_name.into();
        let first = create_private_pipe(&pipe_name, true)?;
        Ok(Self {
            pipe_name,
            first,
            service,
        })
    }

    pub async fn run(self) -> Result<(), String> {
        let mut current = self.first;
        let mut tasks = tokio::task::JoinSet::new();
        loop {
            tokio::select! {
                connected = current.connect() => {
                    connected.map_err(|error| error.to_string())?;
                    let next = create_private_pipe(&self.pipe_name, false)?;
                    let connected = std::mem::replace(&mut current, next);
                    let service = self.service.clone();
                    tasks.spawn(async move {
                        let _ = handle_stream(connected, service).await;
                    });
                }
                _ = self.service.engine.wait_for_shutdown() => break,
                _ = tasks.join_next(), if !tasks.is_empty() => {}
            }
        }
        tasks.abort_all();
        while tasks.join_next().await.is_some() {}
        self.service.engine.wait_for_idle().await;
        Ok(())
    }
}

#[cfg(windows)]
fn create_private_pipe(
    pipe_name: &Path,
    first: bool,
) -> Result<tokio::net::windows::named_pipe::NamedPipeServer, String> {
    use std::os::windows::ffi::OsStrExt;
    use windows_sys::Win32::Foundation::LocalFree;
    use windows_sys::Win32::Security::Authorization::{
        ConvertStringSecurityDescriptorToSecurityDescriptorW, SDDL_REVISION_1,
    };
    use windows_sys::Win32::Security::{PSECURITY_DESCRIPTOR, SECURITY_ATTRIBUTES};

    let sddl = std::ffi::OsStr::new("D:P(A;;GA;;;OW)")
        .encode_wide()
        .chain(Some(0))
        .collect::<Vec<_>>();
    let mut descriptor: PSECURITY_DESCRIPTOR = std::ptr::null_mut();
    // SAFETY: the SDDL string is NUL-terminated and the output pointer is valid.
    if unsafe {
        ConvertStringSecurityDescriptorToSecurityDescriptorW(
            sddl.as_ptr(),
            SDDL_REVISION_1,
            &mut descriptor,
            std::ptr::null_mut(),
        )
    } == 0
    {
        return Err("cannot create private pipe security descriptor".into());
    }
    let mut attributes = SECURITY_ATTRIBUTES {
        nLength: std::mem::size_of::<SECURITY_ATTRIBUTES>() as u32,
        lpSecurityDescriptor: descriptor,
        bInheritHandle: 0,
    };
    let mut options = tokio::net::windows::named_pipe::ServerOptions::new();
    options.first_pipe_instance(first);
    // SAFETY: attributes and descriptor remain valid for the duration of the create call.
    let created = unsafe {
        options.create_with_security_attributes_raw(
            pipe_name.as_os_str(),
            (&mut attributes as *mut SECURITY_ATTRIBUTES).cast(),
        )
    };
    // SAFETY: the descriptor was allocated by the conversion function.
    unsafe { LocalFree(descriptor) };
    created.map_err(|error| error.to_string())
}

#[cfg(unix)]
async fn ensure_private_directory(path: &Path) -> Result<(), String> {
    use std::io::ErrorKind;
    use std::os::unix::fs::{DirBuilderExt, MetadataExt, PermissionsExt};

    let metadata = match tokio::fs::symlink_metadata(path).await {
        Ok(metadata) => metadata,
        Err(error) if error.kind() == ErrorKind::NotFound => {
            let mut builder = std::fs::DirBuilder::new();
            builder.mode(0o700);
            builder.create(path).map_err(|error| error.to_string())?;
            tokio::fs::symlink_metadata(path)
                .await
                .map_err(|error| error.to_string())?
        }
        Err(error) => return Err(error.to_string()),
    };
    if metadata.file_type().is_symlink() || !metadata.is_dir() {
        return Err("control socket parent must be a real directory".into());
    }
    if metadata.uid() != current_user_id() {
        return Err("control socket parent is not owned by the current user".into());
    }
    if metadata.permissions().mode() & 0o077 != 0 {
        return Err("control socket parent permissions allow access by other users".into());
    }
    Ok(())
}

#[cfg(unix)]
async fn recover_stale_socket(path: &Path) -> Result<(), String> {
    use std::io::ErrorKind;
    use std::os::unix::fs::{FileTypeExt, MetadataExt};

    let metadata = match tokio::fs::symlink_metadata(path).await {
        Ok(metadata) => metadata,
        Err(error) if error.kind() == ErrorKind::NotFound => return Ok(()),
        Err(error) => return Err(error.to_string()),
    };
    if !metadata.file_type().is_socket() || metadata.uid() != current_user_id() {
        return Err("control socket path is not a current-user socket".into());
    }
    let identity = SocketIdentity {
        device: metadata.dev(),
        inode: metadata.ino(),
    };
    match tokio::net::UnixStream::connect(path).await {
        Ok(_) => return Err("control socket is already active".into()),
        Err(error) if error.kind() == ErrorKind::ConnectionRefused => {}
        Err(error) if error.kind() == ErrorKind::NotFound => return Ok(()),
        Err(error) => return Err(format!("cannot verify existing control socket: {error}")),
    }
    remove_owned_socket(path, identity).await
}

#[cfg(unix)]
fn current_user_id() -> u32 {
    // SAFETY: `geteuid` has no arguments and no memory-safety preconditions.
    unsafe { libc::geteuid() }
}

#[cfg(unix)]
async fn remove_owned_socket(path: &Path, identity: SocketIdentity) -> Result<(), String> {
    use std::os::unix::fs::{FileTypeExt, MetadataExt};

    let metadata = tokio::fs::symlink_metadata(path)
        .await
        .map_err(|error| error.to_string())?;
    if !metadata.file_type().is_socket()
        || metadata.dev() != identity.device
        || metadata.ino() != identity.inode
    {
        return Err("control socket changed before cleanup".into());
    }
    tokio::fs::remove_file(path)
        .await
        .map_err(|error| error.to_string())
}
