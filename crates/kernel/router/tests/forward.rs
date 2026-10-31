//! Forwarded calls: wire names, failover over members that do not serve the
//! path, and the errors that stop at once.

use std::collections::BTreeMap;
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use async_trait::async_trait;
use hofund_ir::Request;
use hofund_plugin_api::{
    Forward, MemberId, MemberKey, PluginError, PluginErrorKind, PluginId, PresetId,
    QuotaObservation, RawAnswer, RawBody,
};
use hofund_router::{
    Completion, CooldownPolicy, Cooldowns, FailoverPolicy, Hop, Member, Router, RouterError,
};

type Seen = Arc<Mutex<Vec<(&'static str, String, String)>>>;

struct Fake {
    id: &'static str,
    member_id: MemberId,
    seen: Seen,
    answer: fn() -> Result<RawAnswer, PluginError>,
}

#[async_trait]
impl Member for Fake {
    fn id(&self) -> &MemberId {
        &self.member_id
    }

    fn route_id(&self) -> &str {
        self.id
    }

    fn protocol(&self) -> &str {
        "test"
    }

    async fn execute(&self, _request: Request, _inbound: &str) -> Result<Completion, PluginError> {
        Err(PluginError::internal("not a chat member"))
    }

    async fn forward(&self, path: &str, body: RawBody) -> Result<RawAnswer, PluginError> {
        let text = String::from_utf8(body.bytes().to_vec()).expect("utf8");
        self.seen
            .lock()
            .expect("seen")
            .push((self.id, path.to_owned(), text));
        (self.answer)()
    }
}

fn member_id(id: &'static str) -> MemberId {
    MemberId::from_preset(PresetId::from_static(id), MemberKey::from_static("row"))
}

fn hop(
    id: &'static str,
    wire: Option<&str>,
    seen: &Seen,
    answer: fn() -> Result<RawAnswer, PluginError>,
) -> Hop {
    Hop {
        member: Arc::new(Fake {
            id,
            member_id: member_id(id),
            seen: Arc::clone(seen),
            answer,
        }),
        wire_model: wire.map(str::to_owned),
        max_output_tokens: None,
        context_window: None,
        weight: 1,
        free: false,
    }
}

fn ok() -> Result<RawAnswer, PluginError> {
    Ok(RawAnswer::new(
        200,
        Some("application/json".to_owned()),
        b"{}".to_vec(),
    ))
}

fn router(hops: Vec<Hop>) -> Router {
    let mut routes = BTreeMap::new();
    routes.insert("embed".to_owned(), hops);
    Router::from_hops(routes, FailoverPolicy::default())
}

fn call(model: &str) -> Forward {
    Forward::new(model, "/embeddings", |model| {
        Ok(RawBody::new("application/json", format!("m={model}")))
    })
    .expect("call")
}

fn seen() -> Seen {
    Arc::new(Mutex::new(Vec::new()))
}

fn ids(seen: &Seen) -> Vec<&'static str> {
    seen.lock().expect("seen").iter().map(|row| row.0).collect()
}

#[tokio::test]
async fn each_member_gets_the_body_for_its_own_model_name() {
    let seen = seen();
    let router = router(vec![
        hop("first", Some("vendor-a"), &seen, || {
            Err(PluginError::unsupported("no embeddings"))
        }),
        hop("second", None, &seen, ok),
    ]);
    let forwarded = router.forward(&call("embed")).await.expect("forward");
    assert_eq!(forwarded.upstream_id, "second");
    assert_eq!(forwarded.upstream_model, "embed");
    assert_eq!(forwarded.answer.status(), 200);
    assert_eq!(
        seen.lock().expect("seen").as_slice(),
        [
            ("first", "/embeddings".to_owned(), "m=vendor-a".to_owned()),
            ("second", "/embeddings".to_owned(), "m=embed".to_owned()),
        ]
    );
}

#[tokio::test]
async fn members_that_do_not_serve_the_path_or_fail_over_pass_it_on() {
    let seen = seen();
    let router = router(vec![
        hop("a", None, &seen, || {
            Err(PluginError::status_error(404, "nf"))
        }),
        hop("b", None, &seen, || {
            Err(PluginError::status_error(405, "na"))
        }),
        hop("c", None, &seen, || {
            Err(PluginError::status_error(501, "ni"))
        }),
        hop("d", None, &seen, || {
            Err(PluginError::quota_status(429, "slow"))
        }),
        hop("e", None, &seen, || {
            Err(PluginError::status_error(503, "down"))
        }),
        hop("f", None, &seen, || Err(PluginError::connect("refused"))),
        hop("g", None, &seen, ok),
    ]);
    let forwarded = router.forward(&call("embed")).await.expect("forward");
    assert_eq!(forwarded.upstream_id, "g");
    assert_eq!(ids(&seen), ["a", "b", "c", "d", "e", "f", "g"]);
}

#[tokio::test]
async fn a_non_2xx_answer_is_not_a_failover() {
    let seen = seen();
    let router = router(vec![
        hop("first", None, &seen, || {
            Ok(RawAnswer::new(299, None, b"odd".to_vec()))
        }),
        hop("second", None, &seen, ok),
    ]);
    let forwarded = router.forward(&call("embed")).await.expect("forward");
    assert_eq!(forwarded.upstream_id, "first");
    assert_eq!(forwarded.answer.status(), 299);
    assert_eq!(ids(&seen), ["first"]);
}

#[tokio::test]
async fn a_refusal_stops_at_the_member_that_refused() {
    for (error, status) in [
        (
            (|| Err(PluginError::status_error(400, "bad input"))) as fn() -> _,
            Some(400),
        ),
        (|| Err(PluginError::auth("no key")), None),
    ] {
        let seen = seen();
        let router = router(vec![
            hop("first", None, &seen, error),
            hop("second", None, &seen, ok),
        ]);
        let Err(RouterError::Upstream { upstream_id, error }) =
            router.forward(&call("embed")).await
        else {
            panic!("expected an upstream error");
        };
        assert_eq!(upstream_id, "first");
        assert_eq!(error.status(), status);
        assert_eq!(ids(&seen), ["first"]);
    }
}

#[tokio::test]
async fn when_no_member_answers_the_last_error_is_returned() {
    let seen = seen();
    let router = router(vec![
        hop("first", None, &seen, || Err(PluginError::unsupported("no"))),
        hop("second", None, &seen, || {
            Err(PluginError::status_error(503, "down"))
        }),
    ]);
    let Err(RouterError::Upstream { upstream_id, error }) = router.forward(&call("embed")).await
    else {
        panic!("expected an upstream error");
    };
    assert_eq!(upstream_id, "second");
    assert_eq!(error.status(), Some(503));
    assert_eq!(error.message(), "down");
}

#[tokio::test]
async fn a_body_that_cannot_be_built_stops_before_the_member() {
    let seen = seen();
    let router = router(vec![hop("first", Some("wire"), &seen, ok)]);
    let call = Forward::new("embed", "/embeddings", |model| {
        Err(PluginError::status_error(
            400,
            format!("cannot name {model}"),
        ))
    })
    .expect("call");
    let Err(RouterError::Upstream { upstream_id, error }) = router.forward(&call).await else {
        panic!("expected an upstream error");
    };
    assert_eq!(upstream_id, "first");
    assert_eq!(error.message(), "cannot name wire");
    assert!(ids(&seen).is_empty());
}

#[tokio::test]
async fn an_unknown_model_has_no_route() {
    let router = router(vec![hop("first", None, &seen(), ok)]);
    assert!(matches!(
        router.forward(&call("other")).await,
        Err(RouterError::NoRoute { .. })
    ));
    let empty = Router::from_hops(BTreeMap::new(), FailoverPolicy::default());
    assert!(matches!(
        empty.forward(&call("embed")).await,
        Err(RouterError::NoRoute { .. })
    ));
}

#[tokio::test]
async fn a_member_without_forward_is_passed_over_as_unsupported() {
    struct Plain(MemberId);
    #[async_trait]
    impl Member for Plain {
        fn id(&self) -> &MemberId {
            &self.0
        }
        fn route_id(&self) -> &str {
            "plain"
        }
        fn protocol(&self) -> &str {
            "test"
        }
        async fn execute(&self, _r: Request, _i: &str) -> Result<Completion, PluginError> {
            Err(PluginError::internal("unused"))
        }
    }
    let seen = seen();
    let mut routes = BTreeMap::new();
    routes.insert(
        "embed".to_owned(),
        vec![Hop {
            member: Arc::new(Plain(member_id("plain"))),
            wire_model: None,
            max_output_tokens: None,
            context_window: None,
            weight: 1,
            free: false,
        }],
    );
    let only_plain = Router::from_hops(routes, FailoverPolicy::default());
    let Err(RouterError::Upstream { upstream_id, error }) =
        only_plain.forward(&call("embed")).await
    else {
        panic!("expected an upstream error");
    };
    assert_eq!(upstream_id, "plain");
    assert_eq!(error.kind(), PluginErrorKind::Unsupported);
    assert_eq!(error.message(), "this member does not serve /embeddings");
    drop(seen);
}

#[tokio::test]
async fn cooling_members_are_tried_after_ready_ones() {
    let now = Arc::new(Mutex::new(Instant::now()));
    let clock = Arc::clone(&now);
    let book = Arc::new(Cooldowns::with_clock(
        CooldownPolicy::default(),
        Arc::new(move || *clock.lock().expect("clock")),
    ));
    let observed = QuotaObservation::from_stored(PluginId::from_static("router"))
        .with_remaining(0)
        .with_retry_after(Duration::from_secs(300));
    let _ = book.observe(&member_id("first"), &observed);
    let seen = seen();
    let router = router(vec![
        hop("first", None, &seen, ok),
        hop("second", None, &seen, ok),
    ])
    .with_cooldowns(book);
    let forwarded = router.forward(&call("embed")).await.expect("forward");
    assert_eq!(forwarded.upstream_id, "second");
    assert_eq!(ids(&seen), ["second"]);
}
