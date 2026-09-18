use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};
use ullage_core::{
    MeasurementUnit, ProviderId, ProviderResult, SubscriptionUsage, UsageMeasurement, UsageWindow,
    UsageWindowKind,
};

/// The usage endpoint answers a single `usage` object holding the three
/// windows a Go subscription meters. Any window may be absent.
#[derive(Clone, Debug, Default, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct UsageResponse {
    #[serde(default)]
    pub usage: UsageWindows,
}

#[derive(Clone, Debug, Default, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct UsageWindows {
    pub rolling: Option<WindowUsage>,
    pub weekly: Option<WindowUsage>,
    pub monthly: Option<WindowUsage>,
}

#[derive(Clone, Debug, Default, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct WindowUsage {
    pub status: Option<String>,
    /// Percentage of the window's quota already consumed.
    pub percent: Option<f64>,
    /// When the window resets. OpenCode reports a placeholder `now + window`
    /// value while `percent` is zero, so the field is only meaningful once
    /// the window has measured usage.
    pub resets_at: Option<DateTime<Utc>>,
}

#[derive(Clone, Debug, PartialEq)]
pub struct OpencodeUsage {
    pub account_label: Option<String>,
    pub usage: UsageWindows,
    pub observed_at: DateTime<Utc>,
}

pub fn normalize(usage: OpencodeUsage) -> ProviderResult<SubscriptionUsage> {
    let mut windows = Vec::new();
    push_window(
        &mut windows,
        UsageWindowKind::FiveHours,
        &usage.usage.rolling,
    );
    push_window(&mut windows, UsageWindowKind::Weekly, &usage.usage.weekly);
    push_window(&mut windows, UsageWindowKind::Monthly, &usage.usage.monthly);
    Ok(SubscriptionUsage {
        provider: ProviderId::new("opencode"),
        account_label: usage.account_label,
        plan: Some("Go".into()),
        subscription_expires_at: None,
        observed_at: usage.observed_at,
        windows,
    })
}

fn push_window(
    windows: &mut Vec<UsageWindow>,
    kind: UsageWindowKind,
    window: &Option<WindowUsage>,
) {
    let Some(window) = window else {
        return;
    };
    let percent = window.percent.unwrap_or(0.0);
    windows.push(UsageWindow {
        window: kind,
        // A zero reading means the window never started; `resetsAt` is a
        // placeholder then and would report a meaningless reset time.
        resets_at: if percent == 0.0 {
            None
        } else {
            window.resets_at
        },
        measurements: window
            .percent
            .map(|percent| UsageMeasurement {
                name: "usage".into(),
                used: percent,
                limit: Some(100.0),
                unit: MeasurementUnit::Percent,
            })
            .into_iter()
            .collect(),
    });
}

#[cfg(test)]
mod tests {
    use super::*;
    use chrono::TimeZone;

    #[test]
    fn parses_the_three_windows_and_tolerates_missing_ones() {
        let response: UsageResponse = serde_json::from_str(
            r#"{"usage":{"rolling":{"status":"ok","percent":6,"resetsAt":"2026-09-18T19:00:00Z"},"weekly":{"status":"ok","percent":12,"resetsAt":"2026-09-25T00:00:00Z"}}}"#,
        )
        .unwrap();
        assert_eq!(response.usage.rolling.as_ref().unwrap().percent, Some(6.0));
        assert!(response.usage.weekly.is_some());
        assert!(response.usage.monthly.is_none());
    }

    #[test]
    fn normalize_maps_windows_and_drops_placeholder_reset_times() {
        let resets_at = Utc.with_ymd_and_hms(2026, 9, 18, 19, 0, 0).unwrap();
        let usage = OpencodeUsage {
            account_label: None,
            usage: UsageWindows {
                rolling: Some(WindowUsage {
                    status: Some("ok".into()),
                    percent: Some(6.0),
                    resets_at: Some(resets_at),
                }),
                weekly: Some(WindowUsage {
                    status: Some("ok".into()),
                    percent: Some(0.0),
                    resets_at: Some(resets_at),
                }),
                monthly: None,
            },
            observed_at: Utc::now(),
        };
        let normalized = normalize(usage).unwrap();
        assert_eq!(normalized.provider.as_str(), "opencode");
        assert_eq!(normalized.windows.len(), 2);
        assert_eq!(normalized.windows[0].window, UsageWindowKind::FiveHours);
        assert_eq!(normalized.windows[0].resets_at, Some(resets_at));
        assert_eq!(normalized.windows[1].window, UsageWindowKind::Weekly);
        assert_eq!(normalized.windows[1].resets_at, None);
        assert_eq!(normalized.windows[0].measurements[0].used, 6.0);
        assert_eq!(
            normalized.windows[0].measurements[0].unit,
            MeasurementUnit::Percent
        );
    }
}
