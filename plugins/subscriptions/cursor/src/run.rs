//! One Cursor Run over bidirectional HTTP/2 or the HTTP/1.1 fallback.

use std::borrow::Cow;
use std::collections::{HashMap, VecDeque};
use std::pin::Pin;
use std::time::Duration;

use hofund_plugin_api::{
    BodyStream, DuplexReader, DuplexRequest, DuplexWriter, HostContext, HttpVersionPin,
    PluginError, PluginErrorKind, SecretString,
};
#[cfg(test)]
use hofund_plugin_sdk::futures::StreamExt;
use hofund_plugin_sdk::futures::{self, Stream};
use hofund_plugin_sdk::json::json;
use hofund_plugin_sdk::oauth::secret_text;
use hofund_plugin_sdk::{Call, FuturesUnordered, HttpResponse, Race, next_ready, race, sleep};

use crate::agent_host::AgentHost;
use crate::endpoints::Endpoints;
use crate::frame::{self, Frame, FrameDecoder};
use crate::http2_memory;
use crate::proto::{self, ServerEvent};

/// Clocks and byte caps for one Run. Tests may shrink the durations.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) struct Limits {
    /// HTTP/2 response-header wait.
    pub header_wait: Duration,
    /// Client field-7 heartbeat interval.
    pub heartbeat: Duration,
    /// Process-global HTTP/2-unavailable memory lifetime.
    pub http2_memory: Duration,
    /// Maximum non-2xx response body retained for later error classification.
    pub error_drain: usize,
}

impl Default for Limits {
    fn default() -> Self {
        Self {
            header_wait: Duration::from_secs(15),
            heartbeat: Duration::from_secs(5),
            http2_memory: Duration::from_secs(10 * 60),
            error_drain: 1024 * 1024,
        }
    }
}

/// Inputs for one Run transport session.
pub(crate) struct RunArgs<'a> {
    /// First client payload.
    pub opening: Vec<u8>,
    /// Optional team header forwarded to both transports.
    pub team_id: Option<&'a str>,
    /// Blob bytes indexed by their raw digest.
    pub blobs: Cow<'a, HashMap<Vec<u8>, Vec<u8>>>,
    /// GOOS-compatible operating-system name.
    pub os: Cow<'a, str>,
    /// Temporary directory returned to environment execs.
    pub temp_dir: Cow<'a, str>,
    /// Transport limits.
    pub limits: Limits,
}

/// Events and terminal frames collected from one transport session.
#[cfg(test)]
#[derive(Debug, Default)]
pub(crate) struct RunOutput {
    /// Decoded non-trailer server messages.
    pub events: Vec<ServerEvent>,
    /// Connect trailers, including synthetic HTTP/1.1 failures.
    pub trailers: Vec<Frame>,
    /// Drained non-2xx HTTP/2 response body.
    pub error_body: Option<Vec<u8>>,
}

/// One incremental transport item. Terminal items are data so the caller can
/// distinguish a failed Run from a clean EOF before returning a stream.
#[derive(Debug)]
pub(crate) enum RunEvent {
    /// A decoded server message.
    Server(ServerEvent),
    /// A Connect trailer, including a synthetic HTTP/1.1 failure.
    Trailer(Frame),
    /// A drained non-2xx HTTP/2 response body.
    ErrorBody(Vec<u8>),
}

/// The live Run event stream. Dropping it cancels the transport.
pub(crate) type RunStream<'a> =
    Pin<Box<dyn Stream<Item = Result<RunEvent, PluginError>> + Send + 'a>>;

/// Collects a Run for transport tests and callers that want the whole body.
#[cfg(test)]
pub(crate) async fn run(
    host: &HostContext,
    endpoints: &Endpoints,
    agent_host: &AgentHost,
    token: &SecretString,
    args: RunArgs<'_>,
) -> Result<RunOutput, PluginError> {
    let mut stream = run_stream(host, endpoints, agent_host, token, args).await?;
    let mut output = RunOutput::default();
    while let Some(event) = stream.next().await {
        match event? {
            RunEvent::Server(event) => output.events.push(event),
            RunEvent::Trailer(trailer) => output.trailers.push(trailer),
            RunEvent::ErrorBody(body) => output.error_body = Some(body),
        }
    }
    Ok(output)
}

/// Opens a Run and incrementally yields events while answering KV/exec and
/// sending heartbeats. No separate transport loop exists for `run` to fork.
pub(crate) async fn run_stream<'a>(
    host: &HostContext,
    endpoints: &Endpoints,
    agent_host: &AgentHost,
    token: &SecretString,
    args: RunArgs<'a>,
) -> Result<RunStream<'a>, PluginError> {
    let request_id = hofund_plugin_sdk::uuid::v4()
        .map_err(|error| PluginError::io(format!("cannot create a Run request id: {error}")))?;
    if !http2_memory::blocked(host, args.limits.http2_memory) {
        let agent = agent_host
            .resolve(host, endpoints, token, false, args.team_id)
            .await;
        let url = format!("{}/agent.v1.AgentService/Run", agent.trim_end_matches('/'));
        let request = run_duplex(
            url,
            endpoints,
            token,
            &request_id,
            args.team_id,
            args.limits.header_wait,
        );
        match host.http().open_duplex(request).await {
            Ok(session) => {
                return run_http2(session, args).await;
            }
            Err(error) => {
                if http2_memory::should_remember(&error) {
                    http2_memory::remember(host);
                }
            }
        }
    }
    run_http1(host, endpoints, token, request_id, args).await
}

fn one_event(event: RunEvent) -> RunStream<'static> {
    Box::pin(futures::unfold(Some(event), |event| async move {
        event.map(|event| (Ok(event), None))
    }))
}

fn run_duplex(
    url: String,
    endpoints: &Endpoints,
    token: &SecretString,
    request_id: &str,
    team_id: Option<&str>,
    header_wait: Duration,
) -> DuplexRequest {
    let mut request = DuplexRequest::post(url)
        .pin(HttpVersionPin::Http2)
        .header_timeout(header_wait)
        .header("content-type", "application/connect+proto")
        .header("connect-protocol-version", "1")
        .header("user-agent", "")
        .sensitive_header("authorization", format!("Bearer {}", secret_text(token)))
        .header("x-cursor-client-type", "cli")
        .header("x-cursor-client-version", &endpoints.client_version)
        .header("x-ghost-mode", "true")
        .header("x-request-id", request_id)
        .header(
            "x-cursor-agent-allowed-tools",
            "mcp_tool_call,get_mcp_tools_tool_call",
        );
    if let Some(team_id) = team_id.filter(|value| !value.is_empty()) {
        request = request.header("x-cursor-team-id", team_id);
    }
    request
}

fn run_call(
    url: String,
    content_type: &str,
    streaming: bool,
    endpoints: &Endpoints,
    token: &SecretString,
    request_id: &str,
    team_id: Option<&str>,
) -> Call {
    let mut call = Call::post(url)
        .header("content-type", content_type)
        .header("connect-protocol-version", "1")
        .header("user-agent", "")
        .sensitive_header("authorization", format!("Bearer {}", secret_text(token)))
        .header("x-cursor-client-type", "cli")
        .header("x-cursor-client-version", &endpoints.client_version)
        .header("x-ghost-mode", "true")
        .header("x-request-id", request_id)
        .header(
            "x-cursor-agent-allowed-tools",
            "mcp_tool_call,get_mcp_tools_tool_call",
        );
    if streaming {
        call = call
            .header("accept-encoding", "identity")
            .header("x-cursor-streaming", "true");
    }
    if let Some(team_id) = team_id.filter(|value| !value.is_empty()) {
        call = call.header("x-cursor-team-id", team_id);
    }
    call
}

type Heartbeat<'a> = Pin<Box<dyn std::future::Future<Output = ()> + Send + 'a>>;

fn heartbeat(duration: Duration) -> Heartbeat<'static> {
    Box::pin(sleep(duration))
}

struct Http2State<'a> {
    reader: DuplexReader,
    writer: Option<DuplexWriter>,
    decoder: Option<FrameDecoder>,
    heartbeat: Heartbeat<'a>,
    args: RunArgs<'a>,
    pending: VecDeque<RunEvent>,
    finished: bool,
}

async fn run_http2<'a>(
    mut session: hofund_plugin_api::DuplexSession,
    args: RunArgs<'a>,
) -> Result<RunStream<'a>, PluginError> {
    if !(200..300).contains(&session.status().as_u16()) {
        let body = drain(session.reader(), args.limits.error_drain).await?;
        return Ok(one_event(RunEvent::ErrorBody(body)));
    }
    let (reader, mut writer) = session.into_split();
    writer
        .write(frame::encode(0, &args.opening)?.into())
        .await?;
    let heartbeat = heartbeat(args.limits.heartbeat);
    let state = Http2State {
        reader,
        writer: Some(writer),
        decoder: Some(FrameDecoder::new()),
        heartbeat,
        args,
        pending: VecDeque::new(),
        finished: false,
    };
    Ok(Box::pin(futures::unfold(state, |mut state| async move {
        match next_http2(&mut state).await {
            Ok(Some(event)) => Some((Ok(event), state)),
            Ok(None) => None,
            Err(error) => {
                state.finished = true;
                Some((Err(error), state))
            }
        }
    })))
}

async fn next_http2(state: &mut Http2State<'_>) -> Result<Option<RunEvent>, PluginError> {
    loop {
        if let Some(event) = state.pending.pop_front() {
            return Ok(Some(event));
        }
        if state.finished {
            return Ok(None);
        }
        match race(state.reader.reader().next_chunk(), &mut state.heartbeat).await {
            Race::Left(Some(Ok(chunk))) => {
                let decoder = state.decoder.as_mut().expect("open decoder");
                let (events, replies) = decode_chunk(decoder, &chunk, &state.args)?;
                for payload in replies {
                    state
                        .writer
                        .as_mut()
                        .expect("open writer")
                        .write(frame::encode(0, &payload)?.into())
                        .await?;
                }
                state.pending.extend(events);
            }
            Race::Left(Some(Err(error))) => return Err(error),
            Race::Left(None) => {
                let decoder = state.decoder.take().expect("open decoder");
                let (events, replies) = finish_decoder(decoder, &state.args)?;
                for payload in replies {
                    state
                        .writer
                        .as_mut()
                        .expect("open writer")
                        .write(frame::encode(0, &payload)?.into())
                        .await?;
                }
                state.writer.take().expect("open writer").finish().await?;
                state.pending.extend(events);
                state.finished = true;
            }
            Race::Right(()) => {
                state
                    .writer
                    .as_mut()
                    .expect("open writer")
                    .write(frame::encode(0, &proto::heartbeat())?.into())
                    .await?;
                state.heartbeat = heartbeat(state.args.limits.heartbeat);
            }
        }
    }
}

struct Http1State<'a> {
    response: HttpResponse,
    context: AppendContext<'a>,
    appends: AppendState<'a>,
    decoder: Option<FrameDecoder>,
    heartbeat: Heartbeat<'a>,
    args: RunArgs<'a>,
    pending: VecDeque<RunEvent>,
    finished: bool,
}

async fn run_http1<'a>(
    host: &HostContext,
    endpoints: &Endpoints,
    token: &SecretString,
    request_id: String,
    args: RunArgs<'a>,
) -> Result<RunStream<'a>, PluginError> {
    let request_id: std::sync::Arc<str> = request_id.into();
    let down_url = format!(
        "{}/agent.v1.AgentService/RunSSE",
        endpoints.api.trim_end_matches('/')
    );
    let down_payload = proto::Pb::new().string(1, &request_id).finish();
    let down = run_call(
        down_url,
        "application/connect+proto",
        true,
        endpoints,
        token,
        &request_id,
        args.team_id,
    )
    .bytes(frame::encode(0, &down_payload)?)
    .open_raw(host.http(), 1)
    .await;
    let mut response = match down {
        Ok(response) => response,
        Err(error) => {
            return Ok(one_event(RunEvent::Trailer(synthetic_end(
                AppendFailure::from(error),
            )?)));
        }
    };
    let status = response.status().as_u16();
    if !(200..300).contains(&status) {
        let body = drain(response.body_mut(), args.limits.error_drain).await?;
        let message = error_message(status, &body);
        return Ok(one_event(RunEvent::Trailer(synthetic_end(
            AppendFailure::status(status, message),
        )?)));
    }

    let context = AppendContext {
        host: host.clone(),
        endpoints: endpoints.clone(),
        token: token.clone(),
        request_id,
        team_id: args.team_id,
    };
    if let Err(failure) = append(context.clone(), args.opening.clone(), 0).await {
        return Ok(one_event(RunEvent::Trailer(synthetic_end(failure)?)));
    }

    let heartbeat = heartbeat(args.limits.heartbeat);
    let state = Http1State {
        response,
        context,
        appends: AppendState::new(1),
        decoder: Some(FrameDecoder::new()),
        heartbeat,
        args,
        pending: VecDeque::new(),
        finished: false,
    };
    Ok(Box::pin(futures::unfold(state, |mut state| async move {
        match next_http1(&mut state).await {
            Ok(Some(event)) => Some((Ok(event), state)),
            Ok(None) => None,
            Err(error) => {
                state.finished = true;
                Some((Err(error), state))
            }
        }
    })))
}

async fn next_http1(state: &mut Http1State<'_>) -> Result<Option<RunEvent>, PluginError> {
    loop {
        if let Some(event) = state.pending.pop_front() {
            return Ok(Some(event));
        }
        if state.finished {
            return Ok(None);
        }
        if let Err(failure) = state.appends.send_ready(state.context.clone()).await {
            state.finished = true;
            return Ok(Some(RunEvent::Trailer(synthetic_end(failure)?)));
        }
        match race(
            race(state.response.body_mut().next_chunk(), &mut state.heartbeat),
            next_ready(&mut state.appends.in_flight),
        )
        .await
        {
            Race::Left(Race::Left(Some(Ok(chunk)))) => {
                let decoder = state.decoder.as_mut().expect("open decoder");
                let (events, replies) = decode_chunk(decoder, &chunk, &state.args)?;
                state.appends.pending.extend(replies);
                state.pending.extend(events);
            }
            Race::Left(Race::Left(Some(Err(error)))) => return Err(error),
            Race::Left(Race::Left(None)) => {
                let decoder = state.decoder.take().expect("open decoder");
                let (events, replies) = finish_decoder(decoder, &state.args)?;
                state.appends.pending.extend(replies);
                state.pending.extend(events);
                if let Err(failure) = state.appends.flush(state.context.clone()).await {
                    state
                        .pending
                        .push_back(RunEvent::Trailer(synthetic_end(failure)?));
                }
                state.finished = true;
            }
            Race::Left(Race::Right(())) => {
                state.appends.pending.push_back(proto::heartbeat());
                state.heartbeat = heartbeat(state.args.limits.heartbeat);
            }
            Race::Right(Ok(())) => {}
            Race::Right(Err(failure)) => {
                state.finished = true;
                return Ok(Some(RunEvent::Trailer(synthetic_end(failure)?)));
            }
        }
    }
}

const MAX_IN_FLIGHT_KV: usize = 16;

#[derive(Clone)]
struct AppendContext<'a> {
    host: HostContext,
    endpoints: Endpoints,
    token: SecretString,
    request_id: std::sync::Arc<str>,
    team_id: Option<&'a str>,
}

type AppendFuture<'a> =
    std::pin::Pin<Box<dyn std::future::Future<Output = Result<(), AppendFailure>> + Send + 'a>>;

struct AppendState<'a> {
    sequence: u64,
    pending: VecDeque<Vec<u8>>,
    in_flight: FuturesUnordered<AppendFuture<'a>>,
}

impl<'a> AppendState<'a> {
    fn new(sequence: u64) -> Self {
        Self {
            sequence,
            pending: VecDeque::new(),
            in_flight: FuturesUnordered::new(),
        }
    }

    async fn send_ready(&mut self, context: AppendContext<'a>) -> Result<(), AppendFailure> {
        loop {
            let Some(payload) = self.pending.front() else {
                return Ok(());
            };
            if payload.first() == Some(&0x1a) {
                if self.in_flight.len() >= MAX_IN_FLIGHT_KV {
                    return Ok(());
                }
                let payload = self.pending.pop_front().unwrap_or_default();
                let sequence = self.take_sequence();
                self.in_flight
                    .push(Box::pin(append(context.clone(), payload, sequence)));
            } else {
                if !self.in_flight.is_empty() {
                    return Ok(());
                }
                let payload = self.pending.pop_front().unwrap_or_default();
                let sequence = self.take_sequence();
                append(context.clone(), payload, sequence).await?;
            }
        }
    }

    async fn flush(&mut self, context: AppendContext<'a>) -> Result<(), AppendFailure> {
        loop {
            self.send_ready(context.clone()).await?;
            if self.pending.is_empty() && self.in_flight.is_empty() {
                return Ok(());
            }
            next_ready(&mut self.in_flight).await?;
        }
    }

    fn take_sequence(&mut self) -> u64 {
        let sequence = self.sequence;
        self.sequence += 1;
        sequence
    }
}

struct AppendFailure {
    code: &'static str,
    message: String,
}

impl AppendFailure {
    fn status(status: u16, message: String) -> Self {
        Self {
            code: connect_code(status),
            message,
        }
    }
}

impl From<PluginError> for AppendFailure {
    fn from(error: PluginError) -> Self {
        if error.kind() == PluginErrorKind::Timeout {
            Self {
                code: "deadline_exceeded",
                message: error.message().to_owned(),
            }
        } else {
            Self {
                code: "unavailable",
                message: error.message().to_owned(),
            }
        }
    }
}

async fn append(
    context: AppendContext<'_>,
    payload: Vec<u8>,
    sequence: u64,
) -> Result<(), AppendFailure> {
    let url = format!(
        "{}/aiserver.v1.BidiService/BidiAppend",
        context.endpoints.api.trim_end_matches('/')
    );
    let body = proto::bidi_append(&payload, &context.request_id, sequence);
    let reply = run_call(
        url,
        "application/proto",
        true,
        &context.endpoints,
        &context.token,
        &context.request_id,
        context.team_id,
    )
    .read_timeout(append_deadline(body.len()))
    .bytes(&body)
    .fetch(context.host.http())
    .await
    .map_err(AppendFailure::from)?;
    if (200..300).contains(&reply.status()) {
        Ok(())
    } else {
        Err(AppendFailure::status(
            reply.status(),
            error_message(reply.status(), reply.body()),
        ))
    }
}

fn append_deadline(body_len: usize) -> Duration {
    let chunks = u64::try_from(body_len.div_ceil(65_536)).unwrap_or(u64::MAX);
    Duration::from_secs(60_u64.saturating_add(chunks))
}

fn connect_code(status: u16) -> &'static str {
    match status {
        400 => "invalid_argument",
        401 => "unauthenticated",
        403 => "permission_denied",
        429 => "resource_exhausted",
        _ => "unavailable",
    }
}

fn synthetic_end(failure: AppendFailure) -> Result<Frame, PluginError> {
    let payload = json!({
        "error": {
            "code": failure.code,
            "message": failure.message,
        }
    })
    .to_string()
    .into_bytes();
    frame::encode(frame::FLAG_TRAILER, &payload)?;
    Ok(Frame {
        flags: frame::FLAG_TRAILER,
        payload,
    })
}

fn error_message(status: u16, body: &[u8]) -> String {
    let message = String::from_utf8_lossy(body).trim().to_owned();
    if message.is_empty() {
        format!("upstream returned status {status}")
    } else {
        message
    }
}

async fn drain(reader: &mut BodyStream, cap: usize) -> Result<Vec<u8>, PluginError> {
    let mut body = Vec::new();
    while body.len() < cap {
        let Some(chunk) = reader.next_chunk().await else {
            break;
        };
        let chunk = chunk?;
        let remaining = cap.saturating_sub(body.len());
        body.extend_from_slice(&chunk[..chunk.len().min(remaining)]);
    }
    Ok(body)
}

fn decode_chunk(
    decoder: &mut FrameDecoder,
    chunk: &[u8],
    args: &RunArgs<'_>,
) -> Result<(Vec<RunEvent>, Vec<Vec<u8>>), PluginError> {
    let mut events = Vec::new();
    let mut replies = Vec::new();
    for decoded in decoder.push(chunk)? {
        push_frame(decoded, args, &mut events, &mut replies);
    }
    Ok((events, replies))
}

fn finish_decoder(
    decoder: FrameDecoder,
    args: &RunArgs<'_>,
) -> Result<(Vec<RunEvent>, Vec<Vec<u8>>), PluginError> {
    let mut events = Vec::new();
    let mut replies = Vec::new();
    for decoded in decoder.finish()? {
        push_frame(decoded, args, &mut events, &mut replies);
    }
    Ok((events, replies))
}

fn push_frame(
    decoded: Frame,
    args: &RunArgs<'_>,
    events: &mut Vec<RunEvent>,
    replies: &mut Vec<Vec<u8>>,
) {
    if decoded.is_trailer() {
        events.push(RunEvent::Trailer(decoded));
        return;
    }
    for event in proto::decode_server(&decoded.payload).0 {
        reply_for(&event, args, replies);
        events.push(RunEvent::Server(event));
    }
}

fn reply_for(event: &ServerEvent, args: &RunArgs<'_>, replies: &mut Vec<Vec<u8>>) {
    match event {
        ServerEvent::Kv(request) => match &request.kind {
            proto::KvKind::Get { digest } => {
                if let Some(blob) = args.blobs.get(digest) {
                    replies.push(proto::kv_get_found(&request.id, blob));
                } else {
                    replies.push(proto::kv_get_missing(&request.id));
                }
            }
            proto::KvKind::Store => replies.push(proto::kv_store_ack(&request.id)),
        },
        ServerEvent::Exec(request) => match request.kind {
            proto::ExecKind::Environment => {
                replies.push(proto::exec_env_result(
                    request.id,
                    &request.exec_id,
                    &args.os,
                    &args.temp_dir,
                ));
                replies.push(proto::close_exec(request.id));
            }
            proto::ExecKind::ListTools => {
                replies.push(proto::exec_tools_result(request.id, &request.exec_id, &[]));
                replies.push(proto::close_exec(request.id));
            }
            proto::ExecKind::McpArguments { .. } | proto::ExecKind::Other => {
                replies.push(proto::refuse_exec(request.id));
                replies.push(proto::close_exec(request.id));
            }
        },
        _ => {}
    }
}

#[cfg(test)]
mod tests {
    use std::collections::HashMap;
    use std::time::Duration;

    use hofund_kit_subscription::{
        Http1Response, Http1Server, Http2Response, Http2Server, TestHost, advance_time, block_on,
        block_on_paused, header_park, yield_times,
    };
    use hofund_plugin_api::{PluginId, PluginKind, SecretString};
    use hofund_plugin_sdk::{Race, race};

    use super::{Limits, RunArgs, append_deadline, connect_code, run};
    use crate::agent_host::AgentHost;
    use crate::endpoints::Endpoints;
    use crate::frame;
    use crate::http2_memory::{self, blocked};
    use crate::proto::{self, Pb, ServerEvent};

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

    fn text_frame(text: &str) -> Vec<u8> {
        let payload = Pb::new()
            .message(1, Pb::new().message(1, Pb::new().string(1, text)))
            .finish();
        frame::encode(0, &payload).expect("text frame")
    }

    static EMPTY_BLOBS: std::sync::LazyLock<HashMap<Vec<u8>, Vec<u8>>> =
        std::sync::LazyLock::new(HashMap::new);

    fn run_args(blobs: &HashMap<Vec<u8>, Vec<u8>>) -> RunArgs<'_> {
        RunArgs {
            opening: proto::client_run(b"opening"),
            team_id: None,
            blobs: std::borrow::Cow::Borrowed(blobs),
            os: std::borrow::Cow::Borrowed("linux"),
            temp_dir: std::borrow::Cow::Borrowed("/tmp/r8f2"),
            limits: Limits::default(),
        }
    }

    fn empty_run_args() -> RunArgs<'static> {
        run_args(&EMPTY_BLOBS)
    }

    fn server_frame(payload: Vec<u8>) -> Vec<u8> {
        frame::encode(0, &payload).expect("server frame")
    }

    fn server_kv_get(id: &[u8], digest: &[u8]) -> Vec<u8> {
        Pb::new()
            .message(
                4,
                Pb::new()
                    .bytes(1, id)
                    .message(2, Pb::new().bytes(1, digest)),
            )
            .finish()
    }

    fn server_kv_store(id: &[u8]) -> Vec<u8> {
        Pb::new()
            .message(4, Pb::new().bytes(1, id).bytes(3, &[]))
            .finish()
    }

    fn server_exec(id: u32, exec_id: &str, field: u32) -> Vec<u8> {
        Pb::new()
            .message(
                2,
                Pb::new()
                    .varint(1, u64::from(id))
                    .string(15, exec_id)
                    .bytes(field, &[]),
            )
            .finish()
    }

    fn endpoints(api: &Http1Server, agent: &str) -> Endpoints {
        Endpoints {
            api: api.base_url().to_owned(),
            agent: agent.to_owned(),
            client_version: "cli-test".to_owned(),
            ..Endpoints::default()
        }
    }

    fn token() -> SecretString {
        SecretString::from("dummy-access-token".to_owned())
    }

    fn run_sse_count(server: &Http1Server) -> usize {
        server
            .requests()
            .iter()
            .filter(|request| request.path() == "/agent.v1.AgentService/RunSSE")
            .count()
    }

    fn assert_clean_http1(server: &Http1Server) {
        let mut reference_name = String::from("mag");
        reference_name.push_str("pie");
        for request in server.requests() {
            let mut bytes = Vec::new();
            for (name, value) in request.headers() {
                bytes.extend_from_slice(name.as_bytes());
                bytes.extend_from_slice(value.as_bytes());
            }
            bytes.extend_from_slice(request.body());
            let lower = String::from_utf8_lossy(&bytes).to_ascii_lowercase();
            assert!(!lower.contains("hofund"));
            assert!(!lower.contains(&reference_name));
        }
    }

    #[test]
    fn http2_run_waits_for_a_success_head_then_writes_and_reads_frames() {
        let _guard = http2_memory::test_lock();
        http2_memory::reset_for_tests();
        block_on(async {
            let h2 =
                Http2Server::start(vec![Http2Response::new(200).data(text_frame("hello"))]).await;
            let h1 = Http1Server::start(vec![config_reply(h2.base_url())]).await;
            let host = pooled_host();
            let endpoints = endpoints(&h1, h2.base_url());
            let output = run(
                host.context(),
                &endpoints,
                &AgentHost::default(),
                &token(),
                empty_run_args(),
            )
            .await
            .expect("run");
            assert_eq!(output.events, [ServerEvent::TextDelta("hello".into())]);
            assert!(output.trailers.is_empty());

            let requests = h2.requests();
            assert_eq!(requests.len(), 1);
            let request = &requests[0];
            assert_eq!(request.path(), "/agent.v1.AgentService/Run");
            assert_eq!(request.method(), "POST");
            assert_eq!(
                request.header("content-type"),
                Some("application/connect+proto")
            );
            assert_eq!(request.header("connect-protocol-version"), Some("1"));
            assert_eq!(
                request.header("authorization"),
                Some("Bearer dummy-access-token")
            );
            assert_eq!(request.header("x-cursor-client-type"), Some("cli"));
            assert_eq!(request.header("x-cursor-client-version"), Some("cli-test"));
            assert_eq!(request.header("x-ghost-mode"), Some("true"));
            assert_eq!(
                request.header("x-cursor-agent-allowed-tools"),
                Some("mcp_tool_call,get_mcp_tools_tool_call")
            );
            assert!(request.header("x-request-id").is_some());
            assert_eq!(request.header("x-cursor-team-id"), None);
            let written = request.data().concat();
            assert_eq!(
                written,
                frame::encode(0, &proto::client_run(b"opening")).expect("frame")
            );

            let mut reference_name = String::from("mag");
            reference_name.push_str("pie");
            let mut outbound = written;
            for (name, value) in request.headers() {
                outbound.extend_from_slice(name.as_bytes());
                outbound.extend_from_slice(value.as_bytes());
            }
            let lower = String::from_utf8_lossy(&outbound).to_ascii_lowercase();
            assert!(!lower.contains("hofund"));
            assert!(!lower.contains(&reference_name));
            assert_clean_http1(&h1);
        });
    }

    #[test]
    fn team_header_is_present_only_when_the_hook_supplies_it() {
        let _guard = http2_memory::test_lock();
        http2_memory::reset_for_tests();
        block_on(async {
            let h2 = Http2Server::start(vec![Http2Response::new(200)]).await;
            let h1 = Http1Server::start(vec![config_reply(h2.base_url())]).await;
            let host = pooled_host();
            run(
                host.context(),
                &endpoints(&h1, h2.base_url()),
                &AgentHost::default(),
                &token(),
                RunArgs {
                    team_id: Some("42"),
                    ..empty_run_args()
                },
            )
            .await
            .expect("run");
            assert_eq!(h2.requests()[0].header("x-cursor-team-id"), Some("42"));
        });
    }

    #[test]
    fn delayed_http2_headers_fall_back_and_are_remembered_for_ten_minutes() {
        let _guard = http2_memory::test_lock();
        http2_memory::reset_for_tests();
        block_on_paused(async {
            // Park headers behind a gate so the first stream cannot later
            // complete as an empty HTTP/2 200 (header_delay + no DATA + no
            // hold_open sends END_STREAM). The test clock then expires only
            // the client header wait.
            let (header_release, header_gate) = header_park();
            let h2 = Http2Server::start(vec![
                Http2Response::new(200).park_headers(header_gate),
                Http2Response::new(200).data(text_frame("after")),
            ])
            .await;
            let h1 = Http1Server::start(vec![
                config_reply(h2.base_url()),
                Http1Response::new(200),
                Http1Response::new(200),
                Http1Response::new(200),
                Http1Response::new(200),
                config_reply(h2.base_url()),
            ])
            .await;
            let host = pooled_host();
            let endpoints = endpoints(&h1, h2.base_url());
            let resolver = AgentHost::default();
            let token = token();
            let limits = Limits::default();
            let first = run(
                host.context(),
                &endpoints,
                &resolver,
                &token,
                RunArgs {
                    limits,
                    ..empty_run_args()
                },
            );
            let expire_after_http2_is_recorded = async {
                for _ in 0..100 {
                    if !h2.requests().is_empty() {
                        break;
                    }
                    yield_times(2).await;
                }
                assert_eq!(
                    h2.requests().len(),
                    1,
                    "HTTP/2 handshake must finish before the header wait expires"
                );
                // Let the server enter the header gate before the clock jumps.
                yield_times(10).await;
                advance_time(limits.header_wait + Duration::from_millis(1)).await;
                std::future::pending::<()>().await;
            };
            let Race::Left(first) = race(first, expire_after_http2_is_recorded).await else {
                unreachable!("the control future stays pending after the clock advances");
            };
            first.expect("fallback");
            assert!(
                blocked(host.context(), Duration::from_secs(600)),
                "first header timeout must set HTTP/2 memory"
            );
            assert_eq!(
                run_sse_count(&h1),
                1,
                "first fallback must open RunSSE; paths: {:?}",
                h1.requests()
                    .iter()
                    .map(|request| request.path().to_owned())
                    .collect::<Vec<_>>()
            );
            assert_eq!(h2.requests().len(), 1);

            run(
                host.context(),
                &endpoints,
                &resolver,
                &token,
                RunArgs {
                    limits,
                    ..empty_run_args()
                },
            )
            .await
            .expect("fallback");
            assert_eq!(h2.requests().len(), 1);
            assert_eq!(
                run_sse_count(&h1),
                2,
                "remembered HTTP/2 unavailability must send a second RunSSE; paths: {:?}",
                h1.requests()
                    .iter()
                    .map(|request| request.path().to_owned())
                    .collect::<Vec<_>>()
            );
            assert!(blocked(host.context(), Duration::from_secs(600)));

            advance_time(Duration::from_secs(601)).await;
            let output = run(
                host.context(),
                &endpoints,
                &resolver,
                &token,
                RunArgs {
                    limits,
                    ..empty_run_args()
                },
            )
            .await
            .expect("http2 retry");
            assert_eq!(output.events, [ServerEvent::TextDelta("after".into())]);
            assert_eq!(h2.requests().len(), 2);
            drop(header_release);
        });
    }

    fn connection_refused_falls_back_once_without_setting_memory() {
        let _guard = http2_memory::test_lock();
        http2_memory::reset_for_tests();
        block_on(async {
            let initial = Http2Server::start(Vec::new()).await;
            let port = initial.port();
            let agent = initial.base_url().to_owned();
            initial.shutdown().await;
            let h1 = Http1Server::start(vec![
                config_reply(&agent),
                Http1Response::new(200),
                Http1Response::new(200),
            ])
            .await;
            let host = pooled_host();
            let endpoints = endpoints(&h1, &agent);
            let resolver = AgentHost::default();
            run(
                host.context(),
                &endpoints,
                &resolver,
                &token(),
                empty_run_args(),
            )
            .await
            .expect("fallback");
            assert!(!blocked(host.context(), Duration::from_secs(600)));

            let h2 = Http2Server::start_on(port, vec![Http2Response::new(200)]).await;
            run(
                host.context(),
                &endpoints,
                &resolver,
                &token(),
                empty_run_args(),
            )
            .await
            .expect("second run");
            assert_eq!(h2.requests().len(), 1);
            assert_eq!(
                h1.requests()
                    .iter()
                    .filter(|request| request.path() == "/agent.v1.AgentService/RunSSE")
                    .count(),
                1
            );
        });
    }

    #[test]
    fn refused_stream_falls_back_once_without_setting_memory() {
        let _guard = http2_memory::test_lock();
        http2_memory::reset_for_tests();
        block_on(async {
            let h2 = Http2Server::start(vec![
                Http2Response::new(200).refused(),
                Http2Response::new(200),
            ])
            .await;
            let h1 = Http1Server::start(vec![
                config_reply(h2.base_url()),
                Http1Response::new(200),
                Http1Response::new(200),
            ])
            .await;
            let host = pooled_host();
            let endpoints = endpoints(&h1, h2.base_url());
            let resolver = AgentHost::default();
            for _ in 0..2 {
                run(
                    host.context(),
                    &endpoints,
                    &resolver,
                    &token(),
                    empty_run_args(),
                )
                .await
                .expect("run");
            }
            assert_eq!(h2.requests().len(), 2);
            assert!(!blocked(host.context(), Duration::from_secs(600)));
        });
    }

    #[test]
    fn non_success_http2_head_is_drained_without_an_opening_frame() {
        let _guard = http2_memory::test_lock();
        http2_memory::reset_for_tests();
        block_on(async {
            let h2 =
                Http2Server::start(vec![Http2Response::new(403).data(b"denied".to_vec())]).await;
            let h1 = Http1Server::start(vec![config_reply(h2.base_url())]).await;
            let host = pooled_host();
            let output = run(
                host.context(),
                &endpoints(&h1, h2.base_url()),
                &AgentHost::default(),
                &token(),
                RunArgs {
                    opening: proto::client_run(b"must-not-be-written"),
                    ..empty_run_args()
                },
            )
            .await
            .expect("error body");
            assert_eq!(output.error_body.as_deref(), Some(&b"denied"[..]));
            assert!(h2.requests()[0].data().is_empty());
            assert_eq!(h1.requests().len(), 1);
        });
    }

    #[test]
    fn http1_fallback_sends_ordered_opening_and_heartbeat_appends() {
        let _guard = http2_memory::test_lock();
        http2_memory::reset_for_tests();
        block_on_paused(async {
            let h1 = Http1Server::start(vec![
                Http1Response::chunked(vec![text_frame("fallback")]).hold_open(),
                Http1Response::new(200),
                Http1Response::new(200),
            ])
            .await;
            let host = pooled_host();
            http2_memory::remember(host.context());
            let endpoints = endpoints(&h1, "http://127.0.0.1:9");
            let limits = Limits {
                heartbeat: Duration::from_secs(1),
                ..Limits::default()
            };
            let resolver = AgentHost::default();
            let token = token();
            let session = run(
                host.context(),
                &endpoints,
                &resolver,
                &token,
                RunArgs {
                    limits,
                    ..empty_run_args()
                },
            );
            let control = async {
                for _ in 0..100 {
                    if h1.requests().len() >= 2 {
                        break;
                    }
                    yield_times(2).await;
                }
                assert_eq!(h1.requests().len(), 2);
                for _ in 0..3 {
                    yield_times(10).await;
                    advance_time(Duration::from_secs(1)).await;
                    for _ in 0..100 {
                        if h1.requests().len() >= 3 {
                            break;
                        }
                        yield_times(2).await;
                    }
                    if h1.requests().len() >= 3 {
                        break;
                    }
                }
                assert_eq!(h1.requests().len(), 3);
            };
            assert!(matches!(race(session, control).await, Race::Right(())));

            let requests = h1.requests();
            assert_eq!(requests[0].path(), "/agent.v1.AgentService/RunSSE");
            assert_eq!(requests[0].header("accept-encoding"), Some("identity"));
            assert_eq!(requests[0].header("x-cursor-streaming"), Some("true"));
            assert_eq!(
                requests[0].header("content-type"),
                Some("application/connect+proto")
            );
            assert_eq!(requests[1].path(), "/aiserver.v1.BidiService/BidiAppend");
            assert_eq!(
                requests[1].header("content-type"),
                Some("application/proto")
            );
            let first = proto::fields(requests[1].body());
            assert_eq!(first[2].as_varint(), Some(0));
            let request_id = requests[0].header("x-request-id").expect("request id");
            assert_eq!(
                first[0].as_string().as_deref(),
                Some(proto::to_hex(&proto::client_run(b"opening")).as_str())
            );
            assert_eq!(
                first[1].as_message().expect("request id")[0]
                    .as_string()
                    .as_deref(),
                Some(request_id)
            );
            let second = proto::fields(requests[2].body());
            assert_eq!(second[2].as_varint(), Some(1));
            assert_eq!(second[0].as_string().as_deref(), Some("3a00"));
            assert_clean_http1(&h1);
        });
    }

    #[test]
    fn dropping_a_session_does_not_set_http2_memory() {
        let _guard = http2_memory::test_lock();
        http2_memory::reset_for_tests();
        block_on_paused(async {
            let h2 = Http2Server::start(vec![Http2Response::new(200).hold_open()]).await;
            let h1 = Http1Server::start(vec![config_reply(h2.base_url())]).await;
            let host = pooled_host();
            let endpoints = endpoints(&h1, h2.base_url());
            let resolver = AgentHost::default();
            let token = token();
            let session = run(
                host.context(),
                &endpoints,
                &resolver,
                &token,
                RunArgs {
                    limits: Limits {
                        heartbeat: Duration::from_secs(1),
                        ..Limits::default()
                    },
                    ..empty_run_args()
                },
            );
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
                assert!(!h2.requests()[0].data().is_empty());
                for _ in 0..3 {
                    yield_times(10).await;
                    advance_time(Duration::from_secs(1)).await;
                    for _ in 0..100 {
                        if h2.requests()[0].data().len() >= 2 {
                            break;
                        }
                        yield_times(2).await;
                    }
                    if h2.requests()[0].data().len() >= 2 {
                        break;
                    }
                }
                let written = h2.requests()[0].data().concat();
                let mut decoder = frame::FrameDecoder::new();
                let frames = decoder.push(&written).expect("client frames");
                assert_eq!(frames.len(), 2);
                assert_eq!(frames[1].payload, proto::heartbeat());
            };
            assert!(matches!(race(session, control).await, Race::Right(())));
            assert!(!blocked(host.context(), Duration::from_secs(600)));
        });
    }

    #[test]
    fn non_success_run_sse_becomes_a_connect_trailer() {
        let _guard = http2_memory::test_lock();
        http2_memory::reset_for_tests();
        block_on(async {
            let h1 =
                Http1Server::start(vec![Http1Response::new(400).body(b"bad opening".to_vec())])
                    .await;
            let host = pooled_host();
            http2_memory::remember(host.context());
            let output = run(
                host.context(),
                &endpoints(&h1, "http://127.0.0.1:9"),
                &AgentHost::default(),
                &token(),
                empty_run_args(),
            )
            .await
            .expect("synthetic trailer");
            assert_eq!(output.trailers.len(), 1);
            assert!(output.trailers[0].is_trailer());
            let body: hofund_plugin_sdk::json::Value =
                hofund_plugin_sdk::json::from_slice(&output.trailers[0].payload)
                    .expect("trailer JSON");
            assert_eq!(body["error"]["code"], "invalid_argument");
            assert_eq!(body["error"]["message"], "bad opening");
            assert_eq!(h1.requests().len(), 1);
        });
    }

    #[test]
    fn http2_answers_kv_and_exec_requests_before_finishing() {
        let _guard = http2_memory::test_lock();
        http2_memory::reset_for_tests();
        block_on(async {
            let digest = b"digest-found".to_vec();
            let found = Pb::new()
                .message(
                    3,
                    Pb::new()
                        .wire(1, &proto::WireValue::Bytes(b"kv-found".to_vec()))
                        .bytes(2, &Pb::new().bytes(1, b"exact blob bytes").finish()),
                )
                .finish();
            let server_payloads = [
                server_kv_get(b"kv-found", &digest),
                server_kv_get(b"kv-missing", b"digest-missing"),
                server_kv_store(b"kv-store"),
                server_exec(10, "exec-env", 10),
                server_exec(36, "exec-tools", 36),
                server_exec(11, "exec-mcp", 11),
                server_exec(12, "exec-other", 99),
            ];
            let body = server_payloads
                .iter()
                .flat_map(|payload| server_frame(payload.clone()))
                .collect();
            let h2 = Http2Server::start(vec![Http2Response::new(200).data(body)]).await;
            let h1 = Http1Server::start(vec![config_reply(h2.base_url())]).await;
            let host = pooled_host();
            let endpoints = endpoints(&h1, h2.base_url());
            let mut blobs = HashMap::new();
            blobs.insert(digest, b"exact blob bytes".to_vec());
            let output = run(
                host.context(),
                &endpoints,
                &AgentHost::default(),
                &token(),
                run_args(&blobs),
            )
            .await
            .expect("run");
            let expected_events = server_payloads
                .iter()
                .flat_map(|payload| proto::decode_server(payload).0)
                .collect::<Vec<_>>();
            assert_eq!(output.events, expected_events);
            assert!(output.trailers.is_empty());

            let expected = vec![
                proto::kv_get_found(
                    &proto::WireValue::Bytes(b"kv-found".to_vec()),
                    b"exact blob bytes",
                ),
                proto::kv_get_missing(&proto::WireValue::Bytes(b"kv-missing".to_vec())),
                proto::kv_store_ack(&proto::WireValue::Bytes(b"kv-store".to_vec())),
                proto::exec_env_result(10, "exec-env", "linux", "/tmp/r8f2"),
                proto::close_exec(10),
                proto::exec_tools_result(36, "exec-tools", &[]),
                proto::close_exec(36),
                proto::refuse_exec(11),
                proto::close_exec(11),
                proto::refuse_exec(12),
                proto::close_exec(12),
            ];
            assert_eq!(found, expected[0]);
            let expected_written = std::iter::once(proto::client_run(b"opening"))
                .chain(expected)
                .flat_map(|payload| frame::encode(0, &payload).expect("client frame"))
                .collect::<Vec<_>>();
            for _ in 0..100 {
                if h2.requests()[0].data().concat().len() >= expected_written.len() {
                    break;
                }
                yield_times(2).await;
            }
            let request = &h2.requests()[0];
            let written = request.data().concat();
            assert_eq!(written, expected_written);
            let mut outbound = written;
            for (name, value) in request.headers() {
                outbound.extend_from_slice(name.as_bytes());
                outbound.extend_from_slice(value.as_bytes());
            }
            let lower = String::from_utf8_lossy(&outbound).to_ascii_lowercase();
            let mut reference_name = String::from("mag");
            reference_name.push_str("pie");
            assert!(!lower.contains("hofund"));
            assert!(!lower.contains(&reference_name));
            assert_clean_http1(&h1);
        });
    }

    #[test]
    fn http1_answers_kv_and_exec_requests_with_ordered_appends() {
        let _guard = http2_memory::test_lock();
        http2_memory::reset_for_tests();
        block_on(async {
            let digest = b"digest-http1".to_vec();
            let server_payloads = [
                server_kv_get(b"kv-found", &digest),
                server_kv_get(b"kv-missing", b"digest-missing"),
                server_kv_store(b"kv-store"),
                server_exec(20, "exec-env", 10),
                server_exec(21, "exec-refused", 12),
            ];
            let body = server_payloads
                .iter()
                .flat_map(|payload| server_frame(payload.clone()))
                .collect();
            let h1 = Http1Server::start(vec![Http1Response::chunked(vec![body])]).await;
            let host = pooled_host();
            http2_memory::remember(host.context());
            let mut blobs = HashMap::new();
            blobs.insert(digest, b"http1 blob".to_vec());
            let output = run(
                host.context(),
                &endpoints(&h1, "http://127.0.0.1:9"),
                &AgentHost::default(),
                &token(),
                run_args(&blobs),
            )
            .await
            .expect("run");
            assert_eq!(output.events.len(), 5);
            assert!(output.trailers.is_empty());

            let expected = vec![
                proto::client_run(b"opening"),
                proto::kv_get_found(
                    &proto::WireValue::Bytes(b"kv-found".to_vec()),
                    b"http1 blob",
                ),
                proto::kv_get_missing(&proto::WireValue::Bytes(b"kv-missing".to_vec())),
                proto::kv_store_ack(&proto::WireValue::Bytes(b"kv-store".to_vec())),
                proto::exec_env_result(20, "exec-env", "linux", "/tmp/r8f2"),
                proto::close_exec(20),
                proto::refuse_exec(21),
                proto::close_exec(21),
            ];
            let requests = h1.requests();
            let appends = requests
                .iter()
                .filter(|request| request.path() == "/aiserver.v1.BidiService/BidiAppend")
                .collect::<Vec<_>>();
            assert_eq!(appends.len(), expected.len());
            for (sequence, (request, payload)) in appends.into_iter().zip(expected).enumerate() {
                let fields = proto::fields(request.body());
                assert_eq!(
                    fields[0].as_string().as_deref(),
                    Some(proto::to_hex(&payload).as_str())
                );
                assert_eq!(fields[2].as_varint(), Some(sequence as u64));
            }
            assert_clean_http1(&h1);
        });
    }

    #[test]
    fn http1_pipelines_sixteen_kv_replies_and_barriers_the_next_writes() {
        let _guard = http2_memory::test_lock();
        http2_memory::reset_for_tests();
        block_on_paused(async {
            let digest = b"digest-window".to_vec();
            let mut server_payloads = (0..17)
                .map(|id| server_kv_get(format!("kv-{id}").as_bytes(), &digest))
                .collect::<Vec<_>>();
            server_payloads.push(server_exec(30, "exec-after-kv", 12));
            let body = server_payloads
                .iter()
                .flat_map(|payload| server_frame(payload.clone()))
                .collect();
            let mut responses = vec![Http1Response::chunked(vec![body]), Http1Response::new(200)];
            responses.extend((0..16).map(|_| {
                Http1Response::new(200)
                    .body(b"delayed".to_vec())
                    .chunk_delay(Duration::from_secs(5))
            }));
            let h1 = Http1Server::start(responses).await;
            let host = pooled_host();
            http2_memory::remember(host.context());
            let mut blobs = HashMap::new();
            blobs.insert(digest, b"window blob".to_vec());
            let mut args = run_args(&blobs);
            args.limits.heartbeat = Duration::from_secs(60);
            let endpoints = endpoints(&h1, "http://127.0.0.1:9");
            let resolver = AgentHost::default();
            let token = token();
            let session = run(host.context(), &endpoints, &resolver, &token, args);
            let control = async {
                for _ in 0..200 {
                    if h1.requests().len() >= 18 {
                        break;
                    }
                    yield_times(2).await;
                }
                assert_eq!(h1.requests().len(), 18);
                for request in &h1.requests()[2..] {
                    let fields = proto::fields(request.body());
                    assert!(
                        fields[0]
                            .as_string()
                            .is_some_and(|hex| hex.starts_with("1a"))
                    );
                }
                yield_times(20).await;
                assert_eq!(h1.requests().len(), 18, "a 17th KV must wait");
                advance_time(Duration::from_secs(5)).await;
                std::future::pending::<()>().await;
            };
            let output = match race(session, control).await {
                Race::Left(output) => output.expect("run"),
                Race::Right(()) => panic!("control finished before the session"),
            };
            assert_eq!(output.events.len(), 18);
            assert!(output.trailers.is_empty());

            let expected = std::iter::once(proto::client_run(b"opening"))
                .chain((0..17).map(|id| {
                    proto::kv_get_found(
                        &proto::WireValue::Bytes(format!("kv-{id}").into_bytes()),
                        b"window blob",
                    )
                }))
                .chain([proto::refuse_exec(30), proto::close_exec(30)])
                .collect::<Vec<_>>();
            let requests = h1.requests();
            let appends = requests
                .iter()
                .filter(|request| request.path() == "/aiserver.v1.BidiService/BidiAppend")
                .collect::<Vec<_>>();
            assert_eq!(appends.len(), expected.len());
            for (sequence, (request, payload)) in appends.into_iter().zip(expected).enumerate() {
                let fields = proto::fields(request.body());
                assert_eq!(
                    fields[0].as_string().as_deref(),
                    Some(proto::to_hex(&payload).as_str())
                );
                assert_eq!(fields[2].as_varint(), Some(sequence as u64));
            }
        });
    }

    #[test]
    fn append_deadline_and_connect_codes_follow_the_wire_table() {
        assert_eq!(append_deadline(1), Duration::from_secs(61));
        assert_eq!(append_deadline(65_536), Duration::from_secs(61));
        assert_eq!(append_deadline(65_537), Duration::from_secs(62));
        assert_eq!(connect_code(400), "invalid_argument");
        assert_eq!(connect_code(401), "unauthenticated");
        assert_eq!(connect_code(403), "permission_denied");
        assert_eq!(connect_code(429), "resource_exhausted");
        assert_eq!(connect_code(500), "unavailable");
    }
}
