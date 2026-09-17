//! Query-string parsing shared by the HTTP routes.

use percent_encoding::percent_decode_str;

use crate::metric::decode_metric_name;

#[derive(Clone, Debug)]
pub(crate) struct QueryParams {
    pub(crate) account: Option<String>,
    pub(crate) wait: bool,
    pub(crate) diagnose: bool,
    pub(crate) metric: Vec<String>,
    pub(crate) keys: Vec<String>,
}

impl QueryParams {
    pub(crate) fn parse(query: &str) -> Result<Self, ()> {
        let mut account = None;
        let mut wait = None;
        let mut diagnose = None;
        let mut metric = Vec::new();
        let mut keys = Vec::new();
        if query.is_empty() {
            return Ok(Self {
                account: None,
                wait: true,
                diagnose: false,
                metric,
                keys,
            });
        }
        for pair in query.split('&') {
            let (key, value) = pair.split_once('=').ok_or(())?;
            let key = decode_component(key)?;
            if key == "metric" {
                // Repeated names form the union of the metric filter. Decoding
                // keeps control characters so `MetricFilter` answers
                // `invalid_metric` instead of a generic parse error.
                metric.push(decode_metric_name(value)?);
                keys.push(key);
                continue;
            }
            let value = decode_component(value)?;
            match key.as_str() {
                "account" => {
                    if account.is_some() || value.is_empty() {
                        return Err(());
                    }
                    account = Some(value);
                }
                "wait" => {
                    if wait.is_some() {
                        return Err(());
                    }
                    wait = Some(match value.as_str() {
                        "true" => true,
                        "false" => false,
                        _ => return Err(()),
                    });
                }
                "diagnose" => {
                    if diagnose.is_some() {
                        return Err(());
                    }
                    diagnose = Some(match value.as_str() {
                        "1" => true,
                        "0" => false,
                        _ => return Err(()),
                    });
                }
                _ => return Err(()),
            }
            keys.push(key);
        }
        Ok(Self {
            account,
            wait: wait.unwrap_or(true),
            diagnose: diagnose.unwrap_or(false),
            metric,
            keys,
        })
    }
}

/// Percent-decodes one path segment or query component. `+` is left literal
/// on purpose: this API uses percent-encoding, not `application/x-www-form-
/// urlencoded`, so a plus is data rather than a space.
pub(crate) fn decode_component(value: &str) -> Result<String, ()> {
    let decoded = percent_decode_str(value).decode_utf8().map_err(|_| ())?;
    if decoded.contains('\0') {
        return Err(());
    }
    Ok(decoded.into_owned())
}
