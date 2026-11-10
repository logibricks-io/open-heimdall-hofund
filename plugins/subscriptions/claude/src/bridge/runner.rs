//! Test-only runner that records a spawn plan and replays a fixture.
//!
//! Production spawn through `LongLivedChild` lands in a later slice. This
//! runner wraps [`hofund_kit_subscription::FakeClaude`] so plugin tests can
//! inject a program without talking to the network or a real binary.

use std::collections::BTreeMap;
use std::ffi::OsString;

use hofund_kit_subscription::{FakeClaude, FakeClaudeError, RunRecord};

/// Arguments, environment, and stdin a later slice will hand to Claude Code.
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

/// Replays a scripted Claude Code stdout fixture and records the plan.
#[derive(Debug)]
pub(crate) struct FakeRunner {
    fake: FakeClaude,
}

impl FakeRunner {
    /// Builds a runner that writes `stdout` when [`Self::run`] is called.
    #[must_use]
    pub(crate) fn from_stdout(stdout: impl Into<String>) -> Self {
        Self {
            fake: FakeClaude::from_stdout(stdout),
        }
    }

    /// Requires `CLAUDE_CONFIG_DIR` to equal `dir`.
    #[must_use]
    pub(crate) fn expect_config_dir(mut self, dir: impl Into<OsString>) -> Self {
        self.fake = self.fake.expect_config_dir(dir);
        self
    }

    /// Requires `IS_SANDBOX=1`, as a root run must set.
    #[must_use]
    pub(crate) fn expect_sandbox(mut self) -> Self {
        self.fake = self.fake.expect_sandbox();
        self
    }

    /// Requires `CLAUDE_CODE_OAUTH_TOKEN` in the environment, never on argv.
    #[must_use]
    pub(crate) fn expect_token(mut self, token: impl Into<OsString>) -> Self {
        self.fake = self.fake.expect_token(token);
        self
    }

    /// Requires these strings to appear in argv (helper and prompt paths).
    #[must_use]
    pub(crate) fn expect_argv_paths(mut self, paths: Vec<OsString>) -> Self {
        self.fake = self.fake.expect_argv_paths(paths);
        self
    }

    /// Replays the fixture against `plan` and records env, argv, and stdin.
    pub(crate) fn run(&self, plan: &SpawnPlan) -> Result<RunRecord, FakeClaudeError> {
        self.fake.run(&plan.argv, &plan.env, &plan.stdin)
    }
}

#[cfg(test)]
mod tests {
    use super::{FakeRunner, SpawnPlan};
    use std::ffi::OsString;

    fn os(s: &str) -> OsString {
        OsString::from(s)
    }

    fn plan(argv: &[&str], env: &[(&str, &str)], stdin: &str) -> SpawnPlan {
        SpawnPlan {
            argv: argv.iter().map(|s| os(s)).collect(),
            env: env.iter().map(|(k, v)| (os(k), os(v))).collect(),
            stdin: stdin.to_owned(),
        }
    }

    #[test]
    fn records_proxy_env_and_replays_stdout() {
        let runner = FakeRunner::from_stdout("{\"type\":\"result\"}\n");
        let record = runner
            .run(&plan(
                &["claude", "-p"],
                &[
                    ("HTTP_PROXY", "http://127.0.0.1:8080"),
                    ("HTTPS_PROXY", "http://127.0.0.1:8080"),
                    ("ALL_PROXY", "socks5://127.0.0.1:1080"),
                    ("NO_PROXY", "127.0.0.1"),
                ],
                "{\"type\":\"user\"}\n",
            ))
            .expect("run");
        assert_eq!(record.stdout, "{\"type\":\"result\"}\n");
        assert_eq!(record.stdin, "{\"type\":\"user\"}\n");
        assert_eq!(
            record.env_value("HTTP_PROXY").as_deref(),
            Some("http://127.0.0.1:8080")
        );
        assert_eq!(
            record.env_value("HTTPS_PROXY").as_deref(),
            Some("http://127.0.0.1:8080")
        );
        assert_eq!(
            record.env_value("ALL_PROXY").as_deref(),
            Some("socks5://127.0.0.1:1080")
        );
        assert_eq!(record.env_value("NO_PROXY").as_deref(), Some("127.0.0.1"));
    }

    #[test]
    fn token_must_be_in_env_and_not_on_argv() {
        let runner = FakeRunner::from_stdout("ok\n").expect_token("secret-token");
        let ok = plan(
            &["claude", "-p"],
            &[("CLAUDE_CODE_OAUTH_TOKEN", "secret-token")],
            "",
        );
        runner.run(&ok).expect("token in env");

        let on_argv = plan(
            &["claude", "-p", "secret-token"],
            &[("CLAUDE_CODE_OAUTH_TOKEN", "secret-token")],
            "",
        );
        assert!(runner.run(&on_argv).is_err());

        let missing = plan(&["claude", "-p"], &[], "");
        assert!(runner.run(&missing).is_err());
    }

    #[test]
    fn config_dir_sandbox_and_rewritten_paths_are_checked() {
        let runner = FakeRunner::from_stdout("ok\n")
            .expect_config_dir("/tmp/acct/.claude")
            .expect_sandbox()
            .expect_argv_paths(vec![
                os("/mnt/c/Program Files/hofund/hofund"),
                os("/mnt/c/Users/me/AppData/Local/Temp/prompt.txt"),
            ]);
        let record = runner
            .run(&plan(
                &[
                    "wsl",
                    "-d",
                    "Ubuntu",
                    "--exec",
                    "claude",
                    "--mcp-config",
                    "/mnt/c/Program Files/hofund/hofund",
                    "--system-prompt-file",
                    "/mnt/c/Users/me/AppData/Local/Temp/prompt.txt",
                ],
                &[
                    ("CLAUDE_CONFIG_DIR", "/tmp/acct/.claude"),
                    ("IS_SANDBOX", "1"),
                ],
                "",
            ))
            .expect("env and rewritten paths");
        assert_eq!(
            record.env_value("CLAUDE_CONFIG_DIR").as_deref(),
            Some("/tmp/acct/.claude")
        );
        assert_eq!(record.env_value("IS_SANDBOX").as_deref(), Some("1"));
    }
}
