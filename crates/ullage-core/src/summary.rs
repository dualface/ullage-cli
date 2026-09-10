//! Derives the readable usage summary shown by default in table output.
//!
//! This layer is a pure projection of [`SubscriptionUsage`]: it decides which
//! measurements a human wants to see, how to name them, and what each value
//! means. It never formats, pads, or colors anything, so it can be unit tested
//! without a terminal. Rendering lives in [`crate::table`].
//!
//! Provider-supplied strings are carried through verbatim; sanitizing happens
//! in the render layer, which is the only place that emits escape sequences.

use chrono::{DateTime, Utc};

use crate::usage::{
    MeasurementUnit, SubscriptionUsage, UsageMeasurement, UsageWindow, UsageWindowKind,
};

mod filter;

pub use filter::{
    MAX_METRIC_NAME_CHARACTERS, MAX_METRIC_NAMES, MetricFilter, MetricFilterError,
    filter_usage_measurements, summarize_filtered,
};

/// Measurements that carry provider bookkeeping rather than remaining quota.
///
/// All of them stay in `--raw` output. The first six are status booleans: a
/// hidden boolean never hides the state it reports, because every abnormal
/// value re-appears as [`UsageSummary::limit_reached`], a
/// [`SummaryValue::CreditsUnlimited`] row, or the `(off)` marker and its
/// [`SummaryValue::Disabled`] stand-in. The last two are Cursor's component
/// spends, which are hidden unconditionally: they are not a state, only a
/// second breakdown of money `total_spend` already reports.
const HIDDEN_MEASUREMENTS: [&str; 8] = [
    "allowed",
    "limit_reached",
    "has_credits",
    "unlimited",
    "on_demand_enabled",
    "enabled",
    "included_spend",
    "bonus_spend",
];

/// Measurement names that mean "the window's own quota" for their provider.
const POOL_MEASUREMENTS: [&str; 3] = ["included_usage", "total", "weekly_pool"];

/// Product names whose canonical spelling cannot be derived from their id.
const BRAND_NAMES: [(&str, &str); 1] = [("codex", "Codex")];

/// How a single summary row reports its number.
#[derive(Clone, Debug, PartialEq)]
pub enum SummaryValue {
    /// Percent of the window's quota still available.
    Remains(f64),
    /// Percent consumed, used when the provider gave no percentage limit.
    Used(f64),
    /// Money on hand, with no spending limit to compare against.
    Balance { amount: f64, currency: Currency },
    /// Money spent against a known limit.
    Spent {
        amount: f64,
        limit: f64,
        currency: Currency,
    },
    /// A countable credit balance, optionally against a known limit.
    Credits { used: f64, limit: Option<f64> },
    /// Credits the provider reported as unmetered.
    CreditsUnlimited,
    /// A plain count in a unit with no agreed remaining-quota reading.
    Counted { used: f64, limit: Option<f64> },
    /// A feature the provider reported as switched off, with no amount of its
    /// own for the `(off)` marker to sit on.
    Disabled,
}

/// Currency of a monetary measurement, as reported by the provider.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Currency {
    pub code: String,
}

/// One line of the summary view.
#[derive(Clone, Debug, PartialEq)]
pub struct SummaryRow {
    /// Human name of the window, e.g. `5h` or `Extra Usage Credits`.
    pub window: String,
    /// Human name of the measurement, e.g. `usage` or `Codex`.
    pub metric: String,
    pub value: SummaryValue,
    pub resets_at: Option<DateTime<Utc>>,
    /// Fraction of quota still available, `0.0..=1.0`, when the value is remaining
    /// quota. Money spent is an amount, so it stays `None`.
    pub remaining_ratio: Option<f64>,
    /// The provider reported this row's feature as switched off.
    pub disabled: bool,
}

/// The whole summary for one `SubscriptionUsage`.
#[derive(Clone, Debug, PartialEq)]
pub struct UsageSummary {
    pub rows: Vec<SummaryRow>,
    /// A window reported `allowed = 0` or `limit_reached = 1`.
    pub limit_reached: bool,
    pub observed_at: DateTime<Utc>,
    pub expires_at: Option<DateTime<Utc>>,
}

impl UsageSummary {
    /// No row survived the mapping, so the caller should fall back to raw output.
    pub fn is_empty(&self) -> bool {
        self.rows.is_empty()
    }
}

/// Projects provider usage onto the rows a human reads.
pub fn summarize(usage: &SubscriptionUsage) -> UsageSummary {
    let mut rows = Vec::new();
    let mut limit_reached = false;
    for window in &usage.windows {
        limit_reached |= window_hit_its_limit(window);
        rows.extend(summarize_window(window));
    }
    UsageSummary {
        rows,
        limit_reached,
        observed_at: usage.observed_at,
        expires_at: usage.subscription_expires_at,
    }
}

fn summarize_window(window: &UsageWindow) -> Vec<SummaryRow> {
    let window_name = window_display_name(&window.window);
    let unlimited = boolean_measurement(window, "unlimited") == Some(true);
    let whole_window_off = boolean_measurement(window, "enabled") == Some(false);
    let on_demand_off = boolean_measurement(window, "on_demand_enabled") == Some(false);

    let row = |metric: String, value: SummaryValue, disabled: bool| SummaryRow {
        window: window_name.clone(),
        metric,
        remaining_ratio: remaining_ratio(&value),
        value,
        resets_at: window.resets_at,
        disabled,
    };

    let mut rows: Vec<SummaryRow> = window
        .measurements
        .iter()
        .filter(|measurement| !HIDDEN_MEASUREMENTS.contains(&measurement.name.as_str()))
        .map(|measurement| {
            row(
                metric_display_name(&measurement.name),
                measurement_value(measurement, unlimited),
                whole_window_off || (on_demand_off && measurement.name.starts_with("on_demand")),
            )
        })
        .collect();

    // A status boolean can be the only thing the provider reported about this
    // window. Hiding it would then hide the abnormal state itself, so give the
    // state a row to sit on.
    if unlimited
        && !rows
            .iter()
            .any(|row| row.value == SummaryValue::CreditsUnlimited)
    {
        rows.push(row(
            "credits".into(),
            SummaryValue::CreditsUnlimited,
            whole_window_off,
        ));
    }
    if whole_window_off && !rows.iter().any(|row| row.disabled) {
        rows.push(row("status".into(), SummaryValue::Disabled, false));
    }
    if on_demand_off && !has_on_demand_amount(window) {
        rows.push(row("on demand".into(), SummaryValue::Disabled, false));
    }
    rows
}

/// Whether an `on_demand_enabled = 0` window carries an amount to mark `(off)`.
fn has_on_demand_amount(window: &UsageWindow) -> bool {
    window.measurements.iter().any(|measurement| {
        measurement.name.starts_with("on_demand")
            && !HIDDEN_MEASUREMENTS.contains(&measurement.name.as_str())
    })
}

/// `allowed = 0` and `limit_reached = 1` are hidden as rows but never dropped.
fn window_hit_its_limit(window: &UsageWindow) -> bool {
    boolean_measurement(window, "allowed") == Some(false)
        || boolean_measurement(window, "limit_reached") == Some(true)
}

fn boolean_measurement(window: &UsageWindow, name: &str) -> Option<bool> {
    window
        .measurements
        .iter()
        .find(|measurement| measurement.name == name)
        .map(|measurement| measurement.used != 0.0)
}

fn measurement_value(measurement: &UsageMeasurement, unlimited: bool) -> SummaryValue {
    match &measurement.unit {
        MeasurementUnit::Percent => match measurement.limit {
            Some(100.0) => SummaryValue::Remains((100.0 - measurement.used).clamp(0.0, 100.0)),
            _ => SummaryValue::Used(measurement.used.max(0.0)),
        },
        MeasurementUnit::Currency { code } => {
            let currency = Currency { code: code.clone() };
            match measurement.limit {
                Some(limit) => SummaryValue::Spent {
                    amount: measurement.used,
                    limit,
                    currency,
                },
                None => SummaryValue::Balance {
                    amount: measurement.used,
                    currency,
                },
            }
        }
        MeasurementUnit::Credits if unlimited => SummaryValue::CreditsUnlimited,
        MeasurementUnit::Credits => SummaryValue::Credits {
            used: measurement.used,
            limit: measurement.limit,
        },
        // Requests, tokens, and provider-specific units have no agreed wording
        // for what is left, so the text column reports the count itself. A
        // provider-supplied limit still gives an honest remaining ratio.
        _ => SummaryValue::Counted {
            used: measurement.used,
            limit: measurement.limit,
        },
    }
}

/// The share of quota left, when the value is remaining quota rather than an amount.
fn remaining_ratio(value: &SummaryValue) -> Option<f64> {
    match value {
        SummaryValue::Remains(percent) => Some((percent / 100.0).clamp(0.0, 1.0)),
        SummaryValue::Credits {
            used: amount,
            limit: Some(limit),
        }
        | SummaryValue::Counted {
            used: amount,
            limit: Some(limit),
        } if *limit > 0.0 => Some(((limit - amount) / limit).clamp(0.0, 1.0)),
        _ => None,
    }
}

fn window_display_name(window: &UsageWindowKind) -> String {
    match window {
        UsageWindowKind::FiveHours => "5h".into(),
        UsageWindowKind::Weekly => "weekly".into(),
        UsageWindowKind::Monthly => "monthly".into(),
        UsageWindowKind::Other { id, label } => {
            if let Some(short) = other_window_display_name(id, label) {
                return short.into();
            }
            if label.trim().is_empty() {
                id.clone()
            } else {
                label.clone()
            }
        }
    }
}

fn other_window_display_name(id: &str, label: &str) -> Option<&'static str> {
    if token_is("fable", id) || token_is("fable", label) {
        return Some("fable");
    }
    if id == "rate_limit_reset_credits" || label.eq_ignore_ascii_case("Rate limit reset credits") {
        return Some("Resets");
    }
    None
}

fn token_is(needle: &str, value: &str) -> bool {
    value
        .split(|character: char| !character.is_ascii_alphanumeric())
        .any(|part| part.eq_ignore_ascii_case(needle))
}

/// Whether a provider measurement is bookkeeping hidden from the summary view.
pub fn is_hidden_measurement(name: &str) -> bool {
    HIDDEN_MEASUREMENTS.contains(&name)
}

/// The human name of a provider measurement, e.g. `codex_usage` -> `Codex`.
pub fn metric_display_name(name: &str) -> String {
    if POOL_MEASUREMENTS.contains(&name) {
        return "usage".into();
    }
    let name = name.strip_prefix("product:").unwrap_or(name);
    let name = name.strip_suffix("_usage").unwrap_or(name);
    let name = name.strip_suffix("-Codex-Spark").unwrap_or(name);
    if name.is_empty() {
        return "usage".into();
    }
    if let Some((_, brand)) = BRAND_NAMES.iter().find(|(id, _)| *id == name) {
        return (*brand).into();
    }
    name.replace('_', " ")
}

#[cfg(test)]
pub(crate) mod tests {
    use super::*;
    use crate::ProviderId;
    use chrono::TimeZone as _;

    pub(crate) fn usage(windows: Vec<UsageWindow>) -> SubscriptionUsage {
        SubscriptionUsage {
            provider: ProviderId::new("test"),
            account_label: None,
            plan: Some("pro".into()),
            subscription_expires_at: None,
            observed_at: Utc.with_ymd_and_hms(2026, 8, 29, 12, 0, 0).unwrap(),
            windows,
        }
    }

    pub(crate) fn percent(name: &str, used: f64) -> UsageMeasurement {
        UsageMeasurement {
            name: name.into(),
            used,
            limit: Some(100.0),
            unit: MeasurementUnit::Percent,
        }
    }

    pub(crate) fn boolean(name: &str, value: bool) -> UsageMeasurement {
        UsageMeasurement {
            name: name.into(),
            used: f64::from(u8::from(value)),
            limit: Some(1.0),
            unit: MeasurementUnit::Other {
                id: "boolean".into(),
                label: "Boolean".into(),
            },
        }
    }

    pub(crate) fn money(name: &str, used: f64, limit: Option<f64>) -> UsageMeasurement {
        UsageMeasurement {
            name: name.into(),
            used,
            limit,
            unit: MeasurementUnit::Currency { code: "USD".into() },
        }
    }

    pub(crate) fn window(
        kind: UsageWindowKind,
        measurements: Vec<UsageMeasurement>,
    ) -> UsageWindow {
        UsageWindow {
            window: kind,
            resets_at: None,
            measurements,
        }
    }

    #[test]
    fn claude_percent_windows_report_remaining_quota() {
        let summary = summarize(&usage(vec![
            window(
                UsageWindowKind::FiveHours,
                vec![percent("included_usage", 3.0)],
            ),
            window(
                UsageWindowKind::Other {
                    id: "seven_day_opus".into(),
                    label: "Weekly Opus".into(),
                },
                vec![percent("included_usage", 11.0)],
            ),
        ]));

        assert_eq!(summary.rows.len(), 2);
        assert_eq!(summary.rows[0].window, "5h");
        assert_eq!(summary.rows[0].metric, "usage");
        assert_eq!(summary.rows[0].value, SummaryValue::Remains(97.0));
        assert_eq!(summary.rows[0].remaining_ratio, Some(0.97));
        assert_eq!(summary.rows[1].window, "Weekly Opus");
        assert_eq!(summary.rows[1].value, SummaryValue::Remains(89.0));
        assert!(!summary.limit_reached);
    }

    #[test]
    fn chatgpt_status_booleans_are_hidden_but_a_reached_limit_still_shows() {
        let summary = summarize(&usage(vec![window(
            UsageWindowKind::FiveHours,
            vec![
                percent("codex_usage", 80.0),
                boolean("allowed", true),
                boolean("limit_reached", true),
            ],
        )]));

        assert_eq!(summary.rows.len(), 1);
        assert_eq!(summary.rows[0].metric, "Codex");
        assert!(summary.limit_reached);
    }

    #[test]
    fn chatgpt_normal_status_booleans_leave_no_trace() {
        let summary = summarize(&usage(vec![window(
            UsageWindowKind::FiveHours,
            vec![
                percent("code_review_usage", 10.0),
                boolean("allowed", true),
                boolean("limit_reached", false),
            ],
        )]));

        assert_eq!(summary.rows.len(), 1);
        assert!(!summary.limit_reached);
    }

    #[test]
    fn chatgpt_denied_window_reports_a_reached_limit() {
        let summary = summarize(&usage(vec![window(
            UsageWindowKind::Weekly,
            vec![percent("codex_usage", 100.0), boolean("allowed", false)],
        )]));

        assert!(summary.limit_reached);
        assert_eq!(summary.rows[0].value, SummaryValue::Remains(0.0));
        assert_eq!(summary.rows[0].remaining_ratio, Some(0.0));
    }

    #[test]
    fn chatgpt_credits_report_a_countable_balance() {
        let summary = summarize(&usage(vec![window(
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
                boolean("has_credits", true),
                boolean("unlimited", false),
            ],
        )]));

        assert_eq!(summary.rows.len(), 1);
        assert_eq!(summary.rows[0].window, "Credits");
        assert_eq!(summary.rows[0].metric, "credit balance");
        assert_eq!(
            summary.rows[0].value,
            SummaryValue::Credits {
                used: 4.0,
                limit: None,
            }
        );
        assert_eq!(summary.rows[0].remaining_ratio, None);
    }

    #[test]
    fn spent_amounts_do_not_report_remaining_ratio() {
        let summary = summarize(&usage(vec![window(
            UsageWindowKind::Monthly,
            vec![money("total_spend", 925.11, Some(400.0))],
        )]));

        assert_eq!(
            summary.rows[0].value,
            SummaryValue::Spent {
                amount: 925.11,
                limit: 400.0,
                currency: Currency { code: "USD".into() },
            }
        );
        assert_eq!(summary.rows[0].remaining_ratio, None);
    }

    #[test]
    fn credits_with_a_positive_limit_report_remaining_ratio() {
        let summary = summarize(&usage(vec![window(
            UsageWindowKind::Monthly,
            vec![UsageMeasurement {
                name: "total".into(),
                used: 285.0,
                limit: Some(1000.0),
                unit: MeasurementUnit::Credits,
            }],
        )]));

        assert_eq!(
            summary.rows[0].value,
            SummaryValue::Credits {
                used: 285.0,
                limit: Some(1000.0),
            }
        );
        assert_eq!(summary.rows[0].remaining_ratio, Some(0.715));
    }

    #[test]
    fn credits_with_a_zero_limit_do_not_compute_a_ratio() {
        let summary = summarize(&usage(vec![window(
            UsageWindowKind::Monthly,
            vec![UsageMeasurement {
                name: "total".into(),
                used: 10.0,
                limit: Some(0.0),
                unit: MeasurementUnit::Credits,
            }],
        )]));

        assert_eq!(
            summary.rows[0].value,
            SummaryValue::Credits {
                used: 10.0,
                limit: Some(0.0),
            }
        );
        assert_eq!(summary.rows[0].remaining_ratio, None);
    }

    #[test]
    fn credits_without_a_limit_keep_a_bare_count() {
        let summary = summarize(&usage(vec![window(
            UsageWindowKind::Monthly,
            vec![UsageMeasurement {
                name: "total".into(),
                used: 285.0,
                limit: None,
                unit: MeasurementUnit::Credits,
            }],
        )]));

        assert_eq!(
            summary.rows[0].value,
            SummaryValue::Credits {
                used: 285.0,
                limit: None,
            }
        );
        assert_eq!(summary.rows[0].remaining_ratio, None);
    }

    #[test]
    fn unlimited_credits_replace_the_balance_reading() {
        let summary = summarize(&usage(vec![window(
            UsageWindowKind::Other {
                id: "credits".into(),
                label: "Credits".into(),
            },
            vec![
                UsageMeasurement {
                    name: "credit_balance".into(),
                    used: 0.0,
                    limit: None,
                    unit: MeasurementUnit::Credits,
                },
                boolean("unlimited", true),
            ],
        )]));

        assert_eq!(summary.rows[0].value, SummaryValue::CreditsUnlimited);
    }

    #[test]
    fn cursor_keeps_total_spend_and_drops_its_two_components() {
        let summary = summarize(&usage(vec![window(
            UsageWindowKind::Monthly,
            vec![
                money("total_spend", 3.0, Some(20.0)),
                money("included_spend", 2.0, Some(20.0)),
                money("bonus_spend", 1.0, None),
                percent("total", 15.0),
                money("on_demand_spend", 0.0, Some(50.0)),
                boolean("on_demand_enabled", false),
            ],
        )]));

        let metrics: Vec<&str> = summary.rows.iter().map(|row| row.metric.as_str()).collect();
        assert_eq!(
            metrics,
            vec!["total spend", "usage", "on demand spend"],
            "{summary:?}"
        );
        assert_eq!(
            summary.rows[0].value,
            SummaryValue::Spent {
                amount: 3.0,
                limit: 20.0,
                currency: Currency { code: "USD".into() },
            }
        );
        assert_eq!(summary.rows[0].remaining_ratio, None);
        assert!(!summary.rows[0].disabled);
        assert_eq!(summary.rows[2].remaining_ratio, None);
        assert!(summary.rows[2].disabled, "on-demand is switched off");
    }

    #[test]
    fn cursor_components_stay_hidden_even_without_total_spend() {
        let summary = summarize(&usage(vec![window(
            UsageWindowKind::Monthly,
            vec![
                money("included_spend", 2.0, None),
                money("bonus_spend", 1.0, None),
            ],
        )]));

        assert!(summary.is_empty(), "{summary:?}");
    }

    #[test]
    fn an_unlimited_flag_without_a_balance_still_shows_the_state() {
        let summary = summarize(&usage(vec![window(
            UsageWindowKind::Other {
                id: "credits".into(),
                label: "Credits".into(),
            },
            vec![boolean("has_credits", true), boolean("unlimited", true)],
        )]));

        assert_eq!(summary.rows.len(), 1);
        assert_eq!(summary.rows[0].window, "Credits");
        assert_eq!(summary.rows[0].metric, "credits");
        assert_eq!(summary.rows[0].value, SummaryValue::CreditsUnlimited);
    }

    #[test]
    fn a_switched_off_window_without_amounts_still_shows_the_state() {
        for (flag, metric) in [("enabled", "status"), ("on_demand_enabled", "on demand")] {
            let summary = summarize(&usage(vec![window(
                UsageWindowKind::Other {
                    id: "on_demand".into(),
                    label: "On-demand usage".into(),
                },
                vec![boolean(flag, false)],
            )]));

            assert_eq!(summary.rows.len(), 1, "{flag}: {summary:?}");
            assert_eq!(summary.rows[0].metric, metric);
            assert_eq!(summary.rows[0].value, SummaryValue::Disabled);
        }
    }

    #[test]
    fn a_disabled_on_demand_feature_is_reported_beside_the_windows_other_rows() {
        let summary = summarize(&usage(vec![window(
            UsageWindowKind::Monthly,
            vec![
                money("total_spend", 3.0, Some(20.0)),
                boolean("on_demand_enabled", false),
            ],
        )]));

        assert_eq!(summary.rows.len(), 2, "{summary:?}");
        assert_eq!(summary.rows[0].metric, "total spend");
        assert!(!summary.rows[0].disabled, "only on-demand is switched off");
        assert_eq!(summary.rows[1].metric, "on demand");
        assert_eq!(summary.rows[1].value, SummaryValue::Disabled);
    }

    #[test]
    fn a_switched_off_window_marks_its_synthetic_unlimited_row() {
        let summary = summarize(&usage(vec![window(
            UsageWindowKind::Other {
                id: "credits".into(),
                label: "Credits".into(),
            },
            vec![boolean("unlimited", true), boolean("enabled", false)],
        )]));

        assert_eq!(summary.rows.len(), 1, "{summary:?}");
        assert_eq!(summary.rows[0].value, SummaryValue::CreditsUnlimited);
        assert!(
            summary.rows[0].disabled,
            "a switched-off window must not read as merely unmetered: {summary:?}"
        );
    }

    #[test]
    fn a_switched_off_window_with_amounts_marks_them_instead_of_adding_a_row() {
        let summary = summarize(&usage(vec![window(
            UsageWindowKind::Other {
                id: "on_demand".into(),
                label: "On-demand usage".into(),
            },
            vec![boolean("enabled", false), money("spent", 1.0, Some(10.0))],
        )]));

        assert_eq!(summary.rows.len(), 1, "{summary:?}");
        assert!(summary.rows[0].disabled);
    }

    #[test]
    fn grok_products_and_prepaid_balance_map_to_readable_names() {
        let resets_at = Utc.with_ymd_and_hms(2026, 9, 3, 12, 0, 0).unwrap();
        let summary = summarize(&usage(vec![
            UsageWindow {
                window: UsageWindowKind::Weekly,
                resets_at: Some(resets_at),
                measurements: vec![percent("weekly_pool", 40.0), percent("product:grok-4", 5.0)],
            },
            window(
                UsageWindowKind::Other {
                    id: "prepaid".into(),
                    label: "Extra Usage Credits".into(),
                },
                vec![money("remaining", 12.34, None)],
            ),
            window(
                UsageWindowKind::Other {
                    id: "on_demand".into(),
                    label: "On-demand usage".into(),
                },
                vec![boolean("enabled", false), money("spent", 1.0, Some(10.0))],
            ),
        ]));

        assert_eq!(summary.rows[0].metric, "usage");
        assert_eq!(summary.rows[0].resets_at, Some(resets_at));
        assert_eq!(summary.rows[1].metric, "grok-4");
        assert_eq!(summary.rows[2].window, "Extra Usage Credits");
        assert_eq!(summary.rows[2].metric, "remaining");
        assert_eq!(
            summary.rows[2].value,
            SummaryValue::Balance {
                amount: 12.34,
                currency: Currency { code: "USD".into() },
            }
        );
        assert_eq!(summary.rows[2].resets_at, None);
        assert_eq!(summary.rows[3].window, "On-demand usage");
        assert!(summary.rows[3].disabled, "the whole window is switched off");
    }

    #[test]
    fn a_percent_without_a_full_scale_limit_reports_consumption() {
        let summary = summarize(&usage(vec![window(
            UsageWindowKind::Weekly,
            vec![UsageMeasurement {
                name: "included_usage".into(),
                used: 30.0,
                limit: None,
                unit: MeasurementUnit::Percent,
            }],
        )]));

        assert_eq!(summary.rows[0].value, SummaryValue::Used(30.0));
        assert_eq!(summary.rows[0].remaining_ratio, None);
    }

    #[test]
    fn an_other_window_without_a_label_falls_back_to_its_id() {
        let summary = summarize(&usage(vec![window(
            UsageWindowKind::Other {
                id: "custom_quota".into(),
                label: "   ".into(),
            },
            vec![UsageMeasurement {
                name: "available_count".into(),
                used: 1.0,
                limit: None,
                unit: MeasurementUnit::Credits,
            }],
        )]));

        assert_eq!(summary.rows[0].window, "custom_quota");
        assert_eq!(
            summary.rows[0].value,
            SummaryValue::Credits {
                used: 1.0,
                limit: None,
            }
        );
    }

    #[test]
    fn reset_credits_windows_use_a_short_table_name() {
        let summary = summarize(&usage(vec![window(
            UsageWindowKind::Other {
                id: "rate_limit_reset_credits".into(),
                label: "Rate limit reset credits".into(),
            },
            vec![UsageMeasurement {
                name: "available_count".into(),
                used: 1.0,
                limit: None,
                unit: MeasurementUnit::Credits,
            }],
        )]));

        assert_eq!(summary.rows[0].window, "Resets");
        assert_eq!(summary.rows[0].metric, "available count");
    }

    #[test]
    fn fable_windows_use_a_short_table_name() {
        let summary = summarize(&usage(vec![window(
            UsageWindowKind::Other {
                id: "model_weekly_scoped_weekly_fable".into(),
                label: "Fable (weekly_scoped)".into(),
            },
            vec![percent("included_usage", 89.0)],
        )]));

        assert_eq!(summary.rows[0].window, "fable");
        assert_eq!(summary.rows[0].metric, "usage");
    }

    #[test]
    fn chatgpt_spark_usage_shortens_to_the_model_version() {
        let summary = summarize(&usage(vec![window(
            UsageWindowKind::Weekly,
            vec![percent("GPT-5.3-Codex-Spark_usage", 42.0)],
        )]));

        assert_eq!(summary.rows[0].metric, "GPT-5.3");
    }

    #[test]
    fn a_window_of_only_hidden_booleans_produces_no_rows() {
        let summary = summarize(&usage(vec![window(
            UsageWindowKind::Other {
                id: "codex_status".into(),
                label: "codex status".into(),
            },
            vec![boolean("allowed", true), boolean("limit_reached", false)],
        )]));

        assert!(summary.is_empty());
    }

    #[test]
    fn summary_carries_the_timestamps_the_header_lines_need() {
        let expires_at = Utc.with_ymd_and_hms(2026, 12, 1, 0, 0, 0).unwrap();
        let mut source = usage(vec![window(
            UsageWindowKind::Weekly,
            vec![percent("included_usage", 1.0)],
        )]);
        source.subscription_expires_at = Some(expires_at);

        let summary = summarize(&source);
        assert_eq!(summary.expires_at, Some(expires_at));
        assert_eq!(summary.observed_at, source.observed_at);
    }
}
