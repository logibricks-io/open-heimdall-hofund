//! In-process fake Claude Code runner for tests.
//!
//! A test hands it a scripted stdout fixture, a spawn plan (argv, env,
//! stdin), and optional assertions. It records the plan and returns the
//! fixture. There is no network and no real `claude` binary.

use std::collections::BTreeMap;
use std::ffi::{OsStr, OsString};

/// Scripted Claude Code process used by plugin tests.
#[derive(Clone, Debug, Default)]
pub struct FakeClaude {
    stdout: String,
    expect_config_dir: Option<OsString>,
    expect_sandbox: bool,
    expect_token: Option<OsString>,
    expect_argv_paths: Vec<OsString>,
}

/// What a [`FakeClaude`] run observed.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct RunRecord {
    /// Program and arguments as supplied by the caller.
    pub argv: Vec<OsString>,
    /// Environment as supplied by the caller.
    pub env: BTreeMap<OsString, OsString>,
    /// stream-json stdin as supplied by the caller.
    pub stdin: String,
    /// Scripted stdout returned to the caller.
    pub stdout: String,
}

impl RunRecord {
    /// Lossy UTF-8 value of `key`, if present.
    #[must_use]
    pub fn env_value(&self, key: &str) -> Option<String> {
        self.env
            .get(OsStr::new(key))
            .map(|v| v.to_string_lossy().into_owned())
    }
}

/// Why a fake run rejected the spawn plan.
#[derive(Clone, Debug, Eq, PartialEq)]
pub enum FakeClaudeError {
    /// `CLAUDE_CONFIG_DIR` was missing or did not match.
    ConfigDir,
    /// A root run required `IS_SANDBOX=1`.
    Sandbox,
    /// The OAuth token was missing from the environment.
    TokenMissing,
    /// The OAuth token appeared in argv.
    TokenOnArgv,
    /// An expected rewritten helper or prompt path was missing from argv.
    PathRewrite,
}

impl std::fmt::Display for FakeClaudeError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::ConfigDir => write!(f, "CLAUDE_CONFIG_DIR was missing or did not match"),
            Self::Sandbox => write!(f, "IS_SANDBOX=1 was required and missing"),
            Self::TokenMissing => write!(
                f,
                "CLAUDE_CODE_OAUTH_TOKEN was missing from the environment"
            ),
            Self::TokenOnArgv => write!(f, "CLAUDE_CODE_OAUTH_TOKEN must not appear on argv"),
            Self::PathRewrite => write!(f, "an expected rewritten path was missing from argv"),
        }
    }
}

impl std::error::Error for FakeClaudeError {}

impl FakeClaude {
    /// Builds a runner that returns `stdout` on a successful run.
    #[must_use]
    pub fn from_stdout(stdout: impl Into<String>) -> Self {
        Self {
            stdout: stdout.into(),
            ..Self::default()
        }
    }

    /// Requires `CLAUDE_CONFIG_DIR` to equal `dir`.
    #[must_use]
    pub fn expect_config_dir(mut self, dir: impl Into<OsString>) -> Self {
        self.expect_config_dir = Some(dir.into());
        self
    }

    /// Requires `IS_SANDBOX=1`.
    #[must_use]
    pub fn expect_sandbox(mut self) -> Self {
        self.expect_sandbox = true;
        self
    }

    /// Requires `CLAUDE_CODE_OAUTH_TOKEN` in env and forbids it on argv.
    #[must_use]
    pub fn expect_token(mut self, token: impl Into<OsString>) -> Self {
        self.expect_token = Some(token.into());
        self
    }

    /// Requires each path to appear as an argv element (WSL-rewritten paths).
    #[must_use]
    pub fn expect_argv_paths(mut self, paths: Vec<OsString>) -> Self {
        self.expect_argv_paths = paths;
        self
    }

    /// Records the plan, checks assertions, and returns the scripted stdout.
    pub fn run(
        &self,
        argv: &[OsString],
        env: &BTreeMap<OsString, OsString>,
        stdin: &str,
    ) -> Result<RunRecord, FakeClaudeError> {
        if let Some(dir) = &self.expect_config_dir
            && env.get(OsStr::new("CLAUDE_CONFIG_DIR")) != Some(dir)
        {
            return Err(FakeClaudeError::ConfigDir);
        }
        if self.expect_sandbox
            && env.get(OsStr::new("IS_SANDBOX")).map(OsString::as_os_str) != Some(OsStr::new("1"))
        {
            return Err(FakeClaudeError::Sandbox);
        }
        if let Some(token) = &self.expect_token {
            if env.get(OsStr::new("CLAUDE_CODE_OAUTH_TOKEN")) != Some(token) {
                return Err(FakeClaudeError::TokenMissing);
            }
            if argv
                .iter()
                .any(|arg| arg == token || contains_os(arg, token))
            {
                return Err(FakeClaudeError::TokenOnArgv);
            }
        }
        for path in &self.expect_argv_paths {
            if !argv.iter().any(|arg| arg == path) {
                return Err(FakeClaudeError::PathRewrite);
            }
        }
        Ok(RunRecord {
            argv: argv.to_vec(),
            env: env.clone(),
            stdin: stdin.to_owned(),
            stdout: self.stdout.clone(),
        })
    }
}

fn contains_os(haystack: &OsStr, needle: &OsStr) -> bool {
    haystack
        .to_string_lossy()
        .contains(needle.to_string_lossy().as_ref())
}

#[cfg(test)]
mod tests {
    use super::{FakeClaude, FakeClaudeError};
    use std::collections::BTreeMap;
    use std::ffi::OsString;

    fn os(s: &str) -> OsString {
        OsString::from(s)
    }

    fn env(pairs: &[(&str, &str)]) -> BTreeMap<OsString, OsString> {
        pairs.iter().map(|(k, v)| (os(k), os(v))).collect()
    }

    #[test]
    fn rejects_token_on_argv_even_when_env_has_it() {
        let fake = FakeClaude::from_stdout("ok\n").expect_token("tok");
        let err = fake
            .run(
                &[os("claude"), os("--token=tok")],
                &env(&[("CLAUDE_CODE_OAUTH_TOKEN", "tok")]),
                "",
            )
            .expect_err("token on argv");
        assert_eq!(err, FakeClaudeError::TokenOnArgv);
    }

    #[test]
    fn accepts_matching_config_sandbox_and_paths() {
        let fake = FakeClaude::from_stdout("out\n")
            .expect_config_dir("/cfg")
            .expect_sandbox()
            .expect_argv_paths(vec![os("/mnt/c/hofund")]);
        let record = fake
            .run(
                &[os("claude"), os("/mnt/c/hofund")],
                &env(&[("CLAUDE_CONFIG_DIR", "/cfg"), ("IS_SANDBOX", "1")]),
                "in\n",
            )
            .expect("ok");
        assert_eq!(record.stdout, "out\n");
        assert_eq!(record.stdin, "in\n");
    }
}
