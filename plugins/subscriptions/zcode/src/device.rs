//! Device sign-in against zcode.z.ai with X-Device-Mid.

use std::path::PathBuf;
use std::sync::Mutex;
use std::time::Duration;

use hofund_plugin_api::{AccountId, HostContext, PluginError, SecretString};
use hofund_plugin_sdk::json;
use hofund_plugin_sdk::oauth::{device, secret_text};
use hofund_plugin_sdk::{Call, sleep};

use crate::creds::{self, PlanAuth, Site};
use crate::{Endpoints, ZAI_ANTHROPIC_BASE};

/// Device authorization waiting for approval.
pub(crate) struct DevicePending {
    pub(crate) start: device::DeviceStart,
}

pub(crate) fn verification_page(endpoints: &Endpoints) -> String {
    endpoints.device_page.clone()
}

pub(crate) async fn start(
    host: &HostContext,
    endpoints: &Endpoints,
    device_path: &Mutex<Option<PathBuf>>,
) -> Result<DevicePending, PluginError> {
    let mid = device_mid(device_path)?;
    let reply = Call::post(&endpoints.device_authorize)
        .header("content-type", "application/json")
        .header("accept", "application/json")
        .header("x-device-mid", &mid)
        .bytes(json::json!({ "client": "hofund" }).to_string())
        .fetch(host.http())
        .await?;
    if !(200..300).contains(&reply.status()) {
        return Err(PluginError::status_error(
            reply.status(),
            "Z Code device sign-in could not start",
        ));
    }
    let start = device::parse_start(reply.body())?;
    Ok(DevicePending { start })
}

pub(crate) async fn finish(
    host: &HostContext,
    endpoints: &Endpoints,
    pending: &DevicePending,
    device_path: &Mutex<Option<PathBuf>>,
) -> Result<SignedIn, PluginError> {
    let mid = device_mid(device_path)?;
    let token_url = endpoints.device_token.clone();
    let http = host.http();
    let set = device::poll(host.clock(), &pending.start, || {
        Call::post(token_url.clone())
            .header("content-type", "application/x-www-form-urlencoded")
            .header("accept", "application/json")
            .header("x-device-mid", &mid)
            .bytes(device::device_code_form("hofund", &pending.start))
            .fetch(http)
    })
    .await?;
    let access = set.access().clone();
    let label = "Z Code".to_owned();
    let uid = String::new();
    let mut auth = PlanAuth {
        api_key: None,
        jwt: Some(access.clone()),
        team_token: None,
        org: String::new(),
        project: String::new(),
        base: ZAI_ANTHROPIC_BASE.to_owned(),
        site: Site::Zai,
        uid,
        label: label.clone(),
        follow_agent: false,
        agent_credentials: None,
    };
    if secret_text(&access).contains('.') && !secret_text(&access).contains(' ') {
        auth.api_key = Some(access.clone());
        auth.jwt = None;
    }
    let id = creds::account_id_for(&auth);
    let blob = creds::secret_blob(&auth, &mid);
    Ok(SignedIn {
        id,
        label,
        secret: blob.to_secret(),
    })
}

pub(crate) struct SignedIn {
    pub(crate) id: AccountId,
    pub(crate) label: String,
    pub(crate) secret: SecretString,
}

pub(crate) fn device_mid(device_path: &Mutex<Option<PathBuf>>) -> Result<String, PluginError> {
    let path = device_file(device_path)?;
    if let Ok(text) = std::fs::read_to_string(&path) {
        let id = text.trim().to_ascii_lowercase();
        if uuid_v4(&id) {
            return Ok(id);
        }
    }
    let id = new_uuid_v4();
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent)
            .map_err(|_| PluginError::io("cannot create the Z Code device id directory"))?;
    }
    std::fs::write(&path, format!("{id}\n"))
        .map_err(|_| PluginError::io("cannot write the Z Code device id"))?;
    Ok(id)
}

fn device_file(device_path: &Mutex<Option<PathBuf>>) -> Result<PathBuf, PluginError> {
    let mut guard = device_path
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner);
    if guard.is_none() {
        *guard = Some(default_device_path());
    }
    Ok(guard.clone().expect("path"))
}

fn default_device_path() -> PathBuf {
    std::env::var("HOFUND_STATE_DIR")
        .map(PathBuf::from)
        .unwrap_or_else(|_| PathBuf::from(".hofund"))
        .join("zcode-device-mid")
}

fn uuid_v4(text: &str) -> bool {
    let bytes: Vec<u8> = text.as_bytes().to_vec();
    if bytes.len() != 36 {
        return false;
    }
    let pattern = b"xxxxxxxx-xxxx-xxxx-xxxx-xxxxxxxxxxxx";
    for (index, byte) in bytes.iter().enumerate() {
        match pattern[index] {
            b'x' => {
                if !byte.is_ascii_hexdigit() {
                    return false;
                }
            }
            b'-' if *byte != b'-' => return false,
            _ => {}
        }
    }
    true
}

fn new_uuid_v4() -> String {
    use std::time::{SystemTime, UNIX_EPOCH};
    let nanos = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_nanos())
        .unwrap_or(0);
    let mut bytes = [0_u8; 16];
    for (index, slot) in bytes.iter_mut().enumerate() {
        *slot = ((nanos >> (index * 4)) & 0xff) as u8;
    }
    bytes[6] = (bytes[6] & 0x0f) | 0x40;
    bytes[8] = (bytes[8] & 0x3f) | 0x80;
    format!(
        "{:02x}{:02x}{:02x}{:02x}-{:02x}{:02x}-{:02x}{:02x}-{:02x}{:02x}-{:02x}{:02x}{:02x}{:02x}{:02x}{:02x}",
        bytes[0],
        bytes[1],
        bytes[2],
        bytes[3],
        bytes[4],
        bytes[5],
        bytes[6],
        bytes[7],
        bytes[8],
        bytes[9],
        bytes[10],
        bytes[11],
        bytes[12],
        bytes[13],
        bytes[14],
        bytes[15]
    )
}

#[allow(dead_code)]
pub(crate) async fn poll_sleep(interval: Duration) {
    sleep(interval).await;
}
