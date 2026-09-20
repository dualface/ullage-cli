//! The `ullage` client library: command dispatch, the control transport, and
//! the CLI schema the binary embeds.
//!
//! `lib.rs` keeps the dispatch core (`run_from`, `execute_with`) and the
//! shared client types. The schema lives in [`cli`], socket/pipe transport in
//! [`transport`], reply validation in [`validate`], and error envelopes and
//! sanitization in [`errors`]; each command registers once in
//! `validate::spec_for`.

use std::ffi::OsString;
use std::sync::atomic::{AtomicU64, Ordering};

use clap::Parser;
use thiserror::Error;
use ullage_core::is_unsafe_identity_character as is_unsafe_control;
use ullage_protocol::{CONTROL_PROTOCOL_VERSION, ControlRequest, ControlResponse, ControlResult};

mod cli;
mod errors;
mod login;
pub mod prompt;
mod render;
mod service;
mod table;
mod transport;
mod validate;

#[cfg(test)]
mod summary_render_tests;

pub use cli::{
    AccountCommand, AuthCommand, AuthMethodArg, Cli, ColorMode, Command, DaemonCommand,
    DeviceCommand, OutputFormat, ProbeArgs, ProviderCommand, ShowArgs,
};
pub use transport::{SystemClient, control_endpoint_from_environment};

pub(crate) use errors::{
    control_error_kind, error_hint, error_hint_for_control, error_output_with_options,
    result_exit_code, with_diagnostic,
};
pub(crate) use render::{RenderView, render_result, sanitize_cell};
pub(crate) use validate::{response_matches_command, to_control_command};

use errors::{
    diagnostics_requested, error_output, infer_output_format, parse_error_output,
    sanitize_partial_failure_controls, success_text, unsafe_control_error_output,
    unsafe_control_in_raw_arguments,
};
use render::render_stopped_service;
use validate::{metric_filter_choice, unsafe_control_param_name};

static REQUEST_SEQUENCE: AtomicU64 = AtomicU64::new(1);

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

fn next_request_id() -> String {
    format!(
        "cli-{}-{}",
        std::process::id(),
        REQUEST_SEQUENCE.fetch_add(1, Ordering::Relaxed)
    )
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
    if let Some(notice) = daemon_upgrade_notice(response.daemon_version.as_deref()) {
        output.stderr.insert_str(0, &format!("{notice}\n"));
    }
    output
}

/// Dotted numeric triple for build versions like `0.1.6`; a `-suffix`
/// (pre-release or local build tag) is ignored. Anything else is not
/// comparable and yields `None`.
fn parse_build_version(version: &str) -> Option<(u64, u64, u64)> {
    let mut parts = version.split(['.', '-']);
    let major = parts.next()?.parse().ok()?;
    let minor = parts.next()?.parse().ok()?;
    let patch = parts.next().unwrap_or("0").parse().ok()?;
    Some((major, minor, patch))
}

/// Warning text when the answering daemon is older than this CLI build. A
/// missing `daemon_version` means the daemon predates the field, which is
/// always older than a client that understands it. Unparseable versions
/// stay silent rather than warn on a build that may simply be newer.
pub(crate) fn daemon_upgrade_notice(daemon_version: Option<&str>) -> Option<String> {
    let cli_version = env!("CARGO_PKG_VERSION");
    let older = match daemon_version {
        None => true,
        Some(version) => match (
            parse_build_version(version),
            parse_build_version(cli_version),
        ) {
            (Some(daemon), Some(cli)) => daemon < cli,
            _ => false,
        },
    };
    if !older {
        return None;
    }
    let daemon = daemon_version.unwrap_or("unknown, predates version reporting");
    Some(format!(
        "warning: the running daemon ({daemon}) is older than this CLI ({cli_version}); \
         upgrades and new providers stay invisible until it is restarted\n  upgrade: \
         ullage daemon install"
    ))
}
