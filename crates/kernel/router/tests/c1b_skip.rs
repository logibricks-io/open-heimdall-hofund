//! C-1b: router skips hops disabled in the catalog.

use std::collections::BTreeMap;
use std::sync::Arc;
use std::sync::atomic::{AtomicUsize, Ordering};

use async_trait::async_trait;
use hofund_config::CatalogSettings;
use hofund_ir::{Message, Request, Response, Role};
use hofund_model_catalog::CatalogVisibility;
use hofund_plugin_api::{MemberId, MemberKey, PluginError, PresetId};
use hofund_router::{Completion, FailoverPolicy, Hop, Member, Router};

struct CountingMember {
    member_id: MemberId,
    route_id: &'static str,
    hits: Arc<AtomicUsize>,
}

#[async_trait]
impl Member for CountingMember {
    fn id(&self) -> &MemberId {
        &self.member_id
    }

    fn route_id(&self) -> &str {
        self.route_id
    }

    fn protocol(&self) -> &str {
        "test"
    }

    async fn execute(&self, _: Request, _: &str) -> Result<Completion, PluginError> {
        self.hits.fetch_add(1, Ordering::SeqCst);
        Ok(Completion::Unary(Box::new(Response::new(Message::new(
            Role::Assistant,
        )))))
    }
}

fn member(route_id: &'static str, hits: Arc<AtomicUsize>) -> Arc<dyn Member> {
    Arc::new(CountingMember {
        member_id: MemberId::from_preset(
            PresetId::try_from(route_id).expect("preset"),
            MemberKey::from_static("k"),
        ),
        route_id,
        hits,
    })
}

#[tokio::test]
async fn c1b_12_router_skips_disabled_member_hop() {
    let off_hits = Arc::new(AtomicUsize::new(0));
    let on_hits = Arc::new(AtomicUsize::new(0));
    let off = member("off", Arc::clone(&off_hits));
    let on = member("on", Arc::clone(&on_hits));
    let hops = vec![Hop::new(Arc::clone(&off)), Hop::new(Arc::clone(&on))];
    let settings = CatalogSettings {
        disabled: vec!["off/*".to_owned()],
        ..Default::default()
    };
    let visibility = CatalogVisibility::open(settings, std::path::Path::new("."));
    let skip = {
        let visibility = visibility.clone();
        Arc::new(
            move |route: &str, asked: &str, provider: &str, wire: &str, _key_id: &str| {
                visibility.hop_disabled(route, asked, provider, wire)
            },
        )
    };
    let router = Router::from_hops(
        BTreeMap::from([("demo".to_owned(), hops)]),
        FailoverPolicy::default(),
    )
    .with_member_skip(skip);
    let request = Request::new("demo");
    router
        .dispatch(&request, "inbound")
        .await
        .expect("dispatch");
    assert_eq!(off_hits.load(Ordering::SeqCst), 0);
    assert_eq!(on_hits.load(Ordering::SeqCst), 1);
}
