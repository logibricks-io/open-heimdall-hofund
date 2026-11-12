//! Control resume argv and Pi-to-omp carry without shell lines.

mod support;

use std::path::{Path, PathBuf};
use std::sync::Arc;

use hofund_control_proto::{
    API_VERSION, SessionCarryResponse, SessionListResponse, decode_response,
};
use hofund_daemon::{
    CompiledPreset, ControlRuntime, DaemonRegistry, Running, StartOptions, start_linked,
};
use hofund_plugin_agent_omp::OmpStore;
use hofund_plugin_agent_pi::PiStore;
use hofund_plugin_api::{
    AgentDescription, DynUpstreamFactory, InboundSurface, ProtocolId, SessionStore,
};
use hofund_sessions::consent::Consent;

struct PiOmpRegistry {
    stores: Vec<Arc<dyn SessionStore>>,
}

impl DaemonRegistry for PiOmpRegistry {
    fn factory(&self, _id: &str) -> Option<Arc<dyn DynUpstreamFactory>> {
        None
    }

    fn is_kernel_factory(&self, _id: &str) -> bool {
        false
    }

    fn builtin_factory_ids(&self) -> &'static [&'static str] {
        &[]
    }

    fn builtin_protocols(&self) -> Vec<ProtocolId> {
        Vec::new()
    }

    fn find_preset(&self, _id: &str) -> Option<(&'static CompiledPreset, Option<&'static str>)> {
        None
    }

    fn agents(&self) -> &'static [&'static AgentDescription] {
        &[]
    }

    fn surfaces(&self) -> Vec<Arc<dyn InboundSurface>> {
        Vec::new()
    }

    fn session_stores(&self) -> Vec<Arc<dyn SessionStore>> {
        self.stores.clone()
    }
}

fn pi_fixture(name: &str) -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR"))
        .join("../../../plugins/agents/pi/tests/fixtures")
        .join(name)
}

fn install_pi_session(root: &Path) -> PathBuf {
    let sessions = root.join("sessions").join("work");
    std::fs::create_dir_all(&sessions).expect("pi sessions");
    let path = sessions.join("2026-01-01T00-00-00_pi-main.jsonl");
    std::fs::copy(pi_fixture("main.jsonl"), &path).expect("pi fixture");
    path
}

async fn start_pi_omp(dir: &Path) -> (Running, PathBuf) {
    let pi_home = dir.join("pi").join("agent");
    std::fs::create_dir_all(&pi_home).expect("pi home");
    let session_path = install_pi_session(&pi_home);
    let omp_root = dir.join("omp").join("sessions");
    std::fs::create_dir_all(&omp_root).expect("omp root");
    let pi = Arc::new(PiStore::with_root(&pi_home)) as Arc<dyn SessionStore>;
    let omp = Arc::new(OmpStore::with_session_roots([omp_root])) as Arc<dyn SessionStore>;
    let registry = Arc::new(PiOmpRegistry {
        stores: vec![pi, omp],
    });
    let config = dir.join("hofund.toml");
    std::fs::write(&config, "listen = \"127.0.0.1:0\"\n").expect("config");
    let mut consent = Consent::new(dir);
    consent.settle();
    consent
        .set_reads(Some(true), [])
        .expect("consent on for pi and omp");
    let running = start_linked(
        &config,
        registry,
        StartOptions {
            control_listen: Some("127.0.0.1:0".parse().expect("control address")),
            control_runtime: Some(ControlRuntime::new(None, dir.to_path_buf())),
            ..StartOptions::default()
        },
    )
    .await
    .expect("daemon");
    (running, session_path)
}

/// Compare filesystem paths by components so Windows `\\` and `/` agree.
fn assert_same_path(left: impl AsRef<Path>, right: impl AsRef<Path>) {
    let left = left.as_ref();
    let right = right.as_ref();
    assert!(
        left.components().eq(right.components()),
        "paths differ: {left:?} vs {right:?}"
    );
}

fn token(dir: &Path) -> String {
    std::fs::read_to_string(dir.join("hofund.token"))
        .expect("token")
        .trim()
        .to_owned()
}

async fn post_json(running: &Running, dir: &Path, route: &str, body: &str) -> reqwest::Response {
    support::client()
        .post(format!(
            "http://{}{route}",
            running.control_local_addr().expect("control")
        ))
        .bearer_auth(token(dir))
        .header("content-type", "application/json")
        .body(body.to_owned())
        .send()
        .await
        .expect("control post")
}

#[tokio::test]
async fn pi_list_includes_resume_argv_and_carry_returns_omp_fork() {
    let dir = tempfile::tempdir().expect("temp dir");
    let (running, session_path) = start_pi_omp(dir.path()).await;

    let response = support::client()
        .get(format!(
            "http://{}/v1/hofund/sessions",
            running.control_local_addr().expect("control")
        ))
        .bearer_auth(token(dir.path()))
        .send()
        .await
        .expect("list");
    assert_eq!(response.status(), reqwest::StatusCode::OK);
    let list: SessionListResponse =
        decode_response(API_VERSION, &response.bytes().await.expect("list body")).expect("list");
    let row = list
        .sessions
        .iter()
        .find(|session| session.id == "pi:pi-main")
        .expect("pi session");
    let resume = row.resume.as_ref().expect("pi resume");
    assert_eq!(resume.argv[0], "pi");
    assert_eq!(resume.argv[1], "--session");
    assert_same_path(&resume.argv[2], &session_path);
    assert_eq!(resume.cwd, "/work/pi");

    let response = post_json(
        &running,
        dir.path(),
        "/v1/hofund/sessions/carry",
        r#"{"version":1,"id":"pi:pi-main","to":"omp"}"#,
    )
    .await;
    assert_eq!(response.status(), reqwest::StatusCode::OK);
    let carry: SessionCarryResponse =
        decode_response(API_VERSION, &response.bytes().await.expect("carry body")).expect("carry");
    assert_eq!(carry.agent, "omp");
    assert_eq!(carry.id, "pi:pi-main");
    assert_eq!(carry.command.argv[0], "omp");
    assert_eq!(carry.command.argv[1], "--fork");
    assert_same_path(&carry.command.argv[2], &session_path);
    assert_eq!(carry.command.cwd, "/work/pi");

    support::shutdown(running).await;
}

#[tokio::test]
async fn carry_unknown_pairs_return_not_found() {
    let dir = tempfile::tempdir().expect("temp dir");
    let (running, _session_path) = start_pi_omp(dir.path()).await;
    let response = post_json(
        &running,
        dir.path(),
        "/api/sessions/carry",
        r#"{"version":1,"id":"pi:pi-main","to":"claude"}"#,
    )
    .await;
    assert_eq!(response.status(), reqwest::StatusCode::NOT_FOUND);
    let response = post_json(
        &running,
        dir.path(),
        "/v1/hofund/sessions/carry",
        r#"{"version":1,"id":"pi:pi-main","to":"codex"}"#,
    )
    .await;
    assert_eq!(response.status(), reqwest::StatusCode::NOT_FOUND);
    support::shutdown(running).await;
}
