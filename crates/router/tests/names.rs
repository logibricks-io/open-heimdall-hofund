//! Model names: a route alias, provider/model, a wire name, and a count.

use std::collections::BTreeMap;
use std::sync::Arc;
use std::sync::Mutex;

use async_trait::async_trait;
use hofund_ir::{ContentPart, Message, Request, Response, Role};
use hofund_router::{
    Completion, CountOutcome, FailoverPolicy, Hop, Provider, ProviderError, Router, RouterError,
};

struct Fake {
    id: &'static str,
    seen: Arc<Mutex<Vec<String>>>,
    count: Count,
}

enum Count {
    Number(u64),
    Missing,
    Reject(u16),
}

#[async_trait]
impl Provider for Fake {
    fn id(&self) -> &str {
        self.id
    }

    async fn count_input(
        &self,
        request: Request,
        _inbound_protocol: &str,
    ) -> Result<CountOutcome, ProviderError> {
        self.seen.lock().expect("seen").push(request.model);
        match self.count {
            Count::Number(tokens) => Ok(CountOutcome::Counted(tokens)),
            Count::Missing => Ok(CountOutcome::Unsupported),
            Count::Reject(status) => Err(ProviderError::status(status, "no")),
        }
    }

    async fn execute(
        &self,
        request: Request,
        _inbound_protocol: &str,
    ) -> Result<Completion, ProviderError> {
        self.seen.lock().expect("seen").push(request.model.clone());
        if let Count::Reject(status) = self.count {
            return Err(ProviderError::status(status, "down"));
        }
        let mut message = Message::new(Role::Assistant);
        message.content.push(ContentPart::text("ok"));
        Ok(Completion::Unary(Box::new(Response {
            id: None,
            model: Some(request.model),
            message,
            stop: None,
            usage: None,
            extensions: hofund_ir::Extensions::new(),
        })))
    }
}

fn hop(id: &'static str, wire: Option<&str>, seen: &Arc<Mutex<Vec<String>>>, count: Count) -> Hop {
    Hop {
        provider: Arc::new(Fake {
            id,
            seen: Arc::clone(seen),
            count,
        }),
        wire_model: wire.map(str::to_owned),
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
        .dispatch(ask("demo"), "inbound")
        .await
        .expect("routed");
    assert_eq!(routed.upstream_id, "backup");
    assert_eq!(
        seen.lock().expect("seen").as_slice(),
        ["upstream-a", "gpt-demo"]
    );
    match routed.completion {
        Completion::Unary(response) => assert_eq!(response.model.as_deref(), Some("demo")),
        Completion::Stream(_) => panic!("unary"),
    }
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
        .dispatch(ask("primary/demo"), "inbound")
        .await
        .expect("one hop");
    assert_eq!(routed.upstream_id, "primary");
    assert_eq!(seen.lock().expect("seen").as_slice(), ["shared"]);
    match routed.completion {
        Completion::Unary(response) => assert_eq!(response.model.as_deref(), Some("primary/demo")),
        Completion::Stream(_) => panic!("unary"),
    }
    let error = match table.dispatch(ask("shared"), "inbound").await {
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
        .dispatch(ask("vendor-model"), "inbound")
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
    let count = table.count(ask("demo"), "inbound").await.expect("count");
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
        .count(ask("demo"), "inbound")
        .await
        .expect("estimate");
    assert!(count.estimated);
    assert_eq!(count.tokens, 1);
}
