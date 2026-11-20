//! Advisor usage rows and caller-scoped nested calls. Fakes only.

use std::collections::{BTreeMap, HashMap};
use std::sync::{Arc, Mutex};

use async_trait::async_trait;
use axum::body::Body;
use axum::http::{Request, StatusCode};
use hofund_codec::{AnthropicMessages, Codec};
use hofund_config::{CatalogSettings, ModelPriceOverride, UsageSettings};
use hofund_gateway::Gateway;
use hofund_ir::{AdvisorCall, Message, Request as ModelRequest, Response, Role, Usage};
use hofund_model_catalog::{CatalogFetcher, EffectivePricer, FetchResponse, ModelCatalog};
use hofund_plugin_api::{CallerStamp, MemberId, MemberKey, PluginError, PresetId};
use hofund_router::{Completion, FailoverPolicy, Hop, Member, Router, Snapshot};
use hofund_secrets::KeyStore;
use hofund_usage::Ledger;
use tower::ServiceExt;

struct Fake {
    member_id: MemberId,
    hits: Arc<Mutex<Vec<String>>>,
    advisor_cache_read: u64,
    hold: Option<Arc<Hold>>,
    speak: &'static str,
    /// A unary usage whose input already includes cache reads, and no iterations.
    plain: bool,
}

struct Hold {
    entered: Mutex<Option<tokio::sync::oneshot::Sender<()>>>,
    release: Mutex<Option<tokio::sync::oneshot::Receiver<()>>>,
}

#[async_trait]
impl Member for Fake {
    fn id(&self) -> &MemberId {
        &self.member_id
    }

    fn route_id(&self) -> &str {
        "relay"
    }

    fn protocol(&self) -> &str {
        self.speak
    }

    async fn execute(
        &self,
        request: ModelRequest,
        _inbound: &str,
    ) -> Result<Completion, PluginError> {
        self.hits.lock().expect("hits").push(request.model.clone());
        if let Some(hold) = &self.hold {
            if let Some(entered) = hold.entered.lock().expect("entered").take() {
                let _ = entered.send(());
            }
            let release = hold.release.lock().expect("hold").take();
            if let Some(release) = release {
                let _ = release.await;
            }
        }
        let mut message = Message::new(Role::Assistant);
        message.content = vec![hofund_ir::ContentPart::Text(hofund_ir::TextPart {
            text: "look there".to_owned(),
            extensions: hofund_ir::Extensions::new(),
        })];
        let usage = if self.plain {
            Usage {
                input_tokens: Some(100),
                output_tokens: Some(8),
                cache_read_tokens: Some(40),
                ..Usage::default()
            }
        } else {
            Usage {
                input_tokens: Some(1760),
                output_tokens: Some(531),
                cache_read_tokens: Some(412),
                advisor_calls: vec![AdvisorCall {
                    model: "reviewer-5-5".to_owned(),
                    input_tokens: 823,
                    output_tokens: 1612,
                    cache_read_tokens: self.advisor_cache_read,
                    ..AdvisorCall::default()
                }],
                ..Usage::default()
            }
        };
        let response = Response::new(message).with_usage(Some(usage));
        Ok(Completion::Unary(Box::new(response)))
    }
}

struct Offline;

impl CatalogFetcher for Offline {
    fn fetch(
        &self,
        _url: &str,
        _if_none_match: Option<&str>,
    ) -> Result<FetchResponse, std::io::Error> {
        Err(std::io::Error::other("offline"))
    }
}

struct List;

impl hofund_model_catalog::ListPriceLookup for List {
    fn list_price(&self, _provider_id: &str, _model: &str) -> Option<hofund_model_catalog::Price> {
        None
    }
}

struct Fixture {
    app: axum::Router,
    ask: Arc<dyn hofund_plugin_api::AskBackend>,
    usage: Arc<Ledger>,
    hits: Arc<Mutex<Vec<String>>>,
    _dir: tempfile::TempDir,
}

fn price(input: f64, output: f64) -> ModelPriceOverride {
    ModelPriceOverride {
        input,
        output,
        cache_read: 4.0,
        cache_write: 0.0,
        cache_write_1h: None,
    }
}

fn hop(member: Arc<Fake>, wire: &str) -> Hop {
    let mut hop = Hop::new(member);
    hop.wire_model = Some(wire.to_owned());
    hop
}

fn fixture() -> Fixture {
    fixture_with(0, None)
}

fn fixture_with(advisor_cache_read: u64, hold: Option<Arc<Hold>>) -> Fixture {
    fixture_member(advisor_cache_read, hold, "subscription", false, false)
}

fn fixture_member(
    advisor_cache_read: u64,
    hold: Option<Arc<Hold>>,
    speak: &'static str,
    plain: bool,
    price_route: bool,
) -> Fixture {
    let dir = tempfile::tempdir().expect("dir");
    let store = Arc::new(KeyStore::beside_token(dir.path().join("hofund.token")));
    store.list().expect("migration");
    let usage = Arc::new(Ledger::open(&dir.path().join("usage.sqlite")).expect("usage"));
    let hits = Arc::new(Mutex::new(Vec::new()));
    let member = Arc::new(Fake {
        member_id: MemberId::from_preset(
            PresetId::try_from("relay").expect("preset"),
            MemberKey::from_static("row"),
        ),
        hits: Arc::clone(&hits),
        advisor_cache_read,
        hold,
        speak,
        plain,
    });
    let routes = BTreeMap::from([
        (
            "relay/worker-5.5".to_owned(),
            vec![hop(Arc::clone(&member), "worker-5-5")],
        ),
        (
            "relay/reviewer-5.5".to_owned(),
            vec![hop(member, "reviewer-5.5")],
        ),
    ]);
    let catalog = ModelCatalog::open(dir.path(), CatalogSettings::default(), Arc::new(Offline))
        .expect("catalog");
    let mut settings = UsageSettings::default();
    settings
        .prices
        .insert("*/worker-5-5".into(), price(1.0, 2.0));
    settings
        .prices
        .insert("*/reviewer-5.5".into(), price(10.0, 20.0));
    if price_route {
        settings
            .prices
            .insert("*/relay/reviewer-5.5".into(), price(10.0, 20.0));
    }
    let pricer = Arc::new(EffectivePricer::new(settings, catalog, Arc::new(List)));
    let token = store.token_file().read().expect("token");
    let gateway = Gateway::new(
        Snapshot::new(Router::from_hops(routes, FailoverPolicy::default())),
        token.clone(),
        HashMap::from([(
            "/v1/messages".to_owned(),
            Arc::new(AnthropicMessages) as Arc<dyn Codec>,
        )]),
        Arc::clone(&usage),
    )
    .with_key_store(store)
    .with_effective_pricer(pricer);
    let ask = gateway.ask_backend();
    Fixture {
        app: gateway.into_router(),
        ask,
        usage,
        hits,
        _dir: dir,
    }
}

#[tokio::test]
async fn d227_advisor_iterations_are_their_own_priced_rows() {
    let fixture = fixture();
    let secret = fixture.app.clone();
    let token = std::fs::read_to_string(fixture._dir.path().join("hofund.token")).expect("token");
    let response = secret
        .oneshot(
            Request::builder()
                .method("POST")
                .uri("/v1/messages")
                .header("authorization", format!("Bearer {}", token.trim()))
                .header("content-type", "application/json")
                .body(Body::from(
                    r#"{"model":"relay/worker-5.5","max_tokens":32,"messages":[{"role":"user","content":"hi"}],"tools":[{"type":"advisor_20260301","name":"advisor","model":"relay/reviewer-5.5"}]}"#,
                ))
                .expect("request"),
        )
        .await
        .expect("response");
    assert_eq!(response.status(), StatusCode::OK);
    let rows = fixture.usage.rows().expect("rows");
    let executor = rows
        .iter()
        .find(|row| row.kind.is_empty())
        .expect("executor");
    let advisor = rows
        .iter()
        .find(|row| row.kind == "advisor")
        .expect("advisor");
    assert_eq!(executor.input_tokens, Some(1760));
    assert_eq!(executor.output_tokens, Some(531));
    assert_eq!(executor.cache_read_tokens, Some(412));
    assert_eq!(executor.status, "200");
    assert_eq!(advisor.model, "reviewer-5.5");
    assert_eq!(advisor.served_model, "reviewer-5-5");
    assert_eq!(advisor.input_tokens, Some(823));
    assert_eq!(advisor.output_tokens, Some(1612));
    assert_eq!(advisor.status, "200");
    assert_eq!(executor.model, "relay/worker-5.5");
    let executor_cost = executor.cost_usd.expect("executor price");
    let advisor_cost = advisor.cost_usd.expect("advisor price");
    assert!(advisor.priced);
    assert!(
        (advisor_cost - (823.0 / 1_000_000.0 * 10.0 + 1612.0 / 1_000_000.0 * 20.0)).abs() < 1e-9
    );
    assert!(executor_cost != advisor_cost);
}

#[tokio::test]
async fn d227_advisor_ask_keeps_the_callers_model_list() {
    let fixture = fixture();
    let held = CallerStamp {
        key_id: "held".into(),
        models: vec!["relay/worker-5.5".into()],
        ..CallerStamp::default()
    };
    assert!(
        fixture.ask.serves(&held, "relay/reviewer-5.5"),
        "a model this gateway routes stays offered"
    );
    let mut request = ModelRequest::new("relay/reviewer-5.5");
    request.messages = vec![Message::new(Role::User)];
    let error = match fixture.ask.ask(&held, request).await {
        Err(error) => error,
        Ok(_) => panic!("a key held to one model was allowed to ask another"),
    };
    assert_eq!(error.status(), Some(403));
    assert!(fixture.hits.lock().expect("hits").is_empty());

    let open = CallerStamp {
        key_id: "open".into(),
        ..CallerStamp::default()
    };
    assert!(fixture.ask.serves(&open, "relay/reviewer-5.5"));
    let mut request = ModelRequest::new("relay/reviewer-5.5");
    request.messages = vec![Message::new(Role::User)];
    assert!(fixture.ask.ask(&open, request).await.is_ok());
    assert_eq!(
        fixture.hits.lock().expect("hits").as_slice(),
        ["reviewer-5.5"]
    );
}

#[tokio::test]
async fn d227_advisor_cache_read_stays_on_its_own_column() {
    let fixture = fixture_with(100, None);
    let secret = fixture.app.clone();
    let token = std::fs::read_to_string(fixture._dir.path().join("hofund.token")).expect("token");
    let response = secret
        .oneshot(
            Request::builder()
                .method("POST")
                .uri("/v1/messages")
                .header("authorization", format!("Bearer {}", token.trim()))
                .header("content-type", "application/json")
                .body(Body::from(
                    r#"{"model":"relay/worker-5.5","max_tokens":32,"messages":[{"role":"user","content":"hi"}],"tools":[{"type":"advisor_20260301","name":"advisor","model":"relay/reviewer-5.5"}]}"#,
                ))
                .expect("request"),
        )
        .await
        .expect("response");
    assert_eq!(response.status(), StatusCode::OK);
    let rows = fixture.usage.rows().expect("rows");
    let advisor = rows
        .iter()
        .find(|row| row.kind == "advisor")
        .expect("advisor");
    assert_eq!(advisor.input_tokens, Some(823));
    assert_eq!(advisor.cache_read_tokens, Some(100));
    let cost = advisor.cost_usd.expect("advisor price");
    let expected = (823.0 * 10.0 + 100.0 * 4.0 + 1612.0 * 20.0) / 1_000_000.0;
    assert!((cost - expected).abs() < 1e-9, "{cost} != {expected}");
}

#[tokio::test]
async fn d227_advisor_ask_holds_a_prompt_estimate() {
    let (entered_tx, entered_rx) = tokio::sync::oneshot::channel();
    let (release_tx, release_rx) = tokio::sync::oneshot::channel();
    let hold = Arc::new(Hold {
        entered: Mutex::new(Some(entered_tx)),
        release: Mutex::new(Some(release_rx)),
    });
    let fixture = fixture_with(0, Some(Arc::clone(&hold)));
    let mut heavy = ModelRequest::new("relay/reviewer-5.5");
    let mut user = Message::new(Role::User);
    user.content = vec![hofund_ir::ContentPart::Text(hofund_ir::TextPart {
        text: "x".repeat(400),
        extensions: hofund_ir::Extensions::new(),
    })];
    heavy.messages = vec![user];
    let limited = CallerStamp {
        key_id: "limited".into(),
        limits: vec![hofund_plugin_api::CallerLimit {
            period: "day".into(),
            tokens: 1,
            ..hofund_plugin_api::CallerLimit::default()
        }],
        ..CallerStamp::default()
    };
    let ask = Arc::clone(&fixture.ask);
    let first_request = heavy.clone();
    let first_caller = limited.clone();
    let first = tokio::spawn(async move { ask.ask(&first_caller, first_request).await });
    entered_rx
        .await
        .expect("the first ask reached the provider");
    let second = fixture.ask.ask(&limited, heavy).await;
    let error = match second {
        Err(error) => error,
        Ok(_) => panic!("a second ask was not held to the first prompt"),
    };
    assert_eq!(error.status(), Some(429));
    let _ = release_tx.send(());
    assert!(first.await.expect("join").is_ok());
}

#[tokio::test]
async fn d227_nested_ask_records_usage_with_the_hop_protocol() {
    let speak = hofund_plugin_api::ProtocolId::OPENAI_CHAT.as_str();
    let fixture = fixture_member(0, None, speak, true, true);
    let caller = CallerStamp {
        key_id: "open".into(),
        ..CallerStamp::default()
    };
    let mut request = ModelRequest::new("relay/reviewer-5.5");
    request.messages = vec![Message::new(Role::User)];
    fixture.ask.ask(&caller, request).await.expect("ask");
    let rows = fixture.usage.rows().expect("rows");
    let row = rows
        .iter()
        .find(|row| row.kind.is_empty())
        .expect("nested row");
    assert_eq!(row.input_tokens, Some(100));
    assert_eq!(row.cache_read_tokens, Some(40));
    assert_eq!(row.output_tokens, Some(8));
    let cost = row.cost_usd.expect("priced");
    let expected = (60.0 * 10.0 + 40.0 * 4.0 + 8.0 * 20.0) / 1_000_000.0;
    assert!((cost - expected).abs() < 1e-9, "{cost} != {expected}");
}
