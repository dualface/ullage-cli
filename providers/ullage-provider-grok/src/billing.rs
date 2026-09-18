use chrono::{DateTime, TimeZone, Utc};
use serde::{Deserialize, Serialize};
use serde_json::{Map, Value};
use ullage_core::{
    MeasurementUnit, PartialFailure, ProviderError, ProviderId, ProviderResult, QueryOutcome,
    SubscriptionUsage, UsageMeasurement, UsageWindow, UsageWindowKind,
};

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum NormalizedTier {
    Free,
    SuperGrok,
    SuperGrokHeavy,
    Premium,
    PremiumPlus,
    Enterprise,
    Unknown,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct GrokTier {
    /// Exact vendor value. This is intentionally retained when normalization is unknown.
    pub raw: String,
    pub normalized: NormalizedTier,
}

impl GrokTier {
    pub fn new(raw: impl Into<String>) -> Self {
        let raw = raw.into();
        let key: String = raw
            .chars()
            .filter(|character| character.is_ascii_alphanumeric())
            .flat_map(char::to_lowercase)
            .collect();
        let normalized = match key.as_str() {
            "free" | "freetier" => NormalizedTier::Free,
            "supergrok" | "supergrokbasic" => NormalizedTier::SuperGrok,
            "supergrokheavy" | "heavy" => NormalizedTier::SuperGrokHeavy,
            "premium" => NormalizedTier::Premium,
            "premiumplus" => NormalizedTier::PremiumPlus,
            "enterprise" | "business" => NormalizedTier::Enterprise,
            _ => NormalizedTier::Unknown,
        };
        Self { raw, normalized }
    }
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct GrokPeriod {
    pub starts_at: Option<DateTime<Utc>>,
    pub ends_at: Option<DateTime<Utc>>,
    /// Window kind from `currentPeriod.type` when the vendor sends a recognized value.
    pub kind: Option<UsageWindowKind>,
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct GrokProductUsage {
    pub product: String,
    pub usage_percent: f64,
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct GrokPrepaid {
    pub remaining: f64,
    pub currency: Option<String>,
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct GrokOnDemand {
    pub enabled: bool,
    pub used: Option<f64>,
    pub limit: Option<f64>,
    pub currency: Option<String>,
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct GrokBillingUsage {
    pub account_label: Option<String>,
    pub tier: Option<GrokTier>,
    pub current_period: Option<GrokPeriod>,
    pub usage_percent: Option<f64>,
    /// True when `usage_percent` was computed from monthly used/limit rather
    /// than read from an explicit percent field.
    #[serde(default)]
    pub usage_percent_derived: bool,
    /// True when the credits/currentPeriod schema required a typed window, so
    /// a missing kind must fall back to Weekly even if the percent was derived.
    #[serde(default)]
    pub prefer_weekly_type_fallback: bool,
    /// Absolute monthly usage count from cli-chat-proxy `used.val`.
    pub monthly_used: Option<f64>,
    /// Monthly quota when the provider reports a positive limit.
    pub monthly_limit: Option<f64>,
    pub products: Vec<GrokProductUsage>,
    pub prepaid: Option<GrokPrepaid>,
    pub on_demand: Option<GrokOnDemand>,
    /// Exact vendor `topUpMethod` value. Not normalized and not shown as usage.
    #[serde(default)]
    pub top_up_method: Option<String>,
    /// True when `/v1/settings` reports `allow_access == false` or a non-empty `gate_message`.
    #[serde(default)]
    pub access_restricted: bool,
    pub observed_at: DateTime<Utc>,
}

#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub(crate) struct GrokSettings {
    pub subscription_tier_display: Option<String>,
    pub access_restricted: bool,
}

// Field alias tables shared between envelope selection and field parsing so
// the two passes can never drift apart.
const TIER_FIELDS: &[&str] = &["tier", "plan", "subscription_tier", "subscriptionTier"];
const CURRENT_PERIOD_FIELDS: &[&str] = &["currentPeriod", "current_period"];
const LEGACY_PERIOD_FIELDS: &[&str] = &["billing_period", "billingPeriod", "period"];
const USAGE_PERCENT_FIELDS: &[&str] = &[
    "usage_percent",
    "usagePercent",
    "used_percent",
    "usedPercent",
    "percent_used",
    "creditUsagePercent",
];
const MONTHLY_USED_FIELDS: &[&str] = &["used", "usedAmount", "used_amount"];
const MONTHLY_LIMIT_FIELDS: &[&str] = &["monthlyLimit", "monthly_limit"];
const ON_DEMAND_CAP_FIELDS: &[&str] = &["onDemandCap", "on_demand_cap"];
const ON_DEMAND_USED_FIELDS: &[&str] = &["onDemandUsed", "on_demand_used"];
const PRODUCTS_FIELDS: &[&str] = &[
    "products",
    "product_usage",
    "productUsage",
    "usage_by_product",
    "usageByProduct",
    "breakdown",
];
const PREPAID_FIELDS: &[&str] = &[
    "prepaid",
    "prepaid_credits",
    "prepaidCredits",
    "credits",
    "extra_usage_credits",
    "prepaidBalance",
];
/// Envelope-selector subset of [`PREPAID_FIELDS`]. `prepaidBalance` is parsed
/// only after an object is chosen; letting it claim an earlier envelope would
/// hide a later object that carries the percentage window.
const PREPAID_SELECTOR_FIELDS: &[&str] = &[
    "prepaid",
    "prepaid_credits",
    "prepaidCredits",
    "credits",
    "extra_usage_credits",
];
const ON_DEMAND_FIELDS: &[&str] = &[
    "on_demand",
    "onDemand",
    "pay_as_you_go",
    "payAsYouGo",
    "extra_usage",
];
const TOP_UP_METHOD_FIELDS: &[&str] = &["topUpMethod", "top_up_method"];
const FLAT_PERIOD_START_FIELDS: &[&str] = &[
    "billingPeriodStart",
    "billing_period_start",
    "period_start",
    "periodStart",
];
const FLAT_PERIOD_END_FIELDS: &[&str] = &[
    "billingPeriodEnd",
    "billing_period_end",
    "period_end",
    "periodEnd",
];

pub fn parse_billing(
    response: Value,
    account_label: Option<String>,
    observed_at: DateTime<Utc>,
) -> ProviderResult<QueryOutcome<GrokBillingUsage>> {
    let object = billing_object(&response).ok_or_else(|| ProviderError::ProtocolIncompatible {
        message: "Grok billing response contains no usable object".into(),
    })?;
    let mut failures = Vec::new();
    let mut recognized = false;

    let tier_values = fields(object, TIER_FIELDS);
    let tier_present = !tier_values.is_empty();
    recognized |= tier_present;
    let tier = tier_values.into_iter().find_map(parse_tier_value);
    if tier_present && tier.is_none() {
        failures.push(failure("tier"));
    }

    // Prefer camelCase `currentPeriod` (format=credits canonical) so a typed
    // period is not shadowed by an earlier snake_case alias that only has dates.
    let current_period_values = fields(object, CURRENT_PERIOD_FIELDS);
    let legacy_period_values = fields(object, LEGACY_PERIOD_FIELDS);
    let current_period_key_present = !current_period_values.is_empty();
    recognized |= current_period_key_present || !legacy_period_values.is_empty();
    let nested_from_current =
        first_with_failures(current_period_values, &mut failures, parse_period);
    let mut current_period = nested_from_current;
    if current_period.is_none() {
        current_period = first_with_failures(legacy_period_values, &mut failures, parse_period);
    }
    // Do not borrow period kind from sibling aliases. If the preferred
    // currentPeriod object is present but untyped/unknown, keep kind = None so
    // PartialFailure + Weekly fallback remain authoritative.
    // Dates may still be filled from flat aliases and sibling objects below.
    // currentPeriod.type is authoritative even when nested timestamps are
    // omitted or partial; fill missing dates from flat aliases, then from
    // sibling currentPeriod / current_period objects.
    if let Some(period) = current_period.as_mut() {
        if period.starts_at.is_none() || period.ends_at.is_none() {
            if let Some(flat) = parse_flat_period(object, &mut failures) {
                if period.starts_at.is_none() {
                    period.starts_at = flat.starts_at;
                }
                if period.ends_at.is_none() {
                    period.ends_at = flat.ends_at;
                }
            }
        }
        if period.starts_at.is_none() || period.ends_at.is_none() {
            for value in fields(object, CURRENT_PERIOD_FIELDS) {
                let mut local_failures = Vec::new();
                let sibling = parse_period(value, &mut local_failures);
                if let Some(sibling) = sibling {
                    if period.starts_at.is_none() {
                        period.starts_at = sibling.starts_at;
                    }
                    if period.ends_at.is_none() {
                        period.ends_at = sibling.ends_at;
                    }
                }
                if period.starts_at.is_none() {
                    failures.extend(
                        local_failures
                            .iter()
                            .filter(|item| item.scope.contains("starts_at"))
                            .cloned(),
                    );
                }
                if period.ends_at.is_none() {
                    failures.extend(
                        local_failures
                            .iter()
                            .filter(|item| item.scope.contains("ends_at"))
                            .cloned(),
                    );
                }
                if period.starts_at.is_some() && period.ends_at.is_some() {
                    break;
                }
            }
        }
    } else if let Some(period) = parse_flat_period(object, &mut failures) {
        recognized = true;
        current_period = Some(period);
    }

    let usage_values = fields(object, USAGE_PERCENT_FIELDS);
    let usage_present = !usage_values.is_empty();
    let credit_usage_field_present = object.contains_key("creditUsagePercent");
    let product_usage_field_present = object.contains_key("productUsage");
    recognized |= usage_present;
    let mut usage_percent = usage_values.into_iter().find_map(non_negative_number);
    if usage_present && usage_percent.is_none() {
        failures.push(failure("usage_percent"));
    }

    let monthly_used_values = fields(object, MONTHLY_USED_FIELDS);
    let monthly_limit_values = fields(object, MONTHLY_LIMIT_FIELDS);
    let on_demand_cap_values = fields(object, ON_DEMAND_CAP_FIELDS);
    recognized |= !monthly_used_values.is_empty()
        || !monthly_limit_values.is_empty()
        || !on_demand_cap_values.is_empty();
    let monthly_used_present = !monthly_used_values.is_empty();
    let monthly_used = monthly_used_values
        .into_iter()
        .find_map(non_negative_number);
    if monthly_used_present && monthly_used.is_none() {
        failures.push(failure("used"));
    }
    let monthly_limit_present = !monthly_limit_values.is_empty();
    let raw_monthly_limit = monthly_limit_values
        .into_iter()
        .find_map(non_negative_number);
    if monthly_limit_present && raw_monthly_limit.is_none() {
        failures.push(failure("monthly_limit"));
    }
    let monthly_limit = raw_monthly_limit.filter(|limit| *limit > 0.0);
    let mut usage_percent_derived = false;
    if usage_percent.is_none() {
        if let (Some(used), Some(limit)) = (monthly_used, monthly_limit) {
            usage_percent = Some((used / limit) * 100.0);
            usage_percent_derived = true;
        }
    }

    let products_values = fields(object, PRODUCTS_FIELDS);
    recognized |= !products_values.is_empty();
    let (products, products_valid) = first_products(products_values, &mut failures);
    // Credits envelope without a percent pool is Partial, not Complete empty windows.
    let credits_schema =
        current_period_key_present || credit_usage_field_present || product_usage_field_present;
    let kind_missing = current_period.as_ref().is_none_or(|p| p.kind.is_none());
    let mut will_emit_percent_window = usage_percent.is_some() || !products.is_empty();
    if credits_schema && !will_emit_percent_window {
        if usage_present {
            // A percent field was present but unparseable: keep the failure
            // recorded above instead of inferring zero usage.
            push_unique_failure(&mut failures, "usage_percent");
        } else {
            // The credits response is protobuf JSON, which omits zero-valued
            // scalar fields: a parseable envelope without any percent key
            // means 0% used this period, not an incompatible response.
            // Wrapper messages such as `{"val":0}` are still serialized,
            // which is why onDemandCap appears while creditUsagePercent does
            // not.
            usage_percent = Some(0.0);
            will_emit_percent_window = true;
        }
    }
    if will_emit_percent_window && kind_missing && credits_schema {
        push_unique_failure(&mut failures, "current_period.type");
    }

    let prepaid_values = fields(object, PREPAID_FIELDS);
    recognized |= !prepaid_values.is_empty();
    let prepaid = first_with_failures(prepaid_values, &mut failures, parse_prepaid);

    let on_demand_values = fields(object, ON_DEMAND_FIELDS);
    recognized |= !on_demand_values.is_empty();
    let mut on_demand = first_with_failures(on_demand_values, &mut failures, parse_on_demand);
    let on_demand_cap_present = !on_demand_cap_values.is_empty();
    let raw_on_demand_cap = on_demand_cap_values
        .into_iter()
        .find_map(non_negative_number);
    if on_demand_cap_present && raw_on_demand_cap.is_none() {
        failures.push(failure("on_demand_cap"));
    }
    let on_demand_used_values = fields(object, ON_DEMAND_USED_FIELDS);
    recognized |= !on_demand_used_values.is_empty();
    let on_demand_used_present = !on_demand_used_values.is_empty();
    let raw_on_demand_used = on_demand_used_values
        .into_iter()
        .find_map(non_negative_number);
    if on_demand_used_present && raw_on_demand_used.is_none() {
        failures.push(failure("on_demand_used"));
    }
    if on_demand.is_none() {
        if let Some(cap) = raw_on_demand_cap.filter(|cap| *cap > 0.0) {
            on_demand = Some(GrokOnDemand {
                enabled: true,
                used: raw_on_demand_used,
                limit: Some(cap),
                currency: None,
            });
        }
    }

    let top_up_method_values = fields(object, TOP_UP_METHOD_FIELDS);
    recognized |= !top_up_method_values.is_empty();
    let top_up_method_present = !top_up_method_values.is_empty();
    let top_up_method = top_up_method_values.into_iter().find_map(raw_string_value);
    if top_up_method_present && top_up_method.is_none() {
        failures.push(failure("top_up_method"));
    }

    if !recognized {
        return Err(ProviderError::ProtocolIncompatible {
            message: "Grok billing response contains no recognized fields".into(),
        });
    }
    let usable = tier.is_some()
        || current_period.is_some()
        || usage_percent.is_some()
        || monthly_used.is_some()
        || products_valid
        || prepaid.is_some()
        || on_demand.is_some();
    if !usable {
        return Err(ProviderError::ProtocolIncompatible {
            message: "Grok billing response contains no usable fields".into(),
        });
    }

    let data = GrokBillingUsage {
        account_label,
        tier,
        current_period,
        usage_percent,
        usage_percent_derived,
        prefer_weekly_type_fallback: credits_schema,
        monthly_used,
        monthly_limit,
        products,
        prepaid,
        on_demand,
        top_up_method,
        access_restricted: false,
        observed_at,
    };
    if failures.is_empty() {
        Ok(QueryOutcome::Complete { data })
    } else {
        Ok(QueryOutcome::Partial { data, failures })
    }
}

pub fn normalize(usage: GrokBillingUsage) -> ProviderResult<SubscriptionUsage> {
    let resets_at = usage
        .current_period
        .as_ref()
        .and_then(|period| period.ends_at);
    let percent_window_kind = usage
        .current_period
        .as_ref()
        .and_then(|period| period.kind.clone())
        .unwrap_or({
            // Credits/currentPeriod schema missing a type must stay Weekly.
            // Only the legacy flat monthly used/limit path inherits Monthly.
            if usage.prefer_weekly_type_fallback {
                UsageWindowKind::Weekly
            } else if usage.usage_percent_derived {
                UsageWindowKind::Monthly
            } else {
                UsageWindowKind::Weekly
            }
        });
    let mut percent_measurements = Vec::new();
    if let Some(used) = usage.usage_percent {
        percent_measurements.push(UsageMeasurement {
            // The pool name tracks the resolved window kind so a derived
            // monthly percent is not labeled weekly.
            name: match percent_window_kind {
                UsageWindowKind::Monthly => "monthly_pool",
                _ => "weekly_pool",
            }
            .into(),
            used,
            limit: Some(100.0),
            unit: MeasurementUnit::Percent,
        });
    }
    percent_measurements.extend(usage.products.into_iter().map(|product| UsageMeasurement {
        name: format!("product:{}", product.product),
        used: product.usage_percent,
        limit: Some(100.0),
        unit: MeasurementUnit::Percent,
    }));
    let mut windows = Vec::new();
    // Prefer the percent window whenever it exists. The previous monthly-first
    // if/else silently dropped usage_percent measurements whenever monthly_used
    // was present.
    if !percent_measurements.is_empty() {
        windows.push(UsageWindow {
            window: percent_window_kind,
            resets_at,
            measurements: percent_measurements,
        });
    } else if let Some(used) = usage.monthly_used {
        windows.push(UsageWindow {
            window: UsageWindowKind::Monthly,
            resets_at,
            measurements: vec![UsageMeasurement {
                name: "total".into(),
                used,
                limit: usage.monthly_limit,
                unit: MeasurementUnit::Credits,
            }],
        });
    }

    if let Some(prepaid) = usage.prepaid.filter(|prepaid| prepaid.remaining > 0.0) {
        windows.push(UsageWindow {
            window: UsageWindowKind::Other {
                id: "prepaid".into(),
                label: "Extra Usage Credits".into(),
            },
            resets_at: None,
            measurements: vec![UsageMeasurement {
                name: "remaining".into(),
                used: prepaid.remaining,
                limit: None,
                unit: money_unit(prepaid.currency),
            }],
        });
    }

    if let Some(on_demand) = usage.on_demand {
        let mut measurements = vec![UsageMeasurement {
            name: "enabled".into(),
            used: if on_demand.enabled { 1.0 } else { 0.0 },
            limit: Some(1.0),
            unit: MeasurementUnit::Other {
                id: "boolean".into(),
                label: "Enabled".into(),
            },
        }];
        if let Some(used) = on_demand.used {
            measurements.push(UsageMeasurement {
                name: "spent".into(),
                used,
                limit: on_demand.limit,
                unit: money_unit(on_demand.currency),
            });
        } else if let Some(limit) = on_demand.limit {
            measurements.push(UsageMeasurement {
                name: "limit".into(),
                used: 0.0,
                limit: Some(limit),
                unit: money_unit(on_demand.currency),
            });
        }
        windows.push(UsageWindow {
            window: UsageWindowKind::Other {
                id: "on_demand".into(),
                label: "On-demand usage".into(),
            },
            resets_at: None,
            measurements,
        });
    }

    if usage.access_restricted {
        windows.push(UsageWindow {
            window: UsageWindowKind::Other {
                id: "access".into(),
                label: "Access".into(),
            },
            resets_at: None,
            measurements: vec![
                boolean_measurement("allowed", false),
                boolean_measurement("limit_reached", true),
            ],
        });
    }

    Ok(SubscriptionUsage {
        provider: ProviderId::new("grok"),
        account_label: usage.account_label,
        plan: usage.tier.map(|tier| tier.raw),
        // A billing period end is a usage reset, not a subscription expiration.
        subscription_expires_at: None,
        observed_at: usage.observed_at,
        windows,
    })
}

/// Extracts the display-tier and access-gate fields from `/v1/settings`.
///
/// Unknown keys are ignored. The raw JSON is not retained.
pub(crate) fn parse_settings(response: &Value) -> (GrokSettings, Vec<PartialFailure>) {
    let mut failures = Vec::new();
    let Some(object) = response.as_object() else {
        failures.push(failure("settings"));
        return (GrokSettings::default(), failures);
    };

    let display_values = fields(object, &["subscription_tier_display"]);
    let subscription_tier_display = display_values.into_iter().find_map(raw_string_value);
    if subscription_tier_display.is_none() {
        failures.push(failure("subscription_tier_display"));
    }

    let allow_access = object.get("allow_access").and_then(boolean);
    let gated = object
        .get("gate_message")
        .and_then(raw_string_value)
        .is_some();
    let access_restricted = allow_access == Some(false) || gated;

    (
        GrokSettings {
            subscription_tier_display,
            access_restricted,
        },
        failures,
    )
}

/// Settings display names win over billing `tier` / `plan` / `subscription_tier`.
pub(crate) fn apply_settings(usage: &mut GrokBillingUsage, settings: GrokSettings) {
    if let Some(display) = settings.subscription_tier_display {
        usage.tier = Some(GrokTier::new(display));
    }
    usage.access_restricted = settings.access_restricted;
}

pub(crate) fn with_failures<T>(
    outcome: QueryOutcome<T>,
    extra: Vec<PartialFailure>,
) -> QueryOutcome<T> {
    if extra.is_empty() {
        return outcome;
    }
    match outcome {
        QueryOutcome::Complete { data } => QueryOutcome::Partial {
            data,
            failures: extra,
        },
        QueryOutcome::Partial { data, mut failures } => {
            failures.extend(extra);
            QueryOutcome::Partial { data, failures }
        }
    }
}

fn outcome_data_mut<T>(outcome: &mut QueryOutcome<T>) -> &mut T {
    match outcome {
        QueryOutcome::Complete { data } | QueryOutcome::Partial { data, .. } => data,
    }
}

pub(crate) fn apply_settings_to_outcome(
    outcome: &mut QueryOutcome<GrokBillingUsage>,
    settings: GrokSettings,
) {
    apply_settings(outcome_data_mut(outcome), settings);
}

fn boolean_measurement(name: &str, value: bool) -> UsageMeasurement {
    UsageMeasurement {
        name: name.into(),
        used: if value { 1.0 } else { 0.0 },
        limit: Some(1.0),
        unit: MeasurementUnit::Other {
            id: "boolean".into(),
            label: "Boolean (0=false, 1=true)".into(),
        },
    }
}

fn billing_object(response: &Value) -> Option<&Map<String, Value>> {
    [
        response.pointer("/data/billing"),
        response.get("billing"),
        response.pointer("/data/usage"),
        response.get("usage"),
        response.get("data"),
        response.pointer("/data/config"),
        response.get("config"),
        Some(response),
    ]
    .into_iter()
    .flatten()
    .filter_map(Value::as_object)
    .find(|object| has_usable_billing_field(object))
}

fn has_usable_billing_field(object: &Map<String, Value>) -> bool {
    let tier = fields(object, TIER_FIELDS)
        .into_iter()
        .find_map(parse_tier_value)
        .is_some();
    let mut ignored_failures = Vec::new();
    let period = fields(object, CURRENT_PERIOD_FIELDS)
        .into_iter()
        .chain(fields(object, LEGACY_PERIOD_FIELDS))
        .find_map(|value| parse_period(value, &mut ignored_failures))
        .is_some()
        || parse_flat_period(object, &mut ignored_failures).is_some();
    let usage = fields(object, USAGE_PERCENT_FIELDS)
        .into_iter()
        .find_map(non_negative_number)
        .is_some();
    let products = fields(object, PRODUCTS_FIELDS)
        .into_iter()
        .any(|value| parse_products(value, &mut ignored_failures).1);
    let prepaid = fields(object, PREPAID_SELECTOR_FIELDS)
        .into_iter()
        .find_map(|value| parse_prepaid(value, &mut ignored_failures))
        .is_some();
    let on_demand = fields(object, ON_DEMAND_FIELDS)
        .into_iter()
        .find_map(|value| parse_on_demand(value, &mut ignored_failures))
        .is_some();
    let monthly = fields(object, MONTHLY_USED_FIELDS)
        .into_iter()
        .find_map(non_negative_number)
        .is_some()
        || fields(object, MONTHLY_LIMIT_FIELDS)
            .into_iter()
            .find_map(non_negative_number)
            .is_some();
    // prepaidBalance / onDemandCap / onDemandUsed are parsed after an object
    // is selected. They must not independently capture an earlier envelope and
    // hide a later object that has the percentage window.
    tier || period || usage || products || prepaid || on_demand || monthly
}

fn fields<'a>(object: &'a Map<String, Value>, names: &[&str]) -> Vec<&'a Value> {
    names.iter().filter_map(|name| object.get(*name)).collect()
}

fn parse_tier_value(value: &Value) -> Option<GrokTier> {
    raw_string_value(value)
        .or_else(|| {
            value.as_object().and_then(|item| {
                fields(item, &["tier", "name", "id"])
                    .into_iter()
                    .find_map(raw_string_value)
            })
        })
        .map(GrokTier::new)
}

fn first_with_failures<'a, T>(
    values: Vec<&'a Value>,
    failures: &mut Vec<PartialFailure>,
    mut parse: impl FnMut(&'a Value, &mut Vec<PartialFailure>) -> Option<T>,
) -> Option<T> {
    let mut first_failures = None;
    for value in values {
        let mut local_failures = Vec::new();
        if let Some(parsed) = parse(value, &mut local_failures) {
            failures.extend(local_failures);
            return Some(parsed);
        }
        first_failures.get_or_insert(local_failures);
    }
    failures.extend(first_failures.unwrap_or_default());
    None
}

fn first_products(
    values: Vec<&Value>,
    failures: &mut Vec<PartialFailure>,
) -> (Vec<GrokProductUsage>, bool) {
    let mut first_failures = None;
    for value in values {
        let mut local_failures = Vec::new();
        let (products, usable) = parse_products(value, &mut local_failures);
        if usable {
            failures.extend(local_failures);
            return (products, true);
        }
        first_failures.get_or_insert(local_failures);
    }
    failures.extend(first_failures.unwrap_or_default());
    (Vec::new(), false)
}

fn string_value(value: &Value) -> Option<String> {
    value
        .as_str()
        .map(str::trim)
        .filter(|value| !value.is_empty())
        .map(str::to_owned)
}

fn raw_string_value(value: &Value) -> Option<String> {
    value
        .as_str()
        .filter(|value| !value.trim().is_empty())
        .map(str::to_owned)
}

fn number(value: &Value) -> Option<f64> {
    value
        .as_f64()
        .or_else(|| value.as_str()?.trim().parse().ok())
        .or_else(|| {
            let object = value.as_object()?;
            fields(object, &["value", "val", "amount"])
                .into_iter()
                .find_map(number)
        })
        .filter(|value| value.is_finite())
}

fn non_negative_number(value: &Value) -> Option<f64> {
    number(value).filter(|value| *value >= 0.0)
}

fn boolean(value: &Value) -> Option<bool> {
    value.as_bool().or_else(
        || match value.as_str()?.trim().to_ascii_lowercase().as_str() {
            "true" | "enabled" | "yes" | "1" => Some(true),
            "false" | "disabled" | "no" | "0" => Some(false),
            _ => None,
        },
    )
}

fn timestamp(value: &Value) -> Option<DateTime<Utc>> {
    let raw = if let Some(text) = value.as_str() {
        if let Ok(date) = DateTime::parse_from_rfc3339(text.trim()) {
            return Some(date.with_timezone(&Utc));
        }
        text.trim().parse().ok()?
    } else {
        value.as_i64()?
    };
    // Epoch seconds stay below 1e10 until the year 2286, while epoch
    // milliseconds have been above it since 1970-04. Splitting on that
    // magnitude therefore assigns each plausible vendor timestamp the unit
    // that yields a sane date.
    if raw.unsigned_abs() >= 10_000_000_000 {
        Utc.timestamp_millis_opt(raw).single()
    } else {
        Utc.timestamp_opt(raw, 0).single()
    }
}

fn parse_flat_period(
    object: &Map<String, Value>,
    failures: &mut Vec<PartialFailure>,
) -> Option<GrokPeriod> {
    let start_values = fields(object, FLAT_PERIOD_START_FIELDS);
    let end_values = fields(object, FLAT_PERIOD_END_FIELDS);
    if start_values.is_empty() && end_values.is_empty() {
        return None;
    }
    let start_present = !start_values.is_empty();
    let starts_at = start_values.into_iter().find_map(timestamp);
    if start_present && starts_at.is_none() {
        failures.push(failure("billingPeriodStart"));
    }
    let end_present = !end_values.is_empty();
    let ends_at = end_values.into_iter().find_map(timestamp);
    if end_present && ends_at.is_none() {
        failures.push(failure("billingPeriodEnd"));
    }
    if starts_at.is_none() && ends_at.is_none() {
        None
    } else {
        Some(GrokPeriod {
            starts_at,
            ends_at,
            kind: None,
        })
    }
}

fn parse_period_type(value: &Value) -> Option<UsageWindowKind> {
    let raw = string_value(value)?;
    match raw.as_str() {
        "USAGE_PERIOD_TYPE_WEEKLY" => Some(UsageWindowKind::Weekly),
        "USAGE_PERIOD_TYPE_MONTHLY" => Some(UsageWindowKind::Monthly),
        _ => None,
    }
}

fn parse_period(value: &Value, failures: &mut Vec<PartialFailure>) -> Option<GrokPeriod> {
    let Some(object) = value.as_object() else {
        failures.push(failure("current_period"));
        return None;
    };
    let type_values = fields(object, &["type", "period_type", "periodType"]);
    let type_present = !type_values.is_empty();
    let kind = type_values.into_iter().find_map(parse_period_type);
    if type_present && kind.is_none() {
        failures.push(failure("current_period.type"));
    }
    let start_values = fields(
        object,
        &["starts_at", "start_at", "startAt", "start", "period_start"],
    );
    let start_present = !start_values.is_empty();
    let starts_at = start_values.into_iter().find_map(timestamp);
    if start_present && starts_at.is_none() {
        failures.push(failure("current_period.starts_at"));
    }
    let end_values = fields(
        object,
        &[
            "ends_at",
            "end_at",
            "endAt",
            "end",
            "period_end",
            "reset_at",
        ],
    );
    let end_present = !end_values.is_empty();
    let ends_at = end_values.into_iter().find_map(timestamp);
    if end_present && ends_at.is_none() {
        failures.push(failure("current_period.ends_at"));
    }
    if starts_at.is_none() && ends_at.is_none() {
        if kind.is_some() {
            // Type alone is enough to classify the percent window; dates may
            // arrive via flat billingPeriod* fields and are merged later.
            return Some(GrokPeriod {
                starts_at: None,
                ends_at: None,
                kind,
            });
        }
        if !start_present && !end_present {
            failures.push(failure("current_period"));
        }
        None
    } else {
        Some(GrokPeriod {
            starts_at,
            ends_at,
            kind,
        })
    }
}

fn parse_products(
    value: &Value,
    failures: &mut Vec<PartialFailure>,
) -> (Vec<GrokProductUsage>, bool) {
    if let Some(object) = value.as_object() {
        let products: Vec<GrokProductUsage> = object
            .iter()
            .enumerate()
            .filter_map(
                |(index, (product, amount))| match non_negative_number(amount) {
                    Some(usage_percent) => Some(GrokProductUsage {
                        product: product.clone(),
                        usage_percent,
                    }),
                    None => {
                        failures.push(failure(format!("products[{index}]")));
                        None
                    }
                },
            )
            .collect();
        let usable = object.is_empty() || !products.is_empty();
        return (products, usable);
    }
    let Some(items) = value.as_array() else {
        failures.push(failure("products"));
        return (Vec::new(), false);
    };
    let products: Vec<GrokProductUsage> = items
        .iter()
        .enumerate()
        .filter_map(|(index, item)| {
            let Some(object) = item.as_object() else {
                failures.push(failure(format!("products[{index}]")));
                return None;
            };
            let product = fields(object, &["product", "name", "id", "category"])
                .into_iter()
                .find_map(string_value);
            let used_values = fields(
                object,
                &[
                    "usage_percent",
                    "usagePercent",
                    "percent",
                    "used_percent",
                    "value",
                ],
            );
            let used_present = !used_values.is_empty();
            let used = used_values.into_iter().find_map(non_negative_number);
            match (product, used, used_present) {
                (Some(product), Some(usage_percent), _) => Some(GrokProductUsage {
                    product,
                    usage_percent,
                }),
                // SuperGrok Heavy lists GrokChat / GrokImagine with a name and
                // no percent. Omit them; a missing measurement is not a
                // protocol failure.
                (Some(_), None, false) => None,
                _ => {
                    failures.push(failure(format!("products[{index}]")));
                    None
                }
            }
        })
        .collect();
    let usable = items.is_empty() || !products.is_empty();
    (products, usable)
}

fn parse_prepaid(value: &Value, failures: &mut Vec<PartialFailure>) -> Option<GrokPrepaid> {
    if let Some(remaining) = non_negative_number(value) {
        return Some(GrokPrepaid {
            remaining,
            currency: None,
        });
    }
    let Some(object) = value.as_object() else {
        failures.push(failure("prepaid"));
        return None;
    };
    let remaining = fields(
        object,
        &[
            "remaining",
            "balance",
            "available",
            "amount",
            "value",
            "val",
        ],
    )
    .into_iter()
    .find_map(non_negative_number);
    let Some(remaining) = remaining else {
        failures.push(failure("prepaid.remaining"));
        return None;
    };
    let currency_values = fields(object, &["currency", "currency_code", "currencyCode"]);
    let currency_present = !currency_values.is_empty();
    let currency = currency_values.into_iter().find_map(string_value);
    if currency_present && currency.is_none() {
        failures.push(failure("prepaid.currency"));
    }
    Some(GrokPrepaid {
        remaining,
        currency,
    })
}

fn parse_on_demand(value: &Value, failures: &mut Vec<PartialFailure>) -> Option<GrokOnDemand> {
    if let Some(enabled) = boolean(value) {
        return Some(GrokOnDemand {
            enabled,
            used: None,
            limit: None,
            currency: None,
        });
    }
    let Some(object) = value.as_object() else {
        failures.push(failure("on_demand"));
        return None;
    };
    let enabled = fields(object, &["enabled", "is_enabled", "isEnabled", "active"])
        .into_iter()
        .find_map(boolean);
    let Some(enabled) = enabled else {
        failures.push(failure("on_demand.enabled"));
        return None;
    };
    let used_values = fields(
        object,
        &["used", "spent", "spend", "amount_used", "amountUsed"],
    );
    let used_present = !used_values.is_empty();
    let used = used_values.into_iter().find_map(non_negative_number);
    if used_present && used.is_none() {
        failures.push(failure("on_demand.used"));
    }
    let limit_values = fields(object, &["limit", "spending_limit", "spendingLimit", "cap"]);
    let limit_present = !limit_values.is_empty();
    let limit = limit_values.into_iter().find_map(non_negative_number);
    if limit_present && limit.is_none() {
        failures.push(failure("on_demand.limit"));
    }
    let currency_values = fields(object, &["currency", "currency_code", "currencyCode"]);
    let currency_present = !currency_values.is_empty();
    let currency = currency_values.into_iter().find_map(string_value);
    if currency_present && currency.is_none() {
        failures.push(failure("on_demand.currency"));
    }
    Some(GrokOnDemand {
        enabled,
        used,
        limit,
        currency,
    })
}

fn money_unit(currency: Option<String>) -> MeasurementUnit {
    currency.map_or(MeasurementUnit::Credits, |code| MeasurementUnit::Currency {
        code,
    })
}

fn failure(scope: impl Into<String>) -> PartialFailure {
    PartialFailure::from_error(
        scope,
        &ProviderError::ProtocolIncompatible {
            message: String::new(),
        },
    )
}

fn push_unique_failure(failures: &mut Vec<PartialFailure>, scope: &str) {
    if !failures.iter().any(|item| item.scope == scope) {
        failures.push(failure(scope));
    }
}
