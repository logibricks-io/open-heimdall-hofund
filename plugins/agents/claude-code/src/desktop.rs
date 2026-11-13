//! Claude Desktop session discovery over Cowork data folders.

use std::path::{Path, PathBuf};

use hofund_plugin_api::{PluginError, SessionFile, SessionLocator, SessionParser, SessionStore};

use crate::{collect_cc_files, parser_for, walk_desktop_homes};

/// The Claude Desktop session agent id.
pub const DESKTOP_AGENT: &str = "claude-desktop";

/// Claude Desktop's session store.
#[derive(Clone, Debug)]
pub struct ClaudeDesktopStore {
    locator: ClaudeDesktopLocator,
}

impl ClaudeDesktopStore {
    /// Uses Claude Desktop data folders on this machine.
    #[must_use]
    pub fn new() -> Self {
        Self::with_roots(desktop_data_dirs())
    }

    /// Uses explicit Desktop data roots for tests and embedding.
    #[must_use]
    pub fn with_roots(roots: impl IntoIterator<Item = PathBuf>) -> Self {
        Self {
            locator: ClaudeDesktopLocator {
                roots: roots.into_iter().collect(),
            },
        }
    }
}

impl Default for ClaudeDesktopStore {
    fn default() -> Self {
        Self::new()
    }
}

/// Directory-only Claude Desktop locator.
#[derive(Clone, Debug)]
pub struct ClaudeDesktopLocator {
    roots: Vec<PathBuf>,
}

impl SessionLocator for ClaudeDesktopLocator {
    fn roots(&self) -> Vec<PathBuf> {
        self.roots.clone()
    }

    fn present(&self) -> bool {
        self.roots.iter().any(|root| {
            root.join("local-agent-mode-sessions").is_dir()
                || root.join("claude-code-sessions").is_dir()
        })
    }
}

impl SessionStore for ClaudeDesktopStore {
    fn agent(&self) -> &'static str {
        DESKTOP_AGENT
    }

    fn locator(&self) -> &dyn SessionLocator {
        &self.locator
    }

    fn files(&self, root: &Path) -> Result<Vec<SessionFile>, PluginError> {
        let mut files = Vec::new();
        for home in walk_desktop_homes(root)? {
            files.extend(collect_cc_files(DESKTOP_AGENT, &home.join("projects"))?);
        }
        files.sort_by(|left, right| left.path.cmp(&right.path));
        Ok(files)
    }

    fn parser(
        &self,
        file: &SessionFile,
        state: Option<&str>,
    ) -> Result<Box<dyn SessionParser>, PluginError> {
        parser_for(DESKTOP_AGENT, file, state)
    }
}

fn desktop_data_dirs() -> Vec<PathBuf> {
    let home = home_dir();
    #[cfg(windows)]
    {
        let mut roots = Vec::new();
        if let Some(app_data) = std::env::var_os("APPDATA") {
            roots.push(PathBuf::from(app_data).join("Claude"));
        }
        if let Some(local) = std::env::var_os("LOCALAPPDATA") {
            roots.push(PathBuf::from(&local).join("Claude"));
            roots.push(PathBuf::from(local).join("Claude-3p"));
        }
        if roots.is_empty() {
            roots.push(home.join("AppData").join("Roaming").join("Claude"));
        }
        roots
    }
    #[cfg(target_os = "macos")]
    {
        let base = home.join("Library").join("Application Support");
        vec![base.join("Claude"), base.join("Claude-3p")]
    }
    #[cfg(all(unix, not(target_os = "macos")))]
    {
        let base = std::env::var_os("XDG_CONFIG_HOME")
            .filter(|value| !value.is_empty())
            .map_or_else(|| home.join(".config"), PathBuf::from);
        vec![base.join("Claude"), base.join("Claude-3p")]
    }
}

fn home_dir() -> PathBuf {
    #[cfg(windows)]
    let home = std::env::var_os("USERPROFILE").or_else(|| std::env::var_os("HOME"));
    #[cfg(not(windows))]
    let home = std::env::var_os("HOME");
    home.map_or_else(|| PathBuf::from("."), PathBuf::from)
}

#[cfg(test)]
mod tests {
    use std::io::{BufRead, BufReader};

    use hofund_plugin_sdk::json;

    use super::*;

    fn fixture(name: &str) -> PathBuf {
        Path::new(env!("CARGO_MANIFEST_DIR"))
            .join("tests/fixtures")
            .join(name)
    }

    #[test]
    fn claude_desktop_lists_cowork_projects() {
        let root = fixture("desktop/data");
        let store = ClaudeDesktopStore::with_roots([root.clone()]);
        let files = store.files(&root).expect("files");
        assert_eq!(files.len(), 1);
        assert_eq!(files[0].id, "desktop-one");
    }

    #[test]
    #[ignore = "run once to refresh golden fixtures"]
    fn dump_desktop_golden() {
        let root = fixture("desktop/data");
        let path = root.join(
            "local-agent-mode-sessions/acct/org/local_fixture/.claude/projects/app/desktop-one.jsonl",
        );
        let file = SessionFile::new("desktop-one", &path);
        let store = ClaudeDesktopStore::with_roots([root]);
        let mut parser = store.parser(&file, None).expect("parser");
        let input = std::fs::File::open(&path).expect("fixture");
        for line in BufReader::new(input).lines() {
            let line = line.expect("UTF-8");
            let value = json::from_str(&line).expect("JSON");
            parser.push(&value).expect("push");
        }
        let mut record = parser.finish().expect("finish").record.expect("record");
        record.path = PathBuf::new();
        eprintln!("{}", json::to_string_pretty(&record).expect("JSON"));
    }

    #[test]
    fn claude_desktop_golden_matches_cli_shape() {
        let root = fixture("desktop/data");
        let path = root.join(
            "local-agent-mode-sessions/acct/org/local_fixture/.claude/projects/app/desktop-one.jsonl",
        );
        let file = SessionFile::new("desktop-one", &path);
        let store = ClaudeDesktopStore::with_roots([root]);
        let mut parser = store.parser(&file, None).expect("parser");
        let input = std::fs::File::open(&path).expect("fixture");
        for line in BufReader::new(input).lines() {
            let line = line.expect("UTF-8");
            let value = json::from_str(&line).expect("JSON");
            parser.push(&value).expect("push");
        }
        let mut record = parser.finish().expect("finish").record.expect("record");
        record.path = PathBuf::new();
        let actual = json::to_value(record).expect("JSON");
        let expected: json::Value = json::from_str(
            &std::fs::read_to_string(fixture("desktop/desktop.json")).expect("golden"),
        )
        .expect("golden");
        assert_eq!(actual, expected);
    }

    #[test]
    fn claude_desktop_has_no_resume_argv() {
        let store = ClaudeDesktopStore::with_roots([]);
        assert_eq!(
            store.resume_argv("claude-desktop:one", Path::new("/tmp/session.jsonl")),
            None
        );
    }

    #[test]
    fn present_is_true_when_either_session_dir_exists() {
        let root = fixture("desktop/data");
        let locator = ClaudeDesktopLocator { roots: vec![root] };
        assert!(locator.present());
        let missing = ClaudeDesktopLocator {
            roots: vec![fixture("desktop/missing-data")],
        };
        assert!(!missing.present());
        let empty = ClaudeDesktopLocator { roots: vec![] };
        assert!(!empty.present());
    }

    #[test]
    fn data_dirs_and_home_are_not_empty() {
        let dirs = desktop_data_dirs();
        assert_ne!(dirs.len(), 0);
        assert!(dirs.iter().all(|path| !path.as_os_str().is_empty()));
        assert!(!home_dir().as_os_str().is_empty());
    }

    #[test]
    fn desktop_roots_return_the_configured_paths() {
        let roots = vec![
            PathBuf::from("/tmp/desktop-a"),
            PathBuf::from("/tmp/desktop-b"),
        ];
        let store = ClaudeDesktopStore::with_roots(roots.clone());
        assert_eq!(store.locator().roots(), roots);
    }
}
