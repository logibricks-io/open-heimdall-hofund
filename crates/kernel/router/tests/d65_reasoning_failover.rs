//! Failover when a member demands earlier reasoning back.

use std::collections::BTreeMap;
use std::sync::Arc;
use std::sync::atomic::{AtomicUsize, Ordering};

use async_trait::async_trait;
use hofund_ir::{Message, Request, Response, Role};
use hofund_plugin_api::{MemberId, MemberKey, PluginError, PresetId};
use hofund_router::{Completion, FailoverPolicy, Hop, Member, Route, Router};

struct Fake {
    member_id: MemberId,
    id: &'static str,
    hits: Arc<AtomicUsize>,
    message: &'static str,
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
        "inbound"
    }

    async fn execute(&self, _request: Request, _inbound: &str) -> Result<Completion, PluginError> {
        self.hits.fetch_add(1, Ordering::SeqCst);
        Err(PluginError::status_error(400, self.message))
    }
}

fn ok() -> Completion {
    let mut message = Message::new(Role::Assistant);
    message.content.push(hofund_ir::ContentPart::text("ok"));
    Completion::Unary(Box::new(Response::new(message)))
}

struct OkOnce {
    member_id: MemberId,
    id: &'static str,
    hits: Arc<AtomicUsize>,
}

#[async_trait]
impl Member for OkOnce {
    fn id(&self) -> &MemberId {
        &self.member_id
    }

    fn route_id(&self) -> &str {
        self.id
    }

    fn protocol(&self) -> &str {
        "inbound"
    }

    async fn execute(&self, _request: Request, _inbound: &str) -> Result<Completion, PluginError> {
        self.hits.fetch_add(1, Ordering::SeqCst);
        Ok(ok())
    }
}

#[tokio::test]
async fn d65_9_group_failsover_missing_reasoning_to_the_next_member() {
    let beam_hits = Arc::new(AtomicUsize::new(0));
    let glyph_hits = Arc::new(AtomicUsize::new(0));
    let hops = vec![
        Hop::new(Arc::new(Fake {
            member_id: MemberId::from_preset(
                PresetId::from_static("beam"),
                MemberKey::from_static("row"),
            ),
            id: "beam",
            hits: Arc::clone(&beam_hits),
            message: r#"Missing required parameter: 'messages[1].reasoning_content'"#,
        })),
        Hop::new(Arc::new(OkOnce {
            member_id: MemberId::from_preset(
                PresetId::from_static("glyph"),
                MemberKey::from_static("row"),
            ),
            id: "glyph",
            hits: Arc::clone(&glyph_hits),
        })),
    ];
    let router = Router::from_routes(
        BTreeMap::from([("demo".to_owned(), Route::new(hops))]),
        FailoverPolicy::default(),
    );
    let routed = router
        .dispatch(&Request::new("demo"), "inbound")
        .await
        .expect("answered");
    assert_eq!(routed.upstream_id, "glyph");
    assert_eq!(beam_hits.load(Ordering::SeqCst), 1);
    assert_eq!(glyph_hits.load(Ordering::SeqCst), 1);
}
