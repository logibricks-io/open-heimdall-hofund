//! Serial walk budget across route pool members.

use std::collections::BTreeMap;
use std::sync::{Arc, Mutex};
use std::time::Duration;

use async_trait::async_trait;
use hofund_ir::{Message, Request, Response, Role};
use hofund_plugin_api::{MemberId, MemberKey, PluginError, PresetId};
use hofund_router::{
    Completion, FailoverPolicy, GROUP_WALK_BUDGET, Hop, Member, Route, Router, RouterError,
};

struct Slow {
    id: MemberId,
    name: &'static str,
    delay: Duration,
    calls: Arc<Mutex<Vec<&'static str>>>,
}

#[async_trait]
impl Member for Slow {
    fn id(&self) -> &MemberId {
        &self.id
    }

    fn route_id(&self) -> &str {
        self.name
    }

    fn protocol(&self) -> &str {
        "test"
    }

    async fn execute(&self, _request: Request, _inbound: &str) -> Result<Completion, PluginError> {
        self.calls.lock().expect("calls").push(self.name);
        tokio::time::sleep(self.delay).await;
        Err(PluginError::status_error(503, "busy"))
    }
}

fn answer() -> Completion {
    let mut message = Message::new(Role::Assistant);
    message.content.push(hofund_ir::ContentPart::text("ok"));
    Completion::Unary(Box::new(Response::new(message)))
}

struct OkMember {
    id: MemberId,
    name: &'static str,
    calls: Arc<Mutex<Vec<&'static str>>>,
}

#[async_trait]
impl Member for OkMember {
    fn id(&self) -> &MemberId {
        &self.id
    }

    fn route_id(&self) -> &str {
        self.name
    }

    fn protocol(&self) -> &str {
        "test"
    }

    async fn execute(&self, _request: Request, _inbound: &str) -> Result<Completion, PluginError> {
        self.calls.lock().expect("calls").push(self.name);
        Ok(answer())
    }
}

#[test]
fn d69_1_group_walk_budget_is_nine_minutes() {
    assert_eq!(GROUP_WALK_BUDGET, Duration::from_secs(540));
}

#[tokio::test(start_paused = true)]
async fn d69_2_walk_stops_after_budget_without_asking_the_next_member() {
    let calls = Arc::new(Mutex::new(Vec::new()));
    let hops = vec![
        Hop::new(Arc::new(Slow {
            id: MemberId::from_preset(PresetId::from_static("slow"), MemberKey::from_static("row")),
            name: "slow",
            delay: Duration::from_millis(15),
            calls: Arc::clone(&calls),
        })),
        Hop::new(Arc::new(OkMember {
            id: MemberId::from_preset(
                PresetId::from_static("ready"),
                MemberKey::from_static("row"),
            ),
            name: "ready",
            calls: Arc::clone(&calls),
        })),
    ];
    let router = Router::from_routes(
        BTreeMap::from([("demo".to_owned(), Route::new(hops))]),
        FailoverPolicy::default(),
    )
    .with_group_walk_budget(Duration::from_millis(10));
    let result = router.dispatch(&Request::new("demo"), "test").await;
    let error = match result {
        Err(error) => error,
        Ok(_) => panic!("expected budget exhaustion"),
    };
    let RouterError::Upstream { error, .. } = error else {
        panic!("expected upstream error");
    };
    assert_eq!(error.status(), Some(504));
    assert!(error.message().contains("budget"));
    assert_eq!(calls.lock().expect("calls").as_slice(), ["slow"]);
}
