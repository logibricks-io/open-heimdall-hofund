//! The account's model list from `/codex/models`, and the instructions each
//! model is sent with, which the list carries and the plugin keeps.

use std::collections::{BTreeMap, HashMap};
use std::path::{Path, PathBuf};
use std::sync::{Mutex, MutexGuard, PoisonError};
use std::time::{Duration, SystemTime};

use hofund_plugin_api::{AccountId, HostContext, ModelList, ModelSpec, PluginError};
use hofund_plugin_sdk::json::{self, Value};
use hofund_plugin_sdk::oauth::new_state;

use crate::tokens::Access;
use crate::upstream::account_call;
use crate::{CODEX_VERSION, Endpoints};

/// The file in the plugin's cache directory that keeps instructions by model.
pub(crate) const PROMPTS_FILE: &str = "codex-prompts.json";

/// How long a model with no known instructions waits before its account's
/// list is asked for again.
pub(crate) const ASK_AGAIN: Duration = Duration::from_secs(10 * 60);

/// The instructions of each model, as the latest model lists carried them:
/// read from [`PROMPTS_FILE`] again whenever it changes, so every process
/// shares them.
#[derive(Debug, Default)]
pub(crate) struct Prompts {
    kept: Mutex<Kept>,
}

#[derive(Debug, Default)]
struct Kept {
    /// The file read and its modification time.
    read: Option<(PathBuf, SystemTime)>,
    by_model: HashMap<String, String>,
    /// When each account's list was last asked for a missing model.
    asked: HashMap<AccountId, Duration>,
}

impl Prompts {
    fn lock(&self) -> MutexGuard<'_, Kept> {
        self.kept.lock().unwrap_or_else(PoisonError::into_inner)
    }

    /// The kept instructions of `model`.
    pub(crate) fn get(&self, host: &HostContext, model: &str) -> Option<String> {
        let path = prompts_path(host)?;
        let modified = std::fs::metadata(&path)
            .and_then(|meta| meta.modified())
            .ok()?;
        let mut kept = self.lock();
        let current = kept
            .read
            .as_ref()
            .is_some_and(|(read, at)| *read == path && *at == modified);
        if !current {
            kept.by_model = std::fs::read(&path)
                .ok()
                .and_then(|bytes| json::from_slice(&bytes).ok())
                .unwrap_or_default();
            kept.read = Some((path, modified));
        }
        kept.by_model.get(model).cloned()
    }

    /// Keeps the instructions a model list carries, beside those of models
    /// it does not list (another account's).
    pub(crate) fn save(&self, host: &HostContext, fresh: HashMap<String, String>) {
        if fresh.is_empty() {
            return;
        }
        let Some(path) = prompts_path(host) else {
            return;
        };
        let mut kept = self.lock();
        let mut all: BTreeMap<String, String> = std::fs::read(&path)
            .ok()
            .and_then(|bytes| json::from_slice(&bytes).ok())
            .unwrap_or_default();
        all.extend(fresh);
        if let Ok(bytes) = json::to_vec(&all) {
            let _ = write_atomic(&path, &bytes);
        }
        // Two writes in one tick share a modification time: read it again.
        kept.read = None;
    }

    /// Says `account`'s list may be asked for a missing model now, and
    /// notes that it was.
    pub(crate) fn may_ask(&self, account: &AccountId, now: Duration) -> bool {
        let mut kept = self.lock();
        if let Some(at) = kept.asked.get(account)
            && now.saturating_sub(*at) < ASK_AGAIN
        {
            return false;
        }
        kept.asked.insert(account.clone(), now);
        true
    }
}

fn prompts_path(host: &HostContext) -> Option<PathBuf> {
    host.dirs()
        .cache_dir()
        .ok()
        .map(|dir| dir.join(PROMPTS_FILE))
}

/// Writes `bytes` to `path` through a file beside it, so a reader never
/// sees half of it.
fn write_atomic(path: &Path, bytes: &[u8]) -> std::io::Result<()> {
    let tag = new_state().map_err(|_| std::io::Error::other("no random tag"))?;
    let partial = path.with_extension(format!("json.{tag}.partial"));
    std::fs::write(&partial, bytes)?;
    std::fs::rename(&partial, path).inspect_err(|_| {
        let _ = std::fs::remove_file(&partial);
    })
}

/// The instructions each model in a `/codex/models` reply is sent with: its
/// instructions template, else its base instructions.
pub(crate) fn prompts(body: &[u8]) -> HashMap<String, String> {
    let Ok(value) = json::from_slice::<Value>(body) else {
        return HashMap::new();
    };
    let mut out = HashMap::new();
    for row in value
        .get("models")
        .and_then(Value::as_array)
        .map_or(&[][..], Vec::as_slice)
    {
        let Some(slug) = row.get("slug").and_then(Value::as_str) else {
            continue;
        };
        let template = row
            .pointer("/model_messages/instructions_template")
            .and_then(Value::as_str)
            .filter(|text| !text.is_empty());
        let base = row
            .get("base_instructions")
            .and_then(Value::as_str)
            .filter(|text| !text.is_empty());
        if let Some(text) = template.or(base) {
            out.insert(slug.to_owned(), text.to_owned());
        }
    }
    out
}

/// The `/codex/models` URL for the Codex version the plugin names.
pub(crate) fn list_url(endpoints: &Endpoints) -> String {
    format!("{}/models?client_version={CODEX_VERSION}", endpoints.base)
}

/// Reads the account's model list, keeping the instructions it carries.
pub(crate) async fn list(
    host: &HostContext,
    endpoints: &Endpoints,
    access: &Access,
    prompts: &Prompts,
) -> Result<ModelList, PluginError> {
    let url = list_url(endpoints);
    let call = account_call(url.clone(), access);
    let reply = call.fetch(host.http()).await?;
    match reply.status() {
        200 => {}
        401 | 403 => {
            return Err(PluginError::auth(
                "ChatGPT refused to list the account's models",
            ));
        }
        status => {
            return Err(PluginError::status_error(
                status,
                "ChatGPT did not list the account's models",
            ));
        }
    }
    let body = reply.body();
    prompts.save(host, self::prompts(body));
    let (models, dropped) = parse(body)?;
    if models.is_empty() {
        return Err(PluginError::decode("ChatGPT listed no Codex models"));
    }
    Ok(ModelList::new(models, None)
        .with_dropped(dropped)
        .with_source_url(url))
}

fn strings(value: Option<&Value>, field: Option<&str>) -> Vec<String> {
    value
        .and_then(Value::as_array)
        .map_or(&[][..], Vec::as_slice)
        .iter()
        .filter_map(|entry| match field {
            Some(field) => entry.get(field).and_then(Value::as_str),
            None => entry.as_str(),
        })
        .map(str::to_owned)
        .collect()
}

/// Reads a `/codex/models` reply: the models in the backend's order, and
/// the ids it hides.
pub(crate) fn parse(body: &[u8]) -> Result<(Vec<ModelSpec>, Vec<String>), PluginError> {
    let value: Value = json::from_slice(body)
        .map_err(|_| PluginError::decode("the ChatGPT model list is not JSON"))?;
    let mut rows: Vec<&Value> = value
        .get("models")
        .and_then(Value::as_array)
        .ok_or_else(|| PluginError::decode("the ChatGPT model list has no models"))?
        .iter()
        .collect();
    rows.sort_by_key(|row| row.get("priority").and_then(Value::as_i64).unwrap_or(0));
    let mut models = Vec::new();
    let mut dropped = Vec::new();
    for row in rows {
        let Some(slug) = row
            .get("slug")
            .and_then(Value::as_str)
            .filter(|slug| !slug.is_empty())
        else {
            continue;
        };
        if row.get("visibility").and_then(Value::as_str) == Some("hide") {
            dropped.push(slug.to_owned());
            continue;
        }
        let name = row
            .get("display_name")
            .and_then(Value::as_str)
            .filter(|name| !name.is_empty())
            .map(str::to_owned);
        let mut spec = ModelSpec::new(slug, name)?
            .with_context(
                row.get("context_window")
                    .and_then(Value::as_u64)
                    .unwrap_or(0),
            )
            .with_efforts(strings(
                row.get("supported_reasoning_levels"),
                Some("effort"),
            ))
            .with_service_tiers(strings(row.get("service_tiers"), Some("id")));
        if let Some(Value::Array(_)) = row.get("input_modalities") {
            let modalities = strings(row.get("input_modalities"), None);
            let image = modalities.iter().any(|modality| modality == "image");
            spec = spec
                .with_input_modalities(modalities)
                .with_image_input(Some(image));
        }
        models.push(spec);
    }
    Ok((models, dropped))
}
