//! Antigravity account setup and the Cloud project stored with the sign-in.

use std::time::Duration;

use hofund_plugin_api::{AccountId, HostContext, PluginError, SecretString};
use hofund_plugin_sdk::Call;
use hofund_plugin_sdk::json::{self, Value, json};
use hofund_plugin_sdk::oauth::SecretObject;

use crate::{Endpoints, version};

pub(crate) const PROJECT: &str = "project";
pub(crate) const PLAN: &str = "plan";
pub(crate) const TIER: &str = "tier";
pub(crate) const PAID: &str = "paid";
pub(crate) const PENDING_REASON: &str =
    "Antigravity has not set this account up yet; refresh the account to try again";
const ONBOARD_ASKS: usize = 5;
const LEASE_WAIT: Duration = Duration::from_secs(30);

pub(crate) fn known(blob: &SecretObject, configured: Option<&str>) -> bool {
    blob.text(PROJECT).is_some() || configured.is_some()
}

#[derive(Debug, PartialEq, Eq)]
pub(crate) struct Setup {
    pub(crate) id: String,
    pub(crate) plan: String,
    pub(crate) tier: String,
    pub(crate) paid: String,
}

impl Setup {
    fn apply(&self, blob: SecretObject) -> SecretObject {
        blob.with_text(PROJECT, &self.id)
            .with_text(PLAN, &self.plan)
            .with_text(TIER, &self.tier)
            .with_text(PAID, &self.paid)
    }
}

pub(crate) async fn settle(
    host: &HostContext,
    endpoints: &Endpoints,
    access: &SecretString,
    wanted: Option<&str>,
    blob: SecretObject,
) -> SecretObject {
    match setup(host, endpoints, access, wanted).await {
        Ok(outcome) => outcome.apply(blob),
        Err(_) => blob,
    }
}

pub(crate) async fn ensure(
    host: &HostContext,
    endpoints: &Endpoints,
    account: &AccountId,
    access: &SecretString,
    wanted: Option<&str>,
) -> Result<(), PluginError> {
    let secret =
        host.accounts().secret(account).await?.ok_or_else(|| {
            PluginError::auth("no Antigravity sign-in is stored for this account")
        })?;
    if SecretObject::parse(&secret)?.text(PROJECT).is_some() {
        return Ok(());
    }
    let outcome = setup(host, endpoints, access, wanted).await?;
    let lease = host.accounts().lease(account, LEASE_WAIT).await?;
    let current = SecretObject::parse(lease.secret())?;
    if current.text(PROJECT).is_some() {
        return Ok(());
    }
    lease
        .commit(None, outcome.apply(current).to_secret())
        .await?;
    Ok(())
}

pub(crate) async fn resolve(
    host: &HostContext,
    endpoints: &Endpoints,
    account: &AccountId,
    access: &SecretString,
    configured: Option<&str>,
) -> Result<String, PluginError> {
    if let Some(project) = stored(host, account).await? {
        return Ok(project);
    }
    if let Some(project) = configured {
        return Ok(project.to_owned());
    }
    ensure(host, endpoints, account, access, None).await?;
    stored(host, account)
        .await?
        .ok_or_else(|| PluginError::auth(PENDING_REASON))
}

async fn stored(host: &HostContext, account: &AccountId) -> Result<Option<String>, PluginError> {
    let secret =
        host.accounts().secret(account).await?.ok_or_else(|| {
            PluginError::auth("no Antigravity sign-in is stored for this account")
        })?;
    Ok(SecretObject::parse(&secret)?
        .text(PROJECT)
        .map(str::to_owned))
}

pub(crate) async fn set(
    host: &HostContext,
    email: &str,
    project: &str,
) -> Result<AccountId, PluginError> {
    let email = email.trim();
    let account = host
        .accounts()
        .list()
        .await?
        .into_iter()
        .find(|account| account.label().eq_ignore_ascii_case(email))
        .ok_or_else(|| PluginError::config(format!("no antigravity account `{email}`")))?;
    let id = account.id().clone();
    let lease = host.accounts().lease(&id, LEASE_WAIT).await?;
    let blob = SecretObject::parse(lease.secret())?
        .without(PROJECT)
        .with_text("email", lease.account().label());
    let project = project.trim();
    let next = if project.is_empty() {
        blob
    } else {
        blob.with_text(PROJECT, project)
    };
    lease.commit(None, next.to_secret()).await?;
    Ok(id)
}

pub(crate) async fn setup(
    host: &HostContext,
    endpoints: &Endpoints,
    access: &SecretString,
    wanted: Option<&str>,
) -> Result<Setup, PluginError> {
    let load = call(
        host,
        endpoints,
        &endpoints.load_code_assist,
        access,
        "loadCodeAssist",
        &json!({"metadata": {"ideType": "ANTIGRAVITY"}}),
        &[],
    )
    .await?;
    let plan = plan(&load);
    let (tier, paid) = tiers(&load);
    if let Some(id) = companion(load.get("cloudaicompanionProject")) {
        return Ok(Setup {
            id,
            plan,
            tier,
            paid,
        });
    }
    if let Some(id) = wanted {
        return Ok(Setup {
            id: id.to_owned(),
            plan,
            tier,
            paid,
        });
    }
    let mut tier = default_tier(&load);
    if tier.id.is_empty() || tier.id == "legacy-tier" {
        tier.id = "free-tier".to_owned();
    }
    let hub_version = version::resolve(host, &endpoints.version_manifest).await;
    let request = json!({
        "tier_id": tier.id,
        "metadata": {
            "ide_type": "ANTIGRAVITY",
            "ide_version": hub_version,
            "ide_name": "antigravity",
        }
    });
    let headers = [
        (
            "user-agent".to_owned(),
            format!(
                "{} google-api-nodejs-client/10.3.0",
                version::user_agent_for(&hub_version)
            ),
        ),
        ("x-goog-api-client".to_owned(), "gl-node/22.21.1".to_owned()),
    ];
    let id = onboard(host, endpoints, access, &request, &headers).await?;
    if id.is_empty() {
        let why = load
            .get("ineligibleTiers")
            .and_then(Value::as_array)
            .into_iter()
            .flatten()
            .filter_map(|tier| tier.get("reasonMessage").and_then(Value::as_str))
            .find(|why| !why.is_empty())
            .unwrap_or_default();
        let message = if why.is_empty() {
            "Antigravity hasn't set this Google account up (it gave no project); sign in to the Antigravity app with it once, then try again".to_owned()
        } else {
            format!("Antigravity won't serve this Google account: {why}")
        };
        return Err(PluginError::auth(message));
    }
    Ok(Setup {
        id,
        plan: tier.name,
        tier: tier.id,
        paid,
    })
}

#[derive(Debug)]
struct Tier {
    id: String,
    name: String,
}

impl Tier {
    fn read(value: &Value) -> Self {
        let text = |key: &str| {
            value
                .get(key)
                .and_then(Value::as_str)
                .unwrap_or_default()
                .to_owned()
        };
        Self {
            id: text("id"),
            name: text("name"),
        }
    }
}

fn default_tier(load: &Value) -> Tier {
    let allowed = load
        .get("allowedTiers")
        .and_then(Value::as_array)
        .map(Vec::as_slice)
        .unwrap_or_default();
    allowed
        .iter()
        .find(|tier| tier.get("isDefault").and_then(Value::as_bool) == Some(true))
        .or_else(|| allowed.first())
        .map_or(
            Tier {
                id: "legacy-tier".to_owned(),
                name: String::new(),
            },
            Tier::read,
        )
}

fn plan(load: &Value) -> String {
    let named = |key: &str| {
        load.get(key)
            .filter(|tier| tier.is_object())
            .map(Tier::read)
    };
    let tier = match named("paidTier") {
        Some(paid) if !paid.name.is_empty() => Some(paid),
        _ => named("currentTier"),
    };
    tier.map_or(String::new(), |tier| {
        if tier.name.is_empty() {
            tier.id
        } else {
            tier.name
        }
    })
}

fn tiers(load: &Value) -> (String, String) {
    (
        load.get("currentTier")
            .map(Tier::read)
            .map_or(String::new(), |tier| tier.id),
        load.get("paidTier")
            .map(Tier::read)
            .map_or(String::new(), |tier| tier.id),
    )
}

pub(crate) fn tier_ids(load: &Value) -> (String, String) {
    tiers(load)
}

fn companion(value: Option<&Value>) -> Option<String> {
    let id = match value? {
        Value::String(id) => id.as_str(),
        Value::Object(project) => project.get("id").and_then(Value::as_str)?,
        _ => return None,
    };
    (!id.is_empty()).then(|| id.to_owned())
}

async fn onboard(
    host: &HostContext,
    endpoints: &Endpoints,
    access: &SecretString,
    request: &Value,
    headers: &[(String, String)],
) -> Result<String, PluginError> {
    for ask in 0..ONBOARD_ASKS {
        if ask > 0 {
            hofund_plugin_sdk::sleep(endpoints.onboard_poll).await;
        }
        let reply = call(
            host,
            endpoints,
            &endpoints.code_assist,
            access,
            "onboardUser",
            request,
            headers,
        )
        .await?;
        if reply.get("done").and_then(Value::as_bool) == Some(true) {
            return Ok(companion(
                reply
                    .get("response")
                    .and_then(|response| response.get("cloudaicompanionProject")),
            )
            .unwrap_or_default());
        }
    }
    Err(PluginError::timeout(
        "setting the account up for Code Assist is taking long; try again in a minute",
    ))
}

pub(crate) async fn call(
    host: &HostContext,
    endpoints: &Endpoints,
    base: &str,
    access: &SecretString,
    method: &str,
    body: &Value,
    headers: &[(String, String)],
) -> Result<Value, PluginError> {
    let hub_version = version::resolve(host, &endpoints.version_manifest).await;
    let mut call = Call::post(format!("{base}/v1internal:{method}"))
        .bearer(access)?
        .header("user-agent", version::user_agent_for(&hub_version))
        .json(body.to_string());
    for (name, value) in headers {
        call = call.header(name, value);
    }
    let reply = call.fetch(host.http()).await?;
    let status = reply.status();
    if status != 200 {
        let message = format!("Code Assist {method} refused the request: HTTP {status}");
        return Err(match status {
            401 => PluginError::auth(message),
            429 => PluginError::quota_status(status, message),
            _ => PluginError::status_error(status, message),
        });
    }
    json::from_slice(reply.body())
        .map_err(|_| PluginError::decode(format!("Code Assist {method} sent back unreadable JSON")))
}
