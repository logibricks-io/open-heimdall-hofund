//! Cursor text and caller-tool upstream backed by one stateless Run per execute.

use std::collections::VecDeque;
use std::path::PathBuf;
use std::sync::{Arc, Mutex};

use hofund_plugin_api::{
    AccountId, BlockStart, Completion, ContentBlockStart, ContentBlockStop, Extensions,
    HostContext, MessageDelta, MessageStart, MessageStop, PluginError, ProbeQuery, ProbeReport,
    ProtocolId, Request, Stop, StopReason, StreamEvent, TextDelta, ToolArgumentsDelta,
    UpstreamProvider,
};
use hofund_plugin_sdk::futures::{self, StreamExt};

use crate::agent_host::AgentHost;
use crate::cli::KeychainCache;
use crate::endpoints::Endpoints;
use crate::failure::{self, EMPTY_REPLY};
use crate::proto::{ExecRequest, ServerEvent, ToolDef};
use crate::run::{Limits, RunEvent, RunStream, run_stream};
use crate::{request, session};

/// Account and process facilities needed by one stateless Cursor Run.
pub(crate) struct CursorUpstream {
    account: AccountId,
    host: HostContext,
    endpoints: Arc<Endpoints>,
    agent_home: PathBuf,
    program: Option<PathBuf>,
    keychain: Arc<Mutex<KeychainCache>>,
    keychain_enabled: bool,
    agent_host: AgentHost,
    limits: Limits,
}

impl CursorUpstream {
    pub(crate) fn new(
        account: AccountId,
        host: HostContext,
        endpoints: Arc<Endpoints>,
        agent_home: PathBuf,
        program: Option<PathBuf>,
        keychain: Arc<Mutex<KeychainCache>>,
        keychain_enabled: bool,
    ) -> Self {
        Self {
            account,
            host,
            endpoints,
            agent_home,
            program,
            keychain,
            keychain_enabled,
            agent_host: AgentHost::default(),
            limits: Limits::default(),
        }
    }
}

#[hofund_plugin_sdk::async_trait]
impl UpstreamProvider for CursorUpstream {
    async fn execute(
        &self,
        request: Request,
        _inbound: ProtocolId,
    ) -> Result<Completion, PluginError> {
        let model = request.model.clone();
        let mapped = request::map(&request)?;
        let tools = mapped.tools.clone();
        let token = session::access_token(
            &self.host,
            &self.endpoints,
            &self.account,
            &self.agent_home,
            self.program.clone(),
            &self.keychain,
            self.keychain_enabled,
        )
        .await?;
        let response_id = hofund_plugin_sdk::uuid::v4()
            .map(|id| format!("msg_{id}"))
            .map_err(|error| PluginError::io(format!("cannot create a response id: {error}")))?;
        let mut args = mapped.into_args();
        args.limits = self.limits;
        let mut retried = false;
        'runs: loop {
            let attempt = run_stream(
                &self.host,
                &self.endpoints,
                &self.agent_host,
                &token,
                args.clone(),
            )
            .await?;
            let http1 = attempt.http1;
            let agent_url = attempt.agent_url;
            let mut run = attempt.stream;

            while let Some(event) = run.next().await {
                let error = match event {
                    Ok(RunEvent::Server(ServerEvent::TextDelta(text))) => {
                        return Ok(completion_stream(
                            run,
                            response_id,
                            model,
                            tools,
                            FirstOutput::Text(text),
                        ));
                    }
                    Ok(RunEvent::Server(ServerEvent::Exec(request)))
                        if is_caller_call(&tools, &request) =>
                    {
                        return Ok(completion_stream(
                            run,
                            response_id,
                            model,
                            tools,
                            FirstOutput::Tool(request),
                        ));
                    }
                    Ok(RunEvent::Server(ServerEvent::TurnEnd(_))) => return Err(empty_reply()),
                    Ok(RunEvent::Server(_)) | Ok(RunEvent::ToolStepEnd) => continue,
                    Ok(RunEvent::Trailer(frame)) => failure::trailer(&frame.payload),
                    Ok(RunEvent::ErrorBody { status, body }) => failure::body(status, &body),
                    Err(error) => failure::timeout_before_text(error),
                };
                if !retried && failure::regional(error.message()) {
                    crate::http2_memory::clear();
                    let fresh = self
                        .agent_host
                        .resolve(&self.host, &self.endpoints, &token, true, args.team_id)
                        .await;
                    if http1 || fresh != agent_url {
                        retried = true;
                        continue 'runs;
                    }
                }
                return Err(error);
            }
            return Err(empty_reply());
        }
    }

    async fn probe(&self, query: &ProbeQuery) -> Result<ProbeReport, PluginError> {
        let completion = self
            .execute(query.minimal_request(), query.protocol())
            .await?;
        ProbeReport::from_completion(completion).await
    }
}

struct CursorStreamState {
    run: RunStream<'static>,
    tools: Vec<ToolDef>,
    pending: VecDeque<StreamEvent>,
    next_index: u32,
    open_text: Option<u32>,
    emitted_calls: usize,
    finished: bool,
}

enum FirstOutput {
    Text(String),
    Tool(ExecRequest),
}

fn is_caller_call(tools: &[ToolDef], request: &ExecRequest) -> bool {
    let crate::proto::ExecKind::McpArguments { name, .. } = &request.kind else {
        return false;
    };
    tools.iter().any(|tool| tool.name == *name)
}

fn completion_stream(
    run: RunStream<'static>,
    id: String,
    model: String,
    tools: Vec<ToolDef>,
    first: FirstOutput,
) -> Completion {
    let pending = VecDeque::from([StreamEvent::MessageStart(MessageStart {
        id: Some(id),
        model: Some(model),
        usage: None,
        extensions: Extensions::new(),
    })]);
    let mut state = CursorStreamState {
        run,
        tools,
        pending,
        next_index: 0,
        open_text: None,
        emitted_calls: 0,
        finished: false,
    };
    match first {
        FirstOutput::Text(text) => state.emit_text(text),
        FirstOutput::Tool(request) => state.emit_tool(request),
    }
    Completion::Stream(Box::pin(futures::unfold(state, |mut state| async move {
        match next_completion_event(&mut state).await {
            Ok(Some(event)) => Some((Ok(event), state)),
            Ok(None) => None,
            Err(error) => {
                state.finished = true;
                Some((Err(error), state))
            }
        }
    })))
}

impl CursorStreamState {
    fn emit_text(&mut self, text: String) {
        let index = *self.open_text.get_or_insert_with(|| {
            let index = self.next_index;
            self.next_index += 1;
            self.pending
                .push_back(StreamEvent::ContentBlockStart(ContentBlockStart {
                    index,
                    block: BlockStart::Text,
                    extensions: Extensions::new(),
                }));
            index
        });
        self.pending.push_back(StreamEvent::TextDelta(TextDelta {
            index,
            text,
            extensions: Extensions::new(),
        }));
    }

    fn emit_tool(&mut self, request: ExecRequest) {
        let crate::proto::ExecKind::McpArguments {
            name,
            call_id,
            arguments,
        } = request.kind
        else {
            return;
        };
        self.close_text();
        let index = self.next_index;
        self.next_index += 1;
        self.emitted_calls += 1;
        self.pending.extend([
            StreamEvent::ContentBlockStart(ContentBlockStart {
                index,
                block: BlockStart::ToolCall {
                    id: call_id.replace('\n', "__"),
                    name,
                },
                extensions: Extensions::new(),
            }),
            StreamEvent::ToolArgumentsDelta(ToolArgumentsDelta {
                index,
                partial_json: arguments.to_string(),
                extensions: Extensions::new(),
            }),
            StreamEvent::ContentBlockStop(ContentBlockStop {
                index,
                extensions: Extensions::new(),
            }),
        ]);
    }

    fn close_text(&mut self) {
        if let Some(index) = self.open_text.take() {
            self.pending
                .push_back(StreamEvent::ContentBlockStop(ContentBlockStop {
                    index,
                    extensions: Extensions::new(),
                }));
        }
    }

    fn finish(&mut self, reason: StopReason) {
        self.pending
            .push_back(StreamEvent::MessageDelta(MessageDelta {
                stop: Some(Stop {
                    reason,
                    sequence: None,
                }),
                usage: None,
                extensions: Extensions::new(),
            }));
        self.close_text();
        self.pending
            .push_back(StreamEvent::MessageStop(MessageStop {
                extensions: Extensions::new(),
            }));
        self.finished = true;
    }
}

async fn next_completion_event(
    state: &mut CursorStreamState,
) -> Result<Option<StreamEvent>, PluginError> {
    loop {
        if let Some(event) = state.pending.pop_front() {
            return Ok(Some(event));
        }
        if state.finished {
            return Ok(None);
        }
        match state.run.next().await {
            Some(Ok(RunEvent::Server(ServerEvent::TextDelta(text)))) => {
                state.emit_text(text);
            }
            Some(Ok(RunEvent::Server(ServerEvent::Exec(request))))
                if is_caller_call(&state.tools, &request) =>
            {
                state.emit_tool(request);
            }
            Some(Ok(RunEvent::ToolStepEnd)) => state.finish(StopReason::ToolUse),
            Some(Ok(RunEvent::Server(ServerEvent::TurnEnd(_)))) => {
                if state.emitted_calls == 0 {
                    state.finish(StopReason::EndTurn);
                }
            }
            Some(Ok(RunEvent::Server(_))) => {}
            Some(Ok(RunEvent::Trailer(frame))) => {
                state.finished = true;
                return Err(failure::trailer(&frame.payload));
            }
            Some(Ok(RunEvent::ErrorBody { status, body })) => {
                state.finished = true;
                return Err(failure::body(status, &body));
            }
            Some(Err(error)) => {
                state.finished = true;
                return Err(failure::after_output(error));
            }
            None => {
                if state.emitted_calls > 0 {
                    state.finish(StopReason::ToolUse);
                } else {
                    state.finished = true;
                }
            }
        }
    }
}

fn empty_reply() -> PluginError {
    PluginError::status_error(502, format!("upstream Run returned {EMPTY_REPLY}"))
}

#[cfg(test)]
mod tests {
    use std::path::PathBuf;
    use std::sync::{Arc, Mutex};
    use std::time::Duration;

    use hofund_kit_subscription::{
        Http1Response, Http1Server, Http2Response, Http2Server, TestHost, advance_time, block_on,
        block_on_paused, yield_times,
    };
    use hofund_plugin_api::{
        AccountId, BlockStart, Completion, ContentBlockStart, ContentBlockStop, Extensions,
        Message, MessageDelta, MessageStop, PluginError, PluginId, PluginKind, ProtocolId, Request,
        Role, SecretString, Stop, StopReason, StreamEvent, TextDelta, ToolArgumentsDelta,
        UpstreamProvider,
    };
    use hofund_plugin_sdk::futures::StreamExt;
    use hofund_plugin_sdk::json::{from_value, json};
    use hofund_plugin_sdk::{Race, race};

    use super::CursorUpstream;
    use crate::cli::KeychainCache;
    use crate::endpoints::Endpoints;
    use crate::frame;
    use crate::http2_memory;
    use crate::proto::{self, Pb};
    use crate::run::Limits;
    use crate::tokens;

    fn pooled_host() -> TestHost {
        TestHost::open_for_with_pooled_http_and_tokio_monotonic(
            PluginKind::Subscription,
            PluginId::from_static("cursor"),
            tempfile::tempdir().expect("tempdir").keep(),
        )
    }

    fn config_reply(agent: &str) -> Http1Response {
        Http1Response::new(200)
            .body(format!(r#"{{"agentUrlConfig":{{"agentUrl":"{agent}"}}}}"#).into_bytes())
    }

    fn interaction(field: u32, inner: Pb) -> Vec<u8> {
        let payload = Pb::new()
            .message(1, Pb::new().message(field, inner))
            .finish();
        frame::encode(0, &payload).expect("interaction frame")
    }

    fn text_frame(text: &str) -> Vec<u8> {
        interaction(1, Pb::new().string(1, text))
    }

    fn thinking_frame(text: &str) -> Vec<u8> {
        interaction(4, Pb::new().string(1, text))
    }

    fn turn_end_frame() -> Vec<u8> {
        interaction(
            14,
            Pb::new()
                .varint(1, 100)
                .varint(2, 20)
                .varint(3, 30)
                .varint(4, 40)
                .varint(5, 5),
        )
    }

    async fn store_account(host: &TestHost) -> AccountId {
        let id = AccountId::from_static("a-00000000000000f3");
        let account = host
            .context()
            .accounts()
            .account(id.clone(), "stream@example.test")
            .expect("account");
        let blob = tokens::blob_oauth(
            &SecretString::from("loopback-access-token".to_owned()),
            &SecretString::from("unused-refresh-token".to_owned()),
            4_000_000_000_000,
            "stream@example.test",
            None,
        );
        host.context()
            .accounts()
            .upsert(account, blob.to_secret())
            .await
            .expect("store");
        id
    }

    fn upstream(
        host: &TestHost,
        api: &Http1Server,
        agent: &str,
        account: AccountId,
    ) -> CursorUpstream {
        let endpoints = Arc::new(Endpoints {
            api: api.base_url().to_owned(),
            agent: agent.to_owned(),
            client_version: "cli-test".to_owned(),
            ..Endpoints::default()
        });
        CursorUpstream::new(
            account,
            host.context().clone(),
            endpoints,
            PathBuf::from("/tmp/unused-cursor-home"),
            None,
            Arc::new(Mutex::new(KeychainCache::default())),
            true,
        )
    }

    fn text_request() -> Request {
        Request::new("auto").with_messages(vec![Message::new(Role::User)])
    }

    fn tool_request() -> Request {
        from_value(json!({
            "model": "auto",
            "messages": [{"role": "user", "content": [{"type": "text", "text": "look up weather"}]}],
            "tools": [{
                "name": "lookup",
                "description": "find a value",
                "input_schema": {"type": "object", "properties": {"query": {"type": "string"}}}
            }]
        }))
        .expect("tool request")
    }

    fn exec_frame(id: u32, exec_id: &str, field: u32, body: Pb) -> Vec<u8> {
        let payload = Pb::new()
            .message(
                2,
                Pb::new()
                    .varint(1, u64::from(id))
                    .string(15, exec_id)
                    .bytes(field, &body.finish()),
            )
            .finish();
        frame::encode(0, &payload).expect("exec frame")
    }

    fn list_tools_frame(id: u32) -> Vec<u8> {
        exec_frame(id, "list-tools", 36, Pb::new())
    }

    fn mcp_frame(id: u32, name: &str, call_id: &str) -> Vec<u8> {
        let arguments = Pb::new()
            .string(1, &format!("tools-{name}"))
            .bytes(
                2,
                &Pb::new()
                    .string(1, "query")
                    .bytes(2, &proto::pb_value(&json!("weather")))
                    .finish(),
            )
            .string(3, call_id)
            .string(5, name);
        exec_frame(id, "call-tool", 11, arguments)
    }

    fn tool_update_frame(field: u32, call_id: &str) -> Vec<u8> {
        interaction(field, Pb::new().string(1, call_id))
    }

    fn tool_count_frame(count: u64) -> Vec<u8> {
        interaction(27, Pb::new().varint(1, count))
    }

    fn checkpoint_frame(pending: usize) -> Vec<u8> {
        let mut state = Pb::new();
        for index in 0..pending {
            state = state.string(4, &format!("call-{index}"));
        }
        let payload = Pb::new().bytes(3, &state.finish()).finish();
        frame::encode(0, &payload).expect("checkpoint frame")
    }

    fn heartbeat_frame() -> Vec<u8> {
        interaction(13, Pb::new())
    }

    async fn client_payloads(server: &Http2Server, minimum: usize) -> Vec<Vec<u8>> {
        for attempt in 0..100 {
            let chunks = server.requests()[0].data().to_vec();
            let bytes = chunks.concat();
            let frames = frame::FrameDecoder::new()
                .push(&bytes)
                .expect("client frames");
            if frames.len() >= minimum {
                return frames.into_iter().map(|frame| frame.payload).collect();
            }
            if attempt == 99 {
                panic!(
                    "recorded at least {minimum} client frames, got {} in {} bytes across {} chunks",
                    frames.len(),
                    bytes.len(),
                    chunks.len()
                );
            }
            yield_times(2).await;
        }
        unreachable!()
    }

    async fn collect(completion: Completion) -> Result<Vec<StreamEvent>, PluginError> {
        let Completion::Stream(mut stream) = completion else {
            panic!("Cursor execute returns a stream");
        };
        let mut events = Vec::new();
        while let Some(event) = stream.next().await {
            events.push(event?);
        }
        Ok(events)
    }

    #[test]
    fn requested_mcp_exec_emits_one_caller_tool_and_answers_tool_discovery() {
        let _guard = crate::http2_memory::test_lock();
        crate::http2_memory::reset_for_tests();
        block_on(async {
            let body = [
                list_tools_frame(7),
                mcp_frame(8, "lookup", "call_one\nfc_9"),
                tool_count_frame(1),
                checkpoint_frame(1),
                heartbeat_frame(),
            ]
            .concat();
            let h2 = Http2Server::start(vec![Http2Response::new(200).data(body).hold_open()]).await;
            let h1 = Http1Server::start(vec![config_reply(h2.base_url())]).await;
            let host = pooled_host();
            let account = store_account(&host).await;
            let upstream = upstream(&host, &h1, h2.base_url(), account);
            let Completion::Stream(mut stream) = upstream
                .execute(tool_request(), ProtocolId::OPENAI_CHAT)
                .await
                .expect("tool execute")
            else {
                panic!("stream");
            };
            let mut events = Vec::new();
            while let Some(event) = stream.next().await {
                events.push(event.expect("tool stream"));
            }
            let [StreamEvent::MessageStart(_), rest @ ..] = events.as_slice() else {
                panic!("message start first: {events:?}");
            };
            assert_eq!(
                rest,
                [
                    StreamEvent::ContentBlockStart(ContentBlockStart {
                        index: 0,
                        block: BlockStart::ToolCall {
                            id: "call_one__fc_9".to_owned(),
                            name: "lookup".to_owned(),
                        },
                        extensions: Extensions::new(),
                    }),
                    StreamEvent::ToolArgumentsDelta(ToolArgumentsDelta {
                        index: 0,
                        partial_json: "{\"query\":\"weather\"}".to_owned(),
                        extensions: Extensions::new(),
                    }),
                    StreamEvent::ContentBlockStop(ContentBlockStop {
                        index: 0,
                        extensions: Extensions::new(),
                    }),
                    StreamEvent::MessageDelta(MessageDelta {
                        stop: Some(Stop {
                            reason: StopReason::ToolUse,
                            sequence: None,
                        }),
                        usage: None,
                        extensions: Extensions::new(),
                    }),
                    StreamEvent::MessageStop(MessageStop {
                        extensions: Extensions::new(),
                    }),
                ]
            );

            drop(stream);
        });
    }

    #[test]
    fn unknown_mcp_exec_is_refused_without_a_caller_tool_or_result_write() {
        let _guard = crate::http2_memory::test_lock();
        crate::http2_memory::reset_for_tests();
        block_on(async {
            let body = [
                mcp_frame(12, "cursor_owned", "call-unknown"),
                text_frame("continued"),
                turn_end_frame(),
            ]
            .concat();
            let h2 = Http2Server::start(vec![Http2Response::new(200).data(body)]).await;
            let h1 = Http1Server::start(vec![config_reply(h2.base_url())]).await;
            let host = pooled_host();
            let account = store_account(&host).await;
            let upstream = upstream(&host, &h1, h2.base_url(), account);
            let Completion::Stream(mut stream) = upstream
                .execute(tool_request(), ProtocolId::OPENAI_CHAT)
                .await
                .expect("text after refusal")
            else {
                panic!("stream");
            };
            let mut events = Vec::new();
            while let Some(event) = stream.next().await {
                events.push(event.expect("text stream"));
            }
            assert!(events.iter().all(|event| !matches!(
                event,
                StreamEvent::ContentBlockStart(ContentBlockStart {
                    block: BlockStart::ToolCall { .. },
                    ..
                }) | StreamEvent::ToolArgumentsDelta(_)
            )));
            assert!(events.iter().any(|event| matches!(
                event,
                StreamEvent::TextDelta(TextDelta { text, .. }) if text == "continued"
            )));

            drop(stream);
        });
    }

    #[test]
    fn execute_waits_for_text_then_streams_the_stop_trio_without_usage() {
        let _guard = crate::http2_memory::test_lock();
        crate::http2_memory::reset_for_tests();
        block_on(async {
            let body = [
                thinking_frame("not emitted"),
                text_frame("hel"),
                text_frame("lo"),
                turn_end_frame(),
            ]
            .concat();
            let h2 = Http2Server::start(vec![Http2Response::new(200).data(body)]).await;
            let h1 = Http1Server::start(vec![config_reply(h2.base_url())]).await;
            let host = pooled_host();
            let account = store_account(&host).await;
            let upstream = upstream(&host, &h1, h2.base_url(), account);
            let mut request = text_request();
            request.params.temperature = Some(0.7);
            request.params.top_p = Some(0.8);
            request.params.max_tokens = Some(123);
            let events = collect(
                upstream
                    .execute(request, ProtocolId::OPENAI_CHAT)
                    .await
                    .expect("execute"),
            )
            .await
            .expect("stream");

            let [StreamEvent::MessageStart(start), rest @ ..] = events.as_slice() else {
                panic!("message start first: {events:?}");
            };
            assert!(start.id.as_ref().is_some_and(|id| !id.is_empty()));
            assert_eq!(start.model.as_deref(), Some("auto"));
            assert!(start.usage.is_none());
            assert_eq!(
                rest,
                [
                    StreamEvent::ContentBlockStart(ContentBlockStart {
                        index: 0,
                        block: BlockStart::Text,
                        extensions: Extensions::new(),
                    }),
                    StreamEvent::TextDelta(TextDelta {
                        index: 0,
                        text: "hel".to_owned(),
                        extensions: Extensions::new(),
                    }),
                    StreamEvent::TextDelta(TextDelta {
                        index: 0,
                        text: "lo".to_owned(),
                        extensions: Extensions::new(),
                    }),
                    StreamEvent::MessageDelta(MessageDelta {
                        stop: Some(Stop {
                            reason: StopReason::EndTurn,
                            sequence: None,
                        }),
                        usage: None,
                        extensions: Extensions::new(),
                    }),
                    StreamEvent::ContentBlockStop(ContentBlockStop {
                        index: 0,
                        extensions: Extensions::new(),
                    }),
                    StreamEvent::MessageStop(MessageStop {
                        extensions: Extensions::new(),
                    }),
                ]
            );
            assert!(
                events
                    .iter()
                    .all(|event| !matches!(event, StreamEvent::ThinkingDelta(_)))
            );
            assert_eq!(h2.requests().len(), 1);
            assert_eq!(h2.requests()[0].path(), "/agent.v1.AgentService/Run");
        });
    }

    #[test]
    fn execute_returns_after_the_first_tool_event_without_waiting_for_eof() {
        let _guard = crate::http2_memory::test_lock();
        crate::http2_memory::reset_for_tests();
        block_on(async {
            let h2 = Http2Server::start(vec![
                Http2Response::new(200)
                    .data(mcp_frame(9, "lookup", "call-early"))
                    .hold_open(),
            ])
            .await;
            let h1 = Http1Server::start(vec![config_reply(h2.base_url())]).await;
            let host = pooled_host();
            let account = store_account(&host).await;
            let upstream = upstream(&host, &h1, h2.base_url(), account);
            let Completion::Stream(mut stream) = upstream
                .execute(tool_request(), ProtocolId::OPENAI_CHAT)
                .await
                .expect("first tool event is enough")
            else {
                panic!("stream");
            };
            let start = stream.next().await.expect("start").expect("start event");
            assert!(matches!(start, StreamEvent::MessageStart(_)));
            let block = stream.next().await.expect("block").expect("block event");
            assert!(matches!(
                block,
                StreamEvent::ContentBlockStart(ContentBlockStart {
                    block: BlockStart::ToolCall { ref id, ref name },
                    ..
                }) if id == "call-early" && name == "lookup"
            ));
            let arguments = stream.next().await.expect("arguments").expect("arguments");
            assert!(matches!(
                arguments,
                StreamEvent::ToolArgumentsDelta(ToolArgumentsDelta { ref partial_json, .. })
                    if partial_json == "{\"query\":\"weather\"}"
            ));
            let stop = stream.next().await.expect("stop").expect("stop event");
            assert!(matches!(stop, StreamEvent::ContentBlockStop(_)));
            assert!(
                client_payloads(&h2, 2)
                    .await
                    .contains(&proto::close_exec(9))
            );
            drop(stream);
        });
    }

    #[test]
    fn execute_returns_after_the_first_delta_without_waiting_for_eof() {
        let _guard = crate::http2_memory::test_lock();
        crate::http2_memory::reset_for_tests();
        block_on(async {
            let h2 = Http2Server::start(vec![
                Http2Response::new(200)
                    .data(text_frame("ready"))
                    .hold_open(),
            ])
            .await;
            let h1 = Http1Server::start(vec![config_reply(h2.base_url())]).await;
            let host = pooled_host();
            let account = store_account(&host).await;
            let upstream = upstream(&host, &h1, h2.base_url(), account);
            let Completion::Stream(mut stream) = upstream
                .execute(text_request(), ProtocolId::OPENAI_CHAT)
                .await
                .expect("first delta is enough")
            else {
                panic!("stream");
            };
            for expected in ["start", "block", "ready"] {
                let event = stream.next().await.expect("initial event").expect("event");
                match (expected, event) {
                    ("start", StreamEvent::MessageStart(_))
                    | ("block", StreamEvent::ContentBlockStart(_)) => {}
                    ("ready", StreamEvent::TextDelta(delta)) => assert_eq!(delta.text, "ready"),
                    (_, other) => panic!("unexpected initial event: {other:?}"),
                }
            }
            for _ in 0..100 {
                if h2.requests().first().is_some_and(|request| {
                    frame::FrameDecoder::new()
                        .push(&request.data().concat())
                        .is_ok_and(|frames| !frames.is_empty())
                }) {
                    break;
                }
                yield_times(2).await;
            }
            let requests = h2.requests();
            assert_eq!(requests.len(), 1);
            assert_eq!(requests[0].path(), "/agent.v1.AgentService/Run");
            let written = requests[0].data().concat();
            let frames = frame::FrameDecoder::new().push(&written).expect("frame");
            assert!(!frames.is_empty(), "complete opening frame");
            let client = proto::fields(&frames[0].payload);
            let run = proto::fields(client[0].as_bytes().expect("run"));
            let model = run
                .iter()
                .find(|field| field.number == 9)
                .and_then(proto::Field::as_message)
                .expect("requested model");
            assert_eq!(model[0].as_string().as_deref(), Some("auto"));
            let mut outbound = written;
            for (name, value) in requests[0].headers() {
                outbound.extend_from_slice(name.as_bytes());
                outbound.extend_from_slice(value.as_bytes());
            }
            let lower = String::from_utf8_lossy(&outbound).to_ascii_lowercase();
            let mut reference_name = String::from("mag");
            reference_name.push_str("pie");
            assert!(!lower.contains("hofund"));
            assert!(!lower.contains(&reference_name));
            drop(stream);
        });
    }

    #[test]
    fn stall_550_seals_one_caller_call_after_two_server_completed_calls() {
        let _guard = crate::http2_memory::test_lock();
        crate::http2_memory::reset_for_tests();
        block_on_paused(async {
            let body = [
                tool_update_frame(7, "server-1"),
                tool_update_frame(3, "server-1"),
                tool_update_frame(7, "server-2"),
                tool_update_frame(3, "server-2"),
                mcp_frame(21, "lookup", "caller-3"),
                tool_count_frame(3),
                checkpoint_frame(1),
                heartbeat_frame(),
                heartbeat_frame(),
            ]
            .concat();
            let h2 = Http2Server::start(vec![Http2Response::new(200).data(body).hold_open()]).await;
            let h1 = Http1Server::start(vec![config_reply(h2.base_url())]).await;
            let host = pooled_host();
            let account = store_account(&host).await;
            let upstream = upstream(&host, &h1, h2.base_url(), account);
            let attempt = async {
                let completion = upstream
                    .execute(tool_request(), ProtocolId::OPENAI_CHAT)
                    .await?;
                collect(completion).await
            };
            let control = async {
                advance_time(Duration::from_secs(241)).await;
                std::future::pending::<()>().await;
            };
            let Race::Left(events) = race(attempt, control).await else {
                unreachable!("control remains pending")
            };
            let events = events.expect("the tool count seals the step before quiet");
            let calls = events
                .iter()
                .filter(|event| {
                    matches!(
                        event,
                        StreamEvent::ContentBlockStart(ContentBlockStart {
                            block: BlockStart::ToolCall { .. },
                            ..
                        })
                    )
                })
                .collect::<Vec<_>>();
            assert_eq!(calls.len(), 1, "server-completed calls are not re-run");
            assert!(events.iter().any(|event| matches!(
                event,
                StreamEvent::MessageDelta(MessageDelta {
                    stop: Some(Stop {
                        reason: StopReason::ToolUse,
                        ..
                    }),
                    ..
                })
            )));
            assert!(matches!(events.last(), Some(StreamEvent::MessageStop(_))));
        });
    }

    #[test]
    fn checkpoint_pending_count_seals_the_tool_step_before_heartbeats() {
        let _guard = crate::http2_memory::test_lock();
        crate::http2_memory::reset_for_tests();
        block_on_paused(async {
            let body = [
                mcp_frame(22, "lookup", "caller-checkpoint"),
                tool_count_frame(3),
                checkpoint_frame(1),
                heartbeat_frame(),
            ]
            .concat();
            let h2 = Http2Server::start(vec![Http2Response::new(200).data(body).hold_open()]).await;
            let h1 = Http1Server::start(vec![config_reply(h2.base_url())]).await;
            let host = pooled_host();
            let account = store_account(&host).await;
            let upstream = upstream(&host, &h1, h2.base_url(), account);
            let attempt = async {
                let completion = upstream
                    .execute(tool_request(), ProtocolId::OPENAI_CHAT)
                    .await?;
                collect(completion).await
            };
            let control = async {
                advance_time(Duration::from_secs(241)).await;
                std::future::pending::<()>().await;
            };
            let Race::Left(events) = race(attempt, control).await else {
                unreachable!("control remains pending")
            };
            let events = events.expect("checkpoint seals the step");
            assert!(events.iter().any(|event| matches!(
                event,
                StreamEvent::MessageDelta(MessageDelta {
                    stop: Some(Stop {
                        reason: StopReason::ToolUse,
                        ..
                    }),
                    ..
                })
            )));
        });
    }

    #[test]
    fn a_turn_without_text_returns_the_empty_reply_error() {
        let _guard = crate::http2_memory::test_lock();
        crate::http2_memory::reset_for_tests();
        block_on(async {
            for response in [
                Http2Response::new(200).data(turn_end_frame()),
                Http2Response::new(200),
            ] {
                let h2 = Http2Server::start(vec![response]).await;
                let h1 = Http1Server::start(vec![config_reply(h2.base_url())]).await;
                let host = pooled_host();
                let account = store_account(&host).await;
                let upstream = upstream(&host, &h1, h2.base_url(), account);
                let error = upstream
                    .execute(text_request(), ProtocolId::OPENAI_CHAT)
                    .await
                    .map(|_| ())
                    .expect_err("an empty successful stream is not allowed");
                assert_eq!(error.status(), Some(502));
                assert!(
                    error
                        .message()
                        .contains("an empty reply (the model answered nothing; try again)")
                );
                assert!(!error.message().contains("the upstream stopped sending"));
            }
        });
    }

    #[test]
    fn images_fail_before_opening_a_tool_bearing_run() {
        let _guard = crate::http2_memory::test_lock();
        crate::http2_memory::reset_for_tests();
        block_on(async {
            let h2 = Http2Server::start(Vec::new()).await;
            let h1 = Http1Server::start(Vec::new()).await;
            let host = pooled_host();
            let account = store_account(&host).await;
            let upstream = upstream(&host, &h1, h2.base_url(), account);
            let request: Request = from_value(json!({
                "model": "auto",
                "messages": [{"role": "user", "content": [
                    {"type": "text", "text": "hi"},
                    {"type": "image", "source": {"type": "url", "url": "https://image.test/a.png"}}
                ]}],
                "tools": [{"name": "lookup", "input_schema": {"type": "object"}}]
            }))
            .expect("request");
            let error = upstream
                .execute(request, ProtocolId::OPENAI_CHAT)
                .await
                .map(|_| ())
                .expect_err("unsupported image");
            assert_eq!(error.status(), Some(404));
            assert!(error.message().contains("images are not served"));
            assert!(!error.message().contains("tools are not served"));
            assert!(!error.cools());
            assert_eq!(h2.requests().len(), 0);
            assert_eq!(h1.requests().len(), 0, "not even agent discovery");
        });
    }

    #[test]
    fn a_non_success_http2_head_fails_execute_instead_of_returning_an_empty_stream() {
        let _guard = crate::http2_memory::test_lock();
        crate::http2_memory::reset_for_tests();
        block_on(async {
            let h2 =
                Http2Server::start(vec![Http2Response::new(503).data(b"unavailable".to_vec())])
                    .await;
            let h1 = Http1Server::start(vec![config_reply(h2.base_url())]).await;
            let host = pooled_host();
            let account = store_account(&host).await;
            let upstream = upstream(&host, &h1, h2.base_url(), account);
            let error = upstream
                .execute(text_request(), ProtocolId::OPENAI_CHAT)
                .await
                .map(|_| ())
                .expect_err("terminal failure before Ok");
            assert_eq!(error.status(), Some(503));
            assert!(error.message().contains("unavailable"));
        });
    }

    #[test]
    fn an_http1_error_end_frame_also_fails_before_ok() {
        let _guard = crate::http2_memory::test_lock();
        crate::http2_memory::reset_for_tests();
        block_on(async {
            let h1 = Http1Server::start(vec![
                Http1Response::new(400).body(b"bad run".to_vec()),
                Http1Response::new(200),
            ])
            .await;
            let host = pooled_host();
            crate::http2_memory::remember(host.context());
            let account = store_account(&host).await;
            let upstream = upstream(&host, &h1, "http://127.0.0.1:9", account);
            let error = upstream
                .execute(text_request(), ProtocolId::OPENAI_CHAT)
                .await
                .map(|_| ())
                .expect_err("synthetic end frame");
            assert_eq!(error.status(), Some(400));
            assert!(error.message().contains("bad run"));
            assert_eq!(h1.requests().len(), 2);
        });
    }

    #[test]
    fn idle_before_text_is_a_stall_without_http2_memory() {
        let _guard = http2_memory::test_lock();
        http2_memory::reset_for_tests();
        block_on_paused(async {
            let h2 = Http2Server::start(vec![Http2Response::new(200).hold_open()]).await;
            let h1 = Http1Server::start(vec![config_reply(h2.base_url())]).await;
            let host = pooled_host();
            let account = store_account(&host).await;
            let mut upstream = upstream(&host, &h1, h2.base_url(), account);
            upstream.limits = Limits {
                idle: Duration::from_secs(2),
                quiet: Duration::from_secs(20),
                heartbeat: Duration::from_secs(60),
                ..Limits::default()
            };
            let attempt = upstream.execute(text_request(), ProtocolId::OPENAI_CHAT);
            let control = async {
                for _ in 0..100 {
                    if h2
                        .requests()
                        .first()
                        .is_some_and(|request| !request.data().is_empty())
                    {
                        break;
                    }
                    yield_times(2).await;
                }
                advance_time(Duration::from_millis(2_001)).await;
                std::future::pending::<()>().await;
            };
            let Race::Left(result) = race(attempt, control).await else {
                unreachable!("control remains pending")
            };
            let error = result.map(|_| ()).expect_err("idle stall");
            assert_eq!(error.status(), Some(502));
            assert!(error.message().contains("idle"));
            assert!(!error.message().contains("an empty reply"));
            assert!(!error.message().contains("the upstream stopped sending"));
            assert!(!http2_memory::blocked(
                host.context(),
                Duration::from_secs(600)
            ));
        });
    }

    #[test]
    fn heartbeat_only_quiet_time_is_a_stall_without_http2_memory() {
        let _guard = http2_memory::test_lock();
        http2_memory::reset_for_tests();
        block_on_paused(async {
            let heartbeat = interaction(13, Pb::new());
            let h2 = Http2Server::start(vec![
                Http2Response::new(200)
                    .data(heartbeat.clone())
                    .data(heartbeat.clone())
                    .data(heartbeat)
                    .data_delay(Duration::from_secs(1))
                    .hold_open(),
            ])
            .await;
            let h1 = Http1Server::start(vec![config_reply(h2.base_url())]).await;
            let host = pooled_host();
            let account = store_account(&host).await;
            let mut upstream = upstream(&host, &h1, h2.base_url(), account);
            upstream.limits = Limits {
                idle: Duration::from_secs(20),
                quiet: Duration::from_secs(3),
                heartbeat: Duration::from_secs(60),
                ..Limits::default()
            };
            let attempt = upstream.execute(text_request(), ProtocolId::OPENAI_CHAT);
            let control = async {
                for _ in 0..100 {
                    if h2
                        .requests()
                        .first()
                        .is_some_and(|request| !request.data().is_empty())
                    {
                        break;
                    }
                    yield_times(2).await;
                }
                for _ in 0..3 {
                    advance_time(Duration::from_secs(1)).await;
                    yield_times(10).await;
                }
                advance_time(Duration::from_millis(100)).await;
                std::future::pending::<()>().await;
            };
            let Race::Left(result) = race(attempt, control).await else {
                unreachable!("control remains pending")
            };
            let error = result.map(|_| ()).expect_err("quiet stall");
            assert_eq!(error.status(), Some(502));
            assert!(error.message().contains("quiet"));
            assert!(error.message().contains("heartbeat"));
            assert!(!error.message().contains("an empty reply"));
            assert!(!error.message().contains("the upstream stopped sending"));
            assert!(!http2_memory::blocked(
                host.context(),
                Duration::from_secs(600)
            ));
        });
    }

    #[test]
    fn heartbeats_do_not_extend_the_tool_count_wait() {
        let _guard = http2_memory::test_lock();
        http2_memory::reset_for_tests();
        block_on_paused(async {
            let body = [
                mcp_frame(31, "lookup", "call-without-count"),
                heartbeat_frame(),
                heartbeat_frame(),
                heartbeat_frame(),
            ]
            .concat();
            let h2 = Http2Server::start(vec![
                Http2Response::new(200)
                    .data(body)
                    .data_delay(Duration::from_secs(1))
                    .hold_open(),
            ])
            .await;
            let h1 = Http1Server::start(vec![config_reply(h2.base_url())]).await;
            let host = pooled_host();
            let account = store_account(&host).await;
            let mut upstream = upstream(&host, &h1, h2.base_url(), account);
            upstream.limits = Limits {
                idle: Duration::from_secs(20),
                quiet: Duration::from_secs(20),
                tool_count_wait: Duration::from_secs(2),
                heartbeat: Duration::from_secs(60),
                ..Limits::default()
            };
            let attempt = async {
                let completion = upstream
                    .execute(tool_request(), ProtocolId::OPENAI_CHAT)
                    .await?;
                collect(completion).await
            };
            let control = async {
                // Wait until the HTTP/2 Run is accepted (response headers
                // are immediate; DATA is delayed 1s). Then expire only the
                // data delay and the 2s tool-count wait. A further 20s jump
                // can also complete idle/quiet and is not needed to prove
                // that heartbeats do not extend the count wait.
                for _ in 0..100 {
                    if !h2.requests().is_empty() {
                        break;
                    }
                    yield_times(2).await;
                }
                advance_time(Duration::from_secs(1)).await;
                yield_times(10).await;
                advance_time(Duration::from_millis(2_001)).await;
                yield_times(10).await;
                std::future::pending::<()>().await;
            };
            let Race::Left(events) = race(attempt, control).await else {
                unreachable!("control remains pending")
            };
            let events = events.expect("count wait ends with ToolUse, not a quiet stall");
            assert!(events.iter().any(|event| matches!(
                event,
                StreamEvent::MessageDelta(MessageDelta {
                    stop: Some(Stop {
                        reason: StopReason::ToolUse,
                        ..
                    }),
                    ..
                })
            )));
        });
    }

    #[test]
    fn idle_after_text_uses_the_after_output_wording() {
        let _guard = http2_memory::test_lock();
        http2_memory::reset_for_tests();
        block_on_paused(async {
            let h2 = Http2Server::start(vec![
                Http2Response::new(200)
                    .data(text_frame("started"))
                    .hold_open(),
            ])
            .await;
            let h1 = Http1Server::start(vec![config_reply(h2.base_url())]).await;
            let host = pooled_host();
            let account = store_account(&host).await;
            let mut upstream = upstream(&host, &h1, h2.base_url(), account);
            upstream.limits = Limits {
                idle: Duration::from_secs(2),
                quiet: Duration::from_secs(20),
                heartbeat: Duration::from_secs(60),
                ..Limits::default()
            };
            let Completion::Stream(mut stream) = upstream
                .execute(text_request(), ProtocolId::OPENAI_CHAT)
                .await
                .expect("text stream")
            else {
                panic!("stream")
            };
            for _ in 0..3 {
                stream.next().await.expect("event").expect("initial event");
            }
            let next = stream.next();
            let control = async {
                advance_time(Duration::from_millis(2_001)).await;
                std::future::pending::<()>().await;
            };
            let Race::Left(event) = race(next, control).await else {
                unreachable!("control remains pending")
            };
            let error = event
                .expect("terminal event")
                .expect_err("after-text stall");
            assert_eq!(error.status(), Some(502));
            assert!(error.message().contains("the upstream stopped sending"));
            assert!(!error.message().contains("an empty reply"));
            assert!(!http2_memory::blocked(
                host.context(),
                Duration::from_secs(600)
            ));
        });
    }

    #[test]
    fn regional_http1_failure_clears_memory_and_retries_once_at_the_fresh_host() {
        let _guard = http2_memory::test_lock();
        http2_memory::reset_for_tests();
        block_on(async {
            let old = Http2Server::start(Vec::new()).await;
            let regional = Http2Server::start(vec![
                Http2Response::new(200)
                    .data([text_frame("regional text"), turn_end_frame()].concat()),
            ])
            .await;
            let h1 = Http1Server::start(vec![
                config_reply(old.base_url()),
                Http1Response::new(401).body(b"account is in another region".to_vec()),
                Http1Response::new(200),
                config_reply(regional.base_url()),
            ])
            .await;
            let host = pooled_host();
            let account = store_account(&host).await;
            let upstream = upstream(&host, &h1, old.base_url(), account);
            let token = SecretString::from("loopback-access-token".to_owned());
            assert_eq!(
                upstream
                    .agent_host
                    .resolve(host.context(), &upstream.endpoints, &token, false, None)
                    .await,
                old.base_url()
            );
            http2_memory::remember(host.context());

            let events = collect(
                upstream
                    .execute(text_request(), ProtocolId::OPENAI_CHAT)
                    .await
                    .expect("one regional retry"),
            )
            .await
            .expect("regional stream");
            assert!(events.iter().any(|event| matches!(
                event,
                StreamEvent::TextDelta(TextDelta { text, .. }) if text == "regional text"
            )));
            assert_eq!(old.requests().len(), 0);
            assert_eq!(regional.requests().len(), 1);
            assert_eq!(h1.requests().len(), 4);
            assert_eq!(h1.requests()[1].path(), "/agent.v1.AgentService/RunSSE");
            assert_eq!(
                h1.requests()[2].path(),
                "/aiserver.v1.BidiService/BidiAppend"
            );
            assert_eq!(
                h1.requests()[3].path(),
                "/aiserver.v1.ServerConfigService/GetServerConfig"
            );
            assert!(!http2_memory::blocked(
                host.context(),
                Duration::from_secs(600)
            ));
        });
    }

    #[test]
    fn regional_http1_failure_retries_http2_even_when_the_agent_url_is_unchanged() {
        let _guard = http2_memory::test_lock();
        http2_memory::reset_for_tests();
        block_on(async {
            let h2 = Http2Server::start(vec![
                Http2Response::new(200)
                    .data([text_frame("same host retry"), turn_end_frame()].concat()),
            ])
            .await;
            let h1 = Http1Server::start(vec![
                config_reply(h2.base_url()),
                Http1Response::new(401).body(b"account is in another region".to_vec()),
                Http1Response::new(200),
                config_reply(h2.base_url()),
            ])
            .await;
            let host = pooled_host();
            let account = store_account(&host).await;
            let upstream = upstream(&host, &h1, h2.base_url(), account);
            let token = SecretString::from("loopback-access-token".to_owned());
            assert_eq!(
                upstream
                    .agent_host
                    .resolve(host.context(), &upstream.endpoints, &token, false, None)
                    .await,
                h2.base_url()
            );
            http2_memory::remember(host.context());

            let events = collect(
                upstream
                    .execute(text_request(), ProtocolId::OPENAI_CHAT)
                    .await
                    .expect("http1 regional retry at the same host"),
            )
            .await
            .expect("retried stream");
            assert!(events.iter().any(|event| matches!(
                event,
                StreamEvent::TextDelta(TextDelta { text, .. }) if text == "same host retry"
            )));
            assert_eq!(h2.requests().len(), 1);
            assert_eq!(h1.requests().len(), 4);
            assert_eq!(
                h1.requests()[3].path(),
                "/aiserver.v1.ServerConfigService/GetServerConfig"
            );
        });
    }

    #[test]
    fn a_second_regional_failure_is_403_without_a_third_attempt() {
        let _guard = http2_memory::test_lock();
        http2_memory::reset_for_tests();
        block_on(async {
            let old = Http2Server::start(Vec::new()).await;
            let regional = Http2Server::start(vec![
                Http2Response::new(401).data(b"still the wrong region".to_vec()),
            ])
            .await;
            let h1 = Http1Server::start(vec![
                config_reply(old.base_url()),
                Http1Response::new(401).body(b"account is in another region".to_vec()),
                Http1Response::new(200),
                config_reply(regional.base_url()),
            ])
            .await;
            let host = pooled_host();
            let account = store_account(&host).await;
            let upstream = upstream(&host, &h1, old.base_url(), account);
            let token = SecretString::from("loopback-access-token".to_owned());
            upstream
                .agent_host
                .resolve(host.context(), &upstream.endpoints, &token, false, None)
                .await;
            http2_memory::remember(host.context());

            let error = upstream
                .execute(text_request(), ProtocolId::OPENAI_CHAT)
                .await
                .map(|_| ())
                .expect_err("second regional failure");
            assert_eq!(error.status(), Some(403));
            assert!(error.message().contains("still the wrong region"));
            assert_eq!(regional.requests().len(), 1);
            assert_eq!(h1.requests().len(), 4);
        });
    }

    #[test]
    fn regional_http2_at_an_unchanged_host_refreshes_without_retrying() {
        let _guard = http2_memory::test_lock();
        http2_memory::reset_for_tests();
        block_on(async {
            let h2 = Http2Server::start(vec![
                Http2Response::new(401).data(b"regional account".to_vec()),
            ])
            .await;
            let h1 = Http1Server::start(vec![
                config_reply(h2.base_url()),
                config_reply(h2.base_url()),
            ])
            .await;
            let host = pooled_host();
            let account = store_account(&host).await;
            let upstream = upstream(&host, &h1, h2.base_url(), account);
            let error = upstream
                .execute(text_request(), ProtocolId::OPENAI_CHAT)
                .await
                .map(|_| ())
                .expect_err("unchanged host is not retried");
            assert_eq!(error.status(), Some(403));
            assert!(error.message().contains("regional account"));
            assert_eq!(h2.requests().len(), 1);
            assert_eq!(h1.requests().len(), 2, "initial and fresh discovery");
        });
    }

    #[test]
    fn a_non_regional_failure_does_not_refresh_or_retry() {
        let _guard = http2_memory::test_lock();
        http2_memory::reset_for_tests();
        block_on(async {
            let h2 = Http2Server::start(Vec::new()).await;
            let h1 = Http1Server::start(vec![
                Http1Response::new(400).body(b"bad run".to_vec()),
                Http1Response::new(200),
            ])
            .await;
            let host = pooled_host();
            http2_memory::remember(host.context());
            let account = store_account(&host).await;
            let upstream = upstream(&host, &h1, h2.base_url(), account);
            let error = upstream
                .execute(text_request(), ProtocolId::OPENAI_CHAT)
                .await
                .map(|_| ())
                .expect_err("ordinary failure");
            assert_eq!(error.status(), Some(400));
            assert_eq!(h1.requests().len(), 2);
            assert_eq!(h2.requests().len(), 0);
        });
    }
}
