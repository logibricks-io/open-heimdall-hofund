//! Device sign-in: ask GitHub for a code, poll for the OAuth token, then
//! find out whose token it is and that the user has Copilot.

use std::time::Duration;

use hofund_plugin_api::{AccountId, HostContext, PluginError, SecretString};
use hofund_plugin_sdk::json::{self, Value};
use hofund_plugin_sdk::oauth::device::{self, DeviceStart};
use hofund_plugin_sdk::oauth::{SecretObject, account_id_from};
use hofund_plugin_sdk::{Call, form_body};

use crate::session::github_get;
use crate::site::Site;

/// The OAuth app the Copilot editors sign in as.
pub(crate) const CLIENT_ID: &str = "Iv1.b507a08c87ecfe98";

/// The scope asked for: enough to read the user's login.
pub(crate) const SCOPE: &str = "read:user";

/// How long the user lookups after the token may take.
pub(crate) const FINISH_WAIT: Duration = Duration::from_secs(60);

/// A finished sign-in, ready to store.
pub(crate) struct SignedIn {
    pub(crate) id: AccountId,
    pub(crate) label: String,
    pub(crate) secret: SecretString,
}

fn form_post(url: String, form: String) -> Call {
    Call::post(url)
        .header("accept", "application/json")
        .header("content-type", "application/x-www-form-urlencoded")
        .bytes(form)
}

/// Asks `site` for a device code.
pub(crate) async fn start(host: &HostContext, site: &Site) -> Result<DeviceStart, PluginError> {
    let form = form_body(&[("client_id", CLIENT_ID), ("scope", SCOPE)]);
    device::start(form_post(site.device_code_url(), form).fetch(host.http())).await
}

/// Polls for the token, then reads the user within [`FINISH_WAIT`].
pub(crate) async fn finish(
    host: &HostContext,
    site: &Site,
    start: &DeviceStart,
) -> Result<SignedIn, PluginError> {
    let token_url = site.token_url();
    let http = host.http();
    let set = device::poll(host.clock(), start, || {
        form_post(
            token_url.clone(),
            device::device_code_form(CLIENT_ID, start),
        )
        .fetch(http)
    })
    .await?;
    let oauth = set.access().clone();
    hofund_plugin_sdk::timeout(FINISH_WAIT, identify(host, site, oauth))
        .await
        .unwrap_or_else(|| {
            Err(PluginError::timeout(
                "GitHub issued the token but did not say whose account it is within a minute",
            ))
        })
}

async fn identify(
    host: &HostContext,
    site: &Site,
    oauth: SecretString,
) -> Result<SignedIn, PluginError> {
    let reply = github_get(site.user_url(), &oauth)
        .fetch(host.http())
        .await?;
    if !(200..300).contains(&reply.status()) {
        return Err(PluginError::status_error(
            reply.status(),
            "GitHub would not say whose sign-in this is",
        ));
    }
    let (login, user_id) = parse_user(reply.body())?;
    let reply = github_get(site.copilot_user_url(), &oauth)
        .fetch(host.http())
        .await?;
    match reply.status() {
        200..=299 => {}
        401 | 403 | 404 => {
            return Err(PluginError::auth(format!(
                "{login} has no Copilot subscription"
            )));
        }
        status => {
            return Err(PluginError::status_error(
                status,
                "GitHub would not show the Copilot seat",
            ));
        }
    }
    let key = user_id.map_or_else(|| login.clone(), |id| id.to_string());
    let id = account_id_from(&[site.name(), &key]);
    let label = if site.is_enterprise() {
        format!("{login}@{}", site.name())
    } else {
        login.clone()
    };
    let secret = SecretObject::new()
        .with_number("v", 1)
        .with_secret("oauth_token", &oauth)
        .with_text("host", site.name())
        .with_text("login", &login)
        .to_secret();
    Ok(SignedIn { id, label, secret })
}

/// Reads `login` and the numeric `id` from a GitHub user reply.
pub(crate) fn parse_user(body: &[u8]) -> Result<(String, Option<u64>), PluginError> {
    let Ok(Value::Object(object)) = json::from_slice::<Value>(body) else {
        return Err(PluginError::decode(
            "the GitHub user reply is not a JSON object",
        ));
    };
    let login = object
        .get("login")
        .and_then(Value::as_str)
        .map(str::trim)
        .filter(|login| !login.is_empty())
        .ok_or_else(|| PluginError::decode("the GitHub user reply has no login"))?;
    Ok((login.to_owned(), object.get("id").and_then(Value::as_u64)))
}
