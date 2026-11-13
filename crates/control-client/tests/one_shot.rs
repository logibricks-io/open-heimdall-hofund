//! One-shot `hofundd` client: a silent child must not hang the handshake.

use std::path::{Path, PathBuf};
use std::process::Command;
use std::time::{Duration, Instant};

use hofund_control_client::Client;

/// Matches [`ONE_SHOT_HANDSHAKE_WAIT`] in the client library.
const HANDSHAKE_WAIT: Duration = Duration::from_secs(5);

fn hanging_child(dir: &Path) -> PathBuf {
    #[cfg(windows)]
    {
        let path = dir.join("hang.cmd");
        std::fs::write(
            &path,
            "@echo off\n:loop\nping -n 2 127.0.0.1 >nul\ngoto loop\n",
        )
        .expect("hang script");
        path
    }
    #[cfg(not(windows))]
    {
        let path = dir.join("hang");
        // Sleep, not `cat`: the child must keep stdout open and must not
        // exit when the client drops stdin after a failed or timed-out
        // handshake.
        std::fs::write(&path, "#!/bin/sh\nexec sleep 3600\n").expect("hang script");
        use std::os::unix::fs::PermissionsExt;
        std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o755)).expect("chmod");
        path
    }
}

#[tokio::test]
async fn one_shot_handshake_times_out_when_the_child_never_answers() {
    let dir = {
        let mut scratch = tempfile::Builder::new();
        scratch.prefix("hf-oneshot-");
        #[cfg(unix)]
        {
            scratch.tempdir_in("/tmp").expect("temp dir")
        }
        #[cfg(not(unix))]
        {
            scratch.tempdir().expect("temp dir")
        }
    };
    let exe = hanging_child(dir.path());
    let config = dir.path().join("hofund.toml");
    std::fs::write(&config, "listen = \"127.0.0.1:0\"\n").expect("config");

    // Prove the hang script itself would not exit on its own.
    let mut probe = Command::new(&exe)
        .stdin(std::process::Stdio::piped())
        .stdout(std::process::Stdio::piped())
        .stderr(std::process::Stdio::piped())
        .spawn()
        .expect("spawn hang");
    std::thread::sleep(Duration::from_millis(50));
    assert!(
        probe.try_wait().expect("try_wait").is_none(),
        "hang script exited"
    );
    let _ = probe.kill();
    let _ = probe.wait();

    let client = Client::one_shot(exe, config);
    let started = Instant::now();
    let error = tokio::time::timeout(HANDSHAKE_WAIT + Duration::from_secs(3), client.health())
        .await
        .expect("handshake timeout must return before the outer bound")
        .expect_err("silent child");
    let elapsed = started.elapsed();
    assert!(
        elapsed >= HANDSHAKE_WAIT,
        "timed out too early: {elapsed:?}"
    );
    assert!(
        elapsed < HANDSHAKE_WAIT + Duration::from_secs(3),
        "timed out too late: {elapsed:?}"
    );
    let message = error.to_string();
    assert!(
        message.contains("handshake timed out after 5s"),
        "{message}"
    );
}
