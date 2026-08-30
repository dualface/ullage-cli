use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::Duration;

use serde::Deserialize;
use ullage_core::{RegisteredProvider, SubscriptionUsage, UsageQuery};
use ullage_protocol::{
    Account, AccountError, AccountId as ProtocolAccountId, AccountStatusPayload,
    CONTROL_PROTOCOL_VERSION, ControlCommand, ControlError, ControlRequest, ControlResponse,
    ControlResult, CredentialBackendId, DaemonStatusPayload, ProbePayload, SanitizedErrorPayload,
    SnapshotPayload,
};

use crate::model::sanitize_provider_error;
use crate::{
    AccountConfig, AccountId, BackoffConfig, DaemonEngine, DaemonError, DaemonStatus, ProbeError,
    ProbeTrigger, SanitizedError, SnapshotRecord,
};

const CONTROL_ACCOUNT_INTERVAL: Duration = Duration::from_secs(5 * 60);
const CONTROL_ACCOUNT_TIMEOUT: Duration = Duration::from_secs(30);
const CONTROL_ACCOUNT_JITTER: Duration = Duration::from_secs(5);

#[derive(Clone)]
pub struct ControlService {
    engine: DaemonEngine,
    credential_backend: CredentialBackendId,
}

impl ControlService {
    pub fn new(engine: DaemonEngine) -> Self {
        Self {
            engine,
            credential_backend: CredentialBackendId::native(),
        }
    }

    #[must_use]
    pub fn with_credential_backend(mut self, credential_backend: CredentialBackendId) -> Self {
        self.credential_backend = credential_backend;
        self
    }

    pub async fn daemon_status(&self) -> DaemonStatus {
        self.engine.status().await
    }

    pub async fn probe(
        &self,
        account_id: &AccountId,
    ) -> Result<ullage_core::QueryOutcome<SubscriptionUsage>, ProbeError> {
        self.engine.probe(account_id, ProbeTrigger::Manual).await
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

    pub async fn handle(&self, request: ControlRequest) -> ControlResponse {
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
                        Ok(snapshot) => ControlResult::Snapshots(
                            snapshot.into_iter().map(snapshot_payload).collect(),
                        ),
                    }
                }
                None => ControlResult::Snapshots(
                    self.engine
                        .show_all()
                        .await
                        .into_iter()
                        .map(snapshot_payload)
                        .collect(),
                ),
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
            } => match self.provider_for_account(&provider, &account).await {
                Ok(provider) => match provider.start_auth(request).await {
                    Ok(challenge) => ControlResult::AuthChallenge(challenge),
                    Err(error) => {
                        diagnostic = provider_error_diagnostic(diagnostics, &error);
                        ControlResult::Error(sanitize_provider_error(error).into())
                    }
                },
                Err(error) => ControlResult::Error(error),
            },
            ControlCommand::CompleteAuth {
                provider,
                account,
                request,
            } => match self.provider_for_account(&provider, &account).await {
                Ok(provider) => match provider.complete_auth(request).await {
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
                Ok(provider) => match provider.select_workspace(&workspace_id).await {
                    Ok(workspace) => ControlResult::Workspace(workspace),
                    Err(error) => ControlResult::Error(sanitize_provider_error(error).into()),
                },
                Err(error) => ControlResult::Error(error),
            },
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

/// `AuthState::Invalid.reason` is provider text. Keep it only when this call
/// opted into diagnostics; otherwise replace it with the stable sanitized kind.
fn sanitize_auth_state(diagnostics: bool, state: ullage_auth::AuthState) -> ullage_auth::AuthState {
    match state {
        ullage_auth::AuthState::Invalid { .. } if !diagnostics => ullage_auth::AuthState::Invalid {
            reason: "provider authentication is invalid".into(),
        },
        state => state,
    }
}

fn account_payload(config: AccountConfig) -> Account {
    Account {
        id: ProtocolAccountId::new(config.id.to_string()),
        provider: config.provider,
        label: config.query.account_label,
        enabled: config.enabled,
    }
}

fn account_control_error(error: DaemonError, requested: AccountId) -> ControlError {
    match error {
        DaemonError::DuplicateAccount(account) => {
            AccountError::Duplicate(ProtocolAccountId::new(account.to_string())).into()
        }
        DaemonError::DuplicateAccountSelector { .. } => {
            AccountError::Duplicate(ProtocolAccountId::new(requested.to_string())).into()
        }
        DaemonError::Storage(_) => ControlError::Storage,
        DaemonError::Cancelled => ControlError::Cancelled,
        DaemonError::InvalidGlobalConcurrency
        | DaemonError::InvalidProviderConcurrency
        | DaemonError::InvalidProviderLimit(_)
        | DaemonError::InvalidInterval(_)
        | DaemonError::InvalidTimeout(_)
        | DaemonError::InvalidBackoff(_) => ControlError::UnsupportedCommand,
    }
}

fn probe_control_error(error: ProbeError) -> ControlError {
    match error {
        ProbeError::Provider(error) => ControlError::Provider(sanitize_provider_error(error)),
        ProbeError::Registry(error) => ControlError::Registry(error),
        ProbeError::AccountNotFound(account_id) => ControlError::AccountNotFound {
            account_id: account_id.to_string(),
        },
        ProbeError::Timeout => ControlError::Timeout,
        ProbeError::Cancelled => ControlError::Cancelled,
        ProbeError::Storage(_) => ControlError::Storage,
    }
}

fn status_payload(
    status: DaemonStatus,
    credential_backend: CredentialBackendId,
) -> DaemonStatusPayload {
    DaemonStatusPayload {
        shutting_down: status.shutting_down,
        credential_backend,
        accounts: status
            .accounts
            .into_iter()
            .map(|account| AccountStatusPayload {
                account_id: account.id.to_string(),
                provider: account.provider,
                enabled: account.enabled,
                in_flight: account.in_flight,
                consecutive_failures: account.consecutive_failures,
                next_probe_at: account.next_probe_at,
                has_snapshot: account.has_snapshot,
                stale: account.stale,
                last_error: account.last_error.map(sanitized_error_payload),
            })
            .collect(),
    }
}

fn snapshot_payload(snapshot: SnapshotRecord) -> SnapshotPayload {
    SnapshotPayload {
        account_id: snapshot.account_id.to_string(),
        usage: snapshot.usage,
        last_success_at: snapshot.last_success_at,
        stale: snapshot.stale,
        last_error: snapshot.last_error.map(sanitized_error_payload),
        last_error_at: snapshot.last_error_at,
    }
}

fn sanitized_error_payload(error: SanitizedError) -> SanitizedErrorPayload {
    match error {
        SanitizedError::AuthenticationInvalid => SanitizedErrorPayload::AuthenticationInvalid,
        SanitizedError::RateLimited {
            retry_after_seconds,
        } => SanitizedErrorPayload::RateLimited {
            retry_after_seconds,
        },
        SanitizedError::Network => SanitizedErrorPayload::Network,
        SanitizedError::ProtocolIncompatible => SanitizedErrorPayload::ProtocolIncompatible,
        SanitizedError::UnsupportedCapability => SanitizedErrorPayload::UnsupportedCapability,
        SanitizedError::Timeout => SanitizedErrorPayload::Timeout,
        SanitizedError::Cancelled => SanitizedErrorPayload::Cancelled,
        SanitizedError::ProviderNotFound => SanitizedErrorPayload::ProviderNotFound,
        SanitizedError::Storage => SanitizedErrorPayload::Storage,
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
