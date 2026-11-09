//! Disconnect every managed agent and optionally remove hofund's own files.

use std::collections::BTreeSet;
use std::io::{BufRead, IsTerminal, Write};
use std::path::{Component, Path, PathBuf};
use std::process::ExitCode;

use hofund_control_client::Client;
use hofund_control_client::hofund_control_proto::AgentDisconnectRequest;

use crate::connect::{Target, one_shot_client, select};

pub(crate) fn run(config: &Path, yes: bool, purge: bool) -> ExitCode {
    let stdin = std::io::stdin();
    let terminal = stdin.is_terminal();
    let mut input = stdin.lock();
    let mut output = std::io::stdout().lock();
    let mut error = std::io::stderr().lock();
    run_with_io(
        config,
        yes,
        purge,
        terminal,
        &mut input,
        &mut output,
        &mut error,
    )
}

fn run_with_io(
    config: &Path,
    yes: bool,
    purge: bool,
    terminal: bool,
    input: &mut impl BufRead,
    output: &mut impl Write,
    error: &mut impl Write,
) -> ExitCode {
    let runtime = match tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
    {
        Ok(runtime) => runtime,
        Err(build) => {
            let _ = writeln!(error, "cannot start the async runtime: {build}");
            return ExitCode::FAILURE;
        }
    };
    match runtime.block_on(select(config)) {
        Ok(Target::Daemon(_)) => {
            let _ = writeln!(
                error,
                "a hofund gateway/daemon is running; stop it first, e.g. `hofund shutdown`. Nothing was changed."
            );
            return ExitCode::FAILURE;
        }
        Ok(Target::NoDaemon) => {}
        Err(problem) => {
            let _ = writeln!(error, "{problem}");
            return ExitCode::FAILURE;
        }
    }
    let owned = match OwnedPaths::resolve(config) {
        Ok(owned) => owned,
        Err(problem) => {
            let _ = writeln!(error, "{problem}");
            return ExitCode::FAILURE;
        }
    };
    if !yes {
        if !terminal {
            let _ = writeln!(
                error,
                "stdin is not a terminal; rerun with `hofund uninstall --yes` to disconnect every managed agent without a prompt. Nothing was changed."
            );
            return ExitCode::FAILURE;
        }
        if let Err(problem) = print_plan(&owned, purge, output) {
            let _ = writeln!(error, "cannot print the uninstall plan: {problem}");
            return ExitCode::FAILURE;
        }
        match confirm(input, output, purge) {
            Ok(true) => {}
            Ok(false) => {
                let _ = writeln!(error, "nothing was changed");
                return ExitCode::FAILURE;
            }
            Err(problem) => {
                let _ = writeln!(error, "cannot read confirmation: {problem}");
                return ExitCode::FAILURE;
            }
        }
    }

    let (mut report, mut failed) = match runtime.block_on(disconnect_all(config)) {
        Ok(result) => result,
        Err(problem) => {
            let _ = writeln!(error, "{problem}");
            return ExitCode::FAILURE;
        }
    };
    report.push_str(
        "OS registrations: none; hofund does not install services, login items, or shortcuts.\n",
    );
    if purge {
        failed |= purge_paths(&owned, &mut report);
        report.push_str(
            "left: any account credentials in OS keychain service io.logibricks.hofund.accounts; the existing API cannot enumerate and remove them as a group.\n",
        );
    } else {
        report.push_str("settings and state kept:\n");
        for path in owned.all() {
            report.push_str(&format!("  {}\n", path.display()));
        }
    }
    match std::env::current_exe() {
        Ok(executable) => report.push_str(&format!(
            "hofund binary: {}\ndelete it to finish the uninstall.",
            executable.display()
        )),
        Err(problem) => {
            report.push_str(&format!("cannot locate the hofund binary: {problem}"));
            failed = true;
        }
    }
    if failed {
        report.push_str("\nuninstall incomplete; everything marked failed or left above remains.");
        let _ = writeln!(error, "{report}");
        ExitCode::FAILURE
    } else {
        let _ = writeln!(output, "{report}");
        ExitCode::SUCCESS
    }
}

fn print_plan(owned: &OwnedPaths, purge: bool, output: &mut impl Write) -> std::io::Result<()> {
    writeln!(output, "This uninstall will:")?;
    writeln!(
        output,
        "  disconnect every agent with hofund backups, preserving edits made after connect"
    )?;
    writeln!(
        output,
        "  leave OS registrations unchanged (hofund installs no service, login item, or shortcut)"
    )?;
    for path in owned.all() {
        writeln!(
            output,
            "  {} {}",
            if purge { "delete" } else { "keep" },
            path.display()
        )?;
    }
    if purge {
        writeln!(
            output,
            "  leave any hofund account entries in the OS keychain; no bulk-removal API exists"
        )?;
    }
    Ok(())
}

fn confirm(
    input: &mut impl BufRead,
    output: &mut impl Write,
    purge: bool,
) -> std::io::Result<bool> {
    if purge {
        writeln!(output, "The delete operations above cannot be undone.")?;
    }
    write!(output, "Uninstall hofund? [y/N] ")?;
    output.flush()?;
    let mut answer = String::new();
    input.read_line(&mut answer)?;
    Ok(matches!(
        answer.trim().to_ascii_lowercase().as_str(),
        "y" | "yes"
    ))
}

async fn disconnect_all(config: &Path) -> Result<(String, bool), String> {
    let client = one_shot_client(config)?;
    disconnect_with(&client).await
}

async fn disconnect_with(client: &Client) -> Result<(String, bool), String> {
    let agents = client.agents().await.map_err(crate::commands::failure)?;
    let mut report = String::new();
    let mut failed = false;
    for agent in agents.agents {
        let id = agent.id;
        match client
            .disconnect_agent(&id, AgentDisconnectRequest::new(false, None))
            .await
        {
            Ok(response) => {
                if response.changed_paths.is_empty() && response.warnings.is_empty() {
                    report.push_str(&format!("{id}: nothing to do\n"));
                } else {
                    report.push_str(&format!("{id}: restored\n"));
                }
                for warning in response.warnings {
                    report.push_str(&format!("{warning}\n"));
                }
            }
            Err(problem) => {
                failed = true;
                report.push_str(&format!(
                    "{id}: failed: {}\n  hint: hofund agent disconnect {id} --force\n",
                    crate::commands::failure(problem)
                ));
            }
        }
    }
    Ok((report, failed))
}

#[derive(Debug)]
struct OwnedPaths {
    files: Vec<PathBuf>,
    directories: Vec<PathBuf>,
    /// The directory holding the config file, removed by purge when empty.
    config_dir: Option<PathBuf>,
}

impl OwnedPaths {
    fn resolve(config: &Path) -> Result<Self, String> {
        let loaded = hofund_config::load_path(config)
            .map_err(|problem| format!("{}: {problem}", config.display()))?;
        let config = absolute(config)?;
        let token = absolute(&crate::local::beside(&config, &loaded.token_file))?;
        let usage = absolute(&crate::local::beside(&config, &loaded.usage_file))?;
        let keys = hofund_secrets::KeyStore::beside_token(&token)
            .path()
            .to_path_buf();
        let rules = hofund_secrets::oidc::RuleStore::beside_token(&token)
            .path()
            .to_path_buf();
        let state = config
            .parent()
            .unwrap_or_else(|| Path::new("."))
            .join(".hofund");
        let config_dir = config.parent().map(Path::to_path_buf);
        let mut paths = Self {
            files: vec![token, usage, keys, rules, config],
            directories: vec![state],
            config_dir,
        };
        paths.files = dedup(paths.files);
        paths.directories = dedup(paths.directories);
        Ok(paths)
    }

    fn all(&self) -> impl Iterator<Item = &PathBuf> {
        self.files.iter().chain(&self.directories)
    }
}

fn dedup(paths: Vec<PathBuf>) -> Vec<PathBuf> {
    paths
        .into_iter()
        .collect::<BTreeSet<_>>()
        .into_iter()
        .collect()
}

fn absolute(path: &Path) -> Result<PathBuf, String> {
    let path = if path.is_absolute() {
        path.to_path_buf()
    } else {
        std::env::current_dir()
            .map_err(|problem| format!("cannot resolve the current directory: {problem}"))?
            .join(path)
    };
    if path
        .components()
        .any(|component| matches!(component, Component::ParentDir))
    {
        return Err(format!(
            "unsafe uninstall path `{}`: parent-directory components are not allowed",
            path.display()
        ));
    }
    Ok(path)
}

fn purge_paths(owned: &OwnedPaths, report: &mut String) -> bool {
    let mut failed = false;
    for path in &owned.files {
        failed |= remove_owned(path, false, report);
    }
    for path in &owned.directories {
        failed |= remove_owned(path, true, report);
    }
    if let Some(parent) = &owned.config_dir {
        remove_if_empty(parent, report);
    }
    failed
}

/// Removes the config file's directory once purge has emptied it.
///
/// `remove_dir` only removes an empty directory, so anything else the user
/// keeps there stays, and a home or XDG root is never removed.
fn remove_if_empty(path: &Path, report: &mut String) {
    if removable(path).is_err() {
        return;
    }
    let empty = std::fs::read_dir(path).is_ok_and(|mut entries| entries.next().is_none());
    if empty && std::fs::remove_dir(path).is_ok() {
        report.push_str(&format!("deleted empty directory: {}\n", path.display()));
    }
}

fn remove_owned(path: &Path, directory: bool, report: &mut String) -> bool {
    if let Err(problem) = removable(path) {
        report.push_str(&format!("failed: {problem}\n"));
        return true;
    }
    let metadata = match std::fs::symlink_metadata(path) {
        Ok(metadata) => metadata,
        Err(problem) if problem.kind() == std::io::ErrorKind::NotFound => {
            report.push_str(&format!("not present: {}\n", path.display()));
            return false;
        }
        Err(problem) => {
            report.push_str(&format!("failed: {}: {problem}\n", path.display()));
            return true;
        }
    };
    let result = if metadata.file_type().is_symlink() {
        remove_link(path)
    } else if !metadata.is_dir() {
        std::fs::remove_file(path)
    } else if directory {
        remove_tree(path)
    } else {
        Err(std::io::Error::new(
            std::io::ErrorKind::IsADirectory,
            "refusing to remove a directory where a file was configured",
        ))
    };
    match result {
        Ok(()) => {
            report.push_str(&format!("deleted: {}\n", path.display()));
            false
        }
        Err(problem) => {
            report.push_str(&format!("failed: {}: {problem}\n", path.display()));
            true
        }
    }
}

fn remove_tree(path: &Path) -> std::io::Result<()> {
    for entry in std::fs::read_dir(path)? {
        let entry = entry?;
        let child = entry.path();
        let metadata = std::fs::symlink_metadata(&child)?;
        if metadata.is_dir() && !metadata.file_type().is_symlink() {
            remove_tree(&child)?;
        } else if metadata.file_type().is_symlink() {
            remove_link(&child)?;
        } else {
            std::fs::remove_file(&child)?;
        }
    }
    std::fs::remove_dir(path)
}

#[cfg(not(windows))]
fn remove_link(path: &Path) -> std::io::Result<()> {
    std::fs::remove_file(path)
}

#[cfg(windows)]
fn remove_link(path: &Path) -> std::io::Result<()> {
    std::fs::remove_file(path).or_else(|_| std::fs::remove_dir(path))
}

fn removable(path: &Path) -> Result<(), String> {
    let protected = [
        "HOME",
        "USERPROFILE",
        "XDG_CONFIG_HOME",
        "XDG_DATA_HOME",
        "XDG_STATE_HOME",
        "XDG_CACHE_HOME",
        "XDG_RUNTIME_DIR",
        "APPDATA",
        "LOCALAPPDATA",
    ]
    .into_iter()
    .filter_map(std::env::var_os)
    .map(PathBuf::from)
    .any(|root| root == path);
    if path.parent().is_none() || protected {
        Err(format!(
            "unsafe uninstall path `{}`: refusing a filesystem, home, or XDG root",
            path.display()
        ))
    } else {
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use std::io::Cursor;
    use std::path::{Path, PathBuf};

    #[test]
    fn interactive_confirmation_defaults_to_no_and_accepts_yes() {
        for (input, expected) in [
            ("n\n", false),
            ("\n", false),
            ("y\n", true),
            ("yes\n", true),
        ] {
            let mut out = Vec::new();
            let confirmed = super::confirm(&mut Cursor::new(input), &mut out, false)
                .expect("interactive confirmation");
            assert_eq!(confirmed, expected, "input {input:?}");
            let printed = String::from_utf8(out).expect("UTF-8");
            assert!(printed.contains("[y/N]"), "{printed:?}");
        }
    }

    #[test]
    fn purge_confirmation_names_the_files_it_deletes() {
        let mut out = Vec::new();
        super::confirm(&mut Cursor::new("n\n"), &mut out, true).expect("confirmation");
        let printed = String::from_utf8(out).expect("UTF-8");
        assert!(printed.contains("delete"), "{printed:?}");
    }

    #[cfg(unix)]
    #[test]
    fn purge_removes_links_without_touching_their_targets() {
        let dir = tempfile::tempdir().expect("temp dir");
        let state = dir.path().join(".hofund");
        let outside = dir.path().join("outside");
        std::fs::create_dir(&state).expect("state");
        std::fs::write(&outside, "keep").expect("outside");
        std::os::unix::fs::symlink(&outside, state.join("link")).expect("link");
        let mut report = String::new();
        assert!(!super::remove_owned(&state, true, &mut report));
        assert!(!state.exists());
        assert_eq!(std::fs::read_to_string(outside).expect("outside"), "keep");
    }

    #[test]
    fn purge_removes_the_config_directory_only_when_empty() {
        let dir = tempfile::tempdir().expect("temp dir");
        let empty = dir.path().join("empty");
        let kept = dir.path().join("kept");
        std::fs::create_dir(&empty).expect("empty");
        std::fs::create_dir(&kept).expect("kept");
        std::fs::write(kept.join("notes.txt"), "mine").expect("user file");
        let mut report = String::new();
        super::remove_if_empty(&empty, &mut report);
        super::remove_if_empty(&kept, &mut report);
        assert!(!empty.exists());
        assert!(kept.join("notes.txt").exists());
        assert!(report.contains("deleted empty directory"), "{report}");
        assert!(!report.contains("kept"), "{report}");
    }

    fn owned(files: Vec<PathBuf>, directories: Vec<PathBuf>) -> super::OwnedPaths {
        super::OwnedPaths {
            files,
            directories,
            config_dir: None,
        }
    }

    #[test]
    fn the_plan_lists_every_owned_path_as_kept_or_deleted() {
        let paths = owned(
            vec![PathBuf::from("/cfg/hofund.toml")],
            vec![PathBuf::from("/cfg/.hofund")],
        );
        for (purge, verb) in [(false, "keep"), (true, "delete")] {
            let mut out = Vec::new();
            super::print_plan(&paths, purge, &mut out).expect("plan");
            let printed = String::from_utf8(out).expect("UTF-8");
            assert!(printed.contains("This uninstall will:"), "{printed}");
            for path in ["/cfg/hofund.toml", "/cfg/.hofund"] {
                assert!(printed.contains(&format!("{verb} {path}")), "{printed}");
            }
        }
    }

    #[test]
    fn a_failed_file_or_directory_removal_fails_the_purge() {
        let dir = tempfile::tempdir().expect("temp dir");
        let not_a_file = dir.path().join("token");
        std::fs::create_dir(&not_a_file).expect("directory");
        let mut report = String::new();
        assert!(super::purge_paths(
            &owned(vec![not_a_file.clone()], Vec::new()),
            &mut report
        ));
        assert!(not_a_file.is_dir());
        let mut report = String::new();
        assert!(super::purge_paths(
            &owned(Vec::new(), vec![PathBuf::from("/")]),
            &mut report
        ));
        let mut report = String::new();
        assert!(!super::purge_paths(
            &owned(vec![dir.path().join("absent")], Vec::new()),
            &mut report
        ));
    }

    #[test]
    fn a_missing_path_is_not_present_but_an_unreadable_one_fails() {
        let dir = tempfile::tempdir().expect("temp dir");
        let mut report = String::new();
        assert!(!super::remove_owned(
            &dir.path().join("absent"),
            false,
            &mut report
        ));
        assert!(report.contains("not present"), "{report}");
        let file = dir.path().join("plain");
        std::fs::write(&file, "x").expect("file");
        let mut report = String::new();
        assert!(super::remove_owned(&file.join("child"), false, &mut report));
        assert!(report.contains("failed"), "{report}");
    }

    #[test]
    fn purge_never_removes_a_filesystem_root() {
        let mut report = String::new();
        assert!(super::remove_owned(Path::new("/"), true, &mut report));
        assert!(report.contains("refusing"), "{report}");
    }
}
