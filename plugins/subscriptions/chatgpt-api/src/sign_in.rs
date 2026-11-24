//! Browser sign-in for Sign in with ChatGPT.

use std::time::{Duration, SystemTime, UNIX_EPOCH};

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
/// An ID token is sent as `id_token_hint` only while its `exp` is still
/// further ahead than this. A hint past that point is left out.
const HINT_MARGIN: Duration = Duration::from_secs(60);

pub(crate) struct SignedIn {
    pub(crate) id: AccountId,
    pub(crate) label: String,
    pub(crate) secret: SecretString,
}

/// Authorization-code token request after the browser callback.
pub(crate) struct Exchange<'a> {
    pub(crate) token_url: &'a str,
    pub(crate) client_id: &'a str,
    pub(crate) code: &'a SecretString,
    pub(crate) verifier: &'a SecretString,
    pub(crate) redirect_uri: &'a str,
    pub(crate) nonce: &'a str,
    pub(crate) host_id: &'a str,
    /// Subject of the account being signed in again. Another subject is refused.
    pub(crate) expected_subject: Option<&'a str>,
}

/// A saved account being signed in again.
#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) struct SavedSignIn {
    pub(crate) client_id: String,
    pub(crate) id_token: String,
    pub(crate) email: String,
    pub(crate) subject: String,
}

/// Fields from [`SavedSignIn`] that the authorize URL may send.
#[derive(Clone, Copy)]
pub(crate) struct AuthorizeSaved<'a> {
    pub(crate) client_id: &'a str,
    pub(crate) id_token: &'a str,
    pub(crate) email: &'a str,
    pub(crate) now: SystemTime,
}

/// The saved ChatGPT API account named by `user` (email, subject, label, or
/// account id), when one is stored with an issued client id and a refresh token.
///
/// An empty or unknown name is a new account.
pub(crate) async fn saved_sign_in(
    host: &HostContext,
    user: Option<&str>,
) -> Result<Option<SavedSignIn>, PluginError> {
    let Some(user) = user.map(str::trim).filter(|user| !user.is_empty()) else {
        return Ok(None);
    };
    for account in host.accounts().list().await? {
        let Some(secret) = host.accounts().secret(account.id()).await? else {
            continue;
        };
        let Ok(blob) = SecretObject::parse(&secret) else {
            continue;
        };
        let Some(client_id) = blob.text("client_id").filter(|id| !id.is_empty()) else {
            continue;
        };
        if blob.secret("refresh_token").is_none() {
            continue;
        }
        let id_token = blob
            .secret("id_token")
            .map(|token| secret_text(&token).to_owned())
            .unwrap_or_default();
        let (email, subject) = email_and_subject(&id_token);
        if !names_saved(
            account.id().as_str(),
            account.label(),
            user,
            &email,
            &subject,
        ) {
            continue;
        }
        return Ok(Some(SavedSignIn {
            client_id: client_id.to_owned(),
            id_token,
            email,
            subject,
        }));
    }
    Ok(None)
}

fn names_saved(id: &str, label: &str, user: &str, email: &str, subject: &str) -> bool {
    id.eq_ignore_ascii_case(user)
        || label.eq_ignore_ascii_case(user)
        || (!email.is_empty() && email.eq_ignore_ascii_case(user))
        || (!subject.is_empty() && subject.eq_ignore_ascii_case(user))
}

fn email_and_subject(id_token: &str) -> (String, String) {
    if id_token.is_empty() {
        return (String::new(), String::new());
    }
    let Ok(claims) = unverified_claims(&SecretString::from(id_token.to_owned())) else {
        return (String::new(), String::new());
    };
    let text = |key: &str| {
        claims
            .get(key)
            .and_then(Value::as_str)
            .unwrap_or_default()
            .to_owned()
    };
    (text("email"), text("sub"))
}

/// Whether `id_token` can be sent as `id_token_hint` at `now`.
///
/// A token with no `exp`, a token that is not a JWT, and a token whose `exp`
/// is not strictly after `now` plus one minute are left out.
#[must_use]
pub(crate) fn id_token_hint_live(id_token: &str, now: SystemTime) -> bool {
    if id_token.is_empty() {
        return false;
    }
    let Ok(claims) = unverified_claims(&SecretString::from(id_token.to_owned())) else {
        return false;
    };
    let Some(exp) = claims.get("exp").and_then(exp_instant) else {
        return false;
    };
    let Some(limit) = now.checked_add(HINT_MARGIN) else {
        return false;
    };
    exp > limit
}

fn exp_instant(value: &Value) -> Option<SystemTime> {
    if let Some(seconds) = value.as_u64() {
        return UNIX_EPOCH.checked_add(Duration::from_secs(seconds));
    }
    if let Some(seconds) = value.as_i64() {
        if seconds < 0 {
            return None;
        }
        return UNIX_EPOCH.checked_add(Duration::from_secs(seconds as u64));
    }
    let seconds = value.as_f64()?;
    if !seconds.is_finite() || seconds < 0.0 {
        return None;
    }
    UNIX_EPOCH.checked_add(Duration::from_secs(seconds as u64))
}

pub(crate) fn authorize(
    base: &str,
    redirect_uri: &str,
    pkce: &Pkce,
    state: &str,
    nonce: &str,
    host_id: &str,
    saved: Option<AuthorizeSaved<'_>>,
) -> Result<String, PluginError> {
    let saved = saved.filter(|row| !row.client_id.is_empty());
    let client_id = saved.map(|row| row.client_id).unwrap_or(DYNAMIC_CLIENT);
    let hint = saved
        .filter(|row| id_token_hint_live(row.id_token, row.now))
        .map(|row| row.id_token);
    let login = saved
        .filter(|row| !row.email.is_empty())
        .map(|row| row.email);
    let mut pairs = vec![("response_type", "code"), ("client_id", client_id)];
    if saved.is_some() {
        if let Some(token) = hint {
            pairs.push(("id_token_hint", token));
        }
        if let Some(email) = login {
            pairs.push(("login_hint", email));
        }
    } else {
        pairs.push(("agent_name_hint", AGENT_NAME));
    }
    pairs.extend([
        ("ext_agent_host_id", host_id),
        ("redirect_uri", redirect_uri),
        ("resource", RESOURCE),
        ("scope", SIGN_IN_SCOPE),
        ("state", state),
        ("nonce", nonce),
        ("code_challenge", pkce.challenge.as_str()),
        ("code_challenge_method", "S256"),
    ]);
    authorize_url(base, &pairs)
}

pub(crate) fn new_nonce() -> Result<String, PluginError> {
    new_state_with_random_bytes(24)
}

pub(crate) async fn exchange(
    host: &HostContext,
    form: &Exchange<'_>,
) -> Result<SignedIn, PluginError> {
    if form.client_id.is_empty() {
        return Err(PluginError::auth(
            "ChatGPT sign-in: OpenAI did not finish registering hofund (no client id came back); start it again",
        ));
    }
    let body = form_body(&[
        ("grant_type", "authorization_code"),
        ("client_id", form.client_id),
        ("code", secret_text(form.code)),
        ("code_verifier", secret_text(form.verifier)),
        ("redirect_uri", form.redirect_uri),
        ("resource", RESOURCE),
    ]);
    let reply = Call::post(form.token_url)
        .header("content-type", "application/x-www-form-urlencoded")
        .header("accept", "application/json")
        .bytes(body)
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
    check_id(id_token, form.client_id, form.nonce, now)?;
    let identity = identity(id_token)?;
    if let Some(expected) = form.expected_subject.filter(|subject| !subject.is_empty())
        && identity.sub != expected
    {
        let who = if identity.email.is_empty() {
            "another account"
        } else {
            identity.email.as_str()
        };
        return Err(PluginError::auth(format!(
            "ChatGPT sign-in: {who} signed in, not the account being signed in to again; add it as a new account instead"
        )));
    }
    let expires = set.expires_at().map(tokens::unix_seconds).unwrap_or(0);
    let secret = SecretObject::new()
        .with_number("v", 1)
        .with_text("client_id", form.client_id)
        .with_text("host_id", form.host_id)
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

fn check_id(
    id_token: &SecretString,
    client_id: &str,
    nonce: &str,
    now: SystemTime,
) -> Result<(), PluginError> {
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
        let now = now
            .duration_since(UNIX_EPOCH)
            .map_or(0, |elapsed| elapsed.as_secs());
        if exp.saturating_add(60) < now {
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
    sub: String,
    email: String,
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
        sub: sub.to_owned(),
        email: email.to_owned(),
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
