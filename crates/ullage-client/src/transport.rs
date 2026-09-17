//! The system [`ControlClient`]: endpoint discovery, socket/pipe I/O, peer
//! validation, daemon readiness, and detached daemon launching.
//!
//! The control endpoint is a private boundary: a Unix socket must be owned by
//! the current user with no group/other bits, a Windows pipe must be local and
//! its server must run as the same user, and every reply is bounded by
//! [`MAX_RESPONSE_BYTES`] and a per-command deadline.

use std::ffi::OsString;
use std::io::Read as _;
#[cfg(windows)]
use std::io::{BufRead, BufReader};
use std::path::{Path, PathBuf};
use std::process::Command as ProcessCommand;
use std::process::Stdio;
use std::sync::atomic::Ordering;
#[cfg(any(unix, windows))]
use std::time::Duration;

use ullage_protocol::{
    CONTROL_PROTOCOL_VERSION, ControlCommand, ControlRequest, ControlResponse, ControlResult,
    DaemonStatusPayload,
};

use crate::errors::sanitize_cli_text;
use crate::validate::{daemon_status_payload_is_well_formed, result_contains_unsafe_control};
use crate::{
    ClientError, ControlClient, REQUEST_SEQUENCE, ServiceAction, next_request_id, service,
};

const MAX_RESPONSE_BYTES: u64 = 1024 * 1024;

pub struct SystemClient {
    endpoint: Option<PathBuf>,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum DaemonReadiness {
    Ready,
    Unavailable,
    NotReady,
}

fn classify_readiness_response(
    response: &ControlResponse,
    request_id: &str,
) -> Result<DaemonReadiness, ClientError> {
    if response.request_id != request_id {
        return Err(ClientError::InvalidResponse);
    }
    if response.version == CONTROL_PROTOCOL_VERSION
        && !result_contains_unsafe_control(&response.result)
        && matches!(
            &response.result,
            ControlResult::DaemonStatus(status)
                if !status.shutting_down && daemon_status_payload_is_well_formed(status)
        )
    {
        Ok(DaemonReadiness::Ready)
    } else if matches!(&response.result, ControlResult::ProtocolMismatch { .. })
        || (response.version == CONTROL_PROTOCOL_VERSION
            && matches!(
                &response.result,
                ControlResult::DaemonStatus(DaemonStatusPayload {
                    shutting_down: true,
                    ..
                })
            ))
    {
        Ok(DaemonReadiness::NotReady)
    } else {
        Err(ClientError::InvalidResponse)
    }
}

impl SystemClient {
    pub fn from_environment() -> Self {
        #[cfg(unix)]
        let endpoint = std::env::var_os("ULLAGE_CONTROL_SOCKET")
            .map(PathBuf::from)
            .or_else(|| {
                std::env::var_os("XDG_RUNTIME_DIR")
                    .map(PathBuf::from)
                    .map(|runtime| runtime.join("ullage/control.sock"))
            })
            .or_else(|| Some(default_unix_control_socket()));
        #[cfg(windows)]
        let endpoint = std::env::var_os("ULLAGE_CONTROL_PIPE")
            .map(PathBuf::from)
            .or_else(|| {
                ullage_auth::current_windows_user_scope()
                    .ok()
                    .map(|scope| PathBuf::from(format!(r"\\.\pipe\ullage-{scope}")))
            })
            .filter(|path| is_local_windows_pipe(path));
        Self { endpoint }
    }

    fn daemon_readiness(&self, timeout: Duration) -> Result<DaemonReadiness, ClientError> {
        let request_id = next_request_id();
        let request = ControlRequest::new(&request_id, ControlCommand::DaemonStatus);
        let response = match self.send_readiness(&request, timeout) {
            Ok(response) => response,
            Err(ClientError::DaemonUnavailable) => return Ok(DaemonReadiness::Unavailable),
            Err(error) => return Err(error),
        };
        classify_readiness_response(&response, &request_id)
    }

    /// `NotReady` — the daemon is draining or answered `ProtocolMismatch` — is
    /// a transient state callers retry inside their existing budget, not a
    /// broken reply.
    fn daemon_is_ready(&self, timeout: Duration) -> Result<bool, ClientError> {
        match self.daemon_readiness(timeout)? {
            DaemonReadiness::Ready => Ok(true),
            DaemonReadiness::Unavailable | DaemonReadiness::NotReady => Ok(false),
        }
    }

    /// Waits out a draining or protocol-mismatched daemon that still holds the
    /// endpoint inside `deadline`. A child spawned while the endpoint is taken
    /// exits on "already active" instead of becoming ready. Returns `true`
    /// when the existing daemon already serves requests, `false` once the
    /// endpoint is free to spawn on.
    fn wait_for_endpoint(&self, deadline: std::time::Instant) -> Result<bool, ClientError> {
        loop {
            let remaining = deadline
                .saturating_duration_since(std::time::Instant::now())
                .min(Duration::from_millis(250));
            match self.daemon_readiness(remaining)? {
                DaemonReadiness::Ready => return Ok(true),
                DaemonReadiness::Unavailable => return Ok(false),
                DaemonReadiness::NotReady => {
                    if std::time::Instant::now() >= deadline {
                        return Err(ClientError::DaemonStillRunning);
                    }
                    std::thread::sleep(Duration::from_millis(25));
                }
            }
        }
    }

    fn manage_service(&self, action: ServiceAction) -> Result<(), ClientError> {
        if action == ServiceAction::Start {
            if !service::installed().map_err(|_| ClientError::DaemonProcess)? {
                return Err(ClientError::DaemonProcess);
            }
            if self.wait_for_endpoint(std::time::Instant::now() + Duration::from_secs(5))? {
                return Ok(());
            }
        }
        if matches!(action, ServiceAction::Stop | ServiceAction::Uninstall) {
            let stop_issued = service::stop().map_err(|_| ClientError::DaemonProcess)?;
            if stop_issued {
                self.wait_for_service_stopped()?;
            } else {
                // The service stop removed nothing: a `daemon run` instance it
                // does not manage may still answer. Success is only honest
                // when the control endpoint is actually gone.
                match self.daemon_readiness(Duration::from_millis(250))? {
                    DaemonReadiness::Unavailable => {}
                    DaemonReadiness::Ready | DaemonReadiness::NotReady => {
                        return Err(ClientError::DaemonStillRunning);
                    }
                }
            }
            if action == ServiceAction::Stop {
                return Ok(());
            }
            return service::manage(ServiceAction::Uninstall)
                .map_err(|_| ClientError::DaemonProcess);
        }
        service::manage(action).map_err(|_| ClientError::DaemonProcess)?;
        match action {
            ServiceAction::Start => self.wait_for_service_ready(),
            ServiceAction::Install => Ok(()),
            ServiceAction::Stop | ServiceAction::Uninstall => unreachable!(),
        }
    }

    fn wait_for_service_ready(&self) -> Result<(), ClientError> {
        let deadline = std::time::Instant::now() + DAEMON_STARTUP_TIMEOUT;
        loop {
            let remaining = deadline.saturating_duration_since(std::time::Instant::now());
            if self.daemon_is_ready(remaining.min(Duration::from_millis(250)))? {
                return Ok(());
            }
            if remaining.is_zero() {
                return Err(ClientError::DaemonProcess);
            }
            std::thread::sleep(Duration::from_millis(25));
        }
    }

    fn wait_for_service_stopped(&self) -> Result<(), ClientError> {
        let deadline = std::time::Instant::now() + Duration::from_secs(5);
        loop {
            let remaining = deadline.saturating_duration_since(std::time::Instant::now());
            if self.daemon_readiness(remaining.min(Duration::from_millis(250)))?
                == DaemonReadiness::Unavailable
            {
                return Ok(());
            }
            if remaining.is_zero() {
                return Err(ClientError::DaemonProcess);
            }
            std::thread::sleep(Duration::from_millis(25));
        }
    }

    #[cfg(unix)]
    fn send_readiness(
        &self,
        request: &ControlRequest,
        timeout: Duration,
    ) -> Result<ControlResponse, ClientError> {
        self.send_unix(request, timeout.max(Duration::from_millis(1)))
    }

    #[cfg(unix)]
    fn send_unix(
        &self,
        request: &ControlRequest,
        timeout: Duration,
    ) -> Result<ControlResponse, ClientError> {
        use std::os::unix::net::UnixStream;

        let deadline = std::time::Instant::now() + timeout;
        let endpoint = self
            .endpoint
            .as_ref()
            .ok_or(ClientError::DaemonUnavailable)?;
        validate_private_unix_socket(endpoint)?;
        let mut stream =
            UnixStream::connect(endpoint).map_err(|_| ClientError::DaemonUnavailable)?;
        validate_unix_peer(&stream)?;
        let write_timeout = deadline
            .saturating_duration_since(std::time::Instant::now())
            .max(Duration::from_millis(1))
            .min(Duration::from_secs(5));
        stream
            .set_write_timeout(Some(write_timeout))
            .map_err(|_| ClientError::DaemonUnavailable)?;
        write_request(&mut stream, request)?;

        let mut encoded = Vec::new();
        while !encoded.ends_with(b"\n") && encoded.len() as u64 <= MAX_RESPONSE_BYTES {
            let remaining = deadline.saturating_duration_since(std::time::Instant::now());
            if remaining.is_zero() {
                return Err(ClientError::DaemonUnavailable);
            }
            stream
                .set_read_timeout(Some(remaining))
                .map_err(|_| ClientError::DaemonUnavailable)?;
            let remaining = (MAX_RESPONSE_BYTES + 1).saturating_sub(encoded.len() as u64) as usize;
            let mut chunk = [0; 8192];
            let chunk_len = chunk.len().min(remaining);
            let read = stream
                .read(&mut chunk[..chunk_len])
                .map_err(|_| ClientError::DaemonUnavailable)?;
            if read == 0 {
                break;
            }
            encoded.extend_from_slice(&chunk[..read]);
        }
        if !encoded.ends_with(b"\n") || encoded.len() as u64 > MAX_RESPONSE_BYTES {
            return Err(ClientError::InvalidResponse);
        }
        serde_json::from_slice(&encoded).map_err(|_| ClientError::InvalidResponse)
    }

    #[cfg(windows)]
    fn send_readiness(
        &self,
        request: &ControlRequest,
        timeout: Duration,
    ) -> Result<ControlResponse, ClientError> {
        let endpoint = self
            .endpoint
            .as_ref()
            .ok_or(ClientError::DaemonUnavailable)?;
        send_windows_pipe_with_deadline(endpoint, request, std::time::Instant::now() + timeout)
    }
}

#[cfg(unix)]
fn default_unix_control_socket() -> PathBuf {
    // SAFETY: `geteuid` has no arguments and no memory-safety preconditions.
    let user_id = unsafe { libc::geteuid() };
    std::env::temp_dir()
        .join(format!("ullage-{user_id}"))
        .join("control.sock")
}

#[cfg(unix)]
impl ControlClient for SystemClient {
    fn send(&self, request: &ControlRequest) -> Result<ControlResponse, ClientError> {
        self.send_unix(request, send_timeout(request))
    }

    fn run_daemon(&self) -> Result<(), ClientError> {
        let (executable, arguments) = daemon_command(false)?;
        let deadline = std::time::Instant::now() + Duration::from_secs(5);
        if self.wait_for_endpoint(deadline)? {
            return Ok(());
        }
        let (stderr_log, stderr_sink) = daemon_error_log()?;
        run_daemon_process(
            executable,
            &arguments,
            stderr_log,
            stderr_sink,
            DAEMON_STARTUP_TIMEOUT,
            |remaining| self.daemon_is_ready(remaining),
        )
    }

    fn manage_daemon(&self, action: ServiceAction) -> Result<(), ClientError> {
        self.manage_service(action)
    }

    fn daemon_service_installed(&self) -> Result<bool, ClientError> {
        service::installed().map_err(|_| ClientError::DaemonProcess)
    }
}

#[cfg(windows)]
impl ControlClient for SystemClient {
    fn send(&self, request: &ControlRequest) -> Result<ControlResponse, ClientError> {
        let endpoint = self
            .endpoint
            .as_ref()
            .ok_or(ClientError::DaemonUnavailable)?
            .clone();
        let timeout = send_timeout(request);
        let request = request.clone();
        let (sender, receiver) = std::sync::mpsc::sync_channel(1);
        std::thread::Builder::new()
            .name("ullage-pipe-request".into())
            .spawn(move || {
                let _ = sender.send(send_windows_pipe(&endpoint, &request));
            })
            .map_err(|_| ClientError::DaemonUnavailable)?;
        receiver
            .recv_timeout(timeout)
            .map_err(|_| ClientError::DaemonUnavailable)?
    }

    fn run_daemon(&self) -> Result<(), ClientError> {
        let (executable, arguments) = daemon_command(true)?;
        let deadline = std::time::Instant::now() + Duration::from_secs(5);
        if self.wait_for_endpoint(deadline)? {
            return Ok(());
        }
        let (stderr_log, stderr_sink) = daemon_error_log()?;
        run_daemon_process(
            executable,
            &arguments,
            stderr_log,
            stderr_sink,
            DAEMON_STARTUP_TIMEOUT,
            |remaining| self.daemon_is_ready(remaining),
        )
    }

    fn manage_daemon(&self, action: ServiceAction) -> Result<(), ClientError> {
        self.manage_service(action)
    }

    fn daemon_service_installed(&self) -> Result<bool, ClientError> {
        service::installed().map_err(|_| ClientError::DaemonProcess)
    }
}

#[cfg(windows)]
fn is_local_windows_pipe(path: &std::path::Path) -> bool {
    path.to_string_lossy()
        .to_ascii_lowercase()
        .starts_with(r"\\.\pipe\")
}

/// Longest the client waits on a `Probe { wait: true }` answer. It must outlast
/// `SIGN_IN_TIMEOUT` and `MAX_ACCOUNT_TIMEOUT` — the largest provider query
/// timeout the daemon accepts — while still bounding the read so a wedged
/// daemon cannot hang `ullage probe` or interactive login's
/// duplicate-retirement probe.
const PROBE_WAIT_TIMEOUT: Duration = Duration::from_secs(300);
const _: () =
    assert!(PROBE_WAIT_TIMEOUT.as_secs() > ullage_protocol::MAX_ACCOUNT_TIMEOUT.as_secs());

/// Longest a spawned or service-started daemon may take to answer Ready. The
/// daemon reports ready once its control plane is up — HTTP setup is
/// non-fatal background work — so this only has to cover process spawn plus
/// control-endpoint binding.
const DAEMON_STARTUP_TIMEOUT: Duration = Duration::from_secs(5);

fn send_timeout(request: &ControlRequest) -> Duration {
    if request_completes_a_sign_in(request) {
        SIGN_IN_TIMEOUT
    } else if request_waits_for_probe(request) {
        PROBE_WAIT_TIMEOUT
    } else {
        CONTROL_TIMEOUT
    }
}

/// Bytes of daemon stderr read back for a startup failure report. The sink is
/// a private log file rather than a pipe: the launching CLI exits right after
/// readiness, and a pipe drained by a thread here would break under the
/// daemon later — a late panic-hook `eprintln!` could then abort the daemon.
const DAEMON_STDERR_TAIL_BYTES: usize = 8192;

/// The private daemon stderr sink: a per-user log under the same runtime
/// directory scheme as the default control socket. Each launch gets its own
/// file — concurrent launchers must never truncate or interleave each
/// other's diagnostics.
fn daemon_error_log_name() -> String {
    // PIDs are recycled and the sequence restarts per process, so a
    // nanosecond stamp keeps names unique across process lifetimes.
    let nanos = std::time::SystemTime::now()
        .duration_since(std::time::SystemTime::UNIX_EPOCH)
        .map(|duration| duration.as_nanos())
        .unwrap_or(0);
    format!(
        "daemon-error-{}-{}-{}.log",
        std::process::id(),
        nanos,
        REQUEST_SEQUENCE.fetch_add(1, Ordering::Relaxed)
    )
}

/// A fresh log can still collide when an OS recycles a PID inside the
/// retention window and the timestamp lands identically; retrying with a new
/// name is cheap compared to failing the launch with an opaque error.
const DAEMON_ERROR_LOG_ATTEMPTS: u32 = 8;

fn create_daemon_error_log(
    directory: &Path,
    mut name: impl FnMut() -> String,
) -> Result<(PathBuf, std::fs::File), ClientError> {
    for _ in 0..DAEMON_ERROR_LOG_ATTEMPTS {
        let path = directory.join(name());
        let mut options = std::fs::OpenOptions::new();
        options.create_new(true).write(true);
        #[cfg(unix)]
        {
            use std::os::unix::fs::OpenOptionsExt;
            options.mode(0o600);
        }
        match options.open(&path) {
            Ok(file) => return Ok((path, file)),
            Err(error) if error.kind() == std::io::ErrorKind::AlreadyExists => continue,
            Err(_) => return Err(ClientError::DaemonProcess),
        }
    }
    Err(ClientError::DaemonProcess)
}

/// A launcher's log file is seconds old when it sweeps, so an age cutoff can
/// never delete a concurrent launcher's active diagnostics the way a full
/// directory sweep could.
const DAEMON_ERROR_LOG_MAX_AGE: Duration = Duration::from_secs(24 * 60 * 60);

fn sweep_daemon_error_logs(directory: &Path) {
    let cutoff = std::time::SystemTime::now()
        .checked_sub(DAEMON_ERROR_LOG_MAX_AGE)
        .unwrap_or(std::time::SystemTime::UNIX_EPOCH);
    sweep_daemon_error_logs_before(directory, cutoff);
}

fn sweep_daemon_error_logs_before(directory: &Path, cutoff: std::time::SystemTime) {
    if let Ok(entries) = std::fs::read_dir(directory) {
        for entry in entries.flatten() {
            let name = entry.file_name();
            let name = name.to_string_lossy();
            if !(name.starts_with("daemon-error-") && name.ends_with(".log")) {
                continue;
            }
            let stale = entry
                .metadata()
                .and_then(|metadata| metadata.modified())
                .map(|modified| modified < cutoff)
                .unwrap_or(false);
            if stale {
                let _ = std::fs::remove_file(entry.path());
            }
        }
    }
}

/// Log directory beside the control socket: the runtime dir is already a
/// private boundary, and only the no-runtime-dir fallback needs the per-uid
/// name under a world-writable `temp_dir()`.
#[cfg(unix)]
fn daemon_error_log_directory() -> PathBuf {
    std::env::var_os("XDG_RUNTIME_DIR")
        .map(|runtime| PathBuf::from(runtime).join("ullage"))
        .unwrap_or_else(|| {
            // SAFETY: `geteuid` has no arguments and no memory-safety preconditions.
            let user_id = unsafe { libc::geteuid() };
            std::env::temp_dir().join(format!("ullage-{user_id}"))
        })
}

/// The fallback base is world-writable, so a pre-existing entry must prove it
/// is a real directory owned by the current user with private permissions
/// before diagnostics land in it — otherwise a local neighbour could pre-create
/// or symlink the path and control what `read_log_tail` later reopens. Mirrors
/// the daemon's `ensure_private_directory` for its socket parent.
#[cfg(unix)]
fn ensure_daemon_log_directory(path: &Path) -> Result<(), ClientError> {
    use std::os::unix::fs::{DirBuilderExt, MetadataExt, PermissionsExt};

    let metadata = match std::fs::symlink_metadata(path) {
        Ok(metadata) => metadata,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => {
            std::fs::DirBuilder::new()
                .recursive(true)
                .mode(0o700)
                .create(path)
                .map_err(|_| ClientError::DaemonProcess)?;
            std::fs::symlink_metadata(path).map_err(|_| ClientError::DaemonProcess)?
        }
        Err(_) => return Err(ClientError::DaemonProcess),
    };
    // SAFETY: `geteuid` has no arguments and no memory-safety preconditions.
    let owned = metadata.uid() == unsafe { libc::geteuid() };
    if metadata.file_type().is_symlink()
        || !metadata.is_dir()
        || !owned
        || metadata.permissions().mode() & 0o077 != 0
    {
        return Err(ClientError::DaemonProcess);
    }
    Ok(())
}

#[cfg(unix)]
fn daemon_error_log() -> Result<(PathBuf, std::fs::File), ClientError> {
    let directory = daemon_error_log_directory();
    ensure_daemon_log_directory(&directory)?;
    sweep_daemon_error_logs(&directory);
    create_daemon_error_log(&directory, daemon_error_log_name)
}

#[cfg(windows)]
fn daemon_error_log() -> Result<(PathBuf, std::fs::File), ClientError> {
    let scope = ullage_auth::current_windows_user_scope()
        .map(|scope| format!("ullage-{scope}"))
        .unwrap_or_else(|_| "ullage".to_owned());
    let directory = std::env::temp_dir().join(scope);
    std::fs::create_dir_all(&directory).map_err(|_| ClientError::DaemonProcess)?;
    sweep_daemon_error_logs(&directory);
    create_daemon_error_log(&directory, daemon_error_log_name)
}

fn run_daemon_process(
    executable: PathBuf,
    arguments: &[OsString],
    stderr_log: PathBuf,
    stderr_sink: std::fs::File,
    startup_timeout: Duration,
    mut readiness: impl FnMut(Duration) -> Result<bool, ClientError>,
) -> Result<(), ClientError> {
    let mut child = ProcessCommand::new(executable)
        .args(arguments)
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::from(stderr_sink))
        .spawn()
        .map_err(|_| ClientError::DaemonProcess)?;
    let deadline = std::time::Instant::now() + startup_timeout;
    loop {
        if child
            .try_wait()
            .map_err(|_| ClientError::DaemonProcess)?
            .is_some()
        {
            let _ = child.wait();
            return Err(daemon_process_failure(
                &stderr_log,
                "daemon exited during startup",
            ));
        }
        let remaining = deadline.saturating_duration_since(std::time::Instant::now());
        match readiness(remaining) {
            Ok(true) => break,
            Ok(false) if std::time::Instant::now() < deadline => {
                std::thread::sleep(Duration::from_millis(25));
            }
            Ok(false) => {
                let _ = child.kill();
                let _ = child.wait();
                return Err(daemon_process_failure(
                    &stderr_log,
                    "daemon did not become ready in time",
                ));
            }
            Err(error) => {
                let _ = child.kill();
                let _ = child.wait();
                return Err(error);
            }
        }
    }
    std::thread::Builder::new()
        .name("ullage-daemon-reaper".into())
        .spawn(move || {
            let _ = child.wait();
        })
        .map_err(|_| ClientError::DaemonProcess)?;
    Ok(())
}

/// The error a failed daemon launch reports: `fallback` when stderr stayed
/// empty, otherwise the sanitized tail so config, credential, and bind errors
/// are visible instead of a bare `daemon_process_failed`.
fn daemon_process_failure(stderr_log: &Path, fallback: &str) -> ClientError {
    let tail = sanitize_cli_text(String::from_utf8_lossy(&read_log_tail(stderr_log)).trim());
    if tail.is_empty() {
        ClientError::DaemonProcessOutput(fallback.to_owned())
    } else {
        ClientError::DaemonProcessOutput(format!("{fallback}; daemon stderr: {tail}"))
    }
}

/// Last `DAEMON_STDERR_TAIL_BYTES` of the daemon stderr log; an unreadable or
/// missing file yields an empty tail. The path is reopened after the child
/// exits, so the final component must not follow a swapped symlink into an
/// unrelated file the launcher can read.
fn read_log_tail(path: &Path) -> Vec<u8> {
    use std::io::{Read, Seek, SeekFrom};

    let mut options = std::fs::OpenOptions::new();
    options.read(true);
    #[cfg(unix)]
    {
        use std::os::unix::fs::OpenOptionsExt;
        options.custom_flags(libc::O_NOFOLLOW);
    }
    let mut file = match options.open(path) {
        Ok(file) => file,
        Err(_) => return Vec::new(),
    };
    let length = file.metadata().map(|metadata| metadata.len()).unwrap_or(0);
    if file
        .seek(SeekFrom::Start(
            length.saturating_sub(DAEMON_STDERR_TAIL_BYTES as u64),
        ))
        .is_err()
    {
        return Vec::new();
    }
    let mut tail = Vec::new();
    if file.read_to_end(&mut tail).is_err() {
        Vec::new()
    } else {
        tail
    }
}

fn daemon_command(windows: bool) -> Result<(PathBuf, Vec<OsString>), ClientError> {
    if let Some(executable) = std::env::var_os("ULLAGE_DAEMON_BIN") {
        return Ok((PathBuf::from(executable), Vec::new()));
    }
    let current = std::env::current_exe().map_err(|_| ClientError::DaemonProcess)?;
    let is_ullage = current
        .file_stem()
        .is_some_and(|name| name.eq_ignore_ascii_case("ullage"));
    if is_ullage {
        return Ok((current, vec![OsString::from("__daemon")]));
    }
    let mut executable = current;
    executable.set_file_name(if windows {
        "ullage-daemon.exe"
    } else {
        "ullage-daemon"
    });
    Ok((executable, Vec::new()))
}

fn request_waits_for_probe(request: &ControlRequest) -> bool {
    matches!(request.command, ControlCommand::Probe { wait: true, .. })
}

/// Completing a sign-in waits on the provider, and the daemon does not send a
/// reply twice: timing out early would drop the one carrying the credential it
/// has already stored. This has to outlast the provider HTTP timeouts.
fn request_completes_a_sign_in(request: &ControlRequest) -> bool {
    matches!(request.command, ControlCommand::CompleteAuth { .. })
}

const CONTROL_TIMEOUT: Duration = Duration::from_secs(30);
const SIGN_IN_TIMEOUT: Duration = Duration::from_secs(90);

#[cfg(windows)]
fn send_windows_pipe(
    endpoint: &std::path::Path,
    request: &ControlRequest,
) -> Result<ControlResponse, ClientError> {
    use std::fs::OpenOptions;
    use std::os::windows::fs::OpenOptionsExt;
    use windows_sys::Win32::Storage::FileSystem::{SECURITY_IDENTIFICATION, SECURITY_SQOS_PRESENT};

    let mut pipe = OpenOptions::new()
        .read(true)
        .write(true)
        .custom_flags(SECURITY_SQOS_PRESENT | SECURITY_IDENTIFICATION)
        .open(endpoint)
        .map_err(|_| ClientError::DaemonUnavailable)?;
    validate_windows_pipe_server(&pipe)?;
    write_request(&mut pipe, request)?;

    let mut encoded = Vec::new();
    BufReader::new(pipe)
        .take(MAX_RESPONSE_BYTES + 1)
        .read_until(b'\n', &mut encoded)
        .map_err(|_| ClientError::DaemonUnavailable)?;
    if !encoded.ends_with(b"\n") || encoded.len() as u64 > MAX_RESPONSE_BYTES {
        return Err(ClientError::InvalidResponse);
    }
    serde_json::from_slice(&encoded).map_err(|_| ClientError::InvalidResponse)
}

#[cfg(windows)]
fn send_windows_pipe_with_deadline(
    endpoint: &std::path::Path,
    request: &ControlRequest,
    deadline: std::time::Instant,
) -> Result<ControlResponse, ClientError> {
    use std::fs::OpenOptions;
    use std::os::windows::fs::OpenOptionsExt;
    use std::os::windows::io::AsRawHandle;
    use windows_sys::Win32::Foundation::HANDLE;
    use windows_sys::Win32::Storage::FileSystem::{SECURITY_IDENTIFICATION, SECURITY_SQOS_PRESENT};
    use windows_sys::Win32::System::Pipes::PeekNamedPipe;

    let mut pipe = OpenOptions::new()
        .read(true)
        .write(true)
        .custom_flags(SECURITY_SQOS_PRESENT | SECURITY_IDENTIFICATION)
        .open(endpoint)
        .map_err(|_| ClientError::DaemonUnavailable)?;
    validate_windows_pipe_server(&pipe)?;
    write_request(&mut pipe, request)?;

    let mut encoded = Vec::new();
    loop {
        if std::time::Instant::now() >= deadline {
            return Err(ClientError::DaemonUnavailable);
        }
        let mut available = 0;
        if unsafe {
            PeekNamedPipe(
                pipe.as_raw_handle() as HANDLE,
                std::ptr::null_mut(),
                0,
                std::ptr::null_mut(),
                &mut available,
                std::ptr::null_mut(),
            )
        } == 0
        {
            return Err(ClientError::DaemonUnavailable);
        }
        if available == 0 {
            std::thread::sleep(Duration::from_millis(10));
            continue;
        }
        let remaining = (MAX_RESPONSE_BYTES + 1)
            .saturating_sub(encoded.len() as u64)
            .min(u64::from(available)) as usize;
        if remaining == 0 {
            return Err(ClientError::InvalidResponse);
        }
        let mut chunk = vec![0; remaining];
        let read = pipe
            .read(&mut chunk)
            .map_err(|_| ClientError::DaemonUnavailable)?;
        if read == 0 {
            return Err(ClientError::DaemonUnavailable);
        }
        encoded.extend_from_slice(&chunk[..read]);
        if let Some(end) = encoded.iter().position(|byte| *byte == b'\n') {
            encoded.truncate(end + 1);
            break;
        }
    }
    if encoded.len() as u64 > MAX_RESPONSE_BYTES {
        return Err(ClientError::InvalidResponse);
    }
    serde_json::from_slice(&encoded).map_err(|_| ClientError::InvalidResponse)
}

#[cfg(windows)]
fn validate_windows_pipe_server(pipe: &std::fs::File) -> Result<(), ClientError> {
    use std::os::windows::io::AsRawHandle;
    use windows_sys::Win32::Foundation::{CloseHandle, HANDLE};
    use windows_sys::Win32::Security::{
        EqualSid, GetTokenInformation, TOKEN_QUERY, TOKEN_USER, TokenUser,
    };
    use windows_sys::Win32::System::Pipes::GetNamedPipeServerProcessId;
    use windows_sys::Win32::System::Threading::{
        GetCurrentProcess, OpenProcess, OpenProcessToken, PROCESS_QUERY_LIMITED_INFORMATION,
    };

    struct OwnedHandle(HANDLE);
    impl Drop for OwnedHandle {
        fn drop(&mut self) {
            if !self.0.is_null() {
                unsafe {
                    CloseHandle(self.0);
                }
            }
        }
    }

    fn process_token(process: HANDLE) -> Result<OwnedHandle, ClientError> {
        let mut token = std::ptr::null_mut();
        if unsafe { OpenProcessToken(process, TOKEN_QUERY, &mut token) } == 0 {
            return Err(ClientError::InvalidResponse);
        }
        Ok(OwnedHandle(token))
    }

    fn token_user(token: HANDLE) -> Result<Vec<usize>, ClientError> {
        let mut required = 0;
        unsafe {
            GetTokenInformation(token, TokenUser, std::ptr::null_mut(), 0, &mut required);
        }
        if required < std::mem::size_of::<TOKEN_USER>() as u32 {
            return Err(ClientError::InvalidResponse);
        }
        let words = (required as usize).div_ceil(std::mem::size_of::<usize>());
        let mut buffer = vec![0usize; words];
        if unsafe {
            GetTokenInformation(
                token,
                TokenUser,
                buffer.as_mut_ptr().cast(),
                required,
                &mut required,
            )
        } == 0
        {
            return Err(ClientError::InvalidResponse);
        }
        Ok(buffer)
    }

    let mut server_pid = 0;
    if unsafe { GetNamedPipeServerProcessId(pipe.as_raw_handle() as HANDLE, &mut server_pid) } == 0
    {
        return Err(ClientError::InvalidResponse);
    }
    let server_process =
        OwnedHandle(unsafe { OpenProcess(PROCESS_QUERY_LIMITED_INFORMATION, 0, server_pid) });
    if server_process.0.is_null() {
        return Err(ClientError::InvalidResponse);
    }
    let server_token = process_token(server_process.0)?;
    let current_token = process_token(unsafe { GetCurrentProcess() })?;
    let server_user = token_user(server_token.0)?;
    let current_user = token_user(current_token.0)?;
    let server_sid = unsafe { (*(server_user.as_ptr().cast::<TOKEN_USER>())).User.Sid };
    let current_sid = unsafe { (*(current_user.as_ptr().cast::<TOKEN_USER>())).User.Sid };
    if unsafe { EqualSid(server_sid, current_sid) } == 0 {
        return Err(ClientError::InvalidResponse);
    }
    Ok(())
}

#[cfg(unix)]
fn validate_private_unix_socket(path: &std::path::Path) -> Result<(), ClientError> {
    use std::os::unix::fs::{FileTypeExt, MetadataExt};

    // A relative path can only come from the environment
    // (`ULLAGE_CONTROL_SOCKET` or a relative `XDG_RUNTIME_DIR`): that is a
    // configuration error, not a protocol failure.
    if !path.is_absolute() {
        return Err(ClientError::InvalidEndpoint);
    }
    let metadata = std::fs::symlink_metadata(path).map_err(|_| ClientError::DaemonUnavailable)?;
    let current_user = unsafe { libc::geteuid() };
    // Between the daemon's bind and its chmod the socket exists with unsafe
    // permissions; treat every such state as unavailable so readiness probes
    // retry instead of declaring the peer untrustworthy.
    if !metadata.file_type().is_socket()
        || metadata.file_type().is_symlink()
        || metadata.uid() != current_user
        || metadata.mode() & 0o077 != 0
    {
        return Err(ClientError::DaemonUnavailable);
    }
    Ok(())
}

fn write_request(
    writer: &mut impl std::io::Write,
    request: &ControlRequest,
) -> Result<(), ClientError> {
    let mut encoded = serde_json::to_vec(request).map_err(|_| ClientError::InvalidResponse)?;
    encoded.push(b'\n');
    writer
        .write_all(&encoded)
        .map_err(|_| ClientError::DaemonUnavailable)
}

#[cfg(target_os = "linux")]
fn validate_unix_peer(stream: &std::os::unix::net::UnixStream) -> Result<(), ClientError> {
    use std::os::fd::AsRawFd;

    let mut credentials: libc::ucred = unsafe { std::mem::zeroed() };
    let mut length = std::mem::size_of::<libc::ucred>() as libc::socklen_t;
    let status = unsafe {
        libc::getsockopt(
            stream.as_raw_fd(),
            libc::SOL_SOCKET,
            libc::SO_PEERCRED,
            (&mut credentials as *mut libc::ucred).cast(),
            &mut length,
        )
    };
    if status != 0 || length as usize != std::mem::size_of::<libc::ucred>() {
        return Err(ClientError::InvalidResponse);
    }
    if credentials.uid != unsafe { libc::geteuid() } {
        return Err(ClientError::InvalidResponse);
    }
    Ok(())
}

#[cfg(all(unix, not(target_os = "linux")))]
fn validate_unix_peer(stream: &std::os::unix::net::UnixStream) -> Result<(), ClientError> {
    use std::os::fd::AsRawFd;

    let mut peer_uid = 0;
    let mut peer_gid = 0;
    if unsafe { libc::getpeereid(stream.as_raw_fd(), &mut peer_uid, &mut peer_gid) } != 0
        || peer_uid != unsafe { libc::geteuid() }
    {
        return Err(ClientError::InvalidResponse);
    }
    Ok(())
}
#[cfg(all(test, unix))]
mod tests {
    use std::io::{BufRead, BufReader, Write};
    use std::os::unix::fs::PermissionsExt;
    use std::os::unix::net::UnixListener;

    use ullage_protocol::{AccountId, CredentialBackendId, ProviderId};

    use super::*;

    struct FailingWriter;

    impl std::io::Write for FailingWriter {
        fn write(&mut self, _: &[u8]) -> std::io::Result<usize> {
            Err(std::io::Error::new(
                std::io::ErrorKind::BrokenPipe,
                "closed",
            ))
        }

        fn flush(&mut self) -> std::io::Result<()> {
            Ok(())
        }
    }

    #[test]
    fn request_write_failure_is_daemon_unavailable() {
        let error = write_request(
            &mut FailingWriter,
            &ControlRequest::new("write-test", ControlCommand::ListProviders),
        )
        .unwrap_err();
        assert!(matches!(error, ClientError::DaemonUnavailable));
    }

    #[test]
    fn system_client_accepts_a_private_same_user_socket() {
        let directory = std::env::temp_dir().join(format!(
            "ullage-cli-transport-{}-{}",
            std::process::id(),
            REQUEST_SEQUENCE.fetch_add(1, Ordering::Relaxed)
        ));
        std::fs::create_dir(&directory).unwrap();
        let socket_path = directory.join("control.sock");
        let listener = UnixListener::bind(&socket_path).unwrap();
        std::fs::set_permissions(&socket_path, std::fs::Permissions::from_mode(0o600)).unwrap();

        let server = std::thread::spawn(move || {
            let (mut stream, _) = listener.accept().unwrap();
            let mut encoded = String::new();
            BufReader::new(&mut stream).read_line(&mut encoded).unwrap();
            let request: ControlRequest = serde_json::from_str(&encoded).unwrap();
            let response = ControlResponse {
                version: CONTROL_PROTOCOL_VERSION,
                request_id: request.request_id,
                result: ControlResult::Ack,
                diagnostic: None,
            };
            serde_json::to_writer(&mut stream, &response).unwrap();
            stream.write_all(b"\n").unwrap();
        });

        let client = SystemClient {
            endpoint: Some(socket_path.clone()),
        };
        let response = client
            .send(&ControlRequest::new(
                "transport-test",
                ControlCommand::ListProviders,
            ))
            .unwrap();
        assert_eq!(response.result, ControlResult::Ack);

        server.join().unwrap();
        std::fs::remove_file(socket_path).unwrap();
        std::fs::remove_dir(directory).unwrap();
    }

    #[test]
    fn readiness_rejects_stopping_and_protocol_mismatch_responses() {
        for result in [
            ControlResult::DaemonStatus(DaemonStatusPayload {
                shutting_down: true,
                accounts: Vec::new(),
                credential_backend: CredentialBackendId::native(),
            }),
            ControlResult::ProtocolMismatch {
                supported_version: CONTROL_PROTOCOL_VERSION,
            },
        ] {
            let directory = std::env::temp_dir().join(format!(
                "ullage-cli-readiness-{}-{}",
                std::process::id(),
                REQUEST_SEQUENCE.fetch_add(1, Ordering::Relaxed)
            ));
            std::fs::create_dir(&directory).unwrap();
            let socket_path = directory.join("control.sock");
            let listener = UnixListener::bind(&socket_path).unwrap();
            std::fs::set_permissions(&socket_path, std::fs::Permissions::from_mode(0o600)).unwrap();
            let server = std::thread::spawn(move || {
                let (mut stream, _) = listener.accept().unwrap();
                let mut encoded = String::new();
                BufReader::new(&mut stream).read_line(&mut encoded).unwrap();
                let request: ControlRequest = serde_json::from_str(&encoded).unwrap();
                let response = ControlResponse {
                    version: CONTROL_PROTOCOL_VERSION,
                    request_id: request.request_id,
                    result,
                    diagnostic: None,
                };
                serde_json::to_writer(&mut stream, &response).unwrap();
                stream.write_all(b"\n").unwrap();
            });
            let client = SystemClient {
                endpoint: Some(socket_path.clone()),
            };

            // `NotReady` is transient: the readiness probe reports "not ready"
            // so callers retry inside their budget instead of failing.
            assert!(matches!(
                client.daemon_is_ready(Duration::from_secs(1)),
                Ok(false)
            ));
            server.join().unwrap();
            std::fs::remove_file(socket_path).unwrap();
            std::fs::remove_dir(directory).unwrap();
        }
    }

    #[test]
    fn readiness_retries_through_not_ready_until_ready() {
        let directory = std::env::temp_dir().join(format!(
            "ullage-cli-readiness-retry-{}-{}",
            std::process::id(),
            REQUEST_SEQUENCE.fetch_add(1, Ordering::Relaxed)
        ));
        std::fs::create_dir(&directory).unwrap();
        let socket_path = directory.join("control.sock");
        let listener = UnixListener::bind(&socket_path).unwrap();
        std::fs::set_permissions(&socket_path, std::fs::Permissions::from_mode(0o600)).unwrap();
        let server = std::thread::spawn(move || {
            let mut answered = 0;
            while let Ok((mut stream, _)) = listener.accept() {
                let mut encoded = String::new();
                BufReader::new(&mut stream).read_line(&mut encoded).unwrap();
                let request: ControlRequest = serde_json::from_str(&encoded).unwrap();
                answered += 1;
                let result = if answered < 3 {
                    ControlResult::DaemonStatus(DaemonStatusPayload {
                        shutting_down: true,
                        accounts: Vec::new(),
                        credential_backend: CredentialBackendId::native(),
                    })
                } else {
                    ControlResult::DaemonStatus(DaemonStatusPayload {
                        shutting_down: false,
                        accounts: Vec::new(),
                        credential_backend: CredentialBackendId::native(),
                    })
                };
                let response = ControlResponse {
                    version: CONTROL_PROTOCOL_VERSION,
                    request_id: request.request_id,
                    result,
                    diagnostic: None,
                };
                serde_json::to_writer(&mut stream, &response).unwrap();
                stream.write_all(b"\n").unwrap();
            }
        });
        let client = SystemClient {
            endpoint: Some(socket_path.clone()),
        };

        client.wait_for_service_ready().unwrap();
        std::fs::remove_file(socket_path).unwrap();
        std::fs::remove_dir(directory).unwrap();
        // The accept loop stays blocked on the unlinked socket until the test
        // process exits; dropping the handle detaches it.
        drop(server);
    }

    #[test]
    fn stop_probe_recognizes_authenticated_non_ready_daemons() {
        for result in [
            ControlResult::DaemonStatus(DaemonStatusPayload {
                shutting_down: true,
                accounts: Vec::new(),
                credential_backend: CredentialBackendId::native(),
            }),
            ControlResult::ProtocolMismatch {
                supported_version: CONTROL_PROTOCOL_VERSION,
            },
        ] {
            let response = ControlResponse {
                version: CONTROL_PROTOCOL_VERSION,
                request_id: "stop-probe".into(),
                result,
                diagnostic: None,
            };
            assert_eq!(
                classify_readiness_response(&response, "stop-probe").unwrap(),
                DaemonReadiness::NotReady
            );
        }
    }

    #[test]
    fn stop_waits_through_shutting_down_until_the_endpoint_disappears() {
        let directory = std::env::temp_dir().join(format!(
            "ullage-cli-stop-wait-{}-{}",
            std::process::id(),
            REQUEST_SEQUENCE.fetch_add(1, Ordering::Relaxed)
        ));
        std::fs::create_dir(&directory).unwrap();
        let socket_path = directory.join("control.sock");
        let listener = UnixListener::bind(&socket_path).unwrap();
        std::fs::set_permissions(&socket_path, std::fs::Permissions::from_mode(0o600)).unwrap();
        let server = std::thread::spawn(move || {
            let (mut stream, _) = listener.accept().unwrap();
            let mut encoded = String::new();
            BufReader::new(&mut stream).read_line(&mut encoded).unwrap();
            let request: ControlRequest = serde_json::from_str(&encoded).unwrap();
            let response = ControlResponse {
                version: CONTROL_PROTOCOL_VERSION,
                request_id: request.request_id,
                result: ControlResult::DaemonStatus(DaemonStatusPayload {
                    shutting_down: true,
                    accounts: Vec::new(),
                    credential_backend: CredentialBackendId::native(),
                }),
                diagnostic: None,
            };
            serde_json::to_writer(&mut stream, &response).unwrap();
            stream.write_all(b"\n").unwrap();
        });
        let client = SystemClient {
            endpoint: Some(socket_path.clone()),
        };

        client.wait_for_service_stopped().unwrap();
        server.join().unwrap();
        std::fs::remove_file(socket_path).unwrap();
        std::fs::remove_dir(directory).unwrap();
    }

    #[test]
    fn readiness_read_is_bounded_by_its_budget() {
        let directory = std::env::temp_dir().join(format!(
            "ullage-cli-readiness-timeout-{}-{}",
            std::process::id(),
            REQUEST_SEQUENCE.fetch_add(1, Ordering::Relaxed)
        ));
        std::fs::create_dir(&directory).unwrap();
        let socket_path = directory.join("control.sock");
        let listener = UnixListener::bind(&socket_path).unwrap();
        std::fs::set_permissions(&socket_path, std::fs::Permissions::from_mode(0o600)).unwrap();
        let server = std::thread::spawn(move || {
            let (mut stream, _) = listener.accept().unwrap();
            let mut encoded = String::new();
            BufReader::new(&mut stream).read_line(&mut encoded).unwrap();
            std::thread::sleep(Duration::from_millis(200));
        });
        let client = SystemClient {
            endpoint: Some(socket_path.clone()),
        };
        let started = std::time::Instant::now();

        assert!(matches!(
            client.daemon_is_ready(Duration::from_millis(50)),
            Ok(false)
        ));
        assert!(started.elapsed() < Duration::from_millis(150));
        server.join().unwrap();
        std::fs::remove_file(socket_path).unwrap();
        std::fs::remove_dir(directory).unwrap();
    }

    #[test]
    fn readiness_slow_trickle_cannot_reset_its_budget() {
        let directory = std::env::temp_dir().join(format!(
            "ullage-cli-readiness-trickle-{}-{}",
            std::process::id(),
            REQUEST_SEQUENCE.fetch_add(1, Ordering::Relaxed)
        ));
        std::fs::create_dir(&directory).unwrap();
        let socket_path = directory.join("control.sock");
        let listener = UnixListener::bind(&socket_path).unwrap();
        std::fs::set_permissions(&socket_path, std::fs::Permissions::from_mode(0o600)).unwrap();
        let server = std::thread::spawn(move || {
            let (mut stream, _) = listener.accept().unwrap();
            let mut encoded = String::new();
            BufReader::new(&mut stream).read_line(&mut encoded).unwrap();
            for _ in 0..10 {
                if stream.write_all(b"{").is_err() {
                    break;
                }
                std::thread::sleep(Duration::from_millis(20));
            }
        });
        let client = SystemClient {
            endpoint: Some(socket_path.clone()),
        };
        let started = std::time::Instant::now();

        assert!(matches!(
            client.daemon_is_ready(Duration::from_millis(60)),
            Ok(false)
        ));
        assert!(started.elapsed() < Duration::from_millis(150));
        server.join().unwrap();
        std::fs::remove_file(socket_path).unwrap();
        std::fs::remove_dir(directory).unwrap();
    }

    #[test]
    fn default_unix_endpoint_is_scoped_to_the_effective_user() {
        let endpoint = default_unix_control_socket();
        // SAFETY: `geteuid` has no arguments and no memory-safety preconditions.
        let user_id = unsafe { libc::geteuid() };
        let expected_parent = format!("ullage-{user_id}");

        assert_eq!(endpoint.file_name().unwrap(), "control.sock");
        assert_eq!(
            endpoint.parent().unwrap().file_name().unwrap(),
            std::ffi::OsStr::new(&expected_parent)
        );
    }

    #[test]
    fn probe_reads_are_bounded_above_the_sign_in_timeout() {
        let probe = ControlRequest::new(
            "t",
            ControlCommand::Probe {
                account_id: "claude-a".into(),
                wait: true,
            },
        );
        assert_eq!(send_timeout(&probe), PROBE_WAIT_TIMEOUT);
        assert!(PROBE_WAIT_TIMEOUT > SIGN_IN_TIMEOUT);

        let sign_in = ControlRequest::new(
            "t",
            ControlCommand::CompleteAuth {
                provider: ProviderId::new("claude"),
                account: AccountId::new("claude-a"),
                request: ullage_protocol::AuthCompleteRequest {
                    flow_id: "f".into(),
                    authorization_code: None,
                    redirect_uri: None,
                },
            },
        );
        assert_eq!(send_timeout(&sign_in), SIGN_IN_TIMEOUT);

        let control = ControlRequest::new("t", ControlCommand::ListProviders);
        assert_eq!(send_timeout(&control), CONTROL_TIMEOUT);
    }

    #[test]
    fn relative_socket_path_is_an_endpoint_error_not_a_protocol_error() {
        let client = SystemClient {
            endpoint: Some(PathBuf::from("relative/control.sock")),
        };
        let error = client
            .send(&ControlRequest::new("t", ControlCommand::ListProviders))
            .unwrap_err();
        assert!(matches!(error, ClientError::InvalidEndpoint));
    }

    #[test]
    fn unsafe_permitted_socket_is_unavailable_so_readiness_retries() {
        let directory = std::env::temp_dir().join(format!(
            "ullage-cli-perms-{}-{}",
            std::process::id(),
            REQUEST_SEQUENCE.fetch_add(1, Ordering::Relaxed)
        ));
        std::fs::create_dir(&directory).unwrap();
        let socket_path = directory.join("control.sock");
        let _listener = UnixListener::bind(&socket_path).unwrap();
        // The state a client sees between the daemon's bind and its chmod.
        std::fs::set_permissions(&socket_path, std::fs::Permissions::from_mode(0o777)).unwrap();

        let client = SystemClient {
            endpoint: Some(socket_path.clone()),
        };
        let error = client
            .send(&ControlRequest::new("t", ControlCommand::ListProviders))
            .unwrap_err();
        assert!(matches!(error, ClientError::DaemonUnavailable));
        std::fs::remove_file(socket_path).unwrap();
        std::fs::remove_dir(directory).unwrap();
    }

    fn daemon_script(directory: &std::path::Path, name: &str, body: &str) -> PathBuf {
        use std::os::unix::fs::PermissionsExt;

        std::fs::create_dir_all(directory).unwrap();
        let script = directory.join(name);
        std::fs::write(&script, body).unwrap();
        std::fs::set_permissions(&script, std::fs::Permissions::from_mode(0o700)).unwrap();
        script
    }

    fn daemon_log(directory: &std::path::Path) -> (PathBuf, std::fs::File) {
        let path = directory.join("daemon-error.log");
        let file = std::fs::OpenOptions::new()
            .create(true)
            .write(true)
            .truncate(true)
            .open(&path)
            .unwrap();
        (path, file)
    }

    #[test]
    fn run_daemon_process_reports_stderr_tail_on_early_exit() {
        let directory = std::env::temp_dir().join(format!(
            "ullage-cli-daemon-early-{}-{}",
            std::process::id(),
            REQUEST_SEQUENCE.fetch_add(1, Ordering::Relaxed)
        ));
        let script = daemon_script(
            &directory,
            "noisy-daemon",
            "#!/bin/sh\nprintf 'bind failed: address in use\\n' >&2\nexit 1\n",
        );
        let (log_path, log_sink) = daemon_log(&directory);

        // Spawning can transiently fail (EAGAIN) when the whole suite runs in
        // parallel; the behavior under test is the early-exit report.
        let mut result = run_daemon_process(
            script.clone(),
            &[],
            log_path.clone(),
            log_sink.try_clone().unwrap(),
            Duration::from_secs(5),
            |_| Ok(false),
        );
        for _ in 0..3 {
            if !matches!(result, Err(ClientError::DaemonProcess)) {
                break;
            }
            std::thread::sleep(Duration::from_millis(50));
            result = run_daemon_process(
                script.clone(),
                &[],
                log_path.clone(),
                log_sink.try_clone().unwrap(),
                Duration::from_secs(5),
                |_| Ok(false),
            );
        }
        let error = result.unwrap_err();
        let ClientError::DaemonProcessOutput(detail) = error else {
            panic!("expected DaemonProcessOutput, got {error:?}");
        };
        assert!(detail.contains("daemon exited during startup"), "{detail}");
        assert!(detail.contains("bind failed: address in use"), "{detail}");
        std::fs::remove_dir_all(directory).unwrap();
    }

    #[test]
    fn run_daemon_process_reports_stderr_tail_on_readiness_timeout() {
        let directory = std::env::temp_dir().join(format!(
            "ullage-cli-daemon-timeout-{}-{}",
            std::process::id(),
            REQUEST_SEQUENCE.fetch_add(1, Ordering::Relaxed)
        ));
        let script = daemon_script(
            &directory,
            "stuck-daemon",
            "#!/bin/sh\nprintf 'still loading plugins\\n' >&2\nsleep 30\n",
        );
        let (log_path, log_sink) = daemon_log(&directory);

        // Spawning can transiently fail (EAGAIN) when the whole suite runs in
        // parallel; the behavior under test is the readiness-timeout report.
        let mut result = run_daemon_process(
            script.clone(),
            &[],
            log_path.clone(),
            log_sink.try_clone().unwrap(),
            Duration::from_millis(120),
            |_| Ok(false),
        );
        for _ in 0..3 {
            if !matches!(result, Err(ClientError::DaemonProcess)) {
                break;
            }
            std::thread::sleep(Duration::from_millis(50));
            result = run_daemon_process(
                script.clone(),
                &[],
                log_path.clone(),
                log_sink.try_clone().unwrap(),
                Duration::from_millis(120),
                |_| Ok(false),
            );
        }
        let error = result.unwrap_err();
        let ClientError::DaemonProcessOutput(detail) = error else {
            panic!("expected DaemonProcessOutput, got {error:?}");
        };
        assert!(detail.contains("did not become ready"), "{detail}");
        assert!(detail.contains("still loading plugins"), "{detail}");
        std::fs::remove_dir_all(directory).unwrap();
    }

    #[test]
    fn run_daemon_process_returns_once_readiness_reports_ready() {
        let directory = std::env::temp_dir().join(format!(
            "ullage-cli-daemon-ready-{}-{}",
            std::process::id(),
            REQUEST_SEQUENCE.fetch_add(1, Ordering::Relaxed)
        ));
        // The daemon must outlive the first readiness check: a shorter sleep
        // lets try_wait observe the exit first and the test flakes.
        let script = daemon_script(&directory, "daemon", "#!/bin/sh\nsleep 30\n");
        let (log_path, log_sink) = daemon_log(&directory);

        // Spawning can transiently fail (EAGAIN) when the whole suite runs in
        // parallel; the behavior under test is the readiness return.
        let mut result = run_daemon_process(
            script.clone(),
            &[],
            log_path.clone(),
            log_sink.try_clone().unwrap(),
            Duration::from_secs(5),
            |_| Ok(true),
        );
        for _ in 0..3 {
            if result.is_ok() {
                break;
            }
            std::thread::sleep(Duration::from_millis(50));
            result = run_daemon_process(
                script.clone(),
                &[],
                log_path.clone(),
                log_sink.try_clone().unwrap(),
                Duration::from_secs(5),
                |_| Ok(true),
            );
        }
        result.unwrap();
        std::fs::remove_dir_all(directory).unwrap();
    }

    #[test]
    fn run_daemon_waits_out_a_stopping_daemon_instead_of_spawning() {
        // The endpoint serves NotReady twice, then Ready: `daemon run` must
        // wait inside its budget and never spawn a colliding child.
        let directory = std::env::temp_dir().join(format!(
            "ullage-cli-run-wait-{}-{}",
            std::process::id(),
            REQUEST_SEQUENCE.fetch_add(1, Ordering::Relaxed)
        ));
        std::fs::create_dir(&directory).unwrap();
        let socket_path = directory.join("control.sock");
        let listener = UnixListener::bind(&socket_path).unwrap();
        std::fs::set_permissions(&socket_path, std::fs::Permissions::from_mode(0o600)).unwrap();
        let server = std::thread::spawn(move || {
            for poll in 0..3 {
                let Ok((mut stream, _)) = listener.accept() else {
                    return;
                };
                let mut encoded = String::new();
                if BufReader::new(&mut stream).read_line(&mut encoded).is_err() {
                    return;
                }
                let Ok(request) = serde_json::from_str::<ControlRequest>(&encoded) else {
                    return;
                };
                let response = ControlResponse {
                    version: CONTROL_PROTOCOL_VERSION,
                    request_id: request.request_id,
                    result: ControlResult::DaemonStatus(DaemonStatusPayload {
                        shutting_down: poll < 2,
                        accounts: Vec::new(),
                        credential_backend: CredentialBackendId::native(),
                    }),
                    diagnostic: None,
                };
                if serde_json::to_writer(&mut stream, &response).is_err()
                    || stream.write_all(b"\n").is_err()
                {
                    return;
                }
            }
        });
        let client = SystemClient {
            endpoint: Some(socket_path.clone()),
        };

        client.run_daemon().unwrap();
        server.join().unwrap();
        std::fs::remove_file(socket_path).unwrap();
        std::fs::remove_dir(directory).unwrap();
    }

    #[test]
    fn run_daemon_reports_a_daemon_that_never_frees_the_endpoint() {
        let directory = std::env::temp_dir().join(format!(
            "ullage-cli-run-held-{}-{}",
            std::process::id(),
            REQUEST_SEQUENCE.fetch_add(1, Ordering::Relaxed)
        ));
        std::fs::create_dir(&directory).unwrap();
        let socket_path = directory.join("control.sock");
        let listener = UnixListener::bind(&socket_path).unwrap();
        std::fs::set_permissions(&socket_path, std::fs::Permissions::from_mode(0o600)).unwrap();
        let server = std::thread::spawn(move || {
            while let Ok((mut stream, _)) = listener.accept() {
                let mut encoded = String::new();
                if BufReader::new(&mut stream).read_line(&mut encoded).is_err() {
                    continue;
                }
                let Ok(request) = serde_json::from_str::<ControlRequest>(&encoded) else {
                    continue;
                };
                let response = ControlResponse {
                    version: CONTROL_PROTOCOL_VERSION,
                    request_id: request.request_id,
                    result: ControlResult::DaemonStatus(DaemonStatusPayload {
                        shutting_down: true,
                        accounts: Vec::new(),
                        credential_backend: CredentialBackendId::native(),
                    }),
                    diagnostic: None,
                };
                if serde_json::to_writer(&mut stream, &response).is_err()
                    || stream.write_all(b"\n").is_err()
                {
                    continue;
                }
            }
        });
        let client = SystemClient {
            endpoint: Some(socket_path.clone()),
        };

        let started = std::time::Instant::now();
        let error = client.run_daemon().unwrap_err();
        assert!(
            matches!(error, ClientError::DaemonStillRunning),
            "{error:?}"
        );
        assert!(started.elapsed() >= Duration::from_secs(5));
        drop(server);
        std::fs::remove_file(socket_path).unwrap();
        std::fs::remove_dir(directory).unwrap();
    }

    #[test]
    fn daemon_stop_fails_while_a_non_service_daemon_still_answers() {
        // `service::stop` must see "not installed": point the systemd unit
        // directory at an empty temp config root.
        let config_root = std::env::temp_dir().join(format!(
            "ullage-cli-stop-config-{}-{}",
            std::process::id(),
            REQUEST_SEQUENCE.fetch_add(1, Ordering::Relaxed)
        ));
        std::fs::create_dir_all(&config_root).unwrap();
        unsafe {
            std::env::set_var("XDG_CONFIG_HOME", &config_root);
        }
        let result = std::panic::catch_unwind(|| {
            let directory = std::env::temp_dir().join(format!(
                "ullage-cli-stop-live-{}-{}",
                std::process::id(),
                REQUEST_SEQUENCE.fetch_add(1, Ordering::Relaxed)
            ));
            std::fs::create_dir(&directory).unwrap();
            let socket_path = directory.join("control.sock");
            let listener = UnixListener::bind(&socket_path).unwrap();
            std::fs::set_permissions(&socket_path, std::fs::Permissions::from_mode(0o600)).unwrap();
            let server = std::thread::spawn(move || {
                while let Ok((mut stream, _)) = listener.accept() {
                    let mut encoded = String::new();
                    BufReader::new(&mut stream).read_line(&mut encoded).unwrap();
                    let request: ControlRequest = serde_json::from_str(&encoded).unwrap();
                    let response = ControlResponse {
                        version: CONTROL_PROTOCOL_VERSION,
                        request_id: request.request_id,
                        result: ControlResult::DaemonStatus(DaemonStatusPayload {
                            shutting_down: false,
                            accounts: Vec::new(),
                            credential_backend: CredentialBackendId::native(),
                        }),
                        diagnostic: None,
                    };
                    serde_json::to_writer(&mut stream, &response).unwrap();
                    stream.write_all(b"\n").unwrap();
                }
            });
            let client = SystemClient {
                endpoint: Some(socket_path.clone()),
            };
            let error = client.manage_service(ServiceAction::Stop).unwrap_err();
            assert!(matches!(error, ClientError::DaemonStillRunning));

            // Once nothing answers, stop succeeds again.
            drop(server);
            std::fs::remove_file(&socket_path).unwrap();
            client.manage_service(ServiceAction::Stop).unwrap();
            std::fs::remove_dir(directory).unwrap();
        });
        unsafe {
            std::env::remove_var("XDG_CONFIG_HOME");
        }
        std::fs::remove_dir_all(config_root).unwrap();
        if let Err(payload) = result {
            std::panic::resume_unwind(payload);
        }
    }

    #[test]
    fn daemon_error_log_sweep_removes_only_stale_files() {
        let directory = std::env::temp_dir().join(format!(
            "ullage-cli-log-sweep-{}-{}",
            std::process::id(),
            REQUEST_SEQUENCE.fetch_add(1, Ordering::Relaxed)
        ));
        std::fs::create_dir(&directory).unwrap();
        let active = directory.join("daemon-error-1-0.log");
        let also_active = directory.join("daemon-error-2-0.log");
        let unrelated = directory.join("control.sock");
        for path in [&active, &also_active, &unrelated] {
            std::fs::File::create(path).unwrap();
        }

        // A sweep at launch time must leave fresh logs alone: they may belong
        // to another launcher still starting its daemon.
        sweep_daemon_error_logs(&directory);
        assert!(active.exists());
        assert!(also_active.exists());
        assert!(unrelated.exists());

        // Everything older than the cutoff is stale and removed; unrelated
        // files are never touched.
        sweep_daemon_error_logs_before(
            &directory,
            std::time::SystemTime::now() + Duration::from_secs(60),
        );
        assert!(!active.exists());
        assert!(!also_active.exists());
        assert!(unrelated.exists());

        std::fs::remove_dir_all(directory).unwrap();
    }

    #[test]
    fn daemon_error_log_retries_a_fresh_name_on_collision() {
        let directory = std::env::temp_dir().join(format!(
            "ullage-cli-log-collision-{}-{}",
            std::process::id(),
            REQUEST_SEQUENCE.fetch_add(1, Ordering::Relaxed)
        ));
        std::fs::create_dir(&directory).unwrap();
        // A recycled PID inside the retention window can collide with a
        // fresh log left by an earlier process; the create must retry with
        // the next name instead of failing the launch.
        let taken = directory.join("daemon-error-taken.log");
        std::fs::File::create(&taken).unwrap();

        let mut names = ["daemon-error-taken.log", "daemon-error-free.log"]
            .into_iter()
            .map(str::to_owned);
        let (path, _file) = create_daemon_error_log(&directory, || names.next().unwrap()).unwrap();
        assert_eq!(path, directory.join("daemon-error-free.log"));

        // Exhausting every generated name surfaces DaemonProcess rather than
        // truncating or blocking on someone else's file.
        let mut stuck = || "daemon-error-taken.log".to_owned();
        let result = create_daemon_error_log(&directory, &mut stuck);
        assert!(matches!(result, Err(ClientError::DaemonProcess)));

        std::fs::remove_dir_all(directory).unwrap();
    }

    #[cfg(unix)]
    #[test]
    fn daemon_log_directory_rejects_symlinks_and_foreign_dirs() {
        use std::os::unix::fs::PermissionsExt;

        let directory = std::env::temp_dir().join(format!(
            "ullage-cli-logdir-{}-{}",
            std::process::id(),
            REQUEST_SEQUENCE.fetch_add(1, Ordering::Relaxed)
        ));
        std::fs::create_dir(&directory).unwrap();

        // A world-writable parent lets a neighbour pre-create the log dir;
        // loose permissions must fail the launch instead of being adopted.
        let loose = directory.join("loose");
        std::fs::create_dir(&loose).unwrap();
        std::fs::set_permissions(&loose, std::fs::Permissions::from_mode(0o755)).unwrap();
        assert!(matches!(
            ensure_daemon_log_directory(&loose),
            Err(ClientError::DaemonProcess)
        ));

        // A symlink must never be followed into attacker-chosen territory.
        let link = directory.join("link");
        std::os::unix::fs::symlink(&loose, &link).unwrap();
        assert!(matches!(
            ensure_daemon_log_directory(&link),
            Err(ClientError::DaemonProcess)
        ));

        // A missing path is created fresh with private permissions.
        let fresh = directory.join("fresh");
        ensure_daemon_log_directory(&fresh).unwrap();
        let metadata = std::fs::symlink_metadata(&fresh).unwrap();
        assert!(metadata.is_dir());
        assert_eq!(metadata.permissions().mode() & 0o777, 0o700);

        std::fs::remove_dir_all(directory).unwrap();
    }

    #[cfg(unix)]
    #[test]
    fn read_log_tail_does_not_follow_a_symlink() {
        let directory = std::env::temp_dir().join(format!(
            "ullage-cli-logtail-{}-{}",
            std::process::id(),
            REQUEST_SEQUENCE.fetch_add(1, Ordering::Relaxed)
        ));
        std::fs::create_dir(&directory).unwrap();
        let secret = directory.join("secret.txt");
        std::fs::write(&secret, b"do-not-leak").unwrap();
        let link = directory.join("daemon-error-swapped.log");
        std::os::unix::fs::symlink(&secret, &link).unwrap();

        // The name was swapped between create and read-back; the tail must
        // come out empty rather than streaming the link target.
        assert!(read_log_tail(&link).is_empty());
        assert_eq!(read_log_tail(&secret), b"do-not-leak");

        std::fs::remove_dir_all(directory).unwrap();
    }
}
