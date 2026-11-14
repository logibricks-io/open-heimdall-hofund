//! Optional prompt-cache keepalive on the host scheduler.

use std::collections::VecDeque;
use std::fmt::Write as _;
use std::sync::{Arc, Mutex, Weak};
use std::time::Duration;

use hofund_plugin_api::{ContentPart, HostContext, JobId, ProtocolId, Request, Role, SecretString};
use hofund_plugin_sdk::oauth::secret_text;
use hofund_plugin_sdk::{Call, digest, sleep};

use crate::catalog;
use crate::connect::chat::{ChatDecoder, ChatEvent, ChatUsage, EncodedChatRequest};
use crate::creds::Stored;
use crate::errors;
use crate::upstream::{CHAT_PATH, read_capped};

const DEFAULT_INTERVAL: Duration = Duration::from_secs(285);
const DEFAULT_MAX_PINGS: u8 = 6;
const MAX_CONVERSATIONS: usize = 32;
const MAX_FIRST_MESSAGES: usize = 256;
const ERROR_BODY_CAP: usize = 64 * 1024;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) struct Settings {
    pub(crate) enabled: bool,
    pub(crate) interval: Duration,
    pub(crate) max_pings: u8,
}

impl Default for Settings {
    fn default() -> Self {
        Self {
            enabled: false,
            interval: DEFAULT_INTERVAL,
            max_pings: DEFAULT_MAX_PINGS,
        }
    }
}

impl Settings {
    pub(crate) fn new(enabled: bool, interval_seconds: i64, max_pings: i64) -> Self {
        let defaults = Self::default();
        Self {
            enabled,
            interval: (30..=299)
                .contains(&interval_seconds)
                .then_some(interval_seconds)
                .map_or(defaults.interval, |seconds| {
                    Duration::from_secs(seconds as u64)
                }),
            max_pings: (1..=50)
                .contains(&max_pings)
                .then_some(max_pings)
                .map_or(defaults.max_pings, |max| max as u8),
        }
    }
}

#[derive(Clone)]
pub(crate) struct Keepalive {
    inner: Arc<Inner>,
}

struct Inner {
    host: HostContext,
    settings: Settings,
    state: Mutex<State>,
}

impl Drop for Inner {
    fn drop(&mut self) {
        let jobs = self
            .state
            .get_mut()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .entries
            .drain(..)
            .filter_map(|entry| entry.job)
            .collect::<Vec<_>>();
        for job in jobs {
            let _ = self.host.scheduler().cancel(job);
        }
    }
}

#[derive(Default)]
struct State {
    entries: Vec<Active>,
    firsts: VecDeque<FirstMessage>,
    generation: u64,
}

struct Active {
    id: String,
    generation: u64,
    job: Option<JobId>,
}

struct FirstMessage {
    id: String,
    group: String,
    count: usize,
    blocks: Vec<String>,
    last_len: usize,
    last_hash: String,
}

pub(crate) struct PingRequest {
    origin: String,
    key: SecretString,
    body: Vec<u8>,
    gzipped: bool,
}

impl Keepalive {
    pub(crate) fn new(host: &HostContext, settings: Settings) -> Self {
        Self {
            inner: Arc::new(Inner {
                host: host.clone(),
                settings,
                state: Mutex::new(State::default()),
            }),
        }
    }

    pub(crate) fn begin(&self, request: &Request, inbound: ProtocolId) -> Option<String> {
        if !self.inner.settings.enabled {
            self.stop_all();
            return None;
        }
        let id = self.key(request, inbound);
        self.stop(&id);
        Some(id)
    }

    pub(crate) fn schedule(
        &self,
        id: String,
        stored: &Stored,
        encoded: &EncodedChatRequest,
        sent_at: Duration,
    ) {
        let request = PingRequest {
            origin: catalog::origin(stored),
            key: stored.credential.key.clone(),
            body: encoded.body.clone(),
            gzipped: encoded.gzipped,
        };
        self.stop(&id);
        let (generation, expired) = {
            let mut state = self.lock();
            state.generation = state.generation.wrapping_add(1);
            let generation = state.generation;
            let expired = if state.entries.len() >= MAX_CONVERSATIONS {
                state.entries.remove(0).job
            } else {
                None
            };
            state.entries.push(Active {
                id: id.clone(),
                generation,
                job: None,
            });
            (generation, expired)
        };
        if let Some(job) = expired {
            let _ = self.inner.host.scheduler().cancel(job);
        }

        let weak = Arc::downgrade(&self.inner);
        let host = self.inner.host.clone();
        let task_host = host.clone();
        let settings = self.inner.settings;
        let job_id = id.clone();
        let result = self.inner.host.scheduler().run_once(
            "devin-cache-keepalive",
            Box::pin(async move {
                Self::run(
                    task_host, settings, weak, job_id, generation, request, sent_at,
                )
                .await;
            }),
        );
        match result {
            Ok(job) => {
                let current = {
                    let mut state = self.lock();
                    if let Some(active) = state
                        .entries
                        .iter_mut()
                        .find(|entry| entry.id == id && entry.generation == generation)
                    {
                        active.job = Some(job);
                        true
                    } else {
                        false
                    }
                };
                if !current {
                    let _ = host.scheduler().cancel(job);
                }
            }
            Err(_) => self.finish(&id, generation),
        }
    }

    fn stop(&self, id: &str) {
        let job = {
            let mut state = self.lock();
            state
                .entries
                .iter()
                .position(|entry| entry.id == id)
                .and_then(|index| state.entries.remove(index).job)
        };
        if let Some(job) = job {
            let _ = self.inner.host.scheduler().cancel(job);
        }
    }

    fn stop_all(&self) {
        let jobs = self
            .lock()
            .entries
            .drain(..)
            .filter_map(|entry| entry.job)
            .collect::<Vec<_>>();
        for job in jobs {
            let _ = self.inner.host.scheduler().cancel(job);
        }
    }

    fn finish(&self, id: &str, generation: u64) {
        self.lock()
            .entries
            .retain(|entry| entry.id != id || entry.generation != generation);
    }

    async fn run(
        host: HostContext,
        settings: Settings,
        weak: Weak<Inner>,
        id: String,
        generation: u64,
        request: PingRequest,
        mut previous_sent: Duration,
    ) {
        let mut misses = 0_u8;
        for _ in 0..settings.max_pings {
            let elapsed = host.clock().monotonic().saturating_sub(previous_sent);
            sleep(settings.interval.saturating_sub(elapsed)).await;
            let sent_at = host.clock().monotonic();
            let result = ping_once(&host, &request).await;
            if matches!(result.status, 401 | 402 | 429) {
                break;
            }
            let hit = result.status == 200
                && result.usage.cache_read > 0
                && u128::from(result.usage.cache_read) * 2
                    >= u128::from(result.usage.input)
                        + u128::from(result.usage.cache_read)
                        + u128::from(result.usage.cache_write);
            misses = if hit { 0 } else { misses + 1 };
            if misses >= 2 {
                break;
            }
            previous_sent = sent_at;
        }
        if let Some(inner) = weak.upgrade() {
            Keepalive { inner }.finish(&id, generation);
        }
    }

    fn key(&self, request: &Request, inbound: ProtocolId) -> String {
        let conversation = request.conversation.as_ref();
        let session = conversation.map_or("", |value| value.session.trim());
        let subagent = conversation.map_or("", |value| value.subagent.trim());
        let system = request
            .messages
            .iter()
            .filter(|message| matches!(message.role, Role::System | Role::Developer))
            .flat_map(|message| content_texts(&message.content))
            .map(|text| {
                if inbound == ProtocolId::ANTHROPIC_MESSAGES {
                    crate::upstream::strip_billing_header(&text).to_owned()
                } else {
                    text
                }
            })
            .collect::<Vec<_>>();
        let tools = request
            .tools
            .iter()
            .map(|tool| tool.name.as_str())
            .collect::<Vec<_>>();
        let prefix = hofund_plugin_sdk::json::to_string(&(system, tools))
            .expect("conversation prefix serializes");
        let group = format!("{session}\n{subagent}\n{}", hash(&prefix));
        if !session.is_empty() {
            return group;
        }
        let first = if inbound == ProtocolId::ANTHROPIC_MESSAGES {
            request
                .messages
                .iter()
                .find(|message| !matches!(message.role, Role::System | Role::Developer))
        } else {
            request
                .messages
                .iter()
                .find(|message| message.role == Role::User)
        };
        let first = first.map_or_else(Vec::new, |message| content_texts(&message.content));
        self.lock().first_of(group, first)
    }

    fn lock(&self) -> std::sync::MutexGuard<'_, State> {
        self.inner
            .state
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
    }
}

impl State {
    fn first_of(&mut self, group: String, first: Vec<String>) -> String {
        let hashes = first.iter().map(|text| hash(text)).collect::<Vec<_>>();
        let mut best = None;
        for (index, known) in self.firsts.iter().enumerate() {
            if known.group != group
                || known.count > first.len()
                || (known.count == 0) != first.is_empty()
            {
                continue;
            }
            if !first.is_empty() {
                let current = &first[known.count - 1];
                if known
                    .blocks
                    .iter()
                    .zip(&hashes)
                    .any(|(left, right)| left != right)
                    || current.len() < known.last_len
                    || current
                        .get(..known.last_len)
                        .is_none_or(|prefix| hash(prefix) != known.last_hash)
                {
                    continue;
                }
            }
            if best.is_none_or(|(_, count, len)| {
                known.count > count || (known.count == count && known.last_len > len)
            }) {
                best = Some((index, known.count, known.last_len));
            }
        }
        let id = best.map_or_else(
            || {
                let text =
                    hofund_plugin_sdk::json::to_string(&first).expect("first message serializes");
                format!("{group}\n{}", hash(&text))
            },
            |(index, _, _)| self.firsts[index].id.clone(),
        );
        let last = first.last().map_or("", String::as_str);
        let known = FirstMessage {
            id: id.clone(),
            group,
            count: first.len(),
            blocks: hashes[..hashes.len().saturating_sub(1)].to_vec(),
            last_len: last.len(),
            last_hash: hash(last),
        };
        if let Some((index, _, _)) = best {
            self.firsts.remove(index);
        } else if let Some(index) = self.firsts.iter().position(|known| known.id == id) {
            self.firsts.remove(index);
        }
        self.firsts.push_back(known);
        while self.firsts.len() > MAX_FIRST_MESSAGES {
            self.firsts.pop_front();
        }
        id
    }
}

fn content_texts(parts: &[ContentPart]) -> Vec<String> {
    parts
        .iter()
        .map(|part| match part {
            ContentPart::Text(text) => text.text.clone(),
            ContentPart::Image(_) => "image".to_owned(),
            ContentPart::ToolCall(_) => "tool_use".to_owned(),
            ContentPart::ToolResult(_) => "tool_result".to_owned(),
            ContentPart::Thinking(_) => "thinking".to_owned(),
            ContentPart::Unknown(part) => part.kind.clone(),
        })
        .collect()
}

fn hash(text: &str) -> String {
    digest::sha256(text.as_bytes())[..16]
        .iter()
        .fold(String::with_capacity(32), |mut out, byte| {
            write!(out, "{byte:02x}").expect("write hash");
            out
        })
}

struct PingOutcome {
    status: u16,
    usage: ChatUsage,
}

async fn ping_once(host: &HostContext, request: &PingRequest) -> PingOutcome {
    let mut call = Call::post(format!("{}{CHAT_PATH}", request.origin))
        .header("Content-Type", "application/connect+proto")
        .header("Connect-Protocol-Version", "1")
        .sensitive_header(
            "Authorization",
            format!("Basic {}", secret_text(&request.key)),
        )
        .read_timeout(Duration::from_secs(300))
        .bytes(request.body.clone());
    if request.gzipped {
        call = call.header("Connect-Content-Encoding", "gzip");
    }
    let response = match call.open_raw(host.http(), 1).await {
        Ok(response) => response,
        Err(_) => return PingOutcome::status(0),
    };
    let status = response.status().as_u16();
    if !(200..300).contains(&status) {
        let body = match read_capped(response.into_body(), ERROR_BODY_CAP).await {
            Ok(body) => body,
            Err(_) => return PingOutcome::status(0),
        };
        return PingOutcome::status(
            errors::failure(status, &body)
                .and_then(|error| error.status())
                .unwrap_or(status),
        );
    }
    let mut body = response.into_body();
    let mut decoder = ChatDecoder::default();
    let mut usage = ChatUsage::default();
    loop {
        match body.next_chunk().await {
            Some(Ok(chunk)) => {
                if decoder.push(&chunk).is_err() {
                    return PingOutcome::status(0);
                }
            }
            Some(Err(_)) => return PingOutcome::status(0),
            None => break,
        }
        while let Some(event) = decoder.next_event() {
            match event {
                ChatEvent::Usage(next) => merge_usage(&mut usage, next),
                ChatEvent::End(Some(error)) => {
                    return PingOutcome::status(errors::connect(&error).status().unwrap_or(502));
                }
                ChatEvent::End(None) => return PingOutcome { status: 200, usage },
                _ => {}
            }
        }
    }
    if decoder.finish().is_err() {
        return PingOutcome::status(0);
    }
    PingOutcome { status: 200, usage }
}

impl PingOutcome {
    fn status(status: u16) -> Self {
        Self {
            status,
            usage: ChatUsage::default(),
        }
    }
}

fn merge_usage(usage: &mut ChatUsage, next: ChatUsage) {
    if next.input != 0 {
        usage.input = next.input;
    }
    if next.output != 0 {
        usage.output = next.output;
    }
    if next.cache_write != 0 {
        usage.cache_write = next.cache_write;
    }
    if next.cache_read != 0 {
        usage.cache_read = next.cache_read;
    }
}

#[cfg(test)]
mod tests {
    use std::time::Duration;

    use hofund_plugin_api::{ContentPart, Conversation, Message, ProtocolId, Request, Role};

    use super::{Keepalive, Settings};

    #[test]
    fn settings_use_reference_defaults_for_out_of_range_values() {
        assert_eq!(
            Settings::new(true, 600, 0),
            Settings {
                enabled: true,
                interval: Duration::from_secs(285),
                max_pings: 6,
            }
        );
        assert_eq!(
            Settings::new(true, 120, 2),
            Settings {
                enabled: true,
                interval: Duration::from_secs(120),
                max_pings: 2,
            }
        );
        assert!(!Settings::default().enabled);
    }

    fn request(text: &str) -> Request {
        let mut request = Request::new("claude-haiku-5-5");
        request.messages = vec![Message::new(Role::System), Message::new(Role::User)];
        request.messages[0].content = vec![ContentPart::text("Instructions")];
        request.messages[1].content = vec![ContentPart::text(text)];
        request
    }

    #[test]
    fn unnamed_first_messages_match_as_they_grow() {
        let dir = tempfile::tempdir().expect("dir");
        let host = crate::tests::host(&dir);
        let keepalive = Keepalive::new(host.context(), Settings::default());
        let first = keepalive.key(&request("History A"), ProtocolId::ANTHROPIC_MESSAGES);
        assert_eq!(
            keepalive.key(
                &request("History A, then B"),
                ProtocolId::ANTHROPIC_MESSAGES
            ),
            first
        );
        assert_ne!(
            keepalive.key(&request("Something else"), ProtocolId::ANTHROPIC_MESSAGES),
            first
        );
    }

    #[test]
    fn named_sessions_and_subagents_have_independent_keys() {
        let dir = tempfile::tempdir().expect("dir");
        let host = crate::tests::host(&dir);
        let keepalive = Keepalive::new(host.context(), Settings::default());
        let mut request = request("same first message");
        request.conversation = Some(Conversation::new("session-1", ""));
        let lead = keepalive.key(&request, ProtocolId::ANTHROPIC_MESSAGES);
        request.messages[1]
            .content
            .push(ContentPart::text("later turns do not change the key"));
        assert_eq!(
            keepalive.key(&request, ProtocolId::ANTHROPIC_MESSAGES),
            lead
        );
        request.conversation = Some(Conversation::new("session-1", "agent-1"));
        assert_ne!(
            keepalive.key(&request, ProtocolId::ANTHROPIC_MESSAGES),
            lead
        );
        request.conversation = Some(Conversation::new("session-2", ""));
        assert_ne!(
            keepalive.key(&request, ProtocolId::ANTHROPIC_MESSAGES),
            lead
        );
    }

    #[test]
    fn schedule_registers_a_cancellable_host_job() {
        let dir = tempfile::tempdir().expect("dir");
        let host = hofund_kit_subscription::TestHost::open_for_with_tokio_monotonic(
            hofund_plugin_api::PluginKind::Subscription,
            hofund_plugin_api::PluginId::from_static("devin"),
            dir.path(),
        );
        let keepalive = Keepalive::new(host.context(), Settings::new(true, 30, 2));
        let request = request("hello");
        let id = keepalive
            .begin(&request, ProtocolId::ANTHROPIC_MESSAGES)
            .expect("keepalive enabled");
        let credential = crate::creds::Credential {
            key: hofund_plugin_api::SecretString::from("key".to_owned()),
            api_server: "https://server.test".to_owned(),
            devin_api: "https://api.devin.test".to_owned(),
            label: "account@example.test".to_owned(),
            plan: String::new(),
            org_id: String::new(),
            team_id: String::new(),
        };
        let stored = crate::creds::Stored {
            credential,
            cli_path: None,
            needs_sign_in: false,
        };
        let prompts = [crate::connect::chat::Prompt::text(
            "prompt-id",
            crate::connect::chat::PromptRole::User,
            "hello",
        )];
        let encoded = crate::connect::chat::encode_request(&crate::connect::chat::ChatRequest {
            session_token: "key",
            os: "linux",
            model_uid: "swe-2-high",
            prompts: &prompts,
            tools: &[],
            max_tokens: None,
            temperature: None,
            top_p: None,
        })
        .expect("encoded");
        hofund_kit_subscription::block_on_paused(async {
            keepalive.schedule(id, &stored, &encoded, host.context().clock().monotonic());
            assert_eq!(host.scheduler().running_once().len(), 1);
            drop(keepalive);
            hofund_kit_subscription::yield_times(4).await;
            assert!(host.scheduler().running_once().is_empty());
        });
    }
}
