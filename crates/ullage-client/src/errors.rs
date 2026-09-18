//! Error envelopes, hints, and output sanitization.
//!
//! Everything the daemon or the user can influence is treated as hostile text:
//! table cells are scrubbed, revealable values are redacted unless `--reveal`,
//! and parse or control failures serialize to a stable JSON envelope.

use std::ffi::OsString;

use clap::CommandFactory;
use clap::error::{ContextKind, ContextValue, ErrorKind};
use serde::Serialize;
use ullage_core::is_unsafe_identity_character as is_unsafe_control;
use ullage_protocol::{
    Account, AccountError, AuthState, ControlError, ControlResult, ProviderError, QueryOutcome,
    RegistryError, SubscriptionUsage,
};

use crate::cli::{Cli, OutputFormat};
use crate::render::sanitize_cell;
use crate::{ExitCode, RunOutput};

/// Diagnostics are opt-in per invocation: the flag, or the environment variable
/// for callers that cannot add a flag.
pub(crate) fn diagnostics_requested(flag: bool) -> bool {
    flag || std::env::var_os("ULLAGE_DIAGNOSE").is_some_and(|value| value == "1")
}

/// Appends provider error text to table stderr, or to a JSON error envelope when
/// one is already present. For `AuthState::Invalid`, JSON keeps `reason` on
/// stdout and table writes `detail:` on stderr.
pub(crate) fn with_diagnostic(stderr: &str, detail: &str, format: OutputFormat) -> String {
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

pub(crate) fn result_exit_code(result: &ControlResult) -> ExitCode {
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

pub(crate) fn control_error_kind(error: &ControlError) -> &'static str {
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

pub(crate) fn success_text(text: &str) -> RunOutput {
    RunOutput {
        stdout: text.into(),
        stderr: String::new(),
        code: ExitCode::Success,
    }
}

pub(crate) fn json_line<T: Serialize>(value: &T, pretty: bool) -> String {
    let encoded = if pretty {
        serde_json::to_string_pretty(value)
    } else {
        serde_json::to_string(value)
    }
    .expect("protocol values are serializable");
    format!("{encoded}\n")
}

pub(crate) fn infer_output_format(arguments: &[OsString]) -> OutputFormat {
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

pub(crate) fn parse_error_output(error: &clap::Error, format: OutputFormat) -> RunOutput {
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

pub(crate) fn unsafe_control_error_output(format: OutputFormat, param: &'static str) -> RunOutput {
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

pub(crate) fn unsafe_control_in_raw_arguments(arguments: &[OsString]) -> Option<&'static str> {
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
            "pass a compiled-in provider id such as claude, chatgpt, grok, cursor, opencode, devin, codex2api, or sub2api; run \
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
            "pass a compiled-in provider id such as claude, chatgpt, grok, cursor, opencode, devin, codex2api, or sub2api; run \
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

pub(crate) fn sanitize_cli_text(text: &str) -> String {
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

pub(crate) fn redact_error_details(result: &mut ControlResult, diagnose: bool) {
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

pub(crate) fn redact_revealable_values(result: &mut ControlResult) {
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

pub(crate) fn sanitize_partial_failure_controls(result: &mut ControlResult) {
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

pub(crate) fn display_sensitive(value: &str, reveal: bool) -> String {
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
    use clap::Parser;

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
