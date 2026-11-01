use std::time::{Duration, SystemTime};

use hofund_plugin_api::{
    CredentialStyle, HostContext, PlanCard, PlanQuery, PlanWindow, PluginError, SecretString,
};
use hofund_plugin_sdk::Call;
use hofund_plugin_sdk::json::Value;

use crate::{Reader, fetch, json_body, number, time_value, vendor_error};

const HOUR: u64 = 3600;
const DAY: u64 = 24 * HOUR;

#[derive(Clone)]
pub(crate) struct CachedPlan {
    id: String,
    until: Option<SystemTime>,
    renew: String,
    at: SystemTime,
}

pub(crate) async fn read(
    reader: &Reader,
    query: &PlanQuery,
    host: &HostContext,
    credits_url: &str,
    subscriptions_url: &str,
) -> Result<Vec<PlanCard>, PluginError> {
    let (status, body) = fetch(host, key_call(query, credits_url)?).await?;
    if status >= 300 {
        return Err(vendor_error(status, &body));
    }
    let value = json_body(&body)?;
    let mut windows = command_windows(&value);
    if windows.is_empty() {
        let plan = value
            .pointer("/credits/planId")
            .and_then(Value::as_str)
            .map(plan_name)
            .unwrap_or_default();
        return Ok(vec![PlanCard::new(plan)]);
    }
    let mut id = value
        .pointer("/credits/planId")
        .and_then(Value::as_str)
        .unwrap_or("")
        .to_owned();
    let mut until = None;
    let mut renew = String::new();
    if id.is_empty() {
        let cached = reader
            .command_plans
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .get(query.api_key())
            .cloned();
        let fresh = cached.as_ref().is_some_and(|cached| {
            query.now().duration_since(cached.at).unwrap_or_default() < Duration::from_secs(600)
        });
        if let Some(cached) = cached.clone().filter(|_| fresh) {
            id = cached.id;
            until = cached.until;
            renew = cached.renew;
        } else {
            let (next_id, next_until, next_renew) =
                read_subscription(query, host, subscriptions_url).await;
            if let Some(next_id) = next_id {
                id = next_id;
                until = next_until;
                renew = next_renew;
            } else if let Some(cached) = cached {
                id = cached.id;
                until = cached.until;
                renew = cached.renew;
            }
            if !id.is_empty() {
                reader
                    .command_plans
                    .lock()
                    .unwrap_or_else(std::sync::PoisonError::into_inner)
                    .insert(
                        query.api_key().to_owned(),
                        CachedPlan {
                            id: id.clone(),
                            until,
                            renew: renew.clone(),
                            at: query.now(),
                        },
                    );
            }
        }
    }
    if until.is_some_and(|until| until <= query.now()) {
        until = None;
    }
    let (monthly, left, known) = credits_left(&value);
    let monthly_pool = plan_monthly(&id);
    if known && monthly_pool > 0.0 {
        let pool = monthly_pool.max(monthly) + left - monthly;
        if pool > 0.0 {
            let spent = pool - left;
            let mut credit = PlanWindow::new("Credits", (spent / pool * 100.0).clamp(0.0, 100.0))
                .with_display(format!("{} / {}", money(spent), money(pool)));
            if let Some(at) = until {
                credit = credit.with_resets_at(at);
            }
            windows.push(credit);
        }
    }
    let mut card = PlanCard::new(plan_name(&id)).with_windows(windows);
    if let Some(at) = until {
        card = card.with_term(at, renew);
    }
    Ok(vec![card])
}

fn key_call(query: &PlanQuery, url: &str) -> Result<Call, PluginError> {
    let secret = SecretString::from(query.api_key().to_owned());
    Ok(Call::get(url)
        .credential(CredentialStyle::Bearer, &secret)?
        .header("accept", "application/json")
        .header("accept-language", "en-US,en"))
}

async fn read_subscription(
    query: &PlanQuery,
    host: &HostContext,
    url: &str,
) -> (Option<String>, Option<SystemTime>, String) {
    let Ok((status, body)) = fetch(host, key_call(query, url).expect("key header")).await else {
        return (None, None, String::new());
    };
    if status >= 300 {
        return (None, None, String::new());
    }
    let Ok(value) = json_body(&body) else {
        return (None, None, String::new());
    };
    if value
        .get("success")
        .and_then(Value::as_bool)
        .is_some_and(|success| !success)
    {
        return (None, None, String::new());
    }
    let Some(data) = value.get("data") else {
        return (None, None, String::new());
    };
    let Some(id) = data.get("planId").and_then(Value::as_str) else {
        return (None, None, String::new());
    };
    if matches!(
        data.get("status").and_then(Value::as_str),
        Some("canceled" | "incomplete_expired")
    ) {
        return (None, None, String::new());
    }
    let renew = data
        .get("cancelAtPeriodEnd")
        .and_then(Value::as_bool)
        .map_or_else(String::new, |cancel| {
            if cancel { "off" } else { "auto" }.to_owned()
        });
    (
        Some(id.to_owned()),
        time_value(data.get("currentPeriodEnd")),
        renew,
    )
}

fn command_windows(value: &Value) -> Vec<PlanWindow> {
    let Some(limits) = value.get("windowLimits") else {
        return Vec::new();
    };
    let mut out = Vec::new();
    for (field, name, seconds) in [
        ("fiveHour", "5 hours", 5 * HOUR),
        ("weekly", "Weekly", 7 * DAY),
    ] {
        let Some(row) = limits.get(field) else {
            continue;
        };
        let (Some(used), Some(cap)) = (number(row.get("used")), number(row.get("cap"))) else {
            continue;
        };
        if cap <= 0.0 {
            continue;
        }
        out.push(
            PlanWindow::new(name, (used.max(0.0) / cap * 100.0).min(100.0))
                .with_span(Duration::from_secs(seconds))
                .with_optional_reset(time_value(row.get("resetAt"))),
        );
    }
    out
}

fn credits_left(value: &Value) -> (f64, f64, bool) {
    let mut monthly = 0.0;
    let mut left = 0.0;
    let mut known = false;
    for (index, field) in ["monthlyCredits", "purchasedCredits", "freeCredits"]
        .into_iter()
        .enumerate()
    {
        if let Some(amount) = number(value.pointer(&format!("/credits/{field}"))) {
            let amount = amount.max(0.0);
            if index == 0 {
                monthly = amount;
            }
            left += amount;
            known = true;
        }
    }
    (monthly, left, known)
}

fn plan_parts(id: &str) -> (&'static str, f64) {
    let id = id.to_ascii_lowercase().replace('_', "-");
    for (prefix, name, monthly) in [
        ("individual-provider", "Provider", 15.0),
        ("individual-pro-v1", "Pro", 80.0),
        ("individual-goat", "GOAT", 70.0),
        ("individual-ultra", "Ultra", 300.0),
        ("individual-max", "Max", 150.0),
        ("individual-pro", "Pro", 30.0),
        ("individual-go", "Go", 10.0),
        ("teams-pro", "Teams Pro", 40.0),
    ] {
        if id.starts_with(prefix) {
            return (name, monthly);
        }
    }
    ("", 0.0)
}

fn plan_name(id: &str) -> String {
    plan_parts(id).0.to_owned()
}

fn plan_monthly(id: &str) -> f64 {
    plan_parts(id).1
}

fn money(amount: f64) -> String {
    format!("${amount:.2}")
}
