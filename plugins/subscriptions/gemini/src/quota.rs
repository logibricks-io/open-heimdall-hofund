//! Code Assist usage limits and models from `retrieveUserQuota`.

use hofund_plugin_api::{ModelSpec, PluginError, QuotaWindow};
use hofund_plugin_sdk::json::Value;
use hofund_plugin_sdk::time::parse_rfc3339;

/// The scale a remaining fraction is reported on.
const SCALE: u64 = 10_000;

/// One quota bucket Code Assist reported.
#[derive(Debug, PartialEq)]
struct Bucket<'a> {
    model: &'a str,
    remaining: Option<f64>,
    resets: Option<&'a str>,
}

/// The buckets Gemini CLI shows: skips ones with no model, a `_vertex`
/// model, or a model already seen.
fn buckets(reply: &Value) -> Vec<Bucket<'_>> {
    let mut seen: Vec<&str> = Vec::new();
    let mut out = Vec::new();
    for bucket in reply
        .get("buckets")
        .and_then(Value::as_array)
        .into_iter()
        .flatten()
    {
        let Some(model) = bucket.get("modelId").and_then(Value::as_str) else {
            continue;
        };
        if model.is_empty() || model.ends_with("_vertex") || seen.contains(&model) {
            continue;
        }
        seen.push(model);
        out.push(Bucket {
            model,
            remaining: bucket.get("remainingFraction").and_then(Value::as_f64),
            resets: bucket.get("resetTime").and_then(Value::as_str),
        });
    }
    out
}

/// One window per model with a remaining fraction: used is the part spent,
/// out of [`SCALE`], and the window covers only that model.
///
/// # Errors
///
/// Never in practice: names are non-empty and used is at most the limit.
pub(crate) fn windows(reply: &Value) -> Result<Vec<QuotaWindow>, PluginError> {
    buckets(reply)
        .into_iter()
        .filter_map(|bucket| Some((bucket.model, bucket.remaining?, bucket.resets)))
        .map(|(model, remaining, resets)| {
            QuotaWindow::new(
                model,
                spent(remaining),
                Some(SCALE),
                resets.and_then(parse_rfc3339),
            )
            .map(|window| window.with_scope_model(model))
        })
        .collect()
}

/// The models the buckets name.
///
/// # Errors
///
/// Returns an error when a model id is not a valid model name.
pub(crate) fn models(reply: &Value) -> Result<Vec<ModelSpec>, PluginError> {
    buckets(reply)
        .into_iter()
        .map(|bucket| ModelSpec::new(bucket.model, None))
        .collect()
}

#[expect(
    clippy::cast_possible_truncation,
    clippy::cast_sign_loss,
    clippy::cast_precision_loss,
    reason = "the value is clamped to 0..=SCALE first"
)]
fn spent(remaining: f64) -> u64 {
    let remaining = if remaining.is_nan() {
        0.0
    } else {
        remaining.clamp(0.0, 1.0)
    };
    ((1.0 - remaining) * SCALE as f64).round() as u64
}

#[cfg(test)]
mod tests {
    use hofund_plugin_sdk::json::json;

    use super::{buckets, spent};

    #[test]
    fn spent_is_the_rounded_complement() {
        assert_eq!(spent(1.0), 0);
        assert_eq!(spent(0.0), 10_000);
        assert_eq!(spent(0.25), 7_500);
        assert_eq!(spent(0.123_45), 8_766);
        assert_eq!(spent(1.5), 0);
        assert_eq!(spent(-0.5), 10_000);
        assert_eq!(spent(f64::NAN), 10_000);
    }

    #[test]
    fn buckets_skip_empty_vertex_and_repeated_models() {
        let reply = json!({"buckets": [
            {"modelId": "", "remainingFraction": 0.5},
            {"remainingFraction": 0.5},
            {"modelId": "gemini-2.5-pro_vertex", "remainingFraction": 0.5},
            {"modelId": "gemini-2.5-pro", "remainingFraction": 0.5, "resetTime": "2026-11-01T00:00:00Z"},
            {"modelId": "gemini-2.5-pro", "remainingFraction": 0.9},
            {"modelId": "gemini-2.5-flash"},
        ]});
        let found = buckets(&reply);
        let models: Vec<&str> = found.iter().map(|bucket| bucket.model).collect();
        assert_eq!(models, vec!["gemini-2.5-pro", "gemini-2.5-flash"]);
        assert_eq!(found[0].remaining, Some(0.5));
        assert_eq!(found[0].resets, Some("2026-11-01T00:00:00Z"));
        assert_eq!(found[1].remaining, None);
        assert!(buckets(&json!({})).is_empty());
    }
}
