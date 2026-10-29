//! The browser sign-in: the authorize address and the code exchange.

use hofund_plugin_api::{AccountId, HostContext, PluginError, SecretString};
use hofund_plugin_sdk::Call;
use hofund_plugin_sdk::oauth::{
    Pkce, SecretObject, account_id_from, authorize_url, code_exchange_form, parse_token_set,
    token_error, unverified_claims,
};

use crate::{CLIENT_ID, SIGN_IN_SCOPE, tokens};

/// The ID token claim that holds the ChatGPT account and plan.
pub(crate) const AUTH_CLAIM: &str = "https://api.openai.com/auth";

/// The authorize address Codex CLI opens.
pub(crate) fn authorize(
    base: &str,
    redirect_uri: &str,
    pkce: &Pkce,
    state: &str,
) -> Result<String, PluginError> {
    authorize_url(
        base,
        &[
            ("response_type", "code"),
            ("client_id", CLIENT_ID),
            ("redirect_uri", redirect_uri),
            ("scope", SIGN_IN_SCOPE),
            ("code_challenge", &pkce.challenge),
            ("code_challenge_method", "S256"),
            ("id_token_add_organizations", "true"),
            ("codex_cli_simplified_flow", "true"),
            ("state", state),
            ("originator", "codex_cli_rs"),
        ],
    )
}

/// A finished sign-in, ready to store.
pub(crate) struct SignedIn {
    pub(crate) id: AccountId,
    pub(crate) label: String,
    pub(crate) secret: SecretString,
}

/// Who an ID token says signed in.
pub(crate) struct Identity {
    pub(crate) id: AccountId,
    pub(crate) label: String,
    pub(crate) chatgpt_account: String,
}

/// Trades the callback code for the token set.
pub(crate) async fn exchange(
    host: &HostContext,
    token_url: &str,
    code: &SecretString,
    verifier: &SecretString,
    redirect_uri: &str,
) -> Result<SignedIn, PluginError> {
    let form = code_exchange_form(CLIENT_ID, redirect_uri, code, verifier);
    let reply = Call::post(token_url)
        .header("content-type", "application/x-www-form-urlencoded")
        .bytes(form)
        .fetch(host.http())
        .await?;
    if !(200..300).contains(&reply.status()) {
        return Err(token_error(reply.status(), reply.body()));
    }
    let now = host.clock().now();
    let set =
        parse_token_set(reply.body(), now).map_err(|error| PluginError::decode(error.message()))?;
    let Some(id_token) = set.id_token() else {
        return Err(PluginError::auth("ChatGPT sent back no ID token"));
    };
    let Some(refresh_token) = set.refresh() else {
        return Err(PluginError::auth("ChatGPT sent back no refresh token"));
    };
    let identity = identity(id_token)?;
    let secret = SecretObject::new()
        .with_number("v", 1)
        .with_secret("id_token", id_token)
        .with_secret("access_token", set.access())
        .with_secret("refresh_token", refresh_token)
        .with_text("account_id", &identity.chatgpt_account)
        .with_number("last_refresh", tokens::unix_seconds(now))
        .to_secret();
    Ok(SignedIn {
        id: identity.id,
        label: identity.label,
        secret,
    })
}

/// Reads the account an ID token names. The token came straight from the
/// token endpoint over TLS, so its signature is not checked.
pub(crate) fn identity(id_token: &SecretString) -> Result<Identity, PluginError> {
    let claims = unverified_claims(id_token)
        .map_err(|_| PluginError::auth("ChatGPT sent back an unreadable ID token"))?;
    let email = claims
        .get("email")
        .and_then(|value| value.as_str())
        .unwrap_or_default();
    if email.is_empty() {
        return Err(PluginError::auth(
            "ChatGPT didn't say which account signed in",
        ));
    }
    let auth = claims.get(AUTH_CLAIM);
    let chatgpt_account = auth
        .and_then(|auth| auth.get("chatgpt_account_id"))
        .and_then(|value| value.as_str())
        .unwrap_or_default()
        .to_owned();
    let plan = auth
        .and_then(|auth| auth.get("chatgpt_plan_type"))
        .and_then(|value| value.as_str())
        .unwrap_or_default();
    let sub = claims
        .get("sub")
        .and_then(|value| value.as_str())
        .unwrap_or_default();
    Ok(Identity {
        id: account_id_from(&[sub, &chatgpt_account]),
        label: label(email, plan),
        chatgpt_account,
    })
}

/// The account label: the email, and the workspace kind for a shared plan.
pub(crate) fn label(email: &str, plan: &str) -> String {
    let workspace = match plan.to_ascii_lowercase().as_str() {
        "team" => "Team",
        "business" => "Business",
        "enterprise" => "Enterprise",
        "edu" => "Edu",
        _ => return email.to_owned(),
    };
    format!("{email} \u{b7} {workspace}")
}
