//! `hofund serve` runs `hofundd` from a real install layout.
//!
//! Each test copies the built `hofund` into a scratch directory, because the
//! CLI looks beside its own resolved executable. The fake `hofundd` is a
//! shell script that records its PID, arguments, and one environment value,
//! then exits 7.

#![cfg(unix)]

use std::fs;
use std::os::unix::fs::PermissionsExt;
use std::path::{Path, PathBuf};
use std::process::{Command, Output, Stdio};

const FAKE: &str = "#!/bin/sh\n\
{ printf 'pid=%s\\n' \"$$\"; printf 'arg=%s\\n' \"$@\"; \
printf 'marker=%s\\n' \"$HOFUND_TEST_MARKER\"; \
printf 'config_env=%s\\n' \"$HOFUND_CONFIG\"; printf 'from=%s\\n' \"$FROM\"; } \
> \"$HOFUND_TEST_OUT\"\nexit 7\n";

fn install_cli(dir: &Path) -> PathBuf {
    fs::create_dir_all(dir).expect("mkdir");
    let cli = dir.join("hofund");
    fs::copy(env!("CARGO_BIN_EXE_hofund"), &cli).expect("copy hofund");
    cli
}

fn install_fake(dir: &Path, from: &str) {
    fs::create_dir_all(dir).expect("mkdir");
    let file = dir.join("hofundd");
    fs::write(&file, FAKE.replace("$FROM", from)).expect("write fake");
    fs::set_permissions(&file, fs::Permissions::from_mode(0o755)).expect("chmod");
}

struct Run {
    pid: u32,
    output: Output,
    record: Option<String>,
}

fn serve(cli: &Path, path: &Path, out: &Path, extra: &[&str]) -> Run {
    let mut command = Command::new(cli);
    command
        .arg("serve")
        .args(extra)
        .env_clear()
        .env("PATH", path)
        .env("HOFUND_TEST_MARKER", "kept")
        .env("HOFUND_CONFIG", "/from/env.toml")
        .env("HOFUND_TEST_OUT", out)
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped());
    let child = command.spawn().expect("spawn hofund");
    let pid = child.id();
    let output = child.wait_with_output().expect("wait hofund");
    let record = fs::read_to_string(out).ok();
    let _ = fs::remove_file(out);
    Run {
        pid,
        output,
        record,
    }
}

// One test function on purpose: writing a script while another test thread
// forks can leave a write handle open in that child, and exec then fails with
// "Text file busy". Running the steps in sequence avoids that race.
#[test]
fn serve_execs_the_daemon_from_the_install_layout() {
    let scratch = tempfile::tempdir().expect("tempdir");
    let root = scratch.path();
    let out = root.join("record.txt");
    let empty = root.join("empty");
    fs::create_dir_all(&empty).expect("mkdir");

    // Beside the CLI: exact arguments, inherited environment, same process.
    let paired = root.join("paired");
    let cli = install_cli(&paired);
    install_fake(&paired, "beside");
    let on_path = root.join("on-path");
    install_fake(&on_path, "path");
    let run = serve(&cli, &on_path, &out, &["--config", "/explicit/hofund.toml"]);
    assert_eq!(run.output.status.code(), Some(7), "{:?}", run.output);
    let record = run.record.expect("fake hofundd ran");
    assert_eq!(
        record,
        format!(
            "pid={}\narg=--config\narg=/explicit/hofund.toml\nmarker=kept\n\
             config_env=/from/env.toml\nfrom=beside\n",
            run.pid
        )
    );

    // Without --config, the CLI resolves HOFUND_CONFIG and passes it once.
    let run = serve(&cli, &on_path, &out, &[]);
    assert_eq!(run.output.status.code(), Some(7), "{:?}", run.output);
    let record = run.record.expect("fake hofundd ran");
    assert!(
        record.contains("\narg=--config\narg=/from/env.toml\nmarker=kept\n"),
        "{record}"
    );
    assert_eq!(record.matches("arg=").count(), 2, "{record}");

    // Nothing beside the CLI: PATH supplies the daemon.
    let solo = root.join("solo");
    let cli = install_cli(&solo);
    let run = serve(&cli, &on_path, &out, &["--config", "/explicit/hofund.toml"]);
    assert_eq!(run.output.status.code(), Some(7), "{:?}", run.output);
    let record = run.record.expect("fake hofundd ran");
    assert!(record.starts_with(&format!("pid={}\n", run.pid)), "{record}");
    assert!(record.ends_with("from=path\n"), "{record}");

    // Neither place: an actionable error and exit 1.
    let run = serve(&cli, &empty, &out, &["--config", "/explicit/hofund.toml"]);
    assert_eq!(run.output.status.code(), Some(1), "{:?}", run.output);
    assert!(run.record.is_none());
    let stderr = String::from_utf8_lossy(&run.output.stderr);
    let solo = fs::canonicalize(&solo).expect("canonical");
    assert!(
        stderr.contains(&format!("at {}", solo.join("hofundd").display())),
        "{stderr}"
    );
    assert!(stderr.contains("not on PATH"), "{stderr}");
    assert!(stderr.contains("hofund-daemon-bin"), "{stderr}");

    // --in-process keeps the old path and never starts hofundd.
    let missing = root.join("missing.toml");
    let missing = missing.to_str().expect("utf-8 path");
    let cli = paired.join("hofund");
    let run = serve(&cli, &on_path, &out, &["--in-process", "--config", missing]);
    assert_eq!(run.output.status.code(), Some(1), "{:?}", run.output);
    assert!(run.record.is_none(), "hofundd must not run");
    let stderr = String::from_utf8_lossy(&run.output.stderr);
    assert!(stderr.starts_with("cannot read config"), "{stderr}");
}
