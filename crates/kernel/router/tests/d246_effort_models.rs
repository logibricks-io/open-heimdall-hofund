//! Effort-family dispatch, token counts, scopes, and old-id compatibility.

use std::collections::BTreeMap;
use std::sync::{Arc, Mutex};
use std::time::Duration;

use async_trait::async_trait;
use hofund_ir::{Effort, Message, Request, Response, Role};
use hofund_plugin_api::{CountOutcome, MemberId, MemberKey, PluginError, PresetId};
use hofund_router::{
    Completion, CooldownPolicy, Cooldowns, EffortModels, FailoverPolicy, Hop, Member, RouteScope,
    Router, RouterError,
};

struct Fake {
    id: MemberId,
    provider: &'static str,
    family: EffortModels,
    seen: Arc<Mutex<Vec<String>>>,
}

#[async_trait]
impl Member for Fake {
    fn id(&self) -> &MemberId {
        &self.id
    }

    fn route_id(&self) -> &str {
        self.provider
    }

    fn protocol(&self) -> &str {
        "test"
    }

    fn effort_models(&self) -> Option<EffortModels> {
        Some(self.family.clone())
    }

    async fn execute(
        &self,
        request: Request,
        _inbound_protocol: &str,
    ) -> Result<Completion, PluginError> {
        self.seen.lock().expect("seen").push(request.model.clone());
        Ok(Completion::Unary(Box::new(
            Response::new(Message::new(Role::Assistant)).with_model(Some(request.model)),
        )))
    }

    async fn count_input(
        &self,
        request: Request,
        _inbound_protocol: &str,
    ) -> Result<CountOutcome, PluginError> {
        self.seen.lock().expect("seen").push(request.model);
        Ok(CountOutcome::Counted(7))
    }
}

fn fixture() -> (Router, Arc<Mutex<Vec<String>>>) {
    let family = EffortModels::new(
        "model-4-alpha",
        "model-4-alpha-medium",
        vec![
            ("low".to_owned(), "model-4-alpha-low".to_owned()),
            ("medium".to_owned(), "model-4-alpha-medium".to_owned()),
            ("high".to_owned(), "model-4-alpha-high".to_owned()),
        ],
    );
    let seen = Arc::new(Mutex::new(Vec::new()));
    let member = Fake {
        id: MemberId::from_preset(
            PresetId::from_static("relay"),
            MemberKey::from_static("row"),
        ),
        provider: "relay",
        family,
        seen: Arc::clone(&seen),
    };
    let mut hop = Hop::new(Arc::new(member));
    hop.wire_model = Some("model-4-alpha".to_owned());
    let router = Router::from_hops(
        BTreeMap::from([("model-4-alpha".to_owned(), vec![hop])]),
        FailoverPolicy::default(),
    );
    (router, seen)
}

fn ask(model: &str, effort: Option<Effort>) -> Request {
    let mut request = Request::new(model);
    request.params.effort = effort;
    request
}

#[tokio::test]
async fn d246_execute_and_count_select_the_same_effort_id() {
    let (router, seen) = fixture();
    for (model, effort, expected) in [
        ("model-4-alpha", None, "model-4-alpha-medium"),
        ("model-4-alpha", Some(Effort::Low), "model-4-alpha-low"),
        ("model-4-alpha", Some(Effort::Xhigh), "model-4-alpha-high"),
        (
            "model-4-alpha-high",
            Some(Effort::Low),
            "model-4-alpha-high",
        ),
        (
            "relay/model-4-alpha-low",
            Some(Effort::High),
            "model-4-alpha-low",
        ),
    ] {
        let routed = router
            .dispatch(&ask(model, effort), "test")
            .await
            .expect("dispatch");
        assert_eq!(routed.upstream_model, expected);
        let counted = router
            .count(&ask(model, effort), "test")
            .await
            .expect("count");
        assert_eq!(counted.tokens, 7);
        assert_eq!(seen.lock().expect("seen").last().unwrap(), expected);
    }
    assert_eq!(
        seen.lock().expect("seen").as_slice(),
        [
            "model-4-alpha-medium",
            "model-4-alpha-medium",
            "model-4-alpha-low",
            "model-4-alpha-low",
            "model-4-alpha-high",
            "model-4-alpha-high",
            "model-4-alpha-high",
            "model-4-alpha-high",
            "model-4-alpha-low",
            "model-4-alpha-low",
        ]
    );
}

#[tokio::test]
async fn d246_old_ids_pass_model_scopes_and_levels_are_listed() {
    let (router, seen) = fixture();
    let scope = RouteScope {
        models: vec!["relay/model-4-alpha-high".to_owned()],
        accounts: Vec::new(),
        ..RouteScope::default()
    };
    for model in ["model-4-alpha-high", "relay/model-4-alpha-high"] {
        router
            .dispatch_scoped(&ask(model, Some(Effort::Low)), "test", Some(&scope), None)
            .await
            .expect("old id is allowed");
    }
    assert_eq!(
        seen.lock().expect("seen").as_slice(),
        ["model-4-alpha-high", "model-4-alpha-high"]
    );
    let low_only = RouteScope {
        models: vec!["relay/model-4-alpha-low".to_owned()],
        accounts: Vec::new(),
        ..RouteScope::default()
    };
    for model in [
        "model-4-alpha",
        "model-4-alpha-high",
        "relay/model-4-alpha",
        "relay/model-4-alpha-high",
    ] {
        match router
            .dispatch_scoped(
                &ask(model, Some(Effort::Low)),
                "test",
                Some(&low_only),
                None,
            )
            .await
        {
            Err(error) => assert!(
                matches!(error, RouterError::ModelScope { .. }),
                "{model}: {error}"
            ),
            Ok(_) => panic!("a low scope admitted {model}"),
        }
    }
    assert_eq!(
        seen.lock().expect("seen").as_slice(),
        ["model-4-alpha-high", "model-4-alpha-high"]
    );
    let allowed = router
        .dispatch_scoped(
            &ask("model-4-alpha-low", Some(Effort::High)),
            "test",
            Some(&low_only),
            None,
        )
        .await
        .expect("the named old id stays allowed");
    assert_eq!(allowed.upstream_model, "model-4-alpha-low");
    assert_eq!(
        router.effort_levels("model-4-alpha"),
        ["low", "medium", "high"]
    );
    assert_eq!(
        router.effort_levels("relay/model-4-alpha"),
        ["low", "medium", "high"]
    );
    assert!(router.effort_levels("other/model-4-alpha").is_empty());
}

#[tokio::test]
async fn d246_qualified_levels_stay_on_the_named_upstream() {
    let seen = Arc::new(Mutex::new(Vec::new()));
    let relay = Fake {
        id: MemberId::from_preset(
            PresetId::from_static("relay"),
            MemberKey::from_static("row"),
        ),
        provider: "relay",
        family: EffortModels::new(
            "model-4-alpha",
            "model-4-alpha-medium",
            vec![
                ("low".to_owned(), "model-4-alpha-low".to_owned()),
                ("medium".to_owned(), "model-4-alpha-medium".to_owned()),
                ("high".to_owned(), "model-4-alpha-high".to_owned()),
            ],
        ),
        seen: Arc::clone(&seen),
    };
    let other = Fake {
        id: MemberId::from_preset(
            PresetId::from_static("other"),
            MemberKey::from_static("row"),
        ),
        provider: "other",
        family: EffortModels::new(
            "model-4-alpha",
            "model-4-alpha-xhigh",
            vec![
                ("minimal".to_owned(), "model-4-alpha-minimal".to_owned()),
                ("xhigh".to_owned(), "model-4-alpha-xhigh".to_owned()),
            ],
        ),
        seen,
    };
    let mut relay_hop = Hop::new(Arc::new(relay));
    relay_hop.wire_model = Some("model-4-alpha".to_owned());
    let mut other_hop = Hop::new(Arc::new(other));
    other_hop.wire_model = Some("model-4-alpha".to_owned());
    let router = Router::from_hops(
        BTreeMap::from([("model-4-alpha".to_owned(), vec![relay_hop, other_hop])]),
        FailoverPolicy::default(),
    );
    assert_eq!(
        router.effort_levels("relay/model-4-alpha"),
        ["low", "medium", "high"]
    );
    assert_eq!(
        router.effort_levels("other/model-4-alpha"),
        ["minimal", "xhigh"]
    );
}

struct Scripted {
    id: MemberId,
    provider: &'static str,
    family: EffortModels,
    seen: Arc<Mutex<Vec<String>>>,
    fail: Option<(u16, &'static str)>,
}

#[async_trait]
impl Member for Scripted {
    fn id(&self) -> &MemberId {
        &self.id
    }

    fn route_id(&self) -> &str {
        self.provider
    }

    fn protocol(&self) -> &str {
        "test"
    }

    fn effort_models(&self) -> Option<EffortModels> {
        Some(self.family.clone())
    }

    async fn execute(
        &self,
        request: Request,
        _inbound_protocol: &str,
    ) -> Result<Completion, PluginError> {
        self.seen
            .lock()
            .expect("seen")
            .push(format!("execute:{}", request.model));
        if let Some((status, message)) = self.fail {
            return Err(PluginError::status_error(status, message));
        }
        Ok(Completion::Unary(Box::new(
            Response::new(Message::new(Role::Assistant)).with_model(Some(request.model)),
        )))
    }

    async fn count_input(
        &self,
        request: Request,
        _inbound_protocol: &str,
    ) -> Result<CountOutcome, PluginError> {
        self.seen
            .lock()
            .expect("seen")
            .push(format!("count:{}", request.model));
        if let Some((status, message)) = self.fail {
            return Err(PluginError::status_error(status, message));
        }
        Ok(CountOutcome::Counted(7))
    }
}

fn family() -> EffortModels {
    EffortModels::new(
        "model-4-alpha",
        "model-4-alpha-medium",
        vec![
            ("low".to_owned(), "model-4-alpha-low".to_owned()),
            ("medium".to_owned(), "model-4-alpha-medium".to_owned()),
            ("high".to_owned(), "model-4-alpha-high".to_owned()),
        ],
    )
}

fn scripted(
    provider: &'static str,
    key: &'static str,
    seen: &Arc<Mutex<Vec<String>>>,
    fail: Option<(u16, &'static str)>,
) -> Hop {
    let mut hop = Hop::new(Arc::new(Scripted {
        id: MemberId::from_preset(PresetId::from_static(provider), MemberKey::from_static(key)),
        provider,
        family: family(),
        seen: Arc::clone(seen),
        fail,
    }));
    hop.wire_model = Some("model-4-alpha".to_owned());
    hop
}

/// The rest is already stored under the id `apply_wire` sends. Looking the
/// family wire id up does not see it, so a later high execute and count still
/// ask that member. A low request uses another id and is not held.
#[tokio::test]
async fn d246_selected_member_hold_blocks_that_effort_only() {
    let seen = Arc::new(Mutex::new(Vec::new()));
    let rested = MemberId::from_preset(
        PresetId::from_static("relay-a"),
        MemberKey::from_static("row"),
    );
    let book = Arc::new(Cooldowns::new(CooldownPolicy::default()));
    book.record_failure_for_model(
        &rested,
        "model-4-alpha-high",
        &PluginError::quota_status(429, "quota exceeded")
            .with_model_scope()
            .with_retry_after(Duration::from_secs(60)),
    )
    .expect("model hold");
    assert!(
        book.cooling_for_model(&rested, "model-4-alpha-high")
            .is_some()
    );
    assert!(book.cooling_for_model(&rested, "model-4-alpha").is_none());
    assert!(
        book.cooling_for_model(&rested, "model-4-alpha-low")
            .is_none()
    );
    let router = Router::from_hops(
        BTreeMap::from([(
            "model-4-alpha".to_owned(),
            vec![
                scripted("relay-a", "row", &seen, None),
                scripted("relay-b", "row", &seen, None),
            ],
        )]),
        FailoverPolicy::default(),
    )
    .with_cooldowns(book);
    let high = router
        .dispatch(&ask("model-4-alpha", Some(Effort::High)), "test")
        .await
        .expect("high");
    assert_eq!(high.upstream_id, "relay-b");
    let counted = router
        .count(&ask("model-4-alpha", Some(Effort::High)), "test")
        .await
        .expect("count");
    assert!(!counted.estimated);
    assert_eq!(counted.upstream_id, "relay-b");
    let low = router
        .dispatch(&ask("model-4-alpha", Some(Effort::Low)), "test")
        .await
        .expect("low");
    assert_eq!(low.upstream_id, "relay-a");
    assert_eq!(low.upstream_model, "model-4-alpha-low");
    assert_eq!(
        seen.lock().expect("seen").as_slice(),
        [
            "execute:model-4-alpha-high",
            "count:model-4-alpha-high",
            "execute:model-4-alpha-low",
        ]
    );
}

/// A regional refusal is closed on the id that was sent. The other account of
/// the same provider that would send that same id is the mate, so the first
/// account is not asked again.
#[tokio::test]
async fn d246_region_close_uses_the_selected_member() {
    let seen = Arc::new(Mutex::new(Vec::new()));
    let region = Some((403, "user location is not supported for the api use"));
    let router = Router::from_hops(
        BTreeMap::from([(
            "model-4-alpha".to_owned(),
            vec![
                scripted("relay", "one", &seen, region),
                scripted("relay", "two", &seen, region),
            ],
        )]),
        FailoverPolicy::default(),
    );
    match router
        .dispatch(&ask("model-4-alpha", Some(Effort::High)), "test")
        .await
    {
        Err(RouterError::Upstream { .. }) => {}
        Err(error) => panic!("region refusal was not an upstream error: {error}"),
        Ok(_) => panic!("region refusal was answered"),
    }
    assert_eq!(
        seen.lock().expect("seen").as_slice(),
        ["execute:model-4-alpha-high", "execute:model-4-alpha-high",]
    );
}
