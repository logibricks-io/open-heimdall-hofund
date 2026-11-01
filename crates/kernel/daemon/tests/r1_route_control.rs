//! Routing view control API coverage.

use std::path::Path;

use hofund_control_proto::{API_VERSION, RouteQuery, decode_response};
use hofund_daemon::{ControlRuntime, StartOptions, start_with_options};
use hofund_router::RouteMeta;

const CONFIG: &str = "listen = \"127.0.0.1:0\"\n[[providers]]\nid = \"chat\"\nprotocol = \"openai-chat\"\nbase_url = \"http://127.0.0.1:9\"\n[[routes]]\nmodel = \"demo\"\nupstreams = [\"chat\"]\n";

struct Scratch(tempfile::TempDir);

impl Scratch {
    fn new(_label: &str) -> Self {
        Self(tempfile::tempdir_in("/tmp").expect("scratch dir"))
    }

    fn write_config(&self, text: &str) -> std::path::PathBuf {
        let path = self.0.path().join("hofund.toml");
        std::fs::write(&path, text).expect("config");
        path
    }
}

async fn token_for(config: &Path) -> String {
    std::fs::read_to_string(config.with_extension("token"))
        .expect("token")
        .trim()
        .to_owned()
}

#[tokio::test]
async fn r1_6_control_route_list_and_session_endpoints() {
    let scratch = Scratch::new("route-control");
    let config = scratch.write_config(CONFIG);
    let running = start_with_options(
        &config,
        StartOptions {
            control_listen: Some("127.0.0.1:0".parse().expect("control address")),
            control_runtime: Some(ControlRuntime::new(None, scratch.0.path().to_path_buf())),
            ..StartOptions::default()
        },
    )
    .await
    .expect("start");
    let token = token_for(&config).await;
    let book = running.route_trace_book().expect("trace book");
    let mut handle = book.begin(RouteMeta {
        session: "sess-a".to_owned(),
        asked: "demo".to_owned(),
        ..RouteMeta::default()
    });
    handle.set_routing("smart");
    handle.finish(Some("chat"), Some("chat"), Some(200), None, None);

    let client = reqwest::Client::new();
    let base = format!(
        "http://{}{}",
        running.control_local_addr().expect("control addr"),
        "/v1/hofund/route"
    );
    let list = client
        .get(&base)
        .bearer_auth(&token)
        .send()
        .await
        .expect("list");
    assert!(list.status().is_success());
    let body = list.bytes().await.expect("body");
    let decoded: hofund_control_proto::RouteListResponse =
        decode_response(API_VERSION, &body).expect("decode list");
    assert_eq!(decoded.decisions.len(), 1);
    assert_eq!(decoded.decisions[0].session, "sess-a");

    let session = client
        .get(format!("{base}/session?session=sess-a"))
        .bearer_auth(&token)
        .send()
        .await
        .expect("session");
    assert!(session.status().is_success());
    let body = session.bytes().await.expect("body");
    let decoded: hofund_control_proto::SessionRouteResponse =
        decode_response(API_VERSION, &body).expect("decode session");
    assert_eq!(decoded.session, "sess-a");
    assert!(decoded.decision.is_some());

    book.hold_unrouted("sess-b", "demo", "plain", "waiting");
    let unrouted = client
        .get(format!("{base}/unrouted"))
        .bearer_auth(&token)
        .send()
        .await
        .expect("unrouted");
    assert!(unrouted.status().is_success());
    let body = unrouted.bytes().await.expect("body");
    let decoded: hofund_control_proto::UnroutedListResponse =
        decode_response(API_VERSION, &body).expect("decode unrouted");
    assert_eq!(decoded.holds.len(), 1);
    assert_eq!(decoded.holds[0].session, "sess-b");

    let _ = RouteQuery::parse(Some("limit=5&after=1"));
}
