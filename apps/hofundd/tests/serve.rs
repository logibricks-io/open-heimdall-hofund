//! The `hofundd` process: startup errors, local control, and signal exit.

use std::path::{Path, PathBuf};
use std::process::{Child, Command, Output, Stdio};
use std::time::{Duration, Instant};

const CONFIG: &str = "listen = \"127.0.0.1:0\"\n\
[[providers]]\nid = \"chat\"\nprotocol = \"openai-chat\"\nbase_url = \"http://127.0.0.1:9/v1\"\napi_key = \"provider-key\"\n\
[[routes]]\nmodel = \"m\"\nupstreams = [\"chat\"]\n";

fn hofundd(scratch: &Path) -> Command {
    let mut command = Command::new(env!("CARGO_BIN_EXE_hofundd"));
    command
        .env_remove("HOFUND_ADDR")
        .env_remove("HOFUND_CONFIG")
        .env_remove("HOFUND_LOG")
        .env("XDG_RUNTIME_DIR", scratch)
        .env("LOCALAPPDATA", scratch)
        .stdin(Stdio::null());
    command
}

fn scratch() -> tempfile::TempDir {
    tempfile::Builder::new()
        .prefix("hofundd-")
        .tempdir()
        .expect("scratch")
}

fn run(command: &mut Command) -> Output {
    command.output().expect("run hofundd")
}

fn stderr(output: &Output) -> String {
    String::from_utf8_lossy(&output.stderr).into_owned()
}

#[test]
fn version_and_help_go_to_stdout() {
    let dir = scratch();
    let output = run(hofundd(dir.path()).arg("--version"));
    assert!(output.status.success());
    assert_eq!(
        String::from_utf8_lossy(&output.stdout),
        format!("hofundd {}\n", env!("CARGO_PKG_VERSION"))
    );
    let output = run(hofundd(dir.path()).arg("--help"));
    assert!(output.status.success());
    assert!(String::from_utf8_lossy(&output.stdout).contains("Usage: hofundd [--config <path>]"));
}

#[test]
fn a_bad_command_line_exits_two() {
    let dir = scratch();
    let output = run(hofundd(dir.path()).arg("--bogus"));
    assert_eq!(output.status.code(), Some(2));
    assert!(
        stderr(&output).starts_with("hofundd: unexpected argument `--bogus`"),
        "{}",
        stderr(&output)
    );
}

#[test]
fn a_bad_log_level_exits_two_before_reading_the_config() {
    let dir = scratch();
    let output = run(hofundd(dir.path())
        .env("HOFUND_LOG", "loud")
        .args(["--config".as_ref(), dir.path().join("absent.toml").as_os_str()]));
    assert_eq!(output.status.code(), Some(2));
    assert!(stderr(&output).contains("HOFUND_LOG=loud"), "{}", stderr(&output));
}

#[test]
fn a_missing_config_is_a_startup_failure() {
    let dir = scratch();
    let output = run(hofundd(dir.path())
        .args(["--config".as_ref(), dir.path().join("absent.toml").as_os_str()]));
    assert_eq!(output.status.code(), Some(1));
    let text = stderr(&output);
    assert!(text.starts_with("hofundd: "), "{text}");
    assert!(!text.contains("listening on"), "{text}");
}

#[test]
fn hofund_config_selects_the_file_without_a_flag() {
    let dir = scratch();
    let absent = dir.path().join("from-env.toml");
    let output = run(hofundd(dir.path()).env("HOFUND_CONFIG", &absent));
    assert_eq!(output.status.code(), Some(1));
    let text = stderr(&output);
    assert!(text.starts_with("hofundd: "), "{text}");
}

struct Daemon {
    child: Child,
    log: PathBuf,
}

impl Daemon {
    fn start(dir: &Path, config: &Path) -> Self {
        let log = dir.join(format!("stderr-{}.log", std::process::id()));
        let child = hofundd(dir)
            .arg("--config")
            .arg(config)
            .stdout(Stdio::null())
            .stderr(std::fs::File::create(&log).expect("stderr file"))
            .spawn()
            .expect("spawn hofundd");
        let daemon = Self { child, log };
        let deadline = Instant::now() + Duration::from_secs(60);
        while !daemon.text().contains("listening on http://127.0.0.1:") {
            assert!(Instant::now() < deadline, "never listened: {}", daemon.text());
            std::thread::sleep(Duration::from_millis(20));
        }
        daemon
    }

    fn text(&self) -> String {
        std::fs::read_to_string(&self.log).unwrap_or_default()
    }
}

impl Drop for Daemon {
    fn drop(&mut self) {
        let _ = self.child.kill();
        let _ = self.child.wait();
    }
}

#[test]
fn serves_with_local_control_held_for_the_config() {
    let dir = scratch();
    let config = dir.path().join("hofund.toml");
    std::fs::write(&config, CONFIG).expect("config");
    let daemon = Daemon::start(dir.path(), &config);
    let second = run(hofundd(dir.path()).arg("--config").arg(&config));
    assert_eq!(second.status.code(), Some(1), "{}", stderr(&second));
    let text = stderr(&second);
    assert!(
        text.starts_with("hofundd: ") && text.contains("already served by PID"),
        "{text}"
    );
    assert!(text.contains(&daemon.child.id().to_string()), "{text}");
}

#[cfg(unix)]
#[test]
fn sigterm_drains_and_reraises() {
    use std::os::unix::process::ExitStatusExt;

    let dir = scratch();
    let config = dir.path().join("hofund.toml");
    std::fs::write(&config, CONFIG).expect("config");
    let mut daemon = Daemon::start(dir.path(), &config);
    let status = Command::new("kill")
        .arg("-15")
        .arg(daemon.child.id().to_string())
        .status()
        .expect("kill");
    assert!(status.success());
    let deadline = Instant::now() + Duration::from_secs(60);
    let status = loop {
        if let Some(status) = daemon.child.try_wait().expect("try_wait") {
            break status;
        }
        assert!(Instant::now() < deadline, "did not exit: {}", daemon.text());
        std::thread::sleep(Duration::from_millis(20));
    };
    assert_eq!(status.signal(), Some(15), "{status:?}: {}", daemon.text());
    let text = daemon.text();
    assert!(text.contains("draining: signal 15"), "{text}");
    assert!(text.contains("stopped"), "{text}");
}
