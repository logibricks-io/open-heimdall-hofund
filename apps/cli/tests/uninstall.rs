//! Clean-exit behavior through the real CLI and one-shot daemon.

#![cfg(unix)]

use std::fs;
use std::path::{Path, PathBuf};
use std::process::{Child, Command, Output, Stdio};
use std::sync::OnceLock;
use std::time::{Duration, Instant};

const DEADLINE: Duration = Duration::from_secs(120);
const CONFIG: &str = "[[providers]]\nid = \"chat\"\nprotocol = \"openai-chat\"\n\
base_url = \"http://127.0.0.1:9/v1\"\napi_key = \"provider-key\"\n\
[providers.models]\nrefresh = \"off\"\n[[routes]]\nmodel = \"m\"\nupstreams = [\"chat\"]\n";

fn hofund() -> &'static Path {
    Path::new(env!("CARGO_BIN_EXE_hofund"))
}

fn hofundd() -> &'static Path {
    static BUILT: OnceLock<PathBuf> = OnceLock::new();
    BUILT.get_or_init(|| {
        let bin_dir = hofund().parent().expect("bin dir");
        let target = bin_dir.parent().expect("target dir");
        let output = Command::new(env!("CARGO"))
            .args([
                "build",
                "--quiet",
                "-p",
                "hofund-daemon-bin",
                "--bin",
                "hofundd",
            ])
            .env("CARGO_TARGET_DIR", target)
            .current_dir(env!("CARGO_MANIFEST_DIR"))
            .stdin(Stdio::null())
            .output()
            .expect("build hofundd");
        assert!(output.status.success(), "building hofundd: {output:?}");
        bin_dir.join("hofundd")
    })
}

struct Scratch {
    _dir: tempfile::TempDir,
    root: PathBuf,
    config: PathBuf,
    home: PathBuf,
    run: PathBuf,
}

impl Scratch {
    fn new() -> Self {
        let dir = tempfile::Builder::new()
            .prefix("hf-uninstall")
            .tempdir_in("/tmp")
            .expect("tempdir");
        let root = fs::canonicalize(dir.path()).expect("canonical");
        let config = root.join("cfg/hofund.toml");
        let home = root.join("home");
        let run = root.join("run");
        for path in [config.parent().expect("config dir"), &home, &run] {
            fs::create_dir_all(path).expect("mkdir");
        }
        fs::write(
            &config,
            format!("listen = \"127.0.0.1:0\"\ntoken_file = \"token\"\nusage_file = \"usage.sqlite\"\n{CONFIG}"),
        )
        .expect("config");
        Self {
            _dir: dir,
            root,
            config,
            home,
            run,
        }
    }

    fn command(&self, exe: &Path, args: &[&str]) -> Command {
        let mut command = Command::new(exe);
        command
            .args(args)
            .arg("--config")
            .arg(&self.config)
            .env_clear()
            .env("PATH", self.root.join("empty-path"))
            .env("HOME", &self.home)
            .env("XDG_RUNTIME_DIR", &self.run)
            .env("HOFUND_LOG", "off")
            .stdin(Stdio::null())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped());
        command
    }

    fn run(&self, args: &[&str]) -> Run {
        hofundd();
        let output = self.command(hofund(), args).output().expect("run hofund");
        Run {
            code: output.status.code(),
            stdout: String::from_utf8_lossy(&output.stdout).into_owned(),
            stderr: String::from_utf8_lossy(&output.stderr).into_owned(),
        }
    }

    fn codex(&self) -> PathBuf {
        self.home.join(".codex/config.toml")
    }

    fn claude(&self) -> PathBuf {
        self.home.join(".claude/settings.json")
    }

    fn connect(&self, id: &str) {
        self.run(&["agent", "connect", id]).ok();
    }
}

#[derive(Debug)]
struct Run {
    code: Option<i32>,
    stdout: String,
    stderr: String,
}

impl Run {
    #[track_caller]
    fn ok(&self) -> &str {
        assert_eq!(self.code, Some(0), "{self:#?}");
        &self.stdout
    }

    #[track_caller]
    fn failed(&self) -> &str {
        assert_eq!(self.code, Some(1), "{self:#?}");
        &self.stderr
    }
}

fn seed(path: &Path, bytes: &str) {
    fs::create_dir_all(path.parent().expect("parent")).expect("mkdir");
    fs::write(path, bytes).expect("seed");
}

fn markers(path: &Path) -> Vec<PathBuf> {
    [".hofund.bak", ".hofund.absent", ".hofund.wrote"]
        .into_iter()
        .map(|suffix| {
            let mut name = path.file_name().expect("name").to_os_string();
            name.push(suffix);
            path.with_file_name(name)
        })
        .collect()
}

fn assert_no_markers(path: &Path) {
    for marker in markers(path) {
        assert!(!marker.exists(), "{} remains", marker.display());
    }
}

#[test]
fn uninstall_restores_two_agents_byte_for_byte() {
    let scratch = Scratch::new();
    let codex = scratch.codex();
    let claude = scratch.claude();
    let codex_before = "model = \"mine\"\n# keep this comment\n";
    let claude_before = "{\n  \"model\": \"mine\",\n  \"custom\": true\n}\n";
    seed(&codex, codex_before);
    seed(&claude, claude_before);
    scratch.connect("codex");
    scratch.connect("claude-code");

    let out = scratch.run(&["uninstall", "--yes"]);
    let text = out.ok();
    assert!(text.contains("codex: restored"), "{out:#?}");
    assert!(text.contains("claude-code: restored"), "{out:#?}");
    assert!(text.contains("delete it to finish"), "{out:#?}");
    assert_eq!(fs::read_to_string(&codex).expect("codex"), codex_before);
    assert_eq!(fs::read_to_string(&claude).expect("claude"), claude_before);
    assert_no_markers(&codex);
    assert_no_markers(&claude);
}

#[test]
fn uninstall_nonforce_keeps_later_user_edits() {
    let scratch = Scratch::new();
    let codex = scratch.codex();
    seed(&codex, "model = \"mine\"\n");
    scratch.connect("codex");
    fs::write(&codex, "model = \"user-after\"\n# user-added\n").expect("user edit");

    let out = scratch.run(&["uninstall", "--yes"]);
    assert!(out.ok().contains("codex: restored"), "{out:#?}");
    let restored = fs::read_to_string(&codex).expect("restored");
    assert!(restored.contains("model = \"user-after\""), "{restored}");
    assert!(restored.contains("# user-added"), "{restored}");
    assert!(!restored.contains("127.0.0.1"), "{restored}");
    assert_no_markers(&codex);
}

#[test]
fn one_bad_agent_does_not_stop_the_others() {
    let scratch = Scratch::new();
    let codex = scratch.codex();
    let claude = scratch.claude();
    let claude_before = "{\"custom\":true}\n";
    seed(&codex, "model = \"mine\"\n");
    seed(&claude, claude_before);
    scratch.connect("codex");
    scratch.connect("claude-code");
    fs::write(&codex, "not = [\n").expect("break codex");

    let out = scratch.run(&["uninstall", "--yes"]);
    let error = out.failed();
    assert!(error.contains("codex: failed"), "{out:#?}");
    assert!(
        error.contains("hofund agent disconnect codex --force"),
        "{out:#?}"
    );
    assert!(error.contains("claude-code: restored"), "{out:#?}");
    assert_eq!(fs::read_to_string(&claude).expect("claude"), claude_before);
    assert_eq!(fs::read_to_string(&codex).expect("codex"), "not = [\n");
    assert_no_markers(&claude);
    assert!(markers(&codex)[0].exists(), "failed backup was removed");
}

#[test]
fn without_yes_nonterminal_input_refuses_before_changing_anything() {
    let scratch = Scratch::new();
    let codex = scratch.codex();
    seed(&codex, "model = \"mine\"\n");
    scratch.connect("codex");
    let connected = fs::read(&codex).expect("connected");
    let config = fs::read(&scratch.config).expect("config");

    let out = scratch.run(&["uninstall"]);
    assert!(out.failed().contains("--yes"), "{out:#?}");
    assert_eq!(fs::read(&codex).expect("codex"), connected);
    assert_eq!(fs::read(&scratch.config).expect("config"), config);
    assert!(markers(&codex)[0].exists());
}

struct Daemon(Option<Child>);

impl Drop for Daemon {
    fn drop(&mut self) {
        if let Some(mut child) = self.0.take() {
            let _ = child.kill();
            let _ = child.wait();
        }
    }
}

fn wait_with_deadline(mut child: Child) -> Output {
    let started = Instant::now();
    loop {
        if child.try_wait().expect("try wait").is_some() {
            return child.wait_with_output().expect("output");
        }
        if started.elapsed() > DEADLINE {
            let _ = child.kill();
            panic!("daemon did not exit");
        }
        std::thread::sleep(Duration::from_millis(10));
    }
}

#[test]
fn a_running_gateway_refuses_uninstall() {
    let scratch = Scratch::new();
    let codex = scratch.codex();
    seed(&codex, "model = \"mine\"\n");
    scratch.connect("codex");
    let connected = fs::read(&codex).expect("connected");
    let mut command = scratch.command(hofundd(), &[]);
    command.stdout(Stdio::null()).stderr(Stdio::null());
    let mut daemon = Daemon(Some(command.spawn().expect("daemon")));
    let started = Instant::now();
    while !scratch
        .run(&["status"])
        .stdout
        .starts_with("daemon running")
    {
        assert!(started.elapsed() < DEADLINE, "daemon never started");
        std::thread::sleep(Duration::from_millis(50));
    }

    let out = scratch.run(&["uninstall", "--yes", "--purge"]);
    assert!(out.failed().contains("hofund shutdown"), "{out:#?}");
    assert_eq!(fs::read(&codex).expect("codex"), connected);
    assert!(scratch.config.exists());

    scratch.run(&["shutdown"]).ok();
    let output = wait_with_deadline(daemon.0.take().expect("daemon"));
    assert!(output.status.success(), "{output:?}");
}

#[test]
fn purge_removes_only_hofund_paths_and_never_follows_links() {
    let scratch = Scratch::new();
    let token = scratch.config.parent().expect("cfg").join("token");
    let usage = scratch.config.parent().expect("cfg").join("usage.sqlite");
    let keys = scratch
        .config
        .parent()
        .expect("cfg")
        .join("caller-keys.json");
    let rules = scratch
        .config
        .parent()
        .expect("cfg")
        .join("oidc-rules.json");
    let state = scratch.config.parent().expect("cfg").join(".hofund");
    let sibling = scratch.config.parent().expect("cfg").join("sibling.txt");
    let outside = scratch.root.join("outside.txt");
    seed(&keys, "keys");
    seed(&rules, "rules");
    seed(&sibling, "keep sibling");
    seed(&outside, "keep target");
    fs::create_dir_all(state.join("host")).expect("state");
    seed(&state.join("host/plugin.json"), "state");
    std::os::unix::fs::symlink(&outside, state.join("outside-link")).expect("symlink");

    let kept = scratch.run(&["uninstall", "--yes"]);
    assert!(kept.ok().contains("settings and state kept"), "{kept:#?}");
    for path in [&scratch.config, &token, &usage, &keys, &rules] {
        assert!(
            path.exists(),
            "{} was removed without --purge",
            path.display()
        );
    }
    assert!(state.join("outside-link").symlink_metadata().is_ok());

    let purged = scratch.run(&["uninstall", "--yes", "--purge"]);
    assert!(purged.ok().contains("deleted"), "{purged:#?}");
    for path in [&scratch.config, &token, &usage, &keys, &rules, &state] {
        assert!(!path.exists(), "{} remains after --purge", path.display());
    }
    assert_eq!(
        fs::read_to_string(&sibling).expect("sibling"),
        "keep sibling"
    );
    assert_eq!(
        fs::read_to_string(&outside).expect("outside"),
        "keep target"
    );
}

#[test]
fn a_purge_that_cannot_remove_a_path_exits_non_zero_and_names_it() {
    let scratch = Scratch::new();
    let keys = scratch
        .config
        .parent()
        .expect("cfg")
        .join("caller-keys.json");
    fs::create_dir_all(keys.join("inner")).expect("directory where a file belongs");

    let out = scratch.run(&["uninstall", "--yes", "--purge"]);
    let error = out.failed();
    assert!(
        error.contains(&format!("failed: {}", keys.display())),
        "{out:#?}"
    );
    assert!(error.contains("uninstall incomplete"), "{out:#?}");
    assert!(keys.join("inner").is_dir(), "the directory was removed");
    assert!(
        !scratch.config.exists(),
        "other owned paths are still purged"
    );
}

#[test]
fn unknown_uninstall_flag_changes_nothing_and_help_lists_the_command() {
    let scratch = Scratch::new();
    let codex = scratch.codex();
    seed(&codex, "model = \"mine\"\n");
    scratch.connect("codex");
    let connected = fs::read(&codex).expect("connected");

    let out = scratch.run(&["uninstall", "--unknown"]);
    assert_eq!(out.code, Some(2), "{out:#?}");
    assert!(out.stderr.contains("unknown flag"), "{out:#?}");
    assert_eq!(fs::read(&codex).expect("codex"), connected);
    let help = scratch.run(&["--help"]);
    assert!(help.ok().contains("uninstall"), "{help:#?}");
}
