//! D-55: gateway key scope for user routing groups.

use std::collections::BTreeMap;
use std::sync::Arc;

use async_trait::async_trait;
use hofund_plugin_api::{MemberId, MemberKey, PluginError, PresetId};
use hofund_router::{
    Completion, FailoverPolicy, Hop, Member, RouteScope, Router, affinity, model_allowed,
};

struct Fake {
    id: MemberId,
    route_id: &'static str,
}

#[async_trait]
impl Member for Fake {
    fn id(&self) -> &MemberId {
        &self.id
    }

    fn route_id(&self) -> &str {
        self.route_id
    }

    fn protocol(&self) -> &str {
        "test"
    }

    async fn execute(
        &self,
        _request: hofund_router::Request,
        _inbound: &str,
    ) -> Result<Completion, PluginError> {
        unreachable!()
    }
}

fn hop(provider: &'static str, wire: &str) -> Hop {
    Hop {
        member: Arc::new(Fake {
            id: MemberId::from_preset(
                PresetId::try_from(provider).expect("preset"),
                MemberKey::from_static("row"),
            ),
            route_id: provider,
        }),
        wire_model: Some(wire.to_owned()),
        max_output_tokens: None,
        context_window: None,
        patience: None,
        weight: 1,
        free: false,
        nearly_spent: false,
        fallback: false,
        effort: String::new(),
        cold_after: affinity::CACHE_COLD,
        units: Vec::new(),
    }
}

#[test]
fn d55_1_key_with_model_does_not_inherit_user_group() {
    let scope = RouteScope {
        models: vec!["primary/demo".to_owned()],
        accounts: Vec::new(),
    };
    let member = hop("primary", "demo");
    assert!(!scope.allows_model("group/only-38", "group/only-38", &member));
    assert!(scope.allows_model("demo", "demo", &member));
}

#[test]
fn d55_2_key_naming_group_may_use_it() {
    let scope = RouteScope {
        models: vec!["group/only-38".to_owned()],
        accounts: Vec::new(),
    };
    let member = hop("primary", "demo");
    assert!(scope.allows_model("group/only-38", "group/only-38", &member));
}

#[test]
fn d55_3_found_pool_passes_when_each_member_is_allowed() {
    let scope = RouteScope {
        models: vec!["primary/a".to_owned(), "spare/a".to_owned()],
        accounts: Vec::new(),
    };
    let member = hop("primary", "a");
    assert!(scope.allows_model("a", "a", &member));
}

#[test]
fn d55_4_unrestricted_key_unchanged() {
    let scope = RouteScope::default();
    let member = hop("primary", "demo");
    assert!(scope.allows_model("group/any", "group/any", &member));
}

#[test]
fn d55_5_group_wildcard_holds() {
    assert!(model_allowed(&["group/*".to_owned()], "group/work"));
}

#[test]
fn d55_6_models_list_hides_groups_without_group_scope() {
    let mut routes = BTreeMap::new();
    routes.insert("group/pool".to_owned(), vec![hop("primary", "demo")]);
    routes.insert("demo".to_owned(), vec![hop("primary", "demo")]);
    let router = Router::from_hops(routes, FailoverPolicy::default());
    let scope = RouteScope {
        models: vec!["primary/demo".to_owned()],
        accounts: Vec::new(),
    };
    let listed = router.models_scoped(Some(&scope));
    assert!(listed.iter().any(|id| id == "demo"));
    assert!(!listed.iter().any(|id| id == "group/pool"));
}

#[tokio::test]
async fn d55_7_bare_name_resolves_model_route_before_group() {
    use hofund_ir::{ContentPart, Message, Request, Role};
    use hofund_plugin_api::CountOutcome;

    struct Tracing {
        id: MemberId,
        route_id: &'static str,
        label: &'static str,
        picked: Arc<std::sync::Mutex<Option<&'static str>>>,
    }

    #[async_trait]
    impl Member for Tracing {
        fn id(&self) -> &MemberId {
            &self.id
        }

        fn route_id(&self) -> &str {
            self.route_id
        }

        fn protocol(&self) -> &str {
            "test"
        }

        async fn count_input(
            &self,
            _request: Request,
            _inbound_protocol: &str,
        ) -> Result<CountOutcome, PluginError> {
            *self.picked.lock().expect("lock") = Some(self.label);
            Ok(CountOutcome::Counted(1))
        }

        async fn execute(
            &self,
            _request: Request,
            _inbound: &str,
        ) -> Result<Completion, PluginError> {
            unreachable!()
        }
    }

    fn traced(
        provider: &'static str,
        wire: &str,
        label: &'static str,
        picked: &Arc<std::sync::Mutex<Option<&'static str>>>,
    ) -> Hop {
        Hop {
            member: Arc::new(Tracing {
                id: MemberId::from_preset(
                    PresetId::try_from(provider).expect("preset"),
                    MemberKey::from_static("row"),
                ),
                route_id: provider,
                label,
                picked: Arc::clone(picked),
            }),
            wire_model: Some(wire.to_owned()),
            max_output_tokens: None,
            context_window: None,
            patience: None,
            weight: 1,
            free: false,
            nearly_spent: false,
            fallback: false,
            effort: String::new(),
            cold_after: affinity::CACHE_COLD,
            units: Vec::new(),
        }
    }

    let picked = Arc::new(std::sync::Mutex::new(None));
    let mut routes = BTreeMap::new();
    routes.insert(
        "shared".to_owned(),
        vec![traced("primary", "shared", "model-route", &picked)],
    );
    routes.insert(
        "group/shared".to_owned(),
        vec![traced("backup", "shared", "group-route", &picked)],
    );
    let router = Router::from_hops(routes, FailoverPolicy::default());
    let scope = RouteScope {
        models: vec!["primary/shared".to_owned()],
        accounts: Vec::new(),
    };
    let mut request = Request::new("shared");
    let mut message = Message::new(Role::User);
    message.content.push(ContentPart::text("hi"));
    request.messages.push(message);
    router
        .count_scoped(&request, "plain", Some(&scope))
        .await
        .expect("count");
    assert_eq!(*picked.lock().expect("lock"), Some("model-route"));
}
