//! Qoder account-page usage windows.

use std::sync::Arc;

use hofund_plugin_api::{AccountId, HostContext, PluginError, QuotaWindow};
use hofund_plugin_sdk::json::Value;
use hofund_plugin_sdk::oauth::secret_text;

use crate::{Endpoints, identity, models, refresh};

pub(crate) async fn read(
    host: &HostContext,
    endpoints: Arc<Endpoints>,
    account: &AccountId,
) -> Result<Vec<QuotaWindow>, PluginError> {
    let mut stored = refresh::ensure_fresh(host, Arc::clone(&endpoints), account).await?;
    for attempt in 0..2 {
        let site = endpoints.resolve(&stored.credential)?;
        let url = format!(
            "{}/sash/api/v2/me/usage",
            site.openapi.trim_end_matches('/')
        );
        let token = if secret_text(&stored.credential.device_token).is_empty() {
            &stored.credential.token
        } else {
            &stored.credential.device_token
        };
        let reply = identity::apply_quota(hofund_plugin_sdk::Call::get(url))
            .sensitive_header("authorization", format!("Bearer {}", secret_text(token)))
            .header("Accept", "application/json")
            .fetch(host.http())
            .await?;
        match reply.status() {
            200..=299 => {
                let mut windows = windows(reply.body())?;
                if !windows.is_empty()
                    && let Ok(listed) =
                        models::list_account(host, Arc::clone(&endpoints), account).await
                {
                    let free: Vec<String> = listed
                        .models()
                        .iter()
                        .filter(|model| model.free() == Some(true))
                        .map(|model| model.id().to_owned())
                        .collect();
                    if !free.is_empty() {
                        windows = windows
                            .into_iter()
                            .map(|window| window.with_excluded_models(free.clone()))
                            .collect();
                    }
                }
                return Ok(windows);
            }
            401 | 403 if attempt == 0 => {
                stored = refresh::force_device(host, Arc::clone(&endpoints), account).await?;
            }
            401 | 403 => {
                return Err(PluginError::status_error(
                    reply.status(),
                    "Qoder refused the account-page sign-in while reading usage",
                ));
            }
            status => {
                return Err(PluginError::status_error(
                    status,
                    "Qoder did not return usage",
                ));
            }
        }
    }
    Err(PluginError::internal(
        "Qoder usage retry ended unexpectedly",
    ))
}

pub(crate) fn windows(body: &[u8]) -> Result<Vec<QuotaWindow>, PluginError> {
    let root: Value = hofund_plugin_sdk::json::from_slice(body)
        .map_err(|_| PluginError::decode("Qoder usage JSON is invalid"))?;
    match root.get("displayMode").and_then(Value::as_str) {
        Some("enterprise") => return Ok(Vec::new()),
        Some("qoder") => {}
        _ => return Err(PluginError::decode("Qoder usage display mode is unknown")),
    }
    let usage = root
        .get("qoderUsage")
        .and_then(Value::as_object)
        .ok_or_else(|| PluginError::decode("Qoder usage has no quota data"))?;
    let reset = field(usage, &["expiresAt", "expires_at"])
        .and_then(|value| {
            hofund_plugin_sdk::chrono::DateTime::parse_from_rfc3339(value.as_str()?).ok()
        })
        .map(|date| date.into());
    let mut pools = Vec::new();
    for (keys, name) in [
        (&["userQuota", "user_quota"][..], "Credits"),
        (&["addOnQuota", "add_on_quota"][..], "Add-on credits"),
        (
            &["orgResourcePackage", "org_resource_package"][..],
            "Shared credits",
        ),
    ] {
        if let Some(pool) = field(usage, keys).and_then(|value| pool_window(value, name, reset)) {
            pools.push(pool);
        }
    }
    if let Some(dedicated) = field(
        usage,
        &["dedicatedResourcePackages", "dedicated_resource_packages"],
    )
    .and_then(Value::as_array)
    {
        for pool in dedicated {
            if let Some(window) = pool_window(pool, "Dedicated credits", None) {
                pools.push(window);
            }
        }
    }
    if pools.is_empty()
        && ["userQuota", "addOnQuota", "orgResourcePackage"]
            .iter()
            .filter_map(|key| usage.get(*key))
            .any(|pool| {
                pool.get("total")
                    .or_else(|| pool.get("cap"))
                    .and_then(Value::as_f64)
                    == Some(0.0)
            })
    {
        return Ok(vec![QuotaWindow::new("Credits", 100, Some(100), None)?]);
    }
    let shared: Vec<&Pool> = pools
        .iter()
        .filter(|pool| {
            matches!(
                pool.name.as_str(),
                "Credits" | "Add-on credits" | "Shared credits"
            )
        })
        .collect();
    let total_used: f64 = shared.iter().map(|pool| pool.used).sum();
    let total_cap: f64 = shared.iter().map(|pool| pool.total).sum();
    let mut windows = Vec::new();
    if shared.len() > 1 && total_cap > 0.0 {
        windows.push(QuotaWindow::new(
            "Total credits",
            percent(total_used, total_cap),
            Some(100),
            reset,
        )?);
    }
    windows.extend(pools.into_iter().map(|pool| pool.window));
    Ok(windows)
}

struct Pool {
    name: String,
    used: f64,
    total: f64,
    window: QuotaWindow,
}

fn pool_window(
    value: &Value,
    fallback: &str,
    reset: Option<std::time::SystemTime>,
) -> Option<Pool> {
    let total = value
        .get("total")
        .or_else(|| value.get("cap"))
        .and_then(Value::as_f64)
        .filter(|total| *total > 0.0)?;
    let used = value
        .get("used")
        .and_then(Value::as_f64)
        .or_else(|| Some(total - value.get("remaining")?.as_f64()?))
        .filter(|used| *used >= 0.0)?;
    let name = value
        .get("name")
        .and_then(Value::as_str)
        .unwrap_or(fallback)
        .to_owned();
    let window = QuotaWindow::new(&name, percent(used, total), Some(100), reset).ok()?;
    Some(Pool {
        name,
        used: used.min(total),
        total,
        window,
    })
}

fn percent(used: f64, total: f64) -> u64 {
    ((used / total) * 100.0).floor().clamp(0.0, 100.0) as u64
}

fn field<'a>(
    object: &'a hofund_plugin_sdk::json::Map<String, Value>,
    keys: &[&str],
) -> Option<&'a Value> {
    keys.iter().find_map(|key| object.get(*key))
}
