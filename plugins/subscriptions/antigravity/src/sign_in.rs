//! Browser sign-in with Antigravity's installed-app OAuth client.

use hofund_plugin_api::{AccountId, HostContext, PluginError, SecretString};
use hofund_plugin_sdk::Call;
use hofund_plugin_sdk::json;
use hofund_plugin_sdk::oauth::{
    Pkce, SecretObject, account_id_from, authorize_url, client_code_exchange_form, parse_token_set,
    token_error,
};

use crate::{Endpoints, SCOPES, client, project};

pub(crate) fn authorize(
    base: &str,
    redirect_uri: &str,
    pkce: &Pkce,
    state: &str,
    oauth: &client::OauthClient,
) -> Result<String, PluginError> {
    authorize_url(
        base,
        &[
            ("client_id", oauth.id()),
            ("redirect_uri", redirect_uri),
            ("response_type", "code"),
            ("scope", SCOPES),
            ("access_type", "offline"),
            ("prompt", "consent"),
            ("state", state),
            ("code_challenge", &pkce.challenge),
            ("code_challenge_method", "S256"),
        ],
    )
}

pub(crate) fn exchange_form(
    redirect_uri: &str,
    code: &SecretString,
    verifier: &SecretString,
    oauth: &client::OauthClient,
) -> String {
    client_code_exchange_form(oauth.id(), oauth.secret(), redirect_uri, code, verifier)
}

pub(crate) struct SignedIn {
    pub(crate) id: AccountId,
    pub(crate) label: String,
    pub(crate) secret: SecretString,
}

pub(crate) async fn exchange(
    host: &HostContext,
    endpoints: &Endpoints,
    oauth: &client::OauthClient,
    code: &SecretString,
    verifier: &SecretString,
    redirect_uri: &str,
    wanted: Option<&str>,
) -> Result<SignedIn, PluginError> {
    let reply = Call::post(&endpoints.token)
        .header("content-type", "application/x-www-form-urlencoded")
        .bytes(exchange_form(redirect_uri, code, verifier, oauth))
        .fetch(host.http())
        .await?;
    if !(200..300).contains(&reply.status()) {
        return Err(token_error(reply.status(), reply.body()));
    }
    let set = parse_token_set(reply.body(), host.clock().now())
        .map_err(|error| PluginError::decode(error.message()))?;
    let Some(refresh_token) = set.refresh() else {
        return Err(PluginError::auth("Google sent back no refresh token"));
    };
    let email = who(host, &endpoints.userinfo, set.access()).await?;
    let blob = SecretObject::new()
        .with_number("v", 1)
        .with_secret("refresh_token", refresh_token)
        .with_text("email", &email);
    let blob = project::settle(host, endpoints, set.access(), wanted, blob).await;
    Ok(SignedIn {
        id: account_id_from(&["antigravity", &email.to_ascii_lowercase()]),
        label: email,
        secret: blob.to_secret(),
    })
}

async fn who(host: &HostContext, url: &str, access: &SecretString) -> Result<String, PluginError> {
    let reply = Call::get(url).bearer(access)?.fetch(host.http()).await?;
    if reply.status() != 200 {
        return Err(PluginError::auth(format!(
            "Google didn't say which account signed in (HTTP {})",
            reply.status()
        )));
    }
    let info: json::Value = json::from_slice(reply.body())
        .map_err(|_| PluginError::decode("Google sent back unreadable account details"))?;
    let email = info
        .get("email")
        .and_then(json::Value::as_str)
        .unwrap_or_default()
        .trim();
    if email.is_empty() {
        return Err(PluginError::auth(
            "Google didn't say which account signed in",
        ));
    }
    Ok(email.to_owned())
}
