//! Runtime commands end to end, with and without a running daemon.
//!
//! Every case runs the built `hofund` against a scratch config, home, and
//! runtime directory. Cases with no daemon use the real `hofundd` one-shot
//! binary, built on first use into the same target directory as `hofund` so
//! the CLI finds it beside itself. Each child process runs under a deadline.

#![cfg(unix)]

use std::fs;
use std::io::{Read, Write};
use std::net::{SocketAddr, TcpListener};
use std::os::unix::fs::PermissionsExt;
use std::path::{Path, PathBuf};
use std::process::{Child, Command, Output, Stdio};
use std::sync::OnceLock;
use std::time::{Duration, Instant};

/// The longest any single CLI or build step may run.
const DEADLINE: Duration = Duration::from_secs(120);

const ROUTED: &str = "[[providers]]\nid = \"chat\"\nprotocol = \"openai-chat\"\n\
base_url = \"http://127.0.0.1:9/v1\"\napi_key = \"provider-key\"\n\
[providers.models]\nrefresh = \"off\"\n[[routes]]\nmodel = \"m\"\nupstreams = [\"chat\"]\n";

fn hofund() -> &'static Path {
    Path::new(env!("CARGO_BIN_EXE_hofund"))
}

/// Builds `hofundd` beside `hofund` once per test process.
fn hofundd() -> &'static Path {
    static BUILT: OnceLock<PathBuf> = OnceLock::new();
    BUILT.get_or_init(|| {
        let bin_dir = hofund().parent().expect("bin dir");
        let target = bin_dir.parent().expect("target dir");
        let mut build = Command::new(env!("CARGO"));
        build
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
            .stdin(Stdio::null());
        if bin_dir.file_name().is_some_and(|name| name == "release") {
            build.arg("--release");
        }
        let output = finish(spawn(&mut build), DEADLINE);
        assert!(output.status.success(), "building hofundd: {output:?}");
        let built = bin_dir.join("hofundd");
        assert!(built.is_file(), "{} missing after build", built.display());
        built
    })
}

/// Spawns `command`, retrying while another test's fork still holds a
/// freshly written executable open.
fn spawn(command: &mut Command) -> Child {
    let mut tries = 0;
    loop {
        match command.spawn() {
            Ok(child) => return child,
            Err(error) if error.raw_os_error() == Some(26) && tries < 50 => {
                tries += 1;
                std::thread::sleep(Duration::from_millis(20));
            }
            Err(error) => panic!("spawn {command:?}: {error}"),
        }
    }
}

/// Waits for `child` within `deadline`, killing it on expiry.
fn finish(mut child: Child, deadline: Duration) -> Output {
    let started = Instant::now();
    loop {
        if child.try_wait().expect("try_wait").is_some() {
            return child.wait_with_output().expect("output");
        }
        if started.elapsed() > deadline {
            let _ = child.kill();
            let output = child.wait_with_output().expect("output");
            panic!("child exceeded {deadline:?}: {output:?}");
        }
        std::thread::sleep(Duration::from_millis(10));
    }
}

/// A scratch install: config, token, home, and runtime roots.
struct Scratch {
    _dir: tempfile::TempDir,
    root: PathBuf,
    config: PathBuf,
    run: PathBuf,
    home: PathBuf,
    path: PathBuf,
    env: Vec<(String, String)>,
}

impl Scratch {
    fn new(listen: &str, extra: &str) -> Self {
        // Under /tmp so the control socket path stays within the Unix
        // socket length limit (104 bytes on macOS, whose default temporary
        // directory is long); a daemon that cannot bind it falls back to
        // the fixed default TCP control address, which parallel tests share.
        let dir = tempfile::Builder::new()
            .prefix("hf")
            .tempdir_in("/tmp")
            .expect("tempdir");
        let root = fs::canonicalize(dir.path()).expect("canonical");
        let config = root.join("cfg").join("hofund.toml");
        let run = root.join("run");
        let home = root.join("home");
        let path = root.join("empty-path");
        for dir in [config.parent().expect("cfg"), &run, &home, &path] {
            fs::create_dir_all(dir).expect("mkdir");
            fs::set_permissions(dir, fs::Permissions::from_mode(0o700)).expect("chmod");
        }
        fs::write(
            &config,
            format!("listen = \"{listen}\"\ntoken_file = \"token\"\n{extra}"),
        )
        .expect("config");
        Self {
            _dir: dir,
            root,
            config,
            run,
            home,
            path,
            env: Vec::new(),
        }
    }

    fn with_env(mut self, key: &str, value: &str) -> Self {
        self.env.push((key.to_owned(), value.to_owned()));
        self
    }

    fn command(&self, exe: &Path, args: &[&str]) -> Command {
        let mut command = Command::new(exe);
        command
            .args(args)
            .arg("--config")
            .arg(&self.config)
            .env_clear()
            .env("PATH", &self.path)
            .env("HOME", &self.home)
            .env("XDG_RUNTIME_DIR", &self.run)
            .env("HOFUND_LOG", "off")
            .envs(self.env.iter().map(|(key, value)| (key, value)))
            .stdin(Stdio::null())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped());
        command
    }

    /// Runs `hofund` with `hofundd` available beside it.
    fn hofund(&self, args: &[&str]) -> Run {
        hofundd();
        self.hofund_with(hofund(), args)
    }

    fn hofund_with(&self, exe: &Path, args: &[&str]) -> Run {
        let started = Instant::now();
        let output = finish(spawn(&mut self.command(exe, args)), DEADLINE);
        Run {
            code: output.status.code(),
            stdout: String::from_utf8_lossy(&output.stdout).into_owned(),
            stderr: String::from_utf8_lossy(&output.stderr).into_owned(),
            elapsed: started.elapsed(),
        }
    }

    fn config_text(&self) -> String {
        fs::read_to_string(&self.config).expect("read config")
    }

    fn config_id(&self) -> String {
        hofund_control_client::resolve_config_id(&self.config).expect("config id")
    }

    /// Asserts that nothing created daemon runtime state: no daemon lock,
    /// discovery file, or control socket beside the config or in the
    /// runtime root. Host state (accounts, usage, picks) under `.hofund/` is
    /// shared with the daemon and may be written by one-shot operations.
    fn assert_no_daemon_state(&self) {
        let beside = self.config.parent().expect("cfg").join(".hofund");
        let mut found = Vec::new();
        for dir in [&beside, &self.run] {
            daemon_files(dir, &mut found);
        }
        assert!(found.is_empty(), "daemon state was created: {found:?}");
    }
}

fn daemon_files(dir: &Path, found: &mut Vec<PathBuf>) {
    let Ok(entries) = fs::read_dir(dir) else {
        return;
    };
    for entry in entries {
        let path = entry.expect("entry").path();
        let name = path
            .file_name()
            .expect("name")
            .to_string_lossy()
            .into_owned();
        if path.is_dir() {
            daemon_files(&path, found);
        } else if (name.starts_with("daemon-") && name.ends_with(".lock"))
            || (name.starts_with("control-") && name.ends_with(".json"))
            || name.ends_with(".sock")
        {
            found.push(path);
        }
    }
}

#[derive(Debug)]
struct Run {
    code: Option<i32>,
    stdout: String,
    stderr: String,
    elapsed: Duration,
}

impl Run {
    #[track_caller]
    fn ok(&self) -> &str {
        assert_eq!(self.code, Some(0), "{self:#?}");
        self.stdout.trim_end()
    }

    #[track_caller]
    fn failed(&self) -> &str {
        assert_eq!(self.code, Some(1), "{self:#?}");
        self.stderr.trim_end()
    }
}

fn free_port() -> u16 {
    TcpListener::bind("127.0.0.1:0")
        .expect("bind")
        .local_addr()
        .expect("addr")
        .port()
}

/// A one-request-at-a-time HTTP server answering every request with `body`
/// and status 200; it records each request line.
struct FakeHealth {
    addr: SocketAddr,
    seen: std::sync::Arc<std::sync::Mutex<Vec<String>>>,
}

fn fake_health(body: &'static str) -> FakeHealth {
    let listener = TcpListener::bind("127.0.0.1:0").expect("bind");
    let addr = listener.local_addr().expect("addr");
    let seen = std::sync::Arc::new(std::sync::Mutex::new(Vec::new()));
    let log = seen.clone();
    std::thread::spawn(move || {
        for stream in listener.incoming() {
            let Ok(mut stream) = stream else { return };
            let _ = stream.set_read_timeout(Some(Duration::from_secs(5)));
            let mut request = Vec::new();
            let mut buf = [0_u8; 1024];
            while !request.windows(4).any(|window| window == b"\r\n\r\n") {
                match stream.read(&mut buf) {
                    Ok(0) | Err(_) => break,
                    Ok(read) => request.extend_from_slice(&buf[..read]),
                }
            }
            let text = String::from_utf8_lossy(&request);
            log.lock()
                .expect("log")
                .push(text.lines().next().unwrap_or_default().to_owned());
            let _ = write!(
                stream,
                "HTTP/1.1 200 OK\r\ncontent-type: application/json\r\ncontent-length: {}\r\nconnection: close\r\n\r\n{body}",
                body.len()
            );
        }
    });
    FakeHealth { addr, seen }
}

#[test]
fn offline_config_check_one_shot() {
    let scratch = Scratch::new("127.0.0.1:0", ROUTED);
    let out = scratch.hofund(&["config", "check"]);
    assert_eq!(out.ok(), format!("config ok: {}", scratch.config.display()));
    assert!(!out.stdout.contains("structural"), "{out:#?}");

    // Full validation runs provider construction: an unknown upstream fails.
    fs::write(
        &scratch.config,
        "listen = \"127.0.0.1:0\"\n[[routes]]\nmodel = \"m\"\nupstreams = [\"ghost\"]\n",
    )
    .expect("write");
    let out = scratch.hofund(&["config", "check"]);
    let error = out.failed();
    assert_eq!(
        error,
        format!(
            "{}: one-shot hofundd failed (exit status: 1): config: route `m` names unknown provider `ghost`",
            scratch.config.display()
        )
    );
    scratch.assert_no_daemon_state();
}

#[test]
fn offline_config_check_without_hofundd_is_labeled_structural_only() {
    let scratch = Scratch::new("127.0.0.1:0", ROUTED);
    let solo = scratch.root.join("solo");
    fs::create_dir_all(&solo).expect("mkdir");
    let cli = solo.join("hofund");
    fs::copy(hofund(), &cli).expect("copy hofund");
    let label = "structural check only; hofundd is unavailable, so provider construction and preflight did not run";

    let out = scratch.hofund_with(&cli, &["config", "check"]);
    assert_eq!(
        out.ok(),
        format!("config ok: {}\n{label}", scratch.config.display())
    );

    fs::write(&scratch.config, "listen = 7\n").expect("write");
    let out = scratch.hofund_with(&cli, &["config", "check"]);
    let error = out.failed();
    assert!(
        error.starts_with(&format!("{}: ", scratch.config.display())),
        "{error}"
    );
    assert!(error.ends_with(label), "{error}");

    // Other one-shot commands report the missing binary instead.
    let out = scratch.hofund_with(&cli, &["usage"]);
    assert!(out.failed().contains("hofundd is not"), "{out:#?}");
    scratch.assert_no_daemon_state();
}

#[test]
fn offline_config_apply_one_shot() {
    let scratch = Scratch::new("127.0.0.1:0", ROUTED);
    let next = scratch.root.join("next.toml");
    let text = format!("listen = \"127.0.0.1:0\"\ntoken_file = \"token\"\n# applied\n{ROUTED}");
    fs::write(&next, &text).expect("write");
    let out = scratch.hofund(&["config", "apply", next.to_str().expect("utf8")]);
    assert!(
        out.ok().starts_with(&format!(
            "config applied: {} (revision ",
            scratch.config.display()
        )),
        "{out:#?}"
    );
    assert_eq!(scratch.config_text(), text);

    // A candidate that does not validate leaves the file alone.
    fs::write(
        &next,
        "listen = \"127.0.0.1:0\"\n[[routes]]\nmodel = \"m\"\nupstreams = [\"ghost\"]\n",
    )
    .expect("write");
    let out = scratch.hofund(&["config", "apply", next.to_str().expect("utf8")]);
    assert!(out.failed().contains("ghost"), "{out:#?}");
    assert_eq!(scratch.config_text(), text);
    scratch.assert_no_daemon_state();
}

#[test]
fn offline_config_reload_is_reported_noop() {
    let scratch = Scratch::new("127.0.0.1:0", ROUTED);
    let before = scratch.config_text();
    let out = scratch.hofund(&["config", "reload"]);
    assert_eq!(out.ok(), "no daemon running; nothing to reload");
    assert_eq!(scratch.config_text(), before);
    scratch.assert_no_daemon_state();
}

#[test]
fn offline_status_local_report() {
    let scratch = Scratch::new("0.0.0.0:4100", ROUTED);
    let out = scratch.hofund(&["status"]);
    let report = out.ok();
    let lines: Vec<&str> = report.lines().collect();
    assert_eq!(lines[0], "daemon not running", "{report}");
    assert_eq!(lines[1], format!("config: {}", scratch.config.display()));
    assert_eq!(lines[2], format!("config id: {}", scratch.config_id()));
    let revision = lines[3].strip_prefix("disk revision: ").expect(report);
    assert!(
        !revision.is_empty() && !revision.contains("unknown"),
        "{report}"
    );
    assert_eq!(lines[4], "model address: 0.0.0.0:4100 (config)");
    assert_eq!(lines.len(), 5, "{report}");

    let scratch = scratch.with_env("HOFUND_ADDR", "127.0.0.1:4200");
    let out = scratch.hofund(&["status"]);
    assert!(
        out.ok()
            .ends_with("model address: 127.0.0.1:4200 (environment)"),
        "{out:#?}"
    );
    scratch.assert_no_daemon_state();
}

#[test]
fn offline_shutdown_one_shot() {
    let scratch = Scratch::new("127.0.0.1:0", ROUTED);
    let out = scratch.hofund(&["shutdown"]);
    assert_eq!(out.ok(), "no daemon running; nothing to shut down");
    scratch.assert_no_daemon_state();
}

#[test]
fn offline_token_show_local() {
    let scratch = Scratch::new("127.0.0.1:0", ROUTED);
    let token_path = scratch.config.parent().expect("cfg").join("token");
    assert!(!token_path.exists());
    let out = scratch.hofund_with(hofund(), &["token", "show"]);
    let shown = out.ok().to_owned();
    assert!(!shown.is_empty());
    assert_eq!(
        fs::read_to_string(&token_path).expect("token").trim(),
        shown
    );
    let again = scratch.hofund_with(hofund(), &["token", "show"]);
    assert_eq!(again.ok(), shown);
    scratch.assert_no_daemon_state();
}

#[test]
fn offline_token_rotate_one_shot() {
    let scratch = Scratch::new("127.0.0.1:0", ROUTED);
    let token_path = scratch.config.parent().expect("cfg").join("token");
    let first = scratch
        .hofund_with(hofund(), &["token", "show"])
        .ok()
        .to_owned();
    let out = scratch.hofund(&["token", "rotate"]);
    let rotated = out.ok().lines().next().expect("token line").to_owned();
    assert_ne!(rotated, first);
    assert_eq!(
        fs::read_to_string(&token_path).expect("token").trim(),
        rotated
    );
    scratch.assert_no_daemon_state();
}

#[test]
fn offline_usage_one_shot() {
    let scratch = Scratch::new("127.0.0.1:0", ROUTED);
    let out = scratch.hofund(&["usage"]);
    assert_eq!(
        out.ok(),
        "MODEL  UPSTREAM  IN  OUT  LATENCY_MS  STATUS  TIME\ntotals  requests=0  in=0  out=0"
    );
    scratch.assert_no_daemon_state();
}

#[test]
fn offline_agent_list_one_shot() {
    let scratch = Scratch::new("127.0.0.1:0", ROUTED);
    let out = scratch.hofund(&["agent", "list"]);
    let listed = out.ok();
    assert!(listed.starts_with("id\tname\tinstalled\n"), "{listed}");
    for id in ["claude-code", "codex", "gemini-cli", "opencode"] {
        assert!(
            listed
                .lines()
                .any(|line| line.starts_with(&format!("{id}\t"))),
            "{id}: {listed}"
        );
    }
    scratch.assert_no_daemon_state();
}

/// Subscription commands run in `hofundd subscription`, which links the
/// registry the CLI no longer links.
#[test]
fn offline_subscription_commands_run_in_hofundd() {
    let scratch = Scratch::new("127.0.0.1:0", ROUTED);
    let listed = scratch.hofund(&["subscription", "list"]);
    assert_eq!(listed.ok(), "subscription\tflow\taccount\tlabel\tstatus");
    let json = scratch.hofund(&["subscription", "quota", "--json"]);
    assert_eq!(json.ok(), "[]");
    let missing = scratch.hofund(&["subscription", "sign-out", "nope", "a-1"]);
    assert_eq!(missing.failed(), "no subscription `nope` in the config");
    scratch.assert_no_daemon_state();
}

fn codex_config(scratch: &Scratch) -> PathBuf {
    scratch.home.join(".codex").join("config.toml")
}

#[test]
fn offline_agent_plan_one_shot() {
    let scratch = Scratch::new("127.0.0.1:4300", ROUTED);
    let token = scratch
        .hofund_with(hofund(), &["token", "show"])
        .ok()
        .to_owned();
    let out = scratch.hofund(&["agent", "connect", "codex", "--dry-run"]);
    let plan = out.ok();
    assert!(plan.contains("127.0.0.1:4300"), "{plan}");
    assert!(!plan.contains(&token), "dry run leaked the token: {plan}");
    assert!(!codex_config(&scratch).exists(), "dry run wrote files");
    scratch.assert_no_daemon_state();
}

#[test]
fn offline_agent_connect_one_shot() {
    let scratch = Scratch::new("127.0.0.1:4301", ROUTED);
    let out = scratch.hofund(&["agent", "connect", "codex"]);
    let written = codex_config(&scratch);
    assert!(
        out.ok()
            .contains(&format!("codex: wrote {}", written.display())),
        "{out:#?}"
    );
    let text = fs::read_to_string(&written).expect("codex config");
    assert!(text.contains("127.0.0.1:4301"), "{text}");

    let out = scratch.hofund(&["agent", "connect", "nope"]);
    assert!(out.failed().contains("unknown agent `nope`"), "{out:#?}");
    scratch.assert_no_daemon_state();
}

#[test]
fn offline_agent_status_one_shot() {
    let scratch = Scratch::new("127.0.0.1:4302", ROUTED);
    let out = scratch.hofund(&["agent", "status", "codex"]);
    assert_eq!(out.ok(), "id\tstate\tmodel\ncodex\tdisconnected\t-");
    scratch.hofund(&["agent", "connect", "codex"]).ok();
    let out = scratch.hofund(&["agent", "status"]);
    let table = out.ok();
    assert!(
        table
            .lines()
            .any(|line| line.starts_with("codex\tconnected\t")),
        "{table}"
    );
    assert!(table.lines().count() >= 4, "{table}");
    let out = scratch.hofund(&["agent", "status", "nope"]);
    assert_eq!(out.failed(), "unknown agent `nope`");
    scratch.assert_no_daemon_state();
}

#[test]
fn offline_agent_disconnect_one_shot() {
    let scratch = Scratch::new("127.0.0.1:4303", ROUTED);
    let written = codex_config(&scratch);
    fs::create_dir_all(written.parent().expect("dir")).expect("mkdir");
    let original = "model = \"mine\"\n# keep me\n";
    fs::write(&written, original).expect("seed");
    scratch.hofund(&["agent", "connect", "codex"]).ok();
    assert_ne!(fs::read_to_string(&written).expect("connected"), original);
    let out = scratch.hofund(&["agent", "disconnect", "codex"]);
    assert_eq!(out.ok(), "codex: restored");
    assert_eq!(fs::read_to_string(&written).expect("restored"), original);
    let out = scratch.hofund(&["agent", "disconnect", "codex"]);
    assert_eq!(out.ok(), "codex: nothing to restore");
    scratch.assert_no_daemon_state();
}

#[test]
fn offline_agent_set_model_one_shot() {
    let scratch = Scratch::new("127.0.0.1:4304", ROUTED);
    let written = codex_config(&scratch);
    fs::create_dir_all(written.parent().expect("dir")).expect("mkdir");
    let original = "model = \"mine\"\n# keep me\n";
    fs::write(&written, original).expect("seed");
    let out = scratch.hofund(&["agent", "set", "codex", "model=m"]);
    assert_eq!(out.ok(), "codex: model m");
    let text = fs::read_to_string(&written).expect("connected");
    assert!(text.contains("model = \"m\""), "{text}");
    assert!(text.contains("127.0.0.1:4304"), "{text}");
    let out = scratch.hofund(&["agent", "set", "codex", "model=m"]);
    assert_eq!(out.ok(), "codex: model m (unchanged)");
    let out = scratch.hofund(&["agent", "set", "codex", "model=zzz"]);
    assert!(out.failed().contains("hofund model list"), "{out:#?}");
    let out = scratch.hofund(&["agent", "disconnect", "codex"]);
    assert_eq!(out.ok(), "codex: restored");
    assert_eq!(fs::read_to_string(&written).expect("restored"), original);
    scratch.assert_no_daemon_state();
}

#[test]
fn offline_agent_clear_model_one_shot() {
    let scratch = Scratch::new("127.0.0.1:4305", ROUTED);
    let written = codex_config(&scratch);
    fs::create_dir_all(written.parent().expect("dir")).expect("mkdir");
    fs::write(&written, "model = \"mine\"\n# keep me\n").expect("seed");
    let out = scratch.hofund(&["agent", "set", "codex", "model=default"]);
    assert_eq!(out.ok(), "codex: model default");
    assert_eq!(
        fs::read_to_string(&written).expect("cleared"),
        "# keep me\n"
    );
    let out = scratch.hofund(&["agent", "set", "codex", "model=default"]);
    assert_eq!(out.ok(), "codex: model default (unchanged)");
    let out = scratch.hofund(&["agent", "status", "codex"]);
    assert!(out.ok().contains("disconnected"), "{out:#?}");
    scratch.assert_no_daemon_state();
}

#[test]
fn offline_healthcheck_fails_without_daemon() {
    let port = free_port();
    let scratch = Scratch::new(&format!("127.0.0.1:{port}"), ROUTED);
    let out = scratch.hofund_with(hofund(), &["healthcheck"]);
    assert!(
        out.failed().contains(&format!("127.0.0.1:{port}")),
        "{out:#?}"
    );
    assert!(out.elapsed < Duration::from_secs(10), "{out:#?}");
    scratch.assert_no_daemon_state();
}

#[test]
fn explicit_control_endpoint_never_falls_back_to_one_shot() {
    let port = free_port();
    let scratch = Scratch::new(&format!("127.0.0.1:{}", free_port()), ROUTED)
        .with_env("HOFUND_CONTROL", &format!("127.0.0.1:{port}"));
    let out = scratch.hofund(&["usage"]);
    assert!(out.failed().starts_with("HOFUND_CONTROL:"), "{out:#?}");
    // One-shot would have created the token and the usage store.
    assert!(!scratch.config.parent().expect("cfg").join("token").exists());
    assert!(
        !scratch
            .config
            .parent()
            .expect("cfg")
            .join(".hofund")
            .exists()
    );
    scratch.assert_no_daemon_state();
}

#[test]
fn healthcheck_probes_model_gateway_not_control() {
    let gateway = fake_health("{\"status\":\"ok\",\"service\":\"hofund\"}");
    let scratch =
        Scratch::new(&gateway.addr.to_string(), ROUTED).with_env("HOFUND_CONTROL", "127.0.0.1:9");
    // No token file, no discovery, and an explicit control endpoint that
    // answers nothing: healthcheck must not touch the control plane.
    let out = scratch.hofund_with(hofund(), &["healthcheck"]);
    assert_eq!(out.ok(), format!("ok: hofund gateway at {}", gateway.addr));
    assert_eq!(
        gateway.seen.lock().expect("seen").as_slice(),
        ["GET /health HTTP/1.1"]
    );
    assert!(!scratch.config.parent().expect("cfg").join("token").exists());

    // A 200 from something that is not hofund fails.
    let other = fake_health("{\"status\":\"ok\"}");
    let scratch = Scratch::new(&other.addr.to_string(), ROUTED);
    let out = scratch.hofund_with(hofund(), &["healthcheck"]);
    assert_eq!(out.code, Some(1), "{out:#?}");
}

#[test]
fn healthcheck_rewrites_unspecified_listen_to_loopback() {
    let gateway = fake_health("{\"service\":\"hofund\"}");
    let scratch = Scratch::new(&format!("0.0.0.0:{}", gateway.addr.port()), ROUTED);
    let out = scratch.hofund_with(hofund(), &["healthcheck"]);
    assert_eq!(out.ok(), format!("ok: hofund gateway at {}", gateway.addr));
}

/// Kills the daemon if the test fails before it shuts down.
struct Daemon(Option<Child>);

impl Drop for Daemon {
    fn drop(&mut self) {
        if let Some(mut child) = self.0.take() {
            let _ = child.kill();
            let _ = child.wait();
        }
    }
}

#[test]
fn hofund_addr_overrides_listen_for_daemon_and_healthcheck() {
    let configured = free_port();
    let effective = free_port();
    let scratch = Scratch::new(&format!("127.0.0.1:{configured}"), ROUTED)
        .with_env("HOFUND_ADDR", &format!("127.0.0.1:{effective}"));
    let mut daemon_command = scratch.command(hofundd(), &[]);
    daemon_command.stdout(Stdio::null()).stderr(Stdio::null());
    let mut daemon = Daemon(Some(spawn(&mut daemon_command)));

    // The healthcheck follows HOFUND_ADDR to the daemon's model listener.
    let started = Instant::now();
    loop {
        let out = scratch.hofund_with(hofund(), &["healthcheck"]);
        if out.code == Some(0) {
            assert_eq!(
                out.stdout.trim_end(),
                format!("ok: hofund gateway at 127.0.0.1:{effective}")
            );
            break;
        }
        assert!(
            started.elapsed() < DEADLINE,
            "daemon never answered: {out:#?}"
        );
        std::thread::sleep(Duration::from_millis(100));
    }
    assert!(
        std::net::TcpStream::connect(("127.0.0.1", configured)).is_err(),
        "the daemon bound the configured address despite HOFUND_ADDR"
    );

    // Online commands find the daemon through discovery.
    let out = scratch.hofund(&["status"]);
    let report = out.ok();
    assert!(report.starts_with("daemon running (pid "), "{report}");
    assert!(
        report.contains(&format!(
            "model address: 127.0.0.1:{effective} (environment)"
        )),
        "{report}"
    );
    let out = scratch.hofund(&["config", "reload"]);
    assert!(
        out.ok().starts_with("config reloaded: generation "),
        "{out:#?}"
    );

    let out = scratch.hofund(&["shutdown"]);
    assert_eq!(out.ok(), "daemon is draining and will exit");
    let child = daemon.0.take().expect("daemon");
    let exited = finish(child, DEADLINE);
    assert!(exited.status.success(), "{exited:?}");
}

#[test]
fn one_shot_never_takes_daemon_lock_or_writes_discovery() {
    // The test holds the model port: a one-shot that tried to bind it would
    // fail, and the commands below must all succeed.
    let held = TcpListener::bind("127.0.0.1:0").expect("bind");
    let addr = held.local_addr().expect("addr");
    let scratch = Scratch::new(&addr.to_string(), ROUTED);
    scratch.hofund(&["usage"]).ok();
    scratch.hofund(&["agent", "status"]).ok();
    let next = scratch.root.join("next.toml");
    fs::write(&next, scratch.config_text() + "# one-shot write\n").expect("write");
    scratch
        .hofund(&["config", "apply", next.to_str().expect("utf8")])
        .ok();
    // No daemon lock, discovery file, socket, or `.hofund/` directory.
    scratch.assert_no_daemon_state();
    // The one-shot process is gone, so nothing watches the config: a later
    // status still reports no daemon.
    assert!(
        scratch
            .hofund(&["status"])
            .ok()
            .starts_with("daemon not running")
    );
    drop(held);
}

#[test]
fn cli_fails_actionably_when_daemon_lock_held_past_wait() {
    let scratch = Scratch::new("127.0.0.1:0", ROUTED);
    let id = scratch.config_id();
    let runtime = scratch.run.join("hofund");
    fs::create_dir_all(&runtime).expect("mkdir");
    fs::set_permissions(&runtime, fs::Permissions::from_mode(0o700)).expect("chmod");
    let lock_path = runtime.join(format!("daemon-{id}.lock"));
    let lock = fs::File::create(&lock_path).expect("lock file");
    lock.lock().expect("hold the daemon lock");
    fs::write(
        runtime.join(format!("control-{id}.json")),
        format!("{{\"pid\":4242,\"config_id\":\"{id}\"}}"),
    )
    .expect("discovery stub");

    let before = scratch.config_text();
    let next = scratch.root.join("next.toml");
    fs::write(&next, before.clone() + "# must not be written\n").expect("write");
    let out = scratch.hofund(&["config", "apply", next.to_str().expect("utf8")]);
    let error = out.failed();
    assert!(
        error.contains("the daemon is starting or unresponsive"),
        "{error}"
    );
    assert!(error.contains(&lock_path.display().to_string()), "{error}");
    assert!(error.contains("PID 4242"), "{error}");
    assert!(out.elapsed >= Duration::from_secs(5), "{out:#?}");
    // No one-shot fallback: the file is untouched, and the probe created no
    // `.hofund/` beside the config (one-shot host state would live there).
    assert_eq!(scratch.config_text(), before);
    assert!(
        !scratch
            .config
            .parent()
            .expect("cfg")
            .join(".hofund")
            .exists()
    );
    drop(lock);
}

/// Finds the running daemon's discovery file in the scratch runtime root.
fn discovery_file(scratch: &Scratch) -> PathBuf {
    let name = format!("control-{}.json", scratch.config_id());
    let candidates = [
        scratch
            .config
            .parent()
            .expect("cfg")
            .join(".hofund")
            .join(&name),
        scratch.run.join("hofund").join(&name),
    ];
    let started = Instant::now();
    loop {
        if let Some(path) = candidates.iter().find(|path| path.is_file()) {
            return path.clone();
        }
        assert!(
            started.elapsed() < DEADLINE,
            "{candidates:?} never appeared"
        );
        std::thread::sleep(Duration::from_millis(50));
    }
}

#[test]
fn cli_waits_for_discovery_when_daemon_lock_held() {
    let scratch = Scratch::new("127.0.0.1:0", ROUTED);
    let mut daemon_command = scratch.command(hofundd(), &[]);
    daemon_command.stdout(Stdio::null()).stderr(Stdio::null());
    let mut daemon = Daemon(Some(spawn(&mut daemon_command)));
    let discovery = discovery_file(&scratch);
    assert!(
        scratch
            .hofund(&["status"])
            .ok()
            .starts_with("daemon running (pid ")
    );

    // Hide discovery and the control socket while the daemon still holds
    // its lock, as during startup, and restore them partway through the
    // CLI's bounded wait.
    let socket = scratch
        .run
        .join("hofund")
        .join(format!("control-{}.sock", scratch.config_id()));
    assert!(socket.exists(), "{} missing", socket.display());
    let hidden = [
        (discovery.clone(), discovery.with_extension("hidden")),
        (socket.clone(), socket.with_extension("hidden")),
    ];
    for (from, to) in &hidden {
        fs::rename(from, to).expect("hide");
    }
    let restore = std::thread::spawn(move || {
        std::thread::sleep(Duration::from_millis(1500));
        for (from, to) in &hidden {
            fs::rename(to, from).expect("restore");
        }
    });
    let before = scratch.config_text();
    let out = scratch.hofund(&["status"]);
    restore.join().expect("restore thread");
    assert!(out.ok().starts_with("daemon running (pid "), "{out:#?}");
    assert!(out.elapsed >= Duration::from_millis(1500), "{out:#?}");
    assert!(out.elapsed < Duration::from_secs(5), "{out:#?}");
    assert_eq!(scratch.config_text(), before);

    assert_eq!(
        scratch.hofund(&["shutdown"]).ok(),
        "daemon is draining and will exit"
    );
    let exited = finish(daemon.0.take().expect("daemon"), DEADLINE);
    assert!(exited.status.success(), "{exited:?}");
}

/// A running `hofund events tail`: its stdout lines arrive on a channel.
/// Dropping it kills the tail.
struct Tail {
    child: Option<Child>,
    lines: std::sync::mpsc::Receiver<String>,
}

impl Tail {
    fn start(scratch: &Scratch) -> Self {
        hofundd();
        let mut child = spawn(&mut scratch.command(hofund(), &["events", "tail"]));
        let stdout = child.stdout.take().expect("stdout");
        let (send, lines) = std::sync::mpsc::channel();
        std::thread::spawn(move || {
            use std::io::BufRead;
            for line in std::io::BufReader::new(stdout).lines() {
                let Ok(line) = line else { break };
                if send.send(line).is_err() {
                    break;
                }
            }
        });
        Self {
            child: Some(child),
            lines,
        }
    }

    /// The next event's name, waiting up to the deadline.
    #[track_caller]
    fn next_event(&self) -> String {
        let line = self
            .lines
            .recv_timeout(DEADLINE)
            .expect("an event line before the deadline");
        let event: serde_json::Value = serde_json::from_str(&line).expect("one JSON event");
        event["payload"]["event"]
            .as_str()
            .unwrap_or_else(|| panic!("no event name in {line}"))
            .to_owned()
    }

    /// Waits for the tail to exit and returns its output.
    fn finish(mut self) -> Output {
        finish(self.child.take().expect("tail"), DEADLINE)
    }
}

impl Drop for Tail {
    fn drop(&mut self) {
        if let Some(mut child) = self.child.take() {
            let _ = child.kill();
            let _ = child.wait();
        }
    }
}

/// Kills the daemon a tail started for `scratch`, if it still runs when
/// the test ends, pass or fail. Create it right after starting the tail so
/// a failed assertion cannot leak the detached daemon. The pid is read from
/// discovery at drop time; a daemon that shut down cleanly removed its
/// discovery file, so nothing is killed and a reused pid is never hit.
struct DaemonGuard(PathBuf);

impl DaemonGuard {
    fn new(scratch: &Scratch) -> Self {
        Self(discovery_file(scratch))
    }

    /// The pid in the discovery file, once the daemon has published it.
    fn pid(&self) -> Option<String> {
        let text = fs::read_to_string(&self.0).ok()?;
        let found: serde_json::Value = serde_json::from_str(&text).ok()?;
        found["pid"].as_u64().map(|pid| pid.to_string())
    }
}

impl Drop for DaemonGuard {
    fn drop(&mut self) {
        if let Some(pid) = self.pid() {
            let _ = Command::new("kill")
                .arg(pid)
                .stdout(Stdio::null())
                .stderr(Stdio::null())
                .status();
        }
    }
}

#[test]
fn offline_events_tail_autostarts_and_retries() {
    let scratch = Scratch::new("127.0.0.1:0", ROUTED);
    let tail = Tail::start(&scratch);
    let daemon = DaemonGuard::new(&scratch);
    // With no daemon the tail starts one and prints its snapshot first.
    assert_eq!(tail.next_event(), "snapshot");
    let pid = daemon.pid().expect("discovery pid");
    let status = scratch.hofund(&["status"]);
    assert!(
        status
            .ok()
            .starts_with(&format!("daemon running (pid {pid})")),
        "{status:#?}"
    );
    let log = scratch
        .config
        .parent()
        .expect("cfg")
        .join(".hofund")
        .join(format!("daemon-{}.log", scratch.config_id()));
    assert!(log.is_file(), "{} missing", log.display());
    assert_eq!(
        fs::metadata(&log).expect("log").permissions().mode() & 0o777,
        0o600
    );

    // A token rotation ends the stream; the tail rereads the token,
    // reconnects, and resumes after the last event without a new snapshot.
    scratch.hofund(&["token", "rotate"]).ok();
    assert_eq!(tail.next_event(), "token_rotated");
    scratch.hofund(&["config", "reload"]).ok();
    assert_eq!(tail.next_event(), "config_reloaded");

    // The daemon outlives the tail that started it.
    drop(tail);
    assert!(
        scratch
            .hofund(&["status"])
            .ok()
            .starts_with("daemon running (pid ")
    );

    // A second tail finds the running daemon instead of starting another,
    // and a shutdown ends it with success.
    let second = Tail::start(&scratch);
    assert_eq!(second.next_event(), "snapshot");
    assert_eq!(
        scratch.hofund(&["shutdown"]).ok(),
        "daemon is draining and will exit"
    );
    assert_eq!(second.next_event(), "shutdown_started");
    let exited = second.finish();
    assert!(exited.status.success(), "{exited:?}");
    let stderr = String::from_utf8_lossy(&exited.stderr);
    assert!(!stderr.contains("started hofundd"), "{stderr}");

    let started = Instant::now();
    while !scratch
        .hofund(&["status"])
        .ok()
        .starts_with("daemon not running")
    {
        assert!(started.elapsed() < DEADLINE, "the daemon never exited");
        std::thread::sleep(Duration::from_millis(100));
    }
    assert_eq!(daemon.pid(), None, "discovery outlived the daemon");
}

#[test]
fn events_tail_reports_where_the_started_daemon_writes() {
    let scratch = Scratch::new("127.0.0.1:0", ROUTED);
    let tail = Tail::start(&scratch);
    let _daemon = DaemonGuard::new(&scratch);
    assert_eq!(tail.next_event(), "snapshot");
    assert_eq!(
        scratch.hofund(&["shutdown"]).ok(),
        "daemon is draining and will exit"
    );
    assert_eq!(tail.next_event(), "shutdown_started");
    let exited = tail.finish();
    assert!(exited.status.success(), "{exited:?}");
    let stderr = String::from_utf8_lossy(&exited.stderr);
    assert!(stderr.contains("started hofundd (pid "), "{stderr}");
    assert!(stderr.contains(".hofund/daemon-"), "{stderr}");
    assert!(stderr.contains("hofund shutdown stops it"), "{stderr}");
}

#[test]
fn events_tail_without_hofundd_fails() {
    let scratch = Scratch::new("127.0.0.1:0", ROUTED);
    // A copy of hofund alone, with no hofundd beside it or on PATH.
    let alone = scratch.root.join("alone");
    fs::create_dir_all(&alone).expect("mkdir");
    let exe = alone.join("hofund");
    fs::copy(hofund(), &exe).expect("copy");
    let out = scratch.hofund_with(&exe, &["events", "tail"]);
    assert!(out.failed().contains("hofundd"), "{out:#?}");
    assert!(out.stdout.is_empty(), "{out:#?}");
    scratch.assert_no_daemon_state();
}

/// A mock OpenAI-compatible upstream: `GET /v1/models` lists `models`, and
/// `POST /v1/chat/completions` answers 200 unless the requested model id
/// starts with `bad`, which gets a 500. Each request line is recorded.
struct Upstream {
    addr: SocketAddr,
    models: std::sync::Arc<std::sync::Mutex<Vec<String>>>,
    seen: std::sync::Arc<std::sync::Mutex<Vec<String>>>,
}

impl Upstream {
    fn start(models: &[&str]) -> Self {
        let listener = TcpListener::bind("127.0.0.1:0").expect("bind");
        let addr = listener.local_addr().expect("addr");
        let models = std::sync::Arc::new(std::sync::Mutex::new(
            models.iter().map(|id| (*id).to_owned()).collect::<Vec<_>>(),
        ));
        let seen = std::sync::Arc::new(std::sync::Mutex::new(Vec::new()));
        let (list, log) = (models.clone(), seen.clone());
        std::thread::spawn(move || {
            for stream in listener.incoming() {
                let Ok(stream) = stream else { return };
                let (list, log) = (list.clone(), log.clone());
                std::thread::spawn(move || answer(stream, &list, &log));
            }
        });
        Self { addr, models, seen }
    }

    fn set_models(&self, models: &[&str]) {
        *self.models.lock().expect("models") = models.iter().map(|id| (*id).to_owned()).collect();
    }

    fn seen(&self) -> Vec<String> {
        self.seen.lock().expect("seen").clone()
    }

    /// A config whose provider `chat` points here and never refreshes on
    /// its own.
    fn config(&self, auto_include: &str) -> String {
        format!(
            "[[providers]]\nid = \"chat\"\nprotocol = \"openai-chat\"\n\
             base_url = \"http://{}/v1\"\napi_key = \"provider-key\"\n\
             [providers.models]\nrefresh = \"off\"\nauto_include = [{auto_include}]\n\
             [[routes]]\nmodel = \"m\"\nupstreams = [\"chat\"]\n",
            self.addr
        )
    }
}

fn answer(
    mut stream: std::net::TcpStream,
    models: &std::sync::Mutex<Vec<String>>,
    seen: &std::sync::Mutex<Vec<String>>,
) {
    let _ = stream.set_read_timeout(Some(Duration::from_secs(5)));
    let mut request = Vec::new();
    let mut buf = [0_u8; 4096];
    let head_end = loop {
        if let Some(at) = request.windows(4).position(|window| window == b"\r\n\r\n") {
            break at + 4;
        }
        match stream.read(&mut buf) {
            Ok(0) | Err(_) => return,
            Ok(read) => request.extend_from_slice(&buf[..read]),
        }
    };
    let head = String::from_utf8_lossy(&request[..head_end]).into_owned();
    let length = head
        .lines()
        .find_map(|line| {
            let (name, value) = line.split_once(':')?;
            name.eq_ignore_ascii_case("content-length")
                .then(|| value.trim().parse::<usize>().ok())
                .flatten()
        })
        .unwrap_or(0);
    while request.len() < head_end + length {
        match stream.read(&mut buf) {
            Ok(0) | Err(_) => break,
            Ok(read) => request.extend_from_slice(&buf[..read]),
        }
    }
    let body = String::from_utf8_lossy(&request[head_end..]).into_owned();
    let line = head.lines().next().unwrap_or_default().to_owned();
    seen.lock().expect("seen").push(line.clone());
    let (status, reply) = if line.starts_with("GET /v1/models") {
        let data = models
            .lock()
            .expect("models")
            .iter()
            .map(|id| format!("{{\"id\":\"{id}\",\"object\":\"model\"}}"))
            .collect::<Vec<_>>()
            .join(",");
        (
            "200 OK",
            format!("{{\"object\":\"list\",\"data\":[{data}]}}"),
        )
    } else if body.replace(' ', "").contains("\"model\":\"bad") {
        (
            "500 Internal Server Error",
            "{\"error\":{\"message\":\"model unavailable\"}}".to_owned(),
        )
    } else {
        ("200 OK", "{\"choices\":[]}".to_owned())
    };
    let _ = write!(
        stream,
        "HTTP/1.1 {status}\r\ncontent-type: application/json\r\ncontent-length: {}\r\nconnection: close\r\n\r\n{reply}",
        reply.len()
    );
}

/// The directory holding the per-provider model state files.
fn models_dir(scratch: &Scratch) -> PathBuf {
    scratch
        .config
        .parent()
        .expect("cfg")
        .join(".hofund/host/models")
}

/// The state file of provider `chat`, once something has saved it.
fn chat_state(scratch: &Scratch) -> PathBuf {
    let dir = models_dir(scratch);
    let found = fs::read_dir(&dir)
        .unwrap_or_else(|error| panic!("{}: {error}", dir.display()))
        .map(|entry| entry.expect("entry").path())
        .filter(|path| {
            let name = path.file_name().expect("name").to_string_lossy();
            name.starts_with("chat-") && name.ends_with(".json")
        })
        .collect::<Vec<_>>();
    assert_eq!(found.len(), 1, "{found:?}");
    found[0].clone()
}

/// Holds the state lock of `chat`, as another writer would.
fn hold_chat_lock(scratch: &Scratch) -> fs::File {
    let mut name = chat_state(scratch).into_os_string();
    name.push(".lock");
    let lock = fs::OpenOptions::new()
        .read(true)
        .write(true)
        .create(true)
        .truncate(false)
        .open(PathBuf::from(name))
        .expect("lock file");
    lock.lock().expect("lock");
    lock
}

/// Starts `hofund` with `args` while the state lock is held, checks it is
/// still waiting, then releases the lock and returns its run.
fn run_past_held_lock(scratch: &Scratch, args: &[&str]) -> Run {
    hofundd();
    let lock = hold_chat_lock(scratch);
    let before = fs::read(chat_state(scratch)).expect("state");
    let started = Instant::now();
    let mut child = spawn(&mut scratch.command(hofund(), args));
    std::thread::sleep(Duration::from_secs(2));
    let waiting = child.try_wait().expect("try_wait").is_none();
    let unchanged = fs::read(chat_state(scratch)).expect("state") == before;
    drop(lock);
    let output = finish(child, DEADLINE);
    let run = Run {
        code: output.status.code(),
        stdout: String::from_utf8_lossy(&output.stdout).into_owned(),
        stderr: String::from_utf8_lossy(&output.stderr).into_owned(),
        elapsed: started.elapsed(),
    };
    assert!(
        waiting,
        "{args:?} did not wait for the state lock: {run:#?}"
    );
    assert!(
        unchanged,
        "{args:?} wrote state under another holder's lock"
    );
    run
}

#[test]
fn offline_provider_list_one_shot() {
    let scratch = Scratch::new("127.0.0.1:0", ROUTED);
    let out = scratch.hofund(&["provider", "list"]);
    let lines = out.ok().lines().collect::<Vec<_>>();
    assert_eq!(lines[0], "id\tkind\tprotocol\tready\tmodels");
    assert_eq!(lines.len(), 2, "{out:#?}");
    let row = lines[1].split('\t').collect::<Vec<_>>();
    assert_eq!(row[0], "chat");
    assert_eq!(row[2], "openai-chat");
    scratch.assert_no_daemon_state();
}

#[test]
fn offline_provider_show_one_shot() {
    let scratch = Scratch::new("127.0.0.1:0", ROUTED);
    let out = scratch.hofund(&["provider", "show", "chat"]);
    let text = out.ok();
    assert!(text.starts_with("id: chat\n"), "{text}");
    assert!(text.contains("\nprotocol: openai-chat\n"), "{text}");
    assert!(text.contains("\npicks: default"), "{text}");
    let error = scratch
        .hofund(&["provider", "show", "ghost"])
        .failed()
        .to_owned();
    assert_eq!(error, "unknown provider `ghost`");
    scratch.assert_no_daemon_state();
}

#[test]
fn offline_provider_test_one_shot() {
    let upstream = Upstream::start(&["good", "bad-one"]);
    let scratch = Scratch::new("127.0.0.1:0", &upstream.config(""));
    let out = scratch.hofund(&["provider", "test", "chat", "good"]);
    let text = out.ok();
    assert!(text.starts_with("ok\tgood\t"), "{text}");
    assert!(text.ends_with(" ms"), "{text}");

    let out = scratch.hofund(&["provider", "test", "chat", "good", "bad-one"]);
    assert_eq!(out.failed(), "chat: 1 of 2 probes failed");
    let lines = out.stdout.lines().collect::<Vec<_>>();
    assert_eq!(lines.len(), 2, "{out:#?}");
    assert!(lines[0].starts_with("ok\tgood\t"), "{out:#?}");
    assert!(lines[1].starts_with("fail\tbad-one\t500 "), "{out:#?}");
    let probes = upstream
        .seen()
        .into_iter()
        .filter(|line| line.starts_with("POST /v1/chat/completions"))
        .count();
    assert_eq!(probes, 3);
    scratch.assert_no_daemon_state();
}

#[test]
fn offline_provider_models_one_shot() {
    let upstream = Upstream::start(&["a", "b"]);
    let scratch = Scratch::new("127.0.0.1:0", &upstream.config(""));
    let out = scratch.hofund(&["provider", "models", "chat"]);
    let text = out.ok();
    assert!(text.starts_with("2 models from chat\nid: chat\n"), "{text}");
    assert!(text.contains("\nlisted: 2\n"), "{text}");
    assert_eq!(
        upstream
            .seen()
            .iter()
            .filter(|line| line.starts_with("GET /v1/models"))
            .count(),
        1
    );
    // The refetch saved the list in host state, under the state lock.
    assert!(
        fs::read_to_string(chat_state(&scratch))
            .expect("state")
            .contains("\"b\"")
    );

    upstream.set_models(&["a", "b", "c"]);
    let out = run_past_held_lock(&scratch, &["provider", "models", "chat"]);
    assert!(out.ok().starts_with("3 models from chat\n"), "{out:#?}");
    assert!(
        fs::read_to_string(chat_state(&scratch))
            .expect("state")
            .contains("\"c\"")
    );
    scratch.assert_no_daemon_state();
}

#[test]
fn offline_provider_refresh_one_shot() {
    let upstream = Upstream::start(&["a", "b"]);
    let scratch = Scratch::new("127.0.0.1:0", &upstream.config("\"new-*\""));
    scratch.hofund(&["provider", "fetch", "chat"]).ok();
    let out = scratch.hofund(&["provider", "models", "chat", "a", "b"]);
    assert!(
        out.ok().starts_with("picks: default -> a, b\nrevision: "),
        "{out:#?}"
    );

    upstream.set_models(&["b", "new-x", "other"]);
    let out = scratch.hofund(&["provider", "refresh", "chat"]);
    let text = out.ok();
    assert!(
        text.starts_with(
            "3 models from chat\n\
             + newly listed, taken by its auto-include rules: new-x\n\
             ! gone from its list, so no longer picked: a\n\
             id: chat\n"
        ),
        "{text}"
    );
    assert!(text.contains("\npicks: new-x, b\n"), "{text}");
    assert!(text.contains("\n  * new-x"), "{text}");
    assert!(text.contains("\n    other"), "{text}");

    let error = scratch.hofund(&["provider", "refresh", "chat", "x"]);
    assert_eq!(error.code, Some(2), "{error:#?}");
    assert!(
        error
            .stderr
            .contains("hofund provider models <id> <ids...> picks from it")
    );
    scratch.assert_no_daemon_state();
}

#[test]
fn one_shot_model_refresh_uses_host_state_lock() {
    let upstream = Upstream::start(&["a"]);
    let scratch = Scratch::new("127.0.0.1:0", &upstream.config(""));
    scratch.hofund(&["provider", "refresh", "chat"]).ok();
    upstream.set_models(&["a", "b"]);
    let out = run_past_held_lock(&scratch, &["provider", "refresh", "chat"]);
    assert!(out.ok().starts_with("2 models from chat\n"), "{out:#?}");
    upstream.set_models(&["a", "b", "c"]);
    let out = run_past_held_lock(&scratch, &["provider", "models", "chat"]);
    assert!(out.ok().starts_with("3 models from chat\n"), "{out:#?}");
    assert!(
        fs::read_to_string(chat_state(&scratch))
            .expect("state")
            .contains("\"c\"")
    );
}

#[test]
fn offline_provider_picks_one_shot_writes_host_state_under_lock() {
    let scratch = Scratch::new("127.0.0.1:0", ROUTED);
    let out = scratch.hofund(&["provider", "models", "chat", "a", "b"]);
    assert!(
        out.ok().starts_with("picks: default -> a, b\nrevision: "),
        "{out:#?}"
    );
    assert!(
        fs::read_to_string(chat_state(&scratch))
            .expect("state")
            .contains("\"a\"")
    );

    let out = run_past_held_lock(&scratch, &["provider", "models", "chat", "-a", "+c"]);
    assert!(
        out.ok().starts_with("picks: a, b -> b, c\nrevision: "),
        "{out:#?}"
    );
    let show = scratch.hofund(&["provider", "show", "chat"]);
    assert!(show.ok().contains("\npicks: b, c"), "{show:#?}");
    scratch.assert_no_daemon_state();
}

#[test]
fn provider_picks_edit_forms_match_reference() {
    let scratch = Scratch::new("127.0.0.1:0", ROUTED);
    let picks = |args: &[&str]| {
        let mut full = vec!["provider", "models", "chat"];
        full.extend_from_slice(args);
        scratch.hofund(&full)
    };
    assert!(
        picks(&["a", "b", "c"])
            .ok()
            .starts_with("picks: default -> a, b, c\n")
    );
    assert!(picks(&["-b"]).ok().starts_with("picks: a, b, c -> a, c\n"));
    assert!(
        picks(&["+d", "+a"])
            .ok()
            .starts_with("picks: a, c -> a, c, d\n")
    );
    assert!(
        picks(&["x", "y"])
            .ok()
            .starts_with("picks: a, c, d -> x, y\n")
    );
    let mixed = picks(&["z", "+w"]);
    assert!(mixed.failed().contains("+id or -id"), "{mixed:#?}");
    let missing = picks(&["-zz"]);
    assert!(!missing.failed().is_empty(), "{missing:#?}");
    let empty = picks(&["-x", "-y"]);
    assert!(!empty.failed().is_empty(), "{empty:#?}");
    assert!(empty.stdout.is_empty(), "{empty:#?}");
    assert!(picks(&["-"]).ok().starts_with("picks: x, y -> default\n"));
    assert!(picks(&["x"]).ok().starts_with("picks: default -> x\n"));
    assert!(picks(&["all"]).ok().starts_with("picks: x -> default\n"));
}

#[test]
fn concurrent_one_shot_picks_serialize_on_state_lock() {
    let scratch = Scratch::new("127.0.0.1:0", ROUTED);
    scratch.hofund(&["provider", "models", "chat", "a"]).ok();
    // Three writers: each loses at most two races, within the three tries.
    let children = ["+x", "+y", "+z"].map(|change| {
        spawn(&mut scratch.command(hofund(), &["provider", "models", "chat", change]))
    });
    for child in children {
        let output = finish(child, DEADLINE);
        assert!(output.status.success(), "{output:?}");
    }
    let show = scratch.hofund(&["provider", "show", "chat"]);
    let picks = show
        .ok()
        .lines()
        .find_map(|line| line.strip_prefix("picks: "))
        .expect("picks line")
        .split(", ")
        .map(str::to_owned)
        .collect::<std::collections::BTreeSet<_>>();
    assert_eq!(picks, ["a", "x", "y", "z"].map(str::to_owned).into());
}

#[test]
fn offline_provider_picks_save_failure_exits_nonzero() {
    let scratch = Scratch::new("127.0.0.1:0", ROUTED);
    // The state directory cannot be created, so neither the state file nor
    // its lock can be written beside the config.
    let host = models_dir(&scratch).parent().expect("host").to_path_buf();
    fs::create_dir_all(&host).expect("host dir");
    fs::set_permissions(&host, fs::Permissions::from_mode(0o500)).expect("chmod");
    if fs::create_dir(host.join("probe")).is_ok() {
        // Permissions do not bind this user (root); nothing to test.
        return;
    }
    let out = scratch.hofund(&["provider", "models", "chat", "a"]);
    let entries = fs::read_dir(&host).expect("host dir").count();
    fs::set_permissions(&host, fs::Permissions::from_mode(0o700)).expect("chmod back");
    let error = out.failed();
    assert!(error.contains("were not saved"), "{out:#?}");
    assert!(out.stdout.is_empty(), "{out:#?}");
    assert_eq!(entries, 0);
    // The lock fell back to the runtime directory.
    let fallback = fs::read_dir(scratch.run.join("hofund"))
        .expect("runtime dir")
        .map(|entry| {
            entry
                .expect("entry")
                .file_name()
                .to_string_lossy()
                .into_owned()
        })
        .filter(|name| name.starts_with("models-chat-") && name.ends_with(".json.lock"))
        .count();
    assert_eq!(fallback, 1);
}

#[test]
fn offline_model_list_one_shot() {
    let scratch = Scratch::new("127.0.0.1:0", ROUTED);
    let out = scratch.hofund(&["model", "list"]);
    assert!(out.ok().lines().any(|line| line == "m"), "{out:#?}");
    scratch.assert_no_daemon_state();
}

#[test]
fn offline_model_show_one_shot() {
    let scratch = Scratch::new("127.0.0.1:0", ROUTED);
    assert_eq!(
        scratch.hofund(&["model", "show", "m"]).ok(),
        "model: m\nrouted: yes"
    );
    let out = scratch.hofund(&["model", "show", "ghost"]);
    assert_eq!(out.failed(), "unknown model `ghost`: no route serves it");
    scratch.assert_no_daemon_state();
}

/// A provider entry with comments the config edits must keep.
const COMMENTED: &str = "# providers below\n[[providers]]\nid = \"chat\" # the main one\n\
protocol = \"openai-chat\"\nbase_url = \"http://127.0.0.1:9/v1\"\napi_key = \"provider-key\"\n\
[providers.models]\nrefresh = \"off\" # by hand\n[[routes]]\nmodel = \"m\"\nupstreams = [\"chat\"]\n";

const SPARE: [&str; 6] = [
    "provider",
    "add",
    "spare",
    "spare-secret-key",
    "protocol=openai-chat",
    "base_url=http://127.0.0.1:9/v1",
];

#[test]
fn offline_provider_add_one_shot() {
    let scratch = Scratch::new("127.0.0.1:0", ROUTED);
    let out = scratch.hofund(&SPARE);
    let text = out.ok();
    assert!(
        text.starts_with("added provider `spare` (revision "),
        "{text}"
    );
    assert!(!text.contains("spare-secret-key"), "{text}");
    // The new entry follows the last provider, before the routes.
    let config = scratch.config_text();
    let spare = "[[providers]]\nid = \"spare\"\napi_key = \"spare-secret-key\"\n\
protocol = \"openai-chat\"\nbase_url = \"http://127.0.0.1:9/v1\"\n";
    assert_eq!(
        config,
        format!(
            "listen = \"127.0.0.1:0\"\ntoken_file = \"token\"\n{}",
            ROUTED.replace("[[routes]]", &format!("\n{spare}[[routes]]"))
        )
    );
    let listed = scratch.hofund(&["provider", "list"]);
    assert!(listed.ok().contains("\nspare\t"), "{listed:#?}");

    // A second add of the same id is refused and writes nothing.
    let error = scratch.hofund(&SPARE).failed().to_owned();
    assert_eq!(
        error,
        "provider `spare` already exists; hofund provider set spare k=v... edits it"
    );
    assert_eq!(scratch.config_text(), config);

    // An add the daemon rejects leaves the file alone.
    let out = scratch.hofund(&["provider", "add", "bad", "quirks=deepseek"]);
    assert!(
        out.failed()
            .contains("provider `bad` needs a protocol or a preset"),
        "{out:#?}"
    );
    assert_eq!(scratch.config_text(), config);
    scratch.assert_no_daemon_state();
}

#[test]
fn offline_provider_set_one_shot() {
    let scratch = Scratch::new("127.0.0.1:0", COMMENTED);
    let out = scratch.hofund(&[
        "provider",
        "set",
        "chat",
        "max_output_tokens=2048",
        "header.X-Trace=on",
    ]);
    assert!(
        out.ok().starts_with("saved provider `chat` (revision "),
        "{out:#?}"
    );
    let config = scratch.config_text();
    assert!(
        config.contains("id = \"chat\" # the main one\n"),
        "{config}"
    );
    assert!(config.contains("max_output_tokens = 2048\n"), "{config}");
    assert!(
        config.contains("[providers.headers]\nX-Trace = \"on\"\n"),
        "{config}"
    );
    assert!(config.contains("# providers below\n"), "{config}");
    assert!(config.contains("refresh = \"off\" # by hand\n"), "{config}");

    // An empty value removes the field, and its section when emptied.
    let out = scratch.hofund(&["provider", "set", "chat", "header.X-Trace="]);
    assert!(out.ok().starts_with("saved provider `chat`"), "{out:#?}");
    let config = scratch.config_text();
    assert!(!config.contains("headers"), "{config}");

    // Asking for what is already there sends nothing.
    let out = scratch.hofund(&["provider", "set", "chat", "max_output_tokens=2048"]);
    assert_eq!(out.ok(), "provider `chat` unchanged");
    assert_eq!(scratch.config_text(), config);

    // `models=` edits the picks, not the file.
    let out = scratch.hofund(&["provider", "set", "chat", "models=a,b"]);
    assert_eq!(out.ok(), "picks: default -> a, b");
    assert_eq!(scratch.config_text(), config);
    // An empty `models=` resets the picks to the default.
    let out = scratch.hofund(&["provider", "set", "chat", "models="]);
    assert_eq!(out.ok(), "picks: a, b -> default");

    let error = scratch
        .hofund(&["provider", "set", "ghost", "quirks=x"])
        .failed()
        .to_owned();
    assert_eq!(error, "unknown provider `ghost`");
    let out = scratch.hofund(&["provider", "set", "chat", "name=x"]);
    assert_eq!(out.code, Some(2), "{out:#?}");
    assert!(
        out.stderr.starts_with("unknown provider field `name`"),
        "{out:#?}"
    );
    assert_eq!(scratch.config_text(), config);
    scratch.assert_no_daemon_state();
}

/// Asserts that every `id=` form of `provider set` fails before any I/O.
fn assert_rename_rejected(scratch: &Scratch) {
    let config = scratch.config_text();
    for args in [
        &["provider", "set", "chat", "id=renamed"][..],
        &["provider", "set", "chat", "models=a", "id=renamed"],
        &[
            "provider",
            "set",
            "chat",
            "max_output_tokens=9",
            "ID=renamed",
        ],
    ] {
        let out = scratch.hofund(args);
        assert_eq!(out.code, Some(2), "{args:?}: {out:#?}");
        assert_eq!(
            out.stderr.trim_end(),
            "renaming a provider is not supported yet; see feature-parity row 11.6"
        );
        assert_eq!(out.stdout, "");
    }
    assert_eq!(scratch.config_text(), config);
    assert!(!models_dir(scratch).exists(), "host state was written");
}

#[test]
fn provider_set_id_is_rejected_until_rename_lands() {
    let scratch = Scratch::new("127.0.0.1:0", ROUTED);
    assert_rename_rejected(&scratch);
    scratch.assert_no_daemon_state();

    // The same with a daemon running, which then takes a real edit.
    let scratch = Scratch::new(&format!("127.0.0.1:{}", free_port()), ROUTED);
    let mut daemon_command = scratch.command(hofundd(), &[]);
    daemon_command.stdout(Stdio::null()).stderr(Stdio::null());
    let mut daemon = Daemon(Some(spawn(&mut daemon_command)));
    let started = Instant::now();
    while scratch.hofund_with(hofund(), &["healthcheck"]).code != Some(0) {
        assert!(started.elapsed() < DEADLINE, "daemon never answered");
        std::thread::sleep(Duration::from_millis(100));
    }
    assert_rename_rejected(&scratch);
    let out = scratch.hofund(&["provider", "set", "chat", "max_output_tokens=9"]);
    assert!(out.ok().starts_with("saved provider `chat`"), "{out:#?}");
    assert!(scratch.config_text().contains("max_output_tokens = 9\n"));
    scratch.hofund(&["shutdown"]).ok();
    let child = daemon.0.take().expect("daemon");
    let exited = finish(child, DEADLINE);
    assert!(exited.status.success(), "{exited:?}");
}

#[test]
fn offline_provider_key_one_shot() {
    let scratch = Scratch::new("127.0.0.1:0", COMMENTED);
    let out = scratch.hofund(&["provider", "key", "chat", "chat-new-key"]);
    let text = out.ok();
    assert!(
        text.starts_with("saved the key of provider `chat` (revision "),
        "{text}"
    );
    assert!(!text.contains("chat-new-key"), "{text}");
    let config = scratch.config_text();
    assert!(config.contains("api_key = \"chat-new-key\"\n"), "{config}");
    assert!(!config.contains("provider-key"), "{config}");
    assert!(
        config.contains("id = \"chat\" # the main one\n"),
        "{config}"
    );
    let error = scratch
        .hofund(&["provider", "key", "ghost", "k"])
        .failed()
        .to_owned();
    assert_eq!(error, "unknown provider `ghost`");
    scratch.assert_no_daemon_state();
}

#[test]
fn offline_provider_rm_one_shot() {
    let scratch = Scratch::new("127.0.0.1:0", ROUTED);
    let before = scratch.config_text();
    scratch.hofund(&SPARE).ok();
    for verb in ["rm", "remove", "delete"] {
        if verb != "rm" {
            scratch.hofund(&SPARE).ok();
        }
        let out = scratch.hofund(&["provider", verb, "spare"]);
        assert!(
            out.ok().starts_with("removed provider `spare` (revision "),
            "{verb}: {out:#?}"
        );
        assert_eq!(scratch.config_text(), before, "{verb}");
    }

    // A provider a route still names stays: no agent or route is moved.
    let out = scratch.hofund(&["provider", "rm", "chat"]);
    assert!(out.failed().contains("chat"), "{out:#?}");
    assert_eq!(scratch.config_text(), before);
    let error = scratch
        .hofund(&["provider", "rm", "ghost"])
        .failed()
        .to_owned();
    assert_eq!(error, "unknown provider `ghost`");
    scratch.assert_no_daemon_state();
}

#[test]
fn offline_provider_refresh_every_one_shot() {
    let scratch = Scratch::new("127.0.0.1:0", COMMENTED);
    let out = scratch.hofund(&["provider", "refresh-every", "chat"]);
    assert_eq!(
        out.ok(),
        "chat: its model list is fetched only when asked (hofund provider refresh chat)"
    );
    let out = scratch.hofund(&["provider", "refresh-every", "chat", "6h"]);
    let text = out.ok();
    assert!(
        text.starts_with("saved provider `chat` (revision "),
        "{text}"
    );
    assert!(
        text.ends_with("\nchat: the daemon refetches its model list every 6h"),
        "{text}"
    );
    let config = scratch.config_text();
    assert!(config.contains("refresh = \"6h\" # by hand\n"), "{config}");

    // An interval the config refuses writes nothing.
    let out = scratch.hofund(&["provider", "refresh-every", "chat", "1s"]);
    assert_eq!(out.code, Some(1), "{out:#?}");
    assert_eq!(scratch.config_text(), config);

    let out = scratch.hofund(&["provider", "refresh-every", "chat", "off"]);
    assert!(
        out.ok().ends_with(
            "\nchat: its model list is fetched only when asked (hofund provider refresh chat)"
        ),
        "{out:#?}"
    );
    assert!(
        scratch
            .config_text()
            .contains("refresh = \"off\" # by hand\n")
    );
    scratch.assert_no_daemon_state();
}

#[test]
fn offline_provider_auto_include_one_shot() {
    let scratch = Scratch::new("127.0.0.1:0", COMMENTED);
    let out = scratch.hofund(&["provider", "auto-include", "chat"]);
    assert_eq!(
        out.ok(),
        "chat: no auto-include rules; a newly listed model is picked only by hand"
    );
    let out = scratch.hofund(&[
        "provider",
        "auto-include",
        "chat",
        "gpt-*",
        "re:o[0-9]{1,2}",
    ]);
    let text = out.ok();
    assert!(
        text.starts_with("saved provider `chat` (revision "),
        "{text}"
    );
    assert!(
        text.ends_with(
            "\nchat: a newly listed model is picked when one of these matches its id: gpt-* re:o[0-9]{1,2}"
        ),
        "{text}"
    );
    let config = scratch.config_text();
    assert!(
        config.contains("auto_include = [\"gpt-*\", \"re:o[0-9]{1,2}\"]\n"),
        "{config}"
    );

    // A rule the config refuses writes nothing.
    let out = scratch.hofund(&["provider", "auto-include", "chat", "re:("]);
    assert_eq!(out.code, Some(1), "{out:#?}");
    assert_eq!(scratch.config_text(), config);

    let out = scratch.hofund(&["provider", "auto-include", "chat", "none"]);
    assert!(
        out.ok().ends_with(
            "\nchat: no auto-include rules; a newly listed model is picked only by hand"
        ),
        "{out:#?}"
    );
    assert!(!scratch.config_text().contains("auto_include"));
    assert!(
        scratch
            .config_text()
            .contains("refresh = \"off\" # by hand\n")
    );
    scratch.assert_no_daemon_state();
}
