use chrono::{TimeZone, Utc};
use ullage_protocol::{ProviderId, QueryOutcome, SnapshotPayload, SubscriptionUsage};

use super::*;

fn usage(provider: &str) -> SubscriptionUsage {
    SubscriptionUsage {
        provider: ProviderId::new(provider),
        account_label: None,
        plan: None,
        subscription_expires_at: None,
        observed_at: Utc.with_ymd_and_hms(2026, 8, 27, 12, 0, 0).unwrap(),
        windows: Vec::new(),
    }
}

fn snapshot(account_id: &str, provider: &str) -> SnapshotPayload {
    SnapshotPayload {
        account_id: account_id.into(),
        usage: QueryOutcome::Complete {
            data: usage(provider),
        },
        last_success_at: Utc.with_ymd_and_hms(2026, 8, 27, 12, 0, 0).unwrap(),
        stale: false,
        last_error: None,
        last_error_at: None,
        metrics: Vec::new(),
    }
}

fn palette() -> Palette {
    Palette::resolve(ColorMode::Never, false, None)
}

#[test]
fn snapshot_headers_redact_control_characters_in_ids() {
    let output = render_snapshots(
        &[
            snapshot(
                "good\r==== ACCOUNT evil (x) ====\x1b[31m",
                "claude\n==== ACCOUNT forged (x) ====",
            ),
            snapshot("second", "claude\n==== ACCOUNT forged (x) ===="),
        ],
        false,
        true,
        false,
        &palette(),
        &MetricFilterChoice::Persisted,
    );
    assert!(
        output.starts_with("==== [redacted] - [redacted] ====\n"),
        "{output}"
    );
    assert_eq!(
        output.lines().filter(|line| line.contains("====")).count(),
        2,
        "{output}"
    );
    assert!(!output.contains("evil"), "{output}");
    assert!(!output.contains("forged"), "{output}");
    assert!(!output.contains('\u{1b}'), "{output}");
    assert!(!output.contains('\r'), "{output}");
}

#[test]
fn snapshot_headers_read_provider_from_partial_outcomes() {
    let snapshots = [SnapshotPayload {
        account_id: "primary".into(),
        usage: QueryOutcome::Partial {
            data: usage("cursor"),
            failures: Vec::new(),
        },
        last_success_at: Utc.with_ymd_and_hms(2026, 8, 27, 12, 0, 0).unwrap(),
        stale: false,
        last_error: None,
        last_error_at: None,
        metrics: Vec::new(),
    }];
    let output = render_snapshots(
        &snapshots,
        false,
        true,
        false,
        &palette(),
        &MetricFilterChoice::Persisted,
    );
    assert!(output.starts_with("==== cursor ====\n"), "{output}");
}

#[test]
fn summary_headers_redact_control_characters_in_ids_and_plans() {
    let mut data = usage("claude\n==== ACCOUNT forged (x) ====");
    data.plan = Some("pro\x1b[31m".into());
    let output = render_snapshots(
        &[SnapshotPayload {
            account_id: "good\r==== ACCOUNT evil (x) ====".into(),
            usage: QueryOutcome::Complete { data },
            last_success_at: Utc.with_ymd_and_hms(2026, 8, 27, 12, 0, 0).unwrap(),
            stale: false,
            last_error: None,
            last_error_at: None,
            metrics: Vec::new(),
        }],
        false,
        false,
        false,
        &palette(),
        &MetricFilterChoice::Persisted,
    );
    assert!(
        output.starts_with("==== [redacted] - [redacted] ====\n"),
        "{output}"
    );
    assert_eq!(
        output.lines().filter(|line| line.contains("====")).count(),
        1,
        "{output}"
    );
    assert!(!output.contains("evil"), "{output}");
    assert!(!output.contains("forged"), "{output}");
    assert!(!output.contains('\u{1b}'), "{output}");
    assert!(!output.contains('\r'), "{output}");
}

#[test]
fn summary_headers_omit_an_absent_plan() {
    let output = render_snapshots(
        &[snapshot("primary", "claude")],
        false,
        false,
        false,
        &palette(),
        &MetricFilterChoice::Persisted,
    );
    assert!(output.starts_with("==== claude ====\n"), "{output}");
}

#[test]
fn snapshots_group_accounts_by_provider() {
    let output = render_snapshots(
        &[
            snapshot("first", "claude"),
            snapshot("solo", "cursor"),
            snapshot("second", "claude"),
        ],
        false,
        false,
        false,
        &palette(),
        &MetricFilterChoice::Persisted,
    );
    let first = output.find("==== claude - first ====").unwrap();
    let second = output.find("==== claude - second ====").unwrap();
    let solo = output.find("==== cursor ====").unwrap();
    assert!(first < second && second < solo, "{output}");
}

#[test]
fn headers_name_the_account_only_when_a_provider_repeats() {
    let output = render_snapshots(
        &[
            snapshot("first", "claude"),
            snapshot("second", "claude"),
            snapshot("solo", "cursor"),
        ],
        false,
        false,
        false,
        &palette(),
        &MetricFilterChoice::Persisted,
    );
    assert!(output.contains("==== claude - first ====\n"), "{output}");
    assert!(output.contains("==== claude - second ====\n"), "{output}");
    assert!(output.contains("==== cursor ====\n"), "{output}");
}

#[test]
fn a_snapshot_without_summarizable_data_falls_back_to_the_raw_table() {
    let output = render_snapshots(
        &[snapshot("primary", "claude")],
        false,
        false,
        false,
        &palette(),
        &MetricFilterChoice::Persisted,
    );
    assert!(
        output.contains("! no summarized metrics available; showing raw data\n"),
        "{output}"
    );
    assert!(output.contains("| WINDOW | MEASUREMENT |"), "{output}");
}

#[test]
fn diagnose_redacts_control_characters_in_partial_failure_scope() {
    use ullage_protocol::PartialFailure;

    let snapshots = [SnapshotPayload {
        account_id: "primary".into(),
        usage: QueryOutcome::Partial {
            data: usage("claude"),
            failures: vec![PartialFailure {
                scope: "profile\x1b[31m".into(),
                message: "provider protocol response is incompatible".into(),
            }],
        },
        last_success_at: Utc.with_ymd_and_hms(2026, 8, 27, 12, 0, 0).unwrap(),
        stale: false,
        last_error: None,
        last_error_at: None,
        metrics: Vec::new(),
    }];
    let output = render_snapshots(
        &snapshots,
        false,
        false,
        true,
        &palette(),
        &MetricFilterChoice::Persisted,
    );
    assert!(!output.contains('\u{1b}'), "{output}");
    assert!(!output.contains("[31m"), "{output}");
    assert!(output.contains("[redacted]"), "{output}");
}
