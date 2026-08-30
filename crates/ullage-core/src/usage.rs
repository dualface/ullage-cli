use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};

use crate::ProviderId;

#[derive(Clone, Debug, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum UsageWindowKind {
    FiveHours,
    Weekly,
    Monthly,
    Other { id: String, label: String },
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum MeasurementUnit {
    Requests,
    Tokens,
    Percent,
    Credits,
    Currency { code: String },
    Other { id: String, label: String },
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct UsageMeasurement {
    pub name: String,
    pub used: f64,
    pub limit: Option<f64>,
    pub unit: MeasurementUnit,
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct UsageWindow {
    pub window: UsageWindowKind,
    pub resets_at: Option<DateTime<Utc>>,
    pub measurements: Vec<UsageMeasurement>,
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct SubscriptionUsage {
    pub provider: ProviderId,
    pub account_label: Option<String>,
    pub plan: Option<String>,
    pub subscription_expires_at: Option<DateTime<Utc>>,
    pub observed_at: DateTime<Utc>,
    pub windows: Vec<UsageWindow>,
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn round_trips_dynamic_windows_and_units() {
        let usage = SubscriptionUsage {
            provider: ProviderId::new("test"),
            account_label: None,
            plan: Some("pro".into()),
            subscription_expires_at: None,
            observed_at: Utc::now(),
            windows: vec![
                UsageWindow {
                    window: UsageWindowKind::FiveHours,
                    resets_at: None,
                    measurements: vec![UsageMeasurement {
                        name: "tokens".into(),
                        used: 12.0,
                        limit: Some(100.0),
                        unit: MeasurementUnit::Tokens,
                    }],
                },
                UsageWindow {
                    window: UsageWindowKind::Other {
                        id: "rolling_30d".into(),
                        label: "Rolling 30 days".into(),
                    },
                    resets_at: None,
                    measurements: vec![UsageMeasurement {
                        name: "spend".into(),
                        used: 3.5,
                        limit: None,
                        unit: MeasurementUnit::Currency { code: "USD".into() },
                    }],
                },
            ],
        };

        let json = serde_json::to_string(&usage).unwrap();
        assert_eq!(
            serde_json::from_str::<SubscriptionUsage>(&json).unwrap(),
            usage
        );
    }
}
