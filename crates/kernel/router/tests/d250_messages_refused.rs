//! API-wide Messages refusal uses another protocol and is remembered (D-250).

use std::collections::BTreeMap;
use std::sync::{Arc, Mutex};

use async_trait::async_trait;
use hofund_ir::{ContentPart, Message, Response, Role};
use hofund_plugin_api::{MemberId, MemberKey, PluginError, PresetId};
use hofund_router::{ApiFitness, Completion, FailoverPolicy, Hop, Member, Route, Router};

const REFUSAL: &str = r#"{"type":"error","error":{"type":"permission_error","message":"This group does not allow /v1/messages dispatch"}}"#;

struct Fake {
    id: MemberId,
    protocol: &'static str,
    seen: Arc<Mutex<Vec<String>>>,
    stands_in_for: Option<String>,
    status: u16,
    body: &'static str,
}

#[async_trait]
impl Member for Fake {
    fn id(&self) -> &MemberId {
        &self.id
    }

    fn route_id(&self) -> &str {
        "relay"
    }

    fn protocol(&self) -> &str {
        self.protocol
    }

    fn stands_in_for(&self) -> Option<&str> {
        self.stands_in_for.as_deref()
    }

    async fn execute(
        &self,
        request: hofund_router::Request,
        _inbound: &str,
    ) -> Result<Completion, PluginError> {
        self.seen
            .lock()
            .expect("seen")
            .push(format!("{}:{}", request.model, self.protocol));
        if self.protocol == "messages" {
            return Err(PluginError::status_error(self.status, self.body));
        }
        let mut message = Message::new(Role::Assistant);
        message.content.push(ContentPart::text("chat ok"));
        Ok(Completion::Unary(Box::new(Response::new(message))))
    }
}

fn hop(
    key: &str,
    protocol: &'static str,
    seen: &Arc<Mutex<Vec<String>>>,
    stands_in_for: Option<String>,
    status: u16,
    body: &'static str,
) -> Hop {
    Hop::new(Arc::new(Fake {
        id: MemberId::from_preset(
            PresetId::from_static("relay"),
            MemberKey::try_from(key).expect("member key"),
        ),
        protocol,
        seen: Arc::clone(seen),
        stands_in_for,
        status,
        body,
    }))
}

fn pair(seen: &Arc<Mutex<Vec<String>>>, status: u16, body: &'static str) -> Vec<Hop> {
    let messages = hop("messages", "messages", seen, None, status, body);
    let primary = messages.member.id().to_string();
    vec![messages, hop("chat", "chat", seen, Some(primary), 200, "")]
}

fn router(
    models: &[&str],
    fitness: ApiFitness,
    seen: &Arc<Mutex<Vec<String>>>,
    statuses: &[u16],
    status: u16,
    body: &'static str,
) -> Router {
    Router::from_routes(
        models
            .iter()
            .map(|model| {
                (
                    (*model).to_owned(),
                    Route::new(pair(seen, status, body)).with_patience(None),
                )
            })
            .collect::<BTreeMap<_, _>>(),
        FailoverPolicy {
            connect_errors: false,
            statuses: statuses.iter().copied().collect(),
        },
    )
    .with_api_fitness(fitness)
}

#[tokio::test]
async fn d250_refused_messages_uses_chat_and_stays_unfit_for_every_model() {
    let seen = Arc::new(Mutex::new(Vec::new()));
    let fitness = ApiFitness::default();
    let first = router(
        &["model-a", "model-b"],
        fitness.clone(),
        &seen,
        &[],
        403,
        REFUSAL,
    );
    for model in ["model-a", "model-b", "model-a"] {
        first
            .dispatch(&hofund_router::Request::new(model), "messages")
            .await
            .expect("chat answers");
    }
    assert_eq!(
        seen.lock().expect("seen").as_slice(),
        [
            "model-a:messages",
            "model-a:chat",
            "model-b:chat",
            "model-a:chat",
        ]
    );

    seen.lock().expect("seen").clear();
    let reloaded = router(&["model-a"], fitness, &seen, &[], 403, REFUSAL);
    reloaded
        .dispatch(&hofund_router::Request::new("model-a"), "messages")
        .await
        .expect("reload shares fitness");
    assert_eq!(seen.lock().expect("seen").as_slice(), ["model-a:chat"]);
}

#[tokio::test]
async fn d250_other_failures_do_not_ask_the_other_api() {
    let seen = Arc::new(Mutex::new(Vec::new()));
    let router = router(
        &["model-a"],
        ApiFitness::default(),
        &seen,
        &[429],
        429,
        "slow down",
    );
    match router
        .dispatch(&hofund_router::Request::new("model-a"), "messages")
        .await
    {
        Ok(_) => panic!("a rate limit stays on this API"),
        Err(error) => {
            assert!(matches!(error, hofund_router::RouterError::Upstream { .. }));
        }
    }
    assert_eq!(seen.lock().expect("seen").as_slice(), ["model-a:messages"]);
}
