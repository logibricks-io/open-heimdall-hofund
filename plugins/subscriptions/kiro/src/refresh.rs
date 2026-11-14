//! Refresh Kiro sign-ins through their owner or the desktop and OIDC endpoints.

use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::Duration;

use hofund_plugin_api::{
    AccountId, BinaryName, HostContext, PluginError, SecretString, SpawnRequest,
};
use hofund_plugin_sdk::json::Value;
use hofund_plugin_sdk::oauth::{SecretObject, form_body, same_secret, secret_text};
use hofund_plugin_sdk::{Call, json, process, tracing, url};

use crate::Endpoints;
use crate::cli_db;
use crate::creds::{self, Credential, Stored};

const LEASE_WAIT: Duration = Duration::from_secs(30);
const CLI_WAIT: Duration = Duration::from_secs(20);

fn method_is(method: &str, tag: &str) -> bool {
    method.eq_ignore_ascii_case(tag)
}

pub(crate) async fn ensure_fresh(
    host: &HostContext,
    account: &AccountId,
    endpoints: Arc<Endpoints>,
    cli_command: Option<&Path>,
) -> Result<Stored, PluginError> {
    let stored = Stored::load(host, account).await?;
    if stored.fresh_at(host.clock().now()) {
        return Ok(stored);
    }
    refresh_stale(host, account, &stored, &endpoints, cli_command).await?;
    Stored::load(host, account).await
}

pub(crate) async fn refresh_stale(
    host: &HostContext,
    account: &AccountId,
    stored: &Stored,
    endpoints: &Endpoints,
    cli_command: Option<&Path>,
) -> Result<(), PluginError> {
    if method_is(&stored.credential.method, "apikey") {
        return Ok(());
    }
    let lease = host.accounts().lease(account, LEASE_WAIT).await?;
    let blob = SecretObject::parse(lease.secret())?;
    let current_refresh = blob
        .secret("refresh_token")
        .unwrap_or_else(|| SecretString::from(String::new()));
    if !same_secret(&current_refresh, &stored.credential.refresh) {
        return Ok(());
    }
    let now = host.clock().now();
    let mut credential = stored.credential.clone();
    let mut refresh_used = credential.refresh.clone();
    if let Some(on_disk) = read_source(&credential) {
        if on_disk.fresh_at(now) && !same_secret(&on_disk.access, &credential.access) {
            lease
                .commit(
                    Some(on_disk.label.clone()),
                    creds::secret_blob(&on_disk).to_secret(),
                )
                .await?;
            return Ok(());
        }
        if on_disk.fresh_at(now) && source_fields_differ(&stored.credential, &on_disk) {
            lease
                .commit(
                    Some(on_disk.label.clone()),
                    creds::secret_blob(&on_disk).to_secret(),
                )
                .await?;
            return Ok(());
        }
        credential = on_disk;
        refresh_used = credential.refresh.clone();
    }
    if !credential.db_key.is_empty()
        && let Some(renewed) = delegated_cli(host, &credential, cli_command).await
    {
        lease
            .commit(
                Some(renewed.label.clone()),
                creds::secret_blob(&renewed).to_secret(),
            )
            .await?;
        return Ok(());
    }
    if secret_text(&credential.refresh).is_empty() {
        return Err(PluginError::auth(
            "Kiro sign-in has expired; sign in again with the Kiro IDE or kiro-cli",
        ));
    }
    let root = if method_is(&credential.method, "idc") && !credential.client_id.is_empty() {
        refresh_idc(host, &credential).await?
    } else if method_is(&credential.method, "external-idp") {
        refresh_external(host, &credential).await?
    } else {
        refresh_social(host, &credential, endpoints).await?
    };
    let access = root
        .get("accessToken")
        .or_else(|| root.get("access_token"))
        .and_then(Value::as_str)
        .filter(|token| !token.is_empty())
        .ok_or_else(|| PluginError::auth("Kiro refresh sent back no access token"))?;
    let refresh = root
        .get("refreshToken")
        .or_else(|| root.get("refresh_token"))
        .and_then(Value::as_str)
        .filter(|token| !token.is_empty())
        .map(SecretString::from)
        .unwrap_or_else(|| credential.refresh.clone());
    let expires_in = root
        .get("expiresIn")
        .or_else(|| root.get("expires_in"))
        .and_then(Value::as_u64)
        .filter(|seconds| *seconds > 0)
        .unwrap_or(3600);
    let profile = root
        .get("profileArn")
        .and_then(Value::as_str)
        .unwrap_or(credential.profile_arn.as_str());
    credential.access = SecretString::from(access.to_owned());
    credential.refresh = refresh;
    credential.profile_arn = profile.to_owned();
    credential.expires_at = host
        .clock()
        .now()
        .checked_add(Duration::from_secs(expires_in));
    lease
        .commit(
            Some(credential.label.clone()),
            creds::secret_blob(&credential).to_secret(),
        )
        .await?;
    write_back(&credential, &refresh_used);
    Ok(())
}

fn source_fields_differ(stored: &Credential, on_disk: &Credential) -> bool {
    stored.profile_arn != on_disk.profile_arn
        || stored.region != on_disk.region
        || stored.label != on_disk.label
        || stored.method != on_disk.method
        || stored.provider != on_disk.provider
        || stored.client_id != on_disk.client_id
        || stored.client_id_hash != on_disk.client_id_hash
        || stored.token_url != on_disk.token_url
        || stored.expires_at != on_disk.expires_at
        || !same_secret(&stored.client_secret, &on_disk.client_secret)
}

fn read_source(credential: &Credential) -> Option<Credential> {
    if !credential.db_key.is_empty() && !credential.cli_db_path.is_empty() {
        return cli_db::read_at(Path::new(&credential.cli_db_path))
            .ok()
            .flatten()
            .filter(|read| read.db_key == credential.db_key);
    }
    if !credential.ide_token_path.is_empty() {
        return creds::read_token_file(Path::new(&credential.ide_token_path))
            .ok()
            .map(|mut read| {
                read.ide_token_path.clone_from(&credential.ide_token_path);
                read
            });
    }
    None
}

async fn delegated_cli(
    host: &HostContext,
    credential: &Credential,
    command: Option<&Path>,
) -> Option<Credential> {
    let program = cli_program(command)?;
    let cwd = program.parent().unwrap_or_else(|| Path::new("/"));
    let request = SpawnRequest::new(
        &program,
        vec!["debug".into(), "refresh-auth-token".into()],
        Vec::new(),
        cwd,
    )
    .ok()?;
    let child = host.spawn_process().spawn_piped(&request).await.ok()?;
    let _ = hofund_plugin_sdk::timeout(CLI_WAIT, child.exit_code()).await;
    let renewed = cli_db::read_at(Path::new(&credential.cli_db_path))
        .ok()
        .flatten()?;
    (renewed.db_key == credential.db_key
        && !same_secret(&renewed.access, &credential.access)
        && renewed.fresh_at(host.clock().now()))
    .then_some(renewed)
}

fn cli_program(command: Option<&Path>) -> Option<PathBuf> {
    if let Some(command) = command {
        return Some(command.to_path_buf());
    }
    if let Some(program) = process::find_program(BinaryName::new("kiro-cli")) {
        return Some(program);
    }
    let home = std::env::var_os("HOME")
        .or_else(|| std::env::var_os("USERPROFILE"))
        .map(PathBuf::from);
    let mut candidates = Vec::new();
    if let Some(home) = home {
        candidates.push(home.join(".local/bin/kiro-cli"));
    }
    candidates.extend([
        PathBuf::from("/Applications/Kiro CLI.app/Contents/MacOS/kiro-cli"),
        PathBuf::from("/usr/local/bin/kiro-cli"),
        PathBuf::from("/opt/homebrew/bin/kiro-cli"),
    ]);
    candidates.into_iter().find(|path| path.is_file())
}

#[allow(clippy::collapsible_if)]
fn write_back(credential: &Credential, refresh_used: &SecretString) {
    if !credential.db_key.is_empty() && !credential.cli_db_path.is_empty() {
        if let Err(error) = cli_db::write_refresh(
            Path::new(&credential.cli_db_path),
            &credential.db_key,
            credential,
            refresh_used,
        ) {
            tracing::warn!(
                target: "hofund_plugin_subscription_kiro",
                "kiro-cli refresh write-back failed: {}",
                error.message()
            );
        }
    } else if !credential.ide_token_path.is_empty() {
        if let Err(error) = creds::write_token_refresh_fields(
            Path::new(&credential.ide_token_path),
            &credential.access,
            &credential.refresh,
            credential.expires_at,
            refresh_used,
        ) {
            tracing::warn!(
                target: "hofund_plugin_subscription_kiro",
                "Kiro IDE token refresh write-back failed: {}",
                error.message()
            );
        }
    }
}

async fn refresh_social(
    host: &HostContext,
    credential: &Credential,
    endpoints: &Endpoints,
) -> Result<Value, PluginError> {
    let url = endpoints.refresh_url(&credential.region);
    let body = json::json!({"refreshToken": secret_text(&credential.refresh)});
    let reply = Call::post(url)
        .header("content-type", "application/json")
        .header("accept", "application/json")
        .header("user-agent", "Kiro-Desktop/0.2.13 (darwin; arm64)")
        .bytes(json::to_vec(&body).expect("json"))
        .fetch(host.http())
        .await?;
    refresh_status(reply.status(), reply.body())
}

async fn refresh_idc(host: &HostContext, credential: &Credential) -> Result<Value, PluginError> {
    let url = format!("https://oidc.{}.amazonaws.com/token", credential.region);
    let body = json::json!({
        "clientId": credential.client_id,
        "clientSecret": secret_text(&credential.client_secret),
        "refreshToken": secret_text(&credential.refresh),
        "grantType": "refresh_token",
    });
    let reply = Call::post(url)
        .header("content-type", "application/json")
        .header("accept", "application/json")
        .bytes(json::to_vec(&body).expect("json"))
        .fetch(host.http())
        .await?;
    refresh_status(reply.status(), reply.body())
}

async fn refresh_external(
    host: &HostContext,
    credential: &Credential,
) -> Result<Value, PluginError> {
    if credential.token_url.is_empty() {
        return Err(PluginError::auth(
            "Kiro external-idp sign-in has no token URL; sign in again with kiro-cli",
        ));
    }
    let valid = url::Url::parse(&credential.token_url).is_ok_and(|url| {
        url.scheme() == "https"
            && url.host_str().is_some()
            && url.username().is_empty()
            && url.password().is_none()
    });
    if !valid {
        return Err(PluginError::auth(
            "Kiro external-idp token URL must be HTTPS without userinfo",
        ));
    }
    let body = form_body(&[
        ("grant_type", "refresh_token"),
        ("client_id", &credential.client_id),
        ("refresh_token", secret_text(&credential.refresh)),
    ]);
    let reply = Call::post(credential.token_url.clone())
        .header("content-type", "application/x-www-form-urlencoded")
        .header("accept", "application/json")
        .bytes(body.into_bytes())
        .fetch(host.http())
        .await?;
    refresh_status(reply.status(), reply.body())
}

fn refresh_status(status: u16, body: &[u8]) -> Result<Value, PluginError> {
    if status == 400 || status == 401 || status == 403 {
        let _ = body;
        return Err(PluginError::auth(
            "Kiro refused to refresh the sign-in; sign in again",
        ));
    }
    if !(200..300).contains(&status) {
        return Err(PluginError::status_error(
            status,
            "Kiro did not refresh the sign-in",
        ));
    }
    json::from_slice(body).map_err(|_| PluginError::decode("Kiro refresh reply is not JSON"))
}
