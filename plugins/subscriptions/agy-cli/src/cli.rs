//! Driving the local `agy` binary.
//!
//! The live `agy` prompt flags are not in the reference commits this plugin
//! ports. This module is hofund's bridge contract, exercised with a scripted
//! child: `agy models` prints a model list, and `agy prompt` reads one JSON
//! object on standard input and prints one JSON object per line. Nothing here
//! downloads or installs `agy`.

use std::ffi::OsString;
use std::path::{Path, PathBuf};

use hofund_plugin_api::{HostContext, PluginError, SpawnRequest};
use hofund_plugin_sdk::json::{self, Value};

use crate::notice::classify_output;

/// Environment variable for one account's agy home.
pub(crate) const HOME_VAR: &str = "AGY_HOME";

/// Environment variable recording the proxy this spawn used.
///
/// A URL is also copied to `HTTPS_PROXY`, `HTTP_PROXY`, and `ALL_PROXY`.
/// `direct` and an empty value set this variable and no HTTP proxy.
pub(crate) const PROXY_VAR: &str = "AGY_PROXY";

const MODELS_ARGS: [&str; 1] = ["models"];
const PROMPT_ARGS: [&str; 1] = ["prompt"];

pub(crate) fn program(command: Option<&PathBuf>) -> Result<PathBuf, PluginError> {
    command.cloned().ok_or_else(|| {
        PluginError::config(
            "set command to the absolute path of the agy binary; hofund does not install it",
        )
    })
}

pub(crate) fn provider_proxy_path(host: &HostContext) -> Result<PathBuf, PluginError> {
    Ok(host.dirs().state_dir()?.join("provider-proxy.txt"))
}

pub(crate) fn read_provider_proxy(host: &HostContext) -> Result<String, PluginError> {
    match std::fs::read_to_string(provider_proxy_path(host)?) {
        Ok(text) => Ok(text.trim().to_owned()),
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => Ok(String::new()),
        Err(error) => Err(PluginError::io(format!(
            "cannot read the agy provider proxy ({error})"
        ))),
    }
}

pub(crate) fn write_provider_proxy(host: &HostContext, proxy: &str) -> Result<(), PluginError> {
    let path = provider_proxy_path(host)?;
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent)
            .map_err(|error| PluginError::io(format!("cannot create agy state ({error})")))?;
    }
    std::fs::write(&path, format!("{proxy}\n"))
        .map_err(|error| PluginError::io(format!("cannot write the agy provider proxy ({error})")))
}

/// The proxy a later call should use: the account's own, else the provider's.
#[must_use]
pub(crate) fn via<'a>(account_proxy: &'a str, provider_proxy: &'a str) -> &'a str {
    if account_proxy.is_empty() {
        provider_proxy
    } else {
        account_proxy
    }
}

pub(crate) fn spawn(
    program: &Path,
    args: &[&str],
    home: &Path,
    via: &str,
) -> Result<SpawnRequest, PluginError> {
    SpawnRequest::new(
        program,
        args.iter().copied().map(OsString::from).collect(),
        proxy_env(home, via),
        home,
    )
}

fn proxy_env(home: &Path, via: &str) -> Vec<(OsString, OsString)> {
    let mut env = vec![
        (OsString::from(HOME_VAR), home.as_os_str().to_owned()),
        (OsString::from(PROXY_VAR), OsString::from(via)),
    ];
    if via.starts_with("http://") || via.starts_with("https://") {
        for name in ["HTTPS_PROXY", "HTTP_PROXY", "ALL_PROXY"] {
            env.push((OsString::from(name), OsString::from(via)));
        }
    }
    env
}

pub(crate) async fn run(
    host: &HostContext,
    request: SpawnRequest,
    stdin: Option<&[u8]>,
) -> Result<String, PluginError> {
    let child = host.spawn_process().spawn_piped(&request).await?;
    if let Some(stdin) = stdin {
        child.write_stdin(stdin.to_vec().into()).await?;
        child.close_stdin().await?;
    }
    let mut stdout = child.stdout()?;
    let mut text = String::new();
    while let Some(chunk) = stdout.next_chunk().await {
        text.push_str(&String::from_utf8_lossy(&chunk?));
    }
    let code = child.exit_code().await?;
    if let Some(error) = classify_output(&text) {
        return Err(error);
    }
    if code != 0 {
        let stderr = child.stderr_tail();
        let tail = String::from_utf8_lossy(&stderr);
        let tail = tail.chars().take(200).collect::<String>();
        let tail = if tail.is_empty() {
            format!("agy exited {code}")
        } else {
            format!("agy exited {code}: {tail}")
        };
        return Err(PluginError::io(tail));
    }
    Ok(text)
}

pub(crate) async fn models(
    host: &HostContext,
    program: &Path,
    home: &Path,
    via: &str,
) -> Result<Vec<(String, String)>, PluginError> {
    let text = run(host, spawn(program, &MODELS_ARGS, home, via)?, None).await?;
    parse_models(&text)
}

pub(crate) async fn prompt(
    host: &HostContext,
    program: &Path,
    home: &Path,
    via: &str,
    body: &Value,
) -> Result<PromptReply, PluginError> {
    let bytes =
        json::to_vec(body).map_err(|_| PluginError::internal("cannot encode the agy prompt"))?;
    let text = run(host, spawn(program, &PROMPT_ARGS, home, via)?, Some(&bytes)).await?;
    parse_prompt(&text)
}

pub(crate) struct PromptReply {
    pub(crate) text: String,
    pub(crate) tool: Option<ToolReply>,
}

pub(crate) struct ToolReply {
    pub(crate) name: String,
    pub(crate) id: String,
    pub(crate) arguments: String,
}

fn parse_models(text: &str) -> Result<Vec<(String, String)>, PluginError> {
    for line in text.lines() {
        let line = line.trim();
        if line.is_empty() {
            continue;
        }
        let Ok(value) = json::from_str::<Value>(line) else {
            continue;
        };
        let Some(models) = value.get("models").and_then(Value::as_array) else {
            continue;
        };
        let mut out = Vec::new();
        for model in models {
            let Some(id) = model.get("id").and_then(Value::as_str) else {
                continue;
            };
            if id.is_empty() {
                continue;
            }
            let name = model
                .get("name")
                .and_then(Value::as_str)
                .filter(|name| !name.is_empty())
                .unwrap_or(id);
            out.push((id.to_owned(), name.to_owned()));
        }
        return Ok(out);
    }
    Err(PluginError::decode("agy models returned no model list"))
}

fn parse_prompt(text: &str) -> Result<PromptReply, PluginError> {
    let mut reply = PromptReply {
        text: String::new(),
        tool: None,
    };
    let mut saw = false;
    for line in text.lines() {
        let line = line.trim();
        if line.is_empty() {
            continue;
        }
        let Ok(value) = json::from_str::<Value>(line) else {
            continue;
        };
        if let Some(text) = value.get("text").and_then(Value::as_str) {
            reply.text = text.to_owned();
            saw = true;
        }
        if let Some(tool) = value.get("tool") {
            let name = tool.get("name").and_then(Value::as_str).unwrap_or("");
            if !name.is_empty() {
                reply.tool = Some(ToolReply {
                    name: name.to_owned(),
                    id: tool
                        .get("id")
                        .and_then(Value::as_str)
                        .filter(|id| !id.is_empty())
                        .unwrap_or("call-1")
                        .to_owned(),
                    arguments: tool
                        .get("arguments")
                        .and_then(Value::as_str)
                        .unwrap_or("{}")
                        .to_owned(),
                });
                saw = true;
            }
        }
    }
    if saw {
        Ok(reply)
    } else {
        Err(PluginError::decode("agy prompt returned no reply"))
    }
}
