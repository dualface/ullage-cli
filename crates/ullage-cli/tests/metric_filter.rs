use ullage_cli::{ClientError, ExitCode, run_from};
use ullage_protocol::{
    Account, AccountId, ControlCommand, ControlRequest, ControlResponse, ControlResult,
    MeasurementUnit, ProbePayload, ProviderId, QueryOutcome, UsageMeasurement, UsageWindow,
    UsageWindowKind,
};

mod common;
use common::*;

#[test]
fn show_metric_filters_rows_case_insensitively_and_unions_repeats() {
    fn responder(request: &ControlRequest) -> Result<ControlResponse, ClientError> {
        Ok(response(
            request,
            ControlResult::Snapshots(vec![snapshot_with_metrics(
                "primary",
                metric_usage(),
                Vec::new(),
            )]),
        ))
    }
    let client = MockClient::new(responder);

    let codex = run_from(
        [
            "ullage", "--color", "never", "show", "primary", "--metric", "cOdEx",
        ],
        &client,
    );
    assert_eq!(codex.code, ExitCode::Success, "{}", codex.stderr);
    assert!(codex.stdout.contains("Codex"), "{}", codex.stdout);
    assert!(codex.stdout.contains("Weekly Opus"), "{}", codex.stdout);
    assert!(
        codex.stdout.lines().all(|line| !line.starts_with("5h")),
        "{}",
        codex.stdout
    );
    assert!(codex.stdout.contains("! limit reached"), "{}", codex.stdout);

    let union = run_from(
        [
            "ullage", "--color", "never", "show", "primary", "--metric", "USAGE", "--metric",
            "codex",
        ],
        &client,
    );
    assert_eq!(union.code, ExitCode::Success, "{}", union.stderr);
    assert!(union.stdout.contains("Codex"), "{}", union.stdout);
    assert!(union.stdout.contains("Weekly Opus"), "{}", union.stdout);
    line_starting_with(&union.stdout, "5h");
}

#[test]
fn show_all_applies_each_accounts_stored_filter_and_flags_override_it() {
    fn responder(request: &ControlRequest) -> Result<ControlResponse, ClientError> {
        Ok(response(
            request,
            ControlResult::Snapshots(vec![
                snapshot_with_metrics("primary", metric_usage(), vec!["usage".into()]),
                snapshot_with_metrics("secondary", metric_usage(), vec!["Codex".into()]),
            ]),
        ))
    }
    let client = MockClient::new(responder);

    let stored = run_from(["ullage", "--color", "never", "show", "--all"], &client);
    assert_eq!(stored.code, ExitCode::Success, "{}", stored.stderr);
    let primary = account_block(&stored.stdout, "primary");
    let secondary = account_block(&stored.stdout, "secondary");
    assert!(
        line_starting_with(primary, "5h").contains("remains"),
        "{primary}"
    );
    assert!(!primary.contains("Weekly Opus"), "{primary}");
    assert!(secondary.contains("Weekly Opus"), "{secondary}");
    assert!(
        secondary.lines().all(|line| !line.starts_with("5h")),
        "{secondary}"
    );

    let explicit = run_from(
        [
            "ullage", "--color", "never", "show", "--all", "--metric", "Codex",
        ],
        &client,
    );
    assert_eq!(explicit.code, ExitCode::Success, "{}", explicit.stderr);
    let primary = account_block(&explicit.stdout, "primary");
    let secondary = account_block(&explicit.stdout, "secondary");
    for block in [primary, secondary] {
        assert!(
            block.contains("Weekly Opus") && block.contains("Codex"),
            "{block}"
        );
        assert!(block.lines().all(|line| !line.starts_with("5h")), "{block}");
    }

    let ignored = run_from(
        [
            "ullage",
            "--color",
            "never",
            "show",
            "--all",
            "--no-metric-filter",
        ],
        &client,
    );
    assert_eq!(ignored.code, ExitCode::Success, "{}", ignored.stderr);
    let primary = account_block(&ignored.stdout, "primary");
    let secondary = account_block(&ignored.stdout, "secondary");
    for block in [primary, secondary] {
        assert!(
            block.contains("Weekly Opus") && block.contains("Codex"),
            "{block}"
        );
        line_starting_with(block, "5h");
    }
}

#[test]
fn show_metric_without_matches_warns_and_keeps_notices_without_raw_fallback() {
    fn responder(request: &ControlRequest) -> Result<ControlResponse, ClientError> {
        Ok(response(
            request,
            ControlResult::Snapshots(vec![snapshot_with_metrics(
                "primary",
                metric_usage(),
                vec!["missing".into()],
            )]),
        ))
    }
    let client = MockClient::new(responder);

    let stored = run_from(["ullage", "--color", "never", "show", "primary"], &client);
    assert_eq!(stored.code, ExitCode::Success, "{}", stored.stderr);
    assert!(
        stored
            .stdout
            .contains("! no rows match the metric filter: missing\n"),
        "{}",
        stored.stdout
    );
    assert!(
        stored.stdout.contains("! limit reached"),
        "{}",
        stored.stdout
    );
    assert!(stored.stdout.contains("\nupdated "), "{}", stored.stdout);
    assert!(
        !stored.stdout.contains("showing raw data"),
        "{}",
        stored.stdout
    );
    assert!(!stored.stdout.contains("| WINDOW |"), "{}", stored.stdout);

    let explicit = run_from(
        [
            "ullage", "--color", "never", "show", "primary", "--metric", "Alpha", "--metric",
            "beta",
        ],
        &client,
    );
    assert_eq!(explicit.code, ExitCode::Success, "{}", explicit.stderr);
    assert!(
        explicit
            .stdout
            .contains("! no rows match the metric filter: Alpha, beta\n"),
        "{}",
        explicit.stdout
    );
}

#[test]
fn show_falls_back_to_raw_when_no_summary_row_exists_before_filtering() {
    fn responder(request: &ControlRequest) -> Result<ControlResponse, ClientError> {
        let mut usage = empty_usage();
        usage.windows.push(UsageWindow {
            window: UsageWindowKind::FiveHours,
            resets_at: None,
            measurements: vec![UsageMeasurement {
                name: "allowed".into(),
                used: 1.0,
                limit: None,
                unit: MeasurementUnit::Percent,
            }],
        });
        Ok(response(
            request,
            ControlResult::Snapshots(vec![snapshot_with_metrics(
                "primary",
                usage,
                vec!["usage".into()],
            )]),
        ))
    }
    let output = run_from(
        ["ullage", "--color", "never", "show", "primary"],
        &MockClient::new(responder),
    );

    assert_eq!(output.code, ExitCode::Success, "{}", output.stderr);
    assert!(
        output
            .stdout
            .contains("! no summarized metrics available; showing raw data\n"),
        "{}",
        output.stdout
    );
    assert!(output.stdout.contains("| allowed "), "{}", output.stdout);
    assert!(
        !output.stdout.contains("no rows match the metric filter"),
        "{}",
        output.stdout
    );
}

#[test]
fn show_raw_and_json_output_ignore_the_metric_filter() {
    fn responder(request: &ControlRequest) -> Result<ControlResponse, ClientError> {
        Ok(response(
            request,
            ControlResult::Snapshots(vec![snapshot_with_metrics(
                "primary",
                metric_usage(),
                vec!["usage".into()],
            )]),
        ))
    }
    let client = MockClient::new(responder);

    let raw = run_from(
        ["ullage", "--color", "never", "--raw", "show", "primary"],
        &client,
    );
    let raw_filtered = run_from(
        [
            "ullage", "--color", "never", "--raw", "show", "primary", "--metric", "Codex",
        ],
        &client,
    );
    assert_eq!(raw.code, ExitCode::Success, "{}", raw.stderr);
    assert_eq!(raw.stdout, raw_filtered.stdout);
    assert!(raw.stdout.contains("codex_usage"), "{}", raw.stdout);

    let json = run_from(["ullage", "--output", "json", "show", "primary"], &client);
    let json_filtered = run_from(
        [
            "ullage", "--output", "json", "show", "primary", "--metric", "Codex",
        ],
        &client,
    );
    assert_eq!(json.code, ExitCode::Success, "{}", json.stderr);
    assert_eq!(json.stdout, json_filtered.stdout);
    assert!(json.stdout.contains("\"codex_usage\""), "{}", json.stdout);
}

#[test]
fn probe_applies_the_stored_metric_filter_and_rejects_the_show_flag() {
    fn responder(request: &ControlRequest) -> Result<ControlResponse, ClientError> {
        Ok(response(
            request,
            ControlResult::Probe(ProbePayload {
                account_id: "primary".into(),
                usage: QueryOutcome::Complete {
                    data: metric_usage(),
                },
                metrics: vec!["Codex".into()],
            }),
        ))
    }
    let client = MockClient::new(responder);

    let output = run_from(["ullage", "--color", "never", "probe", "primary"], &client);
    assert_eq!(output.code, ExitCode::Success, "{}", output.stderr);
    assert!(output.stdout.contains("Codex"), "{}", output.stdout);
    assert!(!output.stdout.contains("usage"), "{}", output.stdout);

    let rejected = run_from(["ullage", "probe", "primary", "--metric", "usage"], &client);
    assert_eq!(rejected.code, ExitCode::Usage);
    assert_eq!(client.requests.lock().unwrap().len(), 1);
}

#[test]
fn account_metrics_sets_clears_and_rejects_invalid_names_without_a_request() {
    fn responder(request: &ControlRequest) -> Result<ControlResponse, ClientError> {
        let ControlCommand::SetAccountMetrics { account, metrics } = &request.command else {
            unreachable!();
        };
        Ok(response(
            request,
            ControlResult::Account(Account {
                id: account.clone(),
                provider: ProviderId::new("claude"),
                label: None,
                enabled: true,
                metrics: metrics.clone(),
            }),
        ))
    }
    let client = MockClient::new(responder);

    let set = run_from(
        [
            "ullage", "--color", "never", "account", "metrics", "primary", " Usage ", "usage",
            "Codex",
        ],
        &client,
    );
    assert_eq!(set.code, ExitCode::Success, "{}", set.stderr);
    let metrics_line = line_starting_with(&set.stdout, "METRICS");
    assert!(metrics_line.ends_with("Usage, Codex"), "{metrics_line}");
    assert!(matches!(
        client.requests.lock().unwrap().last(),
        Some(ControlCommand::SetAccountMetrics { account, metrics })
            if account.as_str() == "primary" && metrics == &["Usage".to_string(), "Codex".to_string()]
    ));

    let clear = run_from(
        [
            "ullage", "--color", "never", "account", "metrics", "primary",
        ],
        &client,
    );
    assert_eq!(clear.code, ExitCode::Success, "{}", clear.stderr);
    let metrics_line = line_starting_with(&clear.stdout, "METRICS");
    assert!(metrics_line.ends_with(" -"), "{metrics_line}");
    assert!(matches!(
        client.requests.lock().unwrap().last(),
        Some(ControlCommand::SetAccountMetrics { metrics, .. }) if metrics.is_empty()
    ));

    let requests_before = client.requests.lock().unwrap().len();
    let invalid_names = [
        "  ".to_string(),
        "x".repeat(129),
        "bad\u{202e}name".to_string(),
    ];
    for invalid in &invalid_names {
        let output = run_from(
            ["ullage", "account", "metrics", "primary", invalid.as_str()],
            &client,
        );
        assert_eq!(output.code, ExitCode::Usage, "{}", output.stderr);
        assert!(
            output.stderr.contains("invalid_account_metrics"),
            "{}",
            output.stderr
        );
        assert!(!output.stderr.contains("bad"), "{}", output.stderr);
    }
    let mut too_many: Vec<String> = vec![
        "ullage".into(),
        "account".into(),
        "metrics".into(),
        "primary".into(),
    ];
    too_many.extend((0..=64).map(|index| format!("metric-{index}")));
    let output = run_from(too_many.iter().map(String::as_str), &client);
    assert_eq!(output.code, ExitCode::Usage, "{}", output.stderr);
    assert!(
        output.stderr.contains("invalid_account_metrics"),
        "{}",
        output.stderr
    );
    assert_eq!(client.requests.lock().unwrap().len(), requests_before);
}

#[test]
fn account_list_and_show_display_the_stored_metric_filter() {
    fn responder(request: &ControlRequest) -> Result<ControlResponse, ClientError> {
        let account = Account {
            id: AccountId::new("primary"),
            provider: ProviderId::new("claude"),
            label: None,
            enabled: true,
            metrics: vec!["usage".into(), "Codex".into()],
        };
        let result = match &request.command {
            ControlCommand::ListAccounts => ControlResult::Accounts(vec![account]),
            ControlCommand::ShowAccount { .. } => ControlResult::Account(account),
            _ => unreachable!(),
        };
        Ok(response(request, result))
    }
    let client = MockClient::new(responder);

    let list = run_from(["ullage", "--color", "never", "account", "list"], &client);
    assert_eq!(list.code, ExitCode::Success, "{}", list.stderr);
    let row = line_starting_with(&list.stdout, "| primary ");
    assert!(row.contains("| usage, Codex |"), "{row}");

    let show = run_from(
        ["ullage", "--color", "never", "account", "show", "primary"],
        &client,
    );
    assert_eq!(show.code, ExitCode::Success, "{}", show.stderr);
    let metrics_line = line_starting_with(&show.stdout, "METRICS");
    assert!(metrics_line.ends_with("usage, Codex"), "{metrics_line}");
}

#[test]
fn show_metric_and_no_metric_filter_are_mutually_exclusive() {
    let client = MockClient::new(|_| unreachable!());
    let output = run_from(
        [
            "ullage",
            "show",
            "primary",
            "--metric",
            "usage",
            "--no-metric-filter",
        ],
        &client,
    );

    assert_eq!(output.code, ExitCode::Usage);
    assert!(
        output.stderr.contains("cannot be used with"),
        "{}",
        output.stderr
    );
    assert!(client.requests.lock().unwrap().is_empty());
}

#[test]
fn show_rejects_invalid_metric_names_without_touching_the_daemon() {
    let client = MockClient::new(|_| unreachable!());
    for invalid in ["  ", "bad\u{202e}name"] {
        let output = run_from(["ullage", "show", "primary", "--metric", invalid], &client);
        assert_eq!(output.code, ExitCode::Usage, "{}", output.stderr);
        assert!(
            output.stderr.contains("invalid_account_metrics"),
            "{}",
            output.stderr
        );
        assert!(!output.stderr.contains("bad"), "{}", output.stderr);
    }
    assert!(client.requests.lock().unwrap().is_empty());
}

/// The metric filter is a read-only account view: mutations keep the table
/// they printed before it existed (QA-NB-1).
#[test]
fn account_mutations_keep_the_single_row_table_without_metrics() {
    fn responder(request: &ControlRequest) -> Result<ControlResponse, ClientError> {
        Ok(response(
            request,
            ControlResult::Account(Account {
                id: AccountId::new("primary"),
                provider: ProviderId::new("claude"),
                label: None,
                enabled: true,
                metrics: vec!["included_usage".into()],
            }),
        ))
    }
    let output = run_from(
        ["ullage", "--color", "never", "account", "enable", "primary"],
        &MockClient::new(responder),
    );
    assert_eq!(output.code, ExitCode::Success);
    assert_eq!(
        output.stdout,
        concat!(
            "+---------+----------+-------+---------+\n",
            "| ACCOUNT | PROVIDER | LABEL | ENABLED |\n",
            "+---------+----------+-------+---------+\n",
            "| primary | claude   | -     | true    |\n",
            "+---------+----------+-------+---------+\n",
        )
    );
    assert!(!output.stdout.contains("METRICS"));
}
