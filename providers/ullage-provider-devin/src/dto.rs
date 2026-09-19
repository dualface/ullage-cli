//! Devin `GetUserStatus` response types and the normalization into Ullage's
//! shared usage model.
//!
//! Devin reports quota as a *remaining* percentage, the opposite direction of
//! providers that report usage consumed, so normalization inverts it.

use chrono::{DateTime, TimeZone, Utc};
use serde::{Deserialize, Deserializer, Serialize};
use serde_json::Value;
use ullage_core::{
    MeasurementUnit, ProviderId, ProviderResult, SubscriptionUsage, UsageMeasurement, UsageWindow,
    UsageWindowKind, is_unsafe_identity_character,
};

/// The Connect-RPC JSON answer nests the plan state under
/// `userStatus.planStatus`; both levels may be absent on a degraded reply.
#[derive(Clone, Debug, Default, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct UserStatusResponse {
    #[serde(default)]
    pub user_status: Option<UserStatus>,
}

#[derive(Clone, Debug, Default, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct UserStatus {
    #[serde(default)]
    pub plan_status: Option<PlanStatus>,
}

#[derive(Clone, Debug, Default, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct PlanStatus {
    #[serde(default)]
    pub plan_info: Option<PlanInfo>,
    /// Remaining share of the daily quota, as a percentage.
    #[serde(default)]
    pub daily_quota_remaining_percent: Option<f64>,
    /// Remaining share of the weekly quota, as a percentage.
    #[serde(default)]
    pub weekly_quota_remaining_percent: Option<f64>,
    /// Unix reset times arrive as numbers or numeric strings.
    #[serde(default, deserialize_with = "optional_unix")]
    pub daily_quota_reset_at_unix: Option<i64>,
    #[serde(default, deserialize_with = "optional_unix")]
    pub weekly_quota_reset_at_unix: Option<i64>,
    /// Plan period bounds: RFC 3339 timestamps or unix seconds.
    #[serde(default, deserialize_with = "optional_datetime")]
    pub plan_start: Option<DateTime<Utc>>,
    #[serde(default, deserialize_with = "optional_datetime")]
    pub plan_end: Option<DateTime<Utc>>,
    /// Negative values mean the plan does not meter prompt credits.
    #[serde(default, deserialize_with = "optional_f64")]
    pub available_prompt_credits: Option<f64>,
    /// Compute-unit consumption; absent on plans that are not quota-metered.
    #[serde(default, deserialize_with = "optional_f64")]
    pub acu_consumed: Option<f64>,
    #[serde(default, deserialize_with = "optional_f64")]
    pub acu_limit: Option<f64>,
}

#[derive(Clone, Debug, Default, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct PlanInfo {
    pub plan_name: Option<String>,
}

/// The vendor payload carried between `query` and `normalize`.
#[derive(Clone, Debug, PartialEq)]
pub struct DevinUsage {
    pub plan_status: PlanStatus,
    pub observed_at: DateTime<Utc>,
}

pub fn normalize(usage: DevinUsage) -> ProviderResult<SubscriptionUsage> {
    let status = &usage.plan_status;
    let mut windows = Vec::new();
    push_quota_window(
        &mut windows,
        UsageWindowKind::Other {
            id: "daily".into(),
            label: "Daily".into(),
        },
        status.daily_quota_remaining_percent,
        status.daily_quota_reset_at_unix,
    );
    push_quota_window(
        &mut windows,
        UsageWindowKind::Weekly,
        status.weekly_quota_remaining_percent,
        status.weekly_quota_reset_at_unix,
    );
    let mut credit_measurements = Vec::new();
    if status.acu_consumed.is_some() || status.acu_limit.is_some() {
        credit_measurements.push(UsageMeasurement {
            name: "acu".into(),
            used: status.acu_consumed.unwrap_or(0.0),
            limit: status.acu_limit,
            unit: MeasurementUnit::Other {
                id: "acu".into(),
                label: "ACU".into(),
            },
        });
    }
    // A negative credit count means the plan does not meter prompt credits;
    // reporting it as a balance would be meaningless.
    if let Some(credits) = status
        .available_prompt_credits
        .filter(|credits| *credits >= 0.0)
    {
        credit_measurements.push(UsageMeasurement {
            name: "available_prompt_credits".into(),
            used: credits,
            limit: None,
            unit: MeasurementUnit::Credits,
        });
    }
    if !credit_measurements.is_empty() {
        windows.push(UsageWindow {
            window: UsageWindowKind::Other {
                id: "credits".into(),
                label: "Credits".into(),
            },
            resets_at: None,
            measurements: credit_measurements,
        });
    }
    Ok(SubscriptionUsage {
        provider: ProviderId::new("devin"),
        account_label: None,
        plan: status
            .plan_info
            .as_ref()
            .and_then(|info| info.plan_name.as_deref())
            .map(sanitize_vendor_text)
            .filter(|plan| !plan.is_empty()),
        subscription_expires_at: status.plan_end,
        observed_at: usage.observed_at,
        windows,
    })
}

fn sanitize_vendor_text(value: &str) -> String {
    value
        .chars()
        .filter(|character| !is_unsafe_identity_character(*character))
        .collect()
}

fn push_quota_window(
    windows: &mut Vec<UsageWindow>,
    kind: UsageWindowKind,
    remaining_percent: Option<f64>,
    reset_at_unix: Option<i64>,
) {
    let Some(remaining) = remaining_percent else {
        return;
    };
    windows.push(UsageWindow {
        window: kind,
        resets_at: reset_at_unix.and_then(unix_to_datetime),
        measurements: vec![UsageMeasurement {
            name: "usage".into(),
            // Devin reports what is left; the shared model reports what was
            // consumed, so the percentage is inverted here.
            used: (100.0 - remaining).clamp(0.0, 100.0),
            limit: Some(100.0),
            unit: MeasurementUnit::Percent,
        }],
    });
}

/// Unix seconds; millisecond-scale values are scaled down like the other
/// quota monitors do for the same endpoint.
fn unix_to_datetime(seconds: i64) -> Option<DateTime<Utc>> {
    let seconds = if seconds > 1_000_000_000_000 {
        seconds / 1000
    } else {
        seconds
    };
    Utc.timestamp_opt(seconds, 0).single()
}

fn optional_unix<'de, D>(deserializer: D) -> Result<Option<i64>, D::Error>
where
    D: Deserializer<'de>,
{
    Ok(Option::<Value>::deserialize(deserializer)?.and_then(|value| value_to_i64(&value)))
}

fn optional_f64<'de, D>(deserializer: D) -> Result<Option<f64>, D::Error>
where
    D: Deserializer<'de>,
{
    Ok(Option::<Value>::deserialize(deserializer)?.and_then(|value| value_to_f64(&value)))
}

fn optional_datetime<'de, D>(deserializer: D) -> Result<Option<DateTime<Utc>>, D::Error>
where
    D: Deserializer<'de>,
{
    Ok(
        Option::<Value>::deserialize(deserializer)?.and_then(|value| {
            if let Some(text) = value.as_str() {
                if let Ok(parsed) = DateTime::parse_from_rfc3339(text) {
                    return Some(parsed.with_timezone(&Utc));
                }
                return text.parse::<i64>().ok().and_then(unix_to_datetime);
            }
            value_to_i64(&value).and_then(unix_to_datetime)
        }),
    )
}

fn value_to_i64(value: &Value) -> Option<i64> {
    value
        .as_i64()
        .or_else(|| value.as_str()?.trim().parse::<i64>().ok())
        .or_else(|| value.as_f64().map(|float| float as i64))
}

fn value_to_f64(value: &Value) -> Option<f64> {
    value
        .as_f64()
        .or_else(|| value.as_str()?.trim().parse::<f64>().ok())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_a_full_plan_status() {
        let response: UserStatusResponse = serde_json::from_str(
            r#"{"userStatus":{"planStatus":{
                "planInfo":{"planName":"Pro"},
                "dailyQuotaRemainingPercent":67,
                "weeklyQuotaRemainingPercent":83,
                "dailyQuotaResetAtUnix":1780000000,
                "weeklyQuotaResetAtUnix":"1780600000",
                "planStart":"2026-09-01T00:00:00Z",
                "planEnd":"2026-10-01T00:00:00Z",
                "availablePromptCredits":-1,
                "acuConsumed":12.5,
                "acuLimit":100
            }}}"#,
        )
        .unwrap();
        let status = response.user_status.unwrap().plan_status.unwrap();
        assert_eq!(
            status
                .plan_info
                .as_ref()
                .and_then(|info| info.plan_name.as_deref()),
            Some("Pro")
        );
        assert_eq!(status.daily_quota_remaining_percent, Some(67.0));
        assert_eq!(status.weekly_quota_reset_at_unix, Some(1780600000));
        assert_eq!(
            status.plan_end.unwrap().to_rfc3339(),
            "2026-10-01T00:00:00+00:00"
        );
        assert_eq!(status.acu_consumed, Some(12.5));
    }

    #[test]
    fn tolerates_missing_optional_fields() {
        let response: UserStatusResponse = serde_json::from_str(
            r#"{"userStatus":{"planStatus":{"planInfo":{"planName":"Core"}}}}"#,
        )
        .unwrap();
        let status = response.user_status.unwrap().plan_status.unwrap();
        assert!(status.acu_consumed.is_none());
        assert!(status.daily_quota_remaining_percent.is_none());
    }

    #[test]
    fn normalize_inverts_remaining_percentages() {
        let usage = DevinUsage {
            plan_status: PlanStatus {
                plan_info: Some(PlanInfo {
                    plan_name: Some("Pro".into()),
                }),
                daily_quota_remaining_percent: Some(67.0),
                weekly_quota_remaining_percent: Some(83.0),
                daily_quota_reset_at_unix: Some(1780000000),
                weekly_quota_reset_at_unix: Some(1780600000),
                plan_start: None,
                plan_end: Some(Utc.with_ymd_and_hms(2026, 10, 1, 0, 0, 0).unwrap()),
                available_prompt_credits: Some(-1.0),
                acu_consumed: Some(12.5),
                acu_limit: Some(100.0),
            },
            observed_at: Utc::now(),
        };
        let normalized = normalize(usage).unwrap();
        assert_eq!(normalized.provider.as_str(), "devin");
        assert_eq!(normalized.plan.as_deref(), Some("Pro"));
        assert!(normalized.subscription_expires_at.is_some());
        assert_eq!(normalized.windows.len(), 3);
        let daily = &normalized.windows[0];
        assert_eq!(
            daily.window,
            UsageWindowKind::Other {
                id: "daily".into(),
                label: "Daily".into()
            }
        );
        assert_eq!(daily.measurements[0].used, 33.0);
        assert!(daily.resets_at.is_some());
        let weekly = &normalized.windows[1];
        assert_eq!(weekly.window, UsageWindowKind::Weekly);
        assert_eq!(weekly.measurements[0].used, 17.0);
        // A -1 prompt credit balance means unmetered and is dropped; ACU stays.
        let credits = &normalized.windows[2];
        assert_eq!(credits.measurements.len(), 1);
        assert_eq!(credits.measurements[0].name, "acu");
        assert_eq!(credits.measurements[0].used, 12.5);
        assert_eq!(credits.measurements[0].limit, Some(100.0));
    }

    #[test]
    fn normalize_reports_available_prompt_credits_when_metered() {
        let usage = DevinUsage {
            plan_status: PlanStatus {
                available_prompt_credits: Some(250.0),
                ..PlanStatus::default()
            },
            observed_at: Utc::now(),
        };
        let normalized = normalize(usage).unwrap();
        assert_eq!(normalized.windows.len(), 1);
        assert_eq!(
            normalized.windows[0].measurements[0].name,
            "available_prompt_credits"
        );
        assert_eq!(normalized.windows[0].measurements[0].used, 250.0);
        assert_eq!(
            normalized.windows[0].measurements[0].unit,
            MeasurementUnit::Credits
        );
    }

    #[test]
    fn normalize_sanitizes_the_plan_name() {
        let normalized = normalize(DevinUsage {
            plan_status: PlanStatus {
                plan_info: Some(PlanInfo {
                    plan_name: Some("Pro\u{202e}\u{1b}".into()),
                }),
                ..PlanStatus::default()
            },
            observed_at: Utc::now(),
        })
        .unwrap();

        assert_eq!(normalized.plan.as_deref(), Some("Pro"));
    }
}
