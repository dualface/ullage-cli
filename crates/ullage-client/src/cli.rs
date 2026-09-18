//! The `ullage` command-line schema: clap declarations and help text only.
//!
//! Every free-text argument parses through [`free_text_argument`], which rejects
//! control and bidirectional-override characters before a `Cli` exists.

use clap::{Args, Parser, Subcommand, ValueEnum};
use ullage_core::is_unsafe_identity_character as is_unsafe_control;
use ullage_protocol::AuthMethod;

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
        /// Provider id such as claude, chatgpt, grok, cursor, or opencode. Not a display name.
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
        /// Provider id such as claude, chatgpt, grok, cursor, or opencode. Not a display name.
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
        /// Provider id such as claude, chatgpt, grok, cursor, or opencode. Not a display name.
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
        /// Provider id such as claude, chatgpt, grok, cursor, or opencode. Not a display name.
        #[arg(value_name = "PROVIDER_ID", value_parser = free_text_argument)]
        provider: String,
        /// Stable account id, not the account label.
        #[arg(long, value_name = "ACCOUNT_ID", value_parser = free_text_argument)]
        account: String,
    },
    /// Forget stored credentials for an account id.
    Logout {
        /// Provider id such as claude, chatgpt, grok, cursor, or opencode. Not a display name.
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

/// Rejects control and bidirectional-override characters in a free-text CLI
/// value at parse time. Every `String`/`Vec<String>` argument carries this
/// `value_parser`; `unsafe_control_param_name` remains the belt for a
/// `Cli` constructed without parsing.
fn free_text_argument(value: &str) -> Result<String, String> {
    if value.chars().any(is_unsafe_control) {
        return Err("contains disallowed control characters".into());
    }
    Ok(value.to_owned())
}
