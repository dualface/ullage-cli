//! Human-readable rendering of control results.
//!
//! Every text cell is sanitized before a palette is applied, so provider
//! strings can never smuggle terminal controls into the output.

use chrono::{DateTime, Utc};
use ullage_core::is_unsafe_identity_character as is_unsafe_control;
use ullage_core::summary::{
    MetricFilter, MetricFilterMode, UsageSummary, summarize, summarize_filtered,
};
use ullage_protocol::{
    Account, AuthMethod, AuthState, Capability, ControlResult, DaemonStatusPayload, DevicePayload,
    MeasurementUnit, PairCodePayload, ProbePayload, QueryOutcome, SnapshotPayload,
    SubscriptionUsage, UsageWindowKind,
};

use crate::cli::{ColorMode, OutputFormat};
use crate::errors::{
    control_error_kind, display_sensitive, error_hint_for_control, error_output,
    error_output_with_options, json_line, redact_error_details, redact_revealable_values,
    result_exit_code, with_diagnostic,
};
use crate::table::{
    Cell, Palette, Style, SummaryLayout, measure_summary_layout, relative_past, render_line,
    render_pairs, render_section_header, render_summary_rows, render_summary_rows_aligned,
    render_table,
};
use crate::{ExitCode, RunOutput};

/// A metric filter resolved for one account: the names plus the polarity of
/// the entry that supplied them.
///
/// `--metric` and `--no-metric-filter` resolve to a keep list, the persisted
/// `account.metrics` value to a hide list.
#[derive(Clone, Debug)]
pub(crate) struct ResolvedMetricFilter {
    filter: MetricFilter,
    mode: MetricFilterMode,
}

impl ResolvedMetricFilter {
    /// One filter for every account, applied as a keep list.
    pub(crate) fn explicit(filter: MetricFilter) -> Self {
        Self {
            filter,
            mode: MetricFilterMode::Keep,
        }
    }

    /// The persisted per-account filter, applied as a hide list.
    pub(crate) fn persisted(saved: &[String]) -> Self {
        Self {
            filter: persisted_metric_filter(saved),
            mode: MetricFilterMode::Hide,
        }
    }

    /// The readable summary for one account under this filter.
    pub(crate) fn summarize(&self, usage: &SubscriptionUsage) -> UsageSummary {
        summarize_filtered(usage, &self.filter, self.mode)
    }

    /// The normalized names, in first-seen order.
    pub(crate) fn names(&self) -> &[String] {
        self.filter.names()
    }
}

/// The filter the daemon stored for an account.
///
/// Stored names are validated when they are written and when the configuration
/// loads; an unexpected value leaves the filter inactive rather than panicking
/// while rendering.
fn persisted_metric_filter(saved: &[String]) -> MetricFilter {
    MetricFilter::new(saved.to_vec()).unwrap_or_default()
}

pub(crate) fn human_result(
    result: &ControlResult,
    reveal: bool,
    raw: bool,
    diagnose: bool,
    palette: &Palette,
    metric_choice: &MetricFilterChoice,
    account_with_metrics: bool,
) -> String {
    match result {
        ControlResult::DaemonStatus(status) => render_daemon_status(status, palette),
        ControlResult::Providers(providers) => {
            let rows = providers
                .iter()
                .map(|provider| {
                    let capabilities = provider
                        .capabilities
                        .iter()
                        .map(capability_name)
                        .collect::<Vec<_>>()
                        .join(",");
                    vec![
                        Cell::new(provider.id.as_str()),
                        Cell::new(&provider.display_name),
                        Cell::new(capabilities),
                    ]
                })
                .collect::<Vec<_>>();
            render_table(&["PROVIDER", "NAME", "CAPABILITIES"], &rows, palette)
        }
        ControlResult::Accounts(accounts) => render_accounts(accounts, reveal, palette),
        ControlResult::Account(account) if account_with_metrics => {
            render_account(account, reveal, palette)
        }
        // Mutation commands and the login flow keep the one-row table they
        // have always printed.
        ControlResult::Account(account) => render_legacy_account(account, reveal, palette),
        ControlResult::Probe(payload) => render_probe(payload, reveal, raw, diagnose, palette),
        ControlResult::Snapshots(snapshots) => {
            render_snapshots(snapshots, reveal, raw, diagnose, palette, metric_choice)
        }
        ControlResult::AuthChallenge(challenge) => render_pairs(
            &[
                ("AUTH", Cell::new("pending")),
                ("METHOD", Cell::new(auth_method_name(&challenge.method))),
                (
                    "FLOW",
                    Cell::new(display_sensitive(&challenge.flow_id, reveal)),
                ),
                (
                    "URI",
                    Cell::new(
                        challenge
                            .verification_uri
                            .as_deref()
                            .map(|value| display_sensitive(value, reveal))
                            .unwrap_or_else(|| "-".into()),
                    ),
                ),
                (
                    "CODE",
                    Cell::new(
                        challenge
                            .user_code
                            .as_deref()
                            .map(|value| display_sensitive(value, reveal))
                            .unwrap_or_else(|| "-".into()),
                    ),
                ),
            ],
            palette,
        ),
        ControlResult::AuthState(state) => {
            let value = serde_json::to_value(state).expect("auth state is serializable");
            render_pairs(
                &[(
                    "AUTH",
                    Cell::new(value["state"].as_str().unwrap_or("unknown")),
                )],
                palette,
            )
        }
        ControlResult::PairCode(pair_code) => render_pair_code(pair_code, palette),
        ControlResult::Devices(devices) => render_devices(devices, palette),
        ControlResult::Ack => "ok\n".into(),
        ControlResult::Workspaces(_)
        | ControlResult::Workspace(_)
        | ControlResult::Usage(_)
        | ControlResult::Error(_)
        | ControlResult::ProtocolMismatch { .. } => String::new(),
    }
}

fn render_pair_code(pair_code: &PairCodePayload, palette: &Palette) -> String {
    render_pairs(
        &[
            ("CODE", Cell::new(&pair_code.code)),
            ("EXPIRES_AT", Cell::new(pair_code.expires_at.to_rfc3339())),
            (
                "NEXT",
                Cell::new("Enter the HTTP API address and this code in the client."),
            ),
            (
                "NOTE",
                Cell::new(
                    "This code is one-time, expires after 300 seconds, and a new code invalidates it.",
                ),
            ),
        ],
        palette,
    )
}

fn render_devices(devices: &[DevicePayload], palette: &Palette) -> String {
    let now = Utc::now();
    let rows = devices
        .iter()
        .map(|device| {
            vec![
                Cell::new(&device.id),
                Cell::new(&device.name),
                Cell::new(relative_past(device.created_at, now)),
                Cell::new(relative_past(device.last_seen_at, now)),
            ]
        })
        .collect::<Vec<_>>();
    render_table(
        &["DEVICE ID", "NAME", "CREATED", "LAST SEEN"],
        &rows,
        palette,
    )
}

fn render_daemon_status(status: &DaemonStatusPayload, palette: &Palette) -> String {
    let state = if status.shutting_down {
        Cell::new("stopping").styled(Style::Warning)
    } else {
        Cell::new("running")
    };
    render_pairs(
        &[
            ("STATUS", state),
            ("ACCOUNTS", Cell::new(status.accounts.len().to_string())),
            (
                "ACTIVE_PROBES",
                Cell::new(
                    status
                        .accounts
                        .iter()
                        .filter(|account| account.in_flight)
                        .count()
                        .to_string(),
                ),
            ),
            (
                "CREDENTIAL_BACKEND",
                Cell::new(status.credential_backend.as_str()),
            ),
        ],
        palette,
    )
}

pub(crate) fn render_stopped_service(
    installed: bool,
    format: OutputFormat,
    color: ColorMode,
) -> RunOutput {
    let stdout = match format {
        OutputFormat::Table => render_pairs(
            &[
                ("STATUS", Cell::new("stopped").styled(Style::Error)),
                (
                    "SERVICE",
                    Cell::new(if installed {
                        "installed"
                    } else {
                        "not-installed"
                    }),
                ),
            ],
            &Palette::from_mode(color),
        ),
        OutputFormat::Json => format!(
            "{{\"service\":\"{}\",\"status\":\"stopped\"}}\n",
            if installed {
                "installed"
            } else {
                "not_installed"
            }
        ),
        OutputFormat::PrettyJson => format!(
            "{{\n  \"service\": \"{}\",\n  \"status\": \"stopped\"\n}}\n",
            if installed {
                "installed"
            } else {
                "not_installed"
            }
        ),
    };
    RunOutput {
        stdout,
        stderr: String::new(),
        code: ExitCode::Success,
    }
}

fn render_accounts(accounts: &[Account], reveal: bool, palette: &Palette) -> String {
    let rows = accounts
        .iter()
        .map(|account| {
            vec![
                Cell::new(account.id.as_str()),
                Cell::new(account.provider.as_str()),
                label_cell(account, reveal),
                Cell::new(account.enabled.to_string()),
                Cell::new(metrics_cell(&account.metrics)),
            ]
        })
        .collect::<Vec<_>>();
    render_table(
        &["ACCOUNT", "PROVIDER", "LABEL", "ENABLED", "METRICS"],
        &rows,
        palette,
    )
}

/// The single-row table that account mutations printed before the metric
/// filter existed.
fn render_legacy_account(account: &Account, reveal: bool, palette: &Palette) -> String {
    render_table(
        &["ACCOUNT", "PROVIDER", "LABEL", "ENABLED"],
        &[vec![
            Cell::new(account.id.as_str()),
            Cell::new(account.provider.as_str()),
            label_cell(account, reveal),
            Cell::new(account.enabled.to_string()),
        ]],
        palette,
    )
}

/// One account as key-value rows, so the stored metric filter has its own line.
fn render_account(account: &Account, reveal: bool, palette: &Palette) -> String {
    render_pairs(
        &[
            ("ACCOUNT", Cell::new(account.id.as_str())),
            ("PROVIDER", Cell::new(account.provider.as_str())),
            ("LABEL", label_cell(account, reveal)),
            ("ENABLED", Cell::new(account.enabled.to_string())),
            ("METRICS", Cell::new(metrics_cell(&account.metrics))),
        ],
        palette,
    )
}

fn label_cell(account: &Account, reveal: bool) -> Cell {
    Cell::new(
        account
            .label
            .as_deref()
            .map(|value| display_sensitive(value, reveal))
            .unwrap_or_else(|| "-".into()),
    )
}

fn metrics_cell(metrics: &[String]) -> String {
    if metrics.is_empty() {
        "-".into()
    } else {
        metrics.join(", ")
    }
}

fn render_probe(
    payload: &ProbePayload,
    reveal: bool,
    raw: bool,
    diagnose: bool,
    palette: &Palette,
) -> String {
    if raw {
        return render_usage_outcome(&payload.usage, reveal, diagnose, palette, Vec::new());
    }
    let filter = ResolvedMetricFilter::persisted(&payload.metrics);
    let mut block = account_section_header(&payload.account_id, &payload.usage, false, palette);
    block.push_str(&render_usage_summary(
        &payload.usage,
        false,
        &SummaryRender {
            reveal,
            diagnose,
            palette,
            now: Utc::now(),
            layout: None,
        },
        &filter,
    ));
    block
}

fn render_snapshots(
    snapshots: &[SnapshotPayload],
    reveal: bool,
    raw: bool,
    diagnose: bool,
    palette: &Palette,
    metric_choice: &MetricFilterChoice,
) -> String {
    let now = Utc::now();
    let layout = (!raw).then(|| {
        let mut layout = SummaryLayout::default();
        for snapshot in snapshots {
            let filter = metric_choice.for_saved(&snapshot.metrics);
            let summary = filter.summarize(usage_data(&snapshot.usage));
            layout.expand(measure_summary_layout(&summary.rows, now));
        }
        layout
    });
    // The account id only earns a place in the heading when more than one
    // rendered account shares the same provider.
    let mut provider_counts = std::collections::HashMap::new();
    for snapshot in snapshots {
        *provider_counts
            .entry(snapshot_provider(&snapshot.usage))
            .or_insert(0usize) += 1;
    }
    let mut blocks = Vec::new();
    for snapshot in snapshots {
        let show_account = provider_counts[snapshot_provider(&snapshot.usage)] > 1;
        let mut block = if raw {
            let mut heading = sanitize_cell(snapshot_provider(&snapshot.usage)).to_string();
            if show_account {
                heading.push_str(" - ");
                heading.push_str(sanitize_cell(&snapshot.account_id));
            }
            let mut block = render_section_header(&heading, palette);
            let status = if snapshot.stale {
                Cell::new("stale").styled(Style::Warning)
            } else {
                Cell::new("current")
            };
            block.push_str(&render_usage_outcome(
                &snapshot.usage,
                reveal,
                diagnose,
                palette,
                vec![("STATUS", status)],
            ));
            block
        } else {
            let filter = metric_choice.for_saved(&snapshot.metrics);
            let mut block = account_section_header(
                &snapshot.account_id,
                &snapshot.usage,
                show_account,
                palette,
            );
            block.push_str(&render_usage_summary(
                &snapshot.usage,
                snapshot.stale,
                &SummaryRender {
                    reveal,
                    diagnose,
                    palette,
                    now,
                    layout: layout.as_ref(),
                },
                &filter,
            ));
            block
        };
        if snapshot.last_error.is_some() {
            block.push_str(&render_pairs(
                &[("LAST_ERROR", Cell::new("[redacted]").styled(Style::Error))],
                palette,
            ));
        }
        blocks.push(block);
    }
    blocks.join("\n")
}

/// The summary view folds provider and plan into the account heading; the
/// account id joins them only when sibling blocks share the provider.
fn account_section_header(
    account_id: &str,
    usage: &QueryOutcome<SubscriptionUsage>,
    show_account: bool,
    palette: &Palette,
) -> String {
    let mut heading = sanitize_cell(snapshot_provider(usage)).to_string();
    if let Some(plan) = usage_data(usage)
        .plan
        .as_deref()
        .map(str::trim)
        .filter(|plan| !plan.is_empty())
    {
        heading.push_str(" - ");
        heading.push_str(sanitize_cell(plan));
    }
    if show_account {
        heading.push_str(" - ");
        heading.push_str(sanitize_cell(account_id));
    }
    render_section_header(&heading, palette)
}

/// The rendering state shared by every summary block of one invocation.
#[derive(Clone, Copy)]
struct SummaryRender<'a> {
    reveal: bool,
    diagnose: bool,
    palette: &'a Palette,
    now: DateTime<Utc>,
    layout: Option<&'a SummaryLayout>,
}

/// Renders the readable summary body, falling back to the raw table when no
/// measurement survives the mapping.
///
/// An active metric filter keeps the account heading and its update time but
/// replaces the rows with a warning when it leaves no row to show; it never
/// falls back to raw output, because the raw table would contradict the
/// filter. A summary that was empty before filtering still falls back as
/// before.
fn render_usage_summary(
    outcome: &QueryOutcome<SubscriptionUsage>,
    stale: bool,
    render: &SummaryRender<'_>,
    filter: &ResolvedMetricFilter,
) -> String {
    let SummaryRender {
        reveal,
        diagnose,
        palette,
        now,
        layout,
    } = *render;
    let usage = usage_data(outcome);
    let summary = summarize(usage);
    if summary.is_empty() {
        // The raw table replaces the rows, not the notices: an account with no
        // readable metric can still be stale, partial, or out of quota.
        let mut output = render_line(
            "! no summarized metrics available; showing raw data",
            Style::Warning,
            palette,
        );
        output.push_str(&render_usage(usage, reveal, palette, Vec::new()));
        output.push_str(&render_summary_notes(
            &summary, outcome, stale, diagnose, palette,
        ));
        return output;
    }

    let filtered = filter.summarize(usage);
    let mut output = render_line(
        &format!("updated {}", relative_past(filtered.observed_at, now)),
        Style::Dim,
        palette,
    );
    if filtered.is_empty() {
        output.push_str(&render_line(
            &format!(
                "! no rows match the metric filter: {}",
                filter.names().join(", ")
            ),
            Style::Warning,
            palette,
        ));
        output.push_str(&render_summary_notes(
            &summary, outcome, stale, diagnose, palette,
        ));
        return output;
    }
    if let Some(expires_at) = filtered.expires_at {
        output.push_str(&render_line(
            &format!("expires {}", expires_at.date_naive()),
            Style::Plain,
            palette,
        ));
    }
    output.push_str(&match layout {
        Some(layout) => render_summary_rows_aligned(&filtered.rows, now, palette, layout),
        None => render_summary_rows(&filtered.rows, now, palette),
    });
    output.push_str(&render_summary_notes(
        &filtered, outcome, stale, diagnose, palette,
    ));
    output
}

fn render_summary_notes(
    summary: &UsageSummary,
    outcome: &QueryOutcome<SubscriptionUsage>,
    stale: bool,
    diagnose: bool,
    palette: &Palette,
) -> String {
    let mut output = String::new();
    if stale {
        output.push_str(&render_line(
            "! stale snapshot; the daemon has not refreshed it",
            Style::Warning,
            palette,
        ));
    }
    if summary.limit_reached {
        output.push_str(&render_line("! limit reached", Style::Error, palette));
    }
    if let QueryOutcome::Partial { failures, .. } = outcome {
        let hint = if diagnose {
            String::new()
        } else {
            " (--raw for raw data, --diagnose for error details)".into()
        };
        output.push_str(&render_line(
            &format!("! {} item(s) unavailable{hint}", failures.len()),
            Style::Warning,
            palette,
        ));
        if diagnose {
            for failure in failures {
                output.push_str(&render_line(
                    &format!("! {}: {}", failure.scope, failure.message),
                    Style::Warning,
                    palette,
                ));
            }
        }
    }
    output
}

fn usage_data(usage: &QueryOutcome<SubscriptionUsage>) -> &SubscriptionUsage {
    match usage {
        QueryOutcome::Complete { data } | QueryOutcome::Partial { data, .. } => data,
    }
}

fn snapshot_provider(usage: &QueryOutcome<SubscriptionUsage>) -> &str {
    usage_data(usage).provider.as_str()
}

fn render_usage_outcome(
    outcome: &QueryOutcome<SubscriptionUsage>,
    reveal: bool,
    diagnose: bool,
    palette: &Palette,
    leading: Vec<(&'static str, Cell)>,
) -> String {
    match outcome {
        QueryOutcome::Complete { data } => render_usage(data, reveal, palette, leading),
        QueryOutcome::Partial { data, failures } => {
            let mut output = render_usage(data, reveal, palette, leading);
            output.push_str(&render_pairs(
                &[(
                    "WARNING",
                    Cell::new(format!("{} partial result(s)", failures.len()))
                        .styled(Style::Warning),
                )],
                palette,
            ));
            if diagnose {
                for failure in failures {
                    output.push_str(&render_pairs(
                        &[(
                            "FAILURE",
                            Cell::new(format!("{}: {}", failure.scope, failure.message))
                                .styled(Style::Warning),
                        )],
                        palette,
                    ));
                }
            }
            output
        }
    }
}

fn render_usage(
    usage: &SubscriptionUsage,
    reveal: bool,
    palette: &Palette,
    mut pairs: Vec<(&'static str, Cell)>,
) -> String {
    pairs.push(("PROVIDER", Cell::new(usage.provider.as_str())));
    pairs.push((
        "ACCOUNT_LABEL",
        Cell::new(
            usage
                .account_label
                .as_deref()
                .map(|value| display_sensitive(value, reveal))
                .unwrap_or_else(|| "-".into()),
        ),
    ));
    pairs.push(("PLAN", Cell::new(usage.plan.as_deref().unwrap_or(""))));
    pairs.push(("OBSERVED_AT", Cell::new(usage.observed_at.to_rfc3339())));
    pairs.push((
        "EXPIRES_AT",
        Cell::new(
            usage
                .subscription_expires_at
                .map(|value| value.to_rfc3339())
                .unwrap_or_default(),
        ),
    ));
    let mut output = render_pairs(&pairs, palette);
    let rows = usage
        .windows
        .iter()
        .flat_map(|window| {
            window.measurements.iter().map(|measurement| {
                vec![
                    Cell::new(window_name(&window.window)),
                    Cell::new(&measurement.name),
                    Cell::used(measurement.used, measurement.limit),
                    Cell::limit(measurement.limit),
                    Cell::new(unit_name(&measurement.unit)),
                    Cell::new(
                        window
                            .resets_at
                            .map(|value| value.to_rfc3339())
                            .unwrap_or_default(),
                    ),
                ]
            })
        })
        .collect::<Vec<_>>();
    output.push_str(&render_table(
        &[
            "WINDOW",
            "MEASUREMENT",
            "USED",
            "LIMIT",
            "UNIT",
            "RESETS_AT",
        ],
        &rows,
        palette,
    ));
    output
}

fn capability_name(capability: &Capability) -> String {
    match capability {
        Capability::Authentication => "authentication".into(),
        Capability::AuthenticationStatus => "authentication_status".into(),
        Capability::Logout => "logout".into(),
        Capability::UsageQuery => "usage_query".into(),
        Capability::SubscriptionExpiry => "subscription_expiry".into(),
        Capability::WorkspaceSelection => "workspace_selection".into(),
        Capability::Other(id) => format!("other:{}", sanitize_cell(id)),
    }
}

fn auth_method_name(method: &AuthMethod) -> String {
    match method {
        AuthMethod::BrowserOAuth => "browser_oauth".into(),
        AuthMethod::DeviceCode => "device_code".into(),
        AuthMethod::ApiToken => "api_token".into(),
        AuthMethod::SessionImport => "session_import".into(),
        AuthMethod::Other(id) => format!("other:{}", sanitize_cell(id)),
    }
}

fn window_name(window: &UsageWindowKind) -> String {
    match window {
        UsageWindowKind::FiveHours => "five_hours".into(),
        UsageWindowKind::Weekly => "weekly".into(),
        UsageWindowKind::Monthly => "monthly".into(),
        UsageWindowKind::Other { id, .. } => format!("other:{}", sanitize_cell(id)),
    }
}

fn unit_name(unit: &MeasurementUnit) -> String {
    match unit {
        MeasurementUnit::Requests => "requests".into(),
        MeasurementUnit::Tokens => "tokens".into(),
        MeasurementUnit::Percent => "percent".into(),
        MeasurementUnit::Credits => "credits".into(),
        MeasurementUnit::Currency { code } => format!("currency:{}", sanitize_cell(code)),
        MeasurementUnit::Other { id, .. } => format!("other:{}", sanitize_cell(id)),
    }
}

pub(crate) fn sanitize_cell(value: &str) -> &str {
    if value.chars().any(char::is_control) {
        "[redacted]"
    } else {
        value
    }
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

/// The one entry point that renders a control result into process output:
/// error envelopes for `Error`/`ProtocolMismatch`, the redacted wire value for
/// JSON, or the human-readable view for tables.
pub(crate) fn render_result(
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
#[cfg(test)]
#[path = "render_tests.rs"]
mod tests;

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
