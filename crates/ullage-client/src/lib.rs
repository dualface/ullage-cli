use std::ffi::OsString;
use std::io::Read as _;
#[cfg(windows)]
use std::io::{BufRead, BufReader};
use std::path::{Path, PathBuf};
use std::process::Command as ProcessCommand;
use std::process::Stdio;
use std::sync::atomic::{AtomicU64, Ordering};
#[cfg(any(unix, windows))]
use std::time::Duration;

use clap::error::{ContextKind, ContextValue, ErrorKind};
use clap::{Args, CommandFactory, Parser, Subcommand, ValueEnum};
use serde::Serialize;
use thiserror::Error;
use ullage_protocol::{
    Account, AccountError, AccountId, AuthCompleteRequest, AuthMethod, AuthStartRequest, AuthState,
    CONTROL_PROTOCOL_VERSION, ControlCommand, ControlError, ControlRequest, ControlResponse,
    ControlResult, DaemonStatusPayload, LogoutRequest, PairCodePayload, ProviderError, ProviderId,
    QueryOutcome, RegistryError, SnapshotPayload, SubscriptionUsage,
};

const MAX_RESPONSE_BYTES: u64 = 1024 * 1024;
static REQUEST_SEQUENCE: AtomicU64 = AtomicU64::new(1);

mod login;
pub mod prompt;
mod render;
mod service;
mod table;

#[cfg(test)]
mod summary_render_tests;

use render::{ResolvedMetricFilter, human_result, render_stopped_service, sanitize_cell};
use table::Palette;
use ullage_core::is_unsafe_identity_character as is_unsafe_control;
use ullage_core::summary::MetricFilter;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
#[repr(i32)]
pub enum ExitCode {
    Success = 0,
    Failure = 1,
    Partial = 2,
    AuthenticationInvalid = 3,
    NetworkFailure = 4,
    DaemonUnavailable = 5,
    ProtocolError = 6,
    Usage = 64,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct RunOutput {
    pub stdout: String,
    pub stderr: String,
    pub code: ExitCode,
}

#[derive(Debug, Error)]
pub enum ClientError {
    #[error("daemon unavailable")]
    DaemonUnavailable,
    #[error("daemon response was invalid")]
    InvalidResponse,
    /// The configured endpoint is malformed: a relative or otherwise unusable
    /// `ULLAGE_CONTROL_SOCKET` value, reported under its own kind instead of a
    /// protocol failure. (Windows treats a rejected `ULLAGE_CONTROL_PIPE` as
    /// no endpoint and reports `DaemonUnavailable`.)
    #[error("control endpoint is invalid")]
    InvalidEndpoint,
    #[error("daemon process failed")]
    DaemonProcess,
    /// Daemon startup or shutdown failed; carries the captured stderr tail or
    /// the reason no tail could be collected.
    #[error("daemon process failed: {0}")]
    DaemonProcessOutput(String),
    /// A daemon still answers on the control endpoint after the service stop.
    #[error("daemon is still running")]
    DaemonStillRunning,
}

pub trait ControlClient {
    fn send(&self, request: &ControlRequest) -> Result<ControlResponse, ClientError>;

    fn run_daemon(&self) -> Result<(), ClientError> {
        Err(ClientError::DaemonProcess)
    }

    fn manage_daemon(&self, _: ServiceAction) -> Result<(), ClientError> {
        Err(ClientError::DaemonProcess)
    }

    fn daemon_service_installed(&self) -> Result<bool, ClientError> {
        Err(ClientError::DaemonProcess)
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum ServiceAction {
    Install,
    Start,
    Stop,
    Uninstall,
}

const CLI_ABOUT: &str = "Inspect subscription usage through the Ullage daemon";
const CLI_LONG_ABOUT: &str =
    "Ullage inspects subscription usage for Claude, ChatGPT, Grok, and Cursor.

Most commands talk to a per-user local daemon over a private control socket on \
Unix or a named pipe on Windows. Install the user-level service once, start it, \
then run the remaining commands. Later logins start the service automatically.

Exit codes:
  0   success
  1   failure
  2   partial
  3   authentication invalid
  4   network failure
  5   daemon unavailable
  6   protocol error
  64  usage";
const CLI_AFTER_HELP: &str = "Examples:
  ullage daemon install
  ullage daemon start
  ullage auth login
  ullage show --all
  ullage device pair";
const DEVICE_AFTER_HELP: &str = "Examples:
  ullage device pair
  ullage device list
  ullage device revoke <DEVICE_ID>";
const DAEMON_INSTALL_AFTER_HELP: &str = "Examples:
  ullage daemon install
  ullage daemon start
  ullage daemon status";
const AUTH_LOGIN_AFTER_HELP: &str = "Examples:
  ullage auth login
  ullage auth login claude
  ullage --reveal auth login claude --account claude-work";
const PROBE_AFTER_HELP: &str = "Examples:
  ullage probe claude-work
  ullage probe claude-work --no-wait";
const SHOW_AFTER_HELP: &str = "Examples:
  ullage show claude-work
  ullage show --all
  ullage show --all --metric usage --metric Codex";
const PROBE_ABOUT: &str = "Query a provider now and persist a usage snapshot";
const PROBE_LONG_ABOUT: &str = "Query a provider now and persist a usage snapshot.

Contacts the provider for one account id and stores a snapshot. By default the \
command waits until that snapshot is ready. --no-wait asks the daemon to start \
the probe and returns immediately with an acknowledgement, without printing \
usage.";
const SHOW_ABOUT: &str = "Print persisted usage snapshots without calling the provider";
const SHOW_LONG_ABOUT: &str = "Print persisted usage snapshots without calling the provider.

Pass an account id, or --all to print every stored snapshot. The readable \
summary hides the rows the account's stored metric filter names; --metric \
keeps only the rows it names for this invocation, and --no-metric-filter \
ignores the stored filter. Both flags affect the readable summary only: --raw \
and JSON output keep every measurement.";

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, ValueEnum)]
pub enum OutputFormat {
    /// Human-readable columns.
    #[default]
    Table,
    /// Compact JSON object.
    Json,
    /// Indented JSON object.
    PrettyJson,
}

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, ValueEnum)]
pub enum ColorMode {
    /// Color table output when stdout is a terminal and `NO_COLOR` is unset or empty.
    #[default]
    Auto,
    /// Always emit ANSI colors on table output.
    Always,
    /// Never emit ANSI colors.
    Never,
}

#[derive(Debug, Parser)]
#[command(
    name = "ullage",
    version,
    about = CLI_ABOUT,
    long_about = CLI_LONG_ABOUT,
    after_help = CLI_AFTER_HELP,
    arg_required_else_help = true
)]
pub struct Cli {
    /// Render stdout as table, json, or pretty-json.
    #[arg(
        long,
        global = true,
        value_enum,
        default_value_t,
        value_name = "FORMAT"
    )]
    pub output: OutputFormat,
    /// Color table output: auto, always, or never.
    ///
    /// `auto` colors stdout when it is a terminal and `NO_COLOR` is unset or
    /// empty. JSON and pretty-json output never include ANSI sequences.
    #[arg(long, global = true, value_enum, default_value_t, value_name = "WHEN")]
    pub color: ColorMode,
    /// Reveal personal and authentication values in output.
    #[arg(long, global = true)]
    pub reveal: bool,
    /// Print the raw provider table instead of the readable usage summary.
    ///
    /// Table output only. `--output json` and `--output pretty-json` ignore it
    /// and always emit the same raw structure.
    #[arg(long, global = true)]
    pub raw: bool,
    /// Show sanitized partial-failure scope and category; on authentication
    /// and probe failures, also attach the provider's own error text.
    ///
    /// `ULLAGE_DIAGNOSE=1` has the same effect for callers that cannot add a flag.
    #[arg(long, global = true)]
    pub diagnose: bool,
    #[command(subcommand)]
    pub command: Command,
}

#[derive(Debug, Subcommand)]
pub enum Command {
    /// Install, run, and inspect the local Ullage daemon.
    #[command(arg_required_else_help = true)]
    Daemon {
        #[command(subcommand)]
        command: DaemonCommand,
    },
    /// Inspect compiled-in providers.
    #[command(arg_required_else_help = true)]
    Provider {
        #[command(subcommand)]
        command: ProviderCommand,
    },
    /// Create and manage local accounts.
    #[command(arg_required_else_help = true)]
    Account {
        #[command(subcommand)]
        command: AccountCommand,
    },
    /// Authenticate accounts with a provider.
    #[command(arg_required_else_help = true)]
    Auth {
        #[command(subcommand)]
        command: AuthCommand,
    },
    // `about`/`long_about` live on `ProbeArgs`/`ShowArgs` so the help text is
    // declared once; doc comments here would duplicate them and drift.
    Probe(ProbeArgs),
    Show(ShowArgs),
    /// Pair, inspect, and revoke HTTP API devices.
    #[command(arg_required_else_help = true, after_help = DEVICE_AFTER_HELP)]
    Device {
        #[command(subcommand)]
        command: DeviceCommand,
    },
}

#[derive(Debug, Subcommand)]
pub enum DeviceCommand {
    /// Create a one-time code for pairing an HTTP API client.
    Pair,
    /// List devices currently allowed to use the HTTP API.
    List,
    /// Revoke one device without prompting for confirmation.
    Revoke {
        /// Stable device identifier shown by `ullage device list`.
        #[arg(value_name = "DEVICE_ID", value_parser = free_text_argument)]
        device_id: String,
    },
}

#[derive(Debug, Subcommand)]
pub enum DaemonCommand {
    /// Register a user-level service that starts the daemon at login.
    ///
    /// Writes a current-user startup entry only. It does not start the daemon;
    /// run `daemon start` afterwards. `uninstall` removes that entry and leaves
    /// configuration, credentials, and snapshots in place.
    #[command(after_help = DAEMON_INSTALL_AFTER_HELP)]
    Install,
    /// Start the installed user-level daemon service.
    Start,
    /// Stop the running user-level daemon service.
    Stop,
    /// Start a detached daemon without installing a user-level service.
    Run,
    /// Report whether the local daemon is reachable and print account and probe counts.
    Status,
    /// Remove the user-level service entry. Config and credentials stay.
    Uninstall,
}

#[derive(Debug, Subcommand)]
pub enum ProviderCommand {
    /// List provider ids and display names.
    List,
}

#[derive(Debug, Subcommand)]
pub enum AccountCommand {
    /// Create an account for a provider id.
    Add {
        /// Provider id such as claude, chatgpt, grok, or cursor. Not a display name.
        #[arg(value_name = "PROVIDER_ID", value_parser = free_text_argument)]
        provider: String,
        /// Account label sent to the provider during probes. Distinct from the account id.
        #[arg(long, value_name = "ACCOUNT_LABEL", value_parser = free_text_argument)]
        label: Option<String>,
    },
    /// List local accounts.
    List,
    /// Show one account by account id.
    Show {
        /// Stable account id, not the account label.
        #[arg(value_name = "ACCOUNT_ID", value_parser = free_text_argument)]
        account: String,
    },
    /// Enable automatic scheduled probing for an account id.
    ///
    /// Manual `probe` still contacts the provider.
    Enable {
        /// Stable account id, not the account label.
        #[arg(value_name = "ACCOUNT_ID", value_parser = free_text_argument)]
        account: String,
    },
    /// Disable automatic scheduled probing for an account id.
    ///
    /// Manual `probe` still contacts the provider.
    Disable {
        /// Stable account id, not the account label.
        #[arg(value_name = "ACCOUNT_ID", value_parser = free_text_argument)]
        account: String,
    },
    /// Set or clear the account label of an account id.
    ///
    /// The label is unique per provider and is sent to the provider during
    /// probes. Omit the label argument to clear it. The account id does not
    /// change.
    Label {
        /// Stable account id, not the account label.
        #[arg(value_name = "ACCOUNT_ID", value_parser = free_text_argument)]
        account: String,
        /// New account label. Omit this argument to clear the current label.
        #[arg(value_name = "ACCOUNT_LABEL", value_parser = free_text_argument)]
        label: Option<String>,
    },
    /// Set or clear the display metric hide list of an account id.
    ///
    /// The stored names are hidden from the account's readable summary; they
    /// match rows by display name, case-insensitively and exactly, and ignore
    /// the window a row belongs to. Omit every METRIC value to clear the list.
    /// Invalid names fail with exit code 64 without contacting the daemon.
    Metrics {
        /// Stable account id, not the account label.
        #[arg(value_name = "ACCOUNT_ID", value_parser = free_text_argument)]
        account: String,
        /// Display metric names to hide. Omit every value to clear.
        #[arg(value_name = "METRIC", value_parser = free_text_argument)]
        metrics: Vec<String>,
    },
    /// Delete an account id and its stored snapshots.
    ///
    /// Does not clear credentials. Run `auth logout` first if the account still
    /// has stored credentials.
    Remove {
        /// Stable account id, not the account label.
        #[arg(value_name = "ACCOUNT_ID", value_parser = free_text_argument)]
        account: String,
    },
}

#[derive(Clone, Copy, Debug, ValueEnum)]
pub enum AuthMethodArg {
    /// Request a browser-based OAuth authorization-code flow.
    BrowserOauth,
    /// Request a device-code flow. Interactive login polls; scripts repeat
    /// `auth complete` while pending.
    DeviceCode,
    /// Request a provider API token, entered without placing it in argv.
    ApiToken,
    /// Request import of an existing provider session, if supported.
    SessionImport,
}

impl From<AuthMethodArg> for AuthMethod {
    fn from(value: AuthMethodArg) -> Self {
        match value {
            AuthMethodArg::BrowserOauth => Self::BrowserOAuth,
            AuthMethodArg::DeviceCode => Self::DeviceCode,
            AuthMethodArg::ApiToken => Self::ApiToken,
            AuthMethodArg::SessionImport => Self::SessionImport,
        }
    }
}

#[derive(Debug, Subcommand)]
pub enum AuthCommand {
    /// Authenticate an account.
    ///
    /// Without `--account`, login is interactive: pick a provider, create or
    /// reuse an account, complete the provider challenge, then set an account
    /// label. That path needs a terminal.
    ///
    /// With `--account`, login starts a non-interactive flow for an existing
    /// account id and prints the challenge. Finish it with `auth complete`.
    /// Flow ids and authorization URIs stay redacted unless `--reveal` is set.
    /// `--method` is a request: the provider may start a different challenge or
    /// reject an unsupported method. The provider argument is a provider id such
    /// as `claude`, not a display name.
    #[command(after_help = AUTH_LOGIN_AFTER_HELP)]
    Login {
        /// Provider id such as claude, chatgpt, grok, or cursor. Not a display name.
        #[arg(value_name = "PROVIDER_ID", value_parser = free_text_argument)]
        provider: Option<String>,
        /// Existing account id. Omit this flag to walk the interactive login flow.
        #[arg(
            long,
            requires = "provider",
            value_name = "ACCOUNT_ID",
            value_parser = free_text_argument
        )]
        account: Option<String>,
        /// Authentication method. Default is the provider's preferred method.
        #[arg(long, value_enum, value_name = "METHOD")]
        method: Option<AuthMethodArg>,
    },
    /// Advance a non-interactive login for an existing account id.
    ///
    /// `FLOW_ID` is the identifier printed by `auth login`. When the provider
    /// asked for a completion value (authorization code or API token), read it
    /// from the environment variable named by `--authorization-code-env`
    /// (default `ULLAGE_AUTH_CODE`), not from process arguments. Leave that
    /// variable unset for device-code flows. One call may return pending until
    /// the user finishes authorization; run the same command again.
    ///
    /// This path does not retire an older account signed in as the same person,
    /// which interactive `auth login` does once the new account works. A script
    /// that re-adds an identity keeps both rows unless it removes the old one
    /// with `account remove`.
    Complete {
        /// Provider id such as claude, chatgpt, grok, or cursor. Not a display name.
        #[arg(value_name = "PROVIDER_ID", value_parser = free_text_argument)]
        provider: String,
        /// Stable account id, not the account label.
        #[arg(long, value_name = "ACCOUNT_ID", value_parser = free_text_argument)]
        account: String,
        /// Flow identifier printed by `auth login`, not a completion value.
        #[arg(value_name = "FLOW_ID", value_parser = free_text_argument)]
        flow_id: String,
        /// OAuth redirect URI to submit with the completion value, when required.
        #[arg(long, value_name = "URI", value_parser = free_text_argument)]
        redirect_uri: Option<String>,
        /// Environment variable that holds the provider-requested completion
        /// value (authorization code or API token). Leave unset for device-code
        /// flows. The value is never accepted as a process argument.
        #[arg(
            long,
            default_value = "ULLAGE_AUTH_CODE",
            value_name = "ENV_VAR",
            value_parser = free_text_argument
        )]
        authorization_code_env: String,
    },
    /// Show the stored authentication state for an account id.
    Status {
        /// Provider id such as claude, chatgpt, grok, or cursor. Not a display name.
        #[arg(value_name = "PROVIDER_ID", value_parser = free_text_argument)]
        provider: String,
        /// Stable account id, not the account label.
        #[arg(long, value_name = "ACCOUNT_ID", value_parser = free_text_argument)]
        account: String,
    },
    /// Forget stored credentials for an account id.
    Logout {
        /// Provider id such as claude, chatgpt, grok, or cursor. Not a display name.
        #[arg(value_name = "PROVIDER_ID", value_parser = free_text_argument)]
        provider: String,
        /// Stable account id, not the account label.
        #[arg(long, value_name = "ACCOUNT_ID", value_parser = free_text_argument)]
        account: String,
        /// Optional account label forwarded to the provider with logout. Some
        /// providers ignore it.
        #[arg(long, value_name = "ACCOUNT_LABEL", value_parser = free_text_argument)]
        account_label: Option<String>,
    },
}

#[derive(Debug, Args)]
#[command(
    about = PROBE_ABOUT,
    long_about = PROBE_LONG_ABOUT,
    after_help = PROBE_AFTER_HELP
)]
pub struct ProbeArgs {
    /// Stable account id, not the account label.
    #[arg(value_name = "ACCOUNT_ID", value_parser = free_text_argument)]
    pub account: String,
    /// Start the probe and return immediately without waiting for usage.
    #[arg(long = "no-wait", action = clap::ArgAction::SetFalse, default_value_t = true)]
    pub wait: bool,
}

#[derive(Debug, Args)]
#[command(
    about = SHOW_ABOUT,
    long_about = SHOW_LONG_ABOUT,
    after_help = SHOW_AFTER_HELP
)]
pub struct ShowArgs {
    /// Stable account id, not the account label. Conflicts with `--all`.
    #[arg(
        required_unless_present = "all",
        conflicts_with = "all",
        value_name = "ACCOUNT_ID",
        value_parser = free_text_argument
    )]
    pub account: Option<String>,
    /// Print every stored snapshot instead of selecting one account id.
    #[arg(long)]
    pub all: bool,
    /// Keep only summary rows whose display name matches.
    ///
    /// Matching is exact and case-insensitive and ignores the window a row
    /// belongs to. Repeat the flag to keep the union of several names.
    #[arg(
        long = "metric",
        value_name = "METRIC",
        conflicts_with = "no_metric_filter",
        value_parser = free_text_argument
    )]
    pub metric: Vec<String>,
    /// Ignore the account's stored metric filter for this invocation.
    #[arg(long = "no-metric-filter")]
    pub no_metric_filter: bool,
}

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

pub fn run_from<I, T>(arguments: I, client: &dyn ControlClient) -> RunOutput
where
    I: IntoIterator<Item = T>,
    T: Into<OsString> + Clone,
{
    run_from_with(arguments, client, &mut prompt::TerminalPrompt::new())
}

pub fn run_from_with<I, T>(
    arguments: I,
    client: &dyn ControlClient,
    prompt: &mut dyn prompt::Prompt,
) -> RunOutput
where
    I: IntoIterator<Item = T>,
    T: Into<OsString> + Clone,
{
    let arguments = arguments.into_iter().map(Into::into).collect::<Vec<_>>();
    let requested_format = infer_output_format(&arguments);
    let cli = match Cli::try_parse_from(&arguments) {
        Ok(cli) => cli,
        Err(error) => {
            if let Some(param) = unsafe_control_in_raw_arguments(&arguments) {
                return unsafe_control_error_output(requested_format, param);
            }
            return match error.kind() {
                clap::error::ErrorKind::DisplayHelp | clap::error::ErrorKind::DisplayVersion => {
                    success_text(&error.to_string())
                }
                _ => parse_error_output(&error, requested_format),
            };
        }
    };
    execute_with(cli, client, prompt)
}

pub fn execute(cli: Cli, client: &dyn ControlClient) -> RunOutput {
    execute_with(cli, client, &mut prompt::TerminalPrompt::new())
}

pub fn execute_with(
    cli: Cli,
    client: &dyn ControlClient,
    prompt: &mut dyn prompt::Prompt,
) -> RunOutput {
    if let Some(param) = unsafe_control_param_name(&cli.command) {
        return unsafe_control_error_output(cli.output, param);
    }
    // Invalid metric names never reach the daemon: surface the usage error
    // before the control request is built.
    let metric_choice = match metric_filter_choice(&cli.command) {
        Ok(choice) => choice,
        Err(()) => {
            return error_output(ExitCode::Usage, "invalid_account_metrics", cli.output);
        }
    };
    // Only the account views that own the metric filter show it: the mutation
    // commands keep their single-row table so existing output stays stable.
    let view = RenderView {
        metric_choice: &metric_choice,
        account_with_metrics: matches!(
            cli.command,
            Command::Account {
                command: AccountCommand::Show { .. } | AccountCommand::Metrics { .. }
            }
        ),
    };
    if let Command::Auth {
        command:
            AuthCommand::Login {
                provider,
                account: None,
                method,
            },
    } = &cli.command
    {
        return login::interactive_login(client, prompt, provider.as_deref(), *method, &cli);
    }
    if matches!(
        cli.command,
        Command::Daemon {
            command: DaemonCommand::Run
        }
    ) {
        return match client.run_daemon() {
            Ok(()) => render_result(
                ControlResult::Ack,
                cli.output,
                cli.reveal,
                cli.raw,
                false,
                cli.color,
                &view,
            ),
            Err(ClientError::InvalidEndpoint) => {
                error_output(ExitCode::Usage, "invalid_control_socket", cli.output)
            }
            Err(ClientError::DaemonProcessOutput(detail)) => error_output_with_options(
                ExitCode::Failure,
                "daemon_process_failed",
                cli.output,
                Some(detail),
                None,
            ),
            Err(_) => error_output(ExitCode::Failure, "daemon_process_failed", cli.output),
        };
    }
    let service_action = match cli.command {
        Command::Daemon {
            command: DaemonCommand::Install,
        } => Some(ServiceAction::Install),
        Command::Daemon {
            command: DaemonCommand::Start,
        } => Some(ServiceAction::Start),
        Command::Daemon {
            command: DaemonCommand::Stop,
        } => Some(ServiceAction::Stop),
        Command::Daemon {
            command: DaemonCommand::Uninstall,
        } => Some(ServiceAction::Uninstall),
        _ => None,
    };
    if let Some(action) = service_action {
        return match client.manage_daemon(action) {
            Ok(()) => render_result(
                ControlResult::Ack,
                cli.output,
                cli.reveal,
                cli.raw,
                false,
                cli.color,
                &view,
            ),
            Err(ClientError::InvalidEndpoint) => {
                error_output(ExitCode::Usage, "invalid_control_socket", cli.output)
            }
            Err(ClientError::DaemonStillRunning) => error_output_with_options(
                ExitCode::Failure,
                "daemon_service_failed",
                cli.output,
                None,
                Some(
                    "a daemon still answers on the control endpoint; stop it where it was \
                     started (`ullage daemon run` terminal or service manager)"
                        .into(),
                ),
            ),
            Err(_) => error_output(ExitCode::Failure, "daemon_service_failed", cli.output),
        };
    }

    let command = to_control_command(&cli.command);
    let diagnose = diagnostics_requested(cli.diagnose) && command.accepts_diagnostics();
    let request_id = next_request_id();
    let request = ControlRequest::new(&request_id, command).with_diagnostics(diagnose);
    let mut response = match client.send(&request) {
        Ok(response) => response,
        Err(ClientError::DaemonUnavailable)
            if matches!(
                cli.command,
                Command::Daemon {
                    command: DaemonCommand::Status
                }
            ) =>
        {
            return match client.daemon_service_installed() {
                Ok(installed) => render_stopped_service(installed, cli.output, cli.color),
                Err(_) => error_output(
                    ExitCode::DaemonUnavailable,
                    "daemon_unavailable",
                    cli.output,
                ),
            };
        }
        Err(ClientError::DaemonUnavailable) => {
            return error_output(
                ExitCode::DaemonUnavailable,
                "daemon_unavailable",
                cli.output,
            );
        }
        Err(ClientError::InvalidEndpoint) => {
            return error_output(ExitCode::Usage, "invalid_control_socket", cli.output);
        }
        // `send` only transports requests: a process-management variant from a
        // `ControlClient` implementation is its own kind of failure, not a
        // protocol problem.
        Err(ClientError::DaemonProcess | ClientError::DaemonStillRunning) => {
            return error_output(ExitCode::Failure, "daemon_process_failed", cli.output);
        }
        Err(ClientError::DaemonProcessOutput(detail)) => {
            return error_output_with_options(
                ExitCode::Failure,
                "daemon_process_failed",
                cli.output,
                Some(detail),
                None,
            );
        }
        Err(ClientError::InvalidResponse) => {
            return error_output(
                ExitCode::ProtocolError,
                "invalid_daemon_response",
                cli.output,
            );
        }
    };

    sanitize_partial_failure_controls(&mut response.result);
    let diagnostic_allowed = diagnose
        && matches!(response.result, ControlResult::Error(_))
        && matches!(cli.command, Command::Auth { .. } | Command::Probe(_));
    let diagnostic_is_safe = response
        .diagnostic
        .as_deref()
        .is_none_or(|detail| !detail.chars().any(is_unsafe_control));
    if response.version != CONTROL_PROTOCOL_VERSION
        || response.request_id != request_id
        || !response_matches_command(&cli.command, &response.result)
        || (response.diagnostic.is_some() && !diagnostic_allowed)
        || !diagnostic_is_safe
    {
        return error_output(
            ExitCode::ProtocolError,
            "invalid_daemon_response",
            cli.output,
        );
    }

    let mut output = render_result(
        response.result,
        cli.output,
        cli.reveal,
        cli.raw,
        diagnose,
        cli.color,
        &view,
    );
    if let Some(detail) = response.diagnostic {
        output.stderr = with_diagnostic(&output.stderr, &detail, cli.output);
    }
    output
}

/// Diagnostics are opt-in per invocation: the flag, or the environment variable
/// for callers that cannot add a flag.
fn diagnostics_requested(flag: bool) -> bool {
    flag || std::env::var_os("ULLAGE_DIAGNOSE").is_some_and(|value| value == "1")
}

/// Appends provider error text to table stderr, or to a JSON error envelope when
/// one is already present. For `AuthState::Invalid`, JSON keeps `reason` on
/// stdout and table writes `detail:` on stderr.
fn with_diagnostic(stderr: &str, detail: &str, format: OutputFormat) -> String {
    match format {
        OutputFormat::Table => format!("{stderr}detail: {detail}\n"),
        OutputFormat::Json | OutputFormat::PrettyJson => {
            let Ok(mut value) = serde_json::from_str::<serde_json::Value>(stderr) else {
                return stderr.to_owned();
            };
            if let Some(error) = value
                .get_mut("error")
                .and_then(serde_json::Value::as_object_mut)
            {
                error.insert(
                    "detail".into(),
                    serde_json::Value::String(detail.to_owned()),
                );
            }
            let encoded = if format == OutputFormat::PrettyJson {
                serde_json::to_string_pretty(&value)
            } else {
                serde_json::to_string(&value)
            };
            encoded
                .map(|encoded| format!("{encoded}\n"))
                .unwrap_or_else(|_| stderr.to_owned())
        }
    }
}

/// Defense in depth for a `Cli` built without `try_parse_from`: every
/// free-text argument is already rejected at parse time by
/// `free_text_argument`; this traversal repeats that check so a
/// programmatically constructed command cannot smuggle control characters to
/// the daemon either. New `String` fields must be listed here as well.
fn unsafe_control_param_name(command: &Command) -> Option<&'static str> {
    let contains = |value: &str| value.chars().any(is_unsafe_control);
    match command {
        Command::Daemon { .. }
        | Command::Provider { .. }
        | Command::Device {
            command: DeviceCommand::Pair | DeviceCommand::List,
        }
        | Command::Account {
            command: AccountCommand::List,
        } => None,
        Command::Device {
            command: DeviceCommand::Revoke { device_id },
        } => {
            if contains(device_id) {
                Some("DEVICE_ID")
            } else {
                None
            }
        }
        Command::Account {
            command: AccountCommand::Add { provider, label },
        } => {
            if contains(provider) {
                Some("PROVIDER_ID")
            } else if label.as_deref().is_some_and(contains) {
                Some("--label")
            } else {
                None
            }
        }
        Command::Account {
            command:
                AccountCommand::Show { account }
                | AccountCommand::Enable { account }
                | AccountCommand::Disable { account }
                | AccountCommand::Remove { account },
        }
        | Command::Probe(ProbeArgs { account, .. }) => {
            if contains(account) {
                Some("ACCOUNT_ID")
            } else {
                None
            }
        }
        Command::Account {
            command: AccountCommand::Metrics { account, metrics },
        } => {
            if contains(account) {
                Some("ACCOUNT_ID")
            } else if metrics.iter().any(|metric| contains(metric)) {
                Some("METRIC")
            } else {
                None
            }
        }
        Command::Account {
            command: AccountCommand::Label { account, label },
        } => {
            if contains(account) {
                Some("ACCOUNT_ID")
            } else if label.as_deref().is_some_and(contains) {
                Some("ACCOUNT_LABEL")
            } else {
                None
            }
        }
        Command::Auth { command } => match command {
            AuthCommand::Login {
                provider, account, ..
            } => {
                if provider.as_deref().is_some_and(contains) {
                    Some("PROVIDER_ID")
                } else if account.as_deref().is_some_and(contains) {
                    Some("--account")
                } else {
                    None
                }
            }
            AuthCommand::Status { provider, account } => {
                if contains(provider) {
                    Some("PROVIDER_ID")
                } else if contains(account) {
                    Some("--account")
                } else {
                    None
                }
            }
            AuthCommand::Complete {
                provider,
                account,
                flow_id,
                redirect_uri,
                authorization_code_env,
            } => {
                if contains(provider) {
                    Some("PROVIDER_ID")
                } else if contains(account) {
                    Some("--account")
                } else if contains(flow_id) {
                    Some("FLOW_ID")
                } else if redirect_uri.as_deref().is_some_and(contains) {
                    Some("--redirect-uri")
                } else if contains(authorization_code_env) {
                    Some("--authorization-code-env")
                } else if std::env::var(authorization_code_env).is_ok_and(|value| contains(&value))
                {
                    Some("authorization_code_env")
                } else {
                    None
                }
            }
            AuthCommand::Logout {
                provider,
                account,
                account_label,
            } => {
                if contains(provider) {
                    Some("PROVIDER_ID")
                } else if contains(account) {
                    Some("--account")
                } else if account_label.as_deref().is_some_and(contains) {
                    Some("--account-label")
                } else {
                    None
                }
            }
        },
        Command::Show(args) => {
            if args.account.as_deref().is_some_and(contains) {
                Some("ACCOUNT_ID")
            } else if args.metric.iter().any(|metric| contains(metric)) {
                Some("--metric")
            } else {
                None
            }
        }
    }
}

/// Which metric filter the readable summary applies to stored snapshots.
#[derive(Clone, Debug, Default)]
pub(crate) enum MetricFilterChoice {
    /// Apply each account's persisted filter as a hide list; the default for
    /// `show` and `probe`.
    #[default]
    Persisted,
    /// Apply one filter as a keep list to every account: `--metric` or
    /// `--no-metric-filter`.
    Explicit(MetricFilter),
}

/// The readable-view choices that one command resolved before rendering.
pub(crate) struct RenderView<'a> {
    pub(crate) metric_choice: &'a MetricFilterChoice,
    /// True only for the account views that own the metric filter.
    pub(crate) account_with_metrics: bool,
}

impl RenderView<'_> {
    /// Persisted filters and the mutation account table: the login flow view.
    pub(crate) fn persisted() -> Self {
        Self {
            metric_choice: &MetricFilterChoice::Persisted,
            account_with_metrics: false,
        }
    }
}

impl MetricFilterChoice {
    pub(crate) fn for_saved(&self, saved: &[String]) -> ResolvedMetricFilter {
        match self {
            Self::Explicit(filter) => ResolvedMetricFilter::explicit(filter.clone()),
            Self::Persisted => ResolvedMetricFilter::persisted(saved),
        }
    }
}

/// The names a validated filter stores: trimmed and deduplicated.
fn normalized_metric_names(names: &[String]) -> Vec<String> {
    MetricFilter::new(names.to_vec())
        .map(|filter| filter.names().to_vec())
        .unwrap_or_else(|_| names.to_vec())
}

/// Resolves the metric filter for this invocation and rejects invalid names
/// before any control request is built.
fn metric_filter_choice(command: &Command) -> Result<MetricFilterChoice, ()> {
    match command {
        Command::Show(args) if !args.metric.is_empty() => MetricFilter::new(args.metric.clone())
            .map(MetricFilterChoice::Explicit)
            .map_err(|_| ()),
        // An inactive filter shows every row, which is what the flag asks for.
        Command::Show(args) if args.no_metric_filter => {
            Ok(MetricFilterChoice::Explicit(MetricFilter::default()))
        }
        Command::Account {
            command: AccountCommand::Metrics { metrics, .. },
        } => MetricFilter::new(metrics.clone())
            .map(|_| MetricFilterChoice::Persisted)
            .map_err(|_| ()),
        _ => Ok(MetricFilterChoice::Persisted),
    }
}

fn to_control_command(command: &Command) -> ControlCommand {
    match command {
        Command::Daemon {
            command: DaemonCommand::Status,
        } => ControlCommand::DaemonStatus,
        Command::Daemon {
            command:
                DaemonCommand::Install
                | DaemonCommand::Start
                | DaemonCommand::Stop
                | DaemonCommand::Run
                | DaemonCommand::Uninstall,
        } => unreachable!(),
        Command::Device { command } => match command {
            DeviceCommand::Pair => ControlCommand::CreatePairCode,
            DeviceCommand::List => ControlCommand::ListDevices,
            DeviceCommand::Revoke { device_id } => ControlCommand::RevokeDevice {
                device_id: device_id.clone(),
            },
        },
        Command::Provider {
            command: ProviderCommand::List,
        } => ControlCommand::ListProviders,
        Command::Account { command } => match command {
            AccountCommand::Add { provider, label } => ControlCommand::AddAccount {
                provider: ProviderId::new(provider),
                label: label.clone(),
            },
            AccountCommand::List => ControlCommand::ListAccounts,
            AccountCommand::Show { account } => ControlCommand::ShowAccount {
                account: AccountId::new(account),
            },
            AccountCommand::Enable { account } => ControlCommand::SetAccountEnabled {
                account: AccountId::new(account),
                enabled: true,
            },
            AccountCommand::Disable { account } => ControlCommand::SetAccountEnabled {
                account: AccountId::new(account),
                enabled: false,
            },
            AccountCommand::Label { account, label } => ControlCommand::SetAccountLabel {
                account: AccountId::new(account),
                label: label.clone(),
            },
            AccountCommand::Metrics { account, metrics } => ControlCommand::SetAccountMetrics {
                account: AccountId::new(account),
                // `metric_filter_choice` already rejected invalid names, so
                // normalizing here keeps the request identical to what the
                // daemon stores and returns.
                metrics: normalized_metric_names(metrics),
            },
            AccountCommand::Remove { account } => ControlCommand::RemoveAccount {
                account: AccountId::new(account),
            },
        },
        Command::Auth { command } => match command {
            AuthCommand::Login {
                provider: Some(provider),
                account: Some(account),
                method,
            } => ControlCommand::StartAuth {
                provider: ProviderId::new(provider),
                account: AccountId::new(account),
                request: AuthStartRequest {
                    method: method.map(Into::into),
                    redirect_uri: None,
                },
            },
            // `execute_with` routes a login without an account to the
            // interactive flow before this point.
            AuthCommand::Login { .. } => unreachable!(),
            AuthCommand::Status { provider, account } => ControlCommand::AuthStatus {
                provider: ProviderId::new(provider),
                account: AccountId::new(account),
            },
            AuthCommand::Complete {
                provider,
                account,
                flow_id,
                redirect_uri,
                authorization_code_env,
            } => ControlCommand::CompleteAuth {
                provider: ProviderId::new(provider),
                account: AccountId::new(account),
                request: AuthCompleteRequest {
                    flow_id: flow_id.clone(),
                    authorization_code: std::env::var(authorization_code_env).ok(),
                    redirect_uri: redirect_uri.clone(),
                },
            },
            AuthCommand::Logout {
                provider,
                account,
                account_label,
            } => ControlCommand::Logout {
                provider: ProviderId::new(provider),
                account: AccountId::new(account),
                request: LogoutRequest {
                    account_label: account_label.clone(),
                },
            },
        },
        Command::Probe(args) => ControlCommand::Probe {
            account_id: args.account.clone(),
            wait: args.wait,
        },
        Command::Show(args) => ControlCommand::Show {
            account_id: args.account.clone(),
        },
    }
}

fn response_matches_command(command: &Command, result: &ControlResult) -> bool {
    if result_contains_unsafe_control(result) {
        return false;
    }
    match (command, result) {
        (_, ControlResult::ProtocolMismatch { .. }) => true,
        (command, ControlResult::Error(error)) => error_matches_command(command, error),
        (
            Command::Daemon {
                command: DaemonCommand::Status,
            },
            ControlResult::DaemonStatus(status),
        ) => daemon_status_payload_is_well_formed(status),
        (
            Command::Provider {
                command: ProviderCommand::List,
            },
            ControlResult::Providers(providers),
        ) => providers.iter().enumerate().all(|(index, provider)| {
            providers[index + 1..]
                .iter()
                .all(|other| other.id != provider.id)
        }),
        (
            Command::Device {
                command: DeviceCommand::Pair,
            },
            ControlResult::PairCode(pair_code),
        ) => pair_code_is_valid(pair_code),
        (
            Command::Device {
                command: DeviceCommand::List,
            },
            ControlResult::Devices(devices),
        ) => devices.iter().enumerate().all(|(index, device)| {
            !device.id.is_empty()
                && !device.name.is_empty()
                && device.last_seen_at >= device.created_at
                && devices[index + 1..]
                    .iter()
                    .all(|other| other.id != device.id)
        }),
        (
            Command::Device {
                command: DeviceCommand::Revoke { .. },
            },
            ControlResult::Ack,
        ) => true,
        (
            Command::Account {
                command: AccountCommand::List,
            },
            ControlResult::Accounts(accounts),
        ) => accounts.iter().enumerate().all(|(index, account)| {
            accounts[index + 1..]
                .iter()
                .all(|other| other.id != account.id)
        }),
        (
            Command::Account {
                command: AccountCommand::Add { provider, label },
            },
            ControlResult::Account(account),
        ) => {
            account.provider.as_str() == provider
                && account.label.as_deref() == label.as_deref()
                && account.enabled
        }
        (
            Command::Account {
                command: AccountCommand::Show { account: requested },
            },
            ControlResult::Account(account),
        ) => account.id.as_str() == requested,
        (
            Command::Account {
                command: AccountCommand::Enable { account: requested },
            },
            ControlResult::Account(account),
        ) => account.id.as_str() == requested && account.enabled,
        (
            Command::Account {
                command: AccountCommand::Disable { account: requested },
            },
            ControlResult::Account(account),
        ) => account.id.as_str() == requested && !account.enabled,
        (
            Command::Account {
                command:
                    AccountCommand::Label {
                        account: requested,
                        label,
                    },
            },
            ControlResult::Account(account),
        ) => account.id.as_str() == requested && account.label.as_deref() == label.as_deref(),
        (
            Command::Account {
                command:
                    AccountCommand::Metrics {
                        account: requested,
                        metrics,
                    },
            },
            ControlResult::Account(account),
        ) => {
            account.id.as_str() == requested && account.metrics == normalized_metric_names(metrics)
        }
        (
            Command::Account {
                command: AccountCommand::Remove { .. },
            },
            ControlResult::Ack,
        )
        | (
            Command::Auth {
                command: AuthCommand::Status { .. },
            },
            ControlResult::AuthState(_),
        )
        | (
            Command::Auth {
                command: AuthCommand::Logout { .. },
            },
            ControlResult::Ack,
        )
        | (
            Command::Auth {
                command: AuthCommand::Complete { .. },
            },
            ControlResult::AuthState(_),
        ) => true,
        (
            Command::Auth {
                command: AuthCommand::Login { .. },
            },
            ControlResult::AuthChallenge(_),
        ) => true,
        (
            Command::Probe(ProbeArgs {
                account: requested,
                wait: true,
            }),
            ControlResult::Probe(payload),
        ) => payload.account_id == *requested && usage_outcome_is_valid(&payload.usage),
        (Command::Probe(ProbeArgs { wait: false, .. }), ControlResult::Ack) => true,
        (Command::Show(args), ControlResult::Snapshots(snapshots)) => match &args.account {
            Some(requested) => snapshots_match(snapshots, requested),
            None => snapshots_are_unique(snapshots),
        },
        _ => false,
    }
}

/// The payload checks a `DaemonStatus` reply must pass to be trusted, shared
/// by the response validator and the readiness probe.
fn daemon_status_payload_is_well_formed(status: &DaemonStatusPayload) -> bool {
    status.accounts.iter().enumerate().all(|(index, account)| {
        (!account.stale || account.has_snapshot)
            && status.accounts[index + 1..]
                .iter()
                .all(|other| other.account_id != account.account_id)
    })
}

/// Rejects control and bidirectional-override characters in a free-text CLI
/// value at parse time. Every `String`/`Vec<String>` argument carries this
/// `value_parser`; `unsafe_control_param_name` below remains the belt for a
/// `Cli` constructed without parsing.
fn free_text_argument(value: &str) -> Result<String, String> {
    if value.chars().any(is_unsafe_control) {
        return Err("contains disallowed control characters".into());
    }
    Ok(value.to_owned())
}

fn result_contains_unsafe_control(result: &ControlResult) -> bool {
    serde_json::to_value(result)
        .map(|value| value_contains_unsafe_control(&value))
        .unwrap_or(true)
}

fn pair_code_is_valid(pair_code: &PairCodePayload) -> bool {
    let bytes = pair_code.code.as_bytes();
    bytes.len() == 7
        && bytes[3] == b'-'
        && bytes
            .iter()
            .enumerate()
            .all(|(index, byte)| index == 3 || b"23456789ABCDEFGHJKMNPQRSTVWXYZ".contains(byte))
}

fn value_contains_unsafe_control(value: &serde_json::Value) -> bool {
    match value {
        serde_json::Value::String(value) => value.chars().any(is_unsafe_control),
        serde_json::Value::Array(values) => values.iter().any(value_contains_unsafe_control),
        serde_json::Value::Object(values) => values.iter().any(|(key, value)| {
            key.chars().any(is_unsafe_control) || value_contains_unsafe_control(value)
        }),
        serde_json::Value::Null | serde_json::Value::Bool(_) | serde_json::Value::Number(_) => {
            false
        }
    }
}

fn error_matches_command(command: &Command, error: &ControlError) -> bool {
    match error {
        ControlError::Account(AccountError::Duplicate(_)) => matches!(
            command,
            Command::Account {
                command: AccountCommand::Add { .. } | AccountCommand::Label { .. }
            }
        ),
        ControlError::Account(AccountError::NotFound(account)) => match command {
            Command::Account {
                command:
                    AccountCommand::Show { account: requested }
                    | AccountCommand::Enable { account: requested }
                    | AccountCommand::Disable { account: requested }
                    | AccountCommand::Label {
                        account: requested, ..
                    }
                    | AccountCommand::Metrics {
                        account: requested, ..
                    }
                    | AccountCommand::Remove { account: requested },
            } => account.as_str() == requested,
            Command::Auth { command } => match command {
                AuthCommand::Login {
                    account: requested, ..
                } => requested.as_deref() == Some(account.as_str()),
                AuthCommand::Complete {
                    account: requested, ..
                }
                | AuthCommand::Status {
                    account: requested, ..
                }
                | AuthCommand::Logout {
                    account: requested, ..
                } => account.as_str() == requested,
            },
            _ => false,
        },
        ControlError::Provider(_) => matches!(
            command,
            Command::Auth { .. } | Command::Probe(ProbeArgs { wait: true, .. })
        ),
        ControlError::Registry(RegistryError::NotFound(provider)) => match command {
            Command::Account {
                command:
                    AccountCommand::Add {
                        provider: requested,
                        ..
                    },
            } => provider.as_str() == requested,
            Command::Auth { command } => auth_command_names_provider(command, provider),
            Command::Probe(ProbeArgs { wait: true, .. }) => true,
            _ => false,
        },
        ControlError::Registry(RegistryError::InstanceUnavailable(provider))
        | ControlError::Registry(RegistryError::InstanceFailed { provider, .. }) => match command {
            Command::Auth { command } => auth_command_names_provider(command, provider),
            Command::Probe(ProbeArgs { wait: true, .. }) => true,
            _ => false,
        },
        ControlError::Registry(
            RegistryError::Duplicate(_)
            | RegistryError::InvalidId(_)
            | RegistryError::DescriptorMismatch { .. },
        ) => false,
        ControlError::AccountNotFound { account_id } => match command {
            Command::Probe(args) => args.account == *account_id,
            Command::Show(ShowArgs {
                account: Some(requested),
                ..
            }) => requested == account_id,
            _ => false,
        },
        ControlError::AccountSelectorNotFound { .. } => {
            matches!(command, Command::Probe(_))
        }
        ControlError::DeviceNotFound { device_id } => matches!(
            command,
            Command::Device {
                command: DeviceCommand::Revoke {
                    device_id: requested,
                },
            } if requested == device_id
        ),
        ControlError::Timeout => matches!(command, Command::Probe(ProbeArgs { wait: true, .. })),
        ControlError::Cancelled => matches!(
            command,
            Command::Probe(_)
                | Command::Account {
                    command: AccountCommand::Remove { .. }
                }
        ),
        ControlError::Storage => matches!(
            command,
            Command::Device { .. }
                | Command::Probe(ProbeArgs { wait: true, .. })
                | Command::Account {
                    command: AccountCommand::Add { .. }
                        | AccountCommand::Enable { .. }
                        | AccountCommand::Disable { .. }
                        | AccountCommand::Label { .. }
                        | AccountCommand::Metrics { .. }
                        | AccountCommand::Remove { .. }
                }
        ),
        ControlError::InvalidAccountMetrics => matches!(
            command,
            Command::Account {
                command: AccountCommand::Metrics { .. }
            }
        ),
        ControlError::UnsupportedCommand | ControlError::InvalidRequest => false,
    }
}

fn auth_command_names_provider(command: &AuthCommand, provider: &ProviderId) -> bool {
    match command {
        AuthCommand::Login {
            provider: requested,
            ..
        } => requested.as_deref() == Some(provider.as_str()),
        AuthCommand::Status {
            provider: requested,
            ..
        }
        | AuthCommand::Complete {
            provider: requested,
            ..
        }
        | AuthCommand::Logout {
            provider: requested,
            ..
        } => provider.as_str() == requested,
    }
}

fn snapshots_match(snapshots: &[SnapshotPayload], requested: &str) -> bool {
    snapshots.len() <= 1
        && snapshots
            .iter()
            .all(|snapshot| snapshot.account_id == requested && snapshot_is_valid(snapshot))
}

fn snapshots_are_unique(snapshots: &[SnapshotPayload]) -> bool {
    snapshots.iter().enumerate().all(|(index, snapshot)| {
        snapshot_is_valid(snapshot)
            && snapshots[index + 1..]
                .iter()
                .all(|other| other.account_id != snapshot.account_id)
    })
}

fn snapshot_is_valid(snapshot: &SnapshotPayload) -> bool {
    usage_outcome_is_valid(&snapshot.usage)
        && if snapshot.stale {
            snapshot.last_error.is_some() && snapshot.last_error_at.is_some()
        } else {
            snapshot.last_error.is_none() && snapshot.last_error_at.is_none()
        }
}

fn usage_outcome_is_valid(outcome: &QueryOutcome<SubscriptionUsage>) -> bool {
    match outcome {
        QueryOutcome::Complete { .. } => true,
        QueryOutcome::Partial { failures, .. } => !failures.is_empty(),
    }
}

fn next_request_id() -> String {
    format!(
        "cli-{}-{}",
        std::process::id(),
        REQUEST_SEQUENCE.fetch_add(1, Ordering::Relaxed)
    )
}

fn render_result(
    result: ControlResult,
    format: OutputFormat,
    reveal: bool,
    raw: bool,
    diagnose: bool,
    color: ColorMode,
    view: &RenderView<'_>,
) -> RunOutput {
    let code = result_exit_code(&result);
    if let ControlResult::Error(error) = &result {
        return error_output_with_options(
            code,
            control_error_kind(error),
            format,
            None,
            error_hint_for_control(error),
        );
    }
    if matches!(result, ControlResult::ProtocolMismatch { .. }) {
        return error_output(ExitCode::ProtocolError, "protocol_mismatch", format);
    }

    let invalid_detail = match &result {
        ControlResult::AuthState(AuthState::Invalid { reason, .. })
            if diagnose && !reason.chars().any(is_unsafe_control) =>
        {
            Some(reason.clone())
        }
        _ => None,
    };
    let stdout = match format {
        OutputFormat::Json | OutputFormat::PrettyJson => {
            let mut output_result = result.clone();
            redact_error_details(&mut output_result, diagnose);
            if !reveal {
                redact_revealable_values(&mut output_result);
            }
            json_line(&output_result, format == OutputFormat::PrettyJson)
        }
        OutputFormat::Table => human_result(
            &result,
            reveal,
            raw,
            diagnose,
            &Palette::from_mode(color),
            view.metric_choice,
            view.account_with_metrics,
        ),
    };
    let mut output = RunOutput {
        stdout,
        stderr: String::new(),
        code,
    };
    if let Some(detail) = invalid_detail {
        output.stderr = with_diagnostic(&output.stderr, &detail, format);
    }
    output
}

fn result_exit_code(result: &ControlResult) -> ExitCode {
    match result {
        ControlResult::Probe(payload) if matches!(payload.usage, QueryOutcome::Partial { .. }) => {
            ExitCode::Partial
        }
        ControlResult::Snapshots(snapshots)
            if snapshots
                .iter()
                .any(|snapshot| matches!(snapshot.usage, QueryOutcome::Partial { .. })) =>
        {
            ExitCode::Partial
        }
        ControlResult::AuthState(AuthState::Invalid { .. }) => ExitCode::AuthenticationInvalid,
        ControlResult::Error(ControlError::Provider(ProviderError::AuthenticationInvalid {
            ..
        })) => ExitCode::AuthenticationInvalid,
        ControlResult::Error(ControlError::Provider(ProviderError::Network { .. })) => {
            ExitCode::NetworkFailure
        }
        ControlResult::Error(ControlError::InvalidAccountMetrics) => ExitCode::Usage,
        ControlResult::ProtocolMismatch { .. } => ExitCode::ProtocolError,
        ControlResult::Error(_) => ExitCode::Failure,
        _ => ExitCode::Success,
    }
}

fn control_error_kind(error: &ControlError) -> &'static str {
    match error {
        ControlError::Account(AccountError::NotFound(_)) => "account_not_found",
        ControlError::Account(AccountError::Duplicate(_)) => "account_duplicate",
        ControlError::Provider(ProviderError::AuthenticationInvalid { .. }) => {
            "authentication_invalid"
        }
        ControlError::Provider(ProviderError::RateLimited { .. }) => "rate_limited",
        ControlError::Provider(ProviderError::Network { .. }) => "network_failure",
        ControlError::Provider(ProviderError::ProtocolIncompatible { .. }) => {
            "provider_protocol_incompatible"
        }
        ControlError::Provider(ProviderError::UnsupportedCapability { .. }) => {
            "unsupported_capability"
        }
        ControlError::Registry(_) => "provider_registry_error",
        ControlError::AccountNotFound { .. } => "account_not_found",
        ControlError::AccountSelectorNotFound { .. } => "account_selector_not_found",
        ControlError::DeviceNotFound { .. } => "device_not_found",
        ControlError::Timeout => "timeout",
        ControlError::Cancelled => "cancelled",
        ControlError::Storage => "storage",
        ControlError::InvalidAccountMetrics => "invalid_account_metrics",
        ControlError::InvalidRequest => "invalid_request",
        ControlError::UnsupportedCommand => "unsupported_command",
    }
}

#[derive(Serialize)]
struct ErrorEnvelope<'a> {
    status: &'static str,
    error: ErrorBody<'a>,
}

#[derive(Serialize)]
struct ErrorBody<'a> {
    kind: &'a str,
    #[serde(skip_serializing_if = "Option::is_none")]
    message: Option<&'a str>,
    #[serde(skip_serializing_if = "Option::is_none")]
    hint: Option<&'a str>,
}

pub(crate) fn error_output(code: ExitCode, kind: &str, format: OutputFormat) -> RunOutput {
    error_output_with_options(
        code,
        kind,
        format,
        None,
        error_hint(kind).map(str::to_owned),
    )
}

pub(crate) fn error_output_with_options(
    code: ExitCode,
    kind: &str,
    format: OutputFormat,
    message: Option<String>,
    hint: Option<String>,
) -> RunOutput {
    let envelope = ErrorEnvelope {
        status: "error",
        error: ErrorBody {
            kind,
            message: message.as_deref(),
            hint: hint.as_deref(),
        },
    };
    let stderr = match format {
        OutputFormat::Table => {
            let mut stderr = format!("error: {kind}\n");
            if let Some(message) = message {
                stderr.push_str(&message);
                if !stderr.ends_with('\n') {
                    stderr.push('\n');
                }
            }
            if let Some(hint) = hint {
                stderr.push_str("hint: ");
                stderr.push_str(&hint);
                if !stderr.ends_with('\n') {
                    stderr.push('\n');
                }
            }
            stderr
        }
        OutputFormat::Json => format!("{}\n", serde_json::to_string(&envelope).unwrap()),
        OutputFormat::PrettyJson => {
            format!("{}\n", serde_json::to_string_pretty(&envelope).unwrap())
        }
    };
    RunOutput {
        stdout: String::new(),
        stderr,
        code,
    }
}

fn success_text(text: &str) -> RunOutput {
    RunOutput {
        stdout: text.into(),
        stderr: String::new(),
        code: ExitCode::Success,
    }
}

fn json_line<T: Serialize>(value: &T, pretty: bool) -> String {
    let encoded = if pretty {
        serde_json::to_string_pretty(value)
    } else {
        serde_json::to_string(value)
    }
    .expect("protocol values are serializable");
    format!("{encoded}\n")
}

fn infer_output_format(arguments: &[OsString]) -> OutputFormat {
    let mut values = arguments.iter().skip(1).filter_map(|value| value.to_str());
    while let Some(value) = values.next() {
        if value == "--" {
            break;
        }
        let selected = if value == "--output" {
            values.next()
        } else {
            value.strip_prefix("--output=")
        };
        match selected {
            Some("json") => return OutputFormat::Json,
            Some("pretty-json") => return OutputFormat::PrettyJson,
            Some("table") => return OutputFormat::Table,
            _ => {}
        }
    }
    OutputFormat::Table
}

fn parse_error_output(error: &clap::Error, format: OutputFormat) -> RunOutput {
    let message = sanitize_cli_text(&parse_error_message(error));
    match format {
        OutputFormat::Table => RunOutput {
            stdout: String::new(),
            stderr: if message.ends_with('\n') {
                message
            } else {
                format!("{message}\n")
            },
            code: ExitCode::Usage,
        },
        OutputFormat::Json | OutputFormat::PrettyJson => {
            error_output_with_options(ExitCode::Usage, "usage", format, Some(message), None)
        }
    }
}

fn parse_error_message(error: &clap::Error) -> String {
    match error.kind() {
        ErrorKind::DisplayHelpOnMissingArgumentOrSubcommand => error.to_string(),
        ErrorKind::MissingSubcommand => missing_subcommand_help(error),
        _ => error.to_string(),
    }
}

fn missing_subcommand_help(error: &clap::Error) -> String {
    let Some(ContextValue::String(parent)) = error.get(ContextKind::InvalidSubcommand) else {
        let mut command = Cli::command();
        command.build();
        return command.render_long_help().to_string();
    };
    let segments: Vec<&str> = parent.split_whitespace().collect();
    let mut command = Cli::command();
    command.build();
    if segments.len() <= 1 {
        return command.render_long_help().to_string();
    }
    render_long_help_for_path(&mut command, &segments[1..])
}

fn render_long_help_for_path(command: &mut clap::Command, path: &[&str]) -> String {
    let Some(subcommand) = command.find_subcommand_mut(path[0]) else {
        return command.render_long_help().to_string();
    };
    if path.len() == 1 {
        return subcommand.render_long_help().to_string();
    }
    render_long_help_for_path(subcommand, &path[1..])
}

fn unsafe_control_error_output(format: OutputFormat, param: &'static str) -> RunOutput {
    error_output_with_options(
        ExitCode::Usage,
        "usage",
        format,
        None,
        Some(unsafe_control_reject_hint(param).into()),
    )
}

fn unsafe_control_reject_hint(param: &'static str) -> &'static str {
    match param {
        "authorization_code_env" => {
            "the environment variable referenced by --authorization-code-env \
                                     contains disallowed control characters"
        }
        "argument" => "a command argument contains disallowed control characters",
        "--label" => "the --label argument contains disallowed control characters",
        "--account" => "the --account argument contains disallowed control characters",
        "--account-label" => "the --account-label argument contains disallowed control characters",
        "--redirect-uri" => "the --redirect-uri argument contains disallowed control characters",
        "--authorization-code-env" => {
            "the --authorization-code-env argument contains disallowed control characters"
        }
        "--metric" => "the --metric argument contains disallowed control characters",
        "--method" => "the --method argument contains disallowed control characters",
        "--output" => "the --output argument contains disallowed control characters",
        "--color" => "the --color argument contains disallowed control characters",
        "PROVIDER_ID" | "ACCOUNT_ID" | "FLOW_ID" | "ACCOUNT_LABEL" | "DEVICE_ID" | "METRIC" => {
            "a command argument contains disallowed control characters"
        }
        _ => "a command argument contains disallowed control characters",
    }
}

fn unsafe_control_in_raw_arguments(arguments: &[OsString]) -> Option<&'static str> {
    let args: Vec<&str> = arguments
        .iter()
        .filter_map(|value| value.to_str())
        .collect();
    let mut after_option_terminator = false;
    for (index, arg) in args.iter().enumerate().skip(1) {
        if *arg == "--" {
            after_option_terminator = true;
            continue;
        }
        if !arg.chars().any(is_unsafe_control) {
            continue;
        }
        if after_option_terminator {
            return Some("argument");
        }
        if arg.starts_with("--") {
            let flag = arg.split('=').next().unwrap_or(arg);
            if let Some(label) = known_value_flag_label(flag) {
                return Some(label);
            }
            return Some("argument");
        }
        // `index` is never 0: the iterator skips the program name.
        if let Some(label) = known_value_flag_label(args[index - 1]) {
            return Some(label);
        }
        return Some("argument");
    }
    None
}

fn known_value_flag_label(flag: &str) -> Option<&'static str> {
    match flag {
        "--label" => Some("--label"),
        "--account" => Some("--account"),
        "--account-label" => Some("--account-label"),
        "--redirect-uri" => Some("--redirect-uri"),
        "--authorization-code-env" => Some("--authorization-code-env"),
        "--metric" => Some("--metric"),
        "--method" => Some("--method"),
        "--output" => Some("--output"),
        "--color" => Some("--color"),
        _ => None,
    }
}

pub(crate) fn error_hint_for_control(error: &ControlError) -> Option<String> {
    match error {
        ControlError::Registry(RegistryError::NotFound(_)) => Some(
            "pass a compiled-in provider id such as claude, chatgpt, grok, or cursor; run \
             `ullage provider list` to see ids"
                .into(),
        ),
        ControlError::Registry(RegistryError::InstanceUnavailable(_)) => None,
        _ => error_hint(control_error_kind(error)).map(str::to_owned),
    }
}

pub(crate) fn error_hint(kind: &str) -> Option<&'static str> {
    match kind {
        "daemon_unavailable" => Some(
            "start the daemon with `ullage daemon install` then `ullage daemon start`, or run \
             `ullage daemon run` in another terminal",
        ),
        "provider_registry_error" => Some(
            "pass a compiled-in provider id such as claude, chatgpt, grok, or cursor; run \
             `ullage provider list` to see ids",
        ),
        "account_not_found" | "account_selector_not_found" => {
            Some("pass a stable account id from `ullage account list`, not a label or display name")
        }
        "invalid_account_metrics" => Some(
            "pass display metric names such as `usage` or `Codex`; repeat the flag or argument to \
             name several metrics",
        ),
        "invalid_control_socket" => Some(
            "set ULLAGE_CONTROL_SOCKET (or ULLAGE_CONTROL_PIPE on Windows) to an absolute path \
             for a private socket owned by the current user, or unset it",
        ),
        _ => None,
    }
}

fn sanitize_cli_text(text: &str) -> String {
    let mut sanitized = String::with_capacity(text.len());
    let mut chars = text.chars().peekable();
    while let Some(character) = chars.next() {
        if character == '\u{1b}' {
            match chars.peek() {
                // CSI: parameter and intermediate bytes run until any final
                // byte in 0x40..=0x7e; a truncated sequence is dropped with
                // the rest of the input.
                Some('[') => {
                    chars.next();
                    for next in chars.by_ref() {
                        if ('\u{40}'..='\u{7e}').contains(&next) {
                            break;
                        }
                    }
                }
                // OSC-family sequences (OSC, DCS, SOS, PM, APC) run until BEL,
                // a C1 ST, or an ESC \ pair; a truncated one is dropped with
                // the rest of the input.
                Some(']' | 'P' | 'X' | '^' | '_') => {
                    chars.next();
                    strip_until_string_terminator(&mut chars);
                }
                _ => {}
            }
            continue;
        }
        if character == '\n' || !character.is_control() && !is_unsafe_control(character) {
            sanitized.push(character);
        }
    }
    sanitized
}

/// Consumes an OSC-family payload through BEL, C1 ST, or ESC \ — whichever
/// comes first — or through end of input when the sequence is truncated.
fn strip_until_string_terminator(chars: &mut std::iter::Peekable<std::str::Chars<'_>>) {
    let mut escaped = false;
    for next in chars.by_ref() {
        if next == '\u{7}' || next == '\u{9c}' || (escaped && next == '\\') {
            break;
        }
        escaped = next == '\u{1b}';
    }
}

fn redact_error_details(result: &mut ControlResult, diagnose: bool) {
    match result {
        ControlResult::Probe(payload) => redact_failures(&mut payload.usage, diagnose),
        ControlResult::Snapshots(snapshots) => {
            for snapshot in snapshots {
                redact_failures(&mut snapshot.usage, diagnose);
            }
        }
        ControlResult::AuthState(AuthState::Invalid { reason, .. }) if !diagnose => {
            *reason = "[redacted]".into();
        }
        _ => {}
    }
}

fn redact_revealable_values(result: &mut ControlResult) {
    match result {
        ControlResult::Accounts(accounts) => {
            for account in accounts {
                redact_account(account);
            }
        }
        ControlResult::Account(account) => redact_account(account),
        ControlResult::Probe(payload) => match &mut payload.usage {
            QueryOutcome::Complete { data } | QueryOutcome::Partial { data, .. } => {
                redact_usage(data);
            }
        },
        ControlResult::Snapshots(snapshots) => {
            for snapshot in snapshots {
                match &mut snapshot.usage {
                    QueryOutcome::Complete { data } | QueryOutcome::Partial { data, .. } => {
                        redact_usage(data);
                    }
                }
            }
        }
        ControlResult::AuthChallenge(challenge) => {
            challenge.flow_id = "[redacted]".into();
            if challenge.verification_uri.is_some() {
                challenge.verification_uri = Some("[redacted]".into());
            }
            if challenge.user_code.is_some() {
                challenge.user_code = Some("[redacted]".into());
            }
        }
        ControlResult::AuthState(state) => match state {
            // `account_key` needs no redaction: it is already a digest, which
            // is all anything compares.
            AuthState::Authenticated { account_label, .. } => {
                if account_label.is_some() {
                    *account_label = Some("[redacted]".into());
                }
            }
            AuthState::Invalid { .. } => {}
            AuthState::Pending { flow_id, .. } => *flow_id = "[redacted]".into(),
            AuthState::NotAuthenticated => {}
        },
        ControlResult::DaemonStatus(_)
        | ControlResult::PairCode(_)
        | ControlResult::Devices(_)
        | ControlResult::Providers(_)
        | ControlResult::Workspaces(_)
        | ControlResult::Workspace(_)
        | ControlResult::Usage(_)
        | ControlResult::Ack
        | ControlResult::Error(_)
        | ControlResult::ProtocolMismatch { .. } => {}
    }
}

fn redact_account(account: &mut Account) {
    if account.label.is_some() {
        account.label = Some("[redacted]".into());
    }
}

fn redact_usage(usage: &mut SubscriptionUsage) {
    if usage.account_label.is_some() {
        usage.account_label = Some("[redacted]".into());
    }
}

fn sanitize_partial_failure_controls(result: &mut ControlResult) {
    match result {
        ControlResult::Probe(payload) => redact_unsafe_failure_text(&mut payload.usage),
        ControlResult::Snapshots(snapshots) => {
            for snapshot in snapshots {
                redact_unsafe_failure_text(&mut snapshot.usage);
            }
        }
        _ => {}
    }
}

fn redact_unsafe_failure_text<T>(outcome: &mut QueryOutcome<T>) {
    if let QueryOutcome::Partial { failures, .. } = outcome {
        for failure in failures {
            if failure.scope.chars().any(is_unsafe_control) {
                failure.scope = "[redacted]".into();
            }
            if failure.message.chars().any(is_unsafe_control) {
                failure.message = "[redacted]".into();
            }
        }
    }
}

fn redact_failures<T>(outcome: &mut QueryOutcome<T>, diagnose: bool) {
    if diagnose {
        return;
    }
    if let QueryOutcome::Partial { failures, .. } = outcome {
        for failure in failures {
            failure.scope = "[redacted]".into();
            failure.message = "[redacted]".into();
        }
    }
}

fn display_sensitive(value: &str, reveal: bool) -> String {
    if reveal {
        sanitize_cell(value).into()
    } else if value.is_empty() {
        "-".into()
    } else {
        "[redacted]".into()
    }
}

#[cfg(test)]
mod error_guidance_tests {
    use super::*;

    #[test]
    fn account_selector_not_found_includes_hint() {
        let output = error_output(
            ExitCode::Failure,
            "account_selector_not_found",
            OutputFormat::Table,
        );
        assert!(output.stderr.contains("account_selector_not_found"));
        assert!(output.stderr.contains("hint: pass a stable account id"));
    }

    #[test]
    fn missing_subcommand_context_uses_command_path() {
        let error = Cli::try_parse_from(["ullage", "account", "--reveal"]).unwrap_err();
        assert_eq!(error.kind(), ErrorKind::MissingSubcommand);
        let parent = match error.get(ContextKind::InvalidSubcommand) {
            Some(ContextValue::String(parent)) => parent.as_str(),
            _ => panic!("missing InvalidSubcommand context"),
        };
        assert!(parent.contains("account"), "{parent}");
        let help = missing_subcommand_help(&error);
        assert!(help.contains("Usage: ullage account"), "{help}");
        assert!(!help.contains("ullage daemon install"), "{help}");
    }

    #[test]
    fn sanitize_cli_text_strips_controls_and_ansi() {
        let sanitized = sanitize_cli_text("bad\u{1b}[31mvalue\u{009b}text");
        assert_eq!(sanitized, "badvaluetext");
    }

    #[test]
    fn sanitize_cli_text_ends_csi_on_any_final_byte() {
        for (input, expected) in [
            // SGR, erase-line, and private cursor sequences each end at their
            // own final byte and must not swallow the text behind them.
            ("bad\u{1b}[31mvalue", "badvalue"),
            ("\u{1b}[2Kprompt", "prompt"),
            ("show\u{1b}[?25l me", "show me"),
            ("\u{1b}[1;2Hhere", "here"),
            ("a\u{1b}[0Kb", "ab"),
        ] {
            assert_eq!(sanitize_cli_text(input), expected, "{input:?}");
        }
    }

    #[test]
    fn sanitize_cli_text_strips_osc_through_bel_or_st() {
        for (input, expected) in [
            // BEL-terminated OSC: the whole payload, not just the ESC, goes.
            ("pre\u{1b}]8;;https://evil.invalid\u{7}post", "prepost"),
            ("pre\u{1b}]0;window title\u{7}post", "prepost"),
            // ST-terminated OSC (ESC \ and C1 ST spellings).
            ("pre\u{1b}]8;;x\u{1b}\\post", "prepost"),
            ("pre\u{1b}]8;;x\u{9c}post", "prepost"),
            // DCS and APC use the same string terminators as OSC.
            ("pre\u{1b}Ppayload\u{1b}\\post", "prepost"),
            ("pre\u{1b}_payload\u{7}post", "prepost"),
        ] {
            assert_eq!(sanitize_cli_text(input), expected, "{input:?}");
        }
    }

    #[test]
    fn sanitize_cli_text_drops_truncated_sequences() {
        for (input, expected) in [
            ("visible\u{1b}[31", "visible"),
            ("visible\u{1b}]8;;never-terminated", "visible"),
            ("visible\u{1b}", "visible"),
            ("kept\u{1b}[malso kept", "keptalso kept"),
        ] {
            assert_eq!(sanitize_cli_text(input), expected, "{input:?}");
        }
    }
}

#[cfg(all(test, unix))]
mod tests {
    use std::io::{BufRead, BufReader, Write};
    use std::os::unix::fs::PermissionsExt;
    use std::os::unix::net::UnixListener;

    use ullage_protocol::CredentialBackendId;

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
