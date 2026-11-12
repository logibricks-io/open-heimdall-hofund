//! Per-model and shared quota windows from Antigravity's model catalog.

use hofund_plugin_api::{HostContext, PluginError, QuotaWindow, SecretString};
use hofund_plugin_sdk::json::{Value, json};
use hofund_plugin_sdk::time::parse_rfc3339;

use crate::{Endpoints, models, project};

const SCALE: u64 = 10_000;

pub(crate) fn model_windows(models: &[models::RawModel]) -> Result<Vec<QuotaWindow>, PluginError> {
    models
        .iter()
        .filter_map(|model| Some((model, model.remaining?)))
        .map(|(model, remaining)| {
            let name = if model.name.is_empty() {
                model.id.clone()
            } else {
                model.name.clone()
            };
            QuotaWindow::new(name, spent(remaining), Some(SCALE), model.resets)
                .map(|window| window.with_scope_model(model.id.clone()))
        })
        .collect()
}

pub(crate) async fn summary_windows(
    host: &HostContext,
    endpoints: &Endpoints,
    access: &SecretString,
    project_id: &str,
) -> Vec<QuotaWindow> {
    let Ok(reply) = project::call(
        host,
        &endpoints.code_assist,
        access,
        "retrieveUserQuotaSummary",
        &json!({"project": project_id}),
        &[],
    )
    .await
    else {
        return Vec::new();
    };
    summary(&reply).unwrap_or_default()
}

fn summary(reply: &Value) -> Result<Vec<QuotaWindow>, PluginError> {
    let mut out = Vec::new();
    for group in reply
        .get("groups")
        .and_then(Value::as_array)
        .into_iter()
        .flatten()
    {
        let pool = pool_name(
            group
                .get("displayName")
                .and_then(Value::as_str)
                .unwrap_or_default(),
        );
        if pool.is_empty() {
            continue;
        }
        for bucket in group
            .get("buckets")
            .and_then(Value::as_array)
            .into_iter()
            .flatten()
        {
            if bucket.get("disabled").and_then(Value::as_bool) == Some(true) {
                continue;
            }
            let window = bucket
                .get("window")
                .and_then(Value::as_str)
                .unwrap_or_default();
            let name = match window.to_ascii_lowercase().as_str() {
                "5h" => "5 hours".to_owned(),
                "weekly" => "7 days".to_owned(),
                _ => ["displayName", "window", "bucketId"]
                    .into_iter()
                    .find_map(|key| {
                        bucket
                            .get(key)
                            .and_then(Value::as_str)
                            .filter(|name| !name.is_empty())
                    })
                    .unwrap_or_default()
                    .to_owned(),
            };
            if name.is_empty() {
                continue;
            }
            let remaining = bucket
                .get("remainingFraction")
                .and_then(Value::as_f64)
                .unwrap_or(0.0);
            let reset = bucket
                .get("resetTime")
                .and_then(Value::as_str)
                .and_then(parse_rfc3339);
            out.push(
                QuotaWindow::new(
                    format!("{pool} · {name}"),
                    spent(remaining),
                    Some(SCALE),
                    reset,
                )?
                .with_blocking(false),
            );
        }
    }
    Ok(out)
}

fn pool_name(name: &str) -> String {
    let name = name.trim();
    let name = if name.to_ascii_lowercase().ends_with(" models") {
        name[..name.len() - " models".len()].trim()
    } else {
        name
    };
    name.replace(" and ", " & ")
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

    use super::{pool_name, spent, summary};

    #[test]
    fn spent_is_the_clamped_complement() {
        assert_eq!(spent(1.0), 0);
        assert_eq!(spent(0.25), 7_500);
        assert_eq!(spent(0.0), 10_000);
        assert_eq!(spent(2.0), 0);
        assert_eq!(spent(-1.0), 10_000);
        assert_eq!(spent(f64::NAN), 10_000);
    }

    #[test]
    fn summary_names_and_disables_shared_windows() {
        let windows = summary(&json!({"groups": [{
            "displayName": "Claude and GPT models",
            "buckets": [
                {"window": "5h", "remainingFraction": 0.5},
                {"window": "weekly", "resetTime": "2026-11-01T00:00:00Z"},
                {"window": "monthly", "disabled": true}
            ]
        }, {
            "displayName": "",
            "buckets": [{"window": "5h", "remainingFraction": 1}]
        }]}))
        .expect("windows");
        assert_eq!(windows.len(), 2);
        assert_eq!(windows[0].name(), "Claude & GPT · 5 hours");
        assert_eq!(windows[0].used(), 5_000);
        assert!(!windows[0].blocking());
        assert_eq!(windows[1].name(), "Claude & GPT · 7 days");
        assert_eq!(windows[1].used(), 10_000);
        assert_eq!(pool_name(" Gemini Models "), "Gemini");
    }
}
