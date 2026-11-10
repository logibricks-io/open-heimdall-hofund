//! Claude Code spawn plan, stdin, and stream-json turn.

use std::collections::BTreeMap;
use std::ffi::OsString;
use std::path::PathBuf;

use hofund_plugin_api::{ContentPart, PluginError, Request, Role, StreamEvent};

use super::decode::{Decoder, Step};
use super::wsl::wrap_argv;

/// Environment names stripped from the inherited host environment before
/// account pairs are applied. Incoming token and API keys must not leak
/// into a run for a different account.
pub(crate) const ENV_DENY: &[&str] = &[
    "ANTHROPIC_API_KEY",
    "ANTHROPIC_AUTH_TOKEN",
    "ANTHROPIC_BASE_URL",
    "CLAUDE_API_KEY",
    "CLAUDE_CODE_OAUTH_TOKEN",
    "CLAUDE_CONFIG_DIR",
];

/// Pairs always set after the deny-list scrub, matching the design §1.5
/// append list.
pub(crate) const ALWAYS_ENV: &[(&str, &str)] = &[
    ("ENABLE_CLAUDEAI_MCP_SERVERS", "0"),
    ("DISABLE_AUTO_COMPACT", "1"),
    ("CLAUDE_CODE_DISABLE_NONESSENTIAL_TRAFFIC", "1"),
    ("CLAUDE_CODE_DISABLE_AUTO_MEMORY", "1"),
    ("CLAUDE_CODE_DISABLE_GIT_INSTRUCTIONS", "1"),
    ("CLAUDE_CODE_PROMPT_CACHE_TTL", "1h"),
];

/// How a text-only run should be started.
#[derive(Clone, Debug, Eq, PartialEq)]
pub(crate) struct StartSpec {
    /// Absolute Claude Code (or WSL) program.
    pub program: PathBuf,
    /// Working directory for the child.
    pub cwd: PathBuf,
    /// Model id passed as `--model`.
    pub model: String,
    /// Access token, set only in the environment.
    pub token: OsString,
    /// `CLAUDE_CONFIG_DIR` for this account.
    pub config_dir: PathBuf,
    /// When true, set `IS_SANDBOX=1`.
    pub sandbox: bool,
    /// Optional proxy pairs recorded on the plan (name, value).
    pub proxy: Vec<(OsString, OsString)>,
    /// Optional WSL distro. When set, the program is wrapped in `wsl.exe`.
    pub wsl_distro: Option<String>,
    /// `--mcp-config` path when the turn offers tools.
    pub mcp_config: Option<PathBuf>,
    /// Session id for `--resume` after unshelve. WSL keeps `--no-session-persistence`.
    pub resume_session: Option<String>,
}

/// Arguments, environment, and stdin handed to Claude Code.
#[derive(Clone, Debug, Eq, PartialEq)]
pub(crate) struct SpawnPlan {
    /// Program and arguments, already rewritten for WSL when the caller
    /// expects that.
    pub argv: Vec<OsString>,
    /// Environment after the caller's scrub and append, including proxy
    /// variables when the account has proxy metadata.
    pub env: BTreeMap<OsString, OsString>,
    /// stream-json lines written to stdin.
    pub stdin: String,
}

/// Rejects inbound images. Tool calls are served by the parking slice.
pub(crate) fn reject_non_text(request: &Request) -> Result<(), PluginError> {
    if has_image(request) {
        return Err(PluginError::unsupported(
            "images through Claude Code are not served yet",
        ));
    }
    Ok(())
}

/// Builds the argv, env, and stdin for a text-only start (no resume, no MCP).
pub(crate) fn start_plan(spec: &StartSpec, request: &Request) -> Result<SpawnPlan, PluginError> {
    reject_non_text(request)?;
    let mut args: Vec<OsString> = vec![
        os("-p"),
        os("--output-format"),
        os("stream-json"),
        os("--input-format"),
        os("stream-json"),
        os("--include-partial-messages"),
        os("--verbose"),
        os("--model"),
        OsString::from(&spec.model),
        os("--tools"),
        os(""),
    ];
    if let Some(config) = &spec.mcp_config {
        args.push(os("--strict-mcp-config"));
        args.push(os("--mcp-config"));
        args.push(config.as_os_str().to_owned());
    }
    args.extend([
        os("--setting-sources"),
        os(""),
        os("--dangerously-skip-permissions"),
        os("--no-session-persistence"),
    ]);
    if let Some(session) = &spec.resume_session
        && spec.wsl_distro.is_none()
    {
        args.retain(|arg| arg != os("--no-session-persistence").as_os_str());
        args.push(os("--resume"));
        args.push(OsString::from(session));
    }
    let argv = if let Some(distro) = &spec.wsl_distro {
        wrap_argv(
            PathBuf::from(r"C:\Windows\System32\wsl.exe").as_path(),
            distro,
            &spec.program,
            &args,
        )
    } else {
        let mut argv = vec![spec.program.as_os_str().to_owned()];
        argv.extend(args);
        argv
    };

    let mut env = std::collections::BTreeMap::new();
    for (key, value) in ALWAYS_ENV {
        env.insert(os(*key), os(*value));
    }
    env.insert(os("CLAUDE_CODE_OAUTH_TOKEN"), spec.token.clone());
    env.insert(
        os("CLAUDE_CONFIG_DIR"),
        spec.config_dir.as_os_str().to_owned(),
    );
    if spec.sandbox {
        env.insert(os("IS_SANDBOX"), os("1"));
    }
    for (key, value) in &spec.proxy {
        env.insert(key.clone(), value.clone());
    }

    Ok(SpawnPlan {
        argv,
        env,
        stdin: stdin_from_request(request),
    })
}

/// The working directory recorded on `spec`.
#[must_use]
pub(crate) fn workdir(spec: &StartSpec) -> &std::path::Path {
    &spec.cwd
}

/// Decodes a complete stream-json script into IR events.
///
/// A `result` error, or a tool-call block in a text-only turn, is a failure.
pub(crate) fn events_from_stdout(stdout: &str) -> Result<Vec<StreamEvent>, PluginError> {
    let mut decoder = Decoder::new();
    let mut events = Vec::new();
    for line in stdout.lines() {
        match decoder.push_line(line) {
            Some(Step::Events(batch)) => {
                if batch.iter().any(event_is_tool) {
                    return Err(PluginError::unsupported(
                        "tool calls through Claude Code are not served yet",
                    ));
                }
                events.extend(batch);
            }
            Some(Step::ResultError(error)) => {
                return Err(PluginError::status_error(
                    error.api_error_status.unwrap_or(502),
                    error.message,
                ));
            }
            Some(Step::Session(_) | Step::RateLimit(_) | Step::Completed) | None => {}
        }
    }
    Ok(events)
}

/// Tool-call id and name pairs decoded from a stream-json turn.
pub(crate) type DecodedCalls = Vec<(String, String)>;

/// Decodes a stream-json script, keeping tool-call events for parking.
pub(crate) fn events_allowing_tools(
    stdout: &str,
) -> Result<(Vec<StreamEvent>, DecodedCalls), PluginError> {
    let mut decoder = Decoder::new();
    let mut events = Vec::new();
    let mut calls = Vec::new();
    for line in stdout.lines() {
        match decoder.push_line(line) {
            Some(Step::Events(batch)) => {
                for event in &batch {
                    if let Some((_, id, name)) = super::decode::start_tool_call(event) {
                        calls.push((id, name));
                    }
                }
                events.extend(batch);
            }
            Some(Step::ResultError(error)) => {
                return Err(PluginError::status_error(
                    error.api_error_status.unwrap_or(502),
                    error.message,
                ));
            }
            Some(Step::Session(_) | Step::RateLimit(_) | Step::Completed) | None => {}
        }
    }
    Ok((events, calls))
}

/// Stream-json stdin for the user and system text in `request`.
pub(crate) fn stdin_from_request(request: &Request) -> String {
    let mut out = String::new();
    for message in &request.messages {
        let text: String = message
            .content
            .iter()
            .filter_map(|part| match part {
                ContentPart::Text(part) => Some(part.text.as_str()),
                _ => None,
            })
            .collect::<Vec<_>>()
            .join(
                "
",
            );
        if text.is_empty() {
            continue;
        }
        let role = match message.role {
            Role::Assistant | Role::Tool => continue,
            Role::System | Role::Developer | Role::User => "user",
        };
        let line = serde_json::json!({
            "type": "user",
            "message": {
                "role": role,
                "content": [{"type": "text", "text": text}]
            }
        });
        out.push_str(&line.to_string());
        out.push('\n');
    }
    out
}

/// Stream-json stdin for the messages since a resumed reply.
pub(crate) fn stdin_from_since(since: &[hofund_plugin_api::Message]) -> String {
    let mut request = Request::new("claude");
    request.messages = since.to_vec();
    stdin_from_request(&request)
}

/// Stream-json stdin for `saidWithResults` user text.
pub(crate) fn stdin_from_said(said: &str) -> String {
    if said.is_empty() {
        return String::new();
    }
    let line = serde_json::json!({
        "type": "user",
        "message": {
            "role": "user",
            "content": [{"type": "text", "text": said}]
        }
    });
    format!("{line}\n")
}

fn event_is_tool(event: &StreamEvent) -> bool {
    let StreamEvent::ContentBlockStart(start) = event else {
        return false;
    };
    serde_json::to_value(&start.block)
        .ok()
        .and_then(|block| {
            block
                .get("type")
                .and_then(|value| value.as_str())
                .map(str::to_owned)
        })
        .is_some_and(|typ| typ == "tool_call")
}

fn os(value: impl AsRef<str>) -> OsString {
    OsString::from(value.as_ref())
}

/// True when `request` carries an image part.
pub(crate) fn has_image(request: &Request) -> bool {
    request.messages.iter().any(|message| {
        message
            .content
            .iter()
            .any(|part| matches!(part, ContentPart::Image(_)))
    })
}

/// True when `request` lists tools or already carries a tool call or result.
pub(crate) fn has_tools(request: &Request) -> bool {
    if !request.tools.is_empty() {
        return true;
    }
    request.messages.iter().any(|message| {
        message
            .content
            .iter()
            .any(|part| matches!(part, ContentPart::ToolCall(_) | ContentPart::ToolResult(_)))
    })
}

/// Deny-list names as `OsString` values for `spawn_long_lived`.
#[must_use]
pub(crate) fn env_deny() -> Vec<OsString> {
    ENV_DENY.iter().map(|name| os(*name)).collect()
}

#[cfg(test)]
mod tests {
    use super::{
        ALWAYS_ENV, ENV_DENY, StartSpec, events_from_stdout, has_image, has_tools, reject_non_text,
        start_plan, stdin_from_request,
    };
    use crate::bridge::runner::FakeRunner;
    use hofund_plugin_api::{ContentPart, Message, PluginErrorKind, Request, Role, StreamEvent};
    use serde_json::json;
    use std::ffi::OsString;
    use std::path::PathBuf;

    const MESSAGE_DELTA: &str = include_str!("fixtures/stream/message_delta.jsonl");
    const TOOL_USE: &str = include_str!("fixtures/stream/tool_use.jsonl");

    #[cfg(not(windows))]
    fn program() -> PathBuf {
        PathBuf::from("/opt/claude/bin/claude")
    }

    #[cfg(windows)]
    fn program() -> PathBuf {
        PathBuf::from(r"C:\claude\bin\claude.exe")
    }

    fn text_request(text: &str) -> Request {
        let mut message = Message::new(Role::User);
        message.content = vec![ContentPart::text(text)];
        Request::new("claude-sonnet-4-5").with_messages(vec![message])
    }

    fn request_from(value: serde_json::Value) -> Request {
        serde_json::from_value(value).expect("request")
    }

    fn spec() -> StartSpec {
        StartSpec {
            program: program(),
            cwd: PathBuf::from(if cfg!(windows) {
                r"C:\tmp\acct"
            } else {
                "/tmp/acct"
            }),
            model: "claude-sonnet-4-5".to_owned(),
            token: OsString::from("secret-token"),
            config_dir: PathBuf::from(if cfg!(windows) {
                r"C:\tmp\acct"
            } else {
                "/tmp/acct"
            }),
            sandbox: true,
            proxy: vec![(
                OsString::from("HTTP_PROXY"),
                OsString::from("http://127.0.0.1:8080"),
            )],
            wsl_distro: None,
            mcp_config: None,
            resume_session: None,
        }
    }

    #[test]
    fn tools_and_images_are_rejected() {
        let tools = request_from(json!({
            "model": "claude-sonnet-4-5",
            "messages": [{"role": "user", "content": [{"type": "text", "text": "hi"}]}],
            "tools": [{"name": "lookup", "input_schema": {"type": "object"}}]
        }));
        assert!(has_tools(&tools));
        assert!(reject_non_text(&tools).is_ok(), "tools are served in R-6d");

        let image = request_from(json!({
            "model": "claude-sonnet-4-5",
            "messages": [{
                "role": "user",
                "content": [{
                    "type": "image",
                    "source": {"type": "url", "url": "https://example.test/a.png"}
                }]
            }]
        }));
        assert!(has_image(&image));
        let error = reject_non_text(&image).expect_err("images");
        assert_eq!(error.kind(), PluginErrorKind::Unsupported);

        assert!(reject_non_text(&text_request("hello")).is_ok());
    }

    #[test]
    fn plan_puts_token_in_env_and_core_argv() {
        let request = text_request("Hello");
        let plan = start_plan(&spec(), &request).expect("plan");

        let argv: Vec<String> = plan
            .argv
            .iter()
            .map(|s| s.to_string_lossy().into_owned())
            .collect();
        assert_eq!(argv[0], program().to_string_lossy());
        assert!(argv.contains(&"-p".to_owned()));
        assert!(
            argv.windows(2)
                .any(|w| w == ["--output-format", "stream-json"])
        );
        assert!(
            argv.windows(2)
                .any(|w| w == ["--input-format", "stream-json"])
        );
        assert!(argv.contains(&"--include-partial-messages".to_owned()));
        assert!(argv.contains(&"--verbose".to_owned()));
        assert!(
            argv.windows(2)
                .any(|w| w == ["--model", "claude-sonnet-4-5"])
        );
        assert!(argv.windows(2).any(|w| w == ["--tools", ""]));
        assert!(argv.contains(&"--dangerously-skip-permissions".to_owned()));
        assert!(argv.contains(&"--no-session-persistence".to_owned()));
        assert!(
            !argv.iter().any(|a| a.contains("secret-token")),
            "token must not appear on argv"
        );

        assert_eq!(
            plan.env.get(&OsString::from("CLAUDE_CODE_OAUTH_TOKEN")),
            Some(&OsString::from("secret-token"))
        );
        assert_eq!(
            plan.env.get(&OsString::from("CLAUDE_CONFIG_DIR")),
            Some(&OsString::from(spec().config_dir.as_os_str()))
        );
        assert_eq!(
            plan.env.get(&OsString::from("IS_SANDBOX")),
            Some(&OsString::from("1"))
        );
        assert_eq!(
            plan.env.get(&OsString::from("HTTP_PROXY")),
            Some(&OsString::from("http://127.0.0.1:8080"))
        );
        for (key, value) in ALWAYS_ENV {
            assert_eq!(
                plan.env
                    .get(&OsString::from(*key))
                    .map(|v| v.to_string_lossy().into_owned()),
                Some((*value).to_owned()),
                "{key}"
            );
        }
        assert!(ENV_DENY.contains(&"CLAUDE_CODE_OAUTH_TOKEN"));
        assert!(ENV_DENY.contains(&"ANTHROPIC_API_KEY"));
        assert_eq!(super::env_deny().len(), ENV_DENY.len());
        assert!(plan.stdin.contains("Hello"));
        assert_eq!(stdin_from_request(&request), plan.stdin);
    }

    #[test]
    fn fake_runner_completes_a_text_only_turn() {
        let request = text_request("Hello");
        let plan = start_plan(&spec(), &request).expect("plan");
        let runner = FakeRunner::from_stdout(MESSAGE_DELTA)
            .expect_token("secret-token")
            .expect_config_dir(spec().config_dir.clone())
            .expect_sandbox();
        let record = runner.run(&plan).expect("fake run");
        let events = events_from_stdout(&record.stdout).expect("decode");
        let text: String = events
            .iter()
            .filter_map(|event| match event {
                StreamEvent::TextDelta(delta) => Some(delta.text.as_str()),
                _ => None,
            })
            .collect();
        assert_eq!(text, "Hello world");
        assert!(
            events
                .iter()
                .any(|event| matches!(event, StreamEvent::MessageStop(_)))
        );
    }

    #[test]
    fn tool_use_in_a_text_only_script_fails_closed() {
        let error = events_from_stdout(TOOL_USE).expect_err("tool use");
        assert_eq!(error.kind(), PluginErrorKind::Unsupported);
    }

    #[test]
    fn unshelved_start_plan_resumes_the_session_and_drops_no_session_persistence() {
        let mut spec = spec();
        spec.resume_session = Some("sess_text".into());
        let plan = start_plan(&spec, &text_request("hi")).expect("plan");
        let args: Vec<String> = plan
            .argv
            .iter()
            .map(|a| a.to_string_lossy().into_owned())
            .collect();
        assert!(args.contains(&"--resume".into()), "{args:?}");
        assert!(args.contains(&"sess_text".into()), "{args:?}");
        assert!(
            !args.iter().any(|a| a == "--no-session-persistence"),
            "{args:?}"
        );
    }

    #[test]
    fn wsl_keeps_no_session_persistence_even_when_a_session_is_set() {
        let mut spec = spec();
        spec.resume_session = Some("sess_text".into());
        spec.wsl_distro = Some("Ubuntu".into());
        let plan = start_plan(&spec, &text_request("hi")).expect("plan");
        let args: Vec<String> = plan
            .argv
            .iter()
            .map(|a| a.to_string_lossy().into_owned())
            .collect();
        assert!(
            args.iter().any(|a| a == "--no-session-persistence"),
            "{args:?}"
        );
        assert!(!args.contains(&"--resume".into()), "{args:?}");
    }
}
