//! Claude Code upstream provider for a text-only turn.

use std::ffi::OsString;
use std::path::PathBuf;
#[cfg(test)]
use std::sync::Arc;

use futures_util::stream;
use hofund_plugin_api::{
    AccountId, Completion, HostContext, PluginError, ProtocolId, Request, SpawnRequest,
    UpstreamProvider,
};
use hofund_plugin_sdk::oauth::SecretObject;

use crate::account::account_dir;
use crate::bridge::run::{
    SpawnPlan, StartSpec, env_deny, events_from_stdout, reject_non_text, start_plan, workdir,
};
use crate::{COMMAND, creds, setup_token};

#[cfg(test)]
type TestRunner = Arc<dyn Fn(&SpawnPlan) -> Result<String, PluginError> + Send + Sync>;

/// Serves text-only turns through the local Claude Code binary.
pub(crate) struct ClaudeUpstream {
    account: AccountId,
    host: HostContext,
    command: Option<PathBuf>,
    #[cfg(test)]
    runner: Option<TestRunner>,
}

impl ClaudeUpstream {
    /// A provider that spawns Claude Code through [`HostContext`].
    #[must_use]
    pub(crate) fn new(account: AccountId, host: HostContext, command: Option<PathBuf>) -> Self {
        Self {
            account,
            host,
            command,
            #[cfg(test)]
            runner: None,
        }
    }

    /// Overrides the spawn with a test runner (the fake Claude Code helper).
    #[cfg(test)]
    #[must_use]
    pub(crate) fn with_runner(mut self, runner: TestRunner) -> Self {
        self.runner = Some(runner);
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

    async fn stdout_for(&self, plan: &SpawnPlan, spec: &StartSpec) -> Result<String, PluginError> {
        #[cfg(test)]
        if let Some(runner) = &self.runner {
            return runner(plan);
        }
        self.spawn_and_read(plan, workdir(spec)).await
    }

    async fn spawn_and_read(
        &self,
        plan: &SpawnPlan,
        cwd: &std::path::Path,
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
        while let Some(chunk) = stdout.next_chunk().await {
            let chunk = chunk?;
            text.push_str(&String::from_utf8_lossy(&chunk));
        }
        let _ = child.wait().await;
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
        request: Request,
        _inbound: ProtocolId,
    ) -> Result<Completion, PluginError> {
        reject_non_text(&request)?;
        let Some(secret) = self.host.accounts().secret(&self.account).await? else {
            return Err(
                PluginError::auth("no Claude Code sign-in is stored for this account")
                    .without_cooldown(),
            );
        };
        let blob = SecretObject::parse(&secret)?;
        if blob.flag(creds::LAPSED) {
            return Err(PluginError::auth(creds::LAPSED_REASON).without_cooldown());
        }
        let token = token_for_run(&blob)?;
        let config_dir = account_dir(&self.host, &self.account)?;
        let program = self.program()?;
        let spec = StartSpec {
            program,
            cwd: config_dir.clone(),
            model: request.model.clone(),
            token,
            config_dir,
            sandbox: running_as_root(),
            proxy: Vec::new(),
            wsl_distro: None,
        };
        let plan = start_plan(&spec, &request)?;
        let stdout = self.stdout_for(&plan, &spec).await?;
        let events = events_from_stdout(&stdout)?;
        Ok(Completion::Stream(Box::pin(stream::iter(
            events.into_iter().map(Ok),
        ))))
    }
}

#[cfg(test)]
mod tests {
    use super::ClaudeUpstream;
    use crate::bridge::runner::FakeRunner;
    use crate::creds;
    use futures_util::StreamExt;
    use hofund_kit_subscription::{TestHost, block_on};
    use hofund_plugin_api::{
        AccountId, Completion, ContentPart, Message, PluginError, PluginErrorKind, PluginId,
        PluginKind, ProtocolId, Request, Role, StreamEvent, UpstreamProvider,
    };
    use hofund_plugin_sdk::oauth::SecretObject;
    use serde_json::json;
    use std::path::PathBuf;
    #[cfg(test)]
    use std::sync::Arc;

    const CREDENTIALS: &str = r#"{"claudeAiOauth":{"accessToken":"secret-token","refreshToken":"rt-1","expiresAt":1900000000000,"scopes":["user:inference"],"subscriptionType":"max"}}"#;
    const MESSAGE_DELTA: &str = include_str!("bridge/fixtures/stream/message_delta.jsonl");

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
        let Completion::Stream(mut stream) = completion else {
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

    #[test]
    fn execute_rejects_tools_before_spawn() {
        let dir = tempfile::tempdir().expect("dir");
        let host = TestHost::open_for(
            PluginKind::Subscription,
            PluginId::from_static("claude"),
            dir.path(),
        );
        let provider = ClaudeUpstream::new(
            AccountId::from_static("a-00000000000000aa"),
            host.context().clone(),
            Some(program()),
        );
        let request = serde_json::from_value::<Request>(json!({
            "model": "claude-sonnet-4-5",
            "messages": [{"role": "user", "content": [{"type": "text", "text": "hi"}]}],
            "tools": [{"name": "lookup", "input_schema": {"type": "object"}}]
        }))
        .expect("request");
        let error = match block_on(provider.execute(request, ProtocolId::AnthropicMessages)) {
            Err(error) => error,
            Ok(_) => panic!("tools must fail closed"),
        };
        assert_eq!(error.kind(), PluginErrorKind::Unsupported);
        assert!(host.process().piped_spawns().is_empty());
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
        let provider = ClaudeUpstream::new(id, host.context().clone(), Some(program()))
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
}
