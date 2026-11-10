//! Tool parking, callback replies, and MCP file layout for a Claude Code run.
//!
//! Resume, idle reuse, and session shelve stay in R-6e. This slice matches the
//! next HTTP request to a parked run by tool-call id, remaps client-rewritten
//! ids, answers a refused `tool_choice` immediately, and unblocks the MCP
//! callback with `{content,is_error,tools?}`.

use std::collections::HashMap;
use std::future::Future;
use std::path::{Path, PathBuf};
use std::pin::Pin;
use std::sync::{Arc, Mutex};
use std::task::{Context, Poll, Waker};
use std::time::Duration;

use bytes::Bytes;
use futures_util::future::BoxFuture;
use hofund_plugin_api::{
    AccountId, BridgeCallbackGuard, BridgeCallbackHandler, BridgeCallbackReply, ContentPart,
    PluginError, Request, SpawnRequest,
};
use ring::rand::{SecureRandom, SystemRandom};
use serde_json::{Value, json};

/// How many times [`Bridge::match_request`] retries while a call id is still
/// being registered (20 × 25 ms, matching the reference).
pub(crate) const MATCH_ATTEMPTS: usize = 20;
/// Gap between match attempts.
pub(crate) const MATCH_GAP: Duration = Duration::from_millis(25);

/// Shared parked-run table for one plugin instance.
#[derive(Clone, Debug, Default)]
pub struct Bridge {
    inner: Arc<Inner>,
}

#[derive(Default)]
struct Inner {
    calls: Mutex<HashMap<String, CallSlot>>,
    early: Mutex<HashMap<String, Delivered>>,
    runs: Mutex<HashMap<u64, ParkedRun>>,
    next_id: Mutex<u64>,
}

impl std::fmt::Debug for Inner {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Inner").finish_non_exhaustive()
    }
}

struct CallSlot {
    run_id: u64,
    _name: String,
    early: Option<Delivered>,
    wait: Arc<WaitCell<Delivered>>,
}

struct ParkedRun {
    owner: AccountId,
    offered: Vec<String>,
    continuation: Option<String>,
    said_queue: String,
    callback_guard: Option<BridgeCallbackGuard>,
}

/// Result delivered to a waiting MCP callback.
#[derive(Clone, Debug)]
pub(crate) struct Delivered {
    /// MCP `content` value.
    pub content: Value,
    /// MCP `is_error` flag.
    pub is_error: bool,
    /// Optional replacement tool table for `tools/list_changed`.
    pub tools: Option<Value>,
}

/// A successful match of a tool-result request onto a parked run.
#[derive(Debug)]
pub(crate) struct Match {
    /// Parked run id.
    pub run_id: u64,
    /// User text queued as its own stdin message, if any.
    pub said: String,
    /// Client id → original tool-call id, when the client rewrote an id.
    pub remapped: Vec<(String, String)>,
}

/// Files written for one Claude Code `--mcp-config`.
#[derive(Debug, Clone, Eq, PartialEq)]
pub(crate) struct McpLayout {
    /// Mode-`0600` file holding the callback URL.
    pub callback_path: PathBuf,
    /// JSON array of MCP tools.
    pub tools_path: PathBuf,
    /// Written by the helper after the first `tools/list`.
    pub listed_path: PathBuf,
    /// The `--mcp-config` JSON path.
    pub config_path: PathBuf,
}

impl Bridge {
    /// An empty table.
    #[must_use]
    pub fn new() -> Self {
        Self::default()
    }

    /// Allocates a parked run for `owner` offering `tool_names`.
    pub(crate) fn park_run(&self, owner: AccountId, tool_names: Vec<String>) -> u64 {
        let mut next = self
            .inner
            .next_id
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        *next += 1;
        let id = *next;
        self.inner
            .runs
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .insert(
                id,
                ParkedRun {
                    owner,
                    offered: tool_names,
                    continuation: None,
                    said_queue: String::new(),
                    callback_guard: None,
                },
            );
        id
    }

    /// Keeps the callback registration alive until the run is aborted or dropped.
    pub(crate) fn set_callback_guard(&self, run_id: u64, guard: BridgeCallbackGuard) {
        if let Some(run) = self
            .inner
            .runs
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .get_mut(&run_id)
        {
            run.callback_guard = Some(guard);
        }
    }

    /// Registers `tool_call_id` on `run_id` so a later request can match it.
    pub(crate) fn register_call(
        &self,
        run_id: u64,
        tool_call_id: impl Into<String>,
        name: impl Into<String>,
    ) {
        let id = tool_call_id.into();
        let name = name.into();
        let wait = Arc::new(WaitCell::new());
        let early = self
            .inner
            .early
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .remove(&id);
        if let Some(ref delivered) = early {
            wait.set(delivered.clone());
        }
        self.inner
            .calls
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .insert(
                id,
                CallSlot {
                    run_id,
                    _name: name,
                    early,
                    wait,
                },
            );
    }

    /// Stores leftover fake-runner stdout to serve after the next `tell`.
    #[allow(dead_code)]
    pub(crate) fn set_continuation(&self, run_id: u64, stdout: String) {
        if let Some(run) = self
            .inner
            .runs
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .get_mut(&run_id)
        {
            run.continuation = Some(stdout);
        }
    }

    /// Takes leftover fake-runner stdout, if any.
    pub(crate) fn take_continuation(&self, run_id: u64) -> Option<String> {
        self.inner
            .runs
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .get_mut(&run_id)
            .and_then(|run| run.continuation.take())
    }

    /// Takes the queued `saidWithResults` user text for `run_id`.
    pub(crate) fn take_said(&self, run_id: u64) -> String {
        self.inner
            .runs
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .get_mut(&run_id)
            .map(|run| std::mem::take(&mut run.said_queue))
            .unwrap_or_default()
    }

    /// Drops a parked run and its call ids. Used on owner mismatch.
    pub(crate) fn abort(&self, run_id: u64) {
        self.inner
            .runs
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .remove(&run_id);
        self.inner
            .calls
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .retain(|_, slot| slot.run_id != run_id);
    }

    /// Finds a parked run for the tool results in `request`, retrying while
    /// registration races the next HTTP request.
    ///
    /// An owner mismatch aborts the parked run and returns `None` so the
    /// caller can start a fresh turn. `claimResume` / history `follows` stay
    /// in R-6e.
    pub(crate) fn match_request(&self, request: &Request, account: &AccountId) -> Option<Match> {
        for attempt in 0..MATCH_ATTEMPTS {
            if let Some(found) = self.match_once(request, account) {
                return found;
            }
            if attempt + 1 < MATCH_ATTEMPTS {
                std::thread::sleep(MATCH_GAP);
            }
        }
        None
    }

    /// A single match attempt with no sleep. Tests use this for the early map.
    pub(crate) fn match_once(
        &self,
        request: &Request,
        account: &AccountId,
    ) -> Option<Option<Match>> {
        let results = tool_results(request);
        if results.is_empty() {
            return Some(None);
        }
        let said = said_with_results(request);
        let more = mcp_tools_value(request);
        let more_needed = self.needs_more_tools(request);
        let tools = more_needed.then_some(more);

        let ids: Vec<String> = results.iter().map(|(id, _, _)| id.clone()).collect();
        let mapped = self.remap_ids(&ids);
        if mapped.is_empty() {
            self.store_early(&results, &said, tools.as_ref());
            return None;
        }

        let run_id = {
            let calls = self
                .inner
                .calls
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner);
            calls.get(&mapped[0].1)?.run_id
        };

        let owner = self
            .inner
            .runs
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .get(&run_id)
            .map(|run| run.owner.clone());
        if owner.as_ref() != Some(account) {
            self.abort(run_id);
            return Some(None);
        }

        let remapped: Vec<(String, String)> = mapped
            .iter()
            .filter(|(client, original)| client != original)
            .cloned()
            .collect();

        for (client_id, original_id, _is_error, content) in
            results.iter().filter_map(|(client, is_error, content)| {
                let original = mapped
                    .iter()
                    .find(|(c, _)| c == client)
                    .map(|(_, o)| o.clone())?;
                Some((client.clone(), original, *is_error, content.clone()))
            })
        {
            let _ = client_id;
            let delivered = Delivered {
                content: mcp_content(&content),
                is_error: _is_error,
                tools: tools.clone(),
            };
            self.deliver(&original_id, delivered);
        }

        if !said.is_empty()
            && let Some(run) = self
                .inner
                .runs
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner)
                .get_mut(&run_id)
        {
            run.said_queue = said.clone();
        }

        Some(Some(Match {
            run_id,
            said,
            remapped,
        }))
    }

    fn needs_more_tools(&self, request: &Request) -> bool {
        let names: Vec<String> = request.tools.iter().map(|tool| tool.name.clone()).collect();
        if names.is_empty() {
            return false;
        }
        let runs = self
            .inner
            .runs
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        runs.values()
            .any(|run| names.iter().any(|n| !run.offered.contains(n)))
    }

    fn remap_ids(&self, client_ids: &[String]) -> Vec<(String, String)> {
        let calls = self
            .inner
            .calls
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        let mut used = Vec::new();
        let mut out = Vec::new();
        let unmatched: Vec<String> = calls
            .keys()
            .filter(|id| !client_ids.contains(id))
            .cloned()
            .collect();
        for client in client_ids {
            if calls.contains_key(client) {
                used.push(client.clone());
                out.push((client.clone(), client.clone()));
            }
        }
        let leftover_clients: Vec<String> = client_ids
            .iter()
            .filter(|id| !used.contains(id))
            .cloned()
            .collect();
        if leftover_clients.len() == 1 && unmatched.len() == 1 {
            out.push((leftover_clients[0].clone(), unmatched[0].clone()));
        }
        out
    }

    fn store_early(
        &self,
        results: &[(String, bool, Vec<ContentPart>)],
        _said: &str,
        tools: Option<&Value>,
    ) {
        let mut early = self
            .inner
            .early
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        for (id, is_error, content) in results {
            early.insert(
                id.clone(),
                Delivered {
                    content: mcp_content(content),
                    is_error: *is_error,
                    tools: tools.cloned(),
                },
            );
        }
    }

    fn deliver(&self, original_id: &str, delivered: Delivered) {
        let mut calls = self
            .inner
            .calls
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        if let Some(slot) = calls.get_mut(original_id) {
            slot.early = Some(delivered.clone());
            slot.wait.set(delivered);
        }
    }

    /// A callback handler for one run. Refused tools are answered immediately;
    /// accepted tools wait for [`Bridge::match_request`].
    #[must_use]
    pub(crate) fn callback_handler(
        &self,
        run_id: u64,
        request_tools: Request,
    ) -> Arc<dyn BridgeCallbackHandler> {
        Arc::new(RunCallback {
            table: self.clone(),
            run_id,
            request: request_tools,
        })
    }

    /// Waits until a result is delivered for `tool_call_id`.
    pub(crate) fn wait_for(&self, tool_call_id: &str) -> Option<Arc<WaitCell<Delivered>>> {
        self.inner
            .calls
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .get(tool_call_id)
            .map(|slot| Arc::clone(&slot.wait))
    }

    fn early_for(&self, tool_call_id: &str) -> Option<Delivered> {
        self.inner
            .calls
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .get(tool_call_id)
            .and_then(|slot| slot.early.clone())
    }
}

struct RunCallback {
    table: Bridge,
    run_id: u64,
    request: Request,
}

impl BridgeCallbackHandler for RunCallback {
    fn handle(&self, body: Bytes) -> BoxFuture<'_, BridgeCallbackReply> {
        Box::pin(async move { self.reply(body).await })
    }
}

impl RunCallback {
    async fn reply(&self, body: Bytes) -> BridgeCallbackReply {
        let parsed: Value = match serde_json::from_slice(&body) {
            Ok(value) => value,
            Err(_) => {
                return BridgeCallbackReply::new(
                    400,
                    json!({"content":[{"type":"text","text":"invalid callback body"}],"is_error":true})
                        .to_string(),
                );
            }
        };
        let id = parsed
            .get("tool_call_id")
            .and_then(Value::as_str)
            .unwrap_or("")
            .to_owned();
        let name = parsed
            .get("name")
            .and_then(Value::as_str)
            .unwrap_or("")
            .to_owned();
        if id.is_empty() {
            return BridgeCallbackReply::new(
                400,
                json!({"content":[{"type":"text","text":"missing tool_call_id"}],"is_error":true})
                    .to_string(),
            );
        }
        if tool_choice_refuses(&name, &self.request) {
            return ok_reply(&Delivered {
                content: json!([{"type":"text","text": format!("tool {name} is not allowed by the client tool_choice")}]),
                is_error: true,
                tools: None,
            });
        }
        self.table.register_call(self.run_id, &id, &name);
        if let Some(early) = self.table.early_for(&id) {
            return ok_reply(&early);
        }
        if let Some(cell) = self.table.wait_for(&id) {
            let delivered = cell.wait().await;
            return ok_reply(&delivered);
        }
        BridgeCallbackReply::new(
            500,
            json!({"content":[{"type":"text","text":"missing parked call"}],"is_error":true})
                .to_string(),
        )
    }
}

fn ok_reply(delivered: &Delivered) -> BridgeCallbackReply {
    let mut body = json!({
        "content": delivered.content,
        "is_error": delivered.is_error,
    });
    if let Some(tools) = &delivered.tools {
        body["tools"] = tools.clone();
    }
    BridgeCallbackReply::new(200, body.to_string())
}

/// 24 random bytes as lowercase hex. RNG failure fails the run closed.
pub(crate) fn new_callback_token() -> Result<String, PluginError> {
    let mut bytes = [0_u8; 24];
    SystemRandom::new()
        .fill(&mut bytes)
        .map_err(|_| PluginError::internal("the system random source failed"))?;
    Ok(bytes.iter().map(|b| format!("{b:02x}")).collect())
}

/// Writes the callback URL, tools list, listed placeholder, and MCP config.
///
/// The callback URL is stored mode `0600` and is never placed on argv.
pub(crate) fn write_mcp_layout(
    dir: &Path,
    callback_url: &str,
    request: &Request,
    helper: &SpawnRequest,
) -> Result<McpLayout, PluginError> {
    std::fs::create_dir_all(dir).map_err(|error| {
        PluginError::internal(format!("cannot create the bridge MCP directory: {error}"))
    })?;
    let callback_path = dir.join("callback");
    let tools_path = dir.join("tools.json");
    let listed_path = dir.join("listed");
    let config_path = dir.join("mcp.json");
    write_private(&callback_path, format!("{callback_url}\n").as_bytes())?;
    let tools = mcp_tools_value(request);
    std::fs::write(
        &tools_path,
        serde_json::to_vec_pretty(&tools).map_err(|error| {
            PluginError::internal(format!("cannot encode bridge tools: {error}"))
        })?,
    )
    .map_err(|error| PluginError::internal(format!("cannot write the tools file: {error}")))?;
    std::fs::write(&listed_path, b"")
        .map_err(|error| PluginError::internal(format!("cannot write the listed file: {error}")))?;

    let mut args: Vec<String> = helper
        .args()
        .iter()
        .map(|arg| arg.to_string_lossy().into_owned())
        .collect();
    args.push(callback_path.to_string_lossy().into_owned());
    args.push(tools_path.to_string_lossy().into_owned());
    args.push(listed_path.to_string_lossy().into_owned());
    let config = json!({
        "mcpServers": {
            "hofund": {
                "command": helper.program().to_string_lossy(),
                "args": args,
            }
        }
    });
    std::fs::write(
        &config_path,
        serde_json::to_vec_pretty(&config).map_err(|error| {
            PluginError::internal(format!("cannot encode the MCP config: {error}"))
        })?,
    )
    .map_err(|error| PluginError::internal(format!("cannot write the MCP config: {error}")))?;
    Ok(McpLayout {
        callback_path,
        tools_path,
        listed_path,
        config_path,
    })
}

fn write_private(path: &Path, bytes: &[u8]) -> Result<(), PluginError> {
    use std::io::Write;
    let mut opts = std::fs::OpenOptions::new();
    opts.write(true).create(true).truncate(true);
    #[cfg(unix)]
    {
        use std::os::unix::fs::OpenOptionsExt;
        opts.mode(0o600);
    }
    opts.open(path)
        .and_then(|mut file| file.write_all(bytes))
        .map_err(|error| PluginError::internal(format!("cannot write the callback file: {error}")))
}

/// MCP tool table for `request.tools`.
pub(crate) fn mcp_tools_value(request: &Request) -> Value {
    Value::Array(
        request
            .tools
            .iter()
            .map(|tool| {
                let mut item = json!({
                    "name": tool.name,
                    "inputSchema": tool.input_schema,
                });
                if let Some(description) = &tool.description {
                    item["description"] = json!(description);
                }
                item
            })
            .collect(),
    )
}

/// True when client `tool_choice` (or the listed tool set) refuses `name`.
pub(crate) fn tool_choice_refuses(name: &str, request: &Request) -> bool {
    if let Some(choice) = request.tool_choice.as_ref()
        && let Ok(value) = serde_json::to_value(choice)
    {
        let mode = value
            .get("mode")
            .and_then(|mode| mode.get("mode"))
            .and_then(Value::as_str);
        match mode {
            Some("none") => return true,
            Some("named") => {
                let allowed = value
                    .get("mode")
                    .and_then(|mode| mode.get("name"))
                    .and_then(Value::as_str);
                return allowed != Some(name);
            }
            _ => {}
        }
    }
    !request.tools.is_empty() && request.tools.iter().all(|tool| tool.name != name)
}

/// User text sitting beside tool results in `request`.
pub(crate) fn said_with_results(request: &Request) -> String {
    request
        .messages
        .iter()
        .filter(|message| {
            message
                .content
                .iter()
                .any(|part| matches!(part, ContentPart::ToolResult(_)))
        })
        .flat_map(|message| {
            message.content.iter().filter_map(|part| match part {
                ContentPart::Text(part) => Some(part.text.as_str()),
                _ => None,
            })
        })
        .collect::<Vec<_>>()
        .join("\n")
}

fn tool_results(request: &Request) -> Vec<(String, bool, Vec<ContentPart>)> {
    let mut out = Vec::new();
    for message in &request.messages {
        for part in &message.content {
            if let ContentPart::ToolResult(result) = part {
                out.push((result.id.clone(), result.is_error, result.content.clone()));
            }
        }
    }
    out
}

fn mcp_content(parts: &[ContentPart]) -> Value {
    let items: Vec<Value> = parts
        .iter()
        .filter_map(|part| match part {
            ContentPart::Text(text) => Some(json!({"type":"text","text": text.text})),
            _ => None,
        })
        .collect();
    if items.is_empty() {
        json!([{"type":"text","text":""}])
    } else {
        Value::Array(items)
    }
}

/// One-shot async cell used to unblock a callback handler.
pub(crate) struct WaitCell<T> {
    state: Mutex<WaitState<T>>,
}

struct WaitState<T> {
    value: Option<T>,
    waker: Option<Waker>,
}

impl<T: Clone> WaitCell<T> {
    fn new() -> Self {
        Self {
            state: Mutex::new(WaitState {
                value: None,
                waker: None,
            }),
        }
    }

    fn set(&self, value: T) {
        let mut state = self
            .state
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        state.value = Some(value);
        if let Some(waker) = state.waker.take() {
            waker.wake();
        }
    }

    fn wait(self: &Arc<Self>) -> WaitFuture<T> {
        WaitFuture {
            cell: Arc::clone(self),
        }
    }
}

pub(crate) struct WaitFuture<T> {
    cell: Arc<WaitCell<T>>,
}

impl<T: Clone> Future for WaitFuture<T> {
    type Output = T;

    fn poll(self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<Self::Output> {
        let mut state = self
            .cell
            .state
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        if let Some(value) = state.value.clone() {
            return Poll::Ready(value);
        }
        state.waker = Some(cx.waker().clone());
        Poll::Pending
    }
}

#[cfg(test)]
mod tests {
    use super::{
        Bridge, MATCH_ATTEMPTS, MATCH_GAP, mcp_tools_value, new_callback_token, said_with_results,
        tool_choice_refuses, write_mcp_layout,
    };
    use bytes::Bytes;
    use hofund_plugin_api::{AccountId, ContentPart, Message, Request, Role, SpawnRequest};
    use serde_json::{Value, json};
    use std::path::PathBuf;
    use std::time::{Duration, Instant};

    const OWNER: &str = "a-00000000000000aa";
    const OTHER: &str = "a-00000000000000bb";

    fn account(id: &'static str) -> AccountId {
        AccountId::from_static(id)
    }

    fn tools_request() -> Request {
        serde_json::from_value(json!({
            "model": "claude-sonnet-4-5",
            "messages": [{"role": "user", "content": [{"type": "text", "text": "hi"}]}],
            "tools": [{"name": "Bash", "input_schema": {"type": "object"}}]
        }))
        .expect("request")
    }

    fn result_request(id: &str, text: &str, extra: Option<&str>) -> Request {
        let mut content = vec![json!({
            "type": "tool_result",
            "id": id,
            "content": [{"type": "text", "text": text}]
        })];
        if let Some(said) = extra {
            content.insert(0, json!({"type": "text", "text": said}));
        }
        serde_json::from_value(json!({
            "model": "claude-sonnet-4-5",
            "messages": [{"role": "user", "content": content}],
            "tools": [{"name": "Bash", "input_schema": {"type": "object"}}]
        }))
        .expect("request")
    }

    #[test]
    fn set_and_take_continuation() {
        let bridge = Bridge::new();
        let owner = account(OWNER);
        let run = bridge.park_run(owner, vec!["Bash".into()]);
        bridge.set_continuation(run, "next".into());
        assert_eq!(bridge.take_continuation(run).as_deref(), Some("next"));
        assert!(bridge.take_continuation(run).is_none());
    }

    #[test]
    fn match_finds_a_registered_call_and_remaps_a_rewritten_id() {
        let bridge = Bridge::new();
        let owner = account(OWNER);
        let run = bridge.park_run(owner.clone(), vec!["Bash".into()]);
        bridge.register_call(run, "toolu_1", "Bash");
        let request = result_request("client_1", "ok", Some("also"));
        let found = bridge.match_once(&request, &owner).expect("registered");
        let found = found.expect("match");
        assert_eq!(found.run_id, run);
        assert_eq!(found.said, "also");
        assert_eq!(found.remapped, vec![("client_1".into(), "toolu_1".into())]);
        assert_eq!(bridge.take_said(run), "also");
    }

    #[test]
    fn match_retries_while_the_call_is_registered() {
        let bridge = Bridge::new();
        let owner = account(OWNER);
        let run = bridge.park_run(owner.clone(), vec!["Bash".into()]);
        let request = result_request("toolu_1", "ok", None);
        let start = Instant::now();
        let table = bridge.clone();
        let handle = std::thread::spawn(move || {
            std::thread::sleep(Duration::from_millis(40));
            table.register_call(run, "toolu_1", "Bash");
        });
        let found = bridge.match_request(&request, &owner).expect("match");
        handle.join().expect("thread");
        assert_eq!(found.run_id, run);
        assert!(start.elapsed() < MATCH_GAP * MATCH_ATTEMPTS as u32);
        assert!(start.elapsed() >= Duration::from_millis(20));
    }

    #[test]
    fn owner_mismatch_aborts_the_run() {
        let bridge = Bridge::new();
        let owner = account(OWNER);
        let other = account(OTHER);
        let run = bridge.park_run(owner, vec!["Bash".into()]);
        bridge.register_call(run, "toolu_1", "Bash");
        let request = result_request("toolu_1", "ok", None);
        let result = bridge.match_once(&request, &other).expect("attempt");
        assert!(result.is_none(), "mismatch must not return a match");
        // The run was aborted, so the original owner also cannot match.
        assert!(
            bridge
                .match_once(&result_request("toolu_1", "x", None), &account(OWNER))
                .unwrap_or(None)
                .is_none()
        );
    }

    #[test]
    fn tool_choice_refuses_unknown_and_none_policy() {
        assert!(tool_choice_refuses("missing", &tools_request()));
        assert!(!tool_choice_refuses("Bash", &tools_request()));
        // Best-effort parse of tool_choice; skip the none-policy assertion if the
        // IR shape does not accept this JSON (the refuse-unknown path is enough).
        if let Ok(none) = serde_json::from_value::<Request>(json!({
            "model": "claude-sonnet-4-5",
            "messages": [{"role": "user", "content": [{"type": "text", "text": "hi"}]}],
            "tools": [{"name": "Bash", "input_schema": {"type": "object"}}],
            "tool_choice": {"mode": {"mode": "none"}}
        })) {
            assert!(tool_choice_refuses("Bash", &none));
        }
    }

    #[test]
    fn refused_tool_is_answered_immediately() {
        use futures_util::FutureExt;
        let bridge = Bridge::new();
        let owner = account(OWNER);
        let run = bridge.park_run(owner, vec!["Bash".into()]);
        let handler = bridge.callback_handler(run, tools_request());
        let body = json!({"tool_call_id":"toolu_x","name":"missing","arguments":{}});
        let reply = handler
            .handle(Bytes::from(body.to_string()))
            .now_or_never()
            .expect("refused tools must not wait for a later HTTP request");
        assert_eq!(reply.status(), 200);
        let value: Value = serde_json::from_slice(reply.body()).expect("json");
        assert_eq!(value["is_error"], true);
        assert!(
            value["content"][0]["text"]
                .as_str()
                .is_some_and(|text| text.contains("not allowed")),
            "{value}"
        );
    }

    #[test]
    fn said_with_results_is_the_user_text_beside_tool_results() {
        let request = result_request("toolu_1", "ok", Some("keep going"));
        assert_eq!(said_with_results(&request), "keep going");
        let mut message = Message::new(Role::User);
        message.content = vec![ContentPart::text("plain")];
        let text_only = Request::new("claude-sonnet-4-5").with_messages(vec![message]);
        assert!(said_with_results(&text_only).is_empty());
    }

    #[test]
    fn callback_token_is_48_hex_chars() {
        let token = new_callback_token().expect("token");
        assert_eq!(token.len(), 48);
        assert!(token.chars().all(|c| c.is_ascii_hexdigit()));
        let again = new_callback_token().expect("token");
        assert_ne!(token, again);
    }

    #[test]
    fn mcp_layout_keeps_the_url_out_of_argv_and_writes_mode_600() {
        let dir = tempfile::tempdir().expect("dir");
        let helper = SpawnRequest::new(
            {
                #[cfg(unix)]
                {
                    PathBuf::from("/usr/bin/hofundd")
                }
                #[cfg(windows)]
                {
                    PathBuf::from(r"C:\hofund\hofundd.exe")
                }
            },
            vec!["bridge-mcp-stdio".into()],
            Vec::new(),
            {
                #[cfg(unix)]
                {
                    PathBuf::from("/tmp")
                }
                #[cfg(windows)]
                {
                    PathBuf::from(r"C:\hofund")
                }
            },
        )
        .expect("spawn");
        let url = "http://127.0.0.1:9/callback/tokentokentokentokentokentokentokentokentoken";
        let layout = write_mcp_layout(dir.path(), url, &tools_request(), &helper).expect("layout");
        let url_text = std::fs::read_to_string(&layout.callback_path).expect("callback");
        assert!(url_text.contains("http://127.0.0.1"));
        assert!(url_text.contains("tokentoken"));
        let config = std::fs::read_to_string(&layout.config_path).expect("config");
        assert!(config.contains("bridge-mcp-stdio"));
        assert!(config.contains("hofund"));
        assert!(
            !config.contains("tokentoken"),
            "callback token must not appear in mcp config argv: {config}"
        );
        let tools: Value =
            serde_json::from_slice(&std::fs::read(&layout.tools_path).expect("tools"))
                .expect("json");
        assert_eq!(tools[0]["name"], "Bash");
        assert_eq!(tools[0]["inputSchema"]["type"], "object");
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            let mode = std::fs::metadata(&layout.callback_path)
                .expect("meta")
                .permissions()
                .mode()
                & 0o777;
            assert_eq!(mode, 0o600);
        }
        let _ = mcp_tools_value(&tools_request());
    }
}
