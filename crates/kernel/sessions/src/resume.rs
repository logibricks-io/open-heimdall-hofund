//! Resume argv for listed sessions without shell joining.

use std::sync::Arc;

use hofund_plugin_api::{SessionRecord, SessionStore};

/// Executable argv and working directory for `std::process::Command`.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ResumeCommand {
    /// Program name and arguments.
    pub argv: Vec<String>,
    /// Working directory for the child process.
    pub cwd: String,
}

/// Whether `id` matches the reference safe session id grammar.
#[must_use]
pub fn safe_session_id(id: &str) -> bool {
    !id.is_empty()
        && id.len() <= 256
        && id
            .bytes()
            .all(|byte| byte.is_ascii_alphanumeric() || byte == b'_' || byte == b'-')
}

/// Finds the session store for a listed agent instance id.
#[must_use]
pub fn store_for_agent<'a>(
    stores: &'a [Arc<dyn SessionStore>],
    agent: &str,
) -> Option<&'a dyn SessionStore> {
    let (base, _) = split_instance(agent);
    stores
        .iter()
        .find(|store| store.agent() == base)
        .map(std::convert::AsRef::as_ref)
}

/// Builds resume argv for a listed record, or returns none when resume is impossible.
#[must_use]
pub fn resume_command(
    record: &SessionRecord,
    store: Option<&dyn SessionStore>,
    read_only: bool,
) -> Option<ResumeCommand> {
    if read_only || record.path.as_os_str().is_empty() {
        return None;
    }
    let bare_id = bare_session_id(&record.id)?;
    if !safe_session_id(bare_id) {
        return None;
    }
    let (base, wsl_distro) = split_instance(&record.agent);
    if base == "cursor" && record.cwd.is_empty() {
        return None;
    }
    let store = store?;
    let mut argv = store.resume_argv(&record.id, &record.path)?;
    if argv.is_empty() {
        return None;
    }
    if let Some(distro) = wsl_distro {
        argv = wsl_argv(distro, argv);
    }
    let cwd = record.cwd.clone();
    if cwd.is_empty() && wsl_distro.is_none() {
        return Some(ResumeCommand { argv, cwd });
    }
    Some(ResumeCommand { argv, cwd })
}

/// Wraps a Linux argv for `wsl.exe -d <distro> -- …`.
#[must_use]
pub fn wsl_argv(distro: &str, inner: Vec<String>) -> Vec<String> {
    let mut argv = vec![
        "wsl.exe".to_owned(),
        "-d".to_owned(),
        distro.to_owned(),
        "--".to_owned(),
    ];
    argv.extend(inner);
    argv
}

/// Strips the `agent:` prefix from a public session id.
#[must_use]
pub fn bare_session_id(id: &str) -> Option<&str> {
    let (_, bare) = id.split_once(':')?;
    Some(bare)
}

/// Splits `agent@wsl:Distro` into base agent id and optional WSL distro name.
#[must_use]
pub fn split_instance(agent: &str) -> (&str, Option<&str>) {
    match agent.split_once("@wsl:") {
        Some((base, distro)) => (base, Some(distro)),
        None => (agent, None),
    }
}

#[cfg(test)]
mod tests {
    use std::path::{Path, PathBuf};

    use hofund_plugin_agent_pi::PiStore;
    use hofund_plugin_api::SessionRecord;

    use super::*;

    fn record(agent: &str, id: &str, path: &Path, cwd: &str) -> SessionRecord {
        let mut record = SessionRecord::new(agent, id, path);
        record.cwd = cwd.to_owned();
        record
    }

    #[test]
    fn wsl_resume_prefixes_argv() {
        let path = if cfg!(windows) {
            PathBuf::from(r"C:\work\pi.jsonl")
        } else {
            PathBuf::from("/work/pi.jsonl")
        };
        let record = record("pi@wsl:Ubuntu", "pi:abc", &path, "/home/me/proj");
        let store = PiStore::with_session_roots("agent", None);
        let cmd = resume_command(&record, Some(&store), false).expect("resume");
        assert_eq!(
            cmd.argv,
            vec![
                "wsl.exe".to_owned(),
                "-d".to_owned(),
                "Ubuntu".to_owned(),
                "--".to_owned(),
                "pi".to_owned(),
                "--session".to_owned(),
                path.to_string_lossy().into_owned(),
            ]
        );
        assert_eq!(cmd.cwd, "/home/me/proj");
    }

    #[test]
    fn unsafe_session_id_returns_none() {
        let path = Path::new("/work/pi.jsonl");
        let record = record("pi", "pi:bad/id", path, "/work");
        let store = PiStore::with_session_roots("agent", None);
        assert!(resume_command(&record, Some(&store), false).is_none());
    }

    #[test]
    fn read_only_sessions_do_not_resume() {
        let path = Path::new("/work/pi.jsonl");
        let record = record("pi", "pi:ok", path, "/work");
        let store = PiStore::with_session_roots("agent", None);
        assert!(resume_command(&record, Some(&store), true).is_none());
    }

    #[test]
    fn safe_session_id_allows_alnum_underscore_and_hyphen() {
        assert!(safe_session_id("abc-def_1"));
        assert!(safe_session_id("a"));
        assert!(safe_session_id("_"));
        assert!(safe_session_id("-"));
        assert!(safe_session_id(&"x".repeat(256)));
        assert!(!safe_session_id(""));
        assert!(!safe_session_id(&"x".repeat(257)));
        assert!(!safe_session_id("bad/id"));
    }

    #[test]
    fn store_for_agent_matches_the_base_agent_id() {
        let store = PiStore::with_session_roots("agent", None);
        let stores: [std::sync::Arc<dyn hofund_plugin_api::SessionStore>; 1] =
            [std::sync::Arc::new(store)];
        assert!(store_for_agent(&stores, "pi").is_some());
        assert!(store_for_agent(&stores, "pi@wsl:Ubuntu").is_some());
        assert!(store_for_agent(&stores, "cursor").is_none());
    }

    #[test]
    fn resume_command_allows_empty_cwd_except_for_cursor() {
        let path = Path::new("/work/pi.jsonl");
        let store = PiStore::with_session_roots("agent", None);
        let empty = record("pi", "pi:ok", path, "");
        let cmd = resume_command(&empty, Some(&store), false).expect("empty cwd");
        assert_eq!(cmd.cwd, "");
        let cursor = record("cursor", "cursor:ok", path, "/work");
        assert!(resume_command(&cursor, Some(&store), false).is_some());
        let cursor_empty = record("cursor", "cursor:ok", path, "");
        assert!(resume_command(&cursor_empty, Some(&store), false).is_none());
    }
}
