//! Browser polling and API-key account import.

use std::future::Future;
use std::time::{Duration, UNIX_EPOCH};

use hofund_plugin_api::{
    AccountId, HostContext, PluginError, SecretString, SignInChallenge, SignInCtx,
};
use hofund_plugin_sdk::json::Value;
use hofund_plugin_sdk::oauth::secret_text;
use hofund_plugin_sdk::transport::Reply;
use hofund_plugin_sdk::{Call, url};

use crate::Endpoints;
use crate::creds::{self, Credential};
use crate::discover;
use crate::site::Site;

const PAGE_VERSION: &str = "2.0.0";
const POLL_INTERVAL: Duration = Duration::from_secs(1);
const SIGN_IN_TIMEOUT: Duration = Duration::from_secs(5 * 60);
const TOKEN_PENDING: i64 = 11217;
const ACCOUNT_PENDING: i64 = 12151;

#[derive(Clone)]
pub(crate) struct Pending {
    site: Site,
    state: String,
}

pub(crate) fn site(ctx: &SignInCtx) -> Result<Site, PluginError> {
    Site::from_sign_in_option(ctx.option("site"))
}

pub(crate) async fn start(
    ctx: &SignInCtx,
    host: &HostContext,
    endpoints: &Endpoints,
) -> Result<(Pending, SignInChallenge), PluginError> {
    let site = site(ctx)?;
    let url = request_url(
        endpoints.base_for(site),
        "/v2/plugin/auth/state",
        &[("platform", site.plugin_id())],
    )?;
    let reply = Call::post(url)
        .header("Content-Type", "application/json")
        .header("Accept", "application/json")
        .header("User-Agent", site.user_agent())
        .header("X-No-Authorization", "true")
        .header("X-No-User-Id", "true")
        .header("X-No-Enterprise-Id", "true")
        .header("X-No-Department-Info", "true")
        .bytes(b"{}")
        .fetch(host.http())
        .await?;
    let data = response_data(reply)?;
    let state = text(&data, "state")
        .ok_or_else(|| PluginError::auth(format!("{} gave no sign-in state", site.name())))?;
    let auth_url = text(&data, "authUrl")
        .ok_or_else(|| PluginError::auth(format!("{} gave no sign-in page", site.name())))?;
    let mut page = url::Url::parse(auth_url)
        .map_err(|_| PluginError::auth(format!("{} gave an invalid sign-in page", site.name())))?;
    if page.scheme() != "https" || page.host_str().is_none() {
        return Err(PluginError::auth(format!(
            "{}'s sign-in page isn't https",
            site.name()
        )));
    }
    let login_session = hofund_plugin_sdk::uuid::v4()
        .map_err(|error| {
            PluginError::internal("sign-in session generation failed").with_source(error)
        })?
        .replace('-', "");
    page.query_pairs_mut()
        .append_pair("version", PAGE_VERSION)
        .append_pair("loginSessionId", &login_session);
    let challenge = SignInChallenge::instructions(format!(
        "Open {} in a browser to sign in to {}. Paste an API key from {} instead, or press Enter to wait for browser sign-in.",
        page,
        site.name(),
        site.keys_page()
    ))?;
    Ok((
        Pending {
            site,
            state: state.to_owned(),
        },
        challenge,
    ))
}

pub(crate) async fn import_api_key(
    ctx: &SignInCtx,
    host: &HostContext,
    key: SecretString,
) -> Result<AccountId, PluginError> {
    let site = site(ctx)?;
    let key_text = secret_text(&key).trim();
    if key_text.is_empty() {
        return Err(PluginError::auth("the API key is empty"));
    }
    let suffix = key_text
        .chars()
        .rev()
        .take(8)
        .collect::<Vec<_>>()
        .into_iter()
        .rev()
        .collect::<String>();
    let credential = Credential {
        uid: format!("anonymous_{suffix}"),
        access: SecretString::from(key_text.to_owned()),
        refresh: SecretString::from(String::new()),
        expires_at: 0,
        refresh_expires_at: 0,
        domain: String::new(),
        token_type: "Bearer".to_owned(),
        label: format!("{} API key", site.name()),
        site,
        desktop: false,
        api_key: true,
        enterprise_id: String::new(),
        auth_path: String::new(),
    };
    discover::store(&credential, host).await
}

pub(crate) async fn finish(
    host: &HostContext,
    endpoints: &Endpoints,
    pending: Pending,
    alive: impl Fn() -> bool,
) -> Result<AccountId, PluginError> {
    let base = endpoints.base_for(pending.site);
    let deadline = host.clock().monotonic() + SIGN_IN_TIMEOUT;
    let token_url = request_url(
        base,
        "/v2/plugin/auth/token",
        &[("state", pending.state.as_str())],
    )?;
    let token = poll(host, deadline, TOKEN_PENDING, &alive, || {
        Call::get(token_url.clone())
            .header("Content-Type", "application/json")
            .header("Accept", "application/json")
            .header("User-Agent", pending.site.user_agent())
            .fetch(host.http())
    })
    .await?;
    let mut credential = credential_from_token(host, pending.site, &token)?;
    let account_url = request_url(
        base,
        "/v2/plugin/login/account",
        &[("state", pending.state.as_str())],
    )?;
    let who = poll(host, deadline, ACCOUNT_PENDING, &alive, || {
        signed_poll_call(account_url.clone(), &credential, base, true).fetch(host.http())
    })
    .await?;
    credential.uid = text(&who, "uid")
        .filter(|uid| !uid.is_empty())
        .ok_or_else(|| PluginError::auth("browser sign-in did not name the account"))?
        .to_owned();
    let enterprise = enterprise_of(host, base, &credential, &who).await;
    credential.label = account_name(&who, &enterprise);
    credential.enterprise_id = enterprise.id;
    discover::store(&credential, host).await
}

async fn poll<F, Fut>(
    host: &HostContext,
    deadline: Duration,
    waiting_code: i64,
    alive: &impl Fn() -> bool,
    mut send: F,
) -> Result<Value, PluginError>
where
    F: FnMut() -> Fut,
    Fut: Future<Output = Result<Reply, PluginError>>,
{
    loop {
        if !alive() {
            return Err(PluginError::config("sign-in was cancelled"));
        }
        if host.clock().monotonic() >= deadline {
            return Err(PluginError::auth("browser sign-in timed out"));
        }
        match poll_step(send().await?, waiting_code) {
            PollStep::Data(data) => return Ok(data),
            PollStep::Wait => hofund_plugin_sdk::sleep(POLL_INTERVAL).await,
            PollStep::Fail(error) => return Err(error),
        }
    }
}

enum PollStep {
    Data(Value),
    Wait,
    Fail(PluginError),
}

fn poll_step(reply: Reply, waiting_code: i64) -> PollStep {
    let status = reply.status();
    if status == 408 || status == 429 {
        return PollStep::Wait;
    }
    if !(200..300).contains(&status) {
        return PollStep::Fail(PluginError::status_error(
            status,
            "browser sign-in request failed",
        ));
    }
    let Ok(root) = hofund_plugin_sdk::json::from_slice::<Value>(reply.body()) else {
        return PollStep::Fail(PluginError::decode("browser sign-in returned invalid JSON"));
    };
    let code = root.get("code").and_then(json_i64).unwrap_or(0);
    if code == waiting_code {
        return PollStep::Wait;
    }
    if code != 0 {
        return PollStep::Fail(PluginError::auth(format!(
            "browser sign-in was refused (code {code})"
        )));
    }
    let data = root.get("data").cloned().unwrap_or(Value::Null);
    if data.as_object().is_some_and(|object| object.is_empty()) || data.is_null() {
        PollStep::Wait
    } else {
        PollStep::Data(data)
    }
}

fn response_data(reply: Reply) -> Result<Value, PluginError> {
    match poll_step(reply, -1) {
        PollStep::Data(data) => Ok(data),
        PollStep::Wait => Err(PluginError::decode(
            "browser sign-in returned an empty response",
        )),
        PollStep::Fail(error) => Err(error),
    }
}

fn credential_from_token(
    host: &HostContext,
    site: Site,
    token: &Value,
) -> Result<Credential, PluginError> {
    let access = text(token, "accessToken")
        .filter(|token| !token.is_empty())
        .ok_or_else(|| PluginError::auth("browser sign-in sent back no access token"))?;
    let now_ms = host
        .clock()
        .now()
        .duration_since(UNIX_EPOCH)
        .map(|duration| duration.as_millis() as i64)
        .unwrap_or(0);
    Ok(Credential {
        uid: String::new(),
        access: SecretString::from(access.to_owned()),
        refresh: SecretString::from(text(token, "refreshToken").unwrap_or("").to_owned()),
        expires_at: expiry(token, "expiresAt", "expiresIn", now_ms),
        refresh_expires_at: expiry(token, "refreshExpiresAt", "refreshExpiresIn", now_ms),
        domain: text(token, "domain").unwrap_or("").to_owned(),
        token_type: text(token, "tokenType").unwrap_or("Bearer").to_owned(),
        label: site.name().to_owned(),
        site,
        desktop: false,
        api_key: false,
        enterprise_id: String::new(),
        auth_path: String::new(),
    })
}

fn expiry(token: &Value, absolute: &str, relative: &str, now_ms: i64) -> i64 {
    token
        .get(absolute)
        .and_then(json_i64)
        .filter(|value| *value > 0)
        .or_else(|| {
            token
                .get(relative)
                .and_then(json_i64)
                .filter(|value| *value > 0)
                .map(|value| now_ms + value.saturating_mul(1_000))
        })
        .unwrap_or(0)
}

fn signed_poll_call(url: String, credential: &Credential, base: &str, account: bool) -> Call {
    let mut call = Call::get(url)
        .header("Content-Type", "application/json")
        .header("Accept", "application/json")
        .header("User-Agent", credential.site.user_agent())
        .sensitive_header(
            "Authorization",
            format!("Bearer {}", secret_text(&credential.access)),
        )
        .header("X-Domain", creds::domain(credential, base));
    if account {
        call = call
            .header("X-No-User-Id", "true")
            .header("X-No-Enterprise-Id", "true");
    }
    call
}

struct Enterprise {
    id: String,
    name: String,
}

async fn enterprise_of(
    host: &HostContext,
    base: &str,
    credential: &Credential,
    who: &Value,
) -> Enterprise {
    let own = text(who, "enterpriseId").unwrap_or("").trim().to_owned();
    let kind = text(who, "type").unwrap_or("").trim().to_ascii_lowercase();
    if own.is_empty() && (kind.is_empty() || kind == "personal") {
        return Enterprise {
            id: String::new(),
            name: String::new(),
        };
    }
    let own_name = text(who, "enterpriseName").unwrap_or("").trim();
    if !own.is_empty() && !own_name.is_empty() {
        return Enterprise {
            id: own,
            name: own_name.to_owned(),
        };
    }
    let Ok(url) = request_url(base, "/v2/plugin/accounts", &[]) else {
        return Enterprise {
            id: own,
            name: String::new(),
        };
    };
    let Ok(reply) = signed_poll_call(url, credential, base, false)
        .fetch(host.http())
        .await
    else {
        return Enterprise {
            id: own,
            name: String::new(),
        };
    };
    let Ok(data) = response_data(reply) else {
        return Enterprise {
            id: own,
            name: String::new(),
        };
    };
    let empty = Vec::new();
    let accounts = data
        .get("accounts")
        .and_then(Value::as_array)
        .unwrap_or(&empty);
    let uid = text(who, "uid").unwrap_or("");
    let mine = accounts
        .iter()
        .filter(|account| text(account, "uid") == Some(uid))
        .filter(|account| text(account, "enterpriseId").is_some_and(|id| !id.trim().is_empty()))
        .collect::<Vec<_>>();
    let selected = if !own.is_empty() {
        mine.iter()
            .find(|account| text(account, "enterpriseId").is_some_and(|id| id.trim() == own))
            .copied()
    } else {
        let same = mine
            .iter()
            .filter(|account| {
                text(account, "type")
                    .unwrap_or("")
                    .trim()
                    .eq_ignore_ascii_case(&kind)
            })
            .copied()
            .collect::<Vec<_>>();
        let last = mine
            .iter()
            .filter(|account| {
                account
                    .get("lastLogin")
                    .and_then(Value::as_bool)
                    .unwrap_or(false)
            })
            .copied()
            .collect::<Vec<_>>();
        if same.len() == 1 {
            same.first().copied()
        } else if last.len() == 1 {
            last.first().copied()
        } else if mine.len() == 1 {
            mine.first().copied()
        } else {
            None
        }
    };
    let id = selected
        .and_then(|account| text(account, "enterpriseId"))
        .unwrap_or(&own)
        .trim()
        .to_owned();
    let name = selected
        .and_then(|account| text(account, "enterpriseName"))
        .unwrap_or("")
        .trim()
        .to_owned();
    Enterprise { id, name }
}

fn account_name(who: &Value, enterprise: &Enterprise) -> String {
    let person = text(who, "nickname")
        .or_else(|| text(who, "phoneNumber"))
        .or_else(|| text(who, "uid"))
        .unwrap_or("");
    if enterprise.id.is_empty() {
        return person.to_owned();
    }
    let team = if enterprise.name.is_empty() {
        let suffix = enterprise
            .id
            .chars()
            .rev()
            .take(6)
            .collect::<Vec<_>>()
            .into_iter()
            .rev()
            .collect::<String>();
        format!("enterprise {suffix}")
    } else {
        enterprise.name.clone()
    };
    if person.is_empty() {
        team
    } else {
        format!("{person} · {team}")
    }
}

fn request_url(base: &str, path: &str, query: &[(&str, &str)]) -> Result<String, PluginError> {
    let mut url = url::Url::parse(&format!("{base}{path}"))
        .map_err(|_| PluginError::config("the sign-in endpoint is invalid"))?;
    url.query_pairs_mut().extend_pairs(query.iter().copied());
    Ok(url.to_string())
}

fn text<'a>(value: &'a Value, key: &str) -> Option<&'a str> {
    value.get(key).and_then(Value::as_str)
}

fn json_i64(value: &Value) -> Option<i64> {
    value
        .as_i64()
        .or_else(|| value.as_u64().map(|number| number as i64))
        .or_else(|| value.as_str().and_then(|text| text.parse().ok()))
}
