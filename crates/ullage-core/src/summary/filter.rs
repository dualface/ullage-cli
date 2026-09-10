//! Display-name filtering for [`summarize`](super::summarize) output.

use thiserror::Error;

use crate::usage::SubscriptionUsage;

use super::{
    SummaryRow, SummaryValue, UsageSummary, is_hidden_measurement, metric_display_name, summarize,
    summarize_window,
};

/// Upper bound on the number of names one filter accepts.
pub const MAX_METRIC_NAMES: usize = 64;

/// Upper bound on the character count of one filter name.
pub const MAX_METRIC_NAME_CHARACTERS: usize = 128;

/// Why a metric filter value was rejected.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Error)]
pub enum MetricFilterError {
    #[error("metric filter names must not be empty")]
    EmptyName,
    #[error("metric filter names must be at most {MAX_METRIC_NAME_CHARACTERS} characters")]
    NameTooLong,
    #[error("a metric filter accepts at most {MAX_METRIC_NAMES} names")]
    TooManyNames,
    #[error("metric filter names must not contain control or bidirectional text characters")]
    UnsafeCharacter,
}

/// A validated set of display names selecting which summary rows to show.
///
/// Names are trimmed, matched case-insensitively against exact display names,
/// and deduplicated while keeping the first spelling. An empty filter is
/// inactive, which means "show every row". Matching ignores the window a row
/// belongs to.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct MetricFilter {
    names: Vec<String>,
}

impl MetricFilter {
    /// Validates and normalizes the given names.
    pub fn new(names: Vec<String>) -> Result<Self, MetricFilterError> {
        if names.len() > MAX_METRIC_NAMES {
            return Err(MetricFilterError::TooManyNames);
        }
        let mut normalized: Vec<String> = Vec::with_capacity(names.len());
        for name in names {
            let name = name.trim();
            if name.is_empty() {
                return Err(MetricFilterError::EmptyName);
            }
            if name.chars().count() > MAX_METRIC_NAME_CHARACTERS {
                return Err(MetricFilterError::NameTooLong);
            }
            if name.chars().any(crate::is_unsafe_identity_character) {
                return Err(MetricFilterError::UnsafeCharacter);
            }
            if !normalized
                .iter()
                .any(|existing| existing.eq_ignore_ascii_case(name))
            {
                normalized.push(name.to_owned());
            }
        }
        Ok(Self { names: normalized })
    }

    /// Whether the filter selects rows. An inactive filter shows every row.
    pub fn is_active(&self) -> bool {
        !self.names.is_empty()
    }

    /// The normalized names, in first-seen order.
    pub fn names(&self) -> &[String] {
        &self.names
    }

    /// Case-insensitive exact match against a display name.
    pub fn matches(&self, display: &str) -> bool {
        let display = display.trim();
        self.names
            .iter()
            .any(|name| name.eq_ignore_ascii_case(display))
    }
}

/// [`summarize`], keeping only rows whose display name the filter selects.
///
/// An inactive filter returns the full summary. Timestamps and
/// `limit_reached` are never affected by filtering.
pub fn summarize_filtered(usage: &SubscriptionUsage, filter: &MetricFilter) -> UsageSummary {
    let mut summary = summarize(usage);
    if filter.is_active() {
        summary.rows.retain(|row| filter.matches(&row.metric));
    }
    summary
}

/// Drops measurements a filter would hide from the summary view.
///
/// The result feeds [`summarize`] and must produce the same rows as
/// [`summarize_filtered`]: a visible measurement survives when its display
/// name matches, and the hidden bookkeeping measurements that carry the
/// `allowed`/`limit_reached` state always survive, so a reached limit is never
/// filtered away. The other hidden flags only shape a matching row or a
/// synthetic row, so they survive only when that row survives.
///
/// An inactive filter returns a clone of the input.
pub fn filter_usage_measurements(
    usage: &SubscriptionUsage,
    filter: &MetricFilter,
) -> SubscriptionUsage {
    if !filter.is_active() {
        return usage.clone();
    }
    let mut filtered = usage.clone();
    for window in &mut filtered.windows {
        let rows = summarize_window(window);
        let kept: Vec<&SummaryRow> = rows
            .iter()
            .filter(|row| filter.matches(&row.metric))
            .collect();
        let keeps_unlimited = kept
            .iter()
            .any(|row| row.value == SummaryValue::CreditsUnlimited);
        let keeps_off = kept
            .iter()
            .any(|row| row.disabled || row.value == SummaryValue::Disabled);
        let keeps_on_demand = kept.iter().any(|row| {
            (row.disabled || row.value == SummaryValue::Disabled)
                && (row.metric == "on demand" || row.metric.starts_with("on demand "))
        });
        window.measurements.retain(|measurement| {
            if is_hidden_measurement(&measurement.name) {
                match measurement.name.as_str() {
                    "allowed" | "limit_reached" => true,
                    "unlimited" => keeps_unlimited,
                    "enabled" => keeps_off,
                    "on_demand_enabled" => keeps_on_demand,
                    _ => false,
                }
            } else {
                filter.matches(&metric_display_name(&measurement.name))
            }
        });
    }
    filtered
}

#[cfg(test)]
mod tests {
    use super::super::tests::{boolean, money, percent, usage, window};
    use super::*;
    use crate::usage::{MeasurementUnit, UsageMeasurement, UsageWindowKind};

    fn metric_filter(names: &[&str]) -> MetricFilter {
        MetricFilter::new(names.iter().map(|name| (*name).to_owned()).collect()).unwrap()
    }

    /// A usage sample with hidden booleans and every kind of synthetic row.
    fn synthetic_usage() -> SubscriptionUsage {
        usage(vec![
            window(
                UsageWindowKind::FiveHours,
                vec![
                    percent("included_usage", 100.0),
                    boolean("allowed", false),
                    boolean("limit_reached", true),
                ],
            ),
            window(
                UsageWindowKind::Other {
                    id: "credits".into(),
                    label: "Credits".into(),
                },
                vec![
                    UsageMeasurement {
                        name: "credit_balance".into(),
                        used: 4.0,
                        limit: None,
                        unit: MeasurementUnit::Credits,
                    },
                    boolean("unlimited", true),
                    boolean("has_credits", true),
                ],
            ),
            window(
                UsageWindowKind::Monthly,
                vec![
                    money("total_spend", 3.0, Some(20.0)),
                    money("on_demand_spend", 0.0, Some(50.0)),
                    boolean("on_demand_enabled", false),
                ],
            ),
            window(
                UsageWindowKind::Other {
                    id: "extra".into(),
                    label: "Extra".into(),
                },
                vec![boolean("on_demand_enabled", false)],
            ),
            window(
                UsageWindowKind::Other {
                    id: "switched_off".into(),
                    label: "Switched off".into(),
                },
                vec![boolean("enabled", false)],
            ),
            window(
                UsageWindowKind::Other {
                    id: "prepaid".into(),
                    label: "Prepaid".into(),
                },
                vec![boolean("unlimited", true), boolean("has_credits", true)],
            ),
            window(
                UsageWindowKind::Other {
                    id: "spent_off".into(),
                    label: "Spent off".into(),
                },
                vec![boolean("enabled", false), money("spent", 1.0, Some(10.0))],
            ),
        ])
    }

    fn row_metrics(summary: &UsageSummary) -> Vec<&str> {
        summary.rows.iter().map(|row| row.metric.as_str()).collect()
    }

    #[test]
    fn metric_filter_trims_deduplicates_and_matches_case_insensitively() {
        let filter =
            MetricFilter::new(vec![" Usage ".into(), "usage".into(), "CODEX".into()]).unwrap();

        assert_eq!(filter.names(), ["Usage".to_owned(), "CODEX".to_owned()]);
        assert!(filter.is_active());
        assert!(filter.matches("usage"));
        assert!(filter.matches(" Codex "));
        assert!(!filter.matches("total spend"));

        let inactive = MetricFilter::new(Vec::new()).unwrap();
        assert!(!inactive.is_active());
        assert!(!inactive.matches("usage"));
    }

    #[test]
    fn metric_filter_rejects_empty_long_oversized_and_unsafe_names() {
        assert_eq!(
            MetricFilter::new(vec!["  ".into()]),
            Err(MetricFilterError::EmptyName)
        );
        assert_eq!(
            MetricFilter::new(vec![format!("x{}", "y".repeat(MAX_METRIC_NAME_CHARACTERS))]),
            Err(MetricFilterError::NameTooLong)
        );
        let too_many: Vec<String> = (0..=MAX_METRIC_NAMES).map(|i| format!("m{i}")).collect();
        assert_eq!(
            MetricFilter::new(too_many),
            Err(MetricFilterError::TooManyNames)
        );
        for unsafe_name in [
            "bad\u{7}",
            "bad\u{202e}name",
            "bad\u{200e}name",
            "bad\u{2066}name",
        ] {
            assert_eq!(
                MetricFilter::new(vec![unsafe_name.into()]),
                Err(MetricFilterError::UnsafeCharacter),
                "{unsafe_name:?}"
            );
        }
        assert_eq!(
            MetricFilter::new(vec!["x".repeat(MAX_METRIC_NAME_CHARACTERS)])
                .unwrap()
                .names(),
            ["x".repeat(MAX_METRIC_NAME_CHARACTERS)]
        );
    }

    #[test]
    fn summarize_filtered_keeps_matching_rows_and_synthetic_rows() {
        let source = synthetic_usage();
        let full = summarize(&source);
        assert!(full.limit_reached);
        assert_eq!(
            row_metrics(&full),
            [
                "usage",
                "credit balance",
                "total spend",
                "on demand spend",
                "on demand",
                "status",
                "credits",
                "spent",
            ]
        );

        let cases: [(&[&str], &[&str]); 8] = [
            (
                &[],
                &[
                    "usage",
                    "credit balance",
                    "total spend",
                    "on demand spend",
                    "on demand",
                    "status",
                    "credits",
                    "spent",
                ],
            ),
            (&["usage"], &["usage"]),
            (&["credits"], &["credits"]),
            (&["credit balance"], &["credit balance"]),
            (&["status"], &["status"]),
            (&["on demand"], &["on demand"]),
            (&["on demand spend"], &["on demand spend"]),
            (&["allowed"], &[]),
        ];
        for (names, expected) in cases {
            let summary = summarize_filtered(&source, &metric_filter(names));
            assert_eq!(row_metrics(&summary), expected, "filter {names:?}");
            assert_eq!(summary.limit_reached, full.limit_reached, "{names:?}");
            assert_eq!(summary.observed_at, full.observed_at);
            assert_eq!(summary.expires_at, full.expires_at);
        }

        // A switched-off window whose only visible row does not match leaves no
        // synthetic `status` row behind: the full summary never had one.
        let spent_off = summarize_filtered(&source, &metric_filter(&["status"]));
        assert_eq!(row_metrics(&spent_off), ["status"]);
        let switched = summarize_filtered(&source, &metric_filter(&["spent"]));
        assert_eq!(row_metrics(&switched), ["spent"]);
        assert!(switched.rows[0].disabled);
        let disabled_on_demand = summarize_filtered(&source, &metric_filter(&["on demand spend"]));
        assert!(disabled_on_demand.rows[0].disabled);
        let unlimited_balance = summarize_filtered(&source, &metric_filter(&["credit balance"]));
        assert_eq!(
            unlimited_balance.rows[0].value,
            SummaryValue::CreditsUnlimited
        );
    }

    #[test]
    fn filtered_measurements_reproduce_the_filtered_rows() {
        let source = synthetic_usage();
        let filters: [&[&str]; 9] = [
            &[],
            &["usage"],
            &["credits"],
            &["credit balance"],
            &["status"],
            &["on demand"],
            &["on demand spend"],
            &["total spend"],
            &["allowed"],
        ];
        for names in filters {
            let filter = metric_filter(names);
            let expected = summarize_filtered(&source, &filter);
            let filtered = filter_usage_measurements(&source, &filter);
            let actual = summarize(&filtered);
            assert_eq!(actual.rows, expected.rows, "filter {names:?}");
            assert_eq!(actual.limit_reached, expected.limit_reached, "{names:?}");
            assert_eq!(actual.observed_at, expected.observed_at);
            assert_eq!(actual.expires_at, expected.expires_at);
        }
    }

    #[test]
    fn inactive_filter_returns_the_input_and_a_reached_limit_survives() {
        let source = synthetic_usage();
        let inactive = MetricFilter::new(Vec::new()).unwrap();
        assert_eq!(filter_usage_measurements(&source, &inactive), source);
        assert_eq!(summarize_filtered(&source, &inactive), summarize(&source));

        let allowed_only = filter_usage_measurements(&source, &metric_filter(&["usage"]));
        let summary = summarize(&allowed_only);
        assert!(summary.limit_reached);
        assert_eq!(row_metrics(&summary), ["usage"]);
    }

    #[test]
    fn hidden_bookkeeping_names_are_publicly_recognized() {
        assert!(is_hidden_measurement("allowed"));
        assert!(is_hidden_measurement("limit_reached"));
        assert!(is_hidden_measurement("has_credits"));
        assert!(!is_hidden_measurement("included_usage"));
        assert_eq!(metric_display_name("codex_usage"), "Codex");
        assert_eq!(metric_display_name("included_usage"), "usage");
    }
}
