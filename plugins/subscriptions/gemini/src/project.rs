//! Setting the account up for Code Assist, as Gemini CLI's setup does, and
//! the Google Cloud project its requests go to.

use std::time::Duration;

use hofund_plugin_api::{AccountId, HostContext, PluginError, SecretString};
use hofund_plugin_sdk::Call;
use hofund_plugin_sdk::json::{self, Value, json};
use hofund_plugin_sdk::oauth::SecretObject;

use crate::{Endpoints, GEMINI_CLI_VERSION};

/// The stored project key.
pub(crate) const PROJECT: &str = "project";
/// The stored plan name, when Code Assist gave one.
pub(crate) const PLAN: &str = "plan";
/// Set when Code Assist needs a project named for this account.
pub(crate) const NEEDS_PROJECT: &str = "needs_project";
/// Google's reason the account needs a project, when it gave one.
const NEEDS_PROJECT_WHY: &str = "needs_project_why";

/// What a Code Assist Standard or Enterprise account without a project is told.
pub(crate) const NEEDS_PROJECT_REASON: &str = "Google serves Gemini CLI's sign-in only to Gemini \
     Code Assist Standard and Enterprise, which bill a Google Cloud project: name it as `project` \
     in this subscription's config";

/// What an account Code Assist has not set up yet is told.
pub(crate) const PENDING_REASON: &str =
    "Code Assist has not set this account up yet; refresh the account to try again";

/// How many times onboarding asks before it gives up.
const ONBOARD_ASKS: usize = 5;

const LEASE_WAIT: Duration = Duration::from_secs(30);

/// The project requests go to: the one Code Assist set up, else the one the
/// config names.
pub(crate) fn known(blob: &SecretObject, configured: Option<&str>) -> bool {
    blob.text(PROJECT).is_some() || configured.is_some()
}

/// Why an account has no project.
pub(crate) fn missing_reason(blob: &SecretObject) -> String {
    if !blob.flag(NEEDS_PROJECT) {
        return PENDING_REASON.to_owned();
    }
    match blob.text(NEEDS_PROJECT_WHY) {
        Some(why) if !why.is_empty() => format!("{why}: {NEEDS_PROJECT_REASON}"),
        _ => NEEDS_PROJECT_REASON.to_owned(),
    }
}

/// What Code Assist's setup came to.
#[derive(Debug, PartialEq, Eq)]
pub(crate) enum Setup {
    /// The project to use, and the plan when Code Assist named one.
    Project { id: String, plan: String },
    /// The account needs a project named, with Google's reason if any.
    NeedsProject { why: String },
}

impl Setup {
    fn apply(&self, blob: SecretObject) -> SecretObject {
        let blob = blob
            .without(PROJECT)
            .without(PLAN)
            .without(NEEDS_PROJECT)
            .without(NEEDS_PROJECT_WHY);
        match self {
            Self::Project { id, plan } if plan.is_empty() => blob.with_text(PROJECT, id),
            Self::Project { id, plan } => blob.with_text(PROJECT, id).with_text(PLAN, plan),
            Self::NeedsProject { why } if why.is_empty() => blob.with_flag(NEEDS_PROJECT, true),
            Self::NeedsProject { why } => blob
                .with_flag(NEEDS_PROJECT, true)
                .with_text(NEEDS_PROJECT_WHY, why),
        }
    }
}

/// Runs the setup at sign-in and records the outcome in `blob`. A failed
/// setup leaves `blob` as it is; [`ensure`] tries again later.
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

/// Sets a stored account up for Code Assist when it has no project yet.
///
/// # Errors
///
/// Returns an `Auth` error when the account still needs a project named,
/// and the setup's error when it failed.
pub(crate) async fn ensure(
    host: &HostContext,
    endpoints: &Endpoints,
    account: &AccountId,
    access: &SecretString,
    wanted: Option<&str>,
) -> Result<(), PluginError> {
    let secret = host
        .accounts()
        .secret(account)
        .await?
        .ok_or_else(|| PluginError::auth("no Gemini CLI sign-in is stored for this account"))?;
    if SecretObject::parse(&secret)?.text(PROJECT).is_some() {
        return Ok(());
    }
    let outcome = setup(host, endpoints, access, wanted).await?;
    let lease = host.accounts().lease(account, LEASE_WAIT).await?;
    let current = SecretObject::parse(lease.secret())?;
    // Another task set it up while this one asked.
    if current.text(PROJECT).is_some() {
        return Ok(());
    }
    let next = outcome.apply(current);
    let reason = missing_reason(&next);
    lease.commit(None, next.to_secret()).await?;
    match outcome {
        Setup::Project { .. } => Ok(()),
        Setup::NeedsProject { .. } => Err(PluginError::auth(reason)),
    }
}

/// What Code Assist is told about the client, as Gemini CLI sends it.
fn metadata(wanted: Option<&str>) -> Value {
    let mut meta = json!({
        "ideType": "IDE_UNSPECIFIED",
        "platform": "PLATFORM_UNSPECIFIED",
        "pluginType": "GEMINI",
    });
    if let Some(want) = wanted {
        meta["duetProject"] = json!(want);
    }
    meta
}

/// Gemini CLI's setup: ask which tier the account is on, and onboard it to
/// the default tier when it is on none.
pub(crate) async fn setup(
    host: &HostContext,
    endpoints: &Endpoints,
    access: &SecretString,
    wanted: Option<&str>,
) -> Result<Setup, PluginError> {
    let meta = metadata(wanted);
    let mut body = json!({ "metadata": meta.clone() });
    if let Some(want) = wanted {
        body["cloudaicompanionProject"] = json!(want);
    }
    let load = call(host, endpoints, access, "loadCodeAssist", &body).await?;
    if load.get("currentTier").is_some_and(Value::is_object) {
        let plan = plan(&load);
        if let Some(id) = companion(load.get("cloudaicompanionProject")) {
            return Ok(Setup::Project { id, plan });
        }
        if let Some(want) = wanted {
            return Ok(Setup::Project {
                id: want.to_owned(),
                plan,
            });
        }
        return Ok(Setup::NeedsProject { why: String::new() });
    }
    let tier = default_tier(&load);
    if tier.user_defined && wanted.is_none() {
        return Ok(Setup::NeedsProject {
            why: ineligible_reason(&load),
        });
    }
    let mut request = json!({ "tierId": tier.id, "metadata": meta });
    if tier.id != "free-tier" {
        request["cloudaicompanionProject"] = json!(wanted.unwrap_or_default());
    }
    let onboarded = onboard(host, endpoints, access, &request).await?;
    match onboarded.or_else(|| wanted.map(str::to_owned)) {
        Some(id) => Ok(Setup::Project {
            id,
            plan: tier.name,
        }),
        None => Ok(Setup::NeedsProject { why: String::new() }),
    }
}

/// A Code Assist tier.
#[derive(Debug, PartialEq, Eq)]
struct Tier {
    id: String,
    name: String,
    user_defined: bool,
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
            user_defined: value
                .get("userDefinedCloudaicompanionProject")
                .and_then(Value::as_bool)
                .unwrap_or(false),
        }
    }
}

/// The tier to onboard to: the default allowed one, else the first, else
/// the legacy tier, which needs a project named.
fn default_tier(load: &Value) -> Tier {
    let allowed = load
        .get("allowedTiers")
        .and_then(Value::as_array)
        .map(Vec::as_slice)
        .unwrap_or_default();
    let chosen = allowed
        .iter()
        .find(|tier| tier.get("isDefault").and_then(Value::as_bool) == Some(true))
        .or_else(|| allowed.first());
    chosen.map_or_else(
        || Tier {
            id: "legacy-tier".to_owned(),
            name: String::new(),
            user_defined: true,
        },
        Tier::read,
    )
}

/// The first reason Google gave for a tier the account cannot have.
fn ineligible_reason(load: &Value) -> String {
    load.get("ineligibleTiers")
        .and_then(Value::as_array)
        .into_iter()
        .flatten()
        .filter_map(|tier| tier.get("reasonMessage").and_then(Value::as_str))
        .find(|why| !why.is_empty())
        .unwrap_or_default()
        .to_owned()
}

/// The plan: the paid tier's name, else the current tier's name or id.
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
    match tier {
        Some(tier) if !tier.name.is_empty() => tier.name,
        Some(tier) => tier.id,
        None => String::new(),
    }
}

/// Code Assist's project, which comes as an id or as `{id, name}`.
fn companion(value: Option<&Value>) -> Option<String> {
    let id = match value? {
        Value::String(id) => id.as_str(),
        Value::Object(project) => project.get("id").and_then(Value::as_str)?,
        _ => return None,
    };
    (!id.is_empty()).then(|| id.to_owned())
}

/// Onboards the account, asking again until Code Assist is done, and
/// returns the project it gave.
async fn onboard(
    host: &HostContext,
    endpoints: &Endpoints,
    access: &SecretString,
    request: &Value,
) -> Result<Option<String>, PluginError> {
    for ask in 0..ONBOARD_ASKS {
        if ask > 0 {
            hofund_plugin_sdk::sleep(endpoints.onboard_poll).await;
        }
        let reply = call(host, endpoints, access, "onboardUser", request).await?;
        if reply.get("done").and_then(Value::as_bool) == Some(true) {
            return Ok(companion(
                reply
                    .get("response")
                    .and_then(|response| response.get("cloudaicompanionProject")),
            ));
        }
    }
    Err(PluginError::timeout(
        "setting the account up for Code Assist is taking long; try again in a minute",
    ))
}

/// Posts one Code Assist method and reads its JSON reply.
async fn call(
    host: &HostContext,
    endpoints: &Endpoints,
    access: &SecretString,
    method: &str,
    body: &Value,
) -> Result<Value, PluginError> {
    let url = format!("{}/v1internal:{method}", endpoints.code_assist);
    let reply = Call::post(url)
        .bearer(access)?
        .header("user-agent", user_agent(None))
        .json(body.to_string())
        .fetch(host.http())
        .await?;
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

/// The user agent Gemini CLI sends: its version, the model, and the
/// platform and architecture in Node's terms.
pub(crate) fn user_agent(model: Option<&str>) -> String {
    user_agent_for(model, std::env::consts::OS, std::env::consts::ARCH)
}

pub(crate) fn user_agent_for(model: Option<&str>, os: &str, arch: &str) -> String {
    let platform = match os {
        "macos" => "darwin",
        "windows" => "win32",
        "linux" => "linux",
        _ => "",
    };
    let arch = match arch {
        "x86_64" => "x64",
        "aarch64" => "arm64",
        _ => "",
    };
    let model = model.unwrap_or("gemini-2.5-pro");
    format!("GeminiCLI/{GEMINI_CLI_VERSION}/{model} ({platform}; {arch}; terminal)")
}
