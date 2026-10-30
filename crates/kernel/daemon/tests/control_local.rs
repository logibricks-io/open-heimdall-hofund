#![cfg(unix)]

//! Per-config local control lifecycle tests.

use std::path::Path;

use hofund_daemon::{ControlRuntime, StartOptions, start_with_options};

fn scratch(label: &str) -> tempfile::TempDir {
    // A short `/tmp` root keeps Unix socket paths under the macOS limit.
    let dir = tempfile::Builder::new()
        .prefix(&format!("hofund-daemon-{label}-"))
        .tempdir_in("/tmp")
        .expect("scratch directory");
    let socket = dir.path().join("control.sock");
    assert!(
        socket.as_os_str().as_encoded_bytes().len() < 100,
        "socket path must be shorter than 100 bytes: {}",
        socket.display()
    );
    dir
}

fn options(dir: &Path) -> StartOptions {
    use std::os::unix::fs::PermissionsExt;
    let xdg = dir.join("runtime");
    std::fs::create_dir_all(&xdg).expect("runtime root");
    std::fs::set_permissions(&xdg, std::fs::Permissions::from_mode(0o700))
        .expect("runtime permissions");
    StartOptions {
        control_enabled: true,
        control_runtime: Some(ControlRuntime::new(Some(xdg), dir.to_path_buf())),
        ..StartOptions::default()
    }
}

fn socket_id(socket: &Path) -> &str {
    socket
        .file_name()
        .and_then(|name| name.to_str())
        .and_then(|name| name.strip_prefix("control-"))
        .and_then(|name| name.strip_suffix(".sock"))
        .expect("control socket id")
}

#[tokio::test]
async fn concurrent_start_single_instance_lock() {
    use std::os::unix::fs::MetadataExt;
    let dir_guard = scratch("single-instance");
    let dir = dir_guard.path();
    let config = dir.join("hofund.toml");
    std::fs::write(&config, "listen = \"127.0.0.1:0\"\n").expect("config");
    let running = start_with_options(&config, options(dir))
        .await
        .expect("first daemon");
    let socket = running
        .control_socket_path()
        .expect("Unix socket")
        .to_path_buf();
    let inode = std::fs::metadata(&socket).expect("socket metadata").ino();
    let id = socket_id(&socket).to_owned();
    let discovery = dir.join(".hofund").join(format!("control-{id}.json"));
    let discovery_bytes = std::fs::read(&discovery).expect("discovery");
    let value: serde_json::Value = serde_json::from_slice(&discovery_bytes).expect("JSON");
    assert_eq!(value["api_version"], 1);
    assert_eq!(value["pid"], std::process::id());
    assert_eq!(value["config_id"], id);
    assert!(value["boot_id"].as_str().is_some_and(|id| !id.is_empty()));
    let token = std::fs::read_to_string(dir.join("hofund.token")).expect("token");
    assert!(!String::from_utf8_lossy(&discovery_bytes).contains(token.trim()));

    let Err(error) = start_with_options(&config, options(dir)).await else {
        panic!("second daemon acquired the single-instance lock");
    };
    assert_eq!(
        error.to_string(),
        format!("already served by PID {}", std::process::id())
    );
    assert_eq!(
        std::fs::metadata(&socket).expect("live socket").ino(),
        inode
    );
    running.shutdown().await;
    assert!(!discovery.exists());
    assert!(!socket.exists());
}

#[tokio::test]
async fn two_configs_in_one_directory_run_independent_daemons() {
    let dir_guard = scratch("independent");
    let dir = dir_guard.path();
    let first = dir.join("first.toml");
    let second = dir.join("second.toml");
    std::fs::write(&first, "listen = \"127.0.0.1:0\"\n").expect("first config");
    std::fs::write(&second, "listen = \"127.0.0.1:0\"\n").expect("second config");
    let first_running = start_with_options(&first, options(dir))
        .await
        .expect("first daemon");
    let second_running = start_with_options(&second, options(dir))
        .await
        .expect("second daemon");
    let first_socket = first_running.control_socket_path().expect("first socket");
    let second_socket = second_running.control_socket_path().expect("second socket");
    assert_ne!(first_socket, second_socket);
    assert_ne!(socket_id(first_socket), socket_id(second_socket));
    assert!(first_socket.exists());
    assert!(second_socket.exists());
    first_running.shutdown().await;
    assert!(second_socket.exists());
    second_running.shutdown().await;
}

#[tokio::test]
async fn read_only_config_dir_falls_back_to_runtime_dir() {
    use std::os::unix::fs::PermissionsExt;
    let root_guard = scratch("read-only");
    let root = root_guard.path();
    let dir = root.join("config");
    std::fs::create_dir(&dir).expect("config directory");
    let config = dir.join("hofund.toml");
    std::fs::write(
        &config,
        format!(
            "listen = \"127.0.0.1:0\"\ntoken_file = \"{}\"\nusage_file = \"{}\"\n",
            root.join("control.token").display(),
            root.join("usage.sqlite").display()
        ),
    )
    .expect("config");
    std::fs::write(dir.join(".hofund"), "not a directory").expect("blocked state path");
    std::fs::set_permissions(&dir, std::fs::Permissions::from_mode(0o500))
        .expect("read-only directory");
    let running = start_with_options(&config, options(root))
        .await
        .expect("daemon with runtime fallback");
    let socket = running.control_socket_path().expect("Unix socket");
    let id = socket_id(socket);
    let runtime = root.join("runtime").join("hofund");
    assert!(runtime.join(format!("daemon-{id}.lock")).exists());
    let discovery = runtime.join(format!("control-{id}.json"));
    assert!(discovery.exists());
    assert_eq!(
        std::fs::metadata(&discovery)
            .expect("discovery metadata")
            .permissions()
            .mode()
            & 0o777,
        0o600
    );
    running.shutdown().await;
    std::fs::set_permissions(&dir, std::fs::Permissions::from_mode(0o700))
        .expect("restore directory permissions");
}

#[tokio::test]
async fn restart_refuses_non_socket_control_path() {
    let dir_guard = scratch("non-socket");
    let dir = dir_guard.path();
    let config = dir.join("hofund.toml");
    std::fs::write(&config, "listen = \"127.0.0.1:0\"\n").expect("config");
    let running = start_with_options(&config, options(dir))
        .await
        .expect("first daemon");
    let socket = running
        .control_socket_path()
        .expect("Unix socket")
        .to_path_buf();
    running.shutdown().await;
    std::fs::write(&socket, "do not remove").expect("regular file");
    let Err(error) = start_with_options(&config, options(dir)).await else {
        panic!("non-socket control path was replaced");
    };
    assert!(error.to_string().contains("non-socket"), "{error}");
    assert_eq!(
        std::fs::read_to_string(&socket).expect("file"),
        "do not remove"
    );
}

#[tokio::test]
async fn unix_socket_unavailable_uses_configured_tcp() {
    let dir_guard = scratch("tcp-fallback");
    let dir = dir_guard.path();
    let config = dir.join("hofund.toml");
    std::fs::write(&config, "listen = \"127.0.0.1:0\"\n").expect("config");
    let running = start_with_options(
        &config,
        StartOptions {
            control_listen: Some("127.0.0.1:0".parse().expect("control TCP")),
            control_runtime: Some(ControlRuntime::new(None, dir.join("missing-runtime"))),
            ..StartOptions::default()
        },
    )
    .await
    .expect("TCP fallback");
    assert!(running.control_local_addr().is_some());
    assert!(running.control_socket_path().is_none());
    let discovery = std::fs::read_dir(dir.join(".hofund"))
        .expect("state directory")
        .filter_map(Result::ok)
        .map(|entry| entry.path())
        .find(|path| {
            path.file_name()
                .and_then(|name| name.to_str())
                .is_some_and(|name| name.starts_with("control-") && name.ends_with(".json"))
        })
        .expect("discovery");
    let value: serde_json::Value =
        serde_json::from_slice(&std::fs::read(discovery).expect("discovery JSON")).expect("JSON");
    assert_eq!(value["socket_path"], serde_json::Value::Null);
    assert!(value["tcp_endpoints"][0].as_str().is_some());
    running.shutdown().await;
}

#[tokio::test]
async fn overlong_unix_socket_path_uses_configured_tcp() {
    use std::os::unix::fs::PermissionsExt;
    let dir_guard = scratch("long-socket-path");
    let dir = dir_guard.path();
    let config = dir.join("hofund.toml");
    std::fs::write(&config, "listen = \"127.0.0.1:0\"\n").expect("config");
    let runtime = dir.join("r".repeat(120));
    std::fs::create_dir(&runtime).expect("long runtime root");
    std::fs::set_permissions(&runtime, std::fs::Permissions::from_mode(0o700))
        .expect("runtime permissions");
    let running = start_with_options(
        &config,
        StartOptions {
            control_listen: Some("127.0.0.1:0".parse().expect("control TCP")),
            control_runtime: Some(ControlRuntime::new(Some(runtime), dir.to_path_buf())),
            ..StartOptions::default()
        },
    )
    .await
    .expect("overlong socket path falls back to TCP");
    assert!(running.control_local_addr().is_some());
    assert!(running.control_socket_path().is_none());
    let discovery = std::fs::read_dir(dir.join(".hofund"))
        .expect("state directory")
        .filter_map(Result::ok)
        .map(|entry| entry.path())
        .find(|path| {
            path.file_name()
                .and_then(|name| name.to_str())
                .is_some_and(|name| name.starts_with("control-") && name.ends_with(".json"))
        })
        .expect("discovery");
    let value: serde_json::Value =
        serde_json::from_slice(&std::fs::read(discovery).expect("discovery JSON")).expect("JSON");
    assert_eq!(value["socket_path"], serde_json::Value::Null);
    assert!(value["tcp_endpoints"][0].as_str().is_some());
    running.shutdown().await;
}
