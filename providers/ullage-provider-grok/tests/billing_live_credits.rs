use async_trait::async_trait;
use chrono::{TimeZone, Utc};
use serde_json::{Value, json};
use ullage_core::{Provider, QueryOutcome, UsageWindowKind};
use ullage_provider_grok::{
    BrowserAuthorization, DeviceAuthorization, GrokApiError, GrokProvider, GrokTransport,
    OAuthPoll, OAuthToken, parse_billing,
};

struct UnusedTransport;

fn unused<T>() -> Result<T, GrokApiError> {
    Err(GrokApiError::Network("unused".into()))
}

#[async_trait]
impl GrokTransport for UnusedTransport {
    fn browser_redirect_uri(&self) -> &str {
        "http://127.0.0.1/callback"
    }

    async fn start_device_authorization(&self) -> Result<DeviceAuthorization, GrokApiError> {
        unused()
    }

    async fn poll_device_authorization(&self, _: &str) -> Result<OAuthPoll, GrokApiError> {
        unused()
    }

    async fn start_browser_authorization(
        &self,
        _: &str,
    ) -> Result<BrowserAuthorization, GrokApiError> {
        unused()
    }

    async fn complete_browser_authorization(
        &self,
        _: &str,
        _: &str,
        _: &str,
    ) -> Result<OAuthToken, GrokApiError> {
        unused()
    }

    async fn refresh(&self, _: &str) -> Result<OAuthToken, GrokApiError> {
        unused()
    }

    async fn revoke(&self, _: &OAuthToken) -> Result<(), GrokApiError> {
        unused()
    }

    async fn fetch_billing(&self, _: &str) -> Result<Value, GrokApiError> {
        unused()
    }

    async fn fetch_settings(&self, _: &str) -> Result<Value, GrokApiError> {
        unused()
    }
}

/// Live `GET /v1/billing?format=credits` shape recorded 2026-09-04 (keys and
/// JSON types only). Compared with `parses_format_credits_schema_and_keeps_percent_window`,
/// `creditUsagePercent` and `productUsage` are absent; the remaining credits
/// envelope is unchanged. Numbers are sanitized; structure matches the capture.
#[test]
fn live_format_credits_without_percent_fields_is_partial_not_empty_complete() {
    let observed_at = Utc.with_ymd_and_hms(2026, 9, 4, 7, 0, 0).unwrap();
    let outcome = parse_billing(
        json!({"config":{
          "currentPeriod":{"type":"USAGE_PERIOD_TYPE_WEEKLY",
                           "start":"2026-08-28T01:18:04.090314+00:00",
                           "end":"2026-09-04T01:18:04.090314+00:00"},
          "onDemandCap":{"val":0},
          "onDemandUsed":{"val":0},
          "prepaidBalance":{"val":0},
          "isUnifiedBillingUser":true,
          "topUpMethod":"TOP_UP_METHOD_SAVED_PAYMENT_METHOD",
          "billingPeriodStart":"2026-08-28T01:18:04.090314+00:00",
          "billingPeriodEnd":"2026-09-04T01:18:04.090314+00:00"
        }}),
        Some("account".into()),
        observed_at,
    )
    .unwrap();
    let QueryOutcome::Partial { data, failures } = outcome else {
        panic!("credits envelope without percent fields must be partial, not complete");
    };
    assert!(
        failures
            .iter()
            .any(|failure| failure.scope == "usage_percent"),
        "{failures:?}"
    );
    assert_eq!(data.usage_percent, None);
    assert!(data.products.is_empty());
    assert_eq!(
        data.current_period
            .as_ref()
            .and_then(|period| period.kind.clone()),
        Some(UsageWindowKind::Weekly)
    );
    assert_eq!(
        data.prepaid.as_ref().map(|prepaid| prepaid.remaining),
        Some(0.0)
    );
    assert!(data.on_demand.is_none());

    let normalized = GrokProvider::new(UnusedTransport).normalize(data).unwrap();
    assert!(
        normalized.windows.is_empty(),
        "missing percent must not invent a usage window: {normalized:?}"
    );
    assert_eq!(normalized.plan, None);
}

/// Live SuperGrok Heavy `GET /v1/billing?format=credits` recorded 2026-09-16.
/// `GrokChat` and `GrokImagine` are named without `usagePercent`; only
/// `GrokBuild` carries a percent. Numbers are sanitized; structure matches
/// the capture.
#[test]
fn live_supergrok_heavy_name_only_products_are_complete() {
    let observed_at = Utc.with_ymd_and_hms(2026, 9, 16, 12, 53, 0).unwrap();
    let outcome = parse_billing(
        json!({"config":{
          "currentPeriod":{"type":"USAGE_PERIOD_TYPE_WEEKLY",
                           "start":"2026-09-11T01:18:04.090314+00:00",
                           "end":"2026-09-18T01:18:04.090314+00:00"},
          "creditUsagePercent":96.0,
          "productUsage":[
            {"product":"GrokBuild","usagePercent":96.0},
            {"product":"GrokChat"},
            {"product":"GrokImagine"}
          ],
          "onDemandCap":{"val":0},
          "onDemandUsed":{"val":0},
          "prepaidBalance":{"val":0},
          "isUnifiedBillingUser":true,
          "topUpMethod":"TOP_UP_METHOD_SAVED_PAYMENT_METHOD",
          "billingPeriodStart":"2026-09-11T01:18:04.090314+00:00",
          "billingPeriodEnd":"2026-09-18T01:18:04.090314+00:00"
        }}),
        Some("account".into()),
        observed_at,
    )
    .unwrap();
    let QueryOutcome::Complete { data } = outcome else {
        panic!("named products without a percent must not make credits partial: {outcome:?}");
    };
    assert_eq!(data.usage_percent, Some(96.0));
    assert_eq!(
        data.products
            .iter()
            .map(|product| (product.product.as_str(), product.usage_percent))
            .collect::<Vec<_>>(),
        vec![("GrokBuild", 96.0)]
    );

    let normalized = GrokProvider::new(UnusedTransport).normalize(data).unwrap();
    assert_eq!(normalized.windows.len(), 1, "{normalized:?}");
    assert_eq!(normalized.windows[0].window, UsageWindowKind::Weekly);
    assert!(
        normalized.windows[0]
            .measurements
            .iter()
            .any(|measurement| measurement.name == "product:GrokBuild")
    );
    assert!(
        !normalized.windows[0]
            .measurements
            .iter()
            .any(|measurement| measurement.name == "product:GrokChat"
                || measurement.name == "product:GrokImagine"),
        "{normalized:?}"
    );
}

#[test]
fn named_product_with_unusable_percent_is_still_partial() {
    let outcome = parse_billing(
        json!({
            "currentPeriod": { "type": "USAGE_PERIOD_TYPE_WEEKLY" },
            "creditUsagePercent": 10.0,
            "productUsage": [
                {"product": "GrokBuild", "usagePercent": 10.0},
                {"product": "GrokChat", "usagePercent": "not-a-number"}
            ]
        }),
        None,
        Utc::now(),
    )
    .unwrap();
    let QueryOutcome::Partial { data, failures } = outcome else {
        panic!("present but unusable product percent must stay partial: {outcome:?}");
    };
    assert_eq!(data.products.len(), 1);
    assert_eq!(data.products[0].product, "GrokBuild");
    assert!(
        failures
            .iter()
            .any(|failure| failure.scope == "products[1]"),
        "{failures:?}"
    );
}
