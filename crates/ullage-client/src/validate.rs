//! Command specs and response validation.
//!
//! A daemon reply is trusted only when it answers the command that was sent:
//! [`spec_for`] registers every `Command` variant once, and each
//! [`CommandSpec`] carries that variant's defense-in-depth parameter check, its
//! wire command, and the result and error shapes that legitimately answer it.
//! Adding a command means adding one `spec_for` arm and one spec — the four
//! lookups below never diverge because they all read the same spec.

use ullage_core::is_unsafe_identity_character as is_unsafe_control;
use ullage_core::summary::MetricFilter;
use ullage_protocol::{
    AccountError, AccountId, AuthCompleteRequest, AuthStartRequest, ControlCommand, ControlError,
    ControlResult, DaemonStatusPayload, LogoutRequest, PairCodePayload, ProviderId, QueryOutcome,
    RegistryError, SnapshotPayload, SubscriptionUsage,
};

use crate::cli::{
    AccountCommand, AuthCommand, Command, DaemonCommand, DeviceCommand, ProbeArgs, ProviderCommand,
    ShowArgs,
};
use crate::render::MetricFilterChoice;

/// Everything the dispatch layer must know about one `Command` variant.
struct CommandSpec {
    /// Names the first free-text field holding an unsafe control character.
    /// A `Cli` built by `try_parse_from` is already clean; this repeats the
    /// check for a programmatically constructed command.
    unsafe_param: fn(&Command) -> Option<&'static str>,
    /// The wire command this variant sends. Variants the dispatch layer
    /// resolves locally (service management, interactive login) panic here,
    /// as the old `unreachable!()` arms did.
    control: fn(&Command) -> ControlCommand,
    /// Whether a non-error `ControlResult` legitimately answers the command.
    response: fn(&Command, &ControlResult) -> bool,
    /// Whether a daemon-reported `ControlError` legitimately answers it.
    error: fn(&Command, &ControlError) -> bool,
}

/// The single registration table: one arm per command variant.
fn spec_for(command: &Command) -> &'static CommandSpec {
    match command {
        Command::Daemon {
            command: DaemonCommand::Status,
        } => &DAEMON_STATUS,
        Command::Daemon {
            command:
                DaemonCommand::Install
                | DaemonCommand::Start
                | DaemonCommand::Stop
                | DaemonCommand::Run
                | DaemonCommand::Uninstall,
        } => &NOT_DISPATCHED,
        Command::Provider {
            command: ProviderCommand::List,
        } => &PROVIDER_LIST,
        Command::Device { command } => match command {
            DeviceCommand::Pair => &DEVICE_PAIR,
            DeviceCommand::List => &DEVICE_LIST,
            DeviceCommand::Revoke { .. } => &DEVICE_REVOKE,
        },
        Command::Account { command } => match command {
            AccountCommand::Add { .. } => &ACCOUNT_ADD,
            AccountCommand::List => &ACCOUNT_LIST,
            AccountCommand::Show { .. } => &ACCOUNT_SHOW,
            AccountCommand::Enable { .. } => &ACCOUNT_ENABLE,
            AccountCommand::Disable { .. } => &ACCOUNT_DISABLE,
            AccountCommand::Label { .. } => &ACCOUNT_LABEL,
            AccountCommand::Metrics { .. } => &ACCOUNT_METRICS,
            AccountCommand::Remove { .. } => &ACCOUNT_REMOVE,
        },
        Command::Auth { command } => match command {
            AuthCommand::Login { .. } => &AUTH_LOGIN,
            AuthCommand::Status { .. } => &AUTH_STATUS,
            AuthCommand::Complete { .. } => &AUTH_COMPLETE,
            AuthCommand::Logout { .. } => &AUTH_LOGOUT,
        },
        Command::Probe(_) => &PROBE,
        Command::Show(_) => &SHOW,
    }
}

/// The wire command `command` sends to the daemon.
pub(crate) fn to_control_command(command: &Command) -> ControlCommand {
    (spec_for(command).control)(command)
}

/// Defense in depth for a `Cli` built without `try_parse_from`: every
/// free-text argument is already rejected at parse time by
/// `free_text_argument`; this repeats that check so a programmatically
/// constructed command cannot smuggle control characters to the daemon either.
pub(crate) fn unsafe_control_param_name(command: &Command) -> Option<&'static str> {
    (spec_for(command).unsafe_param)(command)
}

/// Whether `result` is a well-formed, command-shaped answer. Unsafe control
/// characters anywhere in the payload already fail it; a `ProtocolMismatch`
/// answers every command, and a `ControlError` is matched by the command's own
/// spec.
pub(crate) fn response_matches_command(command: &Command, result: &ControlResult) -> bool {
    if result_contains_unsafe_control(result) {
        return false;
    }
    match result {
        ControlResult::ProtocolMismatch { .. } => true,
        ControlResult::Error(error) => (spec_for(command).error)(command, error),
        _ => (spec_for(command).response)(command, result),
    }
}

fn has_unsafe_control(value: &str) -> bool {
    value.chars().any(is_unsafe_control)
}

/// `Some(label)` when `value` carries a control character, else `None`.
fn unsafe_field(value: &str, label: &'static str) -> Option<&'static str> {
    has_unsafe_control(value).then_some(label)
}

/// Same check for an optional field.
fn unsafe_option(value: &Option<String>, label: &'static str) -> Option<&'static str> {
    value
        .as_deref()
        .and_then(|value| unsafe_field(value, label))
}

fn no_unsafe_param(_: &Command) -> Option<&'static str> {
    None
}

fn no_response(_: &Command, _: &ControlResult) -> bool {
    false
}

fn no_error(_: &Command, _: &ControlError) -> bool {
    false
}

/// A command that resolves without a daemon round trip.
fn never_dispatched(_: &Command) -> ControlCommand {
    unreachable!()
}

fn unwrap_device_revoke(command: &Command) -> &String {
    let Command::Device {
        command: DeviceCommand::Revoke { device_id },
    } = command
    else {
        unreachable!("spec registered for another variant")
    };
    device_id
}

fn account_field(command: &Command) -> &String {
    let Command::Account { command } = command else {
        unreachable!("spec registered for another variant")
    };
    match command {
        AccountCommand::Show { account }
        | AccountCommand::Enable { account }
        | AccountCommand::Disable { account }
        | AccountCommand::Label { account, .. }
        | AccountCommand::Metrics { account, .. }
        | AccountCommand::Remove { account } => account,
        AccountCommand::Add { .. } | AccountCommand::List => {
            unreachable!("spec registered for another variant")
        }
    }
}

/// The `ACCOUNT_ID` check shared by every spec whose only free-text field is
/// the account id (`account show/enable/disable/remove` and `probe`).
fn unsafe_account_param(command: &Command) -> Option<&'static str> {
    match command {
        Command::Account { .. } => unsafe_field(account_field(command), "ACCOUNT_ID"),
        Command::Probe(args) => unsafe_field(&args.account, "ACCOUNT_ID"),
        _ => unreachable!("spec registered for another variant"),
    }
}

/// Whether the error names this command's account id, for the account
/// commands that take one (`show/enable/disable/label/metrics/remove`).
fn error_names_own_account(command: &Command, error: &ControlError) -> bool {
    match error {
        ControlError::Account(AccountError::NotFound(account)) => {
            account.as_str() == account_field(command)
        }
        _ => false,
    }
}

/// Whether the error names a provider this `auth` subcommand was invoked with.
fn error_names_own_provider(command: &AuthCommand, provider: &ProviderId) -> bool {
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

fn unwrap_auth(command: &Command) -> &AuthCommand {
    let Command::Auth { command } = command else {
        unreachable!("spec registered for another variant")
    };
    command
}

fn unwrap_probe(command: &Command) -> &ProbeArgs {
    let Command::Probe(args) = command else {
        unreachable!("spec registered for another variant")
    };
    args
}

fn unwrap_show(command: &Command) -> &ShowArgs {
    let Command::Show(args) = command else {
        unreachable!("spec registered for another variant")
    };
    args
}

/// Errors an `auth` subcommand may legitimately receive: provider failures and
/// registry/account errors that name the request's own provider or account.
fn auth_error(command: &AuthCommand, error: &ControlError) -> bool {
    match error {
        ControlError::Provider(_) => true,
        ControlError::Account(AccountError::NotFound(account)) => match command {
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
        ControlError::Registry(
            RegistryError::NotFound(provider)
            | RegistryError::InstanceUnavailable(provider)
            | RegistryError::InstanceFailed { provider, .. },
        ) => error_names_own_provider(command, provider),
        _ => false,
    }
}

/// Errors `probe --wait` may legitimately receive: it crosses provider and
/// registry boundaries that fire-and-forget probes never wait for.
fn probe_error(args: &ProbeArgs, error: &ControlError) -> bool {
    match error {
        ControlError::AccountNotFound { account_id } => args.account == *account_id,
        ControlError::AccountSelectorNotFound { .. } => true,
        ControlError::Cancelled => true,
        ControlError::Provider(_)
        | ControlError::Registry(
            RegistryError::NotFound(_)
            | RegistryError::InstanceUnavailable(_)
            | RegistryError::InstanceFailed { .. },
        )
        | ControlError::Timeout
        | ControlError::Storage => args.wait,
        _ => false,
    }
}

static DAEMON_STATUS: CommandSpec = CommandSpec {
    unsafe_param: no_unsafe_param,
    control: |_| ControlCommand::DaemonStatus,
    response: |_, result| {
        matches!(
            result,
            ControlResult::DaemonStatus(status) if daemon_status_payload_is_well_formed(status)
        )
    },
    error: no_error,
};

/// Daemon service management is resolved locally by `manage_daemon`; no wire
/// command exists, so any reply is invalid by construction.
static NOT_DISPATCHED: CommandSpec = CommandSpec {
    unsafe_param: no_unsafe_param,
    control: never_dispatched,
    response: no_response,
    error: no_error,
};

static PROVIDER_LIST: CommandSpec = CommandSpec {
    unsafe_param: no_unsafe_param,
    control: |_| ControlCommand::ListProviders,
    response: |_, result| match result {
        ControlResult::Providers(providers) => {
            providers.iter().enumerate().all(|(index, provider)| {
                providers[index + 1..]
                    .iter()
                    .all(|other| other.id != provider.id)
            })
        }
        _ => false,
    },
    error: no_error,
};

static DEVICE_PAIR: CommandSpec = CommandSpec {
    unsafe_param: no_unsafe_param,
    control: |_| ControlCommand::CreatePairCode,
    response: |_, result| match result {
        ControlResult::PairCode(pair_code) => pair_code_is_valid(pair_code),
        _ => false,
    },
    error: |_, error| matches!(error, ControlError::Storage),
};

static DEVICE_LIST: CommandSpec = CommandSpec {
    unsafe_param: no_unsafe_param,
    control: |_| ControlCommand::ListDevices,
    response: |_, result| match result {
        ControlResult::Devices(devices) => devices.iter().enumerate().all(|(index, device)| {
            !device.id.is_empty()
                && !device.name.is_empty()
                && device.last_seen_at >= device.created_at
                && devices[index + 1..]
                    .iter()
                    .all(|other| other.id != device.id)
        }),
        _ => false,
    },
    error: |_, error| matches!(error, ControlError::Storage),
};

static DEVICE_REVOKE: CommandSpec = CommandSpec {
    unsafe_param: |command| unsafe_field(unwrap_device_revoke(command), "DEVICE_ID"),
    control: |command| ControlCommand::RevokeDevice {
        device_id: unwrap_device_revoke(command).clone(),
    },
    response: |_, result| matches!(result, ControlResult::Ack),
    error: |command, error| match error {
        ControlError::Storage => true,
        ControlError::DeviceNotFound { device_id } => unwrap_device_revoke(command) == device_id,
        _ => false,
    },
};

static ACCOUNT_ADD: CommandSpec = CommandSpec {
    unsafe_param: |command| {
        let Command::Account {
            command: AccountCommand::Add { provider, label },
        } = command
        else {
            unreachable!("spec registered for another variant")
        };
        unsafe_field(provider, "PROVIDER_ID").or_else(|| unsafe_option(label, "--label"))
    },
    control: |command| {
        let Command::Account {
            command: AccountCommand::Add { provider, label },
        } = command
        else {
            unreachable!("spec registered for another variant")
        };
        ControlCommand::AddAccount {
            provider: ProviderId::new(provider),
            label: label.clone(),
        }
    },
    response: |command, result| {
        let Command::Account {
            command: AccountCommand::Add { provider, label },
        } = command
        else {
            unreachable!("spec registered for another variant")
        };
        match result {
            ControlResult::Account(account) => {
                account.provider.as_str() == provider
                    && account.label.as_deref() == label.as_deref()
                    && account.enabled
            }
            _ => false,
        }
    },
    error: |command, error| match error {
        ControlError::Account(AccountError::Duplicate(_)) => true,
        ControlError::Registry(RegistryError::NotFound(provider)) => {
            let Command::Account {
                command:
                    AccountCommand::Add {
                        provider: requested,
                        ..
                    },
            } = command
            else {
                unreachable!("spec registered for another variant")
            };
            provider.as_str() == requested
        }
        ControlError::Storage => true,
        _ => false,
    },
};

static ACCOUNT_LIST: CommandSpec = CommandSpec {
    unsafe_param: no_unsafe_param,
    control: |_| ControlCommand::ListAccounts,
    response: |_, result| match result {
        ControlResult::Accounts(accounts) => accounts.iter().enumerate().all(|(index, account)| {
            accounts[index + 1..]
                .iter()
                .all(|other| other.id != account.id)
        }),
        _ => false,
    },
    error: no_error,
};

static ACCOUNT_SHOW: CommandSpec = CommandSpec {
    unsafe_param: unsafe_account_param,
    control: |command| ControlCommand::ShowAccount {
        account: AccountId::new(account_field(command)),
    },
    response: |command, result| match result {
        ControlResult::Account(account) => account.id.as_str() == account_field(command),
        _ => false,
    },
    error: error_names_own_account,
};

static ACCOUNT_ENABLE: CommandSpec = CommandSpec {
    unsafe_param: unsafe_account_param,
    control: |command| ControlCommand::SetAccountEnabled {
        account: AccountId::new(account_field(command)),
        enabled: true,
    },
    response: |command, result| match result {
        ControlResult::Account(account) => {
            account.id.as_str() == account_field(command) && account.enabled
        }
        _ => false,
    },
    error: |command, error| {
        matches!(error, ControlError::Storage) || error_names_own_account(command, error)
    },
};

static ACCOUNT_DISABLE: CommandSpec = CommandSpec {
    unsafe_param: unsafe_account_param,
    control: |command| ControlCommand::SetAccountEnabled {
        account: AccountId::new(account_field(command)),
        enabled: false,
    },
    response: |command, result| match result {
        ControlResult::Account(account) => {
            account.id.as_str() == account_field(command) && !account.enabled
        }
        _ => false,
    },
    error: |command, error| {
        matches!(error, ControlError::Storage) || error_names_own_account(command, error)
    },
};

static ACCOUNT_LABEL: CommandSpec = CommandSpec {
    unsafe_param: |command| {
        let Command::Account {
            command: AccountCommand::Label { account, label },
        } = command
        else {
            unreachable!("spec registered for another variant")
        };
        unsafe_field(account, "ACCOUNT_ID").or_else(|| unsafe_option(label, "ACCOUNT_LABEL"))
    },
    control: |command| {
        let Command::Account {
            command: AccountCommand::Label { account, label },
        } = command
        else {
            unreachable!("spec registered for another variant")
        };
        ControlCommand::SetAccountLabel {
            account: AccountId::new(account),
            label: label.clone(),
        }
    },
    response: |command, result| {
        let Command::Account {
            command: AccountCommand::Label { account, label },
        } = command
        else {
            unreachable!("spec registered for another variant")
        };
        match result {
            ControlResult::Account(result_account) => {
                result_account.id.as_str() == account
                    && result_account.label.as_deref() == label.as_deref()
            }
            _ => false,
        }
    },
    error: |command, error| match error {
        ControlError::Account(AccountError::Duplicate(_)) | ControlError::Storage => true,
        _ => error_names_own_account(command, error),
    },
};

static ACCOUNT_METRICS: CommandSpec = CommandSpec {
    unsafe_param: |command| {
        let Command::Account {
            command: AccountCommand::Metrics { account, metrics },
        } = command
        else {
            unreachable!("spec registered for another variant")
        };
        unsafe_field(account, "ACCOUNT_ID").or_else(|| {
            metrics
                .iter()
                .find_map(|metric| unsafe_field(metric, "METRIC"))
        })
    },
    control: |command| {
        let Command::Account {
            command: AccountCommand::Metrics { account, metrics },
        } = command
        else {
            unreachable!("spec registered for another variant")
        };
        ControlCommand::SetAccountMetrics {
            account: AccountId::new(account),
            // `metric_filter_choice` already rejected invalid names, so
            // normalizing here keeps the request identical to what the
            // daemon stores and returns.
            metrics: normalized_metric_names(metrics),
        }
    },
    response: |command, result| {
        let Command::Account {
            command: AccountCommand::Metrics { account, metrics },
        } = command
        else {
            unreachable!("spec registered for another variant")
        };
        match result {
            ControlResult::Account(result_account) => {
                result_account.id.as_str() == account
                    && result_account.metrics == normalized_metric_names(metrics)
            }
            _ => false,
        }
    },
    error: |command, error| match error {
        ControlError::InvalidAccountMetrics | ControlError::Storage => true,
        _ => error_names_own_account(command, error),
    },
};

static ACCOUNT_REMOVE: CommandSpec = CommandSpec {
    unsafe_param: unsafe_account_param,
    control: |command| ControlCommand::RemoveAccount {
        account: AccountId::new(account_field(command)),
    },
    response: |_, result| matches!(result, ControlResult::Ack),
    error: |command, error| match error {
        ControlError::Cancelled | ControlError::Storage => true,
        _ => error_names_own_account(command, error),
    },
};

static AUTH_LOGIN: CommandSpec = CommandSpec {
    unsafe_param: |command| {
        let AuthCommand::Login {
            provider, account, ..
        } = unwrap_auth(command)
        else {
            unreachable!("spec registered for another variant")
        };
        unsafe_option(provider, "PROVIDER_ID").or_else(|| unsafe_option(account, "--account"))
    },
    control: |command| {
        let AuthCommand::Login {
            provider: Some(provider),
            account: Some(account),
            method,
        } = unwrap_auth(command)
        else {
            // `execute_with` routes a login without an account to the
            // interactive flow before this point.
            unreachable!("spec registered for another variant")
        };
        ControlCommand::StartAuth {
            provider: ProviderId::new(provider),
            account: AccountId::new(account),
            request: AuthStartRequest {
                method: method.map(Into::into),
                redirect_uri: None,
            },
        }
    },
    response: |_, result| matches!(result, ControlResult::AuthChallenge(_)),
    error: |command, error| auth_error(unwrap_auth(command), error),
};

static AUTH_STATUS: CommandSpec = CommandSpec {
    unsafe_param: |command| {
        let AuthCommand::Status { provider, account } = unwrap_auth(command) else {
            unreachable!("spec registered for another variant")
        };
        unsafe_field(provider, "PROVIDER_ID").or_else(|| unsafe_field(account, "--account"))
    },
    control: |command| {
        let AuthCommand::Status { provider, account } = unwrap_auth(command) else {
            unreachable!("spec registered for another variant")
        };
        ControlCommand::AuthStatus {
            provider: ProviderId::new(provider),
            account: AccountId::new(account),
        }
    },
    response: |_, result| matches!(result, ControlResult::AuthState(_)),
    error: |command, error| auth_error(unwrap_auth(command), error),
};

static AUTH_COMPLETE: CommandSpec = CommandSpec {
    unsafe_param: |command| {
        let AuthCommand::Complete {
            provider,
            account,
            flow_id,
            redirect_uri,
            authorization_code_env,
        } = unwrap_auth(command)
        else {
            unreachable!("spec registered for another variant")
        };
        unsafe_field(provider, "PROVIDER_ID")
            .or_else(|| unsafe_field(account, "--account"))
            .or_else(|| unsafe_field(flow_id, "FLOW_ID"))
            .or_else(|| unsafe_option(redirect_uri, "--redirect-uri"))
            .or_else(|| unsafe_field(authorization_code_env, "--authorization-code-env"))
            .or_else(|| {
                std::env::var(authorization_code_env)
                    .is_ok_and(|value| has_unsafe_control(&value))
                    .then_some("authorization_code_env")
            })
    },
    control: |command| {
        let AuthCommand::Complete {
            provider,
            account,
            flow_id,
            redirect_uri,
            authorization_code_env,
        } = unwrap_auth(command)
        else {
            unreachable!("spec registered for another variant")
        };
        ControlCommand::CompleteAuth {
            provider: ProviderId::new(provider),
            account: AccountId::new(account),
            request: AuthCompleteRequest {
                flow_id: flow_id.clone(),
                authorization_code: std::env::var(authorization_code_env).ok(),
                redirect_uri: redirect_uri.clone(),
            },
        }
    },
    response: |_, result| matches!(result, ControlResult::AuthState(_)),
    error: |command, error| auth_error(unwrap_auth(command), error),
};

static AUTH_LOGOUT: CommandSpec = CommandSpec {
    unsafe_param: |command| {
        let AuthCommand::Logout {
            provider,
            account,
            account_label,
        } = unwrap_auth(command)
        else {
            unreachable!("spec registered for another variant")
        };
        unsafe_field(provider, "PROVIDER_ID")
            .or_else(|| unsafe_field(account, "--account"))
            .or_else(|| unsafe_option(account_label, "--account-label"))
    },
    control: |command| {
        let AuthCommand::Logout {
            provider,
            account,
            account_label,
        } = unwrap_auth(command)
        else {
            unreachable!("spec registered for another variant")
        };
        ControlCommand::Logout {
            provider: ProviderId::new(provider),
            account: AccountId::new(account),
            request: LogoutRequest {
                account_label: account_label.clone(),
            },
        }
    },
    response: |_, result| matches!(result, ControlResult::Ack),
    error: |command, error| auth_error(unwrap_auth(command), error),
};

static PROBE: CommandSpec = CommandSpec {
    unsafe_param: unsafe_account_param,
    control: |command| {
        let args = unwrap_probe(command);
        ControlCommand::Probe {
            account_id: args.account.clone(),
            wait: args.wait,
        }
    },
    response: |command, result| {
        let args = unwrap_probe(command);
        match result {
            ControlResult::Probe(payload) => {
                args.wait
                    && payload.account_id == args.account
                    && usage_outcome_is_valid(&payload.usage)
            }
            ControlResult::Ack => !args.wait,
            _ => false,
        }
    },
    error: |command, error| probe_error(unwrap_probe(command), error),
};

static SHOW: CommandSpec = CommandSpec {
    unsafe_param: |command| {
        let args = unwrap_show(command);
        unsafe_option(&args.account, "ACCOUNT_ID").or_else(|| {
            args.metric
                .iter()
                .find_map(|metric| unsafe_field(metric, "--metric"))
        })
    },
    control: |command| ControlCommand::Show {
        account_id: unwrap_show(command).account.clone(),
    },
    response: |command, result| {
        let args = unwrap_show(command);
        match result {
            ControlResult::Snapshots(snapshots) => match &args.account {
                Some(requested) => snapshots_match(snapshots, requested),
                None => snapshots_are_unique(snapshots),
            },
            _ => false,
        }
    },
    error: |command, error| match error {
        ControlError::AccountNotFound { account_id } => {
            unwrap_show(command).account.as_ref() == Some(account_id)
        }
        _ => false,
    },
};

/// The payload checks a `DaemonStatus` reply must pass to be trusted, shared
/// by the response validator and the readiness probe.
pub(crate) fn daemon_status_payload_is_well_formed(status: &DaemonStatusPayload) -> bool {
    status.accounts.iter().enumerate().all(|(index, account)| {
        (!account.stale || account.has_snapshot)
            && status.accounts[index + 1..]
                .iter()
                .all(|other| other.account_id != account.account_id)
    })
}

pub(crate) fn result_contains_unsafe_control(result: &ControlResult) -> bool {
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

/// The names a validated filter stores: trimmed and deduplicated.
fn normalized_metric_names(names: &[String]) -> Vec<String> {
    MetricFilter::new(names.to_vec())
        .map(|filter| filter.names().to_vec())
        .unwrap_or_else(|_| names.to_vec())
}

/// Resolves the metric filter for this invocation and rejects invalid names
/// before any control request is built.
pub(crate) fn metric_filter_choice(command: &Command) -> Result<MetricFilterChoice, ()> {
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
