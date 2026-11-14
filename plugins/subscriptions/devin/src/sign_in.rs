//! Browser PKCE, token exchange, and account identity.

use std::path::Path;
use std::time::Duration;

use hofund_plugin_api::{HostContext, PluginError, SecretString};
use hofund_plugin_sdk::Call;
use hofund_plugin_sdk::json::{Value, json};
use hofund_plugin_sdk::oauth::{Pkce, authorize_url, secret_text};

use crate::creds::Credential;
use crate::{cli, errors, quota};

pub(crate) const SERVER: &str = crate::creds::DEFAULT_API_SERVER;
pub(crate) const AUTHORIZE: &str = "https://app.devin.ai/auth/cli/continue";
const EXCHANGE_PATH: &str =
    "/exa.seat_management_pb.SeatManagementService/ExchangeDevinCLIPKCECode";
const EXCHANGE_BODY_CAP: usize = 1024 * 1024;

pub(crate) struct Pending {
    pub(crate) verifier: SecretString,
    pub(crate) state: String,
    pub(crate) redirect_uri: String,
}

pub(crate) fn generate() -> Result<(Pkce, String), PluginError> {
    let pkce = Pkce::generate_with_random_bytes(48)?;
    let state = hofund_plugin_sdk::oauth::new_state_with_random_bytes(24)?;
    Ok((pkce, state))
}

pub(crate) fn authorize(
    redirect_uri: &str,
    pkce: &Pkce,
    state: &str,
) -> Result<String, PluginError> {
    authorize_url(
        AUTHORIZE,
        &[
            ("redirect_uri", redirect_uri),
            ("state", state),
            ("prompt", "select_account"),
            ("code_challenge", &pkce.challenge),
            ("code_challenge_method", "S256"),
            ("cli_pkce_marker", "1"),
        ],
    )
}

#[derive(Clone, Debug)]
pub(crate) struct ExchangeOutcome {
    pub(crate) key: SecretString,
    pub(crate) devin_webapp: Option<String>,
    pub(crate) devin_api: Option<String>,
}

pub(crate) async fn exchange(
    host: &HostContext,
    code: &SecretString,
    pending: &Pending,
) -> Result<ExchangeOutcome, PluginError> {
    let body = json!({
        "code": secret_text(code),
        "code_verifier": secret_text(&pending.verifier),
        "redirect_uri": pending.redirect_uri,
    });
    let response = Call::post(format!("{SERVER}{EXCHANGE_PATH}"))
        .header("Content-Type", "application/json")
        .header("Accept", "application/json")
        .read_timeout(Duration::from_secs(30))
        .bytes(body.to_string().into_bytes())
        .open_raw(host.http(), 1)
        .await
        .map_err(|_| PluginError::io("Devin's exchange failed"))?;
    let status = response.status().as_u16();
    let mut stream = response.into_body();
    let mut bytes = Vec::new();
    while bytes.len() < EXCHANGE_BODY_CAP {
        let Some(chunk) = stream.next_chunk().await else {
            break;
        };
        let chunk = chunk.map_err(|_| PluginError::io("Devin's exchange response broke off"))?;
        let remaining = EXCHANGE_BODY_CAP - bytes.len();
        bytes.extend_from_slice(&chunk[..chunk.len().min(remaining)]);
    }
    if !(200..300).contains(&status) {
        let text = String::from_utf8_lossy(&bytes);
        let truncated = text.chars().take(300).collect::<String>();
        return Err(PluginError::auth(format!(
            "Devin's exchange: {status} {truncated}"
        )));
    }
    let value = hofund_plugin_sdk::json::from_slice::<Value>(&bytes).unwrap_or(Value::Null);
    let token = value
        .get("sessionToken")
        .and_then(Value::as_str)
        .or_else(|| value.get("session_token").and_then(Value::as_str))
        .filter(|token| !token.is_empty())
        .ok_or_else(|| PluginError::auth("Devin's exchange returned no session token"))?;
    let devin_api = value
        .get("devinApiUrl")
        .and_then(Value::as_str)
        .map(str::to_owned);
    let devin_webapp = value
        .get("devinWebappHost")
        .and_then(Value::as_str)
        .map(str::to_owned);
    Ok(ExchangeOutcome {
        key: SecretString::from(token.to_owned()),
        devin_webapp,
        devin_api,
    })
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) struct Who {
    pub(crate) user: String,
    pub(crate) email: Option<String>,
    pub(crate) plan: String,
    pub(crate) org_id: Option<String>,
    pub(crate) team_id: Option<String>,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) enum Lookup {
    Known(Who),
    Rejected,
    Unknown,
}

pub(crate) async fn who_by_key(host: &HostContext, credential: &Credential) -> Lookup {
    let Ok(reply) = quota::request(
        host,
        &credential.key,
        &crate::creds::effective_api_server(&credential.api_server),
    )
    .await
    else {
        return Lookup::Unknown;
    };
    if reply.status == 401
        || errors::failure(reply.status, &reply.body)
            .is_some_and(|error| error.status() == Some(401))
    {
        return Lookup::Rejected;
    }
    if !(200..300).contains(&reply.status) {
        return Lookup::Unknown;
    }
    let Ok(value) = hofund_plugin_sdk::json::from_slice::<Value>(&reply.body) else {
        return Lookup::Unknown;
    };
    let empty = Value::Object(Default::default());
    let status = value.get("userStatus").unwrap_or(&empty);
    who_of_status(status).map_or(Lookup::Unknown, Lookup::Known)
}

pub(crate) fn who_of_status(status: &Value) -> Option<Who> {
    let base = text(status, "email")
        .or_else(|| text(status, "name"))?
        .to_owned();
    let info = status
        .get("planStatus")
        .and_then(|value| value.get("planInfo"))
        .unwrap_or(&Value::Null);
    let devin = info.get("devinInfo").unwrap_or(&Value::Null);
    let org = text(devin, "accountDisplayName")
        .or_else(|| text(info, "teamName"))
        .or_else(|| text(status, "teamName"))
        .unwrap_or_default();
    let named = !org.is_empty() && !org.eq_ignore_ascii_case(&base);
    let user = if named {
        format!("{base} · {org}")
    } else {
        base.clone()
    };
    let team_id = text(status, "teamId").map(str::to_owned);
    let org_id = text(devin, "orgId")
        .or_else(|| text(status, "teamId"))
        .map(str::to_owned);
    let tier = text(status, "teamsTier").or_else(|| text(info, "teamsTier"));
    let plan = tier
        .map(tier_name)
        .filter(|plan| !plan.is_empty())
        .or_else(|| text(info, "planName").map(str::to_owned))
        .unwrap_or_default();
    Some(Who {
        user,
        email: named.then_some(base),
        plan,
        org_id,
        team_id,
    })
}

pub(crate) fn tier_name(tier: &str) -> String {
    tier.strip_prefix("TEAMS_TIER_")
        .unwrap_or(tier)
        .split('_')
        .filter(|word| {
            !word.is_empty()
                && *word != "UNSPECIFIED"
                && !(word.strip_prefix('V').is_some_and(|version| {
                    !version.is_empty() && version.bytes().all(|byte| byte.is_ascii_digit())
                }))
        })
        .map(|word| {
            let mut chars = word.chars();
            let first = chars.next().map(String::from).unwrap_or_default();
            format!("{first}{}", chars.as_str().to_lowercase())
        })
        .collect::<Vec<_>>()
        .join(" ")
}

fn text<'a>(value: &'a Value, key: &str) -> Option<&'a str> {
    value
        .get(key)
        .and_then(Value::as_str)
        .map(str::trim)
        .filter(|text| !text.is_empty())
}

pub(crate) async fn success(
    host: &HostContext,
    credential: &Credential,
) -> Result<Credential, PluginError> {
    success_with_program(host, credential, None).await
}

pub(crate) async fn success_with_program(
    host: &HostContext,
    credential: &Credential,
    program: Option<&Path>,
) -> Result<Credential, PluginError> {
    let mut who = who_by_key(host, credential).await;
    if who == Lookup::Unknown {
        who = match program {
            Some(program) => cli::identity_with_program(host, credential, program).await,
            None => cli::identity(host, credential).await,
        };
    }
    let who = match who {
        Lookup::Known(who) => Some(who),
        Lookup::Unknown => None,
        Lookup::Rejected => {
            return Err(PluginError::auth(
                "Devin did not take the sign-in: nobody is signed in with the token it gave",
            ));
        }
    };
    let mut signed_in = credential.clone();
    signed_in.label = who
        .as_ref()
        .map(|who| who.user.clone())
        .unwrap_or_else(|| "Devin".to_owned());
    if let Some(who) = who {
        signed_in.plan = who.plan;
        signed_in.org_id = who
            .org_id
            .or_else(|| who.team_id.clone())
            .unwrap_or_default();
        signed_in.team_id = who.team_id.unwrap_or_default();
    }
    Ok(signed_in)
}
