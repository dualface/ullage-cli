//! Rendering regression for the summary rows migrated into `ullage-core`.

use chrono::{DateTime, TimeZone as _, Utc};
use ullage_core::{
    MeasurementUnit, ProviderId, SubscriptionUsage, UsageMeasurement, UsageWindow, UsageWindowKind,
    summary::{SummaryValue, summarize},
};

use crate::table::{Palette, render_summary_rows};

#[test]
fn grok_format_credits_percent_renders_remaining_ratio_and_progress_bar() {
    let resets_at = DateTime::parse_from_rfc3339("2026-09-04T01:18:04.090314Z")
        .unwrap()
        .with_timezone(&Utc);
    let summary = summarize(&SubscriptionUsage {
        provider: ProviderId::new("test"),
        account_label: None,
        plan: None,
        subscription_expires_at: None,
        observed_at: Utc.with_ymd_and_hms(2026, 8, 30, 6, 0, 0).unwrap(),
        windows: vec![UsageWindow {
            window: UsageWindowKind::Weekly,
            resets_at: Some(resets_at),
            measurements: vec![
                UsageMeasurement {
                    name: "weekly_pool".into(),
                    used: 61.0,
                    limit: Some(100.0),
                    unit: MeasurementUnit::Percent,
                },
                UsageMeasurement {
                    name: "product:GrokBuild".into(),
                    used: 61.0,
                    limit: Some(100.0),
                    unit: MeasurementUnit::Percent,
                },
            ],
        }],
    });

    assert_eq!(summary.rows[0].window, "weekly");
    assert_eq!(summary.rows[0].metric, "usage");
    assert_eq!(summary.rows[0].value, SummaryValue::Remains(39.0));
    assert_eq!(summary.rows[0].remaining_ratio, Some(0.39));
    assert_eq!(summary.rows[1].metric, "GrokBuild");
    assert_eq!(summary.rows[1].value, SummaryValue::Remains(39.0));
    assert_eq!(summary.rows[1].remaining_ratio, Some(0.39));

    let rendered = render_summary_rows(
        &summary.rows,
        Utc.with_ymd_and_hms(2026, 8, 30, 6, 0, 0).unwrap(),
        &Palette::off(),
    );
    assert!(
        rendered.contains("remains") && rendered.contains("39%"),
        "remaining percent missing from rendered summary: {rendered:?}"
    );
    assert!(
        rendered.contains('[') && rendered.contains('#') && rendered.contains(']'),
        "progress bar missing from rendered summary: {rendered:?}"
    );
}
