//! Effort-family dispatch, token counts, scopes, and old-id compatibility.

use std::collections::BTreeMap;
use std::sync::{Arc, Mutex};

use async_trait::async_trait;
use hofund_ir::{Effort, Message, Request, Response, Role};
use hofund_plugin_api::{CountOutcome, MemberId, MemberKey, PluginError, PresetId};
use hofund_router::{
    Completion, EffortModels, FailoverPolicy, Hop, Member, RouteScope, Router, RouterError,
};

struct Fake {
    id: MemberId,
    family: EffortModels,
    seen: Arc<Mutex<Vec<String>>>,
}

#[async_trait]
impl Member for Fake {
    fn id(&self) -> &MemberId {
        &self.id
    }

    fn route_id(&self) -> &str {
        "relay"
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
