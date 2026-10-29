//! Signal handling of the blocking daemon entry point, through a re-executed
//! copy of this test binary.
#![cfg(unix)]

use std::os::unix::process::ExitStatusExt;
use std::path::{Path, PathBuf};
use std::process::{Child, Command, ExitStatus, Stdio};
use std::time::{Duration, Instant};

const HELPER: &str = "HOFUND_TERMINATE_HELPER";
const CONFIG: &str = "listen = \"127.0.0.1:0\"\n\
[[providers]]\nid = \"chat\"\nprotocol = \"openai-chat\"\nbase_url = \"http://127.0.0.1:9/v1\"\napi_key = \"provider-key\"\n\
[[routes]]\nmodel = \"m\"\nupstreams = [\"chat\"]\n";
const SIGHUP: i32 = 1;
const SIGINT: i32 = 2;
const SIGTERM: i32 = 15;

/// The re-executed daemon. It does nothing in a normal test run.
#[test]
fn terminate_helper_entry() {
    let Some(config) = std::env::var_os(HELPER) else {
        return;
    };
    let result = hofund_daemon::serve_blocking(Path::new(&config));
    panic!("serve_blocking returned after a signal: {result:?}");
}

struct Daemon {
    child: Child,
    stderr: PathBuf,
    _dir: tempfile::TempDir,
}

enum Launch {
    Foreground,
    Detached,
    ClosedOutput,
    IgnoringHangup,
}

fn launch(how: Launch) -> Daemon {
    let dir = tempfile::Builder::new()
        .prefix("hofund-terminate-")
        .tempdir()
        .expect("scratch");
    let config = dir.path().join("hofund.toml");
    std::fs::write(&config, CONFIG).expect("config");
    let stderr = dir.path().join("stderr.log");
    let exe = std::env::current_exe().expect("test binary");
    let args = [
        "terminate_helper_entry",
        "--exact",
        "--nocapture",
        "--test-threads=1",
    ];
    // A runner started under `nohup` passes SIGHUP down as ignored, which
    // the daemon must keep; GNU env restores the default for the helper.
    let mut program: Vec<std::ffi::OsString> = Vec::new();
    if cfg!(target_os = "linux") && !matches!(how, Launch::IgnoringHangup) {
        program.extend(["env".into(), "--default-signal=HUP".into()]);
    }
    match how {
        Launch::Detached if Path::new("/usr/bin/setsid").exists() => {
            program.push("/usr/bin/setsid".into());
        }
        Launch::IgnoringHangup => {
            program.extend([
                "/bin/sh".into(),
                "-c".into(),
                "trap '' HUP; exec \"$0\" \"$@\"".into(),
            ]);
        }
        _ => {}
    }
    program.push(exe.into());
    let mut command = Command::new(&program[0]);
    command.args(&program[1..]);
    command
        .args(args)
        .env(HELPER, &config)
        .env_remove("HOFUND_ADDR")
        .stdin(Stdio::null());
    match how {
        Launch::ClosedOutput => {
            command.stdout(Stdio::piped()).stderr(Stdio::piped());
        }
        _ => {
            command
                .stdout(Stdio::null())
                .stderr(std::fs::File::create(&stderr).expect("stderr file"));
        }
    }
    let mut child = command.spawn().expect("spawn helper");
    if matches!(how, Launch::ClosedOutput) {
        let deadline = Instant::now() + Duration::from_secs(60);
        let mut seen = Vec::new();
        let mut pipe = child.stderr.take().expect("stderr pipe");
        while !String::from_utf8_lossy(&seen).contains("listening on") {
            assert!(Instant::now() < deadline, "helper never listened");
            let mut byte = [0_u8; 256];
            let read = std::io::Read::read(&mut pipe, &mut byte).expect("read stderr");
            assert!(read > 0, "helper stderr closed early");
            seen.extend_from_slice(&byte[..read]);
        }
        drop(pipe);
        drop(child.stdout.take());
    }
    let daemon = Daemon {
        child,
        stderr,
        _dir: dir,
    };
    if !matches!(how, Launch::ClosedOutput) {
        daemon.wait_for_stderr("listening on");
    }
    daemon
}

impl Daemon {
    fn log(&self) -> String {
        std::fs::read_to_string(&self.stderr).unwrap_or_default()
    }

    fn wait_for_stderr(&self, needle: &str) {
        let deadline = Instant::now() + Duration::from_secs(60);
        while !self.log().contains(needle) {
            assert!(
                Instant::now() < deadline,
                "no `{needle}` in stderr: {}",
                self.log()
            );
            std::thread::sleep(Duration::from_millis(20));
        }
    }

    fn signal(&self, signal: i32) {
        let status = Command::new("kill")
            .arg(format!("-{signal}"))
            .arg(self.child.id().to_string())
            .status()
            .expect("kill");
        assert!(status.success(), "kill -{signal}");
    }

    fn wait(&mut self, within: Duration) -> Option<ExitStatus> {
        let deadline = Instant::now() + within;
        loop {
            if let Some(status) = self.child.try_wait().expect("try_wait") {
                return Some(status);
            }
            if Instant::now() >= deadline {
                return None;
            }
            std::thread::sleep(Duration::from_millis(20));
        }
    }

    fn exit(&mut self) -> ExitStatus {
        match self.wait(Duration::from_secs(60)) {
            Some(status) => status,
            None => {
                let _ = self.child.kill();
                panic!("helper did not exit; stderr: {}", self.log());
            }
        }
    }
}

impl Drop for Daemon {
    fn drop(&mut self) {
        let _ = self.child.kill();
        let _ = self.child.wait();
    }
}

fn assert_drained_then_reraised(how: Launch, signal: i32) {
    let mut daemon = launch(how);
    daemon.signal(signal);
    let status = daemon.exit();
    assert_eq!(
        status.signal(),
        Some(signal),
        "status {status:?}; stderr: {}",
        daemon.log()
    );
    let log = daemon.log();
    let draining = log.find(&format!("draining: signal {signal}"));
    let stopped = log.find("stopped");
    assert!(
        draining.is_some() && stopped > draining,
        "drain lines in order: {log}"
    );
}

#[test]
fn sighup_drains_then_reraises_default_disposition() {
    assert_drained_then_reraised(Launch::Foreground, SIGHUP);
}

#[test]
fn sighup_drains_then_reraises_default_disposition_when_detached() {
    assert_drained_then_reraised(Launch::Detached, SIGHUP);
}

#[test]
fn sigint_and_sigterm_drain_then_reraise() {
    assert_drained_then_reraised(Launch::Foreground, SIGINT);
    assert_drained_then_reraised(Launch::Foreground, SIGTERM);
}

#[test]
fn sighup_drain_with_closed_stdout_does_not_panic() {
    let mut daemon = launch(Launch::ClosedOutput);
    daemon.signal(SIGHUP);
    let status = daemon.exit();
    assert_eq!(status.signal(), Some(SIGHUP), "status {status:?}");
}

#[cfg(target_os = "linux")]
#[test]
fn sighup_ignored_at_start_stays_ignored() {
    let mut daemon = launch(Launch::IgnoringHangup);
    daemon.signal(SIGHUP);
    assert_eq!(
        daemon.wait(Duration::from_millis(750)),
        None,
        "an ignored SIGHUP must not stop the daemon; stderr: {}",
        daemon.log()
    );
    assert!(!daemon.log().contains("draining"), "{}", daemon.log());
    daemon.signal(SIGTERM);
    let status = daemon.exit();
    assert_eq!(status.signal(), Some(SIGTERM), "status {status:?}");
}
