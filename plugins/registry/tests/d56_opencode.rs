//! D-56: OpenCode 2 providers table wiring.

use std::fs;

use hofund_agent::{AgentAdapter, Compiled, GatewayTarget, Session};
use hofund_plugin_registry::agent;

const BASE: &str = "http://127.0.0.1:3425";
const TOKEN: &str = "hofund-agent-token";
const ROUTE: &str = "demo";

fn target() -> GatewayTarget {
    GatewayTarget::new(BASE, TOKEN, vec![ROUTE.to_owned()])
}

fn adapter() -> Compiled {
    Compiled::new(agent("opencode").expect("opencode"))
}

#[test]
fn d56_1_legacy_only_config_still_writes_provider_table() {
    let home = tempfile::tempdir().expect("temp").keep();
    let session = Session::new(&home);
    let path = home.join(".config/opencode/opencode.json");
    fs::create_dir_all(path.parent().expect("parent")).expect("mkdir");
    fs::write(&path, r#"{"model":"demo"}"#).expect("write");
    adapter().apply(&session, &target()).expect("connect");
    let text = fs::read_to_string(&path).expect("read");
    assert!(text.contains("\"provider\""));
    assert!(text.contains("\"hofund\""));
    assert!(!text.contains("\"providers\""));
    let _ = fs::remove_dir_all(home);
}

#[test]
fn d56_2_providers_table_config_writes_and_status_reads_providers_hofund() {
    let home = tempfile::tempdir().expect("temp").keep();
    let session = Session::new(&home);
    let path = home.join(".config/opencode/opencode.json");
    fs::create_dir_all(path.parent().expect("parent")).expect("mkdir");
    fs::write(
        &path,
        r#"{"providers":{"mine":{"package":"other"}},"model":"demo"}"#,
    )
    .expect("write");
    let compiled = adapter();
    compiled.apply(&session, &target()).expect("connect");
    let text = fs::read_to_string(&path).expect("read");
    assert!(text.contains("\"providers\""));
    assert!(text.contains("\"hofund\""));
    assert!(text.contains("\"mine\""));
    assert!(!text.contains("\"provider\""));
    assert!(
        compiled
            .status(&session, &target())
            .expect("status")
            .connected()
    );
    let _ = fs::remove_dir_all(home);
}

#[test]
fn d56_3_user_providers_stay_when_hofund_syncs() {
    let home = tempfile::tempdir().expect("temp").keep();
    let session = Session::new(&home);
    let path = home.join(".config/opencode/opencode.json");
    fs::create_dir_all(path.parent().expect("parent")).expect("mkdir");
    fs::write(
        &path,
        r#"{"providers":{"mine":{"package":"other","settings":{"apiKey":"user"}}},"model":"demo"}"#,
    )
    .expect("write");
    adapter().apply(&session, &target()).expect("connect");
    let text = fs::read_to_string(&path).expect("read");
    assert!(text.contains("\"mine\""));
    assert!(text.contains("user"));
    let _ = fs::remove_dir_all(home);
}
