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
