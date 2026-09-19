//! sub2api admin API response types and the normalization into Ullage's
//! shared usage model.
//!
//! The admin API wraps every payload in a `{code, message, data}` envelope;
//! `api.rs` unwraps it, so the types here model only the `data` payloads of
//! `GET /api/v1/admin/accounts` and
//! `GET /api/v1/admin/accounts/:id/usage`. `UsageInfo` fields are all
//! optional because the gateway fills in only the windows that exist for the
//! account's platform (openai/anthropic windows differ from gemini, grok, and
//! antigravity ones).

use std::collections::BTreeMap;

use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};
use serde_json::Value;
use ullage_core::{
    MeasurementUnit, ProviderId, ProviderResult, SubscriptionUsage, UsageMeasurement, UsageWindow,
    UsageWindowKind, is_unsafe_identity_character,
};

/// One entry of the paged `GET /api/v1/admin/accounts` listing. Only the
/// fields the provider needs are modeled; everything else is skipped.
#[derive(Clone, Debug, Default, PartialEq, Serialize, Deserialize)]
pub struct AdminAccount {
    pub id: i64,
    #[serde(default)]
    pub name: Option<String>,
    #[serde(default)]
    pub platform: Option<String>,
    #[serde(rename = "type", default)]
    pub account_type: Option<String>,
    #[serde(default)]
    pub status: Option<String>,
    /// Loose object the gateway uses for per-platform extras; codex-style
    /// accounts keep their passive usage snapshot inside it.
    #[serde(default)]
    pub credentials: Option<Value>,
    #[serde(default)]
    pub error_message: Option<String>,
}

/// The `data` payload of the paged accounts listing.
#[derive(Clone, Debug, Default, PartialEq, Serialize, Deserialize)]
pub struct AccountsPage {
    #[serde(default)]
    pub items: Vec<AdminAccount>,
    #[serde(default)]
    pub total: i64,
    #[serde(default)]
    pub page: i64,
    #[serde(default)]
    pub page_size: i64,
    #[serde(default)]
    pub pages: i64,
}

/// Statistics accumulated over a quota window, reported per window when the
/// gateway tracks them.
#[derive(Clone, Debug, Default, PartialEq, Serialize, Deserialize)]
pub struct WindowStats {
    #[serde(default)]
    pub requests: i64,
    #[serde(default)]
    pub tokens: i64,
    #[serde(default)]
    pub cost: f64,
    #[serde(default)]
    pub standard_cost: f64,
    #[serde(default)]
    pub user_cost: f64,
}

/// One quota window as `UsageInfo` reports it: utilization as a percentage
/// (0-100+), an optional reset time, and optional window statistics.
#[derive(Clone, Debug, Default, PartialEq, Serialize, Deserialize)]
pub struct UsageProgress {
    #[serde(default)]
    pub utilization: Option<f64>,
    #[serde(default)]
    pub resets_at: Option<DateTime<Utc>>,
    #[serde(default)]
    pub remaining_seconds: Option<i64>,
    #[serde(default)]
    pub window_stats: Option<WindowStats>,
    #[serde(default)]
    pub used_requests: Option<i64>,
    #[serde(default)]
    pub limit_requests: Option<i64>,
}

/// An antigravity per-model quota entry: `utilization` 0-100 and an ISO-8601
/// reset timestamp that does not always arrive in strict RFC 3339 shape.
#[derive(Clone, Debug, Default, PartialEq, Serialize, Deserialize)]
pub struct AntigravityModelQuota {
    #[serde(default)]
    pub utilization: Option<f64>,
    #[serde(default)]
    pub reset_time: Option<String>,
}

/// A Grok quota window: a limit/remaining pair plus reset time delivered as
/// unix seconds, an ISO string, or both.
#[derive(Clone, Debug, Default, PartialEq, Serialize, Deserialize)]
pub struct QuotaWindow {
    #[serde(default)]
    pub limit: Option<i64>,
    #[serde(default)]
    pub remaining: Option<i64>,
    #[serde(default)]
    pub reset_unix: Option<i64>,
    #[serde(default)]
    pub reset_at: Option<String>,
}

/// An Antigravity AI Credits balance entry.
#[derive(Clone, Debug, Default, PartialEq, Serialize, Deserialize)]
pub struct AiCredit {
    #[serde(default)]
    pub credit_type: Option<String>,
    #[serde(default)]
    pub amount: Option<f64>,
    #[serde(default)]
    pub minimum_balance: Option<f64>,
}

/// The `data` payload of `GET /api/v1/admin/accounts/:id/usage`. Fields are
/// optional because the platform decides which windows exist.
#[derive(Clone, Debug, Default, PartialEq, Serialize, Deserialize)]
pub struct UsageInfo {
    #[serde(default)]
    pub source: Option<String>,
    #[serde(default)]
    pub updated_at: Option<DateTime<Utc>>,
    #[serde(default)]
    pub five_hour: Option<UsageProgress>,
    #[serde(default)]
    pub seven_day: Option<UsageProgress>,
    #[serde(default)]
    pub seven_day_sonnet: Option<UsageProgress>,
    #[serde(default)]
    pub seven_day_fable: Option<UsageProgress>,
    #[serde(default)]
    pub thirty_day: Option<UsageProgress>,
    #[serde(default)]
    pub gemini_shared_daily: Option<UsageProgress>,
    #[serde(default)]
    pub gemini_pro_daily: Option<UsageProgress>,
    #[serde(default)]
    pub gemini_flash_daily: Option<UsageProgress>,
    #[serde(default)]
    pub gemini_shared_minute: Option<UsageProgress>,
    #[serde(default)]
    pub gemini_pro_minute: Option<UsageProgress>,
    #[serde(default)]
    pub gemini_flash_minute: Option<UsageProgress>,
    #[serde(default)]
    pub antigravity_quota: Option<BTreeMap<String, AntigravityModelQuota>>,
    #[serde(default)]
    pub grok_request_quota: Option<QuotaWindow>,
    #[serde(default)]
    pub grok_token_quota: Option<QuotaWindow>,
    #[serde(default)]
    pub subscription_tier: Option<String>,
    #[serde(default)]
    pub subscription_tier_raw: Option<String>,
    #[serde(default)]
    pub ai_credits: Vec<AiCredit>,
    /// The gateway reports degraded upstream state here instead of failing
    /// the request (for example `unauthenticated` when the upstream token
    /// died). `query` surfaces it as a `QueryOutcome::Partial` failure; the
    /// fields do not appear in the normalized `SubscriptionUsage`.
    #[serde(default)]
    pub error_code: Option<String>,
    #[serde(default)]
    pub error: Option<String>,
}

/// The vendor payload carried between `query` and `normalize`.
#[derive(Clone, Debug, PartialEq)]
pub struct Sub2apiUsage {
    pub account_label: Option<String>,
    pub info: UsageInfo,
    pub observed_at: DateTime<Utc>,
}

pub fn normalize(usage: Sub2apiUsage) -> ProviderResult<SubscriptionUsage> {
    let info = &usage.info;
    let mut windows = Vec::new();
    push_progress(&mut windows, UsageWindowKind::FiveHours, &info.five_hour);
    push_progress(&mut windows, UsageWindowKind::Weekly, &info.seven_day);
    push_progress(
        &mut windows,
        other_window("seven_day_sonnet", "7d Sonnet"),
        &info.seven_day_sonnet,
    );
    push_progress(
        &mut windows,
        other_window("seven_day_fable", "7d Fable"),
        &info.seven_day_fable,
    );
    push_progress(&mut windows, UsageWindowKind::Monthly, &info.thirty_day);
    push_progress(
        &mut windows,
        other_window("gemini_shared_daily", "Gemini shared daily"),
        &info.gemini_shared_daily,
    );
    push_progress(
        &mut windows,
        other_window("gemini_pro_daily", "Gemini Pro daily"),
        &info.gemini_pro_daily,
    );
    push_progress(
        &mut windows,
        other_window("gemini_flash_daily", "Gemini Flash daily"),
        &info.gemini_flash_daily,
    );
    push_progress(
        &mut windows,
        other_window("gemini_shared_minute", "Gemini shared minute"),
        &info.gemini_shared_minute,
    );
    push_progress(
        &mut windows,
        other_window("gemini_pro_minute", "Gemini Pro minute"),
        &info.gemini_pro_minute,
    );
    push_progress(
        &mut windows,
        other_window("gemini_flash_minute", "Gemini Flash minute"),
        &info.gemini_flash_minute,
    );
    if let Some(quota) = &info.antigravity_quota {
        for (model, entry) in quota {
            let model = sanitized_or_fallback(model, "unknown-model");
            let mut measurements = Vec::new();
            if let Some(utilization) = entry.utilization {
                measurements.push(UsageMeasurement {
                    name: "usage".into(),
                    used: utilization,
                    limit: Some(100.0),
                    unit: MeasurementUnit::Percent,
                });
            }
            if measurements.is_empty() {
                continue;
            }
            windows.push(UsageWindow {
                window: UsageWindowKind::Other {
                    id: format!("antigravity_quota:{model}"),
                    label: model,
                },
                resets_at: entry.reset_time.as_deref().and_then(parse_gateway_datetime),
                measurements,
            });
        }
    }
    push_quota(
        &mut windows,
        other_window("grok_request_quota", "Grok request quota"),
        &info.grok_request_quota,
        MeasurementUnit::Requests,
        "requests",
    );
    push_quota(
        &mut windows,
        other_window("grok_token_quota", "Grok token quota"),
        &info.grok_token_quota,
        MeasurementUnit::Tokens,
        "tokens",
    );
    if !info.ai_credits.is_empty() {
        let measurements = info
            .ai_credits
            .iter()
            .map(|credit| UsageMeasurement {
                name: credit
                    .credit_type
                    .as_deref()
                    .map(sanitize_gateway_text)
                    .filter(|name| !name.is_empty())
                    .unwrap_or_else(|| "credits".into()),
                used: credit.amount.unwrap_or(0.0),
                limit: None,
                unit: MeasurementUnit::Credits,
            })
            .collect();
        windows.push(UsageWindow {
            window: UsageWindowKind::Other {
                id: "ai_credits".into(),
                label: "AI Credits".into(),
            },
            resets_at: None,
            measurements,
        });
    }
    Ok(SubscriptionUsage {
        provider: ProviderId::new("sub2api"),
        account_label: sanitize_optional_gateway_text(usage.account_label),
        plan: info
            .subscription_tier
            .as_deref()
            .map(sanitize_gateway_text)
            .filter(|tier| !tier.is_empty())
            .or_else(|| {
                info.subscription_tier_raw
                    .as_deref()
                    .map(sanitize_gateway_text)
                    .filter(|tier| !tier.is_empty())
            }),
        subscription_expires_at: None,
        observed_at: usage.observed_at,
        windows,
    })
}

/// Strips control and bidirectional characters from gateway-owned text before
/// it enters shared DTOs, logs, or terminal output.
pub fn sanitize_gateway_text(value: &str) -> String {
    value
        .chars()
        .filter(|character| !is_unsafe_identity_character(*character))
        .collect()
}

pub fn sanitize_optional_gateway_text(value: Option<String>) -> Option<String> {
    value
        .map(|text| sanitize_gateway_text(&text))
        .filter(|text| !text.is_empty())
}

fn sanitized_or_fallback(value: &str, fallback: &str) -> String {
    let sanitized = sanitize_gateway_text(value);
    if sanitized.is_empty() {
        fallback.into()
    } else {
        sanitized
    }
}

fn other_window(id: &str, label: &str) -> UsageWindowKind {
    UsageWindowKind::Other {
        id: id.into(),
        label: label.into(),
    }
}

/// Maps one `UsageProgress` to a shared window. Utilization becomes a percent
/// measurement and the gateway's window statistics ride along as extra
/// measurements, matching the "usage plus window cost" shape the gateway
/// reports.
fn push_progress(
    windows: &mut Vec<UsageWindow>,
    kind: UsageWindowKind,
    progress: &Option<UsageProgress>,
) {
    let Some(progress) = progress else {
        return;
    };
    let mut measurements = Vec::new();
    if let Some(utilization) = progress.utilization {
        measurements.push(UsageMeasurement {
            name: "usage".into(),
            used: utilization,
            limit: Some(100.0),
            unit: MeasurementUnit::Percent,
        });
    }
    // `used_requests`/`limit_requests` and `window_stats.requests` both
    // report a request count; when the stats block is present it already
    // carries that row, so the pair is only emitted on its own.
    if progress.window_stats.is_none()
        && (progress.used_requests.is_some() || progress.limit_requests.is_some())
    {
        measurements.push(UsageMeasurement {
            name: "requests".into(),
            used: progress.used_requests.unwrap_or(0) as f64,
            limit: progress.limit_requests.map(|limit| limit as f64),
            unit: MeasurementUnit::Requests,
        });
    }
    if let Some(stats) = &progress.window_stats {
        push_stat(
            &mut measurements,
            "requests",
            stats.requests,
            MeasurementUnit::Requests,
        );
        push_stat(
            &mut measurements,
            "tokens",
            stats.tokens,
            MeasurementUnit::Tokens,
        );
        push_cost(&mut measurements, "cost", stats.cost);
        push_cost(&mut measurements, "standard_cost", stats.standard_cost);
        push_cost(&mut measurements, "user_cost", stats.user_cost);
    }
    if measurements.is_empty() {
        return;
    }
    windows.push(UsageWindow {
        window: kind,
        resets_at: progress.resets_at,
        measurements,
    });
}

fn push_stat(
    measurements: &mut Vec<UsageMeasurement>,
    name: &str,
    value: i64,
    unit: MeasurementUnit,
) {
    measurements.push(UsageMeasurement {
        name: name.into(),
        used: value as f64,
        limit: None,
        unit,
    });
}

/// Window spend is emitted in dollars but deliberately not as
/// `MeasurementUnit::Currency`: the summary maps a limitless currency to a
/// "balance" reading, which would present consumed window cost as money on
/// hand. A named unit keeps the number honest (`used 0.42`).
fn push_cost(measurements: &mut Vec<UsageMeasurement>, name: &str, usd: f64) {
    measurements.push(UsageMeasurement {
        name: name.into(),
        used: usd,
        limit: None,
        unit: MeasurementUnit::Other {
            id: "usd".into(),
            label: "USD".into(),
        },
    });
}

/// Maps a Grok quota window: `limit - remaining` is the consumed amount when
/// both sides arrive, otherwise the remaining count alone is reported.
fn push_quota(
    windows: &mut Vec<UsageWindow>,
    kind: UsageWindowKind,
    quota: &Option<QuotaWindow>,
    unit: MeasurementUnit,
    name: &str,
) {
    let Some(quota) = quota else {
        return;
    };
    let mut measurements = Vec::new();
    match (quota.limit, quota.remaining) {
        (Some(limit), Some(remaining)) => measurements.push(UsageMeasurement {
            name: name.into(),
            used: (limit - remaining).max(0) as f64,
            limit: Some(limit as f64),
            unit: unit.clone(),
        }),
        (Some(limit), None) => measurements.push(UsageMeasurement {
            name: name.into(),
            used: 0.0,
            limit: Some(limit as f64),
            unit: unit.clone(),
        }),
        (None, Some(remaining)) => measurements.push(UsageMeasurement {
            name: "remaining".into(),
            used: remaining as f64,
            limit: None,
            unit: unit.clone(),
        }),
        (None, None) => {}
    }
    if measurements.is_empty() {
        return;
    }
    windows.push(UsageWindow {
        window: kind,
        resets_at: quota
            .reset_unix
            .and_then(unix_to_datetime)
            .or_else(|| quota.reset_at.as_deref().and_then(parse_gateway_datetime)),
        measurements,
    });
}

fn unix_to_datetime(seconds: i64) -> Option<DateTime<Utc>> {
    let seconds = if seconds > 1_000_000_000_000 {
        seconds / 1000
    } else {
        seconds
    };
    DateTime::from_timestamp(seconds, 0)
}

/// The gateway's reset strings are ISO-8601; accept RFC 3339 and the common
/// `YYYY-MM-DD HH:MM:SS` variant so a formatted timestamp never silently
/// drops the reset time.
fn parse_gateway_datetime(text: &str) -> Option<DateTime<Utc>> {
    let text = text.trim();
    if let Ok(parsed) = DateTime::parse_from_rfc3339(text) {
        return Some(parsed.with_timezone(&Utc));
    }
    chrono::NaiveDateTime::parse_from_str(text, "%Y-%m-%d %H:%M:%S")
        .ok()
        .map(|naive| naive.and_utc())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn progress(utilization: f64, resets_at: Option<DateTime<Utc>>) -> UsageProgress {
        UsageProgress {
            utilization: Some(utilization),
            resets_at,
            ..UsageProgress::default()
        }
    }

    #[test]
    fn parses_usage_info_with_platform_specific_windows() {
        let info: UsageInfo = serde_json::from_str(
            r#"{
                "source": "active",
                "five_hour": {"utilization": 42.5, "resets_at": "2026-09-19T08:00:00Z",
                    "window_stats": {"requests": 17, "tokens": 1200, "cost": 0.42,
                        "standard_cost": 0.5, "user_cost": 0.4}},
                "seven_day": {"utilization": 10},
                "seven_day_sonnet": {"utilization": 3},
                "subscription_tier": "PRO"
            }"#,
        )
        .unwrap();
        assert_eq!(info.five_hour.unwrap().utilization, Some(42.5));
        assert!(info.seven_day.is_some());
        assert!(info.gemini_pro_daily.is_none());
        assert_eq!(info.subscription_tier.as_deref(), Some("PRO"));
    }

    #[test]
    fn normalize_maps_openai_style_windows() {
        let resets_at = DateTime::parse_from_rfc3339("2026-09-19T08:00:00Z")
            .unwrap()
            .with_timezone(&Utc);
        let usage = Sub2apiUsage {
            account_label: Some("work".into()),
            info: UsageInfo {
                five_hour: Some(UsageProgress {
                    utilization: Some(42.5),
                    resets_at: Some(resets_at),
                    window_stats: Some(WindowStats {
                        requests: 17,
                        tokens: 1200,
                        cost: 0.42,
                        standard_cost: 0.5,
                        user_cost: 0.4,
                    }),
                    ..UsageProgress::default()
                }),
                seven_day: Some(progress(10.0, None)),
                subscription_tier: Some("PRO".into()),
                ..UsageInfo::default()
            },
            observed_at: Utc::now(),
        };
        let normalized = normalize(usage).unwrap();
        assert_eq!(normalized.provider.as_str(), "sub2api");
        assert_eq!(normalized.account_label.as_deref(), Some("work"));
        assert_eq!(normalized.plan.as_deref(), Some("PRO"));
        assert_eq!(normalized.windows.len(), 2);
        let five_hour = &normalized.windows[0];
        assert_eq!(five_hour.window, UsageWindowKind::FiveHours);
        assert_eq!(five_hour.resets_at, Some(resets_at));
        assert_eq!(five_hour.measurements[0].used, 42.5);
        assert_eq!(five_hour.measurements[0].unit, MeasurementUnit::Percent);
        let cost = five_hour
            .measurements
            .iter()
            .find(|measurement| measurement.name == "cost")
            .unwrap();
        assert_eq!(cost.used, 0.42);
        // Window spend must not be a limitless Currency: the summary would
        // render it as a balance rather than as consumed cost.
        assert!(matches!(
            cost.unit,
            MeasurementUnit::Other { ref id, .. } if id == "usd"
        ));
        assert_eq!(normalized.windows[1].window, UsageWindowKind::Weekly);
    }

    #[test]
    fn normalize_maps_antigravity_quota_and_credits() {
        let mut quota = BTreeMap::new();
        quota.insert(
            "gemini-3-pro".to_owned(),
            AntigravityModelQuota {
                utilization: Some(55.0),
                reset_time: Some("2026-09-20 12:00:00".into()),
            },
        );
        let usage = Sub2apiUsage {
            account_label: None,
            info: UsageInfo {
                antigravity_quota: Some(quota),
                ai_credits: vec![AiCredit {
                    credit_type: Some("credits".into()),
                    amount: Some(12.5),
                    minimum_balance: None,
                }],
                subscription_tier: Some("ULTRA".into()),
                ..UsageInfo::default()
            },
            observed_at: Utc::now(),
        };
        let normalized = normalize(usage).unwrap();
        assert_eq!(normalized.windows.len(), 2);
        assert_eq!(
            normalized.windows[0].window,
            UsageWindowKind::Other {
                id: "antigravity_quota:gemini-3-pro".into(),
                label: "gemini-3-pro".into()
            }
        );
        assert!(normalized.windows[0].resets_at.is_some());
        assert_eq!(
            normalized.windows[1].measurements[0].unit,
            MeasurementUnit::Credits
        );
        assert_eq!(normalized.windows[1].measurements[0].used, 12.5);
    }

    #[test]
    fn normalize_maps_grok_quota_windows() {
        let usage = Sub2apiUsage {
            account_label: None,
            info: UsageInfo {
                grok_request_quota: Some(QuotaWindow {
                    limit: Some(100),
                    remaining: Some(25),
                    reset_unix: Some(1_800_000_000),
                    reset_at: None,
                }),
                grok_token_quota: Some(QuotaWindow {
                    limit: None,
                    remaining: Some(5000),
                    reset_unix: None,
                    reset_at: Some("2026-09-19T08:00:00Z".into()),
                }),
                ..UsageInfo::default()
            },
            observed_at: Utc::now(),
        };
        let normalized = normalize(usage).unwrap();
        assert_eq!(normalized.windows.len(), 2);
        let requests = &normalized.windows[0];
        assert_eq!(requests.measurements[0].used, 75.0);
        assert_eq!(requests.measurements[0].limit, Some(100.0));
        assert_eq!(requests.measurements[0].unit, MeasurementUnit::Requests);
        let tokens = &normalized.windows[1];
        assert_eq!(tokens.measurements[0].name, "remaining");
        assert_eq!(tokens.measurements[0].used, 5000.0);
        assert_eq!(tokens.measurements[0].unit, MeasurementUnit::Tokens);
        assert!(tokens.resets_at.is_some());
    }

    #[test]
    fn normalize_skips_absent_windows_and_falls_back_to_raw_tier() {
        let usage = Sub2apiUsage {
            account_label: None,
            info: UsageInfo {
                seven_day: Some(progress(1.0, None)),
                subscription_tier_raw: Some("claude-pro".into()),
                ..UsageInfo::default()
            },
            observed_at: Utc::now(),
        };
        let normalized = normalize(usage).unwrap();
        assert_eq!(normalized.windows.len(), 1);
        assert_eq!(normalized.windows[0].window, UsageWindowKind::Weekly);
        assert_eq!(normalized.plan.as_deref(), Some("claude-pro"));
    }

    #[test]
    fn windows_without_measurements_are_dropped() {
        let usage = Sub2apiUsage {
            account_label: None,
            info: UsageInfo {
                five_hour: Some(UsageProgress::default()),
                ..UsageInfo::default()
            },
            observed_at: Utc::now(),
        };
        let normalized = normalize(usage).unwrap();
        assert!(normalized.windows.is_empty());
    }

    #[test]
    fn normalize_sanitizes_gateway_owned_text() {
        let mut quota = BTreeMap::new();
        quota.insert(
            "model\u{202e}\u{1b}".into(),
            AntigravityModelQuota {
                utilization: Some(1.0),
                reset_time: None,
            },
        );
        let normalized = normalize(Sub2apiUsage {
            account_label: Some("work\u{202e}\u{1b}".into()),
            info: UsageInfo {
                antigravity_quota: Some(quota),
                subscription_tier: Some("PRO\u{202e}\u{1b}".into()),
                ai_credits: vec![AiCredit {
                    credit_type: Some("credit\u{202e}\u{1b}".into()),
                    amount: Some(1.0),
                    minimum_balance: None,
                }],
                ..UsageInfo::default()
            },
            observed_at: Utc::now(),
        })
        .unwrap();

        assert_eq!(normalized.account_label.as_deref(), Some("work"));
        assert_eq!(normalized.plan.as_deref(), Some("PRO"));
        assert_eq!(
            normalized.windows[0].window,
            UsageWindowKind::Other {
                id: "antigravity_quota:model".into(),
                label: "model".into(),
            }
        );
        assert_eq!(normalized.windows[1].measurements[0].name, "credit");
    }
}
