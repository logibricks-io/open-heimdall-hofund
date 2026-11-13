//! Native Z.ai and BigModel browser sign-in and API-key account import.

use std::time::{Duration, UNIX_EPOCH};

use hofund_plugin_api::{
    AccountId, HostContext, PluginError, SecretString, SignInChallenge, SignInCtx,
};
use hofund_plugin_sdk::json::{self, Value};
use hofund_plugin_sdk::oauth::secret_text;
use hofund_plugin_sdk::url::Url;

use crate::creds::{PlanAuth, Site};
use crate::discover;
use crate::session::Stored;
use crate::{
    APP_VERSION, BIGMODEL_ANTHROPIC_BASE, Endpoints, ZAI_ANTHROPIC_BASE, api, gift, start, team,
};

const DEFAULT_TIMEOUT: Duration = Duration::from_secs(5 * 60);

#[derive(Clone)]
pub(crate) struct Pending {
    site: Site,
    poll: SecretString,
    flow_id: String,
    device: String,
    interval: Duration,
    expires_at: std::time::SystemTime,
    on_team: bool,
}

pub(crate) async fn start(
    ctx: &SignInCtx,
    host: &HostContext,
    endpoints: &Endpoints,
) -> Result<(Pending, SignInChallenge), PluginError> {
    let site = site(ctx)?;
    let poll = SecretString::from(format!(
        "{}{}",
        uuid()?.replace('-', ""),
        uuid()?.replace('-', "")
    ));
    let device = uuid()?;
    let url = format!(
        "{}/api/v1/oauth/cli/init",
        endpoints.portal.trim_end_matches('/')
    );
    let body = json::json!({"provider": site_name(site)});
    let authorization = SecretString::from(format!("Bearer {}", secret_text(&poll)));
    let headers = [("x-device-mid", device.as_str())];
    let device_file = crate::device::device_path(host)?;
    let request = api::EnvelopeRequest {
        method: "POST",
        body: Some(&body),
        ..api::EnvelopeRequest::get(
            host,
            endpoints,
            &url,
            Some(&authorization),
            &headers,
            &device_file,
        )
    };
    let flow = api::get_envelope(&request)
        .await
        .map_err(|error| PluginError::auth(format!("Z Code sign-in: {error}")))?;
    let flow_id = text(&flow, "flow_id").trim().to_owned();
    let mut page = Url::parse(text(&flow, "authorize_url"))
        .map_err(|_| PluginError::auth("Z Code gave no sign-in page"))?;
    if flow_id.is_empty() || page.scheme() != "https" || page.host_str().is_none() {
        return Err(PluginError::auth("Z Code gave no sign-in page"));
    }
    let mut back = Url::parse(&format!(
        "{}/app/oauth/login",
        endpoints.portal.trim_end_matches('/')
    ))
    .map_err(|_| PluginError::config("the Z Code portal endpoint is invalid"))?;
    back.query_pairs_mut()
        .append_pair("redirect", "zcode://oauth/callback")
        .append_pair("app_version", APP_VERSION);
    page.query_pairs_mut().append_pair(
        if site == Site::Bigmodel {
            "redirect"
        } else {
            "redirect_uri"
        },
        back.as_str(),
    );
    let interval = number(flow.get("poll_interval_sec"))
        .map(|seconds| Duration::from_secs_f64(seconds.max(1.0)))
        .unwrap_or(Duration::from_secs(1));
    let expires_at = number(flow.get("expires_at"))
        .filter(|seconds| *seconds > 0.0)
        .map(|seconds| UNIX_EPOCH + Duration::from_secs_f64(seconds))
        .unwrap_or_else(|| host.clock().now() + DEFAULT_TIMEOUT);
    let on_team = ctx.option("team").is_some_and(|value| value == "true");
    let challenge = SignInChallenge::open_url(page.to_string())?;
    Ok((
        Pending {
            site,
            poll,
            flow_id,
            device,
            interval,
            expires_at,
            on_team,
        },
        challenge,
    ))
}

pub(crate) async fn import_api_key(
    ctx: &SignInCtx,
    host: &HostContext,
    key: SecretString,
) -> Result<AccountId, PluginError> {
    let site = site(ctx)?;
    let raw = secret_text(&key).trim().to_owned();
    if raw.is_empty() {
        return Err(PluginError::auth("the API key is empty"));
    }
    let digest = hofund_plugin_sdk::digest::sha256(raw.as_bytes());
    let label = format!(
        "{} API key {:02x}{:02x}",
        site_display(site),
        digest[0],
        digest[1]
    );
    let mut auth = empty_auth(site);
    auth.api_key = Some(key);
    auth.label = label;
    let device = start::key_device(&raw);
    discover::store_with_device(&auth, &device, "", host).await
}

pub(crate) async fn finish(
    host: &HostContext,
    endpoints: &Endpoints,
    pending: Pending,
    alive: impl Fn() -> bool,
) -> Result<AccountId, PluginError> {
    let poll_url = format!(
        "{}/api/v1/oauth/cli/poll/{}",
        endpoints.portal.trim_end_matches('/'),
        encode(&pending.flow_id)
    );
    let authorization = SecretString::from(format!("Bearer {}", secret_text(&pending.poll)));
    let headers = [("x-device-mid", pending.device.as_str())];
    let device_file = crate::device::device_path(host)?;
    let got = loop {
        if !alive() {
            return Err(PluginError::config("sign-in was cancelled"));
        }
        if host.clock().now() >= pending.expires_at {
            return Err(PluginError::auth("the sign-in expired; start again"));
        }
        let result = api::get_envelope(&api::EnvelopeRequest::get(
            host,
            endpoints,
            &poll_url,
            Some(&authorization),
            &headers,
            &device_file,
        ))
        .await;
        match result {
            Ok(got) => {
                let status = text(&got, "status");
                if status == "pending" || status.is_empty() {
                    hofund_plugin_sdk::sleep(pending.interval).await;
                    continue;
                }
                if status == "failed" {
                    return Err(PluginError::auth(format!(
                        "the sign-in was declined on {}",
                        site_display(pending.site)
                    )));
                }
                if status != "ready" {
                    return Err(PluginError::auth(format!(
                        "Z Code sign-in returned an unexpected status: {status}"
                    )));
                }
                break got;
            }
            Err(error) if retryable_poll(&error) => {
                hofund_plugin_sdk::sleep(pending.interval).await;
            }
            Err(error) => {
                return Err(PluginError::auth(format!("Z Code sign-in: {error}")));
            }
        }
    };
    let token = match pending.site {
        Site::Zai => got
            .get("zai")
            .and_then(|site| site.get("access_token"))
            .and_then(Value::as_str),
        Site::Bigmodel => got
            .get("bigmodel")
            .and_then(|site| site.get("access_token").or_else(|| site.get("accessToken")))
            .and_then(Value::as_str),
    }
    .filter(|token| !token.trim().is_empty())
    .ok_or_else(|| PluginError::auth("Z Code sign-in returned no site token"))?;
    let jwt = text(&got, "token").trim().to_owned();
    let user = got.get("user").cloned().unwrap_or(Value::Null);
    let signed = signed_in(
        host,
        endpoints,
        pending.site,
        token,
        &jwt,
        &pending.device,
        pending.on_team,
    )
    .await?;
    let mut auth = signed.auth;
    auth.label = who(&user);
    if pending.on_team {
        auth.label = format!("{} (team)", auth.label);
    }
    auth.uid = text(&user, "user_id").trim().to_owned();
    discover::store_with_device(&auth, &pending.device, &signed.plan, host).await
}

struct Signed {
    auth: PlanAuth,
    plan: String,
}

async fn signed_in(
    host: &HostContext,
    endpoints: &Endpoints,
    site: Site,
    token: &str,
    jwt: &str,
    device: &str,
    on_team: bool,
) -> Result<Signed, PluginError> {
    let business = business_auth(host, endpoints, site, token).await?;
    let root = endpoints.biz_root(base_for(site));
    let info_url = format!("{root}/api/biz/customer/getCustomerInfo");
    let device_file = crate::device::device_path(host)?;
    let info = api::get_envelope(&api::EnvelopeRequest::get(
        host,
        endpoints,
        &info_url,
        Some(&business),
        &[],
        &device_file,
    ))
    .await?;
    let mut first_error = None;
    let mut start_attempted = false;
    if !on_team {
        match own_plan(host, endpoints, site, &business, &info, &device_file).await {
            Ok((key, plan)) if !plan.is_empty() => {
                let mut auth = empty_auth(site);
                auth.api_key = Some(key);
                auth.jwt = nonempty_secret(jwt);
                return Ok(Signed { auth, plan });
            }
            Ok((key, _)) => {
                if let Some(team) =
                    team_seat(host, endpoints, site, &business, &info, &device_file).await?
                {
                    let mut auth = team.auth;
                    auth.jwt = nonempty_secret(jwt);
                    return Ok(Signed {
                        auth,
                        plan: team.plan,
                    });
                }
                start_attempted = true;
                if let Some(signed) =
                    start_plan(host, endpoints, site, Some(key), jwt, device, &device_file).await?
                {
                    return Ok(signed);
                }
            }
            Err(error) => first_error = Some(error),
        }
    } else {
        let team = team_seat(host, endpoints, site, &business, &info, &device_file)
            .await?
            .ok_or_else(|| {
                PluginError::auth(format!(
                    "this {} account has no seat on a team GLM Coding Plan",
                    site_display(site)
                ))
            })?;
        let mut auth = team.auth;
        auth.jwt = nonempty_secret(jwt);
        return Ok(Signed {
            auth,
            plan: team.plan,
        });
    }
    if !start_attempted
        && let Some(signed) =
            start_plan(host, endpoints, site, None, jwt, device, &device_file).await?
    {
        return Ok(signed);
    }
    Err(first_error.unwrap_or_else(|| {
        PluginError::auth(format!(
            "this {} account has no GLM Coding Plan, team seat, or active Z Code Start Plan",
            site_display(site)
        ))
    }))
}

async fn start_plan(
    host: &HostContext,
    endpoints: &Endpoints,
    site: Site,
    key: Option<SecretString>,
    jwt: &str,
    device: &str,
    device_file: &std::path::Path,
) -> Result<Option<Signed>, PluginError> {
    let Some(jwt) = nonempty_secret(jwt) else {
        return Ok(None);
    };
    let mut auth = empty_auth(site);
    auth.api_key = key;
    auth.jwt = Some(jwt);
    let stored = Stored {
        api_key: auth
            .api_key
            .clone()
            .or_else(|| auth.jwt.clone())
            .expect("key or jwt"),
        auth,
        device: device.to_owned(),
    };
    match gift::read(host, endpoints, &stored, device_file).await {
        Ok(balance) if balance.has_plan => Ok(Some(Signed {
            auth: stored.auth,
            plan: balance.plan,
        })),
        Ok(_) => Ok(None),
        Err(error) if error.kind() == hofund_plugin_api::PluginErrorKind::Decode => Ok(None),
        Err(error) => Err(error),
    }
}

async fn own_plan(
    host: &HostContext,
    endpoints: &Endpoints,
    site: Site,
    business: &SecretString,
    info: &Value,
    device_file: &std::path::Path,
) -> Result<(SecretString, String), PluginError> {
    let (org, project) = own_project(info).ok_or_else(|| {
        PluginError::auth(format!(
            "this {} account has no project for an API key",
            site_display(site)
        ))
    })?;
    let key = project_key(
        host,
        endpoints,
        site,
        business,
        &org,
        &project,
        "zcode-api-key",
        None,
        device_file,
    )
    .await?;
    let root = endpoints.biz_root(base_for(site));
    let url = format!("{root}/api/biz/subscription/list");
    let data = api::get_envelope(&api::EnvelopeRequest::get(
        host,
        endpoints,
        &url,
        Some(&key),
        &[],
        device_file,
    ))
    .await?;
    let plan = data
        .as_array()
        .into_iter()
        .flatten()
        .find(|plan| text(plan, "status").eq_ignore_ascii_case("VALID"))
        .and_then(|plan| text_owned(plan, "productName"))
        .unwrap_or_default();
    Ok((key, plan))
}

async fn team_seat(
    host: &HostContext,
    endpoints: &Endpoints,
    site: Site,
    business: &SecretString,
    info: &Value,
    device_file: &std::path::Path,
) -> Result<Option<Signed>, PluginError> {
    for (org, project) in team_projects(info) {
        let root = endpoints.biz_root(base_for(site));
        let url = format!("{root}/api/biz/team/subscribe/product/querySubscribeDetail");
        let headers = team::team_headers(base_for(site), &org, &project);
        let refs: Vec<(&str, &str)> = headers
            .iter()
            .map(|(name, value)| (*name, value.as_str()))
            .collect();
        let data = api::get_envelope(&api::EnvelopeRequest::get(
            host,
            endpoints,
            &url,
            Some(business),
            &refs,
            device_file,
        ))
        .await?;
        let usable = data.get("hasSubscription").and_then(Value::as_bool) != Some(false)
            && text(&data, "status").eq_ignore_ascii_case("EFFECTIVE")
            && text(&data, "memberGrantStatus").eq_ignore_ascii_case("VALID");
        if !usable {
            continue;
        }
        let key = project_key(
            host,
            endpoints,
            site,
            business,
            &org,
            &project,
            "zcode-team-api-key",
            Some(2),
            device_file,
        )
        .await?;
        let mut auth = empty_auth(site);
        auth.api_key = Some(key);
        auth.team_token = Some(business.clone());
        auth.org = org;
        auth.project = project;
        let plan = text_owned(&data, "productName")
            .filter(|name| !name.is_empty())
            .unwrap_or_else(|| "GLM Coding Team".to_owned());
        return Ok(Some(Signed { auth, plan }));
    }
    Ok(None)
}

async fn business_auth(
    host: &HostContext,
    endpoints: &Endpoints,
    site: Site,
    token: &str,
) -> Result<SecretString, PluginError> {
    if site == Site::Bigmodel {
        return Ok(SecretString::from(token.to_owned()));
    }
    let url = format!("{}/api/auth/z/login", crate::ZAI_BIZ);
    let body = json::json!({"token": token});
    let device_file = crate::device::device_path(host)?;
    let request = api::EnvelopeRequest {
        method: "POST",
        body: Some(&body),
        ..api::EnvelopeRequest::get(host, endpoints, &url, None, &[], &device_file)
    };
    let data = api::get_envelope(&request).await?;
    let access = text(&data, "access_token");
    if access.is_empty() {
        return Err(PluginError::auth("Z.ai sign-in returned no business token"));
    }
    Ok(SecretString::from(format!("Bearer {access}")))
}

#[allow(clippy::too_many_arguments)]
async fn project_key(
    host: &HostContext,
    endpoints: &Endpoints,
    site: Site,
    business: &SecretString,
    org: &str,
    project: &str,
    name: &str,
    key_type: Option<i64>,
    device_file: &std::path::Path,
) -> Result<SecretString, PluginError> {
    let root = endpoints.biz_root(base_for(site));
    let url = format!(
        "{root}/api/biz/v1/organization/{}/projects/{}/api_keys",
        encode(org),
        encode(project)
    );
    let headers = if key_type.is_some() {
        team::team_headers(base_for(site), org, project)
    } else {
        Vec::new()
    };
    let refs: Vec<(&str, &str)> = headers
        .iter()
        .map(|(name, value)| (*name, value.as_str()))
        .collect();
    let data = api::get_envelope(&api::EnvelopeRequest::get(
        host,
        endpoints,
        &url,
        Some(business),
        &refs,
        device_file,
    ))
    .await?;
    let mut id = data
        .as_array()
        .into_iter()
        .flatten()
        .find(|key| {
            text(key, "name") == name
                && key_type.is_none_or(|kind| number(key.get("keyType")) == Some(kind as f64))
        })
        .and_then(|key| text_owned(key, "apiKey"))
        .unwrap_or_default();
    if id.is_empty() {
        let body = key_type.map_or_else(
            || json::json!({"name": name}),
            |kind| json::json!({"name": name, "keyType": kind}),
        );
        let request = api::EnvelopeRequest {
            method: "POST",
            body: Some(&body),
            ..api::EnvelopeRequest::get(host, endpoints, &url, Some(business), &refs, device_file)
        };
        let created = api::get_envelope(&request).await?;
        id = text(&created, "apiKey").to_owned();
    }
    if id.is_empty() {
        return Err(PluginError::decode(
            "Z Code key creation returned no key id",
        ));
    }
    let copy = format!("{url}/copy/{}", encode(&id));
    let secret = api::get_envelope(&api::EnvelopeRequest::get(
        host,
        endpoints,
        &copy,
        Some(business),
        &refs,
        device_file,
    ))
    .await?;
    let secret = text(&secret, "secretKey");
    if secret.is_empty() && key_type.is_some() {
        return Ok(SecretString::from(id));
    }
    if secret.is_empty() {
        return Err(PluginError::decode("Z Code key copy returned no secret"));
    }
    Ok(SecretString::from(format!("{id}.{secret}")))
}

fn own_project(info: &Value) -> Option<(String, String)> {
    for org in info
        .get("organizations")
        .and_then(Value::as_array)
        .into_iter()
        .flatten()
    {
        let Some(org_id) = text_owned(org, "organizationId").filter(|id| !id.is_empty()) else {
            continue;
        };
        let project = org
            .get("projects")
            .and_then(Value::as_array)
            .into_iter()
            .flatten()
            .find(|project| number(project.get("projectType")) != Some(2.0))
            .and_then(|project| text_owned(project, "projectId"))
            .filter(|id| !id.is_empty());
        if let Some(project) = project {
            return Some((org_id, project));
        }
    }
    None
}

fn team_projects(info: &Value) -> Vec<(String, String)> {
    info.get("organizations")
        .and_then(Value::as_array)
        .into_iter()
        .flatten()
        .flat_map(|org| {
            let org_id = text(org, "organizationId").to_owned();
            org.get("projects")
                .and_then(Value::as_array)
                .into_iter()
                .flatten()
                .filter(|project| number(project.get("projectType")) == Some(2.0))
                .filter_map(|project| text_owned(project, "projectId"))
                .map(|project| (org_id.clone(), project))
                .collect::<Vec<_>>()
        })
        .filter(|(org, project)| !org.is_empty() && !project.is_empty())
        .collect()
}

fn retryable_poll(error: &PluginError) -> bool {
    match error.status() {
        Some(408 | 429) => true,
        Some(status) => status >= 500,
        None => true,
    }
}

fn site(ctx: &SignInCtx) -> Result<Site, PluginError> {
    match ctx.option("site").unwrap_or("zai") {
        "zai" => Ok(Site::Zai),
        "bigmodel" => Ok(Site::Bigmodel),
        _ => Err(PluginError::config("site must be `zai` or `bigmodel`")),
    }
}

fn empty_auth(site: Site) -> PlanAuth {
    PlanAuth {
        api_key: None,
        jwt: None,
        team_token: None,
        org: String::new(),
        project: String::new(),
        base: base_for(site).to_owned(),
        site,
        uid: String::new(),
        label: site_display(site).to_owned(),
        follow_agent: false,
        agent_credentials: None,
    }
}

fn base_for(site: Site) -> &'static str {
    match site {
        Site::Zai => ZAI_ANTHROPIC_BASE,
        Site::Bigmodel => BIGMODEL_ANTHROPIC_BASE,
    }
}

fn site_name(site: Site) -> &'static str {
    match site {
        Site::Zai => "zai",
        Site::Bigmodel => "bigmodel",
    }
}

fn site_display(site: Site) -> &'static str {
    match site {
        Site::Zai => "Z.ai",
        Site::Bigmodel => "BigModel",
    }
}

fn nonempty_secret(text: &str) -> Option<SecretString> {
    (!text.trim().is_empty()).then(|| SecretString::from(text.trim().to_owned()))
}

fn who(user: &Value) -> String {
    for key in ["email", "name", "user_id"] {
        let value = text(user, key).trim();
        if !value.is_empty() {
            return value
                .strip_suffix("@phone.local")
                .unwrap_or(value)
                .to_owned();
        }
    }
    "Z Code".to_owned()
}

fn text<'a>(value: &'a Value, key: &str) -> &'a str {
    value.get(key).and_then(Value::as_str).unwrap_or("")
}

fn text_owned(value: &Value, key: &str) -> Option<String> {
    value.get(key).and_then(Value::as_str).map(str::to_owned)
}

fn number(value: Option<&Value>) -> Option<f64> {
    value.and_then(|value| match value {
        Value::Number(number) => number.as_f64(),
        Value::String(text) => text.trim().parse().ok(),
        _ => None,
    })
}

fn encode(text: &str) -> String {
    hofund_plugin_sdk::url::form_urlencoded::byte_serialize(text.as_bytes()).collect()
}

fn uuid() -> Result<String, PluginError> {
    hofund_plugin_sdk::uuid::v4().map_err(|error| {
        PluginError::internal("cannot create a Z Code sign-in id").with_source(error)
    })
}
