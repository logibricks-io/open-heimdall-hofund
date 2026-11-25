//! The ChatGPT Codex backend as an OpenAI Responses upstream.

use std::sync::{Arc, Mutex};

use hofund_plugin_api::{
    AccountId, Completion, CredentialStyle, HostContext, PluginError, ProbeQuery, ProbeReport,
    ProtocolId, Request, UpstreamProvider,
};
use hofund_plugin_sdk::Call;
use hofund_plugin_sdk::codex_tool_names;
use hofund_plugin_sdk::oauth::secret_text;
use hofund_plugin_sdk::wire::{self, Target};

use hofund_plugin_sdk::json::{self, Value};

use crate::models::{self, Prompts};
use crate::refusal;
use crate::tokens::{self, Access, Need};
use crate::version::{self, ClientVersion};
use crate::{Endpoints, limits, shape};

/// Body fields the Codex backend refuses or ignores, dropped before
/// encoding; [`shape::body`] reshapes what the codec then writes.
const DROPPED_FIELDS: [&str; 9] = [
    "max_output_tokens",
    "max_completion_tokens",
    "temperature",
    "top_p",
    "previous_response_id",
    "user",
    "safety_identifier",
    "prompt_cache_options",
    "store",
];

pub(crate) struct CodexUpstream {
    account: AccountId,
    host: HostContext,
    endpoints: Arc<Endpoints>,
    prompts: Arc<Prompts>,
    versions: Arc<ClientVersion>,
    /// Headers the last [`Self::target`] put on the routed call.
    ///
    /// `Target` marks those values sensitive, so the recording client keeps
    /// only the names. Tests read this copy.
    #[cfg(test)]
    signed: Mutex<Vec<(String, String)>>,
}

impl CodexUpstream {
    pub(crate) const fn new(
        account: AccountId,
        host: HostContext,
        endpoints: Arc<Endpoints>,
        prompts: Arc<Prompts>,
        versions: Arc<ClientVersion>,
    ) -> Self {
        Self {
            account,
            host,
            endpoints,
            prompts,
            versions,
            #[cfg(test)]
            signed: Mutex::new(Vec::new()),
        }
    }

    /// Headers the last routed call was signed with.
    #[cfg(test)]
    pub(crate) fn signed_headers(&self) -> Vec<(String, String)> {
        self.signed
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .clone()
    }

    /// The instructions Codex CLI sends `model` with: kept from a model
    /// list, else from the account's list asked for now, at most once per
    /// [`models::ASK_AGAIN`].
    async fn instructions(&self, access: &Access, model: &str) -> Option<String> {
        let client_version = version::prepare(&self.host, &self.endpoints, &self.versions).await;
        if let Some(text) = self.prompts.get(&self.host, model) {
            return Some(text);
        }
        if !self.prompts.may_ask(
            &self.account,
            self.host.clock().monotonic(),
            &client_version,
        ) {
            return None;
        }
        // A list that fails leaves the client's instructions in place.
        let _ = models::list(
            &self.host,
            &self.endpoints,
            access,
            &self.prompts,
            &self.versions,
        )
        .await;
        self.prompts.get(&self.host, model)
    }

    /// Sends `request` as a stream and records the usage limits the reply
    /// tells. A client that asked for one reply gets the stream folded
    /// into one.
    async fn send(
        &self,
        access: &Access,
        mut request: Request,
        inbound: ProtocolId,
    ) -> Result<Completion, PluginError> {
        let codex = self.instructions(access, &request.model).await;
        let declared: Vec<String> = request.tools.iter().map(|tool| tool.name.clone()).collect();
        let back = codex_tool_names::codex_tool_names_back(&declared);
        let original = take_compact(&mut request);
        let unary = !request.stream && original.is_none();
        request.stream = true;
        let posted = Arc::new(Mutex::new(None::<Vec<u8>>));
        let trimmed = Arc::new(Mutex::new(None::<Vec<u8>>));
        let mut tries = 0_u8;
        let sent = loop {
            let target = self.target(
                access,
                codex.as_deref(),
                Arc::clone(&posted),
                Arc::clone(&trimmed),
                &back,
                original.clone(),
            );
            match wire::send(
                self.host.http(),
                &target,
                Some(&access.token),
                request.clone(),
                inbound,
            )
            .await
            {
                Ok(sent) => break sent,
                Err(error) if unreadable_refusal(error.status()) => {
                    if refusal::unreadable_retry_exhausted(tries) {
                        return Err(error);
                    }
                    let body = trimmed
                        .lock()
                        .unwrap_or_else(std::sync::PoisonError::into_inner)
                        .clone()
                        .or_else(|| {
                            posted
                                .lock()
                                .unwrap_or_else(std::sync::PoisonError::into_inner)
                                .clone()
                        });
                    let Some(posted_body) = body else {
                        return Err(error);
                    };
                    let Some(next) =
                        refusal::without_unreadable(&posted_body, error.message().as_bytes())
                    else {
                        return Err(error);
                    };
                    *trimmed
                        .lock()
                        .unwrap_or_else(std::sync::PoisonError::into_inner) = Some(next);
                    tries += 1;
                }
                Err(error) => return Err(error),
            }
        };
        let credits = limits::header_credits(sent.headers());
        limits::observe(
            &self.host,
            &limits::header_limits(sent.headers(), self.host.clock().now()),
            credits,
        );
        let host = self.host.clone();
        let sent = sent.watch(
            limits::SCAN,
            Arc::new(move |data| limits::watch(&host, data, credits)),
        );
        if unary {
            sent.fold().await
        } else {
            sent.complete().await
        }
    }

    fn target(
        &self,
        access: &Access,
        codex: Option<&str>,
        posted: Arc<Mutex<Option<Vec<u8>>>>,
        trimmed: Arc<Mutex<Option<Vec<u8>>>>,
        back: &std::collections::BTreeMap<String, String>,
        original: Option<String>,
    ) -> Target {
        let posted_capture = Arc::clone(&posted);
        let trimmed_body = Arc::clone(&trimmed);
        let codex = codex.map(str::to_owned);
        let signed = headers(access.account.as_deref(), &self.versions.current());
        #[cfg(test)]
        {
            *self
                .signed
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner) = signed.clone();
        }
        let mut target = Target::new(ProtocolId::OPENAI_RESPONSES, self.endpoints.base.clone())
            .credential(CredentialStyle::Bearer)
            .chatgpt_codex_zstd(true)
            .headers(signed)
            .rewrite(Arc::new(move |body| {
                if let Some(raw) = trimmed_body
                    .lock()
                    .unwrap_or_else(std::sync::PoisonError::into_inner)
                    .clone()
                {
                    if let Ok(Value::Object(replacement)) = json::from_slice::<Value>(&raw) {
                        body.clear();
                        body.extend(replacement);
                    }
                    return shape::session_headers(body);
                }
                let headers = shape::body(body, codex.as_deref());
                if let Some(original) = original.as_deref() {
                    append_trigger(body, Some(original));
                }
                let bytes = Value::Object(body.clone()).to_string().into_bytes();
                *posted_capture
                    .lock()
                    .unwrap_or_else(std::sync::PoisonError::into_inner) = Some(bytes);
                headers
            }));
        if !back.is_empty() {
            let restore = Arc::new(back.clone());
            let unwrap = Arc::clone(&restore);
            target = target.unwrap(Arc::new(move |mut value| {
                codex_tool_names::restore_tool_names(&mut value, &unwrap);
                value
            }));
        }
        target
    }
}

fn unreadable_refusal(status: Option<u16>) -> bool {
    matches!(status, Some(400) | Some(404))
}

#[hofund_plugin_sdk::async_trait]
impl UpstreamProvider for CodexUpstream {
    fn attempt_base(&self) -> String {
        self.endpoints.base.clone()
    }

    fn compacts_by_trigger(&self) -> bool {
        true
    }

    fn serves_responses(&self, _model: &str) -> bool {
        true
    }

    async fn execute(
        &self,
        mut request: Request,
        inbound: ProtocolId,
    ) -> Result<Completion, PluginError> {
        self.versions.note_request(&request);
        shape(&mut request);
        let access =
            tokens::access(&self.host, &self.endpoints, &self.account, Need::IfExpiring).await?;
        match self.send(&access, request.clone(), inbound).await {
            Err(error) if error.status() == Some(401) && token_expired(error.message()) => {
                // The backend called the token expired before its `exp`:
                // refresh once and send again.
                let renewed = tokens::access(
                    &self.host,
                    &self.endpoints,
                    &self.account,
                    Need::Replace(access.token),
                )
                .await?;
                self.send(&renewed, request, inbound).await
            }
            other => other,
        }
    }

    async fn probe(&self, query: &ProbeQuery) -> Result<ProbeReport, PluginError> {
        let mut request = query.minimal_request();
        request.stream = true;
        let completion = self.execute(request, query.protocol()).await?;
        ProbeReport::from_completion(completion).await
    }
}

/// Drops what the Codex backend refuses from `request`.
pub(crate) fn shape(request: &mut Request) {
    request.params.temperature = None;
    request.params.top_p = None;
    request.params.max_tokens = None;
    for field in DROPPED_FIELDS {
        request.extensions.remove(field);
        request.params.extensions.remove(field);
    }
}

/// The headers Codex CLI sends, besides the bearer token.
pub(crate) fn headers(account: Option<&str>, version: &str) -> Vec<(String, String)> {
    let mut headers = Vec::with_capacity(6);
    if let Some(account) = account.filter(|account| !account.is_empty()) {
        headers.push(("chatgpt-account-id".to_owned(), account.to_owned()));
    }
    headers.extend(
        [
            ("OpenAI-Beta", "responses=experimental".to_owned()),
            ("originator", "codex_cli_rs".to_owned()),
            ("version", version.to_owned()),
            (
                "User-Agent",
                user_agent(std::env::consts::OS, std::env::consts::ARCH, version),
            ),
            ("Accept", "text/event-stream".to_owned()),
        ]
        .map(|(name, value)| (name.to_owned(), value)),
    );
    headers
}

/// A `GET` of `url` as Codex CLI sends one for the account: the bearer
/// token and [`headers`], asking for JSON.
pub(crate) fn account_call(url: String, access: &Access, version: &str) -> Call {
    account_request("GET", url, access, version)
}

/// A `POST` with the same bearer, account headers, and JSON accept line.
pub(crate) fn account_post(url: String, access: &Access, version: &str) -> Call {
    account_request("POST", url, access, version)
}

fn account_request(method: &'static str, url: String, access: &Access, version: &str) -> Call {
    let start = if method == "POST" {
        Call::post(url)
    } else {
        Call::get(url)
    };
    headers(access.account.as_deref(), version)
        .into_iter()
        .filter(|(name, _)| !name.eq_ignore_ascii_case("accept"))
        .fold(
            start.sensitive_header(
                "authorization",
                format!("Bearer {}", secret_text(&access.token)),
            ),
            |call, (name, value)| call.sensitive_header(name, value),
        )
        .header("accept", "application/json")
}

/// The Codex CLI User-Agent for `os` on `arch`.
pub(crate) fn user_agent(os: &str, arch: &str, version: &str) -> String {
    let os = match os {
        "macos" => "Mac OS",
        "windows" => "Windows",
        "linux" => "Linux",
        other => other,
    };
    // The reference names Apple silicon and ARM Linux `arm64`.
    let arch = match arch {
        "aarch64" => "arm64",
        other => other,
    };
    let terminal = std::env::var("TERM_PROGRAM")
        .ok()
        .filter(|term| !term.is_empty() && term.is_ascii())
        .unwrap_or_else(|| "unknown".to_owned());
    format!("codex_cli_rs/{version} ({os}; {arch}) {terminal}")
}

/// Says an error message is the backend calling the access token expired.
/// `refresh_token_expired` does not count: that sign-in is gone.
pub(crate) fn token_expired(message: &str) -> bool {
    let lower = message.to_ascii_lowercase();
    lower.contains("authentication token is expired")
        || lower.contains("authentication token has expired")
        || lower.contains("please try refreshing it")
        || has_word(&lower, "token_expired")
}

const COMPACT_FLAG: &str = "hofund_responses_compact";
const COMPACT_BODY: &str = "hofund_responses_compact_body";

fn take_compact(request: &mut Request) -> Option<String> {
    let flagged = request
        .extensions
        .get(COMPACT_FLAG)
        .and_then(Value::as_bool)
        == Some(true);
    let original = match request.extensions.remove(COMPACT_BODY) {
        Some(Value::String(text)) => text,
        _ => String::new(),
    };
    request.extensions.remove(COMPACT_FLAG);
    flagged.then_some(original)
}

/// Replaces `input` with the client's own items, then one trigger when the
/// last item is not already that trigger. A string input is one user message.
/// Without a readable client body, the encoded input is normalized the same way.
fn append_trigger(body: &mut json::Map<String, Value>, original: Option<&str>) {
    if let Some(items) = original.and_then(client_items) {
        body.insert("input".to_owned(), Value::Array(with_trigger(items)));
        return;
    }
    let input = body
        .entry("input")
        .or_insert_with(|| Value::Array(Vec::new()));
    let items = match input {
        Value::String(text) => vec![user_text(text)],
        Value::Array(items) => std::mem::take(items),
        _ => return,
    };
    *input = Value::Array(with_trigger(items));
}

fn client_items(original: &str) -> Option<Vec<Value>> {
    if original.is_empty() {
        return None;
    }
    let Value::Object(client) = json::from_str::<Value>(original).ok()? else {
        return None;
    };
    Some(match client.get("input") {
        Some(Value::String(text)) => vec![user_text(text)],
        Some(Value::Array(items)) => items.clone(),
        _ => Vec::new(),
    })
}

fn user_text(text: &str) -> Value {
    json::json!({
        "type": "message",
        "role": "user",
        "content": [{"type": "input_text", "text": text}],
    })
}

fn with_trigger(mut items: Vec<Value>) -> Vec<Value> {
    let already = items
        .last()
        .and_then(|item| item.get("type"))
        .and_then(Value::as_str)
        == Some("compaction_trigger");
    if !already {
        items.push(json::json!({"type": "compaction_trigger"}));
    }
    items
}

fn has_word(text: &str, word: &str) -> bool {
    let word_byte = |byte: u8| byte.is_ascii_alphanumeric() || byte == b'_';
    let bytes = text.as_bytes();
    text.match_indices(word).any(|(start, found)| {
        let end = start + found.len();
        let before = start == 0 || !word_byte(bytes[start - 1]);
        let after = end == bytes.len() || !word_byte(bytes[end]);
        before && after
    })
}
