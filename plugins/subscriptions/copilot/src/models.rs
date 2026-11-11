//! The Copilot model list: which models an account may pick, the APIs each
//! is served on, and the models whose terms wait to be accepted.

use std::collections::{HashMap, HashSet};
use std::sync::Mutex;

use hofund_plugin_api::{AccountId, HostContext, ModelList, ModelSpec, PluginError, ProtocolId};
use hofund_plugin_sdk::Call;
use hofund_plugin_sdk::json::{self, Value};
use hofund_plugin_sdk::oauth::secret_text;

use crate::session::Session;

/// Id prefixes Copilot gives models nobody picks by hand.
const INTERNAL_PREFIXES: [&str; 3] = ["copilot-search", "exec-agent", "trajectory"];

/// Id suffixes Copilot gives models nobody picks by hand.
const INTERNAL_SUFFIXES: [&str; 4] = ["-secondary", "-tertiary", "-4th", "-free-auto"];

/// What one model list said about an account.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub(crate) struct Listed {
    /// The APIs of every chat model the list named, offered or not.
    apis: HashMap<String, Vec<ProtocolId>>,
    /// Models listed with terms the account has not accepted yet.
    waiting: HashSet<String>,
    /// Enabled manual picks, likeliest served first.
    picks: Vec<String>,
}

impl Listed {
    /// The protocol to send `model` on for a call that arrived on
    /// `inbound`: the inbound one when the model is served on it, else the
    /// first API the list names, else Chat Completions.
    pub(crate) fn protocol(&self, model: &str, inbound: ProtocolId) -> ProtocolId {
        let apis = self.apis(model);
        if apis.contains(&inbound) {
            return inbound;
        }
        apis.first().copied().unwrap_or(ProtocolId::OPENAI_CHAT)
    }

    pub(crate) fn apis(&self, model: &str) -> Vec<ProtocolId> {
        self.apis
            .get(model)
            .cloned()
            .unwrap_or_else(|| vec![ProtocolId::OPENAI_CHAT])
    }

    pub(crate) fn knows(&self, model: &str) -> bool {
        self.apis.contains_key(model)
    }

    pub(crate) fn picks(&self) -> Vec<String> {
        self.picks.clone()
    }

    /// Whether `model` waits for its terms to be accepted.
    pub(crate) fn waits(&self, model: &str) -> bool {
        self.waiting.contains(model)
    }
}

/// What the plugin learned from each account's latest model list.
#[derive(Debug, Default)]
pub(crate) struct Book {
    accounts: Mutex<HashMap<AccountId, Listed>>,
    pub(crate) autos: Mutex<HashMap<AccountId, crate::auto::AutoSession>>,
    pub(crate) refused: Mutex<HashMap<AccountId, HashMap<String, std::time::SystemTime>>>,
}

impl Book {
    fn lock(&self) -> std::sync::MutexGuard<'_, HashMap<AccountId, Listed>> {
        self.accounts
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
    }

    /// What the latest list said about `account`, if it was listed.
    pub(crate) fn get(&self, account: &AccountId) -> Option<Listed> {
        self.lock().get(account).cloned()
    }

    /// Keeps `listed` as what `account`'s list says.
    pub(crate) fn put(&self, account: &AccountId, listed: Listed) {
        self.lock().insert(account.clone(), listed);
    }

    /// Remembers endpoints an Auto pick named even when `/models` did not.
    pub(crate) fn see(&self, account: &AccountId, model: &str, apis: Vec<ProtocolId>) {
        self.lock()
            .entry(account.clone())
            .or_default()
            .apis
            .insert(model.to_owned(), apis);
    }

    /// Records that `account` accepted the terms of `model`.
    pub(crate) fn accepted(&self, account: &AccountId, model: &str) {
        if let Some(listed) = self.lock().get_mut(account) {
            listed.waiting.remove(model);
        }
    }
}

/// A call to the Copilot API with the session token and client headers.
fn api_call(call: Call, session: &Session) -> Call {
    let call = session.headers().iter().fold(
        call.sensitive_header(
            "authorization",
            format!("Bearer {}", secret_text(&session.token)),
        )
        .header("accept", "application/json"),
        |call, (name, value)| call.header(*name, *value),
    );
    if session.direct {
        call
    } else {
        call.header("X-GitHub-Api-Version", "2026-01-09")
    }
}

/// Reads `/models` for `account` and keeps what it says in `book`.
pub(crate) async fn list(
    host: &HostContext,
    session: &Session,
    account: &AccountId,
    book: &Book,
) -> Result<ModelList, PluginError> {
    let url = format!("{}/models", session.base);
    let reply = api_call(Call::get(url.clone()), session)
        .fetch(host.http())
        .await?;
    match reply.status() {
        200 => {}
        401 | 403 => {
            return Err(PluginError::auth(
                "Copilot refused to list the account's models",
            ));
        }
        status => {
            return Err(PluginError::status_error(
                status,
                "Copilot did not list the account's models",
            ));
        }
    }
    let (mut models, mut dropped, listed) = parse(reply.body())?;
    book.put(account, listed);
    let now = host.clock().now();
    models.retain(|model| {
        let served = !crate::auto::refused(book, account, model.id(), now);
        if !served {
            dropped.push(model.id().to_owned());
        }
        served
    });
    if !models
        .iter()
        .any(|model| model.id() == crate::auto::AUTO_ID)
    {
        models.push(ModelSpec::new(
            crate::auto::AUTO_ID,
            Some("Auto".to_owned()),
        )?);
    }
    Ok(ModelList::new(models, None)
        .with_dropped(dropped)
        .with_source_url(url))
}

/// Accepts the terms of `model` for `account` when they wait. A refusal
/// is left to the request, whose answer then says why; only a 2xx reply
/// counts as accepted.
pub(crate) async fn accept(
    host: &HostContext,
    session: &Session,
    account: &AccountId,
    book: &Book,
    model: &str,
) {
    let url = format!("{}/models/{}/policy", session.base, path_segment(model));
    let call = api_call(Call::post(url), session).json(br#"{"state":"enabled"}"#);
    if let Ok(reply) = call.fetch(host.http()).await
        && (200..300).contains(&reply.status())
    {
        book.accepted(account, model);
    }
}

/// Reads a `/models` reply: the models the account may pick, the ids left
/// out, and what the list says about each model's APIs and terms.
pub(crate) fn parse(body: &[u8]) -> Result<(Vec<ModelSpec>, Vec<String>, Listed), PluginError> {
    let value: Value = json::from_slice(body)
        .map_err(|_| PluginError::decode("the Copilot model list is not JSON"))?;
    let rows = value
        .get("data")
        .and_then(Value::as_array)
        .ok_or_else(|| PluginError::decode("the Copilot model list has no data"))?;
    let mut models = Vec::new();
    let mut dropped = Vec::new();
    let mut listed = Listed::default();
    let mut ranked = Vec::new();
    for row in rows {
        let Some(id) = row
            .get("id")
            .and_then(Value::as_str)
            .filter(|id| !id.is_empty())
        else {
            continue;
        };
        if row.pointer("/capabilities/type").and_then(Value::as_str) != Some("chat") {
            dropped.push(id.to_owned());
            continue;
        }
        let apis = apis(&strings(row.get("supported_endpoints")));
        listed.apis.insert(id.to_owned(), apis.clone());
        if !offered(row, id) {
            dropped.push(id.to_owned());
            continue;
        }
        let picker = row
            .get("model_picker_enabled")
            .and_then(Value::as_bool)
            .unwrap_or(false);
        let policy = row.get("policy").filter(|policy| !policy.is_null());
        let state = policy
            .and_then(|policy| policy.get("state"))
            .and_then(Value::as_str);
        let terms = policy
            .and_then(|policy| policy.get("terms"))
            .and_then(Value::as_str)
            .unwrap_or("");
        let enabled = state == Some("enabled") || (policy.is_none() && picker);
        if !enabled {
            if policy.is_some() && !terms.is_empty() {
                listed.waiting.insert(id.to_owned());
            } else {
                // Not the account's to enable: Copilot answers it 403.
                dropped.push(id.to_owned());
                continue;
            }
        } else {
            let premium = row
                .pointer("/billing/is_premium")
                .and_then(Value::as_bool)
                .unwrap_or(true);
            let rank = if row.get("is_chat_fallback").and_then(Value::as_bool) == Some(true) {
                0
            } else if row.get("is_chat_default").and_then(Value::as_bool) == Some(true) {
                1
            } else if id == "gpt-4.1" {
                2
            } else if !premium {
                3
            } else {
                4
            };
            ranked.push((rank, id.to_owned()));
        }
        let name = row
            .get("name")
            .and_then(Value::as_str)
            .filter(|name| !name.is_empty())
            .map(str::to_owned);
        let spec = ModelSpec::new(id, name)?
            .with_endpoints(
                apis.iter()
                    .map(|api| endpoint_name(*api).to_owned())
                    .collect(),
            )
            .with_efforts(strings(
                row.pointer("/capabilities/supports/reasoning_effort"),
            ));
        models.push(spec);
    }
    ranked.sort_by_key(|(rank, _)| *rank);
    listed.picks = ranked.into_iter().map(|(_, id)| id).collect();
    Ok((models, dropped, listed))
}

/// Whether a chat model is one somebody picks: not internal, not
/// experimental, and offered by some picker.
fn offered(row: &Value, id: &str) -> bool {
    if internal(id) || row.get("vendor").and_then(Value::as_str) == Some("Experimental") {
        return false;
    }
    let picker = row
        .get("model_picker_enabled")
        .and_then(Value::as_bool)
        .unwrap_or(false);
    let category = row
        .get("model_picker_category")
        .and_then(Value::as_str)
        .unwrap_or("");
    // A model no picker offers is an old snapshot or Copilot's own.
    picker || !category.is_empty()
}

/// Whether `id` names a model Copilot keeps for itself.
pub(crate) fn internal(id: &str) -> bool {
    INTERNAL_PREFIXES
        .iter()
        .any(|prefix| id.starts_with(prefix))
        || INTERNAL_SUFFIXES.iter().any(|suffix| id.ends_with(suffix))
}

/// The APIs named by `supported_endpoints`. A model the list gives none is
/// served on Chat Completions alone.
pub(crate) fn apis(endpoints: &[String]) -> Vec<ProtocolId> {
    if endpoints.is_empty() {
        return vec![ProtocolId::OPENAI_CHAT];
    }
    let mut out = Vec::new();
    for endpoint in endpoints {
        let trimmed = endpoint.trim_end_matches('/');
        let path = trimmed.strip_prefix("/v1").unwrap_or(trimmed);
        let api = match path {
            "/chat/completions" => ProtocolId::OPENAI_CHAT,
            "/responses" => ProtocolId::OPENAI_RESPONSES,
            "/messages" => ProtocolId::ANTHROPIC_MESSAGES,
            _ => continue,
        };
        if !out.contains(&api) {
            out.push(api);
        }
    }
    out
}

/// The endpoint name the host reads for `api`.
fn endpoint_name(api: ProtocolId) -> &'static str {
    match api {
        ProtocolId::OPENAI_RESPONSES => "responses",
        ProtocolId::ANTHROPIC_MESSAGES => "anthropic",
        _ => "chat",
    }
}

pub(crate) fn strings(value: Option<&Value>) -> Vec<String> {
    value
        .and_then(Value::as_array)
        .map(|items| {
            items
                .iter()
                .filter_map(Value::as_str)
                .map(str::to_owned)
                .collect()
        })
        .unwrap_or_default()
}

/// `model` escaped as one URL path segment.
fn path_segment(model: &str) -> String {
    let mut out = String::with_capacity(model.len());
    for byte in model.bytes() {
        if byte.is_ascii_alphanumeric() || matches!(byte, b'-' | b'.' | b'_' | b'~') {
            out.push(char::from(byte));
        } else {
            out.push_str(&format!("%{byte:02X}"));
        }
    }
    out
}
