use std::time::Duration;

use ullage_core::{
    MeasurementUnit, PartialFailure, ProviderId, QueryOutcome, SubscriptionUsage, UsageMeasurement,
    UsageWindow, UsageWindowKind,
};
use ullage_daemon::AccountId;

mod common;
use common::*;

/// Usage with two named metrics in different windows, plus the hidden boolean
/// pair that carries a reached limit.
fn metric_query_usage() -> SubscriptionUsage {
    SubscriptionUsage {
        provider: ProviderId::new("claude"),
        account_label: Some("primary".into()),
        plan: None,
        subscription_expires_at: None,
        observed_at: chrono::Utc::now(),
        windows: vec![
            UsageWindow {
                window: UsageWindowKind::FiveHours,
                resets_at: None,
                measurements: vec![
                    UsageMeasurement {
                        name: "included_usage".into(),
                        used: 3.0,
                        limit: Some(100.0),
                        unit: MeasurementUnit::Percent,
                    },
                    UsageMeasurement {
                        name: "allowed".into(),
                        used: 1.0,
                        limit: Some(1.0),
                        unit: MeasurementUnit::Percent,
                    },
                    UsageMeasurement {
                        name: "limit_reached".into(),
                        used: 1.0,
                        limit: Some(1.0),
                        unit: MeasurementUnit::Percent,
                    },
                ],
            },
            UsageWindow {
                window: UsageWindowKind::Other {
                    id: "seven_day_opus".into(),
                    label: "Weekly Opus".into(),
                },
                resets_at: None,
                measurements: vec![UsageMeasurement {
                    name: "codex_usage".into(),
                    used: 11.0,
                    limit: Some(100.0),
                    unit: MeasurementUnit::Percent,
                }],
            },
        ],
    }
}

#[tokio::test(flavor = "multi_thread")]
async fn usage_metric_query_filters_measurements_and_keeps_bookkeeping() {
    let harness = Harness::start_with_first_query(
        Ok(QueryOutcome::Complete {
            data: metric_query_usage(),
        }),
        Duration::ZERO,
    )
    .await;
    let probe = post(
        harness.addr,
        "/v1/accounts/primary/probe",
        &harness.token,
        "",
    );
    assert_eq!(probe.status, 200, "{}", probe.body);

    let filtered = get(
        harness.addr,
        "/v1/usage?account=primary&metric=Codex",
        Some(&harness.token),
        "",
    );
    assert_eq!(filtered.status, 200, "{}", filtered.body);
    assert!(filtered.body.contains("codex_usage"), "{}", filtered.body);
    assert!(
        !filtered.body.contains("included_usage"),
        "{}",
        filtered.body
    );
    // Both windows stay in the payload; filtering only empties or narrows
    // their measurements.
    assert!(
        filtered.body.contains("five_hours") && filtered.body.contains("seven_day_opus"),
        "{}",
        filtered.body
    );
    // The hidden bookkeeping measurement that carries `limit_reached` survives
    // every filter, so a client still sees the reached limit.
    assert!(filtered.body.contains("limit_reached"), "{}", filtered.body);

    let union = get(
        harness.addr,
        "/v1/usage?account=primary&metric=usage&metric=CODEX",
        Some(&harness.token),
        "",
    );
    assert_eq!(union.status, 200, "{}", union.body);
    assert!(
        union.body.contains("included_usage") && union.body.contains("codex_usage"),
        "{}",
        union.body
    );

    let unknown = get(
        harness.addr,
        "/v1/usage?account=primary&metric=missing",
        Some(&harness.token),
        "",
    );
    assert_eq!(unknown.status, 200, "{}", unknown.body);
    // Unknown but valid names are not an error: every visible measurement is
    // filtered away, while the hidden limit bookkeeping stays.
    assert!(
        !unknown.body.contains("included_usage") && !unknown.body.contains("codex_usage"),
        "{}",
        unknown.body
    );
    assert!(
        unknown.body.contains("five_hours") && unknown.body.contains("seven_day_opus"),
        "{}",
        unknown.body
    );
    assert!(unknown.body.contains("limit_reached"), "{}", unknown.body);

    // A stored account filter is not a query parameter, so the endpoint keeps
    // returning every measurement when `metric=` is absent.
    harness
        .engine
        .set_account_metrics(&AccountId::new("primary"), vec!["Codex".into()])
        .await
        .unwrap();
    let unfiltered = get(
        harness.addr,
        "/v1/usage?account=primary",
        Some(&harness.token),
        "",
    );
    assert!(
        unfiltered.body.contains("included_usage"),
        "{}",
        unfiltered.body
    );

    harness.shutdown().await;
}

#[tokio::test(flavor = "multi_thread")]
async fn usage_metric_query_rejects_invalid_names_and_other_routes() {
    let harness = Harness::start(Vec::new(), Duration::from_secs(60)).await;

    for path in [
        "/v1/usage?metric=",
        "/v1/usage?metric=%20",
        "/v1/usage?metric=%00",
        "/v1/usage?metric=bad%1Bname",
    ] {
        let response = get(harness.addr, path, Some(&harness.token), "");
        assert_eq!(response.status, 400, "{path}: {}", response.body);
        assert!(
            response.body.contains("invalid_metric"),
            "{path}: {}",
            response.body
        );
    }

    let long = "x".repeat(129);
    let response = get(
        harness.addr,
        &format!("/v1/usage?metric={long}"),
        Some(&harness.token),
        "",
    );
    assert_eq!(response.status, 400, "{}", response.body);
    assert!(
        response.body.contains("invalid_metric"),
        "{}",
        response.body
    );

    let metrics: Vec<String> = (0..=64).map(|index| format!("metric=m{index}")).collect();
    let query = format!("/v1/usage?{}", metrics.join("&"));
    let response = get(harness.addr, &query, Some(&harness.token), "");
    assert_eq!(response.status, 400, "{}", response.body);
    assert!(
        response.body.contains("invalid_metric"),
        "{}",
        response.body
    );

    let status = get(
        harness.addr,
        "/v1/status?metric=usage",
        Some(&harness.token),
        "",
    );
    assert_eq!(status.status, 400, "{}", status.body);
    assert!(status.body.contains("bad_request"), "{}", status.body);

    let probe = post(
        harness.addr,
        "/v1/accounts/primary/probe?metric=usage",
        &harness.token,
        "",
    );
    assert_eq!(probe.status, 400, "{}", probe.body);
    assert!(probe.body.contains("bad_request"), "{}", probe.body);

    harness.shutdown().await;
}

#[tokio::test(flavor = "multi_thread")]
async fn usage_metric_query_keeps_partial_outcome_failures() {
    let partial = QueryOutcome::Partial {
        data: metric_query_usage(),
        failures: vec![PartialFailure {
            scope: "seven_day_opus".into(),
            message: "window unavailable".into(),
        }],
    };
    let harness = Harness::start_with_first_query(Ok(partial), Duration::ZERO).await;
    let probe = post(
        harness.addr,
        "/v1/accounts/primary/probe",
        &harness.token,
        "",
    );
    assert_eq!(probe.status, 200, "{}", probe.body);

    let filtered = get(
        harness.addr,
        "/v1/usage?account=primary&metric=Codex",
        Some(&harness.token),
        "",
    );
    assert_eq!(filtered.status, 200, "{}", filtered.body);
    assert!(filtered.body.contains("\"failures\""), "{}", filtered.body);
    assert!(
        filtered.body.contains("\"scope\":\"seven_day_opus\""),
        "{}",
        filtered.body
    );
    assert!(
        !filtered.body.contains("window unavailable"),
        "{}",
        filtered.body
    );
    assert!(filtered.body.contains("codex_usage"), "{}", filtered.body);
    assert!(
        !filtered.body.contains("included_usage"),
        "{}",
        filtered.body
    );

    harness.shutdown().await;
}
