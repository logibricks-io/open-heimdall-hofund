//! Spend-cap routing skips capped keys and refuses when all are capped.

use std::collections::BTreeMap;
use std::sync::Arc;

use async_trait::async_trait;
use hofund_ir::Request;
use hofund_plugin_api::{MemberId, MemberKey, PluginError, PresetId};
use hofund_router::Completion;
use hofund_router::{
    FailoverPolicy, Hop, Member, Router, RouterError, SpendCapCandidate, SpendCapExhaustedFn,
};

struct StubMember {
    id: MemberId,
    route_id: String,
    key_id: String,
}

fn member(provider: &str, key_id: &str, preset: &'static str) -> Arc<dyn Member> {
    Arc::new(StubMember {
        id: MemberId::from_preset(PresetId::from_static(preset), MemberKey::from_static("k")),
        route_id: provider.to_owned(),
        key_id: key_id.to_owned(),
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

    fn provider_key_id(&self) -> &str {
        &self.key_id
    }

    fn scope_provider(&self) -> &str {
        &self.route_id
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
    let skip = Arc::new(
        move |_route: &str, _asked: &str, provider: &str, _wire: &str, key_id: &str| {
            provider == "p" && (key_id == "a" || key_id == "b")
        },
    );
    let exhausted: SpendCapExhaustedFn = Arc::new(move |candidates: &[SpendCapCandidate]| {
        if candidates.iter().all(|hop| {
            hop.provider == "p"
                && (hop.key_id == "a" || hop.key_id == "b")
                && !hop.key_id.is_empty()
        }) {
            Some(Some(120))
        } else {
            None
        }
    });
    let router = Router::from_hops(
        BTreeMap::from([(
            "demo".to_owned(),
            vec![
                Hop::new(member("p", "a", "a")),
                Hop::new(member("p", "b", "b")),
            ],
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
