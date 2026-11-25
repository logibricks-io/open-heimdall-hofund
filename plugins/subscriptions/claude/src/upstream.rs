//! Claude Code upstream provider for text turns and parked tool continuations.

use std::ffi::OsString;
use std::path::PathBuf;
use std::sync::Arc;
use std::sync::atomic::AtomicBool;
use std::time::Duration;

use futures_util::stream;
use hofund_plugin_api::{
    AccountId, Completion, HostContext, PluginError, ProbeQuery, ProbeReport, ProtocolId, Request,
    SpawnRequest, UpstreamProvider,
};
use hofund_plugin_sdk::oauth::SecretObject;

use crate::account::account_dir;
use crate::bridge::park::{self, Bridge};
use crate::bridge::run::{
    SpawnPlan, StartSpec, env_deny, events_allowing_tools, events_from_stdout, has_tools,
    reject_non_text, start_plan, stdin_from_said, stdin_from_since, workdir,
};
use crate::bridge::session::{CancelGuard, TurnStop, has_reply};
use crate::launcher;
use crate::proxy;
use crate::{COMMAND, creds, setup_token};

#[cfg(test)]
type TestRunner = Arc<dyn Fn(&SpawnPlan) -> Result<String, PluginError> + Send + Sync>;
#[cfg(test)]
type AsyncTestRunner = Arc<
    dyn for<'a> Fn(
            &'a SpawnPlan,
        ) -> futures_util::future::BoxFuture<'a, Result<String, PluginError>>
        + Send
        + Sync,
>;

/// Serves text turns and parked tool continuations through Claude Code.
pub(crate) struct ClaudeUpstream {
    account: AccountId,
    host: HostContext,
    command: Option<PathBuf>,
    bridge: Bridge,
    limits: crate::limits::Store,
    #[cfg(test)]
    runner: Option<TestRunner>,
    #[cfg(test)]
    async_runner: Option<AsyncTestRunner>,
}

impl ClaudeUpstream {
    /// A provider that spawns Claude Code through [`HostContext`].
    #[must_use]
    pub(crate) fn new(
        account: AccountId,
        host: HostContext,
        command: Option<PathBuf>,
        bridge: Bridge,
        limits: crate::limits::Store,
    ) -> Self {
        Self {
            account,
            host,
            command,
            bridge,
            limits,
            #[cfg(test)]
            runner: None,
            #[cfg(test)]
            async_runner: None,
        }
    }

    /// Overrides the spawn with a test runner (the fake Claude Code helper).
    #[cfg(test)]
    #[must_use]
    pub(crate) fn with_runner(mut self, runner: TestRunner) -> Self {
        self.runner = Some(runner);
        self
    }

    /// Overrides the spawn with an asynchronous fake runner.
    #[cfg(test)]
    #[must_use]
    pub(crate) fn with_async_runner(mut self, runner: AsyncTestRunner) -> Self {
        self.async_runner = Some(runner);
        self
    }

    fn program(&self) -> Result<PathBuf, PluginError> {
        if let Some(command) = &self.command {
            return Ok(command.clone());
        }
        hofund_plugin_sdk::process::find_program(COMMAND).ok_or_else(|| {
            PluginError::config(
                "Claude Code is not installed: `claude` is not on PATH; install it or set `command`",
            )
        })
    }

    async fn stdout_for(
        &self,
        plan: &SpawnPlan,
        spec: &StartSpec,
        run_id: Option<u64>,
    ) -> Result<String, PluginError> {
        #[cfg(test)]
        if let Some(runner) = &self.async_runner {
            let text = runner(plan).await?;
            self.note_stdout(run_id, &text);
            return Ok(text);
        }
        #[cfg(test)]
        if let Some(runner) = &self.runner {
            let text = runner(plan)?;
            self.note_stdout(run_id, &text);
            return Ok(text);
        }
        self.spawn_and_read(plan, workdir(spec), run_id).await
    }

    fn note_stdout(&self, run_id: Option<u64>, stdout: &str) {
        let Some(run_id) = run_id else {
            return;
        };
        let mut decoder = crate::bridge::decode::Decoder::new();
        let mut said = String::new();
        for line in stdout.lines() {
            let Some(crate::bridge::decode::Step::Events(batch)) = decoder.push_line(line) else {
                continue;
            };
            for event in batch {
                if let hofund_plugin_api::StreamEvent::TextDelta(delta) = event {
                    said.push_str(&delta.text);
                }
            }
        }
        self.bridge.note_said(run_id, &said);
    }

    async fn spawn_and_read(
        &self,
        plan: &SpawnPlan,
        cwd: &std::path::Path,
        run_id: Option<u64>,
    ) -> Result<String, PluginError> {
        let program = plan
            .argv
            .first()
            .cloned()
            .map(PathBuf::from)
            .ok_or_else(|| PluginError::config("Claude Code argv is empty"))?;
        let args = plan.argv.iter().skip(1).cloned().collect();
        let env = plan
            .env
            .iter()
            .map(|(k, v)| (k.clone(), v.clone()))
            .collect();
        let request = SpawnRequest::new(program, args, env, cwd)?;
        let child = self
            .host
            .spawn_process()
            .spawn_long_lived(&request, &env_deny())
            .await?;
        child
            .write_stdin(plan.stdin.as_bytes().to_vec().into())
            .await?;
        child.close_stdin().await?;
        let mut stdout = child.stdout_stream()?;
        let mut text = String::new();
        let mut pending = String::new();
        while let Some(chunk) = stdout.next_chunk().await {
            let chunk = chunk?;
            let piece = String::from_utf8_lossy(&chunk);
            text.push_str(&piece);
            pending.push_str(&piece);
            while let Some(at) = pending.find('\n') {
                let line = pending.drain(..=at).collect::<String>();
                self.note_stdout(run_id, line.trim_end_matches(['\n', '\r']));
            }
        }
        if !pending.is_empty() {
            self.note_stdout(run_id, &pending);
        }
        let status = child.wait().await.unwrap_or(-1);
        if status != 0 && !crate::bridge::failure::stream_finished(&text) {
            let stderr = String::from_utf8_lossy(&child.stderr_tail()).into_owned();
            let secrets: Vec<&str> = plan
                .env
                .values()
                .filter_map(|value| value.to_str())
                .filter(|value| value.chars().count() >= 8)
                .collect();
            return Err(crate::bridge::failure::process_ended(&stderr, &secrets));
        }
        Ok(text)
    }
}

fn token_for_run(blob: &SecretObject) -> Result<OsString, PluginError> {
    if let Some(token) = setup_token::token_from_blob(blob) {
        return Ok(OsString::from(token));
    }
    let Some(text) = blob.text(creds::CREDENTIALS) else {
        return Err(
            PluginError::auth("the stored Claude Code sign-in has no credentials")
                .without_cooldown(),
        );
    };
    creds::access_token(text)
        .map(OsString::from)
        .ok_or_else(|| {
            PluginError::auth("the stored Claude Code sign-in has no access token")
                .without_cooldown()
        })
}

fn running_as_root() -> bool {
    #[cfg(unix)]
    {
        std::fs::read_to_string("/proc/self/status")
            .ok()
            .and_then(|text| {
                text.lines()
                    .find(|line| line.starts_with("Uid:"))
                    .and_then(|line| line.split_whitespace().nth(1).map(|uid| uid == "0"))
            })
            .unwrap_or(false)
    }
    #[cfg(not(unix))]
    {
        false
    }
}

#[hofund_plugin_sdk::async_trait]
impl UpstreamProvider for ClaudeUpstream {
    async fn execute(
        &self,
        mut request: Request,
        inbound: ProtocolId,
    ) -> Result<Completion, PluginError> {
        reject_non_text(&request)?;
        if let Some(error) =
            crate::said_out::block_request(&self.host, &self.account, &request.model)
        {
            return Err(error);
        }
        self.bridge.use_ask(self.host.ask().clone());
        let advisor_model = crate::bridge::advisor::prepare(&mut request, self.host.ask());
        let guard = CancelGuard::arm(
            self.bridge.clone(),
            request.clone(),
            self.account.clone(),
            None,
            Arc::new(AtomicBool::new(false)),
        );
        guard.rewatch(request.clone()).await?;
        let result = self
            .execute_guarded(request, inbound, advisor_model, &guard)
            .await;
        guard.defuse();
        result
    }

    fn stopping(&self) {
        self.bridge.mark_stopping();
    }

    async fn probe(&self, query: &ProbeQuery) -> Result<ProbeReport, PluginError> {
        crate::account::ask(
            &self.host,
            self.command.as_ref(),
            &self.account,
            query.model(),
        )
        .await?;
        Ok(ProbeReport::new("ok"))
    }
}

impl ClaudeUpstream {
    async fn execute_guarded(
        &self,
        mut request: Request,
        _inbound: ProtocolId,
        advisor_model: Option<String>,
        guard: &CancelGuard,
    ) -> Result<Completion, PluginError> {
        if let Some(found) = self.try_match(&request) {
            guard.bind(found.run_id);
            self.bridge.begin_turn(found.run_id, &request, None);
            self.bridge.heard(found.run_id, &request.messages);
            return self.continue_parked(found, &request, advisor_model).await;
        }
        if let Some(resume) = self.bridge.resume(&request, &self.account) {
            guard.bind(resume.run_id);
            self.bridge.begin_turn(resume.run_id, &request, None);
            self.bridge.heard(resume.run_id, &request.messages);
            return self.continue_resume(resume, &request, advisor_model).await;
        }
        // Stage D only on a resume miss.
        crate::bridge::flags::apply_stage_d(&mut request);
        guard.rewatch(request.clone()).await?;
        let unshelved = self
            .bridge
            .unshelve(&request, &self.account)
            .or_else(|| self.bridge.retry_branch(&request, &self.account));
        self.bridge.retire(&self.account, &request);
        let room = self
            .bridge
            .room
            .take(|| self.bridge.evict_idle_for_room())?;
        let Some(secret) = self.host.accounts().secret(&self.account).await? else {
            return Err(
                PluginError::auth("no Claude Code sign-in is stored for this account")
                    .without_cooldown(),
            );
        };
        let blob = SecretObject::parse(&secret)?;
        let login_version = blob.number(creds::VERSION);
        if blob.flag(creds::LAPSED) {
            return Err(PluginError::auth(creds::LAPSED_REASON).without_cooldown());
        }
        let token = token_for_run(&blob)?;
        let config_dir = account_dir(&self.host, &self.account)?;
        let command =
            launcher::ClaudeCommand::resolve(&self.host, self.command.as_ref(), COMMAND).await?;
        let program = match &command {
            launcher::ClaudeCommand::Native(path) => path.clone(),
            launcher::ClaudeCommand::Wsl(tool) => std::path::PathBuf::from(&tool.path),
        };
        let proxy_pairs = command
            .proxied_extra(blob.text(proxy::PROXY_KEY), &[])?
            .into_iter()
            .filter(|(key, _)| {
                key.to_str()
                    .is_some_and(|name| name.to_ascii_uppercase().contains("PROXY"))
            })
            .collect();
        let mut spec = StartSpec {
            program,
            cwd: config_dir.clone(),
            model: request.model.clone(),
            token,
            config_dir: config_dir.clone(),
            sandbox: running_as_root(),
            proxy: proxy_pairs,
            wsl_distro: command.wsl_distro().map(str::to_owned),
            wsl_tool: match &command {
                launcher::ClaudeCommand::Wsl(tool) => Some(tool.clone()),
                launcher::ClaudeCommand::Native(_) => None,
            },
            mcp_config: None,
            resume_session: unshelved.as_ref().map(|(saved, _)| saved.session.clone()),
        };
        let mut callback_guard = None;
        let mut run_id = None;
        let tool_names: Vec<String> = request.tools.iter().map(|tool| tool.name.clone()).collect();
        if has_tools(&request) || has_reply(&request.messages) || unshelved.is_some() {
            let id = self
                .bridge
                .park_run(self.account.clone(), tool_names.clone());
            self.bridge
                .ensure_session_run(id, &self.account, tool_names);
            self.bridge.set_login_version(id, login_version);
            self.bridge.heard(id, &request.messages);
            self.bridge
                .set_effort(id, crate::bridge::flags::from_request(&request).effort);
            self.bridge.set_room_guard(id, room);
            self.bridge.begin_reply(id, &request, advisor_model.clone());
            let persists = spec.wsl_distro.is_none();
            self.bridge.begin_turn(id, &request, Some(persists));
            if persists && let Some((saved, since)) = &unshelved {
                self.bridge
                    .note_started_from(id, saved, &request, since.len());
            }
            guard.bind(id);
            run_id = Some(id);
            let token = park::new_callback_token()?;
            let handler = self.bridge.callback_handler(id, request.clone());
            match self.host.bridge_callback().register(&token, handler) {
                Ok(guard) => {
                    let mcp_dir = config_dir.join(format!("bridge-mcp-{id}"));
                    if let Ok(helper) = self.host.bridge_mcp_helper() {
                        let layout = park::write_mcp_layout(
                            &mcp_dir,
                            guard.callback_url(),
                            &request,
                            &helper,
                        )?;
                        spec.mcp_config = Some(layout.config_path);
                    }
                    self.bridge.set_callback_guard(id, guard);
                    callback_guard = Some(());
                }
                Err(error) if error.kind() == hofund_plugin_api::PluginErrorKind::Unsupported => {
                    // Test hosts without a listener still park on the fake runner.
                }
                Err(error) => return Err(error),
            }
        }
        let mut plan = start_plan(&spec, &request)?;
        if let Some((_, since)) = unshelved {
            plan.stdin = stdin_from_since(&since);
        }
        let stdout = self.stdout_for(&plan, &spec, run_id).await?;
        let _ = callback_guard;
        self.finish_stdout(stdout, run_id, has_tools(&request), &request, login_version)
            .await
    }
}

impl ClaudeUpstream {
    fn try_match(&self, request: &Request) -> Option<park::Match> {
        let has_results = request.messages.iter().any(|message| {
            message
                .content
                .iter()
                .any(|part| matches!(part, hofund_plugin_api::ContentPart::ToolResult(_)))
        });
        if !has_results {
            return None;
        }
        let found = self.bridge.match_request(request, &self.account)?;
        if !self.bridge.claim_resume(found.run_id) {
            return None;
        }
        if !self.bridge.follows(found.run_id, &request.messages) {
            self.bridge.released(found.run_id);
            return None;
        }
        Some(found)
    }

    async fn continue_parked(
        &self,
        found: park::Match,
        request: &Request,
        advisor_model: Option<String>,
    ) -> Result<Completion, PluginError> {
        self.bridge
            .begin_reply(found.run_id, request, advisor_model);
        let said = if found.said.is_empty() {
            self.bridge.take_said(found.run_id)
        } else {
            found.said
        };
        let _remapped = found.remapped;
        if let Some(stdout) = self.bridge.take_continuation(found.run_id) {
            return self
                .finish_stdout(
                    stdout,
                    Some(found.run_id),
                    true,
                    request,
                    self.bridge.login_version(found.run_id),
                )
                .await;
        }
        // Fake-runner tests (and any runner hook) serve the next script here.
        let config_dir = account_dir(&self.host, &self.account)?;
        let spec = StartSpec {
            program: self
                .program()
                .unwrap_or_else(|_| PathBuf::from("/opt/claude/bin/claude")),
            cwd: config_dir.clone(),
            model: request.model.clone(),
            token: OsString::from(""),
            config_dir,
            sandbox: false,
            proxy: Vec::new(),
            wsl_distro: None,
            wsl_tool: None,
            mcp_config: None,
            resume_session: None,
        };
        let mut plan = start_plan(&spec, request)?;
        if !said.is_empty() {
            plan.stdin = stdin_from_said(&said);
        }
        let stdout = self.stdout_for(&plan, &spec, Some(found.run_id)).await?;
        self.finish_stdout(
            stdout,
            Some(found.run_id),
            true,
            request,
            self.bridge.login_version(found.run_id),
        )
        .await
    }

    async fn continue_resume(
        &self,
        resume: crate::bridge::session::Resume,
        request: &Request,
        advisor_model: Option<String>,
    ) -> Result<Completion, PluginError> {
        self.bridge
            .begin_reply(resume.run_id, request, advisor_model);
        let config_dir = account_dir(&self.host, &self.account)?;
        let spec = StartSpec {
            program: self
                .program()
                .unwrap_or_else(|_| PathBuf::from("/opt/claude/bin/claude")),
            cwd: config_dir.clone(),
            model: request.model.clone(),
            token: OsString::from(""),
            config_dir,
            sandbox: false,
            proxy: Vec::new(),
            wsl_distro: None,
            wsl_tool: None,
            mcp_config: None,
            resume_session: None,
        };
        let mut plan = start_plan(&spec, request)?;
        plan.stdin = stdin_from_since(&resume.since);
        let wanted = crate::bridge::flags::from_request(request).effort;
        let current = self.bridge.effort(resume.run_id);
        if wanted != current {
            let prefix = crate::bridge::flags::set_effort_line(
                &wanted,
                &crate::bridge::flags::set_effort_request_id(),
            );
            plan.stdin = format!("{prefix}{}", plan.stdin);
            self.bridge.set_effort(resume.run_id, wanted);
        }
        let stdout = self.stdout_for(&plan, &spec, Some(resume.run_id)).await?;
        self.finish_stdout(
            stdout,
            Some(resume.run_id),
            has_tools(request),
            request,
            self.bridge.login_version(resume.run_id),
        )
        .await
    }

    async fn finish_stdout(
        &self,
        stdout: String,
        run_id: Option<u64>,
        allow_tools: bool,
        request: &Request,
        login_version: Option<u64>,
    ) -> Result<Completion, PluginError> {
        self.observe_rate_limits(&stdout);
        let result = if !allow_tools {
            events_from_stdout(&stdout).map(|events| (events, Vec::new(), session_id_from(&stdout)))
        } else {
            events_allowing_tools(&stdout)
                .map(|(events, calls)| (events, calls, session_id_from(&stdout)))
        };
        let (events, calls, session) = match result {
            Ok(decoded) => decoded,
            Err(error) => {
                crate::said_out::note_bridge(
                    &self.host,
                    &self.account,
                    &request.model,
                    &error,
                    self.host.clock().now(),
                );
                if let Some(run_id) = run_id {
                    if error.kind() == hofund_plugin_api::PluginErrorKind::Quota {
                        self.bridge.keep_for_reset(run_id, request, &self.account);
                    }
                    self.bridge
                        .ended(run_id, &self.account, request, "", TurnStop::Other, false);
                }
                if crate::bridge::failure::is_sign_in_required(error.message()) {
                    let _ = self.mark_lapsed_if_current(login_version).await;
                }
                return Err(error);
            }
        };
        let events = if let Some(run_id) = run_id {
            let outcomes = self.bridge.take_outcomes(run_id);
            if outcomes.is_empty() {
                events
            } else {
                crate::bridge::advisor::splice(events, &outcomes)
            }
        } else {
            events
        };
        let armed = run_id.is_some_and(|id| self.bridge.advisor_armed(id));
        let calls: Vec<_> = calls
            .into_iter()
            .filter(|(_, name)| !(armed && crate::bridge::advisor::is_advisor_name(name)))
            .collect();
        if let Some(run_id) = run_id {
            if let Some(session) = session {
                let paths = account_dir(&self.host, &self.account)
                    .map(|dir| crate::bridge::cut::session_dirs(&dir, &session))
                    .unwrap_or_default();
                self.bridge.set_session(run_id, session, paths);
            }
            if calls.is_empty()
                && let Some(message_id) = crate::bridge::cut::completed_reply_id(&stdout)
            {
                self.bridge.note_completed_reply(run_id, &message_id);
            }
            for (id, name) in &calls {
                self.bridge.register_call(run_id, id.clone(), name.clone());
            }
            let said = assistant_text(&events);
            let stop = if calls.is_empty() {
                TurnStop::Stop
            } else {
                TurnStop::Tool
            };
            self.bridge
                .ended(run_id, &self.account, request, &said, stop, true);
        } else if !calls.is_empty() {
            return Err(PluginError::unsupported(
                "tool calls through Claude Code are not served yet",
            ));
        }
        Ok(Completion::Stream(Box::pin(stream::iter(
            events.into_iter().map(Ok),
        ))))
    }

    async fn mark_lapsed_if_current(&self, captured: Option<u64>) -> Result<(), PluginError> {
        let lease = self
            .host
            .accounts()
            .lease(&self.account, Duration::from_secs(30))
            .await?;
        let blob = SecretObject::parse(lease.secret())?;
        if !crate::bridge::failure::should_mark_lapsed(captured, blob.number(creds::VERSION)) {
            return Ok(());
        }
        if blob.flag(creds::LAPSED) {
            return Ok(());
        }
        let lapsed = blob.with_flag(creds::LAPSED, true);
        lease.commit(None, lapsed.to_secret()).await
    }

    fn observe_rate_limits(&self, stdout: &str) {
        let now = self.host.clock().now();
        let mut decoder = crate::bridge::decode::Decoder::new();
        for line in stdout.lines() {
            let Some(crate::bridge::decode::Step::RateLimit(info)) = decoder.push_line(line) else {
                continue;
            };
            self.limits.note(&self.account, &info, now);
            crate::limits::observe_from_info(&self.host, &info);
        }
    }
}

fn assistant_text(events: &[hofund_plugin_api::StreamEvent]) -> String {
    events
        .iter()
        .filter_map(|event| match event {
            hofund_plugin_api::StreamEvent::TextDelta(delta) => Some(delta.text.as_str()),
            _ => None,
        })
        .collect()
}

fn session_id_from(stdout: &str) -> Option<String> {
    let mut decoder = crate::bridge::decode::Decoder::new();
    let mut session = None;
    for line in stdout.lines() {
        if let Some(crate::bridge::decode::Step::Session(id)) = decoder.push_line(line) {
            session = Some(id);
        }
    }
    session
}

#[cfg(test)]
mod tests {
    use super::ClaudeUpstream;
    use crate::bridge::runner::FakeRunner;
    use crate::creds;
    use futures_util::StreamExt;
    use hofund_kit_subscription::{TestHost, block_on};
    use hofund_plugin_api::{
        AccountId, Completion, ContentPart, Message, PluginError, PluginId, PluginKind, ProtocolId,
        Request, Role, StreamEvent, UpstreamProvider,
    };
    use hofund_plugin_sdk::oauth::SecretObject;
    use serde_json::json;
    use std::path::PathBuf;
    #[cfg(test)]
    use std::sync::Arc;

    const CREDENTIALS: &str = r#"{"claudeAiOauth":{"accessToken":"secret-token","refreshToken":"rt-1","expiresAt":1900000000000,"scopes":["user:inference"],"subscriptionType":"max"}}"#;
    const MESSAGE_DELTA: &str = include_str!("bridge/fixtures/stream/message_delta.jsonl");
    const TOOL_USE: &str = include_str!("bridge/fixtures/stream/tool_use.jsonl");

    #[cfg(not(windows))]
    fn program() -> PathBuf {
        PathBuf::from("/opt/claude/bin/claude")
    }

    #[cfg(windows)]
    fn program() -> PathBuf {
        PathBuf::from(r"C:\claude\bin\claude.exe")
    }

    fn text_request() -> Request {
        let mut message = Message::new(Role::User);
        message.content = vec![ContentPart::text("Hello")];
        Request::new("claude-sonnet-4-5").with_messages(vec![message])
    }

    fn collect_events(completion: Completion) -> Vec<StreamEvent> {
        let Completion::Stream(mut stream) = completion.peel() else {
            panic!("expected a stream");
        };
        block_on(async {
            let mut events = Vec::new();
            while let Some(item) = stream.next().await {
                events.push(item.expect("event"));
            }
            events
        })
    }

    fn seed_account(host: &TestHost, id: &AccountId) {
        let blob = SecretObject::new()
            .with_number(creds::VERSION, 1)
            .with_text(creds::CREDENTIALS, CREDENTIALS);
        let account = host
            .context()
            .accounts()
            .account(id.clone(), "ada@example.test")
            .expect("account");
        block_on(host.context().accounts().upsert(account, blob.to_secret())).expect("upsert");
    }

    #[test]
    fn execute_parks_a_tool_turn_and_continues_on_the_result() {
        use std::sync::atomic::{AtomicUsize, Ordering};
        let dir = tempfile::tempdir().expect("dir");
        let host = TestHost::open_for(
            PluginKind::Subscription,
            PluginId::from_static("claude"),
            dir.path(),
        );
        let id = AccountId::from_static("a-00000000000000aa");
        seed_account(&host, &id);
        let calls = Arc::new(AtomicUsize::new(0));
        let counter = Arc::clone(&calls);
        let bridge = crate::bridge::Bridge::new();
        let provider = ClaudeUpstream::new(
            id.clone(),
            host.context().clone(),
            Some(program()),
            bridge.clone(),
            crate::limits::Store::new(),
        )
        .with_runner(Arc::new(move |_plan| {
            let n = counter.fetch_add(1, Ordering::SeqCst);
            if n == 0 {
                Ok(TOOL_USE.to_string())
            } else {
                Ok(MESSAGE_DELTA.to_string())
            }
        }));

        let request = serde_json::from_value::<Request>(json!({
            "model": "claude-sonnet-4-5",
            "messages": [{"role": "user", "content": [{"type": "text", "text": "list files"}]}],
            "tools": [{"name": "Bash", "input_schema": {"type": "object"}}]
        }))
        .expect("request");
        let completion = block_on(provider.execute(request, ProtocolId::AnthropicMessages))
            .expect("parked turn");
        let events = collect_events(completion);
        assert!(
            events.iter().any(|event| matches!(
                event,
                StreamEvent::ContentBlockStart(_) | StreamEvent::ToolArgumentsDelta(_)
            )),
            "expected tool-call IR events, got {events:?}"
        );

        let follow = serde_json::from_value::<Request>(json!({
            "model": "claude-sonnet-4-5",
            "messages": [
                {"role": "user", "content": [{"type": "text", "text": "list files"}]},
                {
                    "role": "user",
                    "content": [
                        {"type": "text", "text": "thanks"},
                        {
                            "type": "tool_result",
                            "id": "toolu_1",
                            "content": [{"type": "text", "text": "ok"}]
                        }
                    ]
                }
            ],
            "tools": [{"name": "Bash", "input_schema": {"type": "object"}}]
        }))
        .expect("follow");
        let completion = block_on(provider.execute(follow, ProtocolId::AnthropicMessages))
            .expect("continuation");
        let events = collect_events(completion);
        let text: String = events
            .iter()
            .filter_map(|event| match event {
                StreamEvent::TextDelta(delta) => Some(delta.text.as_str()),
                _ => None,
            })
            .collect();
        assert_eq!(text, "Hello world");
        assert_eq!(calls.load(Ordering::SeqCst), 2);
    }

    #[test]
    fn d227_continuation_clears_the_previous_advisor_round() {
        let dir = tempfile::tempdir().expect("dir");
        let host = TestHost::open_for(
            PluginKind::Subscription,
            PluginId::from_static("claude"),
            dir.path(),
        );
        let id = AccountId::from_static("a-00000000000000aa");
        seed_account(&host, &id);
        let bridge = crate::bridge::Bridge::new();
        let provider = ClaudeUpstream::new(
            id.clone(),
            host.context().clone(),
            Some(program()),
            bridge.clone(),
            crate::limits::Store::new(),
        )
        .with_runner(Arc::new(|_plan| Ok(TOOL_USE.to_string())));
        let request = serde_json::from_value::<Request>(json!({
            "model": "claude-sonnet-4-5",
            "messages": [{"role": "user", "content": [{"type": "text", "text": "list files"}]}],
            "tools": [{"name": "Bash", "input_schema": {"type": "object"}}]
        }))
        .expect("request");
        block_on(provider.execute(request, ProtocolId::AnthropicMessages)).expect("parked");
        let run = bridge
            .testing_run_ids()
            .into_iter()
            .next()
            .expect("parked run");
        bridge.testing_force_advisor(run, 3);
        assert!(bridge.advisor_armed(run));
        let follow = serde_json::from_value::<Request>(json!({
            "model": "claude-sonnet-4-5",
            "messages": [
                {"role": "user", "content": [{"type": "text", "text": "list files"}]},
                {
                    "role": "user",
                    "content": [{
                        "type": "tool_result",
                        "id": "toolu_1",
                        "content": [{"type": "text", "text": "ok"}]
                    }]
                }
            ],
            "tools": [{"name": "Bash", "input_schema": {"type": "object"}}]
        }))
        .expect("follow");
        block_on(provider.execute(follow, ProtocolId::AnthropicMessages)).expect("continuation");
        assert!(
            !bridge.advisor_armed(run),
            "a parked continuation must start a new advisor round"
        );
    }

    #[test]
    fn execute_serves_a_text_only_fake_runner_turn() {
        let dir = tempfile::tempdir().expect("dir");
        let host = TestHost::open_for(
            PluginKind::Subscription,
            PluginId::from_static("claude"),
            dir.path(),
        );
        let id = AccountId::from_static("a-00000000000000aa");
        let blob = SecretObject::new()
            .with_number(creds::VERSION, 1)
            .with_text(creds::CREDENTIALS, CREDENTIALS);
        let account = host
            .context()
            .accounts()
            .account(id.clone(), "ada@example.test")
            .expect("account");
        block_on(host.context().accounts().upsert(account, blob.to_secret())).expect("upsert");

        let config_dir = host
            .context()
            .dirs()
            .state_dir()
            .expect("state")
            .join("accounts")
            .join(id.as_str());
        let runner = FakeRunner::from_stdout(MESSAGE_DELTA)
            .expect_token("secret-token")
            .expect_config_dir(config_dir);
        let provider = ClaudeUpstream::new(
            id,
            host.context().clone(),
            Some(program()),
            crate::bridge::Bridge::new(),
            crate::limits::Store::new(),
        )
        .with_runner(Arc::new(move |plan| {
            runner
                .run(plan)
                .map(|record| record.stdout)
                .map_err(|error| PluginError::config(error.to_string()))
        }));

        let completion = block_on(provider.execute(text_request(), ProtocolId::AnthropicMessages))
            .expect("text-only turn");
        let events = collect_events(completion);
        let text: String = events
            .iter()
            .filter_map(|event| match event {
                StreamEvent::TextDelta(delta) => Some(delta.text.as_str()),
                _ => None,
            })
            .collect();
        assert_eq!(text, "Hello world");
    }

    #[test]
    fn r5_58_execute_notes_rate_limit_events() {
        use crate::bridge::decode::RATE_LIMIT;
        let dir = tempfile::tempdir().expect("dir");
        let host = TestHost::open_for(
            PluginKind::Subscription,
            PluginId::from_static("claude"),
            dir.path(),
        );
        let id = AccountId::from_static("a-00000000000000ab");
        seed_account(&host, &id);
        let limits = crate::limits::Store::new();
        let provider = ClaudeUpstream::new(
            id.clone(),
            host.context().clone(),
            Some(program()),
            crate::bridge::Bridge::new(),
            limits.clone(),
        )
        .with_runner(Arc::new(move |_plan| Ok(RATE_LIMIT.to_string())));
        let completion = block_on(provider.execute(text_request(), ProtocolId::AnthropicMessages))
            .expect("turn");
        let _ = collect_events(completion);
        let now = host.context().clock().now();
        let windows = limits.kept_windows(&id, now).expect("heard");
        assert_eq!(windows[0].name(), "5 hours");
        assert_eq!(windows[0].used(), 25);
    }

    #[test]
    fn execute_resumes_an_idle_fake_runner_turn_without_a_fresh_token() {
        use std::sync::Mutex;
        use std::sync::atomic::{AtomicUsize, Ordering};
        let dir = tempfile::tempdir().expect("dir");
        let host = TestHost::open_for(
            PluginKind::Subscription,
            PluginId::from_static("claude"),
            dir.path(),
        );
        let id = AccountId::from_static("a-00000000000000aa");
        seed_account(&host, &id);
        let calls = Arc::new(AtomicUsize::new(0));
        let stdins = Arc::new(Mutex::new(Vec::new()));
        let counter = Arc::clone(&calls);
        let captured = Arc::clone(&stdins);
        let bridge = crate::bridge::Bridge::new();
        let provider = ClaudeUpstream::new(
            id.clone(),
            host.context().clone(),
            Some(program()),
            bridge.clone(),
            crate::limits::Store::new(),
        )
        .with_runner(Arc::new(move |plan| {
            counter.fetch_add(1, Ordering::SeqCst);
            captured.lock().expect("stdin").push(plan.stdin.clone());
            Ok(MESSAGE_DELTA.to_string())
        }));

        let first = serde_json::from_value::<Request>(json!({
            "model": "claude-sonnet-4-5",
            "messages": [{"role": "user", "content": [{"type": "text", "text": "Hello"}]}],
            "tools": [{"name": "Bash", "input_schema": {"type": "object"}}]
        }))
        .expect("request");
        let completion =
            block_on(provider.execute(first, ProtocolId::AnthropicMessages)).expect("first turn");
        let events = collect_events(completion);
        let text: String = events
            .iter()
            .filter_map(|event| match event {
                StreamEvent::TextDelta(delta) => Some(delta.text.as_str()),
                _ => None,
            })
            .collect();
        assert_eq!(text, "Hello world");
        assert_eq!(bridge.idle_len(), 1);

        // Dropping the stored secret must not block a resume hit.
        block_on(host.context().accounts().remove(&id)).ok();

        let second = serde_json::from_value::<Request>(json!({
            "model": "claude-sonnet-4-5",
            "messages": [
                {"role": "user", "content": [{"type": "text", "text": "Hello"}]},
                {"role": "assistant", "content": [{"type": "text", "text": "Hello world"}]},
                {"role": "user", "content": [{"type": "text", "text": "Again"}]}
            ],
            "tools": [{"name": "Bash", "input_schema": {"type": "object"}}]
        }))
        .expect("second");
        let completion = block_on(provider.execute(second, ProtocolId::AnthropicMessages))
            .expect("resumed turn");
        let events = collect_events(completion);
        let text: String = events
            .iter()
            .filter_map(|event| match event {
                StreamEvent::TextDelta(delta) => Some(delta.text.as_str()),
                _ => None,
            })
            .collect();
        assert_eq!(text, "Hello world");
        assert_eq!(calls.load(Ordering::SeqCst), 2);
        let stdins = stdins.lock().expect("stdin");
        assert!(
            stdins[0].contains("Hello") && !stdins[0].contains("Again"),
            "first stdin: {}",
            stdins[0]
        );
        assert!(
            stdins[1].contains("Again") && !stdins[1].contains("Hello world"),
            "resume stdin: {}",
            stdins[1]
        );
    }

    #[test]
    fn execute_does_not_idle_a_one_off_text_turn() {
        let dir = tempfile::tempdir().expect("dir");
        let host = TestHost::open_for(
            PluginKind::Subscription,
            PluginId::from_static("claude"),
            dir.path(),
        );
        let id = AccountId::from_static("a-00000000000000aa");
        seed_account(&host, &id);
        let bridge = crate::bridge::Bridge::new();
        let provider = ClaudeUpstream::new(
            id,
            host.context().clone(),
            Some(program()),
            bridge.clone(),
            crate::limits::Store::new(),
        )
        .with_runner(Arc::new(move |_plan| Ok(MESSAGE_DELTA.to_string())));
        let completion = block_on(provider.execute(text_request(), ProtocolId::AnthropicMessages))
            .expect("one-off");
        let _ = collect_events(completion);
        assert_eq!(bridge.idle_len(), 0);
    }

    fn d253_fixture() -> (
        tempfile::TempDir,
        TestHost,
        AccountId,
        crate::bridge::Bridge,
    ) {
        let dir = tempfile::tempdir().expect("dir");
        let host = TestHost::open_for(
            PluginKind::Subscription,
            PluginId::from_static("claude"),
            dir.path(),
        );
        let id = AccountId::from_static("a-00000000000000aa");
        seed_account(&host, &id);
        (dir, host, id, crate::bridge::Bridge::new())
    }

    fn d253_provider(
        host: &TestHost,
        id: &AccountId,
        bridge: crate::bridge::Bridge,
    ) -> ClaudeUpstream {
        ClaudeUpstream::new(
            id.clone(),
            host.context().clone(),
            Some(program()),
            bridge,
            crate::limits::Store::new(),
        )
    }

    fn d253_request(messages: serde_json::Value, tools: serde_json::Value) -> Request {
        serde_json::from_value(json!({
            "model": "claude-sonnet-4-5",
            "messages": messages,
            "tools": tools,
        }))
        .expect("request")
    }

    fn d253_advisor_tool() -> hofund_plugin_api::Tool {
        let mut extensions = hofund_plugin_api::Extensions::new();
        extensions.insert("type", json!("advisor_20260301"));
        extensions.insert("model", json!("not-served"));
        hofund_plugin_api::Tool {
            name: "advisor".to_owned(),
            description: None,
            input_schema: json!({}),
            extensions,
        }
    }

    fn d253_seed_idle(
        host: &TestHost,
        id: &AccountId,
        bridge: &crate::bridge::Bridge,
        request: &Request,
        shelve: bool,
    ) {
        let config = crate::account::account_dir(host.context(), id).expect("account dir");
        let sessions = config.join("projects").join("project");
        std::fs::create_dir_all(&sessions).expect("sessions");
        std::fs::write(
            sessions.join("sess_text.jsonl"),
            "{\"type\":\"assistant\",\"uuid\":\"line-1\",\"sessionId\":\"sess_text\",\"message\":{\"id\":\"msg_text\",\"role\":\"assistant\",\"stop_reason\":\"end_turn\",\"content\":[{\"type\":\"text\",\"text\":\"Hello world\"}]}}\n",
        )
        .expect("transcript");
        let run = bridge.park_run(id.clone(), Vec::new());
        bridge.ensure_session_run(run, id, Vec::new());
        bridge.set_session(run, "sess_text", vec![sessions]);
        bridge.note_completed_reply(run, "msg_text");
        bridge.ended(
            run,
            id,
            request,
            "Hello world",
            crate::bridge::session::TurnStop::Stop,
            true,
        );
        if shelve {
            assert!(bridge.evict_idle_for_room());
        }
    }

    fn d253_drop_execute(provider: ClaudeUpstream, request: Request) {
        let entered = Arc::new(std::sync::atomic::AtomicBool::new(false));
        let runner_entered = Arc::clone(&entered);
        let provider = provider.with_async_runner(Arc::new(move |_plan| {
            let entered = Arc::clone(&runner_entered);
            Box::pin(async move {
                entered.store(true, std::sync::atomic::Ordering::Release);
                std::future::pending::<Result<String, PluginError>>().await
            })
        }));
        block_on(async {
            let mut call = Box::pin(provider.execute(request, ProtocolId::AnthropicMessages));
            futures_util::future::poll_fn(|cx| {
                match std::future::Future::poll(call.as_mut(), cx) {
                    std::task::Poll::Pending => {}
                    std::task::Poll::Ready(Err(error)) => {
                        panic!("execute failed before the runner: {error}")
                    }
                    std::task::Poll::Ready(Ok(_)) => panic!("execute completed before the runner"),
                }
                if entered.load(std::sync::atomic::Ordering::Acquire) {
                    std::task::Poll::Ready(())
                } else {
                    std::task::Poll::Pending
                }
            })
            .await;
            drop(call);
        });
    }

    fn d253_replacement_stdin(provider: ClaudeUpstream, request: Request) -> String {
        let stdin = Arc::new(std::sync::Mutex::new(String::new()));
        let captured = Arc::clone(&stdin);
        let provider = provider.with_runner(Arc::new(move |plan| {
            *captured.lock().expect("stdin") = plan.stdin.clone();
            Ok(MESSAGE_DELTA.to_string())
        }));
        let completion = block_on(provider.execute(request, ProtocolId::AnthropicMessages))
            .expect("replacement turn");
        let _ = collect_events(completion);
        stdin.lock().expect("stdin").clone()
    }

    #[test]
    fn d253_execute_drop_during_same_key_wait_wakes_without_a_poked_flag() {
        let (_dir, host, id, bridge) = d253_fixture();
        let seed = d253_request(
            json!([
                {"role": "user", "content": [{"type": "text", "text": "original"}]}
            ]),
            json!([{"name": "read", "input_schema": {"type": "object"}}]),
        );
        d253_seed_idle(&host, &id, &bridge, &seed, false);
        let request = d253_request(
            json!([
                {"role": "user", "content": [{"type": "text", "text": "original"}]},
                {"role": "assistant", "content": [{"type": "text", "text": "Hello world"}]},
                {"role": "user", "content": [{"type": "text", "text": "same turn"}]}
            ]),
            json!([{"name": "read", "input_schema": {"type": "object"}}]),
        );
        let key = crate::bridge::session::next_turn(id.as_str(), &request)
            .expect("turn")
            .0;
        let gone = Arc::new(std::sync::atomic::AtomicBool::new(false));
        let ticket = bridge
            .watch_cancel_blocking(&key, Arc::clone(&gone))
            .expect("watch")
            .expect("ticket");
        gone.store(true, std::sync::atomic::Ordering::Release);
        let before = bridge.cancel_generation();
        let provider =
            d253_provider(&host, &id, bridge.clone()).with_async_runner(Arc::new(|_plan| {
                Box::pin(async { std::future::pending::<Result<String, PluginError>>().await })
            }));
        let mut call = Box::pin(provider.execute(request, ProtocolId::AnthropicMessages));
        let waker = std::task::Waker::noop();
        let mut cx = std::task::Context::from_waker(waker);
        assert!(std::future::Future::poll(call.as_mut(), &mut cx).is_pending());
        drop(call);
        assert!(bridge.cancel_generation() > before);
        bridge.finish_cancel(ticket);
    }

    #[test]
    fn d253_advisor_mutation_cancel_shelves_and_next_turn_is_only_replacement() {
        let (_dir, host, id, bridge) = d253_fixture();
        let seed = d253_request(
            json!([
                {"role": "system", "content": [{"type": "text", "text": "rules"}]},
                {"role": "user", "content": [{"type": "text", "text": "original"}]}
            ]),
            json!([{"name": "read", "input_schema": {"type": "object"}}]),
        );
        d253_seed_idle(&host, &id, &bridge, &seed, false);
        let mut cancelled = d253_request(
            json!([
                {"role": "system", "content": [{"type": "text", "text": "rules"}]},
                {"role": "user", "content": [{"type": "text", "text": "original"}]},
                {"role": "assistant", "content": [{"type": "text", "text": "Hello world"}]},
                {"role": "user", "content": [{"type": "text", "text": "CANCEL ME"}]}
            ]),
            json!([{"name": "read", "input_schema": {"type": "object"}}]),
        );
        cancelled.tools.push(d253_advisor_tool());
        d253_drop_execute(d253_provider(&host, &id, bridge.clone()), cancelled);
        let mut replacement = d253_request(
            json!([
                {"role": "system", "content": [{"type": "text", "text": "rules"}]},
                {"role": "user", "content": [{"type": "text", "text": "original"}]},
                {"role": "assistant", "content": [{"type": "text", "text": "Hello world"}]},
                {"role": "user", "content": [{"type": "text", "text": "replacement"}]}
            ]),
            json!([{"name": "read", "input_schema": {"type": "object"}}]),
        );
        replacement.tools.push(d253_advisor_tool());
        let stdin = d253_replacement_stdin(d253_provider(&host, &id, bridge.clone()), replacement);
        assert!(stdin.contains("replacement"), "{stdin}");
        assert!(!stdin.contains("CANCEL ME"), "{stdin}");
        assert!(!stdin.contains("original"), "{stdin}");
    }

    #[test]
    fn d253_stage_d_mutation_cancel_shelves_and_next_turn_is_only_replacement() {
        let (_dir, host, id, bridge) = d253_fixture();
        let mut seed = d253_request(
            json!([
                {"role": "system", "content": [{"type": "text", "text": "<block>rules</block>"}]},
                {"role": "user", "content": [{"type": "text", "text": "<transcript>original"}]},
                {"role": "assistant", "content": [{"type": "text", "text": "earlier"}]}
            ]),
            json!([]),
        );
        seed.extensions
            .insert(crate::bridge::flags::BRIDGE_EFFORT, json!("low"));
        d253_seed_idle(&host, &id, &bridge, &seed, true);
        let cancelled = d253_request(
            json!([
                {"role": "system", "content": [{"type": "text", "text": "<block>rules</block>"}]},
                {"role": "user", "content": [{"type": "text", "text": "<transcript>original"}]},
                {"role": "assistant", "content": [{"type": "text", "text": "earlier"}]},
                {"role": "assistant", "content": [{"type": "text", "text": "Hello world"}]},
                {"role": "user", "content": [{"type": "text", "text": "CANCEL ME"}]}
            ]),
            json!([]),
        );
        d253_drop_execute(d253_provider(&host, &id, bridge.clone()), cancelled);
        let replacement = d253_request(
            json!([
                {"role": "system", "content": [{"type": "text", "text": "<block>rules</block>"}]},
                {"role": "user", "content": [{"type": "text", "text": "<transcript>original"}]},
                {"role": "assistant", "content": [{"type": "text", "text": "earlier"}]},
                {"role": "assistant", "content": [{"type": "text", "text": "Hello world"}]},
                {"role": "user", "content": [{"type": "text", "text": "replacement"}]}
            ]),
            json!([]),
        );
        let stdin = d253_replacement_stdin(d253_provider(&host, &id, bridge.clone()), replacement);
        assert!(stdin.contains("replacement"), "{stdin}");
        assert!(!stdin.contains("CANCEL ME"), "{stdin}");
        assert!(!stdin.contains("transcript"), "{stdin}");
    }

    fn stored_blob(host: &TestHost, id: &AccountId) -> SecretObject {
        let secret = block_on(host.context().accounts().secret(id))
            .expect("secret")
            .expect("present");
        SecretObject::parse(&secret).expect("blob")
    }

    const SIGN_IN_RESULT: &str = r#"{"type":"result","subtype":"error","is_error":true,"result":"OAuth token revoked","session_id":"sess_x"}"#;

    #[test]
    fn a_stream_sign_in_error_marks_the_account_lapsed() {
        let dir = tempfile::tempdir().expect("dir");
        let host = TestHost::open_for(
            PluginKind::Subscription,
            PluginId::from_static("claude"),
            dir.path(),
        );
        let id = AccountId::from_static("a-00000000000000aa");
        seed_account(&host, &id);
        let provider = ClaudeUpstream::new(
            id.clone(),
            host.context().clone(),
            Some(program()),
            crate::bridge::Bridge::new(),
            crate::limits::Store::new(),
        )
        .with_runner(Arc::new(move |_plan| Ok(SIGN_IN_RESULT.to_string())));
        let error = match block_on(provider.execute(text_request(), ProtocolId::AnthropicMessages))
        {
            Err(error) => error,
            Ok(_) => panic!("expected a sign-in failure"),
        };
        assert_eq!(error.status(), Some(502));
        assert!(!error.cools());
        assert!(stored_blob(&host, &id).flag(creds::LAPSED));
    }

    #[test]
    fn a_rotated_sign_in_is_not_marked_lapsed() {
        let dir = tempfile::tempdir().expect("dir");
        let host = TestHost::open_for(
            PluginKind::Subscription,
            PluginId::from_static("claude"),
            dir.path(),
        );
        let id = AccountId::from_static("a-00000000000000aa");
        seed_account(&host, &id);
        let ctx = host.context().clone();
        let owner = id.clone();
        let provider = ClaudeUpstream::new(
            id.clone(),
            host.context().clone(),
            Some(program()),
            crate::bridge::Bridge::new(),
            crate::limits::Store::new(),
        )
        .with_runner(Arc::new(move |_plan| {
            let ctx = ctx.clone();
            let owner = owner.clone();
            std::thread::spawn(move || {
                let secret = block_on(ctx.accounts().secret(&owner))
                    .expect("secret")
                    .expect("present");
                let blob = SecretObject::parse(&secret)
                    .expect("blob")
                    .with_number(creds::VERSION, 2);
                let account = ctx
                    .accounts()
                    .account(owner, "ada@example.test")
                    .expect("account");
                block_on(ctx.accounts().upsert(account, blob.to_secret())).expect("rotate");
            })
            .join()
            .expect("rotate thread");
            Ok(SIGN_IN_RESULT.to_string())
        }));
        let error = match block_on(provider.execute(text_request(), ProtocolId::AnthropicMessages))
        {
            Err(error) => error,
            Ok(_) => panic!("expected a sign-in failure"),
        };
        assert_eq!(error.status(), Some(502));
        let blob = stored_blob(&host, &id);
        assert!(!blob.flag(creds::LAPSED));
        assert_eq!(blob.number(creds::VERSION), Some(2));
    }
}
