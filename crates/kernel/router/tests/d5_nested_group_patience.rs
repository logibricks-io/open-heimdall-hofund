//! Innermost-group patience for nested route pools.

use std::collections::{BTreeMap, VecDeque};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use async_trait::async_trait;
use hofund_ir::{Message, Request, Response, Role};
use hofund_plugin_api::{MemberId, MemberKey, PluginError, PresetId};
use hofund_router::{
    Completion, FailoverPolicy, Hop, Member, PoolMemberPatience, Route, Router, Unit, WaitHook,
};

type Script = Mutex<VecDeque<Result<Completion, PluginError>>>;

struct Fake {
    id: MemberId,
    name: &'static str,
    calls: Arc<Mutex<Vec<&'static str>>>,
    answers: Script,
}

#[async_trait]
impl Member for Fake {
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
        self.answers
            .lock()
            .expect("answers")
            .pop_front()
            .expect("scripted answer")
    }
}

fn answer() -> Completion {
    let mut message = Message::new(Role::Assistant);
    message.content.push(hofund_ir::ContentPart::text("ok"));
    Completion::Unary(Box::new(Response::new(message)))
}

fn busy() -> Result<Completion, PluginError> {
    Err(PluginError::status_error(503, "busy"))
}

fn member_id(name: &'static str) -> MemberId {
    MemberId::from_preset(PresetId::from_static(name), MemberKey::from_static("row"))
}

struct Recording(Arc<Mutex<Vec<Duration>>>);

impl WaitHook for Recording {
    fn wait(
        &self,
        duration: Duration,
    ) -> std::pin::Pin<Box<dyn std::future::Future<Output = ()> + Send + '_>> {
        self.0.lock().expect("waits").push(duration);
        Box::pin(tokio::time::sleep(duration))
    }

    fn now(&self) -> std::time::Instant {
        tokio::time::Instant::now().into_std()
    }
}

#[tokio::test(start_paused = true)]
async fn d5_nested_member_waits_by_inner_group_patience_not_the_outer_pool() {
    let calls = Arc::new(Mutex::new(Vec::new()));
    let mut hop = Hop::new(Arc::new(Fake {
        id: member_id("one"),
        name: "one",
        calls: Arc::clone(&calls),
        answers: Mutex::new([busy(), busy(), busy(), Ok(answer())].into_iter().collect()),
    }));
    hop.units = vec![
        Unit {
            key: "group/outer".to_owned(),
            pool: "group/outer".to_owned(),
            routing: "order".to_owned(),
            weight: 1,
            sink: false,
            fallback: false,
            group_patience: None,
        },
        Unit {
            key: "group/inner".to_owned(),
            pool: "group/inner".to_owned(),
            routing: "order".to_owned(),
            weight: 1,
            sink: false,
            fallback: false,
            group_patience: Some(PoolMemberPatience::Wait(Duration::from_secs(90))),
        },
    ];
    let route = Route::new(vec![hop]).with_patience(Some(Duration::from_secs(5)));
    let router = Router::from_routes(
        BTreeMap::from([("group/outer".to_owned(), route)]),
        FailoverPolicy::default(),
    );
    let waits = Arc::new(Mutex::new(Vec::new()));
    let result = router
        .dispatch_with_wait(
            &Request::new("group/outer"),
            "test",
            None,
            Some(Arc::new(Recording(Arc::clone(&waits)))),
        )
        .await
        .expect("inner patience covers a third busy round");
    assert_eq!(result.upstream_id, "one");
    let recorded = waits.lock().expect("waits").clone();
    assert!(
        recorded.len() >= 2,
        "outer five-second patience would stop after one pause: {recorded:?}"
    );
    assert_eq!(recorded[0], Duration::from_secs(2));
    assert_eq!(recorded[1], Duration::from_secs(4));
}
