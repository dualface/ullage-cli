//! Metric filter validation and usage-measurement filtering for HTTP responses.

use percent_encoding::percent_decode_str;
use ullage_core::summary::{MetricFilter, MetricFilterMode, filter_usage_measurements};
use ullage_protocol::{QueryOutcome, SubscriptionUsage};

/// Validates the repeated `metric=` values of one request.
///
/// An invalid name becomes `invalid_metric`; a valid but unknown name stays a
/// working filter with no matching measurements.
pub(crate) fn parse_metric_filter(names: &[String]) -> Result<MetricFilter, ()> {
    MetricFilter::new(names.to_vec()).map_err(|_| ())
}

/// Decodes one `metric=` value without rejecting NUL.
///
/// [`MetricFilter`] reports control characters as `invalid_metric`, so decoding
/// must not turn them into a generic bad request first. UTF-8 is still
/// enforced.
pub(crate) fn decode_metric_name(value: &str) -> Result<String, ()> {
    percent_decode_str(value)
        .decode_utf8()
        .map(|decoded| decoded.into_owned())
        .map_err(|_| ())
}

/// Applies a one-shot keep filter to the data a snapshot carries.
///
/// The outcome variant and its failure list stay untouched so the response
/// keeps reporting partial failures; only the measurements are filtered, and
/// `limit_reached` bookkeeping survives per the core filter contract.
pub(crate) fn filter_usage_outcome(
    outcome: &mut QueryOutcome<SubscriptionUsage>,
    filter: &MetricFilter,
) {
    match outcome {
        QueryOutcome::Complete { data } | QueryOutcome::Partial { data, .. } => {
            *data = filter_usage_measurements(data, filter, MetricFilterMode::Keep);
        }
    }
}
