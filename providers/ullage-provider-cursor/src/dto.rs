use std::collections::BTreeMap;

use chrono::{DateTime, Utc};
use serde::{Deserialize, Deserializer, Serialize, de::DeserializeOwned};
use serde_json::Value;
use ullage_core::{
    MeasurementUnit, ProviderId, ProviderResult, SubscriptionUsage, UsageMeasurement, UsageWindow,
    UsageWindowKind,
};

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct CursorUsage {
    pub account_label: Option<String>,
    pub current_period: CurrentPeriodUsage,
    pub plan: Option<PlanInfoResponse>,
    pub credit_grants: Option<CreditGrantsBalance>,
    pub hard_limit: Option<HardLimit>,
    pub observed_at: DateTime<Utc>,
}

#[derive(Clone, Debug, Default, PartialEq, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct CurrentPeriodUsage {
    pub billing_cycle_start: Option<i64>,
    pub billing_cycle_end: Option<i64>,
    pub plan_usage: Option<PlanUsage>,
    pub spend_limit_usage: Option<SpendLimitUsage>,
    #[serde(default)]
    pub auto_bucket_models: Vec<String>,
    #[serde(flatten)]
    pub extra: BTreeMap<String, Value>,
    /// Wire names of fields that were present but malformed; each one was
    /// dropped to `None` instead of failing the whole response. The query
    /// path reports them as partial failures.
    #[serde(skip)]
    pub malformed_fields: Vec<String>,
}

/// Each field is decoded on its own so one malformed value drops to `None`
/// rather than rejecting the entire `currentPeriod` response.
impl<'de> Deserialize<'de> for CurrentPeriodUsage {
    fn deserialize<D>(deserializer: D) -> Result<Self, D::Error>
    where
        D: Deserializer<'de>,
    {
        #[derive(Deserialize)]
        #[serde(rename_all = "camelCase")]
        struct Raw {
            #[serde(default)]
            billing_cycle_start: Option<Value>,
            #[serde(default)]
            billing_cycle_end: Option<Value>,
            #[serde(default)]
            plan_usage: Option<Value>,
            #[serde(default)]
            spend_limit_usage: Option<Value>,
            #[serde(default)]
            auto_bucket_models: Option<Value>,
            #[serde(flatten)]
            extra: BTreeMap<String, Value>,
        }
        let raw = Raw::deserialize(deserializer)?;
        let mut malformed_fields = Vec::new();
        Ok(Self {
            billing_cycle_start: tolerant_millis(
                raw.billing_cycle_start,
                "billingCycleStart",
                &mut malformed_fields,
            ),
            billing_cycle_end: tolerant_millis(
                raw.billing_cycle_end,
                "billingCycleEnd",
                &mut malformed_fields,
            ),
            plan_usage: tolerant_field(raw.plan_usage, "planUsage", &mut malformed_fields),
            spend_limit_usage: tolerant_field(
                raw.spend_limit_usage,
                "spendLimitUsage",
                &mut malformed_fields,
            ),
            auto_bucket_models: tolerant_field(
                raw.auto_bucket_models,
                "autoBucketModels",
                &mut malformed_fields,
            )
            .unwrap_or_default(),
            extra: raw.extra,
            malformed_fields,
        })
    }
}

fn tolerant_millis(
    value: Option<Value>,
    field: &str,
    malformed_fields: &mut Vec<String>,
) -> Option<i64> {
    match value {
        None | Some(Value::Null) => None,
        Some(value) => {
            let parsed = match &value {
                Value::String(text) => text.parse().ok(),
                Value::Number(number) => number.as_i64(),
                _ => None,
            };
            match parsed {
                Some(millis) => Some(millis),
                None => {
                    malformed_fields.push(field.into());
                    None
                }
            }
        }
    }
}

fn tolerant_field<T: DeserializeOwned>(
    value: Option<Value>,
    field: &str,
    malformed_fields: &mut Vec<String>,
) -> Option<T> {
    match value {
        None | Some(Value::Null) => None,
        Some(value) => match serde_json::from_value(value) {
            Ok(parsed) => Some(parsed),
            Err(_) => {
                malformed_fields.push(field.into());
                None
            }
        },
    }
}

#[derive(Clone, Debug, Default, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct PlanUsage {
    pub total_spend: Option<f64>,
    pub included_spend: Option<f64>,
    pub bonus_spend: Option<f64>,
    pub remaining: Option<f64>,
    pub limit: Option<f64>,
    pub auto_percent_used: Option<f64>,
    pub api_percent_used: Option<f64>,
    pub total_percent_used: Option<f64>,
    #[serde(flatten)]
    pub categories: BTreeMap<String, Value>,
}

#[derive(Clone, Debug, Default, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct SpendLimitUsage {
    pub total_spend: Option<f64>,
    pub limit: Option<f64>,
    pub limit_type: Option<String>,
    #[serde(flatten)]
    pub extra: BTreeMap<String, Value>,
}

#[derive(Clone, Debug, Default, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct PlanInfoResponse {
    pub plan_info: Option<PlanInfo>,
    #[serde(flatten)]
    pub extra: BTreeMap<String, Value>,
}

#[derive(Clone, Debug, Default, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct PlanInfo {
    pub plan_name: Option<String>,
    pub included_amount_cents: Option<f64>,
    pub price: Option<String>,
    #[serde(default, deserialize_with = "optional_millis")]
    pub billing_cycle_end: Option<i64>,
    pub plan_owner: Option<String>,
    #[serde(flatten)]
    pub extra: BTreeMap<String, Value>,
}

#[derive(Clone, Debug, Default, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct CreditGrantsBalance {
    pub balance_cents: Option<f64>,
    pub granted_cents: Option<f64>,
    pub used_cents: Option<f64>,
    #[serde(flatten)]
    pub extra: BTreeMap<String, Value>,
}

#[derive(Clone, Debug, Default, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct HardLimit {
    pub hard_limit: Option<f64>,
    #[serde(default)]
    pub no_usage_based_allowed: bool,
    pub hard_limit_per_user: Option<f64>,
    #[serde(flatten)]
    pub extra: BTreeMap<String, Value>,
}

pub fn normalize(usage: CursorUsage) -> ProviderResult<SubscriptionUsage> {
    let plan_name = usage
        .plan
        .as_ref()
        .and_then(|response| response.plan_info.as_ref())
        .and_then(|plan| plan.plan_name.clone());
    let reset_millis = usage.current_period.billing_cycle_end.or_else(|| {
        usage
            .plan
            .as_ref()
            .and_then(|response| response.plan_info.as_ref())
            .and_then(|plan| plan.billing_cycle_end)
    });
    let resets_at =
        match reset_millis {
            Some(value) => Some(DateTime::<Utc>::from_timestamp_millis(value).ok_or_else(
                || ullage_core::ProviderError::ProtocolIncompatible {
                    message: "Cursor billingCycleEnd is outside the supported UTC range".into(),
                },
            )?),
            None => None,
        };

    let mut measurements = Vec::new();
    if let Some(plan) = &usage.current_period.plan_usage {
        push_currency(
            &mut measurements,
            "total_spend",
            plan.total_spend,
            plan.limit,
        );
        push_currency(
            &mut measurements,
            "included_spend",
            plan.included_spend,
            plan.limit,
        );
        push_currency(&mut measurements, "bonus_spend", plan.bonus_spend, None);
        push_percent(&mut measurements, "auto", plan.auto_percent_used);
        push_percent(&mut measurements, "api", plan.api_percent_used);
        push_percent(&mut measurements, "total", plan.total_percent_used);

        for (field, value) in &plan.categories {
            if let Some(category) = field.strip_suffix("PercentUsed") {
                push_percent(&mut measurements, category, value.as_f64());
            }
        }
    }

    if let Some(grants) = &usage.credit_grants {
        push_currency(
            &mut measurements,
            "bonus_balance",
            grants.balance_cents,
            grants.granted_cents,
        );
    }

    if let Some(spend) = &usage.current_period.spend_limit_usage {
        push_currency(
            &mut measurements,
            "on_demand_spend",
            spend.total_spend,
            spend.limit,
        );
    }

    if let Some(limit) = &usage.hard_limit {
        measurements.push(UsageMeasurement {
            name: "on_demand_enabled".into(),
            used: if limit.no_usage_based_allowed {
                0.0
            } else {
                1.0
            },
            limit: Some(1.0),
            unit: MeasurementUnit::Other {
                id: "boolean".into(),
                label: "Enabled".into(),
            },
        });
        if let Some(hard_limit) = limit.hard_limit {
            measurements.push(UsageMeasurement {
                name: "on_demand_hard_limit".into(),
                used: 0.0,
                limit: Some(hard_limit),
                unit: MeasurementUnit::Currency { code: "USD".into() },
            });
        }
    }

    Ok(SubscriptionUsage {
        provider: ProviderId::new("cursor"),
        account_label: usage.account_label,
        plan: plan_name,
        subscription_expires_at: None,
        observed_at: usage.observed_at,
        windows: vec![UsageWindow {
            window: UsageWindowKind::Monthly,
            resets_at,
            measurements,
        }],
    })
}

fn push_currency(
    measurements: &mut Vec<UsageMeasurement>,
    name: &str,
    cents: Option<f64>,
    limit_cents: Option<f64>,
) {
    if let Some(cents) = cents {
        measurements.push(UsageMeasurement {
            name: name.into(),
            used: cents / 100.0,
            limit: limit_cents.map(|value| value / 100.0),
            unit: MeasurementUnit::Currency { code: "USD".into() },
        });
    }
}

fn push_percent(measurements: &mut Vec<UsageMeasurement>, name: &str, used: Option<f64>) {
    if let Some(used) = used {
        measurements.push(UsageMeasurement {
            name: name.into(),
            used,
            limit: Some(100.0),
            unit: MeasurementUnit::Percent,
        });
    }
}

fn optional_millis<'de, D>(deserializer: D) -> Result<Option<i64>, D::Error>
where
    D: Deserializer<'de>,
{
    let value = Option::<Value>::deserialize(deserializer)?;
    match value {
        None | Some(Value::Null) => Ok(None),
        Some(Value::String(value)) => value.parse().map(Some).map_err(serde::de::Error::custom),
        Some(Value::Number(value)) => value
            .as_i64()
            .ok_or_else(|| serde::de::Error::custom("millisecond timestamp is not an integer"))
            .map(Some),
        Some(_) => Err(serde::de::Error::custom(
            "millisecond timestamp must be a string or integer",
        )),
    }
}
