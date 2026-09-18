//! Wire shapes of the codex2api admin API and the `SubscriptionUsage`
//! projection shared by the list and refresh endpoints.

use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};
use ullage_core::{
    MeasurementUnit, ProviderId, ProviderResult, SubscriptionUsage, UsageMeasurement, UsageWindow,
    UsageWindowKind,
};

/// `GET /api/admin/accounts` answers one `accounts` array whose items already
/// embed the quota fields. Only the fields Ullage reads are modeled; unknown
/// members are ignored so upstream additions stay compatible.
#[derive(Clone, Debug, Default, PartialEq, Serialize, Deserialize)]
pub struct AccountsResponse {
    #[serde(default)]
    pub accounts: Vec<GatewayAccount>,
}

/// One upstream account row as embedded in the admin account list.
#[derive(Clone, Debug, Default, PartialEq, Serialize, Deserialize)]
pub struct GatewayAccount {
    pub id: i64,
    #[serde(default)]
    pub name: Option<String>,
    #[serde(default)]
    pub email: Option<String>,
    #[serde(default)]
    pub plan_type: Option<String>,
    /// RFC 3339 timestamp; the gateway omits it for plans without an expiry.
    #[serde(default)]
    pub subscription_expires_at: Option<String>,
    #[serde(default)]
    pub status: Option<String>,
    /// Percentage of the five-hour window's quota already consumed.
    #[serde(default)]
    pub usage_percent_5h: Option<f64>,
    /// Percentage of the long window's quota already consumed. The window is
    /// nominally seven days; team plans meter it monthly instead.
    #[serde(default)]
    pub usage_percent_7d: Option<f64>,
    /// Percentage of the spark window's quota, present on Pro/Prolite
    /// accounts only.
    #[serde(default)]
    pub usage_percent_spark: Option<f64>,
    #[serde(default)]
    pub reset_5h_at: Option<String>,
    #[serde(default)]
    pub reset_7d_at: Option<String>,
    #[serde(default)]
    pub reset_spark_at: Option<String>,
    /// Real period of the `7d` window: `monthly` on team plans, `weekly` or
    /// empty otherwise.
    #[serde(default)]
    pub usage_window_7d_kind: Option<String>,
    /// Cost billed to the upstream account inside the current 5h window.
    #[serde(default)]
    pub billed_5h: Option<f64>,
    /// Cost billed inside the current long window.
    #[serde(default)]
    pub billed_7d: Option<f64>,
}

/// `POST /api/admin/accounts/:id/usage/refresh` returns only the freshly
/// probed fields; plan and billing data stay list-only.
#[derive(Clone, Debug, Default, PartialEq, Serialize, Deserialize)]
pub struct UsageRefreshResponse {
    #[serde(default)]
    pub refreshed: Option<bool>,
    #[serde(default)]
    pub usage_percent_5h: Option<f64>,
    #[serde(default)]
    pub usage_percent_7d: Option<f64>,
    #[serde(default)]
    pub usage_percent_spark: Option<f64>,
    #[serde(default)]
    pub reset_5h_at: Option<String>,
    #[serde(default)]
    pub reset_7d_at: Option<String>,
    #[serde(default)]
    pub reset_spark_at: Option<String>,
}

/// One quota window after the refresh overlay merged onto the list row.
#[derive(Clone, Debug, Default, PartialEq)]
pub struct QuotaWindow {
    pub percent: Option<f64>,
    pub resets_at: Option<DateTime<Utc>>,
    pub billed: Option<f64>,
}

/// The merged per-account usage a single query produced.
#[derive(Clone, Debug, PartialEq)]
pub struct Codex2apiUsage {
    /// Display name of the matched upstream account (email preferred).
    pub account_label: Option<String>,
    /// The gateway account id, kept for `auth_status` identity reporting.
    pub account_key: Option<String>,
    pub plan_type: Option<String>,
    pub subscription_expires_at: Option<DateTime<Utc>>,
    /// `monthly` when the gateway marks the long window as a team month
    /// window; anything else maps to the weekly kind.
    pub window_7d_kind: Option<String>,
    pub five_hours: QuotaWindow,
    pub long: QuotaWindow,
    pub spark: QuotaWindow,
    pub observed_at: DateTime<Utc>,
}

pub fn normalize(usage: Codex2apiUsage) -> ProviderResult<SubscriptionUsage> {
    let mut windows = Vec::new();
    push_window(&mut windows, UsageWindowKind::FiveHours, &usage.five_hours);
    let long_kind = if usage.window_7d_kind.as_deref() == Some("monthly") {
        UsageWindowKind::Monthly
    } else {
        UsageWindowKind::Weekly
    };
    push_window(&mut windows, long_kind, &usage.long);
    push_window(
        &mut windows,
        UsageWindowKind::Other {
            id: "spark".into(),
            label: "spark".into(),
        },
        &usage.spark,
    );
    Ok(SubscriptionUsage {
        provider: ProviderId::new("codex2api"),
        account_label: usage.account_label,
        plan: usage.plan_type,
        subscription_expires_at: usage.subscription_expires_at,
        observed_at: usage.observed_at,
        windows,
    })
}

fn push_window(windows: &mut Vec<UsageWindow>, kind: UsageWindowKind, window: &QuotaWindow) {
    if window.percent.is_none() && window.billed.is_none() {
        return;
    }
    let mut measurements = Vec::new();
    if let Some(percent) = window.percent {
        measurements.push(UsageMeasurement {
            name: "usage".into(),
            used: percent,
            limit: Some(100.0),
            unit: MeasurementUnit::Percent,
        });
    }
    if let Some(billed) = window.billed {
        measurements.push(UsageMeasurement {
            name: "billed".into(),
            used: billed,
            limit: None,
            unit: MeasurementUnit::Currency { code: "USD".into() },
        });
    }
    windows.push(UsageWindow {
        window: kind,
        resets_at: window.resets_at,
        measurements,
    });
}

/// Parses the RFC 3339 timestamps the gateway emits. An empty or unparseable
/// value means "no reset time known" rather than a protocol failure: the
/// gateway omits the field for plans that never metered the window.
pub fn parse_gateway_time(value: Option<&str>) -> Option<DateTime<Utc>> {
    let value = value?.trim();
    if value.is_empty() {
        return None;
    }
    DateTime::parse_from_rfc3339(value)
        .ok()
        .map(|instant| instant.with_timezone(&Utc))
}

/// Selects the display label for a matched account: the email identifies the
/// upstream login, so it wins over the operator-chosen name.
pub fn account_label(account: &GatewayAccount) -> Option<String> {
    account
        .email
        .as_deref()
        .filter(|email| !email.trim().is_empty())
        .or(account
            .name
            .as_deref()
            .filter(|name| !name.trim().is_empty()))
        .map(str::to_owned)
}

#[cfg(test)]
mod tests {
    use super::*;
    use chrono::TimeZone;

    #[test]
    fn parses_the_account_list_and_tolerates_missing_fields() {
        let response: AccountsResponse = serde_json::from_str(
            r#"{"accounts":[{"id":7,"name":"ops","email":"ops@example.com",
            "plan_type":"pro","subscription_expires_at":"2026-10-01T00:00:00Z",
            "status":"active","usage_percent_5h":12.5,"usage_percent_7d":40,
            "reset_5h_at":"2026-09-19T05:00:00Z","reset_7d_at":"2026-09-26T00:00:00Z",
            "usage_window_7d_kind":"monthly","billed_5h":1.25,"billed_7d":9.5},
            {"id":8,"name":"spare"}]}"#,
        )
        .unwrap();
        assert_eq!(response.accounts.len(), 2);
        let first = &response.accounts[0];
        assert_eq!(first.id, 7);
        assert_eq!(first.usage_percent_5h, Some(12.5));
        assert_eq!(first.usage_percent_spark, None);
        assert_eq!(first.billed_7d, Some(9.5));
        assert_eq!(first.usage_window_7d_kind.as_deref(), Some("monthly"));
        assert!(response.accounts[1].plan_type.is_none());
    }

    #[test]
    fn parses_the_refresh_response() {
        let response: UsageRefreshResponse = serde_json::from_str(
            r#"{"refreshed":true,"usage_percent_5h":55,"usage_percent_7d":10,
            "usage_percent_spark":3,"reset_5h_at":"2026-09-19T06:00:00Z",
            "reset_7d_at":"2026-09-26T00:00:00Z",
            "reset_spark_at":"2026-09-20T00:00:00Z"}"#,
        )
        .unwrap();
        assert_eq!(response.usage_percent_spark, Some(3.0));
        assert_eq!(
            parse_gateway_time(response.reset_spark_at.as_deref()),
            Some(Utc.with_ymd_and_hms(2026, 9, 20, 0, 0, 0).unwrap())
        );
    }

    #[test]
    fn normalize_maps_windows_plan_and_expiry() {
        let resets_at = Utc.with_ymd_and_hms(2026, 9, 19, 5, 0, 0).unwrap();
        let expires_at = Utc.with_ymd_and_hms(2026, 10, 1, 0, 0, 0).unwrap();
        let usage = Codex2apiUsage {
            account_label: Some("ops@example.com".into()),
            account_key: Some("7".into()),
            plan_type: Some("team".into()),
            subscription_expires_at: Some(expires_at),
            window_7d_kind: Some("monthly".into()),
            five_hours: QuotaWindow {
                percent: Some(12.5),
                resets_at: Some(resets_at),
                billed: Some(1.25),
            },
            long: QuotaWindow {
                percent: Some(40.0),
                resets_at: None,
                billed: None,
            },
            spark: QuotaWindow::default(),
            observed_at: Utc::now(),
        };
        let normalized = normalize(usage).unwrap();
        assert_eq!(normalized.provider.as_str(), "codex2api");
        assert_eq!(normalized.plan.as_deref(), Some("team"));
        assert_eq!(normalized.subscription_expires_at, Some(expires_at));
        assert_eq!(normalized.windows.len(), 2);
        assert_eq!(normalized.windows[0].window, UsageWindowKind::FiveHours);
        assert_eq!(normalized.windows[0].resets_at, Some(resets_at));
        assert_eq!(normalized.windows[0].measurements[0].used, 12.5);
        assert_eq!(normalized.windows[0].measurements[1].name, "billed");
        assert_eq!(normalized.windows[1].window, UsageWindowKind::Monthly);
    }

    #[test]
    fn normalize_reports_the_spark_window() {
        let usage = Codex2apiUsage {
            account_label: None,
            account_key: None,
            plan_type: None,
            subscription_expires_at: None,
            window_7d_kind: None,
            five_hours: QuotaWindow::default(),
            long: QuotaWindow::default(),
            spark: QuotaWindow {
                percent: Some(3.0),
                resets_at: None,
                billed: None,
            },
            observed_at: Utc::now(),
        };
        let normalized = normalize(usage).unwrap();
        assert_eq!(normalized.windows.len(), 1);
        assert_eq!(
            normalized.windows[0].window,
            UsageWindowKind::Other {
                id: "spark".into(),
                label: "spark".into()
            }
        );
    }

    #[test]
    fn gateway_time_tolerates_absent_and_malformed_values() {
        assert_eq!(parse_gateway_time(None), None);
        assert_eq!(parse_gateway_time(Some("")), None);
        assert_eq!(parse_gateway_time(Some("not-a-time")), None);
    }
}
