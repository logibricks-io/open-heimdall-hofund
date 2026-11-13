//! Start Plan and gift balances from the Z Code portal.

use std::path::Path;
use std::time::{Duration, UNIX_EPOCH};

use hofund_plugin_api::{HostContext, PluginError, QuotaWindow, SecretString};
use hofund_plugin_sdk::json::Value;
use hofund_plugin_sdk::oauth::secret_text;

use crate::session::Stored;
use crate::{APP_VERSION, Endpoints, api};

pub(crate) struct Balance {
    pub live: bool,
    pub plan: String,
    pub all_models: Vec<String>,
    pub usable_models: Vec<String>,
    pub windows: Vec<QuotaWindow>,
}

pub(crate) async fn read(
    host: &HostContext,
    endpoints: &Endpoints,
    stored: &Stored,
    device_file: &Path,
) -> Result<Balance, PluginError> {
    let jwt =
        stored.auth.jwt.as_ref().ok_or_else(|| {
            PluginError::config("Z Code gift plans require a Z Code browser sign-in")
        })?;
    ensure_live_jwt(host, jwt)?;
    let now = host
        .clock()
        .now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default()
        .as_secs_f64();
    let url = format!(
        "{}/api/v1/zcode-plan/billing/balance?app_version={APP_VERSION}",
        endpoints.portal.trim_end_matches('/')
    );
    let authorization = SecretString::from(format!("Bearer {}", secret_text(jwt)));
    let headers = [("x-device-mid", stored.device.as_str())];
    let data = api::get_envelope(&api::EnvelopeRequest::get(
        host,
        endpoints,
        &url,
        Some(&authorization),
        &headers,
        device_file,
    ))
    .await?;
    parse(
        &data,
        stored.auth.api_key.is_none() && !stored.auth.team(),
        now,
    )
}

pub(crate) fn ensure_live_jwt(host: &HostContext, jwt: &SecretString) -> Result<(), PluginError> {
    let now_ms = host
        .clock()
        .now()
        .duration_since(UNIX_EPOCH)
        .map(|duration| duration.as_millis() as u64)
        .unwrap_or(0);
    if crate::creds::jwt_exp_ms(jwt).is_some_and(|expiry| expiry <= now_ms) {
        return Err(PluginError::auth(
            "the Z Code sign-in has expired; sign in again",
        ));
    }
    Ok(())
}

fn parse(data: &Value, start_only: bool, host_now: f64) -> Result<Balance, PluginError> {
    let now = number(data.get("server_time")).unwrap_or(host_now);
    let empty = Vec::new();
    let plans = data
        .get("plans")
        .and_then(Value::as_array)
        .unwrap_or(&empty);
    let balances = data
        .get("balances")
        .and_then(Value::as_array)
        .unwrap_or(&empty);
    let live_plans: Vec<&Value> = plans
        .iter()
        .filter(|plan| {
            text(plan, "status").eq_ignore_ascii_case("active")
                && !not_started(plan, now)
                && !ended(plan, now)
        })
        .collect();
    let pending: Vec<&Value> = plans
        .iter()
        .filter(|plan| not_started(plan, now) && !ended(plan, now))
        .collect();
    let live = !live_plans.is_empty();
    if !live && pending.is_empty() && start_only {
        return Err(PluginError::decode(
            "this account has no GLM Coding Plan, and the Z Code Start Plan has ended or was never started",
        ));
    }
    let plan = live_plans
        .first()
        .or_else(|| pending.first())
        .and_then(|plan| text_owned(plan, "name"))
        .filter(|name| !name.is_empty())
        .unwrap_or_else(|| "Start Plan".to_owned());
    let several = live_plans.len() > 1;
    let mut all_models = Vec::new();
    let mut usable_models = Vec::new();
    let mut windows = Vec::new();
    for bucket in balances {
        let Some(owner) = live_plans.iter().find(|plan| same_plan(bucket, plan)) else {
            continue;
        };
        let expiry = number(bucket.get("expires_at"));
        if expiry.is_some_and(|seconds| seconds > 0.0 && seconds <= now) {
            continue;
        }
        let models = bucket_models(bucket);
        let spent = bucket_spent(bucket);
        for model in &models {
            push_unique(&mut all_models, model);
            if !spent {
                push_unique(&mut usable_models, model);
            }
        }
        let total = number(bucket.get("total_units"));
        let remaining = number(bucket.get("remaining_units"));
        let used = number(bucket.get("used_units")).or_else(|| {
            total
                .zip(remaining)
                .map(|(total, remaining)| (total - remaining).max(0.0))
        });
        let Some(used) = used else {
            continue;
        };
        let used_pct = if spent {
            100
        } else {
            total
                .filter(|total| *total > 0.0)
                .map(|total| (100.0 * used / total).round() as u64)
                .unwrap_or(0)
        };
        let show_name = text_owned(bucket, "show_name")
            .filter(|name| !name.is_empty())
            .or_else(|| models.first().cloned())
            .unwrap_or_else(|| "Credits".to_owned());
        let plan_name = text(owner, "name");
        let window_name = if start_only {
            if several && !plan_name.is_empty() {
                format!("{show_name} · {plan_name}")
            } else {
                show_name
            }
        } else {
            format!(
                "{show_name} · {}",
                if plan_name.is_empty() {
                    "Start Plan"
                } else {
                    plan_name
                }
            )
        };
        let resets_at = expiry
            .filter(|seconds| *seconds > 0.0)
            .map(|seconds| UNIX_EPOCH + Duration::from_secs_f64(seconds));
        let scope: Vec<String> = models
            .iter()
            .flat_map(|model| {
                if start_only {
                    vec![model.clone(), gift_id(model)]
                } else {
                    vec![gift_id(model)]
                }
            })
            .collect();
        if scope.is_empty() {
            windows.push(QuotaWindow::new(
                window_name,
                used_pct,
                Some(100),
                resets_at,
            )?);
        } else {
            for model in scope {
                windows.push(
                    QuotaWindow::new(window_name.clone(), used_pct, Some(100), resets_at)?
                        .with_scope_model(model),
                );
            }
        }
    }
    for _plan in pending {
        windows.push(
            QuotaWindow::new("Z Code gift plan not started yet", 0, Some(100), None)?
                .with_blocking(false),
        );
    }
    Ok(Balance {
        live,
        plan,
        all_models,
        usable_models,
        windows,
    })
}

pub(crate) fn gift_id(model: &str) -> String {
    format!("{}-Trial", canonical_model(model))
}

pub(crate) fn canonical_model(model: &str) -> String {
    const MODELS: &[&str] = &["GLM-5.3", "GLM-5.3-Flash", "GLM-5.2", "GLM-5-Turbo"];
    MODELS
        .iter()
        .find(|known| known.eq_ignore_ascii_case(model))
        .map_or_else(|| model.to_owned(), |known| (*known).to_owned())
}

pub(crate) fn model_for_trial(model: &str) -> Option<String> {
    model
        .strip_suffix("-Trial")
        .or_else(|| model.strip_suffix("-trial"))
        .map(canonical_model)
}

fn bucket_models(bucket: &Value) -> Vec<String> {
    bucket
        .get("capabilities")
        .and_then(Value::as_array)
        .into_iter()
        .flatten()
        .filter_map(Value::as_str)
        .map(str::trim)
        .filter(|capability| capability.starts_with("model:") || !capability.contains(':'))
        .filter_map(|capability| {
            let model = capability
                .strip_prefix("model:")
                .unwrap_or(capability)
                .trim();
            (!model.is_empty()).then(|| model.to_owned())
        })
        .collect()
}

fn bucket_spent(bucket: &Value) -> bool {
    if let Some(left) = number(bucket.get("remaining_units")) {
        return left <= 0.0;
    }
    match (
        number(bucket.get("total_units")),
        number(bucket.get("used_units")),
    ) {
        (Some(total), Some(used)) => used >= total,
        _ => false,
    }
}

fn same_plan(bucket: &Value, plan: &Value) -> bool {
    let bucket_user = text(bucket, "user_plan_id");
    let plan_user = text(plan, "user_plan_id");
    if !bucket_user.is_empty() && !plan_user.is_empty() {
        return bucket_user == plan_user;
    }
    text(bucket, "plan_id") == text(plan, "plan_id")
}

fn not_started(plan: &Value, now: f64) -> bool {
    number(plan.get("starts_at")).is_some_and(|start| start > now)
        && !text(plan, "status").eq_ignore_ascii_case("expired")
}

fn ended(plan: &Value, now: f64) -> bool {
    number(plan.get("ends_at")).is_some_and(|end| end > 0.0 && end <= now)
        || text(plan, "status").eq_ignore_ascii_case("expired")
}

fn push_unique(models: &mut Vec<String>, model: &str) {
    if !models
        .iter()
        .any(|existing| existing.eq_ignore_ascii_case(model))
    {
        models.push(model.to_owned());
    }
}

fn text<'a>(value: &'a Value, key: &str) -> &'a str {
    value.get(key).and_then(Value::as_str).unwrap_or("")
}

fn text_owned(value: &Value, key: &str) -> Option<String> {
    value
        .get(key)
        .and_then(Value::as_str)
        .map(str::trim)
        .map(str::to_owned)
}

fn number(value: Option<&Value>) -> Option<f64> {
    value.and_then(|value| match value {
        Value::Number(number) => number.as_f64(),
        Value::String(text) => text.trim().parse().ok(),
        _ => None,
    })
}
