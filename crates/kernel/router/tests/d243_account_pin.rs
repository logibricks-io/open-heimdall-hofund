//! Account pin: one named account is asked, and its failure is the reply.

use std::sync::Arc;
use std::sync::atomic::{AtomicUsize, Ordering};

use async_trait::async_trait;
use hofund_plugin_api::{AccountId, MemberId, MemberKey, PluginError, PluginId, PresetId};
use hofund_router::account_pin::{AccountFace, AccountKind};
use hofund_router::{
    CooldownPolicy, Cooldowns, FailoverPolicy, Hop, Member, Request, RouteScope, Router,
    RouterError,
};

struct Fake {
    member_id: MemberId,
    route_id: &'static str,
    provider: &'static str,
    key_id: String,
    hits: Arc<AtomicUsize>,
}

#[async_trait]
impl Member for Fake {
    fn id(&self) -> &MemberId {
        &self.member_id
    }

    fn route_id(&self) -> &str {
        self.route_id
    }

    fn scope_provider(&self) -> &str {
        self.provider
    }

    fn provider_key_id(&self) -> &str {
        &self.key_id
    }

    fn account_refs(&self) -> Vec<String> {
        if self.key_id.is_empty() {
            self.member_id
                .account()
                .map(|account| account.to_string())
                .into_iter()
                .collect()
        } else {
            vec![self.key_id.clone()]
        }
    }

    fn protocol(&self) -> &str {
        "plain"
    }

    async fn execute(
        &self,
        _request: Request,
        _inbound: &str,
    ) -> Result<hofund_router::Completion, PluginError> {
        self.hits.fetch_add(1, Ordering::SeqCst);
        Err(PluginError::status_error(401, "Incorrect API key"))
    }
}

fn key_member(provider: &'static str, fingerprint: &str, hits: &Arc<AtomicUsize>) -> Arc<Fake> {
    Arc::new(Fake {
        member_id: MemberId::from_preset(
            PresetId::try_from(provider).expect("preset"),
            MemberKey::try_from(fingerprint).unwrap_or_else(|_| MemberKey::from_static("default")),
        ),
        route_id: provider,
        provider,
        key_id: fingerprint.to_owned(),
        hits: Arc::clone(hits),
    })
}

fn router(hops: Vec<Hop>, accounts: Vec<AccountFace>) -> Router {
    let mut routes = std::collections::BTreeMap::new();
    routes.insert("demo".to_owned(), hops);
    Router::from_hops(routes, FailoverPolicy::default()).with_accounts(accounts)
}

async fn pin(router: &Router, account: &str) -> RouterError {
    match router
        .dispatch_scoped_for_account(&Request::new("demo"), "plain", None, Some(account))
        .await
    {
        Ok(_) => panic!("the pin was answered"),
        Err(error) => error,
    }
}

#[tokio::test]
async fn d243_pinned_key_returns_its_error_and_skips_the_other() {
    let bad_hits = Arc::new(AtomicUsize::new(0));
    let good_hits = Arc::new(AtomicUsize::new(0));
    let bad = key_member("keyed", "badbadbad1", &bad_hits);
    let good = key_member("keyed", "goodgood11", &good_hits);
    let table = router(
        vec![Hop::new(bad), Hop::new(good)],
        vec![
            AccountFace::key("keyed", "badbadbad1", "broken", "api", true),
            AccountFace::key("keyed", "goodgood11", "work", "api", true),
        ],
    );
    let error = pin(&table, "keyed/badbadbad1").await;
    let RouterError::Upstream { error, .. } = error else {
        panic!("expected the vendor error, got {error}");
    };
    assert_eq!(error.status(), Some(401));
    assert!(error.message().contains("Incorrect API key"));
    assert_eq!(bad_hits.load(Ordering::SeqCst), 1);
    assert_eq!(good_hits.load(Ordering::SeqCst), 0);
}

#[tokio::test]
async fn d243_bare_id_and_only_key_pin_by_fingerprint() {
    let hits = Arc::new(AtomicUsize::new(0));
    let only = key_member("keyed", "onlykey123", &hits);
    let table = router(vec![Hop::new(only)], Vec::new());
    let error = pin(&table, "onlykey123").await;
    assert!(matches!(error, RouterError::Upstream { .. }));
    assert_eq!(hits.load(Ordering::SeqCst), 1);
}

#[tokio::test]
async fn d243_off_key_is_not_asked() {
    let hits = Arc::new(AtomicUsize::new(0));
    let live = key_member("keyed", "livekey123", &hits);
    let table = router(
        vec![Hop::new(live)],
        vec![
            AccountFace::key("keyed", "livekey123", "work", "", true),
            AccountFace::key("keyed", "offkey1234", "spare", "", false),
        ],
    );
    let error = pin(&table, "offkey1234").await;
    let RouterError::Pinned { status, message } = error else {
        panic!("expected a pin refusal, got {error}");
    };
    assert_eq!(status, 404);
    assert!(message.contains("no account"));
    assert_eq!(hits.load(Ordering::SeqCst), 0);
}

#[tokio::test]
async fn d243_resting_pin_is_429() {
    let resting_hits = Arc::new(AtomicUsize::new(0));
    let other_hits = Arc::new(AtomicUsize::new(0));
    let resting = key_member("keyed", "restingkey", &resting_hits);
    let other = key_member("keyed", "otherkey12", &other_hits);
    let cooldowns = Arc::new(Cooldowns::new(CooldownPolicy::default()));
    cooldowns.record_failure_for_model(
        resting.id(),
        "demo",
        &PluginError::status_error(429, "slow down"),
    );
    let mut routes = std::collections::BTreeMap::new();
    routes.insert(
        "demo".to_owned(),
        vec![
            Hop::new(Arc::clone(&resting) as Arc<dyn Member>),
            Hop::new(other),
        ],
    );
    let table = Router::from_hops(routes, FailoverPolicy::default())
        .with_cooldowns(Arc::clone(&cooldowns))
        .with_accounts(vec![
            AccountFace::key("keyed", "restingkey", "tired", "", true),
            AccountFace::key("keyed", "otherkey12", "work", "", true),
        ]);
    let error = pin(&table, "tired").await;
    let RouterError::Pinned { status, message } = error else {
        panic!("expected a pin refusal, got {error}");
    };
    assert_eq!(status, 429);
    assert!(message.contains("rests"));
    assert!(message.contains("no other account is tried"));
    assert_eq!(resting_hits.load(Ordering::SeqCst), 0);
    assert_eq!(other_hits.load(Ordering::SeqCst), 0);
}

#[tokio::test]
async fn d243_held_pin_is_403() {
    let held_hits = Arc::new(AtomicUsize::new(0));
    let allowed_hits = Arc::new(AtomicUsize::new(0));
    let held = key_member("keyed", "heldkey123", &held_hits);
    let allowed = key_member("keyed", "allowedk12", &allowed_hits);
    let table = router(
        vec![Hop::new(held), Hop::new(allowed)],
        vec![
            AccountFace::key("keyed", "heldkey123", "held", "", true),
            AccountFace::key("keyed", "allowedk12", "open", "", true),
        ],
    );
    let scope = RouteScope {
        models: Vec::new(),
        accounts: vec!["keyed/allowedk12".to_owned()],
    };
    let error = match table
        .dispatch_scoped_for_account(&Request::new("demo"), "plain", Some(&scope), Some("held"))
        .await
    {
        Ok(_) => panic!("the pin was answered"),
        Err(error) => error,
    };
    let RouterError::Pinned { status, message } = error else {
        panic!("expected a pin refusal, got {error}");
    };
    assert_eq!(status, 403);
    assert!(message.contains("may not use"));
    assert_eq!(held_hits.load(Ordering::SeqCst), 0);
    assert_eq!(allowed_hits.load(Ordering::SeqCst), 0);
}

#[tokio::test]
async fn d243_plan_exclusion_is_400() {
    let hits = Arc::new(AtomicUsize::new(0));
    let member = key_member("keyed", "plankey123", &hits);
    let table = router(
        vec![Hop::new(member)],
        vec![
            AccountFace::key("keyed", "plankey123", "narrow", "plus", true)
                .with_models(vec!["other".to_owned()]),
        ],
    );
    let error = pin(&table, "narrow").await;
    let RouterError::Pinned { status, message } = error else {
        panic!("expected a pin refusal, got {error}");
    };
    assert_eq!(status, 400);
    assert!(message.contains("plan doesn't list demo"));
    assert_eq!(hits.load(Ordering::SeqCst), 0);
}

#[tokio::test]
async fn d243_missing_pin_names_accounts_and_decodes_a_user() {
    let hits = Arc::new(AtomicUsize::new(0));
    let plugin = PluginId::try_from("acct").expect("plugin");
    let account = AccountId::try_from("login-2").expect("account");
    let member = Arc::new(Fake {
        member_id: MemberId::from_parts(plugin, account),
        route_id: "acct",
        provider: "acct",
        key_id: String::new(),
        hits: Arc::clone(&hits),
    });
    let face = AccountFace::account("acct", "login-2", "spare@example.com", "plus", true);
    assert_eq!(face.kind, AccountKind::Account);
    let table = router(vec![Hop::new(member)], vec![face]);
    let error = pin(&table, "nobody").await;
    let RouterError::Pinned { status, message } = error else {
        panic!("expected a pin refusal, got {error}");
    };
    assert_eq!(status, 404);
    assert!(message.contains("nobody"));
    assert!(message.contains("its accounts are"));
    assert!(message.contains("spare@example.com"));
    assert_eq!(hits.load(Ordering::SeqCst), 0);

    let decoded = pin(&table, "spare%40example.com").await;
    assert!(matches!(decoded, RouterError::Upstream { .. }));
    assert_eq!(hits.load(Ordering::SeqCst), 1);
}
