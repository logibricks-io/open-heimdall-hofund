//! Failover happens only before a stream event is produced.

use std::collections::BTreeMap;
use std::sync::Arc;
use std::sync::atomic::{AtomicUsize, Ordering};

use async_trait::async_trait;
use bytes::Bytes;
use futures_util::StreamExt;
use futures_util::stream;
use hofund_ir::{ContentPart, Message, Request, Response, Role, StreamEvent, TextDelta};
use hofund_plugin_api::{BodyStream, MemberId, MemberKey, PluginError, PluginErrorKind, PresetId};
use hofund_router::{Completion, FailoverPolicy, Member, Router, RouterError, Snapshot};

struct Fake {
    route_id: &'static str,
    member_id: MemberId,
    hits: Arc<AtomicUsize>,
    kind: Kind,
}

enum Kind {
    Status(u16, &'static str),
    Connect(&'static str),
    Auth,
    Unary,
    StreamOk,
    StreamFail(u16),
    RawOk,
    RawFail,
}

#[async_trait]
impl Member for Fake {
    fn id(&self) -> &MemberId {
        &self.member_id
    }

    fn route_id(&self) -> &str {
        self.route_id
    }

    fn protocol(&self) -> &str {
        "test"
    }

    async fn execute(&self, _request: Request, _inbound: &str) -> Result<Completion, PluginError> {
        self.hits.fetch_add(1, Ordering::SeqCst);
        match self.kind {
            Kind::Status(status, message) => Err(PluginError::status_error(status, message)),
            Kind::Connect(message) => Err(PluginError::connect(message)),
            Kind::Auth => Err(PluginError::auth("denied")),
            Kind::Unary => Ok(Completion::Unary(Box::new(reply()))),
            Kind::StreamOk => Ok(Completion::Stream(Box::pin(stream::iter(vec![
                Ok(text_event("hi")),
                Ok(text_event("!")),
            ])))),
            Kind::StreamFail(status) => Ok(Completion::Stream(Box::pin(stream::iter(vec![Err(
                PluginError::status_error(status, "down"),
            )])))),
            Kind::RawOk => Ok(Completion::Raw(BodyStream::from_bytes(Bytes::from_static(
                b"{\"ok\":true}",
            )))),
            Kind::RawFail => Ok(Completion::Raw(BodyStream::from_stream(stream::iter(
                vec![Err(PluginError::status_error(503, "raw"))],
            )))),
        }
    }
}

fn reply() -> Response {
    let mut message = Message::new(Role::Assistant);
    message.content.push(ContentPart::text("ok"));
    Response::new(message)
}

fn text_event(text: &str) -> StreamEvent {
    StreamEvent::TextDelta(TextDelta {
        index: 0,
        text: text.to_owned(),
        extensions: hofund_ir::Extensions::new(),
    })
}

fn router(members: Vec<Arc<dyn Member>>) -> Router {
    let mut routes = BTreeMap::new();
    routes.insert("demo".to_owned(), members);
    Router::new(routes, FailoverPolicy::default())
}

fn fake(id: &'static str, hits: &Arc<AtomicUsize>, kind: Kind) -> Arc<dyn Member> {
    Arc::new(Fake {
        route_id: id,
        member_id: MemberId::from_preset(
            PresetId::try_from(id).expect("preset"),
            MemberKey::from_static("row"),
        ),
        hits: Arc::clone(hits),
        kind,
    })
}

async fn dispatch(router: &Router, model: &str) -> Result<hofund_router::Routed, RouterError> {
    let request = Request::new(model);
    router.dispatch(&request, "inbound").await
}

#[tokio::test]
async fn ordered_failover_skips_status_and_connect_failures() {
    let first_hits = Arc::new(AtomicUsize::new(0));
    let second_hits = Arc::new(AtomicUsize::new(0));
    let third_hits = Arc::new(AtomicUsize::new(0));
    let router = router(vec![
        fake("first", &first_hits, Kind::Status(503, "busy")),
        fake("second", &second_hits, Kind::Connect("refused")),
        fake("third", &third_hits, Kind::Unary),
    ]);
    let routed = dispatch(&router, "demo").await.expect("dispatch");
    assert_eq!(routed.upstream_id, "third");
    assert!(matches!(routed.completion, Completion::Unary(_)));
    assert_eq!(first_hits.load(Ordering::SeqCst), 1);
    assert_eq!(third_hits.load(Ordering::SeqCst), 1);
}

#[tokio::test]
async fn client_error_does_not_try_the_next_upstream() {
    let second = Arc::new(AtomicUsize::new(0));
    let router = router(vec![
        fake(
            "first",
            &Arc::new(AtomicUsize::new(0)),
            Kind::Status(400, "bad"),
        ),
        fake("second", &second, Kind::Unary),
    ]);
    let error = match dispatch(&router, "demo").await {
        Ok(_) => panic!("expected a status error"),
        Err(error) => error,
    };
    let RouterError::Upstream { upstream_id, error } = error else {
        panic!("unexpected error");
    };
    assert_eq!(upstream_id, "first");
    assert_eq!(error.kind(), PluginErrorKind::Status);
    assert_eq!(error.status(), Some(400));
    assert_eq!(second.load(Ordering::SeqCst), 0);
}

#[tokio::test]
async fn stream_error_before_an_event_fails_over() {
    let router = router(vec![
        fake(
            "first",
            &Arc::new(AtomicUsize::new(0)),
            Kind::StreamFail(502),
        ),
        fake("second", &Arc::new(AtomicUsize::new(0)), Kind::StreamOk),
    ]);
    let routed = dispatch(&router, "demo").await.expect("dispatch");
    assert_eq!(routed.upstream_id, "second");
    let Completion::Stream(mut stream) = routed.completion else {
        panic!("expected a stream");
    };
    let first = stream.next().await.expect("event").expect("ok");
    assert!(matches!(first, StreamEvent::TextDelta(_)));
}

#[tokio::test]
async fn stream_event_commits_the_upstream() {
    let second = Arc::new(AtomicUsize::new(0));
    let router = router(vec![
        fake("first", &Arc::new(AtomicUsize::new(0)), Kind::StreamOk),
        fake("second", &second, Kind::Unary),
    ]);
    let routed = dispatch(&router, "demo").await.expect("dispatch");
    assert_eq!(routed.upstream_id, "first");
    assert_eq!(second.load(Ordering::SeqCst), 0);
}

#[tokio::test]
async fn unknown_model_has_no_route() {
    let router = router(vec![]);
    let error = match dispatch(&router, "other").await {
        Ok(_) => panic!("expected a missing route"),
        Err(error) => error,
    };
    assert!(matches!(error, RouterError::NoRoute { .. }));
    assert!(router.models().is_empty());
}

#[tokio::test]
async fn auth_does_not_try_the_next_member() {
    let second = Arc::new(AtomicUsize::new(0));
    let router = router(vec![
        fake("first", &Arc::new(AtomicUsize::new(0)), Kind::Auth),
        fake("second", &second, Kind::Unary),
    ]);
    let error = match dispatch(&router, "demo").await {
        Ok(_) => panic!("auth should fail the call"),
        Err(error) => error,
    };
    let RouterError::Upstream { error, .. } = error else {
        panic!("upstream");
    };
    assert_eq!(error.kind(), PluginErrorKind::Auth);
    assert_eq!(second.load(Ordering::SeqCst), 0);
}

#[tokio::test]
async fn a_raw_error_before_a_chunk_fails_over_and_a_chunk_commits() {
    let second = Arc::new(AtomicUsize::new(0));
    let failed_over = router(vec![
        fake("first", &Arc::new(AtomicUsize::new(0)), Kind::RawFail),
        fake("second", &second, Kind::RawOk),
    ]);
    let routed = dispatch(&failed_over, "demo").await.expect("failover");
    assert_eq!(routed.upstream_id, "second");
    let Completion::Raw(mut body) = routed.completion else {
        panic!("raw");
    };
    let chunk = body.next_chunk().await.expect("chunk").expect("ok");
    assert_eq!(&chunk[..], b"{\"ok\":true}");
    assert_eq!(second.load(Ordering::SeqCst), 1);

    let later = Arc::new(AtomicUsize::new(0));
    let committed = router(vec![
        fake("first", &Arc::new(AtomicUsize::new(0)), Kind::RawOk),
        fake("second", &later, Kind::Unary),
    ]);
    let routed = dispatch(&committed, "demo").await.expect("commit");
    assert_eq!(routed.upstream_id, "first");
    assert_eq!(later.load(Ordering::SeqCst), 0);
}

#[tokio::test]
async fn a_loaded_snapshot_is_unchanged_by_a_later_store() {
    let hits = Arc::new(AtomicUsize::new(0));
    let snapshot = Snapshot::new(router(vec![fake("first", &hits, Kind::Unary)]));
    let held = snapshot.load();
    snapshot.store(router(vec![]));
    let routed = dispatch(&held, "demo").await.expect("held");
    assert_eq!(routed.upstream_id, "first");
    assert!(snapshot.load().models().is_empty());
    assert_eq!(hits.load(Ordering::SeqCst), 1);
}
