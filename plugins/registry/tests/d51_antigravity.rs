//! Task D-51: Antigravity desktop agent adapter regressions.

use std::ffi::OsString;
use std::fs;
use std::path::{Path, PathBuf};

use hofund_agent::{
    AgentAdapter, Compiled, GatewayTarget, Session, TargetOs, resolve, resolve_agent_path,
};
use hofund_plugin_api::AgentPath;
use serde_json::Value;

const BASE: &str = "http://127.0.0.1:3425";
const TOKEN: &str = "hofund-agent-token";
const ROUTE: &str = "route-a";

const OPERATING_SYSTEMS: [TargetOs; 3] = [TargetOs::Linux, TargetOs::Macos, TargetOs::Windows];

fn scratch(name: &str) -> PathBuf {
    let dir = tempfile::tempdir().expect("temp dir");
    let path = dir.keep().join(name);
    fs::create_dir_all(&path).expect("mkdir scratch");
    path
}

fn adapter(os: TargetOs) -> Compiled {
    Compiled::new(hofund_plugin_registry::agent("antigravity").expect("antigravity agent"))
        .with_os(os)
}

fn target(base_url: &str, token: &str) -> GatewayTarget {
    GatewayTarget::new(base_url, token, vec![ROUTE.to_owned()])
}

fn settings_path(home: &Path, os: TargetOs) -> PathBuf {
    let description = hofund_plugin_registry::agent("antigravity").expect("agent");
    let file = description
        .files()
        .iter()
        .find(|file| file.id() == "settings")
        .expect("settings file");
    resolve(file, os, &Session::new(home))
}

fn backend_marker(home: &Path, os: TargetOs) -> PathBuf {
    let path = AgentPath::from_static(
        "~/.gemini/antigravity/agyhub_summaries_proto.pb",
        "~/.gemini/antigravity/agyhub_summaries_proto.pb",
        "~/.gemini/antigravity/agyhub_summaries_proto.pb",
    );
    resolve_agent_path(&path, os, &Session::new(home))
}

fn parse_json(path: &Path) -> Value {
    serde_json::from_str(&fs::read_to_string(path).expect("read settings")).expect("json")
}

#[test]
fn d51_1_connect_on_empty_home_writes_gateway_default_model_and_no_list() {
    for os in OPERATING_SYSTEMS {
        let home = scratch("d51-1");
        let session = Session::new(&home);
        let agent = adapter(os);
        let path = settings_path(&home, os);
        agent
            .apply(&session, &target(BASE, TOKEN))
            .expect("connect");
        let json = parse_json(&path);
        assert_eq!(json["gateway"]["enabled"], true);
        assert_eq!(json["gateway"]["url"], BASE);
        assert_eq!(json["gateway"]["apiKey"], TOKEN);
        assert_eq!(json["gateway"]["wireProtocol"], "openai");
        assert_eq!(json["models"]["defaultModelId"], ROUTE);
        assert!(json["models"].get("list").is_none(), "{os:?}");
        let _ = fs::remove_dir_all(home);
    }
}

#[test]
fn d51_2_disconnect_restores_user_gateway_model_and_list() {
    for os in OPERATING_SYSTEMS {
        let home = scratch("d51-2");
        let path = settings_path(&home, os);
        fs::create_dir_all(path.parent().expect("parent")).expect("mkdir");
        let original = r#"{
  "theme": "dark",
  "gateway": {
    "enabled": true,
    "url": "https://user.example.test",
    "apiKey": "user-key",
    "wireProtocol": "openai",
    "caCertPath": "/etc/certs/user.pem"
  },
  "models": {
    "defaultModelId": "user-model",
    "list": [{"modelId": "user-model"}]
  }
}"#;
        fs::write(&path, original).expect("seed");
        let session = Session::new(&home);
        let agent = adapter(os);
        agent
            .apply(&session, &target(BASE, TOKEN))
            .expect("connect");
        let connected = parse_json(&path);
        assert!(connected["models"].get("list").is_none(), "{os:?}");
        assert_eq!(connected["gateway"]["url"], BASE, "{os:?}");
        assert_eq!(connected["gateway"]["apiKey"], TOKEN, "{os:?}");
        assert_eq!(
            connected["gateway"]["caCertPath"], "/etc/certs/user.pem",
            "{os:?}"
        );
        assert_eq!(connected["models"]["defaultModelId"], ROUTE, "{os:?}");
        assert_eq!(connected["theme"], "dark", "{os:?}");
        agent.disconnect(&session, false).expect("disconnect");
        let restored = fs::read_to_string(&path).expect("restored");
        let expected: Value = serde_json::from_str(original).expect("original json");
        let actual: Value = serde_json::from_str(&restored).expect("restored json");
        assert_eq!(actual, expected, "{os:?}");
        let _ = fs::remove_dir_all(home);
    }
}

#[test]
fn d51_3_clear_user_model_restores_default_model_and_list() {
    for os in OPERATING_SYSTEMS {
        let home = scratch("d51-3");
        let path = settings_path(&home, os);
        fs::create_dir_all(path.parent().expect("parent")).expect("mkdir");
        fs::write(
            &path,
            r#"{
  "gateway": {"enabled": true, "url": "https://user.example.test", "apiKey": "k", "wireProtocol": "openai"},
  "models": {"defaultModelId": "user-model", "list": [{"modelId": "user-model"}]}
}"#,
        )
        .expect("seed");
        let session = Session::new(&home);
        let agent = adapter(os);
        agent
            .apply(&session, &target(BASE, TOKEN))
            .expect("connect");
        agent
            .set_model(&session, &target(BASE, TOKEN), Some(ROUTE))
            .expect("pick route");
        agent
            .set_model(&session, &target(BASE, TOKEN), None)
            .expect("clear to user model");
        let json = parse_json(&path);
        assert_eq!(json["models"]["defaultModelId"], "user-model");
        assert_eq!(json["models"]["list"][0]["modelId"], "user-model");
        let _ = fs::remove_dir_all(home);
    }
}

#[test]
fn d51_8_clear_after_explicit_pick_restores_user_default_without_list() {
    for os in OPERATING_SYSTEMS {
        let home = scratch("d51-8");
        let path = settings_path(&home, os);
        fs::create_dir_all(path.parent().expect("parent")).expect("mkdir");
        fs::write(
            &path,
            r#"{
  "gateway": {"enabled": true, "url": "https://user.example.test", "apiKey": "k", "wireProtocol": "openai"},
  "models": {"defaultModelId": "user-model"}
}"#,
        )
        .expect("seed");
        let session = Session::new(&home);
        let agent = adapter(os);
        agent
            .apply(&session, &target(BASE, TOKEN))
            .expect("connect");
        agent
            .set_model(&session, &target(BASE, TOKEN), Some(ROUTE))
            .expect("pick route");
        agent
            .set_model(&session, &target(BASE, TOKEN), None)
            .expect("clear to user model");
        let json = parse_json(&path);
        assert_eq!(json["models"]["defaultModelId"], "user-model", "{os:?}");
        assert!(json["models"].get("list").is_none(), "{os:?}");
        let _ = fs::remove_dir_all(home);
    }
}

#[test]
fn d51_9_explicit_pick_among_several_routes_writes_that_route() {
    const OTHER: &str = "route-b";
    for os in OPERATING_SYSTEMS {
        let home = scratch("d51-9");
        let path = settings_path(&home, os);
        let session = Session::new(&home);
        let agent = adapter(os);
        let routes = GatewayTarget::new(BASE, TOKEN, vec![ROUTE.to_owned(), OTHER.to_owned()]);
        agent.apply(&session, &routes).expect("connect");
        agent
            .set_model(&session, &routes, Some(OTHER))
            .expect("pick second route");
        let json = parse_json(&path);
        assert_eq!(json["models"]["defaultModelId"], OTHER, "{os:?}");
        let _ = fs::remove_dir_all(home);
    }
}

#[test]
fn d51_4_reapply_keeps_user_gateway_keys_and_updates_moved_url() {
    for os in OPERATING_SYSTEMS {
        let home = scratch("d51-4");
        let path = settings_path(&home, os);
        fs::create_dir_all(path.parent().expect("parent")).expect("mkdir");
        fs::write(&path, "{}\n").expect("seed");
        let session = Session::new(&home);
        let agent = adapter(os);
        agent
            .apply(&session, &target(BASE, TOKEN))
            .expect("connect");
        let mut json = parse_json(&path);
        json["gateway"]["extra"] = Value::String("keep".into());
        fs::write(&path, serde_json::to_string_pretty(&json).expect("pretty")).expect("user key");
        let moved = "http://127.0.0.1:9999";
        agent
            .apply(&session, &target(moved, "rotated-token"))
            .expect("reapply");
        let json = parse_json(&path);
        assert_eq!(json["gateway"]["extra"], "keep");
        assert_eq!(json["gateway"]["url"], moved);
        assert_eq!(json["gateway"]["apiKey"], "rotated-token");
        let _ = fs::remove_dir_all(home);
    }
}

fn admin_path(session: &Session, os: TargetOs) -> PathBuf {
    let description = hofund_plugin_registry::agent("antigravity").expect("agent");
    let check = description
        .status_checks()
        .iter()
        .find_map(|check| match check {
            hofund_plugin_api::AgentStatusCheck::AdminGateway { path, .. } => Some(*path),
            _ => None,
        })
        .expect("admin check");
    resolve_agent_path(&check, os, session)
}

fn session_with_agent_root(home: &Path) -> Session {
    Session::new(home).with_system_root(home)
}

#[test]
fn d51_5_status_reports_admin_env_and_drift() {
    for os in OPERATING_SYSTEMS {
        let home = scratch("d51-5");
        let path = settings_path(&home, os);
        fs::create_dir_all(path.parent().expect("parent")).expect("mkdir");
        fs::write(&path, "{}\n").expect("seed");
        let session = session_with_agent_root(&home);
        let agent = adapter(os);
        let connected_target = target(BASE, TOKEN);
        agent.apply(&session, &connected_target).expect("connect");
        let connected_json = parse_json(&path);
        assert_eq!(connected_json["gateway"]["url"], BASE);

        let admin = admin_path(&session, os);
        if let Some(parent) = admin.parent() {
            fs::create_dir_all(parent).expect("admin dir");
        }
        fs::write(&admin, r#"{"gateway":{"url":"https://corp.example.test"}}"#).expect("admin");
        assert!(
            !agent
                .status(&session, &connected_target)
                .expect("admin status")
                .connected(),
            "{os:?} admin"
        );
        let _ = fs::remove_file(&admin);

        let session = session_with_agent_root(&home).with_env(
            OsString::from("AGY_LLM_GATEWAY_URL"),
            OsString::from("https://env.example.test"),
        );
        assert!(
            !agent
                .status(&session, &connected_target)
                .expect("env status")
                .connected(),
            "{os:?} env"
        );

        let mut json = parse_json(&path);
        json["gateway"]["url"] = Value::String("http://127.0.0.1:1".into());
        fs::write(&path, serde_json::to_string(&json).expect("json")).expect("drift");
        assert!(
            !agent
                .status(&session, &connected_target)
                .expect("drift status")
                .connected(),
            "{os:?} drift"
        );
        assert!(
            agent
                .status(&session, &connected_target)
                .expect("drift managed")
                .managed(),
            "{os:?} managed"
        );
        let _ = fs::remove_dir_all(home);
    }
}

#[test]
fn d51_6_detection_uses_backend_file_not_settings_alone() {
    for os in OPERATING_SYSTEMS {
        let home = scratch("d51-6");
        let session = Session::new(&home);
        let agent = adapter(os);
        let settings = settings_path(&home, os);
        fs::create_dir_all(settings.parent().expect("parent")).expect("mkdir");
        fs::write(&settings, "{}\n").expect("settings only");
        assert!(!agent.detect(&session), "{os:?} settings alone");

        let marker = backend_marker(&home, os);
        fs::create_dir_all(marker.parent().expect("parent")).expect("mkdir marker");
        fs::write(&marker, b"\0").expect("marker");
        assert!(agent.detect(&session), "{os:?} backend file");
        let _ = fs::remove_dir_all(home);
    }
}

#[test]
fn d51_7_fixture_paths_resolve_on_each_os() {
    for os in OPERATING_SYSTEMS {
        let home = scratch("d51-7");
        let session = Session::new(&home);
        let settings = settings_path(&home, os);
        assert!(
            settings.to_string_lossy().contains(".antigravity"),
            "{settings:?}"
        );
        let marker = backend_marker(&home, os);
        assert!(
            marker
                .to_string_lossy()
                .contains("agyhub_summaries_proto.pb"),
            "{marker:?}"
        );
        let admin = admin_path(&session, os);
        match os {
            TargetOs::Linux => assert!(admin.starts_with("/etc/antigravity/")),
            TargetOs::Macos => assert!(admin.starts_with("/Library/Application Support/")),
            TargetOs::Windows => {
                let windows = Session::new(&home).with_env(
                    OsString::from("PROGRAMDATA"),
                    OsString::from("/ProgramData"),
                );
                let admin = admin_path(&windows, os);
                assert!(admin.starts_with("/ProgramData/Antigravity/"), "{admin:?}");
            }
        }
        let _ = fs::remove_dir_all(home);
    }
}
