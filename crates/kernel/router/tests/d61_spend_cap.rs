//! Spend-cap routing skips capped keys and refuses when all are capped.

use std::collections::BTreeMap;
use std::sync::Arc;

use async_trait::async_trait;
use hofund_ir::Request;
use hofund_plugin_api::{MemberId, MemberKey, PluginError, PresetId};
use hofund_router::Completion;
use hofund_router::{FailoverPolicy, Hop, Member, Router, RouterError, SpendCapExhaustedFn};

struct StubMember {
    id: MemberId,
    route_id: String,
}

fn member(route_id: &str, preset: &'static str) -> Arc<dyn Member> {
    Arc::new(StubMember {
        id: MemberId::from_preset(PresetId::from_static(preset), MemberKey::from_static("k")),
        route_id: route_id.to_owned(),
    })
}

#[async_trait]
impl Member for StubMember {
    fn id(&self) -> &MemberId {
        &self.id
    }

    fn route_id(&self) -> &str {
        &self.route_id
    }

    fn scope_provider(&self) -> &str {
        "p"
    }

    fn protocol(&self) -> &str {
        "test"
    }

    async fn execute(&self, _: hofund_ir::Request, _: &str) -> Result<Completion, PluginError> {
        Ok(Completion::Unary(Box::new(hofund_ir::Response::new(
            hofund_ir::Message::new(hofund_ir::Role::Assistant),
        ))))
    }
}

#[tokio::test]
async fn d61_6_all_spend_capped_keys_return_spend_cap_reached() {
    let capped = Arc::new(BTreeMap::from([
        ("p#a".to_owned(), true),
        ("p#b".to_owned(), true),
    ]));
    let skip = {
        let capped = Arc::clone(&capped);
        Arc::new(
            move |_route: &str, _asked: &str, _provider: &str, _wire: &str, route_id: &str| {
                capped.contains_key(route_id)
            },
        )
    };
    let exhausted: SpendCapExhaustedFn = Arc::new(move |routes: &[String]| {
        if routes.iter().all(|route| route == "p#a" || route == "p#b") {
            Some(Some(120))
        } else {
            None
        }
    });
    let router = Router::from_hops(
        BTreeMap::from([(
            "demo".to_owned(),
            vec![Hop::new(member("p#a", "a")), Hop::new(member("p#b", "b"))],
        )]),
        FailoverPolicy::default(),
    )
    .with_member_skip(skip)
    .with_spend_cap_exhausted(exhausted);
    let error = match router.dispatch(&Request::new("demo"), "inbound").await {
        Err(error) => error,
        Ok(_) => panic!("expected spend cap refusal"),
    };
    assert!(matches!(
        error,
        RouterError::SpendCapReached {
            retry_after: Some(120)
        }
    ));
}
