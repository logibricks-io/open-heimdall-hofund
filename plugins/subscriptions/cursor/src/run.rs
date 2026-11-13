//! One Cursor Run over bidirectional HTTP/2 or the HTTP/1.1 fallback.

use std::future::Future;
use std::task::Poll;
use std::time::Duration;

use hofund_plugin_api::{
    BodyStream, DuplexRequest, HostContext, HttpVersionPin, PluginError, PluginErrorKind,
    SecretString,
};
use hofund_plugin_sdk::json::json;
use hofund_plugin_sdk::oauth::secret_text;
use hofund_plugin_sdk::{Call, Race, race, sleep};

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

/// Events and terminal frames collected from one transport session.
#[derive(Debug, Default)]
pub(crate) struct RunOutput {
    /// Decoded non-trailer server messages.
    pub events: Vec<ServerEvent>,
    /// Connect trailers, including synthetic HTTP/1.1 failures.
    pub trailers: Vec<Frame>,
    /// Drained non-2xx HTTP/2 response body.
    pub error_body: Option<Vec<u8>>,
}

/// Opens a Run, writes `opening`, reads server events, and sends heartbeats.
pub(crate) async fn run(
    host: &HostContext,
    endpoints: &Endpoints,
    agent_host: &AgentHost,
    token: &SecretString,
    opening: Vec<u8>,
    team_id: Option<&str>,
    limits: Limits,
) -> Result<RunOutput, PluginError> {
    let request_id = hofund_plugin_sdk::uuid::v4()
        .map_err(|error| PluginError::io(format!("cannot create a Run request id: {error}")))?;
    if !http2_memory::blocked(host, limits.http2_memory) {
        let agent = agent_host
            .resolve(host, endpoints, token, false, team_id)
            .await;
        let url = format!("{}/agent.v1.AgentService/Run", agent.trim_end_matches('/'));
        let request = run_duplex(
            url,
            endpoints,
            token,
            &request_id,
            team_id,
            limits.header_wait,
        );
        match host.http().open_duplex(request).await {
            Ok(session) => {
                return run_http2(session, opening, limits).await;
            }
            Err(error) => {
                if http2_memory::should_remember(&error) {
                    http2_memory::remember(host);
                }
            }
        }
    }
    run_http1(
        host,
        endpoints,
        token,
        opening,
        &request_id,
        team_id,
        limits,
    )
    .await
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

async fn run_http2(
    mut session: hofund_plugin_api::DuplexSession,
    opening: Vec<u8>,
    limits: Limits,
) -> Result<RunOutput, PluginError> {
    if !(200..300).contains(&session.status().as_u16()) {
        let body = drain(session.reader(), limits.error_drain).await?;
        return Ok(RunOutput {
            error_body: Some(body),
            ..RunOutput::default()
        });
    }
    let (mut reader, mut writer) = session.into_split();
    writer.write(frame::encode(0, &opening)?.into()).await?;
    let mut output = RunOutput::default();
    let mut decoder = FrameDecoder::new();
    let mut heartbeat = std::pin::pin!(sleep(limits.heartbeat));
    loop {
        match race(reader.reader().next_chunk(), &mut heartbeat).await {
            Race::Left(Some(Ok(chunk))) => decode_chunk(&mut decoder, &chunk, &mut output)?,
            Race::Left(Some(Err(error))) => return Err(error),
            Race::Left(None) => {
                finish_decoder(decoder, &mut output)?;
                writer.finish().await?;
                return Ok(output);
            }
            Race::Right(()) => {
                writer
                    .write(frame::encode(0, &proto::heartbeat())?.into())
                    .await?;
                heartbeat.as_mut().set(sleep(limits.heartbeat));
            }
        }
    }
}

async fn run_http1(
    host: &HostContext,
    endpoints: &Endpoints,
    token: &SecretString,
    opening: Vec<u8>,
    request_id: &str,
    team_id: Option<&str>,
    limits: Limits,
) -> Result<RunOutput, PluginError> {
    let down_url = format!(
        "{}/agent.v1.AgentService/RunSSE",
        endpoints.api.trim_end_matches('/')
    );
    let down_payload = proto::Pb::new().string(1, request_id).finish();
    let down = run_call(
        down_url,
        "application/connect+proto",
        true,
        endpoints,
        token,
        request_id,
        team_id,
    )
    .bytes(frame::encode(0, &down_payload)?)
    .open_raw(host.http(), 1);
    let opening_append = append(host, endpoints, token, request_id, team_id, &opening, 0);
    let (down, opening_append) = join(down, opening_append).await;
    let mut output = RunOutput::default();
    let mut response = match down {
        Ok(response) => response,
        Err(error) => {
            let failure = AppendFailure::from(error);
            output.trailers.push(synthetic_end(failure)?);
            return Ok(output);
        }
    };
    let status = response.status().as_u16();
    if !(200..300).contains(&status) {
        let body = drain(response.body_mut(), limits.error_drain).await?;
        let message = error_message(status, &body);
        output
            .trailers
            .push(synthetic_end(AppendFailure::status(status, message))?);
        return Ok(output);
    }

    if let Err(failure) = opening_append {
        output.trailers.push(synthetic_end(failure)?);
        return Ok(output);
    }
    let mut sequence = 1_u64;

    let mut cancelled = false;
    let mut decoder = FrameDecoder::new();
    let mut heartbeat = std::pin::pin!(sleep(limits.heartbeat));
    loop {
        match race(response.body_mut().next_chunk(), &mut heartbeat).await {
            Race::Left(Some(Ok(chunk))) => decode_chunk(&mut decoder, &chunk, &mut output)?,
            Race::Left(Some(Err(error))) => return Err(error),
            Race::Left(None) => {
                finish_decoder(decoder, &mut output)?;
                return Ok(output);
            }
            Race::Right(()) => {
                if let Err(failure) = append(
                    host,
                    endpoints,
                    token,
                    request_id,
                    team_id,
                    &proto::heartbeat(),
                    sequence,
                )
                .await
                {
                    output.trailers.push(synthetic_end(failure)?);
                    cancelled = true;
                }
                sequence += 1;
                if cancelled {
                    return Ok(output);
                }
                heartbeat.as_mut().set(sleep(limits.heartbeat));
            }
        }
    }
}

async fn join<A, B>(left: A, right: B) -> (A::Output, B::Output)
where
    A: Future,
    B: Future,
{
    let mut left = std::pin::pin!(left);
    let mut right = std::pin::pin!(right);
    let mut left_output = None;
    let mut right_output = None;
    std::future::poll_fn(move |cx| {
        if left_output.is_none()
            && let Poll::Ready(output) = left.as_mut().poll(cx)
        {
            left_output = Some(output);
        }
        if right_output.is_none()
            && let Poll::Ready(output) = right.as_mut().poll(cx)
        {
            right_output = Some(output);
        }
        if left_output.is_some() && right_output.is_some() {
            Poll::Ready((
                left_output.take().expect("left future completed"),
                right_output.take().expect("right future completed"),
            ))
        } else {
            Poll::Pending
        }
    })
    .await
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

#[allow(clippy::too_many_arguments)]
async fn append(
    host: &HostContext,
    endpoints: &Endpoints,
    token: &SecretString,
    request_id: &str,
    team_id: Option<&str>,
    payload: &[u8],
    sequence: u64,
) -> Result<(), AppendFailure> {
    let url = format!(
        "{}/aiserver.v1.BidiService/BidiAppend",
        endpoints.api.trim_end_matches('/')
    );
    let body = proto::bidi_append(payload, request_id, sequence);
    let reply = run_call(
        url,
        "application/proto",
        true,
        endpoints,
        token,
        request_id,
        team_id,
    )
    .read_timeout(append_deadline(body.len()))
    .bytes(&body)
    .fetch(host.http())
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
    output: &mut RunOutput,
) -> Result<(), PluginError> {
    for decoded in decoder.push(chunk)? {
        push_frame(decoded, output);
    }
    Ok(())
}

fn finish_decoder(decoder: FrameDecoder, output: &mut RunOutput) -> Result<(), PluginError> {
    for decoded in decoder.finish()? {
        push_frame(decoded, output);
    }
    Ok(())
}

fn push_frame(decoded: Frame, output: &mut RunOutput) {
    if decoded.is_trailer() {
        output.trailers.push(decoded);
    } else {
        output
            .events
            .extend(proto::decode_server(&decoded.payload).0);
    }
}

#[cfg(test)]
mod tests {
    use std::time::Duration;

    use hofund_kit_subscription::{
        Http1Response, Http1Server, Http2Response, Http2Server, TestHost, advance_time, block_on,
        block_on_paused, header_park, yield_times,
    };
    use hofund_plugin_api::{PluginId, PluginKind, SecretString};
    use hofund_plugin_sdk::{Race, race};

    use super::{Limits, append_deadline, connect_code, run};
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
                proto::client_run(b"opening"),
                None,
                Limits::default(),
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
                proto::client_run(b"opening"),
                Some("42"),
                Limits::default(),
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
                proto::client_run(b"opening"),
                None,
                limits,
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
                proto::client_run(b"opening"),
                None,
                limits,
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
                proto::client_run(b"opening"),
                None,
                limits,
            )
            .await
            .expect("http2 retry");
            assert_eq!(output.events, [ServerEvent::TextDelta("after".into())]);
            assert_eq!(h2.requests().len(), 2);
            drop(header_release);
        });
    }

    #[test]
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
                proto::client_run(b"opening"),
                None,
                Limits::default(),
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
                proto::client_run(b"opening"),
                None,
                Limits::default(),
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
                    proto::client_run(b"opening"),
                    None,
                    Limits::default(),
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
                proto::client_run(b"must-not-be-written"),
                None,
                Limits::default(),
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
                proto::client_run(b"opening"),
                None,
                limits,
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
    fn http1_fallback_sends_opening_append_before_run_sse_headers() {
        let _guard = http2_memory::test_lock();
        http2_memory::reset_for_tests();
        block_on_paused(async {
            let (header_release, header_gate) = header_park();
            let h1 = Http1Server::start(vec![
                Http1Response::new(200).park_headers(header_gate),
                Http1Response::new(200),
            ])
            .await;
            let host = pooled_host();
            http2_memory::remember(host.context());
            let endpoints = endpoints(&h1, "http://127.0.0.1:9");
            let resolver = AgentHost::default();
            let token = token();
            let mut session = std::pin::pin!(run(
                host.context(),
                &endpoints,
                &resolver,
                &token,
                proto::client_run(b"opening"),
                None,
                Limits::default(),
            ));
            let control = async {
                for _ in 0..100 {
                    if h1
                        .requests()
                        .iter()
                        .any(|request| request.path() == "/aiserver.v1.BidiService/BidiAppend")
                    {
                        break;
                    }
                    yield_times(2).await;
                }
                let requests = h1.requests();
                let opening = requests
                    .iter()
                    .find(|request| request.path() == "/aiserver.v1.BidiService/BidiAppend")
                    .expect("opening BidiAppend must arrive while RunSSE headers are parked");
                let fields = proto::fields(opening.body());
                assert_eq!(fields[2].as_varint(), Some(0));
                assert_eq!(
                    fields[0].as_string().as_deref(),
                    Some(proto::to_hex(&proto::client_run(b"opening")).as_str())
                );
                header_release.release();
            };
            assert!(matches!(race(&mut session, control).await, Race::Right(())));
            let output = session.await.expect("run after releasing headers");
            assert!(output.events.is_empty());
            assert!(output.trailers.is_empty());
            assert_eq!(run_sse_count(&h1), 1);
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
                proto::client_run(b"opening"),
                None,
                Limits {
                    heartbeat: Duration::from_secs(1),
                    ..Limits::default()
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
                proto::client_run(b"opening"),
                None,
                Limits::default(),
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
            assert_eq!(h1.requests().len(), 2);
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
