//! Browser sign-in for Sign in with ChatGPT.

use hofund_plugin_api::{AccountId, HostContext, PluginError, SecretString};
use hofund_plugin_sdk::Call;
use hofund_plugin_sdk::form_body;
use hofund_plugin_sdk::json::{Map, Value};
use hofund_plugin_sdk::oauth::new_state_with_random_bytes;
use hofund_plugin_sdk::oauth::{
    Pkce, SecretObject, account_id_from, authorize_url, parse_token_set, secret_text,
    unverified_claims,
};

use crate::tokens;

pub(crate) const DYNAMIC_CLIENT: &str = "dynamic_agent_client";
pub(crate) const AGENT_NAME: &str = "hofund";
pub(crate) const RESOURCE: &str = "https://api.openai.com/v1";
pub(crate) const DIRECT_SCOPE: &str = "chatgpt.tokens.use.direct";
pub(crate) const SIGN_IN_SCOPE: &str =
    "openid profile email offline_access resource.invoke chatgpt.tokens.use.direct";
pub(crate) const AUTH_CLAIM: &str = "https://api.openai.com/auth";
const ISSUER: &str = "https://auth.openai.com";

pub(crate) struct SignedIn {
    pub(crate) id: AccountId,
    pub(crate) label: String,
    pub(crate) plan: String,
    pub(crate) secret: SecretString,
}

pub(crate) fn authorize(
    base: &str,
    redirect_uri: &str,
    pkce: &Pkce,
    state: &str,
    nonce: &str,
    host_id: &str,
) -> Result<String, PluginError> {
    authorize_url(
        base,
        &[
            ("response_type", "code"),
            ("client_id", DYNAMIC_CLIENT),
            ("agent_name_hint", AGENT_NAME),
            ("ext_agent_host_id", host_id),
            ("redirect_uri", redirect_uri),
            ("resource", RESOURCE),
            ("scope", SIGN_IN_SCOPE),
            ("state", state),
            ("nonce", nonce),
            ("code_challenge", &pkce.challenge),
            ("code_challenge_method", "S256"),
        ],
    )
}

pub(crate) fn new_nonce() -> Result<String, PluginError> {
    new_state_with_random_bytes(24)
}

pub(crate) async fn exchange(
    host: &HostContext,
    token_url: &str,
    client_id: &str,
    code: &SecretString,
    verifier: &SecretString,
    redirect_uri: &str,
    nonce: &str,
    host_id: &str,
) -> Result<SignedIn, PluginError> {
    if client_id.is_empty() {
        return Err(PluginError::auth(
            "ChatGPT sign-in: OpenAI did not finish registering hofund (no client id came back); start it again",
        ));
    }
    let form = form_body(&[
        ("grant_type", "authorization_code"),
        ("client_id", client_id),
        ("code", secret_text(code)),
        ("code_verifier", secret_text(verifier)),
        ("redirect_uri", redirect_uri),
        ("resource", RESOURCE),
    ]);
    let reply = Call::post(token_url)
        .header("content-type", "application/x-www-form-urlencoded")
        .header("accept", "application/json")
        .bytes(form)
        .fetch(host.http())
        .await?;
    if !(200..300).contains(&reply.status()) {
        return Err(tokens::token_error(reply.status(), reply.body()));
    }
    let now = host.clock().now();
    let set =
        parse_token_set(reply.body(), now).map_err(|error| PluginError::decode(error.message()))?;
    let Some(id_token) = set.id_token() else {
        return Err(PluginError::auth(
            "ChatGPT sign-in: OpenAI sent no ID token",
        ));
    };
    verify_scope(reply.body())?;
    check_id(id_token, client_id, nonce)?;
    let identity = identity(id_token)?;
    let expires = set
        .expires_at()
        .map(|time| tokens::unix_seconds(time))
        .unwrap_or(0);
    let secret = SecretObject::new()
        .with_number("v", 1)
        .with_text("client_id", client_id)
        .with_text("host_id", host_id)
        .with_secret("id_token", id_token)
        .with_secret("access_token", set.access())
        .with_secret(
            "refresh_token",
            set.refresh().ok_or_else(|| {
                PluginError::auth("ChatGPT sign-in: OpenAI sent no refresh token")
            })?,
        )
        .with_number("expires_at", expires)
        .with_text("plan", &identity.plan);
    Ok(SignedIn {
        id: identity.id,
        label: identity.label,
        plan: identity.plan,
        secret: secret.to_secret(),
    })
}

fn verify_scope(body: &[u8]) -> Result<(), PluginError> {
    let value: Value = hofund_plugin_sdk::json::from_slice(body)
        .map_err(|_| PluginError::decode("ChatGPT sign-in: unreadable token answer"))?;
    let scope = value
        .get("scope")
        .and_then(Value::as_str)
        .unwrap_or_default();
    if !scope.split_whitespace().any(|part| part == DIRECT_SCOPE) {
        return Err(PluginError::auth(format!(
            "OpenAI did not grant hofund the use of the subscription through its API ({DIRECT_SCOPE}): this ChatGPT account may not be eligible"
        )));
    }
    Ok(())
}

fn check_id(id_token: &SecretString, client_id: &str, nonce: &str) -> Result<(), PluginError> {
    let claims = unverified_claims(id_token)
        .map_err(|_| PluginError::auth("ChatGPT sign-in: OpenAI sent an unreadable ID token"))?;
    let iss = claims
        .get("iss")
        .and_then(|v| v.as_str())
        .unwrap_or_default()
        .trim_end_matches('/');
    if iss != ISSUER.trim_end_matches('/') {
        return Err(PluginError::auth(
            "ChatGPT sign-in: the ID token is not from OpenAI",
        ));
    }
    let aud_ok = match claims.get("aud") {
        Some(hofund_plugin_sdk::json::Value::String(aud)) => aud == client_id,
        Some(hofund_plugin_sdk::json::Value::Array(items)) => {
            items.iter().any(|item| item.as_str() == Some(client_id))
        }
        _ => false,
    };
    if !aud_ok {
        return Err(PluginError::auth(
            "ChatGPT sign-in: the ID token is for another app",
        ));
    }
    if claims.get("nonce").and_then(|v| v.as_str()) != Some(nonce) {
        return Err(PluginError::auth(
            "ChatGPT sign-in: the ID token is not for this sign-in",
        ));
    }
    if let Some(exp) = claims.get("exp").and_then(|v| v.as_u64()) {
        let now = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map_or(0, |d| d.as_secs());
        if exp + 60 < now {
            return Err(PluginError::auth(
                "ChatGPT sign-in: the ID token has expired",
            ));
        }
    }
    Ok(())
}

struct Identity {
    id: AccountId,
    label: String,
    plan: String,
}

fn identity(id_token: &SecretString) -> Result<Identity, PluginError> {
    let claims = unverified_claims(id_token)
        .map_err(|_| PluginError::auth("ChatGPT sent back an unreadable ID token"))?;
    let email = claims
        .get("email")
        .and_then(|v| v.as_str())
        .unwrap_or_default();
    let sub = claims
        .get("sub")
        .and_then(|v| v.as_str())
        .unwrap_or_default();
    let user = if email.is_empty() { sub } else { email };
    if user.is_empty() {
        return Err(PluginError::auth(
            "ChatGPT sign-in: OpenAI did not say which account signed in",
        ));
    }
    let plan = plan_from_token(id_token);
    Ok(Identity {
        id: account_id_from(&[sub, user]),
        label: label(email, &plan),
        plan,
    })
}

pub(crate) fn plan_from_token(token: &SecretString) -> String {
    plan_from_claims(&unverified_claims(token).ok().unwrap_or_default())
}

pub(crate) fn plan_from_claims(claims: &Map<String, Value>) -> String {
    claims
        .get(AUTH_CLAIM)
        .and_then(|auth| auth.get("chatgpt_plan_type"))
        .and_then(Value::as_str)
        .unwrap_or_default()
        .to_owned()
}

pub(crate) fn label(email: &str, plan: &str) -> String {
    let workspace = match plan.to_ascii_lowercase().as_str() {
        "team" => "Team",
        "business" => "Business",
        "enterprise" => "Enterprise",
        "edu" => "Edu",
        _ => return email.to_owned(),
    };
    if email.is_empty() {
        email.to_owned()
    } else {
        format!("{email} \u{b7} {workspace}")
    }
}
