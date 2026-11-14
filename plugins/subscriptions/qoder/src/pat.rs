//! Personal Access Token sign-in and re-exchange on the account's openapi host.

use hofund_plugin_api::{AccountId, HostContext, PluginError, SecretString, SignInCtx};
use hofund_plugin_sdk::Call;
use hofund_plugin_sdk::json::Value;
use hofund_plugin_sdk::oauth::secret_text;
use hofund_plugin_sdk::uuid;

use crate::Endpoints;
use crate::creds::{Credential, Site};
use crate::import;
use crate::sign_in;

/// Whether `pat` matches the reference PAT prefix rule.
pub(crate) fn validate_format(pat: &str) -> Result<(), PluginError> {
    if pat.trim().starts_with("pt-") {
        Ok(())
    } else {
        Err(PluginError::auth("PAT must start with pt-"))
    }
}

/// Exchanges a pasted PAT and stores a PAT-backed account.
pub(crate) async fn sign_in_with_pat(
    host: &HostContext,
    endpoints: &Endpoints,
    ctx: &SignInCtx,
    pat: SecretString,
) -> Result<AccountId, PluginError> {
    let pat_text = secret_text(&pat).trim().to_owned();
    if pat_text.is_empty() {
        return Err(PluginError::auth("enter your Personal Access Token"));
    }
    validate_format(&pat_text)?;
    let site_kind = site_from_ctx(ctx);
    let site = endpoints.resolve_for(site_kind, "")?;
    let credential = exchange(host, &site.openapi, site_kind, &pat_text).await?;
    import::store(&credential, host).await
}

/// Re-exchanges the stored PAT after a refused job-token refresh.
pub(crate) async fn re_exchange(
    host: &HostContext,
    credential: &Credential,
    openapi_base: &str,
) -> Result<Credential, PluginError> {
    let pat = secret_text(&credential.pat).trim();
    if pat.is_empty() {
        return Err(PluginError::auth(
            "Qoder sign-in has expired; sign in again",
        ));
    }
    match exchange(host, openapi_base, credential.site, pat).await {
        Err(error) if error.to_string().contains("refused the PAT") => {
            Err(PluginError::auth(re_exchange_refused_message()))
        }
        other => other,
    }
}

async fn exchange(
    host: &HostContext,
    openapi_base: &str,
    site_kind: Site,
    pat: &str,
) -> Result<Credential, PluginError> {
    let body = hofund_plugin_sdk::json::json!({ "personal_token": pat }).to_string();
    let reply = Call::post(format!(
        "{}/api/v1/jobToken/exchange",
        openapi_base.trim_end_matches('/')
    ))
    .header("Accept", "application/json")
    .header("Content-Type", "application/json")
    .bytes(body)
    .fetch(host.http())
    .await?;
    if (400..=403).contains(&reply.status()) {
        return Err(pat_refused(site_kind));
    }
    if reply.status() != 200 {
        return Err(PluginError::status_error(
            reply.status(),
            "Qoder PAT exchange failed",
        ));
    }
    let root: Value = hofund_plugin_sdk::json::from_slice(reply.body())
        .map_err(|_| PluginError::decode("Qoder PAT exchange returned invalid JSON"))?;
    let token = sign_in::required_token(&root, "Qoder PAT exchange")?;
    let refresh = refresh_from_exchange(&root)?;
    let expires_at = sign_in::job_expiry(host, &root);
    let machine_id =
        uuid::v4().map_err(|_| PluginError::internal("the system random source failed"))?;
    let user = user_info(host, openapi_base, secret_text(&token)).await;
    let uid = first_string(&user, &["id", "user_id", "uid"])
        .ok_or_else(|| PluginError::auth("Qoder PAT sign-in could not determine user id"))?;
    let email = user
        .get("email")
        .and_then(Value::as_str)
        .unwrap_or("")
        .to_owned();
    let name = user
        .get("name")
        .and_then(Value::as_str)
        .unwrap_or("")
        .to_owned();
    Ok(Credential {
        uid,
        email,
        name,
        token,
        refresh_token: refresh,
        expires_at,
        machine_id,
        site: site_kind,
        vpc: String::new(),
        device_chat: false,
        device_token: SecretString::from(String::new()),
        device_refresh_token: SecretString::from(String::new()),
        pat_auth: true,
        pat: SecretString::from(pat.to_owned()),
    })
}

fn pat_refused(site: Site) -> PluginError {
    let name = match site {
        Site::Global => "Qoder",
        Site::China => "Qoder CN",
    };
    PluginError::auth(format!("{name} refused the PAT — check it is still valid"))
}

pub(crate) fn re_exchange_refused_message() -> &'static str {
    "Qoder refused the PAT — sign in again with a new one"
}

fn refresh_from_exchange(root: &Value) -> Result<SecretString, PluginError> {
    root.get("refresh_token")
        .or_else(|| root.get("refreshToken"))
        .and_then(Value::as_str)
        .filter(|token| !token.is_empty())
        .map(|token| SecretString::from(token.to_owned()))
        .ok_or_else(|| PluginError::auth("Qoder PAT exchange returned no refresh token"))
}

async fn user_info(host: &HostContext, openapi_base: &str, token: &str) -> Value {
    let reply = Call::get(format!(
        "{}/api/v1/userinfo",
        openapi_base.trim_end_matches('/')
    ))
    .header("Accept", "application/json")
    .sensitive_header("Authorization", format!("Bearer {token}"))
    .fetch(host.http())
    .await;
    reply
        .ok()
        .filter(|reply| (200..300).contains(&reply.status()))
        .and_then(|reply| hofund_plugin_sdk::json::from_slice(reply.body()).ok())
        .unwrap_or_else(|| hofund_plugin_sdk::json::json!({}))
}

fn site_from_ctx(ctx: &SignInCtx) -> Site {
    match ctx.option("site") {
        Some("qoder-cn" | "cn" | "china") => Site::China,
        _ => Site::Global,
    }
}

fn first_string(value: &Value, keys: &[&str]) -> Option<String> {
    keys.iter().find_map(|key| {
        value
            .get(*key)
            .and_then(Value::as_str)
            .map(str::trim)
            .filter(|text| !text.is_empty())
            .map(str::to_owned)
    })
}
