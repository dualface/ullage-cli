use chrono::{DateTime, TimeZone, Utc};
use serde::{Deserialize, Deserializer, Serialize};
use ullage_core::{
    MeasurementUnit, ProviderError, ProviderId, ProviderResult, SubscriptionUsage,
    UsageMeasurement, UsageWindow, UsageWindowKind,
};

const FIVE_HOURS_SECONDS: u64 = 5 * 60 * 60;
const WEEK_SECONDS: u64 = 7 * 24 * 60 * 60;
/// Largest consecutive integer an `f64` still represents exactly (2^53).
/// Above it only some integers stay exact, so a count past the bound is
/// rejected rather than risk reporting a silently rounded measurement.
const MAX_EXACT_F64_INTEGER: u64 = 1 << 53;

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct ChatGptWorkspace {
    pub id: String,
    pub label: Option<String>,
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct ChatGptUsageResponse {
    #[serde(default)]
    pub plan_type: Option<String>,
    #[serde(default)]
    pub rate_limit: Option<ChatGptRateLimit>,
    #[serde(default)]
    pub code_review_rate_limit: Option<ChatGptRateLimit>,
    #[serde(default)]
    pub credits: Option<ChatGptCredits>,
    #[serde(default, deserialize_with = "deserialize_lenient_list")]
    pub additional_rate_limits: Vec<ChatGptAdditionalRateLimit>,
    #[serde(default)]
    pub rate_limit_reset_credits: Option<ChatGptResetCredits>,
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct ChatGptAdditionalRateLimit {
    pub limit_name: String,
    #[serde(default)]
    pub metered_feature: Option<String>,
    #[serde(default)]
    pub rate_limit: Option<ChatGptRateLimit>,
}

#[derive(Clone, Debug, Default, PartialEq, Serialize, Deserialize)]
pub struct ChatGptRateLimit {
    #[serde(default)]
    pub allowed: Option<bool>,
    #[serde(default)]
    pub limit_reached: Option<bool>,
    #[serde(default)]
    pub primary_window: Option<ChatGptRateLimitWindow>,
    #[serde(default)]
    pub secondary_window: Option<ChatGptRateLimitWindow>,
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct ChatGptRateLimitWindow {
    pub limit_window_seconds: u64,
    pub used_percent: f64,
    #[serde(default)]
    pub reset_at: Option<i64>,
}

#[derive(Clone, Debug, Default, PartialEq, Serialize, Deserialize)]
pub struct ChatGptCredits {
    #[serde(default)]
    pub has_credits: Option<bool>,
    #[serde(default)]
    pub unlimited: Option<bool>,
    #[serde(default, deserialize_with = "deserialize_optional_number")]
    pub balance: Option<f64>,
}

#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct ChatGptResetCredits {
    #[serde(default)]
    pub available_count: Option<u64>,
}

#[derive(Clone, Debug, PartialEq)]
pub struct ChatGptUsage {
    pub workspace: ChatGptWorkspace,
    pub observed_at: DateTime<Utc>,
    pub response: ChatGptUsageResponse,
}

impl ChatGptUsage {
    pub fn normalize(self) -> ProviderResult<SubscriptionUsage> {
        let mut windows = Vec::new();
        append_rate_limit(&mut windows, "codex", self.response.rate_limit)?;
        append_rate_limit(
            &mut windows,
            "code_review",
            self.response.code_review_rate_limit,
        )?;

        for additional in self.response.additional_rate_limits {
            if additional.limit_name.trim().is_empty() {
                return Err(ProviderError::ProtocolIncompatible {
                    message: "additional rate limit has an empty name".into(),
                });
            }
            append_rate_limit(&mut windows, &additional.limit_name, additional.rate_limit)?;
        }

        if let Some(credits) = self.response.credits {
            let mut measurements = Vec::new();
            if let Some(balance) = credits.balance {
                if !balance.is_finite() || balance < 0.0 {
                    return Err(ProviderError::ProtocolIncompatible {
                        message: "credits balance is invalid".into(),
                    });
                }
                measurements.push(UsageMeasurement {
                    name: "credit_balance".into(),
                    used: balance,
                    limit: None,
                    unit: MeasurementUnit::Credits,
                });
            }
            append_boolean_measurement(&mut measurements, "has_credits", credits.has_credits);
            append_boolean_measurement(&mut measurements, "unlimited", credits.unlimited);
            if !measurements.is_empty() {
                windows.push(UsageWindow {
                    window: UsageWindowKind::Other {
                        id: "credits".into(),
                        label: "Credits".into(),
                    },
                    resets_at: None,
                    measurements,
                });
            }
        }

        if let Some(available_count) = self
            .response
            .rate_limit_reset_credits
            .and_then(|credits| credits.available_count)
        {
            if available_count > MAX_EXACT_F64_INTEGER {
                return Err(ProviderError::ProtocolIncompatible {
                    message: "rate limit reset credit count exceeds exact numeric range".into(),
                });
            }
            windows.push(UsageWindow {
                window: UsageWindowKind::Other {
                    id: "rate_limit_reset_credits".into(),
                    label: "Rate limit reset credits".into(),
                },
                resets_at: None,
                measurements: vec![UsageMeasurement {
                    name: "available_count".into(),
                    used: available_count as f64,
                    limit: None,
                    unit: MeasurementUnit::Credits,
                }],
            });
        }

        windows.sort_by(|left, right| window_sort_key(left).cmp(&window_sort_key(right)));
        let account_label = self
            .workspace
            .label
            .filter(|label| !label.trim().is_empty())
            .or(Some(self.workspace.id));

        Ok(SubscriptionUsage {
            provider: ProviderId::new("chatgpt"),
            account_label,
            plan: self.response.plan_type,
            subscription_expires_at: None,
            observed_at: self.observed_at,
            windows,
        })
    }
}

/// Entries in a vendor list are informational: one malformed element must not
/// take the whole usage response down with it, so each is parsed on its own
/// and unparseable ones are dropped.
fn deserialize_lenient_list<'de, D, T>(deserializer: D) -> Result<Vec<T>, D::Error>
where
    D: Deserializer<'de>,
    T: serde::de::DeserializeOwned,
{
    Ok(Option::<Vec<serde_json::Value>>::deserialize(deserializer)?
        .unwrap_or_default()
        .into_iter()
        .filter_map(|value| serde_json::from_value(value).ok())
        .collect())
}

fn append_rate_limit(
    windows: &mut Vec<UsageWindow>,
    scope: &str,
    rate_limit: Option<ChatGptRateLimit>,
) -> ProviderResult<()> {
    let Some(rate_limit) = rate_limit else {
        return Ok(());
    };
    let status_measurements = rate_limit_status_measurements(&rate_limit);
    let vendor_windows = [rate_limit.primary_window, rate_limit.secondary_window]
        .into_iter()
        .flatten()
        .collect::<Vec<_>>();
    if vendor_windows.is_empty() && !status_measurements.is_empty() {
        windows.push(UsageWindow {
            window: UsageWindowKind::Other {
                id: format!("{scope}_status"),
                label: format!("{scope} status"),
            },
            resets_at: None,
            measurements: status_measurements,
        });
        return Ok(());
    }
    for vendor_window in vendor_windows {
        if !vendor_window.used_percent.is_finite() || vendor_window.used_percent < 0.0 {
            return Err(ProviderError::ProtocolIncompatible {
                message: format!("{scope} window has invalid used_percent"),
            });
        }
        let resets_at = vendor_window
            .reset_at
            .map(|timestamp| {
                Utc.timestamp_opt(timestamp, 0).single().ok_or_else(|| {
                    ProviderError::ProtocolIncompatible {
                        message: format!("{scope} window has invalid reset_at"),
                    }
                })
            })
            .transpose()?;
        windows.push(UsageWindow {
            window: classify_window(vendor_window.limit_window_seconds),
            resets_at,
            measurements: std::iter::once(UsageMeasurement {
                name: format!("{scope}_usage"),
                used: vendor_window.used_percent,
                limit: Some(100.0),
                unit: MeasurementUnit::Percent,
            })
            .chain(status_measurements.iter().cloned())
            .collect(),
        });
    }
    Ok(())
}

fn rate_limit_status_measurements(rate_limit: &ChatGptRateLimit) -> Vec<UsageMeasurement> {
    let mut measurements = Vec::new();
    append_boolean_measurement(&mut measurements, "allowed", rate_limit.allowed);
    append_boolean_measurement(&mut measurements, "limit_reached", rate_limit.limit_reached);
    measurements
}

fn append_boolean_measurement(
    measurements: &mut Vec<UsageMeasurement>,
    name: &str,
    value: Option<bool>,
) {
    if let Some(value) = value {
        measurements.push(UsageMeasurement {
            name: name.into(),
            used: f64::from(u8::from(value)),
            limit: Some(1.0),
            unit: MeasurementUnit::Other {
                id: "boolean".into(),
                label: "Boolean (0=false, 1=true)".into(),
            },
        });
    }
}

fn deserialize_optional_number<'de, D>(deserializer: D) -> Result<Option<f64>, D::Error>
where
    D: Deserializer<'de>,
{
    let value = Option::<serde_json::Value>::deserialize(deserializer)?;
    match value {
        None | Some(serde_json::Value::Null) => Ok(None),
        Some(serde_json::Value::Number(number)) => number
            .as_f64()
            .ok_or_else(|| serde::de::Error::custom("number is outside f64 range"))
            .map(Some),
        Some(serde_json::Value::String(text)) => text
            .parse::<f64>()
            .map(Some)
            .map_err(serde::de::Error::custom),
        Some(_) => Err(serde::de::Error::custom(
            "expected a number, numeric string, or null",
        )),
    }
}

fn classify_window(seconds: u64) -> UsageWindowKind {
    match seconds {
        FIVE_HOURS_SECONDS => UsageWindowKind::FiveHours,
        WEEK_SECONDS => UsageWindowKind::Weekly,
        _ => UsageWindowKind::Other {
            id: format!("{seconds}s"),
            label: format!("{seconds} seconds"),
        },
    }
}

fn window_sort_key(window: &UsageWindow) -> (u64, &str) {
    let seconds = match &window.window {
        UsageWindowKind::FiveHours => FIVE_HOURS_SECONDS,
        UsageWindowKind::Weekly => WEEK_SECONDS,
        UsageWindowKind::Monthly => 30 * 24 * 60 * 60,
        UsageWindowKind::Other { id, .. } => id
            .strip_suffix('s')
            .and_then(|value| value.parse().ok())
            .unwrap_or(u64::MAX),
    };
    let name = window
        .measurements
        .first()
        .map(|measurement| measurement.name.as_str())
        .unwrap_or("");
    (seconds, name)
}
