//! Host-known balance endpoints for provider keys.

use std::time::{Duration, SystemTime};

use hofund_plugin_api::{
    BalanceCard, BalancePart, BalanceQuery, BalanceSupport, BalanceWallet, HostContext,
    PlanWindow, PluginError,
};
use hofund_plugin_sdk::Call;
use hofund_plugin_sdk::json::Value;

use crate::{Reader, fetch, json_body, number, vendor_error};

fn at_time(value: &Value) -> Option<SystemTime> {
    crate::time_value(Some(value))
}

const COMMAND_PLAN_KEEP: Duration = Duration::from_secs(600);

#[derive(Clone)]
enum Source {
    DeepSeek(String),
    Moonshot { url: String, sign: &'static str },
    OpenRouter(String),
    SiliconFlow { url: String, sign: &'static str, console: &'static str },
    StepFun { url: String, sign: &'static str },
    CommandCode { credits: String, subscriptions: String },
    Ollama(String),
    AiHubMix { url: String, key: bool },
    OpenCodeZen,
}

impl Source {
    const fn support(&self) -> BalanceSupport {
        match self {
            Self::OpenCodeZen => BalanceSupport::Certain,
            Self::AiHubMix { key: true, .. } => BalanceSupport::Possible,
            _ => BalanceSupport::Certain,
        }
    }
}

pub(crate) fn support(query: &BalanceQuery) -> BalanceSupport {
    if !query.balance_url().trim().is_empty() {
        return BalanceSupport::Unsupported;
    }
    source(query).map_or(BalanceSupport::Unsupported, |source| source.support())
}

pub(crate) async fn read(
    reader: &Reader,
    query: &BalanceQuery,
    host: &HostContext,
) -> Result<Option<BalanceCard>, PluginError> {
    if query.api_key().is_empty() {
        return Ok(None);
    }
    let Some(source) = source(query) else {
        return Ok(None);
    };
    if matches!(source, Source::OpenCodeZen) {
        return Err(PluginError::unsupported(
            "OpenCode tells no API key its balance: Zen's pay-as-you-go credits are shown only in the OpenCode console (https://opencode.ai/console, Overview, Credits)",
        ));
    }
    read_source(reader, query, host, source).await.map(Some)
}

async fn read_source(
    reader: &Reader,
    query: &BalanceQuery,
    host: &HostContext,
    source: Source,
) -> Result<BalanceCard, PluginError> {
    let (post, url, token, _account_wide) = request_of(query, &source)?;
    let (status, body) = send(query, host, post, &url, &token).await?;
    if let Some(message) = balance_refusal(&body) {
        let message = user_header_hint(&message);
        return Err(if status >= 300 {
            PluginError::status_error(status, message)
        } else {
            PluginError::decode(message)
        });
    }
    if status >= 300 {
        if let Source::SiliconFlow { console, .. } = &source
            && retired_siliconflow(status, &body)
        {
            return Err(PluginError::status_error(
                status,
                format!(
                    "SiliconFlow retired its balance query (/v1/user/info) and has no API for it in its place yet; the balance and vouchers are on {console}"
                ),
            ));
        }
        return Err(vendor_error(status, &body));
    }
    parse(reader, query, host, source, &body).await
}

fn source(query: &BalanceQuery) -> Option<Source> {
    for base in query.bases() {
        let url = hofund_plugin_sdk::url::Url::parse(base).ok()?;
        let host = url.host_str()?.to_ascii_lowercase();
        let path = url.path();
        match host.as_str() {
            "api.deepseek.com" => {
                return Some(Source::DeepSeek(
                    "https://api.deepseek.com/user/balance".to_owned(),
                ));
            }
            "api.moonshot.cn" => {
                return Some(Source::Moonshot {
                    url: "https://api.moonshot.cn/v1/users/me/balance".to_owned(),
                    sign: "¥",
                });
            }
            "api.moonshot.ai" => {
                return Some(Source::Moonshot {
                    url: "https://api.moonshot.ai/v1/users/me/balance".to_owned(),
                    sign: "$",
                });
            }
            "openrouter.ai" => {
                return Some(Source::OpenRouter(
                    "https://openrouter.ai/api/v1/credits".to_owned(),
                ));
            }
            "api.siliconflow.cn" => {
                return Some(Source::SiliconFlow {
                    url: "https://api.siliconflow.cn/v1/user/info".to_owned(),
                    sign: "¥",
                    console: "https://cloud.siliconflow.cn",
                });
            }
            "api.siliconflow.com" => {
                return Some(Source::SiliconFlow {
                    url: "https://api.siliconflow.com/v1/user/info".to_owned(),
                    sign: "$",
                    console: "https://cloud.siliconflow.com",
                });
            }
            "api.stepfun.com" => {
                return Some(Source::StepFun {
                    url: "https://api.stepfun.com/v1/accounts".to_owned(),
                    sign: "¥",
                });
            }
            "api.stepfun.ai" => {
                return Some(Source::StepFun {
                    url: "https://api.stepfun.ai/v1/accounts".to_owned(),
                    sign: "$",
                });
            }
            "api.commandcode.ai" => {
                return Some(Source::CommandCode {
                    credits: "https://api.commandcode.ai/alpha/billing/credits".to_owned(),
                    subscriptions: "https://api.commandcode.ai/alpha/billing/subscriptions"
                        .to_owned(),
                });
            }
            "ollama.com" => {
                return Some(Source::Ollama(
                    "https://ollama.com/api/balance".to_owned(),
                ));
            }
            "aihubmix.com" if !query.balance_token().trim().is_empty() => {
                return Some(Source::AiHubMix {
                    url: "https://aihubmix.com/api/user/self".to_owned(),
                    key: false,
                });
            }
            "aihubmix.com" => {
                return Some(Source::AiHubMix {
                    url: "https://aihubmix.com/dashboard/billing/remain".to_owned(),
                    key: true,
                });
            }
            "opencode.ai" if open_code_zen(path) => return Some(Source::OpenCodeZen),
            _ => {}
        }
    }
    None
}

fn request_of(
    query: &BalanceQuery,
    source: &Source,
) -> Result<(bool, String, String, bool), PluginError> {
    let token = match source {
        Source::AiHubMix { key: false, .. } => query.balance_token().to_owned(),
        _ => String::new(),
    };
    let account_wide = !matches!(
        source,
        Source::AiHubMix { key: true, .. } | Source::OpenCodeZen
    );
    let url = match source {
        Source::DeepSeek(url)
        | Source::OpenRouter(url)
        | Source::Ollama(url)
        | Source::CommandCode { credits: url, .. } => url.clone(),
        Source::Moonshot { url, .. }
        | Source::SiliconFlow { url, .. }
        | Source::StepFun { url, .. }
        | Source::AiHubMix { url, .. } => url.clone(),
        Source::OpenCodeZen => String::new(),
    };
    let url = with_key(&url, query.api_key(), true);
    Ok((false, url, token, account_wide))
}

async fn send(
    query: &BalanceQuery,
    host: &HostContext,
    post: bool,
    url: &str,
    token: &str,
) -> Result<(u16, Vec<u8>), PluginError> {
    let mut call = if post {
        Call::post(url)
    } else {
        Call::get(url)
    };
    call = call.header("accept", "application/json");
    if token.is_empty() {
        call = call.sensitive_header("authorization", format!("Bearer {}", query.api_key()));
    } else {
        call = call.sensitive_header("authorization", balance_authorization(token));
    }
    for (name, value) in query.headers() {
        call = call.sensitive_header(name.as_str(), with_key(value, query.api_key(), false));
    }
    fetch(host, call).await
}

async fn parse(
    reader: &Reader,
    query: &BalanceQuery,
    host: &HostContext,
    source: Source,
    body: &[u8],
) -> Result<BalanceCard, PluginError> {
    match source {
        Source::DeepSeek(_) => parse_deepseek(body),
        Source::Moonshot { sign, .. } => {
            let amount = required_number(&at(body, &["data", "available_balance"])?)?;
            Ok(single_balance(money(sign, amount), amount, currency_unit(sign), true))
        }
        Source::OpenRouter(_) => {
            let credits = required_number(&at(body, &["data", "total_credits"])?)?;
            let used = optional_number(&at(body, &["data", "total_usage"])?).unwrap_or(0.0);
            let amount = credits - used;
            Ok(single_balance(money("$", amount), amount, "USD", true))
        }
        Source::SiliconFlow { sign, .. } => {
            let amount = required_number(&at(body, &["data", "totalBalance"])?)?;
            Ok(single_balance(
                money(sign, amount),
                amount,
                currency_unit(sign),
                true,
            ))
        }
        Source::StepFun { sign, .. } => {
            let amount = required_number(&at(body, &["balance"])?)?;
            Ok(single_balance(
                money(sign, amount),
                amount,
                currency_unit(sign),
                true,
            ))
        }
        Source::AiHubMix { key: true, .. } => parse_aihubmix_key(body),
        Source::AiHubMix { key: false, .. } => parse_aihubmix_account(body),
        Source::CommandCode {
            credits: _,
            subscriptions,
        } => parse_command(reader, query, host, &subscriptions, body).await,
        Source::Ollama(_) => parse_ollama(body),
        Source::OpenCodeZen => unreachable!(),
    }
}

fn single_balance(text: String, amount: f64, unit: &str, account_wide: bool) -> BalanceCard {
    BalanceCard::pack(
        text,
        Vec::new(),
        Vec::new(),
        vec![BalanceWallet::new(amount, unit)],
        Vec::new(),
        String::new(),
        Vec::new(),
        None,
        String::new(),
        account_wide,
    )
}

fn parse_deepseek(body: &[u8]) -> Result<BalanceCard, PluginError> {
    let reply = json_body(body)?;
    let infos = reply
        .get("balance_infos")
        .and_then(Value::as_array)
        .ok_or_else(|| PluginError::decode("no balance in the reply"))?;
    let mut amounts = Vec::new();
    for info in infos {
        let Some(total) = info.get("total_balance").and_then(optional_number) else {
            continue;
        };
        let code = info
            .get("currency")
            .and_then(Value::as_str)
            .unwrap_or_default()
            .to_uppercase();
        let granted = info.get("granted_balance").and_then(optional_number);
        let topped = info.get("topped_up_balance").and_then(optional_number);
        amounts.push((code, total, granted.zip(topped)));
    }
    if amounts.is_empty() {
        return Err(PluginError::decode("no balance in the reply"));
    }
    amounts.sort_by(|left, right| {
        (right.1 > 0.0)
            .cmp(&(left.1 > 0.0))
            .then_with(|| left.0.cmp(&right.0))
    });
    let mut text = Vec::new();
    let mut split = Vec::new();
    let mut wallets = Vec::new();
    for (code, total, parts) in amounts {
        let sign = currency_sign(&code);
        text.push(money(sign, total));
        if let Some((granted, topped)) = parts
            && granted != 0.0
        {
            split.push(
                BalancePart::new(money(sign, granted)).with_label("Granted"),
            );
            split.push(
                BalancePart::new(money(sign, topped)).with_label("Topped up"),
            );
        }
        wallets.push(BalanceWallet::new(total, code));
    }
    Ok(BalanceCard::pack(
        text.join(" · "),
        Vec::new(),
        split,
        wallets,
        Vec::new(),
        String::new(),
        Vec::new(),
        None,
        String::new(),
        true,
    ))
}

fn parse_aihubmix_key(body: &[u8]) -> Result<BalanceCard, PluginError> {
    let amount = required_number(&at(body, &["total_usage"])?)?;
    if amount < 0.0 {
        return Err(PluginError::decode(
            "this key has no limit, and AiHubMix tells a key only what is left on it: give hofund the account's access token in this provider's balance settings, or give the key a limit in AiHubMix's console",
        ));
    }
    Ok(single_balance(money("$", amount), amount, "USD", false))
}

fn parse_aihubmix_account(body: &[u8]) -> Result<BalanceCard, PluginError> {
    let reply = json_body(body)?;
    if reply.get("success").and_then(Value::as_bool) != Some(true) {
        return Err(PluginError::decode(
            reply
                .get("message")
                .and_then(Value::as_str)
                .filter(|message| !message.is_empty())
                .unwrap_or("AiHubMix didn't take the access token"),
        ));
    }
    let amount = required_number(
        reply
            .pointer("/data/quota")
            .ok_or_else(|| PluginError::decode("no balance in the reply"))?,
    )? / 500_000.0;
    Ok(single_balance(money("$", amount), amount, "USD", true))
}

async fn parse_command(
    reader: &Reader,
    query: &BalanceQuery,
    host: &HostContext,
    subscriptions_url: &str,
    body: &[u8],
) -> Result<BalanceCard, PluginError> {
    let reply = json_body(body)?;
    let credits = reply
        .get("credits")
        .ok_or_else(|| PluginError::decode("no balance in the reply"))?;
    let monthly = credits.get("monthlyCredits").and_then(optional_number);
    let purchased = credits.get("purchasedCredits").and_then(optional_number);
    let free = credits.get("freeCredits").and_then(optional_number);
    if monthly.is_none() && purchased.is_none() && free.is_none() {
        return Err(PluginError::decode("no balance in the reply"));
    }
    let monthly = monthly.unwrap_or(0.0).max(0.0);
    let left = monthly + purchased.unwrap_or(0.0).max(0.0) + free.unwrap_or(0.0).max(0.0);
    let mut windows = command_windows(&reply);
    let has_windows = !windows.is_empty();
    let mut plan_id = credits
        .get("planId")
        .and_then(Value::as_str)
        .unwrap_or_default()
        .to_owned();
    let mut until = None;
    if plan_id.is_empty() && has_windows {
        let cached = reader
            .command_plans
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .get(query.api_key())
            .cloned();
        let fresh = cached.as_ref().is_some_and(|cached| {
            query.now().duration_since(cached.at).unwrap_or_default() < COMMAND_PLAN_KEEP
        });
        if let Some(cached) = cached.clone().filter(|_| fresh) {
            plan_id = cached.id;
            until = cached.until;
        } else {
            let (id, end, _renew) = read_subscription(query, host, subscriptions_url).await;
            plan_id = cached.as_ref().map_or(id.clone(), |old| {
                if id.is_empty() {
                    old.id.clone()
                } else {
                    id.clone()
                }
            });
            until = if end.is_some() {
                end
            } else {
                cached.as_ref().and_then(|old| old.until)
            };
            if !id.is_empty() {
                reader
                    .command_plans
                    .lock()
                    .unwrap_or_else(std::sync::PoisonError::into_inner)
                    .insert(
                        query.api_key().to_owned(),
                        crate::command_code::CachedPlan {
                            id,
                            until,
                            renew: String::new(),
                            at: query.now(),
                        },
                    );
            }
        }
    }
    let (plan_name, plan_monthly) = command_plan(&plan_id);
    if plan_monthly > 0.0 {
        let pool = plan_monthly.max(monthly) + left - monthly;
        if pool > 0.0 {
            let spent = (pool - left).max(0.0);
            windows.push(
                PlanWindow::new("Credits", (100.0 * spent / pool).min(100.0))
                    .with_count(spent, pool, "USD")
                    .with_display(format!("{} / {}", money("$", spent), money("$", pool)))
                    .with_optional_reset(until.filter(|until| *until > query.now())),
            );
        }
    }
    let plan_windows = if has_windows { windows.clone() } else { windows };
    Ok(BalanceCard::pack(
        money("$", left),
        Vec::new(),
        Vec::new(),
        vec![BalanceWallet::new(left, "USD")],
        Vec::new(),
        if has_windows { plan_name } else { String::new() },
        plan_windows,
        until,
        String::new(),
        true,
    ))
}

async fn read_subscription(
    query: &BalanceQuery,
    host: &HostContext,
    url: &str,
) -> (String, Option<SystemTime>, String) {
    let Ok((status, body)) = send(query, host, false, url, "").await else {
        return (String::new(), None, String::new());
    };
    if status >= 300 {
        return (String::new(), None, String::new());
    }
    let Ok(reply) = json_body(&body) else {
        return (String::new(), None, String::new());
    };
    if reply.get("success").and_then(Value::as_bool) == Some(false) {
        return (String::new(), None, String::new());
    }
    let Some(data) = reply.get("data") else {
        return (String::new(), None, String::new());
    };
    let status = data
        .get("status")
        .and_then(Value::as_str)
        .unwrap_or_default();
    if matches!(status, "canceled" | "incomplete_expired") {
        return (String::new(), None, String::new());
    }
    (
        data.get("planId")
            .and_then(Value::as_str)
            .unwrap_or_default()
            .to_owned(),
        data.get("currentPeriodEnd").and_then(|value| at_time(value)),
        String::new(),
    )
}

fn parse_ollama(body: &[u8]) -> Result<BalanceCard, PluginError> {
    let reply = json_body(body)?;
    let included = reply.get("included");
    let purchased = reply.get("purchased");
    if included.is_none() && purchased.is_none() {
        return Err(PluginError::decode(
            "Ollama's reply has no included or purchased balance",
        ));
    }
    let bought = purchased
        .and_then(|value| value.get("balance_usd"))
        .and_then(optional_number);
    let text = bought.map(|amount| money("$", amount));
    let wallets = bought
        .map(|amount| vec![BalanceWallet::new(amount, "USD")])
        .unwrap_or_default();
    let credits_left = bought.is_some_and(|amount| amount > 0.0);
    let mut plan_windows = Vec::new();
    if let Some(included) = included {
        for (key, name, span) in [
            ("session", "Session limit", None),
            ("weekly", "Weekly", Some(Duration::from_secs(7 * 24 * 3600))),
        ] {
            let Some(limit) = included.get(key) else {
                continue;
            };
            let Some(left) = limit.get("remaining_percent").and_then(optional_number) else {
                continue;
            };
            plan_windows.push({
                let mut window = PlanWindow::new(name, (100.0 - left).clamp(0.0, 100.0))
                    .with_optional_reset(limit.get("resets_at").and_then(at_time))
                    .with_blocking(!credits_left);
                if let Some(span) = span {
                    window = window.with_span(span);
                }
                window
            });
        }
        let allowance = included.get("allowance_usd").and_then(optional_number);
        let left = included.get("balance_usd").and_then(optional_number);
        if let (Some(allowance), Some(left)) = (allowance, left)
            && allowance > 0.0
        {
            let used = (allowance - left).clamp(0.0, allowance);
            let period = included.get("period");
            let from = period
                .and_then(|period| period.get("from"))
                .and_then(at_time);
            let until = period
                .and_then(|period| period.get("until"))
                .and_then(at_time);
            let span = from
                .zip(until)
                .and_then(|(from, until)| until.duration_since(from).ok());
            let monthly = span.is_some_and(|span| {
                let days = span.as_secs_f64() / 86_400.0;
                (27.0..=32.0).contains(&days)
            });
            plan_windows.push({
                let mut window = PlanWindow::new(
                    if monthly { "Month" } else { "Allowance" },
                    (100.0 * used / allowance).min(100.0),
                )
                .with_count(used, allowance, "USD")
                .with_optional_reset(until)
                .with_blocking(!credits_left);
                if let Some(span) = span {
                    window = window.with_span(span);
                }
                window
            });
        }
    }
    Ok(BalanceCard::pack(
        text.unwrap_or_default(),
        Vec::new(),
        Vec::new(),
        wallets,
        Vec::new(),
        String::new(),
        plan_windows,
        None,
        String::new(),
        true,
    ))
}

fn command_windows(reply: &Value) -> Vec<PlanWindow> {
    let Some(windows) = reply.get("windowLimits") else {
        return Vec::new();
    };
    let mut out = Vec::new();
    for (key, name, seconds) in [
        ("fiveHour", "5 hours", 5 * 3600),
        ("weekly", "Weekly", 7 * 24 * 3600),
    ] {
        let Some(window) = windows.get(key) else {
            continue;
        };
        let (Some(used), Some(cap)) = (
            window.get("used").and_then(optional_number),
            window.get("cap").and_then(optional_number),
        ) else {
            continue;
        };
        if cap <= 0.0 {
            continue;
        }
        let used = used.max(0.0);
        out.push(
            PlanWindow::new(name, (100.0 * used / cap).min(100.0))
                .with_count(used, cap, "USD")
                .with_optional_reset(window.get("resetAt").and_then(at_time))
                .with_span(Duration::from_secs(seconds)),
        );
    }
    out
}

fn command_plan(id: &str) -> (String, f64) {
    let id = id.to_lowercase().replace('_', "-");
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
            return (name.to_owned(), monthly);
        }
    }
    (String::new(), 0.0)
}

fn balance_authorization(token: &str) -> String {
    let token = token.trim();
    if token.contains(' ') {
        return token.to_owned();
    }
    if token.starts_with("eyJ") && token.matches('.').count() == 2 {
        return format!("Bearer {token}");
    }
    token.to_owned()
}

fn with_key(text: &str, key: &str, in_url: bool) -> String {
    let replacement = if in_url {
        percent_encode(key)
    } else {
        key.to_owned()
    };
    let mut out = String::new();
    let mut rest = text;
    while let Some(start) = rest.find('{') {
        out.push_str(&rest[..start]);
        let candidate = &rest[start..];
        let Some(end) = candidate.find('}') else {
            out.push_str(candidate);
            return out;
        };
        let token = candidate[..=end].trim_start_matches('$');
        let normalized = token
            .trim_matches(|character| matches!(character, '{' | '}' | ' '))
            .to_lowercase();
        if matches!(
            normalized.as_str(),
            "key" | "apikey" | "api_key" | "api-key" | "token"
        ) {
            out.push_str(&replacement);
        } else {
            out.push_str(&candidate[..=end]);
        }
        rest = &candidate[end + 1..];
    }
    out.push_str(rest);
    out
}

fn percent_encode(value: &str) -> String {
    let mut out = String::new();
    for byte in value.bytes() {
        if byte.is_ascii_alphanumeric() || matches!(byte, b'-' | b'_' | b'.' | b'~') {
            out.push(char::from(byte));
        } else if byte == b' ' {
            out.push('+');
        } else {
            out.push_str(&format!("%{byte:02X}"));
        }
    }
    out
}

fn open_code_zen(path: &str) -> bool {
    !path.contains("/go") && (path.contains("/zen") || path.contains("/inference"))
}

fn retired_siliconflow(status: u16, body: &[u8]) -> bool {
    status == 410
        || json_body(body)
            .ok()
            .and_then(|reply| reply.get("code").and_then(Value::as_i64))
            == Some(20092)
}

fn balance_refusal(body: &[u8]) -> Option<String> {
    let reply = json_body(body).ok()?;
    if reply.get("success").and_then(Value::as_bool) != Some(false) {
        return None;
    }
    reply
        .get("message")
        .and_then(Value::as_str)
        .map(str::trim)
        .filter(|message| !message.is_empty())
        .map(str::to_owned)
}

fn user_header_hint(message: &str) -> String {
    if !message.to_lowercase().contains("new-api-user") {
        return message.to_owned();
    }
    format!(
        "add the header New-Api-User = your user ID (shown in the site's personal settings) to this provider's Headers; the relay said: {message}"
    )
}

fn at(body: &[u8], path: &[&str]) -> Result<Value, PluginError> {
    let mut value = json_body(body)?;
    for segment in path {
        value = value
            .get(*segment)
            .cloned()
            .ok_or_else(|| PluginError::decode("no balance in the reply"))?;
    }
    Ok(value)
}

fn optional_number(value: &Value) -> Option<f64> {
    number(Some(value))
}

fn required_number(value: &Value) -> Result<f64, PluginError> {
    optional_number(value).ok_or_else(|| PluginError::decode("no balance in the reply"))
}

fn money(sign: &str, value: f64) -> String {
    format!("{sign}{value:.2}")
}

fn currency_sign(code: &str) -> &str {
    match code.to_uppercase().as_str() {
        "CNY" | "RMB" => "¥",
        "USD" => "$",
        _ => "",
    }
}

fn currency_unit(sign: &str) -> &str {
    match sign {
        "$" => "USD",
        "¥" => "CNY",
        "€" => "EUR",
        "£" => "GBP",
        _ => "",
    }
}

#[cfg(test)]
mod balance_tests {
    use super::*;
    use hofund_plugin_api::BalanceQuery;

    #[test]
    fn p3a_deepseek_orders_currencies_and_splits_granted_credit() {
        let card = parse_deepseek(
            br#"{"balance_infos":[{"currency":"CNY","total_balance":"-0.05","granted_balance":"1.00","topped_up_balance":"-1.05"},{"currency":"USD","total_balance":"11.12","granted_balance":"1.12","topped_up_balance":"10.00"}]}"#,
        )
        .expect("balance");
        assert_eq!(card.text(), "$11.12 · ¥-0.05");
        assert_eq!(card.wallets()[0].unit(), "USD");
        assert_eq!(card.split().len(), 4);
        assert!(card.account_wide());
    }

    #[test]
    fn p3c_openrouter_subtracts_usage_from_credits() {
        let body = br#"{"data":{"total_credits":20,"total_usage":3.5}}"#;
        let credits = required_number(&at(body, &["data", "total_credits"]).expect("credits"))
            .expect("number");
        let used =
            optional_number(&at(body, &["data", "total_usage"]).expect("usage")).unwrap_or(0.0);
        let card = single_balance(money("$", credits - used), credits - used, "USD", true);
        assert_eq!(card.text(), "$16.50");
    }

    #[test]
    fn p3g_ollama_separates_included_allowance_from_bought_credits() {
        let card = parse_ollama(
            br#"{"included":{"balance_usd":72.5,"allowance_usd":100,"period":{"from":"2026-10-15T09:30:00Z","until":"2026-11-15T09:30:00Z"}},"purchased":{"balance_usd":25}}"#,
        )
        .expect("balance");
        assert_eq!(card.text(), "$25.00");
        assert!(!card.plan_windows().is_empty());
    }

    #[test]
    fn p3m_opencode_zen_says_no_key_balance_without_network() {
        let query = BalanceQuery::new(vec!["https://opencode.ai/zen/v1".to_owned()], "key");
        assert_eq!(support(&query), BalanceSupport::Certain);
    }
}
