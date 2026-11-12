//! Native-app RSA sign-in through Zed's browser page and loopback callback.

use std::sync::Arc;

use hofund_plugin_api::{
    AccountId, HostContext, LoopbackSpec, PluginError, RedirectHost, SignInChallenge, SignInCtx,
    SignInProof,
};
use hofund_plugin_sdk::oauth::secret_text;
use hofund_plugin_sdk::rsa::PrivateKey;
use hofund_plugin_sdk::{Call, url, uuid};

use crate::creds::{self, Credential};
use crate::{Endpoints, headers, import, org};

const SIGN_IN_URL: &str = "https://zed.dev/native_app_signin";
const SIGN_IN_SUCCEEDED: &str = "https://zed.dev/native_app_signin_succeeded";

pub(crate) struct Pending {
    key: PrivateKey,
    system_id: String,
}

pub(crate) fn start(
    ctx: &SignInCtx,
    host: &HostContext,
) -> Result<(Pending, SignInChallenge), PluginError> {
    let key = PrivateKey::generate()?;
    let public_key = key.public_key_pkcs1_base64url()?;
    let system_id = uuid::v4().map_err(|error| {
        PluginError::internal("Zed system id generation failed").with_source(error)
    })?;
    let spec = LoopbackSpec::ephemeral()
        .path("/")
        .redirect_host(RedirectHost::Ipv4)
        .query_token(SIGN_IN_SUCCEEDED);
    let binding = host.loopback().bind(ctx, &spec)?;
    let mut url = url::Url::parse(SIGN_IN_URL)
        .map_err(|_| PluginError::internal("Zed sign-in URL is invalid"))?;
    url.query_pairs_mut()
        .append_pair("native_app_port", &binding.port().to_string())
        .append_pair("native_app_public_key", &public_key)
        .append_pair("system_id", &system_id);
    let challenge = match SignInChallenge::open_url(url.to_string()) {
        Ok(challenge) => challenge,
        Err(error) => {
            host.loopback().release(ctx);
            return Err(error);
        }
    };
    Ok((Pending { key, system_id }, challenge))
}

pub(crate) async fn complete(
    host: &HostContext,
    endpoints: Arc<Endpoints>,
    pending: Pending,
    proof: SignInProof,
) -> Result<AccountId, PluginError> {
    let SignInProof::QueryToken {
        user_id,
        access_token,
    } = proof
    else {
        return Err(PluginError::config(
            "Zed sign-in completes from the native-app browser callback",
        ));
    };
    let access = pending.key.decrypt(secret_text(&access_token))?;
    let mut credential = Credential {
        user_id,
        access,
        system_id: pending.system_id,
        org: String::new(),
        login: String::new(),
        name: String::new(),
        plan: String::new(),
        models_json: "[]".to_owned(),
    };
    let me = users_me(host, &endpoints, &credential).await?;
    org::apply_org(&mut credential, &me);
    if org::models_off(&me, &credential.org) {
        return Err(PluginError::auth(
            "Zed: this account's organization has Zed's hosted models turned off",
        ));
    }
    if let Some(user) = me.get("user") {
        credential.login = user
            .get("github_login")
            .or_else(|| user.get("username"))
            .and_then(hofund_plugin_sdk::json::Value::as_str)
            .unwrap_or("")
            .to_owned();
        credential.name = user
            .get("name")
            .and_then(hofund_plugin_sdk::json::Value::as_str)
            .unwrap_or("")
            .to_owned();
    }
    import::store(&credential, host).await
}

async fn users_me(
    host: &HostContext,
    endpoints: &Endpoints,
    credential: &Credential,
) -> Result<hofund_plugin_sdk::json::Value, PluginError> {
    let mut call = Call::get(format!("{}/client/users/me", endpoints.cloud))
        .header("Accept", "application/json")
        .sensitive_header("authorization", creds::auth_header(credential));
    for (name, value) in headers::cloud_headers() {
        call = call.header(name, value);
    }
    if !credential.system_id.is_empty() {
        call = call.header("x-zed-system-id", credential.system_id.clone());
    }
    let reply = call.fetch(host.http()).await?;
    if !(200..=299).contains(&reply.status()) {
        return Err(PluginError::status_error(
            reply.status(),
            "Zed could not read the signed-in account",
        ));
    }
    org::parse_me(reply.body())
}
