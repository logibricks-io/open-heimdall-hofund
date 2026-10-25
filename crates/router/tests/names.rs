//! Model names: a route alias, provider/model, a wire name, and a count.

use std::collections::BTreeMap;
use std::sync::Arc;
use std::sync::Mutex;

use async_trait::async_trait;
use hofund_ir::{ContentPart, Message, Request, Response, Role};
use hofund_plugin_api::{CountOutcome, MemberId, MemberKey, PluginError, PresetId};
use hofund_router::{Completion, FailoverPolicy, Hop, Member, Router, RouterError};

struct Fake {
    route_id: &'static str,
    member_id: MemberId,
    seen: Arc<Mutex<Vec<String>>>,
    count: Count,
    caps: Option<Arc<Mutex<Vec<Option<u64>>>>>,
}

enum Count {
    Number(u64),
    Missing,
    Reject(u16),
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

    async fn count_input(
        &self,
        request: Request,
        _inbound_protocol: &str,
    ) -> Result<CountOutcome, PluginError> {
        self.seen.lock().expect("seen").push(request.model);
        match self.count {
            Count::Number(tokens) => Ok(CountOutcome::Counted(tokens)),
            Count::Missing => Ok(CountOutcome::Unsupported),
            Count::Reject(status) => Err(PluginError::status_error(status, "no")),
        }
    }

    async fn execute(
        &self,
        request: Request,
        _inbound_protocol: &str,
    ) -> Result<Completion, PluginError> {
        self.seen.lock().expect("seen").push(request.model.clone());
        if let Some(caps) = &self.caps {
            caps.lock().expect("caps").push(request.params.max_tokens);
        }
        if let Count::Reject(status) = self.count {
            return Err(PluginError::status_error(status, "down"));
        }
        let mut message = Message::new(Role::Assistant);
        message.content.push(ContentPart::text("ok"));
        Ok(Completion::Unary(Box::new(
            Response::new(message).with_model(Some(request.model)),
        )))
    }
}

fn member_id(id: &'static str) -> MemberId {
    MemberId::from_preset(
        PresetId::try_from(id).expect("preset"),
        MemberKey::from_static("row"),
    )
}

fn hop(id: &'static str, wire: Option<&str>, seen: &Arc<Mutex<Vec<String>>>, count: Count) -> Hop {
    Hop {
        member: Arc::new(Fake {
            route_id: id,
            member_id: member_id(id),
            seen: Arc::clone(seen),
            count,
            caps: None,
        }),
        wire_model: wire.map(str::to_owned),
        max_output_tokens: None,
    }
}

fn capped(
    id: &'static str,
    seen: &Arc<Mutex<Vec<String>>>,
    caps: &Arc<Mutex<Vec<Option<u64>>>>,
) -> Hop {
    Hop {
        member: Arc::new(Fake {
            route_id: id,
            member_id: member_id(id),
            seen: Arc::clone(seen),
            count: Count::Number(1),
            caps: Some(Arc::clone(caps)),
        }),
        wire_model: None,
        max_output_tokens: Some(8192),
    }
}

fn router(routes: BTreeMap<String, Vec<Hop>>) -> Router {
    Router::from_hops(routes, FailoverPolicy::default())
}

fn ask(model: &str) -> Request {
    let mut request = Request::new(model);
    let mut message = Message::new(Role::User);
    message.content.push(ContentPart::text("abcd"));
    request.messages.push(message);
    request
}

fn unary_model(completion: Completion) -> Option<String> {
    match completion {
        Completion::Unary(response) => response.model,
        _ => None,
    }
}

#[tokio::test]
async fn a_route_sends_each_upstream_its_own_model_and_returns_the_client_name() {
    let seen = Arc::new(Mutex::new(Vec::new()));
    let mut routes = BTreeMap::new();
    routes.insert(
        "demo".to_owned(),
        vec![
            hop("primary", Some("upstream-a"), &seen, Count::Reject(500)),
            hop("backup", Some("gpt-demo"), &seen, Count::Missing),
        ],
    );
    let routed = router(routes)
        .dispatch(&ask("demo"), "inbound")
        .await
        .expect("routed");
    assert_eq!(routed.upstream_id, "backup");
    assert_eq!(
        seen.lock().expect("seen").as_slice(),
        ["upstream-a", "gpt-demo"]
    );
    assert_eq!(unary_model(routed.completion).as_deref(), Some("demo"));
}

#[tokio::test]
async fn provider_model_selects_one_hop_and_a_shared_wire_name_is_an_error() {
    let seen = Arc::new(Mutex::new(Vec::new()));
    let mut routes = BTreeMap::new();
    routes.insert(
        "demo".to_owned(),
        vec![hop("primary", Some("shared"), &seen, Count::Missing)],
    );
    routes.insert(
        "other".to_owned(),
        vec![hop("backup", Some("shared"), &seen, Count::Missing)],
    );
    let table = router(routes);
    let routed = table
        .dispatch(&ask("primary/demo"), "inbound")
        .await
        .expect("one hop");
    assert_eq!(routed.upstream_id, "primary");
    assert_eq!(seen.lock().expect("seen").as_slice(), ["shared"]);
    assert_eq!(
        unary_model(routed.completion).as_deref(),
        Some("primary/demo")
    );
    let error = match table.dispatch(&ask("shared"), "inbound").await {
        Ok(_) => panic!("two providers"),
        Err(error) => error,
    };
    let RouterError::Ambiguous { candidates, .. } = error else {
        panic!("ambiguous");
    };
    assert!(candidates.contains("primary/demo"));
    assert!(candidates.contains("backup/other"));
}

#[tokio::test]
async fn a_bare_wire_name_is_used_when_one_provider_serves_it() {
    let seen = Arc::new(Mutex::new(Vec::new()));
    let mut routes = BTreeMap::new();
    routes.insert(
        "alias".to_owned(),
        vec![hop("primary", Some("vendor-model"), &seen, Count::Missing)],
    );
    let routed = router(routes)
        .dispatch(&ask("vendor-model"), "inbound")
        .await
        .expect("bare");
    assert_eq!(routed.upstream_id, "primary");
    assert_eq!(seen.lock().expect("seen").as_slice(), ["vendor-model"]);
}

#[tokio::test]
async fn count_uses_an_upstream_count_and_estimates_when_none_can() {
    let seen = Arc::new(Mutex::new(Vec::new()));
    let mut routes = BTreeMap::new();
    routes.insert(
        "demo".to_owned(),
        vec![
            hop("primary", None, &seen, Count::Missing),
            hop("backup", Some("counted-model"), &seen, Count::Number(17)),
        ],
    );
    let table = router(routes);
    let count = table.count(&ask("demo"), "inbound").await.expect("count");
    assert_eq!(count.tokens, 17);
    assert!(!count.estimated);
    assert_eq!(count.upstream_id, "backup");
    assert_eq!(
        seen.lock().expect("seen").as_slice(),
        ["demo", "counted-model"]
    );

    let seen = Arc::new(Mutex::new(Vec::new()));
    let mut routes = BTreeMap::new();
    routes.insert(
        "demo".to_owned(),
        vec![hop("primary", None, &seen, Count::Missing)],
    );
    let count = router(routes)
        .count(&ask("demo"), "inbound")
        .await
        .expect("estimate");
    assert!(count.estimated);
    assert_eq!(count.tokens, 1);
}

#[tokio::test]
async fn a_hop_cap_fills_an_omitted_max_tokens_and_a_client_cap_wins() {
    let seen = Arc::new(Mutex::new(Vec::new()));
    let caps = Arc::new(Mutex::new(Vec::new()));
    let mut routes = BTreeMap::new();
    routes.insert("demo".to_owned(), vec![capped("primary", &seen, &caps)]);
    let mut request = ask("demo");
    router(routes)
        .dispatch(&request, "inbound")
        .await
        .expect("dispatch");
    request.params.max_tokens = Some(32);
    let mut routes = BTreeMap::new();
    routes.insert("demo".to_owned(), vec![capped("primary", &seen, &caps)]);
    router(routes)
        .dispatch(&request, "inbound")
        .await
        .expect("dispatch");
    assert_eq!(
        caps.lock().expect("caps").as_slice(),
        [Some(8192), Some(32)]
    );
}
