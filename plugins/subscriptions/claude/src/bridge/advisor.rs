//! Claude Code's advisor, answered by this bridge.
//!
//! The tool is hofund's own, beside the client's tools. It is not a user MCP
//! server. A model this gateway serves is asked through the caller's key.
//! A model it does not serve is left out and the instructions are voided.

use hofund_plugin_api::{
    AskHandle, CallerStamp, ContentPart, Extensions, Message, PluginError, PluginErrorKind,
    Request, Role, StreamEvent, TextPart, Tool, ToolChoice, UnknownEvent,
};
use serde_json::{Value, json};

/// Opens the instructions Claude Code writes when the advisor is on.
const NOTE: &str = "# Advisor Tool\n";
/// What Claude Code writes once the advisor is turned off.
const GONE: &str =
    "The advisor tool is no longer available; disregard the earlier advisor instructions.";
/// How many times one reply may call the advisor.
const USES: u32 = 3;
const ASK: &str = "You are the advisor the assistant has just consulted. Review the conversation above, in which the assistant works on the user's task, and answer its consultation yourself. Give the assistant brief, concrete advice for what to do next: the approach to take, what to check first, and any mistake or risk you see. Write plain text addressed to the assistant; do not call tools.";
const DESCRIPTION: &str = "Ask a stronger reviewer model for advice. Takes no parameters: the whole conversation so far is passed to it, and its advice comes back as the result.";

/// One answered advisor call, in the order Claude Code made them.
#[derive(Clone, Debug)]
pub(crate) struct AdviceOutcome {
    pub text: String,
    pub code: String,
}

/// A web search made as the original caller. Production leaves this unset.
pub(crate) trait CallerSearch: Send + Sync {
    /// Runs the search as `caller`.
    ///
    /// # Errors
    ///
    /// Returns [`PluginError`] when the search cannot run.
    fn search(&self, caller: &CallerStamp, arguments: &Value) -> Result<String, PluginError>;
}

/// Whether `name` is the advisor tool, including a client MCP prefix.
#[must_use]
pub(crate) fn is_advisor_name(name: &str) -> bool {
    name == "advisor" || name.ends_with("__advisor")
}

/// Whether `name` is a web search the run may make.
#[must_use]
pub(crate) fn is_search_name(name: &str) -> bool {
    name == "web_search" || name == "WebSearch"
}

/// Keeps a served advisor's instructions and offers the client tool.
///
/// An advisor model `ask` does not serve is dropped and its instructions are
/// voided. A client tool already named `advisor` is left as the client wrote it.
pub(crate) fn prepare(request: &mut Request, ask: &AskHandle) -> Option<String> {
    if request
        .tools
        .iter()
        .any(|tool| tool.name == "advisor" && !is_server(tool))
    {
        return None;
    }
    let model = request.tools.iter().find_map(|tool| {
        if !is_server(tool) {
            return None;
        }
        tool.extensions
            .get("model")
            .and_then(Value::as_str)
            .map(str::to_owned)
    })?;
    let caller = request.caller.clone().unwrap_or_default();
    if ask.serves(&caller, &model) {
        request.tools.retain(|tool| !is_server(tool));
        if !request.tools.iter().any(|tool| tool.name == "advisor") {
            request.tools.push(tool_def());
        }
        Some(model)
    } else {
        request.tools.retain(|tool| !is_server(tool));
        if request.tools.is_empty() {
            request.tool_choice = None;
        }
        for message in &mut request.messages {
            void_message(message);
        }
        None
    }
}

/// The request the advisor model is asked, without the executor's instructions.
#[must_use]
pub(crate) fn advice_request(request: &Request, said: &str, model: &str) -> Request {
    let mut out = request.clone();
    out.model = model.to_owned();
    out.stream = false;
    out.params.max_tokens = Some(4096);
    out.tool_choice = Some(ToolChoice::none());
    out.tools
        .retain(|tool| tool.name != "advisor" && !is_server(tool));
    out.messages = out.messages.iter().filter_map(cut_message).collect();
    if !said.trim().is_empty() {
        let mut message = Message::new(Role::Assistant);
        message.content.push(text_part(said));
        out.messages.push(message);
    }
    let ask = text_part(ASK);
    if out
        .messages
        .last()
        .is_some_and(|message| message.role == Role::User)
    {
        out.messages.last_mut().expect("last").content.push(ask);
    } else {
        let mut message = Message::new(Role::User);
        message.content.push(ask);
        out.messages.push(message);
    }
    out
}

/// Maps an ask failure onto the error code the client is told.
#[must_use]
pub(crate) fn error_code(error: &PluginError) -> &'static str {
    if error.kind() == PluginErrorKind::Timeout {
        return "execution_time_exceeded";
    }
    match error.status() {
        Some(413) => "prompt_too_long",
        Some(503 | 529) => "overloaded",
        Some(429) => "too_many_requests",
        _ => "unavailable",
    }
}

/// Text Claude Code is given for a failed call.
#[must_use]
pub(crate) fn failure_text(code: &str) -> String {
    if code == "max_uses_exceeded" {
        "The advisor failed: max_uses_exceeded. Go on without it.".to_owned()
    } else {
        format!("The advisor failed: {code}.")
    }
}

/// How many calls one reply may make.
#[must_use]
pub(crate) const fn max_uses() -> u32 {
    USES
}

/// Replaces an advisor `tool_use` with `server_tool_use` and `advisor_tool_result`.
///
/// Later block indexes move by one per replaced call. An empty `outcomes`
/// leaves the events as they are.
#[must_use]
pub(crate) fn splice(events: Vec<StreamEvent>, outcomes: &[AdviceOutcome]) -> Vec<StreamEvent> {
    if outcomes.is_empty() {
        return events;
    }
    let mut out = Vec::new();
    let mut shift = 0_u32;
    let mut cursor = 0;
    let mut used = 0;
    while cursor < events.len() {
        if let Some((index, id, name)) = super::decode::start_tool_call(&events[cursor])
            && is_advisor_name(&name)
        {
            cursor += 1;
            while cursor < events.len() {
                let stop = matches!(
                    &events[cursor],
                    StreamEvent::ContentBlockStop(stop) if stop.index == index
                );
                cursor += 1;
                if stop {
                    break;
                }
            }
            let outcome = outcomes.get(used).cloned().unwrap_or_else(unavailable);
            used += 1;
            out.extend(frames(index + shift, &id, &outcome));
            shift += 1;
            continue;
        }
        let mut event = events[cursor].clone();
        bump(&mut event, shift);
        out.push(event);
        cursor += 1;
    }
    out
}

fn unavailable() -> AdviceOutcome {
    AdviceOutcome {
        text: String::new(),
        code: "unavailable".to_owned(),
    }
}

fn frames(index: u32, id: &str, outcome: &AdviceOutcome) -> Vec<StreamEvent> {
    let shown = client_id(id);
    let content = if outcome.code.is_empty() {
        json!({"type": "advisor_result", "text": outcome.text})
    } else {
        json!({"type": "advisor_tool_result_error", "error_code": outcome.code})
    };
    vec![
        unknown(
            "content_block_start",
            json!({
                "type": "content_block_start",
                "index": index,
                "content_block": {
                    "type": "server_tool_use",
                    "id": shown,
                    "name": "advisor",
                    "input": {}
                }
            }),
        ),
        unknown(
            "content_block_delta",
            json!({
                "type": "content_block_delta",
                "index": index,
                "delta": {"type": "input_json_delta", "partial_json": ""}
            }),
        ),
        unknown(
            "content_block_stop",
            json!({"type": "content_block_stop", "index": index}),
        ),
        unknown(
            "content_block_start",
            json!({
                "type": "content_block_start",
                "index": index + 1,
                "content_block": {
                    "type": "advisor_tool_result",
                    "tool_use_id": shown,
                    "content": content
                }
            }),
        ),
        unknown(
            "content_block_stop",
            json!({"type": "content_block_stop", "index": index + 1}),
        ),
    ]
}

fn client_id(id: &str) -> String {
    if id.starts_with("srvtoolu_") {
        return id.to_owned();
    }
    format!("srvtoolu_{}", safe_id(id))
}

fn safe_id(id: &str) -> String {
    let safe = !id.is_empty()
        && !id.starts_with("mp_")
        && id
            .bytes()
            .all(|byte| byte.is_ascii_alphanumeric() || byte == b'_' || byte == b'-');
    if safe {
        return id.to_owned();
    }
    if id.is_empty() {
        return "advisor".to_owned();
    }
    let mut out = String::from("mp_");
    for byte in id.bytes() {
        if byte.is_ascii_alphanumeric() || byte == b'_' || byte == b'-' {
            out.push(char::from(byte));
        } else {
            out.push('_');
        }
    }
    out
}

fn unknown(name: &str, payload: Value) -> StreamEvent {
    StreamEvent::Unknown(UnknownEvent {
        name: name.to_owned(),
        payload,
    })
}

fn bump(event: &mut StreamEvent, by: u32) {
    if by == 0 {
        return;
    }
    match event {
        StreamEvent::ContentBlockStart(event) => event.index += by,
        StreamEvent::TextDelta(event) => event.index += by,
        StreamEvent::ThinkingDelta(event) => event.index += by,
        StreamEvent::SignatureDelta(event) => event.index += by,
        StreamEvent::ToolArgumentsDelta(event) => event.index += by,
        StreamEvent::ContentBlockStop(event) => event.index += by,
        _ => {}
    }
}

fn is_server(tool: &Tool) -> bool {
    tool.extensions
        .get("type")
        .and_then(Value::as_str)
        .is_some_and(|kind| kind.starts_with("advisor_"))
}

fn tool_def() -> Tool {
    let mut extensions = Extensions::new();
    extensions.insert("bridge_tool", json!(true));
    Tool {
        name: "advisor".to_owned(),
        description: Some(DESCRIPTION.to_owned()),
        input_schema: json!({"type": "object", "properties": {}}),
        extensions,
    }
}

fn void_message(message: &mut Message) {
    for part in &mut message.content {
        if let ContentPart::Text(text) = part {
            text.text = void_text(&text.text);
        }
    }
}

fn void_text(text: &str) -> String {
    let Some(start) = text.find(NOTE) else {
        return text.to_owned();
    };
    let rest = &text[start..];
    let end = rest
        .find("</system-reminder>")
        .map(|at| start + at)
        .unwrap_or(text.len());
    let note = &text[start..end];
    let trail = &note[note.trim_end().len()..];
    format!("{}{GONE}{trail}{}", &text[..start], &text[end..])
}

fn cut_message(message: &Message) -> Option<Message> {
    let mut kept = Message::new(message.role);
    kept.extensions = message.extensions.clone();
    for part in &message.content {
        match part {
            ContentPart::Thinking(_) => {}
            ContentPart::Text(text) => {
                let cut = cut_text(&text.text);
                if !cut.trim().is_empty() {
                    kept.content.push(text_part(&cut));
                }
            }
            other => kept.content.push(other.clone()),
        }
    }
    (!kept.content.is_empty()).then_some(kept)
}

fn cut_text(text: &str) -> String {
    let Some(start) = text.find(NOTE) else {
        return text.to_owned();
    };
    let rest = &text[start..];
    let end = rest
        .find("</system-reminder>")
        .map(|at| start + at)
        .unwrap_or(text.len());
    format!("{}{}", text[..start].trim_end(), &text[end..])
}

fn text_part(text: &str) -> ContentPart {
    ContentPart::Text(TextPart {
        text: text.to_owned(),
        extensions: Extensions::new(),
    })
}

#[cfg(test)]
mod tests {
    use std::sync::{Arc, Mutex};

    use hofund_kit_subscription::block_on;
    use hofund_plugin_api::{
        AccountId, AskBackend, AskHandle, CallerLimit, CallerStamp, Completion, ContentBlockStop,
        Message, PluginError, Request, Response, Role, StreamEvent,
    };
    use serde_json::json;

    use super::{NOTE, prepare, splice};
    use crate::bridge::park::Bridge;

    struct Gate {
        serves: bool,
        seen: Mutex<Vec<(CallerStamp, Request)>>,
    }

    #[hofund_plugin_sdk::async_trait]
    impl AskBackend for Gate {
        fn serves(&self, _caller: &CallerStamp, _model: &str) -> bool {
            self.serves
        }

        async fn ask(
            &self,
            caller: &CallerStamp,
            request: Request,
        ) -> Result<Completion, PluginError> {
            self.seen
                .lock()
                .expect("seen")
                .push((caller.clone(), request));
            let mut message = Message::new(Role::Assistant);
            message.content.push(super::text_part("check the boundary"));
            Ok(Completion::Unary(Box::new(Response::new(message))))
        }
    }

    fn server_tool(model: &str) -> hofund_plugin_api::Tool {
        let mut extensions = hofund_plugin_api::Extensions::new();
        extensions.insert("type", json!("advisor_20260301"));
        extensions.insert("model", json!(model));
        hofund_plugin_api::Tool {
            name: "advisor".to_owned(),
            description: None,
            input_schema: json!({}),
            extensions,
        }
    }

    fn instructed() -> Request {
        let mut request = Request::new("claude-sonnet");
        let mut system = Message::new(Role::System);
        system.content.push(super::text_part(
            "# Advisor Tool\nCall it.\n</system-reminder>",
        ));
        let mut user = Message::new(Role::User);
        user.content.push(super::text_part("ship it"));
        request.messages = vec![system, user];
        request.tools = vec![server_tool("relay/claude-opus-5-5")];
        request.caller = Some(CallerStamp {
            key_id: "friend".to_owned(),
            limits: vec![CallerLimit {
                period: "day".to_owned(),
                tokens: 700,
                ..CallerLimit::default()
            }],
            ..CallerStamp::default()
        });
        request
    }

    #[test]
    fn d227_served_advisor_keeps_instructions_and_offers_the_tool() {
        let ask = AskHandle::from_backend(Arc::new(Gate {
            serves: true,
            seen: Mutex::new(Vec::new()),
        }));
        let mut request = instructed();
        let model = prepare(&mut request, &ask).expect("served");
        assert_eq!(model, "relay/claude-opus-5-5");
        assert!(request.tools.iter().any(|tool| tool.name == "advisor"));
        assert!(request.tools.iter().all(|tool| {
            tool.extensions
                .get("type")
                .and_then(|value| value.as_str())
                .is_none_or(|kind| !kind.starts_with("advisor_"))
        }));
        let text = match &request.messages[0].content[0] {
            hofund_plugin_api::ContentPart::Text(text) => text.text.as_str(),
            _ => "",
        };
        assert!(text.contains(NOTE));
        assert!(!text.contains("no longer available"));
    }

    #[test]
    fn d227_unserved_advisor_is_left_out() {
        let ask = AskHandle::from_backend(Arc::new(Gate {
            serves: false,
            seen: Mutex::new(Vec::new()),
        }));
        let mut request = instructed();
        assert!(prepare(&mut request, &ask).is_none());
        assert!(request.tools.is_empty());
        let text = match &request.messages[0].content[0] {
            hofund_plugin_api::ContentPart::Text(text) => text.text.clone(),
            _ => String::new(),
        };
        assert!(text.contains("no longer available"));
        assert!(!text.contains(NOTE));
    }

    #[test]
    fn d227_advisor_callback_uses_the_caller_and_stops_at_three() {
        let gate = Arc::new(Gate {
            serves: true,
            seen: Mutex::new(Vec::new()),
        });
        let bridge = Bridge::new();
        bridge.use_ask(AskHandle::from_backend(
            Arc::clone(&gate) as Arc<dyn AskBackend>
        ));
        let run = bridge.park_run(
            AccountId::from_static("a-00000000000000aa"),
            vec!["advisor".into()],
        );
        let request = instructed();
        bridge.arm_advisor(run, "relay/claude-opus-5-5".into(), request.caller.clone());
        bridge.note_said(run, "Let me ask.");
        let handler = bridge.callback_handler(run, request);
        let body = json!({"tool_call_id":"toolu_1","name":"advisor","arguments":{}}).to_string();
        for _ in 0..3 {
            let reply = block_on(handler.handle(bytes::Bytes::from(body.clone())));
            let text = String::from_utf8(reply.body().to_vec()).expect("utf8");
            assert!(text.contains("check the boundary"));
            assert!(!text.contains("is_error\":true") && !text.contains("\"is_error\":true"));
        }
        let seen = gate.seen.lock().expect("seen");
        assert_eq!(seen.len(), 3);
        assert_eq!(seen[0].0.key_id, "friend");
        assert_eq!(seen[0].0.limits[0].tokens, 700);
        let asked = advice_text(&seen[0].1);
        assert!(asked.contains("Let me ask."));
        assert!(!asked.contains(NOTE));
        drop(seen);
        let reply = block_on(handler.handle(bytes::Bytes::from(body)));
        let text = String::from_utf8(reply.body().to_vec()).expect("utf8");
        assert!(text.contains("max_uses_exceeded"));
        assert_eq!(gate.seen.lock().expect("seen").len(), 3);
    }

    #[test]
    fn d227_search_callback_uses_the_caller() {
        struct Search {
            seen: Arc<Mutex<CallerStamp>>,
        }
        impl super::CallerSearch for Search {
            fn search(
                &self,
                caller: &CallerStamp,
                _arguments: &serde_json::Value,
            ) -> Result<String, PluginError> {
                *self.seen.lock().expect("seen") = caller.clone();
                Ok("found".to_owned())
            }
        }
        let seen = Arc::new(Mutex::new(CallerStamp::default()));
        let bridge = Bridge::new();
        let run = bridge.park_run(AccountId::from_static("a-00000000000000aa"), Vec::new());
        let request = instructed();
        bridge.set_caller(run, request.caller.clone());
        bridge.set_searcher(Arc::new(Search {
            seen: Arc::clone(&seen),
        }));
        let handler = bridge.callback_handler(run, request);
        let reply = block_on(
            handler.handle(bytes::Bytes::from(
                json!({"tool_call_id":"toolu_s","name":"web_search","arguments":{"query":"rust"}})
                    .to_string(),
            )),
        );
        let text = String::from_utf8(reply.body().to_vec()).expect("utf8");
        assert!(text.contains("found"));
        let caller = seen.lock().expect("seen");
        assert_eq!(caller.key_id, "friend");
        assert_eq!(caller.limits[0].tokens, 700);
    }

    #[test]
    fn d227_client_events_are_server_tool_use() {
        let start: StreamEvent = serde_json::from_value(json!({
            "type": "content_block_start",
            "index": 0,
            "block": {"type": "tool_call", "id": "toolu_1", "name": "advisor"}
        }))
        .expect("start");
        let stop = StreamEvent::ContentBlockStop(ContentBlockStop {
            index: 0,
            extensions: hofund_plugin_api::Extensions::new(),
        });
        let after: StreamEvent = serde_json::from_value(json!({
            "type": "text_delta",
            "index": 1,
            "text": "after"
        }))
        .expect("after");
        let events = splice(
            vec![start, stop, after],
            &[super::AdviceOutcome {
                text: "check the boundary".into(),
                code: String::new(),
            }],
        );
        let names: Vec<_> = events
            .iter()
            .filter_map(|event| match event {
                StreamEvent::Unknown(event) => Some(event.name.as_str()),
                _ => None,
            })
            .collect();
        assert!(names.contains(&"content_block_start"));
        let payload = events
            .iter()
            .find_map(|event| match event {
                StreamEvent::Unknown(event) if event.name == "content_block_start" => {
                    Some(event.payload.to_string())
                }
                _ => None,
            })
            .expect("start");
        assert!(payload.contains("server_tool_use"));
        assert!(payload.contains("advisor_tool_result") || events.iter().any(|event| {
            matches!(event, StreamEvent::Unknown(event) if event.payload.to_string().contains("advisor_tool_result"))
        }));
        assert!(
            events
                .iter()
                .all(|event| { crate::bridge::decode::start_tool_call(event).is_none() })
        );
        match events.last() {
            Some(StreamEvent::TextDelta(delta)) => assert_eq!(delta.index, 2),
            other => panic!("shifted text, got {other:?}"),
        }
    }

    fn advice_text(request: &Request) -> String {
        request
            .messages
            .iter()
            .flat_map(|message| message.content.iter())
            .filter_map(|part| match part {
                hofund_plugin_api::ContentPart::Text(text) => Some(text.text.as_str()),
                _ => None,
            })
            .collect::<Vec<_>>()
            .join("\n")
    }
}
