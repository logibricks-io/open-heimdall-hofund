//! Kiro portal and AWS browser sign-in over a local callback listener.

use std::net::Ipv4Addr;
use std::sync::{Arc, Mutex, MutexGuard};
use std::time::Duration;

use hofund_plugin_api::{HostContext, PluginError, SecretString, SignInChallenge, SignInCtx};
use hofund_plugin_sdk::json::{self, Value};
use hofund_plugin_sdk::oauth::{Pkce, new_state_with_random_bytes, secret_text};
use hofund_plugin_sdk::{Call, url};
use tokio::io::{AsyncReadExt as _, AsyncWriteExt as _};
use tokio::net::{TcpListener, TcpStream};
use tokio::sync::oneshot;

use crate::Endpoints;
use crate::creds::{self, Credential};

const BUILDER_ID_START: &str = "https://view.awsapps.com/start";
const SCOPES: &str = "codewhisperer:completions,codewhisperer:analysis,codewhisperer:conversations,codewhisperer:transformations,codewhisperer:taskassist";
const MAX_HEAD: usize = 8 * 1024;
const HEADER_WAIT: Duration = Duration::from_secs(10);

pub(crate) struct Pending {
    shared: Arc<Shared>,
    receiver: Mutex<Option<oneshot::Receiver<Result<Credential, PluginError>>>>,
}

impl Drop for Pending {
    fn drop(&mut self) {
        self.shared.cancel();
    }
}

pub(crate) async fn start(
    ctx: &SignInCtx,
    host: &HostContext,
    endpoints: Arc<Endpoints>,
) -> Result<(Pending, SignInChallenge), PluginError> {
    let choice = ctx.option("site").unwrap_or("portal");
    if !matches!(choice, "portal" | "builderid" | "enterprise") {
        return Err(PluginError::config("unknown Kiro sign-in method"));
    }
    let issuer = if choice == "enterprise" {
        ctx.option("host").unwrap_or("").trim().to_owned()
    } else {
        BUILDER_ID_START.to_owned()
    };
    let region = ctx
        .option("region")
        .unwrap_or("us-east-1")
        .trim()
        .to_owned();
    if choice == "enterprise" && !https_url(&issuer) {
        return Err(PluginError::config(
            "enter the organization's HTTPS Start URL",
        ));
    }
    if choice != "portal" && !valid_region(&region) {
        return Err(PluginError::config("enter a valid AWS region"));
    }

    let (listener, port) = listen(&endpoints.callback_ports).await?;
    let (sender, receiver) = oneshot::channel();
    let shared = Arc::new(Shared {
        host: host.clone(),
        endpoints,
        shutdown: tokio::sync::Notify::new(),
        state: Mutex::new(Flow {
            stage: Stage::Portal,
            busy: false,
            port,
            portal_state: new_state_with_random_bytes(24)?,
            portal_verifier: Pkce::generate_with_random_bytes(48)?.verifier,
            aws: None,
            outcome: Some(sender),
        }),
    });
    let url = if choice == "portal" {
        portal_url(&shared)?
    } else {
        start_aws(&shared, choice, &issuer, &region).await?
    };
    tokio::spawn(serve(listener, Arc::clone(&shared)));
    Ok((
        Pending {
            shared,
            receiver: Mutex::new(Some(receiver)),
        },
        SignInChallenge::open_url(url)?,
    ))
}

fn portal_url(shared: &Shared) -> Result<String, PluginError> {
    let flow = shared.lock();
    let pkce = Pkce::generate_with_random_bytes(48)?;
    drop(flow);
    let verifier = pkce.verifier;
    let challenge = pkce.challenge;
    let mut flow = shared.lock();
    flow.portal_verifier = verifier;
    let mut url = url::Url::parse(&shared.endpoints.portal)
        .map_err(|_| PluginError::internal("Kiro portal URL is invalid"))?;
    if url.scheme() != "https" || url.host_str().is_none() {
        return Err(PluginError::config("Kiro portal URL must be HTTPS"));
    }
    url.set_path("/signin");
    url.query_pairs_mut()
        .append_pair("state", &flow.portal_state)
        .append_pair("code_challenge", &challenge)
        .append_pair("code_challenge_method", "S256")
        .append_pair("redirect_uri", &format!("http://localhost:{}", flow.port))
        .append_pair("redirect_from", "KiroIDE");
    Ok(url.to_string())
}

async fn listen(ports: &[u16]) -> Result<(TcpListener, u16), PluginError> {
    for port in ports {
        if let Ok(listener) = TcpListener::bind((Ipv4Addr::LOCALHOST, *port)).await {
            let port = listener
                .local_addr()
                .map_err(|error| PluginError::io("Kiro callback address").with_source(error))?
                .port();
            return Ok((listener, port));
        }
    }
    Err(PluginError::io(
        "the ports Kiro's sign-in uses are all busy; close the Kiro IDE's sign-in and try again",
    ))
}

struct Shared {
    host: HostContext,
    endpoints: Arc<Endpoints>,
    shutdown: tokio::sync::Notify,
    state: Mutex<Flow>,
}

impl Shared {
    fn lock(&self) -> MutexGuard<'_, Flow> {
        self.state
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
    }

    fn cancel(&self) {
        let mut flow = self.lock();
        flow.stage = Stage::Over;
        flow.outcome.take();
        drop(flow);
        self.shutdown.notify_one();
    }

    fn fail(&self, error: PluginError) {
        let mut flow = self.lock();
        flow.stage = Stage::Over;
        if let Some(outcome) = flow.outcome.take() {
            let _ = outcome.send(Err(error));
        }
        drop(flow);
        self.shutdown.notify_one();
    }

    fn is_over(&self) -> bool {
        self.lock().stage == Stage::Over
    }

    async fn succeed(&self, mut credential: Credential) {
        identify(&self.host, &self.endpoints, &mut credential).await;
        let mut flow = self.lock();
        if flow.stage == Stage::Over {
            return;
        }
        flow.stage = Stage::Over;
        if let Some(outcome) = flow.outcome.take() {
            let _ = outcome.send(Ok(credential));
        }
        drop(flow);
        self.shutdown.notify_one();
    }
}

#[derive(Clone, Copy, PartialEq, Eq)]
enum Stage {
    Portal,
    Aws,
    Over,
}

struct Flow {
    stage: Stage,
    busy: bool,
    port: u16,
    portal_state: String,
    portal_verifier: SecretString,
    aws: Option<Aws>,
    outcome: Option<oneshot::Sender<Result<Credential, PluginError>>>,
}

struct Aws {
    provider: String,
    region: String,
    client_id: String,
    client_secret: SecretString,
    state: String,
    verifier: SecretString,
    authorize_url: String,
    portal_choice: String,
    issuer: String,
}

pub(crate) async fn complete(pending: Pending) -> Result<Credential, PluginError> {
    let receiver = pending
        .receiver
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner)
        .take()
        .ok_or_else(|| PluginError::config("Kiro browser sign-in is already completing"))?;
    let result = receiver.await.map_err(|_| {
        PluginError::cancelled("Kiro browser sign-in was cancelled before it finished")
    })?;
    pending.shared.cancel();
    result
}

async fn serve(listener: TcpListener, shared: Arc<Shared>) {
    loop {
        if shared.is_over() {
            break;
        }
        tokio::select! {
            () = shared.shutdown.notified() => break,
            accepted = listener.accept() => {
                let Ok((stream, _)) = accepted else {
                    break;
                };
                tokio::spawn(answer(stream, Arc::clone(&shared)));
            }
        }
    }
}

async fn answer(mut stream: TcpStream, shared: Arc<Shared>) {
    let target = tokio::time::timeout(HEADER_WAIT, read_target(&mut stream)).await;
    let Ok(Some(target)) = target else {
        let _ = page(
            &mut stream,
            400,
            "Bad request",
            "This request was not understood.",
        )
        .await;
        return;
    };
    match callback(&shared, &target).await {
        CallbackReply::Page(status, title, text) => {
            let _ = page(&mut stream, status, &title, &text).await;
        }
        CallbackReply::Redirect(location) => {
            let _ = redirect(&mut stream, &location).await;
        }
    }
}

async fn read_target(stream: &mut TcpStream) -> Option<String> {
    let mut head = Vec::with_capacity(1024);
    let mut chunk = [0_u8; 1024];
    while !head.windows(4).any(|window| window == b"\r\n\r\n") {
        if head.len() >= MAX_HEAD {
            return None;
        }
        let read = stream.read(&mut chunk).await.ok()?;
        if read == 0 {
            return None;
        }
        head.extend_from_slice(&chunk[..read]);
    }
    let line = head.split(|byte| *byte == b'\n').next()?;
    let line = std::str::from_utf8(line).ok()?.trim_end_matches('\r');
    let mut parts = line.split(' ');
    let (method, target, version) = (parts.next()?, parts.next()?, parts.next()?);
    if method != "GET" || !version.starts_with("HTTP/1.") || !target.starts_with('/') {
        return None;
    }
    Some(target.to_owned())
}

enum CallbackReply {
    Page(u16, String, String),
    Redirect(String),
}

fn not_ours() -> CallbackReply {
    CallbackReply::Page(
        404,
        "Not this sign-in".to_owned(),
        "This link is not from the sign-in in progress.".to_owned(),
    )
}

fn failed() -> CallbackReply {
    CallbackReply::Page(
        200,
        "Sign-in did not finish".to_owned(),
        "Return to hofund for the reason, then start again.".to_owned(),
    )
}

async fn callback(shared: &Arc<Shared>, target: &str) -> CallbackReply {
    let parsed = url::Url::parse(&format!("http://127.0.0.1{target}"));
    let Ok(url) = parsed else {
        return not_ours();
    };
    if !matches!(url.path(), "/oauth/callback" | "/signin/callback") {
        return not_ours();
    }
    let query = url.query().unwrap_or("").as_bytes();
    let params = |name: &str| {
        url::form_urlencoded::parse(query)
            .find(|(key, _)| key == name)
            .map(|(_, value)| value.into_owned())
    };
    let state = params("state").unwrap_or_default();
    let (portal, aws, busy) = {
        let flow = shared.lock();
        if flow.stage == Stage::Over {
            return CallbackReply::Page(
                404,
                "Sign-in over".to_owned(),
                "Start again from hofund.".to_owned(),
            );
        }
        let portal = state == flow.portal_state;
        let aws = flow.stage == Stage::Aws
            && flow
                .aws
                .as_ref()
                .is_some_and(|aws| !aws.state.is_empty() && state == aws.state);
        (portal, aws, flow.busy)
    };
    if !portal && !aws {
        return not_ours();
    }
    if busy {
        return CallbackReply::Page(
            409,
            "Already finishing".to_owned(),
            "This sign-in is already finishing with this address.".to_owned(),
        );
    }
    if params("error").is_some() {
        shared.fail(PluginError::auth("the provider refused Kiro sign-in"));
        return failed();
    }
    if aws {
        return aws_callback(shared, &url, params("code")).await;
    }
    portal_callback(shared, &url, params).await
}

async fn portal_callback<F>(shared: &Arc<Shared>, url: &url::Url, params: F) -> CallbackReply
where
    F: Fn(&str) -> Option<String>,
{
    let option = params("login_option").unwrap_or_default();
    let known = matches!(
        option.as_str(),
        "google" | "github" | "builderid" | "awsidc" | "internal" | "external_idp"
    );
    if shared.lock().stage == Stage::Aws && !known {
        return not_ours();
    }
    if !known {
        shared.fail(PluginError::auth(
            "Kiro's page returned an unknown sign-in method",
        ));
        return failed();
    }
    let issuer = params("issuer_url").unwrap_or_default();
    let region = params("idc_region").unwrap_or_default();
    {
        let mut flow = shared.lock();
        if flow.stage == Stage::Aws {
            if let Some(aws) = &flow.aws
                && aws.portal_choice == option
                && aws.issuer == issuer
                && aws.region == region
            {
                return CallbackReply::Redirect(aws.authorize_url.clone());
            }
            flow.stage = Stage::Portal;
            flow.aws = None;
        }
        flow.busy = true;
    }
    match option.as_str() {
        "google" | "github" => {
            let Some(code) = params("code").filter(|code| !code.is_empty()) else {
                shared.fail(PluginError::auth("Kiro's page sent back no code"));
                return failed();
            };
            match social_token(shared, code, url.path(), &option).await {
                Ok(credential) => {
                    shared.succeed(credential).await;
                    succeeded(shared)
                }
                Err(error) => {
                    shared.fail(error);
                    failed()
                }
            }
        }
        "builderid" | "awsidc" | "internal" => {
            if issuer.is_empty() || region.is_empty() {
                shared.fail(PluginError::auth(
                    "Kiro's page did not say where to sign in at AWS",
                ));
                return failed();
            }
            if !valid_region(&region) {
                shared.fail(PluginError::auth("Kiro's page named an invalid AWS region"));
                return failed();
            }
            if !https_url(&issuer) {
                shared.fail(PluginError::auth(
                    "Kiro's page named an AWS sign-in that is not HTTPS",
                ));
                return failed();
            }
            match start_aws(shared, &option, &issuer, &region).await {
                Ok(url) => CallbackReply::Redirect(url),
                Err(error) => {
                    shared.fail(error);
                    failed()
                }
            }
        }
        _ => {
            shared.fail(PluginError::auth(
                "sign in with kiro-cli to use a company's own identity provider",
            ));
            failed()
        }
    }
}

async fn social_token(
    shared: &Arc<Shared>,
    code: String,
    path: &str,
    option: &str,
) -> Result<Credential, PluginError> {
    let (verifier, redirect) = {
        let flow = shared.lock();
        (
            secret_text(&flow.portal_verifier).to_owned(),
            format!("http://localhost:{}", flow.port),
        )
    };
    let body = json::json!({
        "code": code,
        "code_verifier": verifier,
        "redirect_uri": format!("{redirect}{path}?login_option={option}"),
    });
    let token = sign_in_post(
        &shared.host,
        &format!("{}/oauth/token", shared.endpoints.auth),
        body,
    )
    .await?;
    let provider = if option == "google" {
        "Google"
    } else {
        "Github"
    };
    credential_from_token(&shared.host, token, "social", provider, "us-east-1", None)
}

async fn aws_callback(shared: &Arc<Shared>, url: &url::Url, code: Option<String>) -> CallbackReply {
    if url.path() != "/oauth/callback" {
        return not_ours();
    }
    let Some(code) = code.filter(|code| !code.is_empty()) else {
        shared.fail(PluginError::auth("AWS sent back no code"));
        return failed();
    };
    let (region, client_id, client_secret, verifier, redirect, provider) = {
        let mut flow = shared.lock();
        flow.busy = true;
        let Some(aws) = &flow.aws else {
            return not_ours();
        };
        (
            aws.region.clone(),
            aws.client_id.clone(),
            secret_text(&aws.client_secret).to_owned(),
            secret_text(&aws.verifier).to_owned(),
            format!("http://127.0.0.1:{}/oauth/callback", flow.port),
            aws.provider.clone(),
        )
    };
    let body = json::json!({
        "clientId": client_id,
        "clientSecret": client_secret,
        "grantType": "authorization_code",
        "redirectUri": redirect,
        "code": code,
        "codeVerifier": verifier,
    });
    let result = sign_in_post(
        &shared.host,
        &oidc(&shared.endpoints, &region, "/token"),
        body,
    )
    .await;
    match result.and_then(|token| {
        credential_from_token(
            &shared.host,
            token,
            "idc",
            &provider,
            &region,
            Some((client_id, SecretString::from(client_secret))),
        )
    }) {
        Ok(credential) => {
            shared.succeed(credential).await;
            succeeded(shared)
        }
        Err(error) => {
            shared.fail(error);
            failed()
        }
    }
}

fn succeeded(_shared: &Shared) -> CallbackReply {
    CallbackReply::Page(
        200,
        "Signed in".to_owned(),
        "You can close this tab.".to_owned(),
    )
}

async fn start_aws(
    shared: &Arc<Shared>,
    choice: &str,
    issuer: &str,
    region: &str,
) -> Result<String, PluginError> {
    let redirect = {
        let flow = shared.lock();
        format!("http://127.0.0.1:{}/oauth/callback", flow.port)
    };
    let registration = json::json!({
        "clientName": "Kiro IDE",
        "clientType": "public",
        "scopes": SCOPES.split(',').collect::<Vec<_>>(),
        "grantTypes": ["authorization_code", "refresh_token"],
        "redirectUris": ["http://127.0.0.1/oauth/callback"],
        "issuerUrl": issuer,
    });
    let registered = sign_in_post(
        &shared.host,
        &oidc(&shared.endpoints, region, "/client/register"),
        registration,
    )
    .await?;
    let client_id = registered
        .get("clientId")
        .and_then(Value::as_str)
        .filter(|id| !id.is_empty())
        .ok_or_else(|| PluginError::auth("AWS registered no client"))?
        .to_owned();
    let client_secret = SecretString::from(
        registered
            .get("clientSecret")
            .and_then(Value::as_str)
            .unwrap_or("")
            .to_owned(),
    );
    let state = new_state_with_random_bytes(24)?;
    let pkce = Pkce::generate_with_random_bytes(48)?;
    let provider = match choice {
        "builderid" => "BuilderId",
        "internal" => "Internal",
        _ => "Enterprise",
    }
    .to_owned();
    let mut authorize = url::Url::parse(&oidc(&shared.endpoints, region, "/authorize"))
        .map_err(|_| PluginError::internal("AWS authorize URL is invalid"))?;
    authorize
        .query_pairs_mut()
        .append_pair("response_type", "code")
        .append_pair("client_id", &client_id)
        .append_pair("redirect_uri", &redirect)
        .append_pair("scopes", SCOPES)
        .append_pair("state", &state)
        .append_pair("code_challenge", &pkce.challenge)
        .append_pair("code_challenge_method", "S256");
    let authorize_url = authorize.to_string();
    let mut flow = shared.lock();
    if flow.stage == Stage::Over {
        return Err(PluginError::cancelled("Kiro sign-in was cancelled"));
    }
    flow.aws = Some(Aws {
        provider,
        region: region.to_owned(),
        client_id,
        client_secret,
        state,
        verifier: pkce.verifier,
        authorize_url: authorize_url.clone(),
        portal_choice: choice.to_owned(),
        issuer: issuer.to_owned(),
    });
    flow.stage = Stage::Aws;
    flow.busy = false;
    Ok(authorize_url)
}

fn credential_from_token(
    host: &HostContext,
    token: Value,
    method: &str,
    provider: &str,
    region: &str,
    registration: Option<(String, SecretString)>,
) -> Result<Credential, PluginError> {
    let access = token
        .get("accessToken")
        .and_then(Value::as_str)
        .filter(|token| !token.is_empty())
        .ok_or_else(|| PluginError::auth("Kiro sign-in sent back no access token"))?;
    let refresh = token
        .get("refreshToken")
        .and_then(Value::as_str)
        .unwrap_or("");
    let expires_in = token
        .get("expiresIn")
        .and_then(Value::as_u64)
        .filter(|seconds| *seconds > 0)
        .unwrap_or(3600);
    let (client_id, client_secret) =
        registration.unwrap_or_else(|| (String::new(), SecretString::from(String::new())));
    Ok(Credential {
        access: SecretString::from(access.to_owned()),
        refresh: SecretString::from(refresh.to_owned()),
        expires_at: host
            .clock()
            .now()
            .checked_add(Duration::from_secs(expires_in)),
        region: region.to_owned(),
        profile_arn: token
            .get("profileArn")
            .and_then(Value::as_str)
            .unwrap_or("")
            .to_owned(),
        label: "Kiro account".to_owned(),
        method: method.to_owned(),
        provider: provider.to_owned(),
        client_id,
        client_secret,
        client_id_hash: String::new(),
        token_url: String::new(),
        ide_token_path: String::new(),
        db_key: String::new(),
        cli_db_path: String::new(),
    })
}

async fn sign_in_post(host: &HostContext, url: &str, body: Value) -> Result<Value, PluginError> {
    if !https_url(url) {
        return Err(PluginError::config("Kiro sign-in endpoints must use HTTPS"));
    }
    let reply = Call::post(url)
        .header("content-type", "application/json")
        .header("accept", "application/json")
        .header("user-agent", "KiroIDE-1.1.70")
        .bytes(json::to_vec(&body).expect("sign-in JSON"))
        .fetch(host.http())
        .await?;
    if !(200..300).contains(&reply.status()) {
        return Err(PluginError::status_error(
            reply.status(),
            "Kiro sign-in was refused",
        ));
    }
    json::from_slice(reply.body())
        .map_err(|_| PluginError::decode("Kiro sign-in reply is not JSON"))
}

async fn identify(host: &HostContext, endpoints: &Endpoints, credential: &mut Credential) {
    if credential.profile_arn.is_empty() {
        if credential.provider.eq_ignore_ascii_case("BuilderId") {
            credential.profile_arn = creds::BUILDER_ID_PROFILE.to_owned();
        } else {
            credential.profile_arn = first_profile(host, endpoints, credential).await;
        }
    }
    let url = endpoints.usage_url(&credential.profile_arn, &credential.region);
    let reply = Call::get(url)
        .sensitive_header(
            "authorization",
            format!("Bearer {}", secret_text(&credential.access)),
        )
        .header("accept", "application/json")
        .fetch(host.http())
        .await;
    let email = reply
        .ok()
        .filter(|reply| (200..300).contains(&reply.status()))
        .and_then(|reply| json::from_slice::<Value>(reply.body()).ok())
        .and_then(|body| {
            body.get("userInfo")
                .and_then(|info| info.get("email"))
                .and_then(Value::as_str)
                .map(str::to_owned)
        })
        .unwrap_or_default();
    if !email.is_empty() {
        let way = match credential.provider.as_str() {
            "Google" => "Google",
            "Github" => "GitHub",
            "BuilderId" => "Builder ID",
            "Enterprise" => "IAM Identity Center",
            "Internal" => "Internal",
            _ => "",
        };
        credential.label = if way.is_empty() {
            email
        } else {
            format!("{email} ({way})")
        };
    }
}

async fn first_profile(
    host: &HostContext,
    endpoints: &Endpoints,
    credential: &Credential,
) -> String {
    for region in ["us-east-1", "eu-central-1"] {
        let base = endpoints.management.replace("{region}", region);
        let reply = Call::get(format!("{base}/List-Available-Profiles"))
            .sensitive_header(
                "authorization",
                format!("Bearer {}", secret_text(&credential.access)),
            )
            .header("accept", "application/json")
            .fetch(host.http())
            .await;
        let Ok(reply) = reply else { continue };
        if !(200..300).contains(&reply.status()) {
            continue;
        }
        let Ok(body) = json::from_slice::<Value>(reply.body()) else {
            continue;
        };
        if let Some(profile) = body
            .get("profiles")
            .and_then(Value::as_array)
            .and_then(|profiles| profiles.first())
            .and_then(|profile| profile.get("arn"))
            .and_then(Value::as_str)
        {
            return profile.to_owned();
        }
    }
    String::new()
}

fn oidc(endpoints: &Endpoints, region: &str, path: &str) -> String {
    format!("{}{path}", endpoints.oidc.replace("{region}", region))
}

fn https_url(text: &str) -> bool {
    url::Url::parse(text).is_ok_and(|url| {
        url.scheme() == "https"
            && url.host_str().is_some()
            && url.username().is_empty()
            && url.password().is_none()
    })
}

fn valid_region(region: &str) -> bool {
    let parts = region.split('-').collect::<Vec<_>>();
    parts.len() >= 3
        && parts[0].len() == 2
        && parts[0].bytes().all(|byte| byte.is_ascii_lowercase())
        && parts[1..parts.len() - 1]
            .iter()
            .all(|part| !part.is_empty() && part.bytes().all(|byte| byte.is_ascii_lowercase()))
        && !parts[parts.len() - 1].is_empty()
        && parts[parts.len() - 1].len() <= 2
        && parts[parts.len() - 1]
            .bytes()
            .all(|byte| byte.is_ascii_digit())
}

async fn page(stream: &mut TcpStream, status: u16, title: &str, text: &str) -> std::io::Result<()> {
    let reason = match status {
        200 => "OK",
        404 => "Not Found",
        409 => "Conflict",
        _ => "Bad Request",
    };
    let body = format!(
        "<!doctype html><html><head><meta charset=\"utf-8\"><title>{}</title></head><body><p>{}</p></body></html>",
        escape(title),
        escape(text),
    );
    let reply = format!(
        "HTTP/1.1 {status} {reason}\r\nContent-Type: text/html; charset=utf-8\r\nCache-Control: no-store\r\nReferrer-Policy: no-referrer\r\nX-Content-Type-Options: nosniff\r\nConnection: close\r\nContent-Length: {}\r\n\r\n{body}",
        body.len(),
    );
    stream.write_all(reply.as_bytes()).await?;
    stream.shutdown().await
}

async fn redirect(stream: &mut TcpStream, location: &str) -> std::io::Result<()> {
    let reply = format!(
        "HTTP/1.1 302 Found\r\nLocation: {location}\r\nCache-Control: no-store\r\nReferrer-Policy: no-referrer\r\nX-Content-Type-Options: nosniff\r\nConnection: close\r\nContent-Length: 0\r\n\r\n"
    );
    stream.write_all(reply.as_bytes()).await?;
    stream.shutdown().await
}

fn escape(text: &str) -> String {
    text.replace('&', "&amp;")
        .replace('<', "&lt;")
        .replace('>', "&gt;")
        .replace('"', "&quot;")
}
