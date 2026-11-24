//! Claude Code spawn plan, stdin, and stream-json turn.

use std::collections::BTreeMap;
use std::ffi::OsString;
use std::path::PathBuf;

use hofund_plugin_api::{ContentPart, PluginError, Request, StreamEvent};

use super::decode::{Decoder, RateLimitInfo, ResultError, Step};
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

/// Defaults that keep a client tool from being moved to the background or
/// aborted for idle time. Each name is set to the default only when the
/// process environment does not already set it. The reference applies the
/// same pair on every bridge run, including a turn that offers no tools,
/// and forwards both names into a WSL distro. One-off runs do not use this
/// plan.
pub(crate) const TOOL_WAIT_DEFAULTS: &[(&str, &str)] = &[
    ("CLAUDE_CODE_MCP_AUTO_BACKGROUND_MS", "0"),
    ("CLAUDE_CODE_MCP_TOOL_IDLE_TIMEOUT", "0"),
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
    /// WSL probe result when [`wsl_distro`] is set; drives `cross_env` on the plan.
    pub wsl_tool: Option<crate::wslrun::Tool>,
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

/// Images are served as stream-json blocks. Kept as a hook for later fail-closed checks.
pub(crate) fn reject_non_text(_request: &Request) -> Result<(), PluginError> {
    Ok(())
}

/// Builds the argv, env, and stdin for a text-only start (no resume, no MCP).
pub(crate) fn start_plan(spec: &StartSpec, request: &Request) -> Result<SpawnPlan, PluginError> {
    start_plan_with(spec, request, |key| std::env::var_os(key))
}

/// [`start_plan`] with an injected process environment.
///
/// `ambient` is how the real process environment is read. Tests pass a map
/// so a user-set tool-wait value is deterministic without mutating the
/// process environment.
fn start_plan_with(
    spec: &StartSpec,
    request: &Request,
    ambient: impl Fn(&str) -> Option<OsString>,
) -> Result<SpawnPlan, PluginError> {
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
    for flag in super::flags::cli_flag_args(request) {
        args.push(os(&flag));
    }
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
    insert_tool_wait(&mut env, ambient);
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

    if let Some(tool) = &spec.wsl_tool {
        let pairs: Vec<_> = env.into_iter().collect();
        let forward = wsl_forward_names();
        let crossed = crate::wslrun::cross_env(tool, &pairs, &forward);
        env = crossed.into_iter().collect();
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
    let mut rejected = None;
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
                return Err(limited(&error, rejected.as_ref()));
            }
            Some(Step::RateLimit(info)) => remember_rate_limit(&mut rejected, info),
            Some(Step::Session(_) | Step::Completed) | None => {}
        }
    }
    Ok(events)
}

fn remember_rate_limit(saved: &mut Option<RateLimitInfo>, info: RateLimitInfo) {
    *saved = (info.status.as_deref() == Some("rejected")).then_some(info);
}

fn limited(error: &ResultError, rejected: Option<&RateLimitInfo>) -> PluginError {
    super::failure::classify_limited(error, rejected, std::time::SystemTime::now())
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
    let mut rejected = None;
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
                return Err(limited(&error, rejected.as_ref()));
            }
            Some(Step::RateLimit(info)) => remember_rate_limit(&mut rejected, info),
            Some(Step::Session(_) | Step::Completed) | None => {}
        }
    }
    Ok((events, calls))
}

/// Stream-json stdin for the user and system text (and image blocks) in `request`.
pub(crate) fn stdin_from_request(request: &Request) -> String {
    let mut out = String::new();
    for message in &request.messages {
        if !super::images::is_prompt_message(message) {
            continue;
        }
        let content = super::images::content_blocks(message);
        if content.is_empty() {
            continue;
        }
        let line = serde_json::json!({
            "type": "user",
            "message": {
                "role": "user",
                "content": content
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

/// Copies each tool-wait name from `ambient`, or the default when absent.
fn insert_tool_wait(
    env: &mut BTreeMap<OsString, OsString>,
    ambient: impl Fn(&str) -> Option<OsString>,
) {
    for &(key, default) in TOOL_WAIT_DEFAULTS {
        let value = ambient(key).unwrap_or_else(|| os(default));
        env.insert(os(key), value);
    }
}

/// Names [`crate::wslrun::cross_env`] lists on `WSLENV` for a bridge plan.
///
/// Account-directory and token names match the previous plan. The tool-wait
/// names are included so a distro run sees the same values as a native run.
fn wsl_forward_names() -> Vec<&'static str> {
    let mut names = Vec::with_capacity(2 + TOOL_WAIT_DEFAULTS.len());
    names.extend(["CLAUDE_CONFIG_DIR/p", "CLAUDE_CODE_OAUTH_TOKEN"]);
    names.extend(TOOL_WAIT_DEFAULTS.iter().map(|(key, _)| *key));
    names
}

/// True when `request` carries an image part.
#[allow(dead_code)]
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
        ALWAYS_ENV, ENV_DENY, StartSpec, events_allowing_tools, events_from_stdout, has_image,
        has_tools, reject_non_text, start_plan, stdin_from_request,
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
            wsl_tool: None,
            mcp_config: None,
            resume_session: None,
        }
    }

    fn wsl_tool() -> crate::wslrun::Tool {
        crate::wslrun::Tool {
            distro: "Ubuntu".into(),
            path: "/usr/bin/claude".into(),
            path_env: "/usr/bin".into(),
            mount: "/mnt/".into(),
            host: "172.20.0.1".into(),
            root: false,
            home: String::new(),
        }
    }

    #[test]
    fn tools_and_images_are_accepted() {
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
        assert!(reject_non_text(&image).is_ok(), "images are served in R-6f");
        let stdin = stdin_from_request(&image);
        assert!(stdin.contains("https://example.test/a.png"), "{stdin}");
        assert!(
            stdin.contains("\"type\":\"url\"") || stdin.contains("\"type\": \"url\""),
            "{stdin}"
        );

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
    fn native_start_plan_leaves_proxy_and_env_without_wslenv() {
        let plan = start_plan(&spec(), &text_request("hi")).expect("plan");
        assert!(
            !plan.env.contains_key(&OsString::from("WSLENV")),
            "{:?}",
            plan.env
        );
        assert_eq!(
            plan.env.get(&OsString::from("HTTP_PROXY")),
            Some(&OsString::from("http://127.0.0.1:8080"))
        );
    }

    #[test]
    fn wsl_start_plan_crosses_env_and_rewrites_loopback_proxy() {
        let mut spec = spec();
        spec.wsl_distro = Some("Ubuntu".into());
        spec.wsl_tool = Some(wsl_tool());
        let plan = start_plan(&spec, &text_request("hi")).expect("plan");
        let wslenv = plan
            .env
            .get(&OsString::from("WSLENV"))
            .expect("WSLENV")
            .to_string_lossy();
        assert!(wslenv.contains("CLAUDE_CONFIG_DIR/p"), "{wslenv}");
        assert!(wslenv.contains("CLAUDE_CODE_OAUTH_TOKEN"), "{wslenv}");
        assert!(wslenv.contains("HTTP_PROXY"), "{wslenv}");
        let config = plan
            .env
            .get(&OsString::from("CLAUDE_CONFIG_DIR"))
            .expect("config dir")
            .to_string_lossy();
        assert!(
            config.contains(r"C:\tmp\acct") || config.contains("/tmp/acct"),
            "{config}"
        );
        assert_eq!(
            plan.env.get(&OsString::from("HTTP_PROXY")),
            Some(&OsString::from("http://172.20.0.1:8080"))
        );
        assert_eq!(
            plan.env.get(&OsString::from("CLAUDE_CODE_OAUTH_TOKEN")),
            Some(&OsString::from("secret-token"))
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

    fn mcp_config_path() -> PathBuf {
        PathBuf::from(if cfg!(windows) {
            r"C:\tmp\acct\mcp.json"
        } else {
            "/tmp/acct/mcp.json"
        })
    }

    fn tools_spec() -> StartSpec {
        let mut spec = spec();
        spec.mcp_config = Some(mcp_config_path());
        spec
    }

    fn tools_request() -> Request {
        request_from(json!({
            "model": "claude-sonnet-4-5",
            "messages": [{"role": "user", "content": [{"type": "text", "text": "hi"}]}],
            "tools": [{"name": "lookup", "input_schema": {"type": "object"}}]
        }))
    }

    fn lookup<'a>(pairs: &'a [(&str, &str)]) -> impl Fn(&str) -> Option<OsString> + 'a {
        move |key| {
            pairs
                .iter()
                .find(|(name, _)| *name == key)
                .map(|(_, value)| OsString::from(*value))
        }
    }

    fn env_value(plan: &super::SpawnPlan, key: &str) -> String {
        plan.env
            .get(&OsString::from(key))
            .map(|value| value.to_string_lossy().into_owned())
            .unwrap_or_else(|| "unset".to_owned())
    }

    fn tool_wait(plan: &super::SpawnPlan) -> (String, String) {
        (
            env_value(plan, "CLAUDE_CODE_MCP_AUTO_BACKGROUND_MS"),
            env_value(plan, "CLAUDE_CODE_MCP_TOOL_IDLE_TIMEOUT"),
        )
    }

    /// A run that hands the client tools waits with both limits off.
    #[test]
    fn d233_tools_run_sets_both_tool_wait_vars_to_zero() {
        let plan =
            super::start_plan_with(&tools_spec(), &tools_request(), lookup(&[])).expect("plan");
        let args: Vec<String> = plan
            .argv
            .iter()
            .map(|arg| arg.to_string_lossy().into_owned())
            .collect();
        assert!(args.contains(&"--mcp-config".to_owned()), "{args:?}");
        assert!(args.contains(&"--strict-mcp-config".to_owned()), "{args:?}");
        assert_eq!(tool_wait(&plan), ("0".to_owned(), "0".to_owned()));
        assert!(
            !plan.env.contains_key(&OsString::from("WSLENV")),
            "a native plan does not invent WSLENV"
        );
    }

    /// A value already set in the process environment is copied through.
    /// An empty value counts as set, matching the reference prefix check.
    #[test]
    fn d233_user_set_tool_wait_value_is_kept() {
        assert_kept(
            &[("CLAUDE_CODE_MCP_AUTO_BACKGROUND_MS", "600000")],
            "600000",
            "0",
        );
        assert_kept(
            &[
                ("CLAUDE_CODE_MCP_AUTO_BACKGROUND_MS", "600000"),
                ("CLAUDE_CODE_MCP_TOOL_IDLE_TIMEOUT", "15"),
            ],
            "600000",
            "15",
        );
        assert_kept(&[("CLAUDE_CODE_MCP_TOOL_IDLE_TIMEOUT", "")], "0", "");
    }

    fn assert_kept(pairs: &[(&str, &str)], background: &str, idle: &str) {
        let plan =
            super::start_plan_with(&tools_spec(), &tools_request(), lookup(pairs)).expect("plan");
        assert_eq!(
            tool_wait(&plan),
            (background.to_owned(), idle.to_owned()),
            "{pairs:?}"
        );
    }

    /// The reference sets the pair on every bridge run, not only when the
    /// turn offers tools. One-off runs never call `start_plan`.
    #[test]
    fn d233_run_without_tools_matches_reference() {
        let plan = super::start_plan_with(&spec(), &text_request("hi"), lookup(&[])).expect("plan");
        let args: Vec<String> = plan
            .argv
            .iter()
            .map(|arg| arg.to_string_lossy().into_owned())
            .collect();
        assert!(!args.iter().any(|arg| arg == "--mcp-config"), "{args:?}");
        assert!(!has_tools(&text_request("hi")));
        assert_eq!(tool_wait(&plan), ("0".to_owned(), "0".to_owned()));

        let mut wsl = spec();
        wsl.wsl_distro = Some("Ubuntu".into());
        wsl.wsl_tool = Some(wsl_tool());
        let crossed =
            super::start_plan_with(&wsl, &text_request("hi"), lookup(&[])).expect("wsl plan");
        assert_eq!(tool_wait(&crossed), ("0".to_owned(), "0".to_owned()));
        let wslenv = env_value(&crossed, "WSLENV");
        assert!(
            wslenv.contains("CLAUDE_CODE_MCP_AUTO_BACKGROUND_MS"),
            "{wslenv}"
        );
        assert!(
            wslenv.contains("CLAUDE_CODE_MCP_TOOL_IDLE_TIMEOUT"),
            "{wslenv}"
        );
    }

    /// A WSL plan that offers tools forwards both names and keeps a user value.
    #[test]
    fn d233_wsl_plan_forwards_both_tool_wait_vars() {
        let mut spec = tools_spec();
        spec.wsl_distro = Some("Ubuntu".into());
        spec.wsl_tool = Some(wsl_tool());
        let plan = super::start_plan_with(&spec, &tools_request(), lookup(&[])).expect("plan");
        assert_eq!(tool_wait(&plan), ("0".to_owned(), "0".to_owned()));
        let wslenv = env_value(&plan, "WSLENV");
        assert!(
            wslenv.contains("CLAUDE_CODE_MCP_AUTO_BACKGROUND_MS"),
            "{wslenv}"
        );
        assert!(
            wslenv.contains("CLAUDE_CODE_MCP_TOOL_IDLE_TIMEOUT"),
            "{wslenv}"
        );

        let kept = super::start_plan_with(
            &spec,
            &tools_request(),
            lookup(&[("CLAUDE_CODE_MCP_AUTO_BACKGROUND_MS", "600000")]),
        )
        .expect("user plan");
        assert_eq!(tool_wait(&kept), ("600000".to_owned(), "0".to_owned()));
        let kept_wslenv = env_value(&kept, "WSLENV");
        assert!(
            kept_wslenv.contains("CLAUDE_CODE_MCP_AUTO_BACKGROUND_MS"),
            "{kept_wslenv}"
        );
        assert!(
            kept_wslenv.contains("CLAUDE_CODE_MCP_TOOL_IDLE_TIMEOUT"),
            "{kept_wslenv}"
        );
    }

    #[test]
    fn d254_a_rejected_reset_is_carried_on_the_result_error() {
        let resets_at = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .expect("clock")
            .as_secs()
            + 2 * 60 * 60;
        let stdout = format!(
            r#"{{"type":"rate_limit_event","status":"rejected","rate_limit_info":{{"status":"rejected","rateLimitType":"seven_day","resetsAt":{resets_at},"utilization":1}}}}
{{"type":"result","subtype":"error","is_error":true,"result":"You've hit your weekly limit","api_error_status":429,"error":"rate_limit"}}
"#
        );
        let error = events_from_stdout(&stdout).expect_err("quota");
        assert_eq!(error.kind(), PluginErrorKind::Quota);
        let after = error.reset_after().expect("reset");
        assert!(after > std::time::Duration::from_secs(60 * 60));
        assert!(after < std::time::Duration::from_secs(3 * 60 * 60));
        assert!(!error.scopes_model());
    }

    #[test]
    fn d254_a_later_unrejected_rate_limit_drops_the_saved_reset() {
        let resets_at = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .expect("clock")
            .as_secs()
            + 2 * 60 * 60;
        let stdout = format!(
            r#"{{"type":"rate_limit_event","status":"rejected","rate_limit_info":{{"status":"rejected","rateLimitType":"seven_day","resetsAt":{resets_at},"utilization":1}}}}
{{"type":"rate_limit_event","status":"allowed","rate_limit_info":{{"status":"allowed","rateLimitType":"five_hour","resetsAt":{resets_at},"utilization":0.1}}}}
{{"type":"result","subtype":"error","is_error":true,"result":"You've hit your weekly limit","api_error_status":429,"error":"rate_limit"}}
"#
        );
        let from_stdout = events_from_stdout(&stdout).expect_err("quota");
        let from_tools = events_allowing_tools(&stdout).expect_err("tools");
        for error in [from_stdout, from_tools] {
            assert_eq!(error.kind(), PluginErrorKind::Quota);
            assert!(error.reset_after().is_none());
        }
    }
}
