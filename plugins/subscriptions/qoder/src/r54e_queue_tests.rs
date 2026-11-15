//! Recorded queue wait, budget, and failover short-circuit tests.

use std::sync::Arc;
use std::time::Duration;

use hofund_kit_subscription::{TestHost, advance_time, block_on, block_on_paused};
use hofund_plugin_api::{
    ANOTHER_ROUTING_CANDIDATE, AccountId, Completion, PluginError, PluginId, PluginKind,
    ProbeQuery, ProtocolId, RawTable, Request, SecretString, Subscription, SubscriptionFactory,
    UpstreamProvider,
};
use hofund_plugin_sdk::json::{self, Value};

use crate::creds::{self, Credential, Site};
use crate::{Endpoints, cosy, factory_with_endpoints};

const QUEUED_STATE: &str = r#"{"isQueued":true,"modelKey":"dfmodel","queueCount":3,"queueType":"p3","retryAfterSeconds":30,"serviceAvailable":true,"waitTime":20}"#;

fn endpoints() -> Endpoints {
    Endpoints {
        web_global: "https://qoder.test".to_owned(),
        web_cn: "https://qoder-cn.test".to_owned(),
        openapi_global: "https://openapi.qoder.test".to_owned(),
        openapi_cn: "https://openapi-cn.qoder.test".to_owned(),
        chat_global: "https://chat.qoder.test".to_owned(),
        chat_cn: "https://chat-cn.qoder.test".to_owned(),
    }
}

fn host_tokio(dir: &tempfile::TempDir) -> TestHost {
    TestHost::open_for_with_tokio_monotonic(
        PluginKind::Subscription,
        PluginId::from_static("qoder"),
        dir.path(),
    )
}

fn subscription(host: &TestHost) -> Arc<dyn Subscription> {
    let factory = factory_with_endpoints(endpoints());
    let config = factory.validate(&RawTable::new()).expect("config");
    factory
        .build(&config, host.context())
        .expect("subscription")
}

fn credential() -> Credential {
    Credential {
        uid: "uid-1".to_owned(),
        email: "me@example.test".to_owned(),
        name: "Example".to_owned(),
        token: SecretString::from("job-token".to_owned()),
        refresh_token: SecretString::from("job-refresh".to_owned()),
        expires_at: 4_000_000_000_000,
        machine_id: "machine-id".to_owned(),
        site: Site::Global,
        vpc: String::new(),
        device_chat: false,
        device_token: SecretString::from("device-token".to_owned()),
        device_refresh_token: SecretString::from("device-refresh".to_owned()),
        pat_auth: false,
        pat: SecretString::from(String::new()),
    }
}

async fn seed_async(host: &TestHost) -> AccountId {
    let credential = credential();
    let id = creds::account_id(&credential);
    let account = host
        .context()
        .accounts()
        .account(id.clone(), creds::label(&credential))
        .expect("account");
    host.context()
        .accounts()
        .upsert(account, creds::secret_blob(&credential).to_secret())
        .await
        .expect("seed");
    id
}

fn seed(host: &TestHost) -> AccountId {
    block_on(seed_async(host))
}

fn queued_refusal_body() -> Vec<u8> {
    format!(
        r#"{{"code":"10605","message":{}}}"#,
        json::to_string(QUEUED_STATE).expect("queue json")
    )
    .into_bytes()
}

fn queue_status_body(state: &str) -> Vec<u8> {
    format!(r#"{{"success":true,"data":{state}}}"#).into_bytes()
}

fn chat_success_sse() -> Vec<u8> {
    let chunk = json::json!({
        "choices": [{"index": 0, "delta": {"content": "hello"}, "finish_reason": "stop"}],
        "usage": {"prompt_tokens": 3, "completion_tokens": 1}
    });
    format!(
        "data: {}\n\ndata: {{\"statusCodeValue\":200,\"body\":\"[DONE]\"}}\n\n",
        json::json!({"statusCodeValue": 200, "body": chunk.to_string()})
    )
    .into_bytes()
}

fn request_for(model: &str) -> Request {
    let query = ProbeQuery::new(ProtocolId::OPENAI_CHAT, model).expect("query");
    let mut request = query.minimal_request();
    request.stream = false;
    request
}

async fn execute_advancing(
    upstream: Arc<dyn UpstreamProvider>,
    request: Request,
    step: Duration,
) -> Result<Completion, PluginError> {
    let execute = upstream.execute(request, ProtocolId::OPENAI_CHAT);
    tokio::pin!(execute);
    loop {
        tokio::select! {
            result = &mut execute => return result,
            _ = tokio::time::sleep(Duration::from_millis(5)) => {
                advance_time(step).await;
            }
        }
    }
}

fn decode_chat_body(body: &[u8]) -> Value {
    let wire = std::str::from_utf8(body).expect("cosy utf-8");
    let plain = cosy::decode(wire).expect("cosy decode");
    json::from_str(&plain).expect("chat json")
}

#[test]
fn r54e_queue_poll_then_resend_succeeds() {
    block_on_paused(async {
        let dir = tempfile::tempdir().expect("dir");
        let host = host_tokio(&dir);
        let id = seed_async(&host).await;
        host.http()
            .enqueue(include_bytes!("fixtures/models.json").as_slice());
        host.http()
            .enqueue_response(403, &[], queued_refusal_body());
        host.http().enqueue(queue_status_body(
            r#"{"isQueued":true,"modelKey":"dfmodel","queueCount":3,"queueType":"p3","retryAfterSeconds":12,"serviceAvailable":true}"#,
        ));
        host.http().enqueue(queue_status_body(
            r#"{"isQueued":true,"modelKey":"dfmodel","queueCount":2,"queueType":"p3","retryAfterSeconds":12,"serviceAvailable":true}"#,
        ));
        host.http().enqueue(queue_status_body(
            r#"{"isQueued":false,"modelKey":"dfmodel","queueType":"p3","retryAfterSeconds":1,"serviceAvailable":true}"#,
        ));
        host.http().enqueue(chat_success_sse());

        let upstream = subscription(&host)
            .upstream(&id, host.context())
            .expect("upstream");
        let completion =
            execute_advancing(upstream, request_for("dfmodel"), Duration::from_secs(1))
                .await
                .expect("through queue");
        assert!(matches!(completion, Completion::Unary(_)));

        let calls = host.http().calls();
        let chat_bodies: Vec<_> = calls
            .iter()
            .filter(|call| call.uri().contains("agent_chat_generation"))
            .map(|call| decode_chat_body(call.body()))
            .collect();
        let status_count = calls
            .iter()
            .filter(|call| call.uri().contains("queue/status"))
            .count();
        assert_eq!(chat_bodies.len(), 2, "{calls:?}");
        assert_eq!(status_count, 3, "{calls:?}");
        assert_eq!(
            chat_bodies[0].get("request_set_id"),
            chat_bodies[1].get("request_set_id")
        );
        assert_ne!(
            chat_bodies[0].get("request_id"),
            chat_bodies[1].get("request_id")
        );

        let status_uri = calls
            .iter()
            .find(|call| call.uri().contains("queue/status"))
            .expect("status")
            .uri();
        assert!(status_uri.contains("requestSetId="));
        assert!(status_uri.contains("modelKey=dfmodel"));
        assert!(status_uri.contains("queueType=p3"));
    });
}

#[test]
fn r54e_queue_budget_exceeded_returns_queue_error_without_resend() {
    block_on_paused(async {
        let dir = tempfile::tempdir().expect("dir");
        let host = host_tokio(&dir);
        let id = seed_async(&host).await;
        host.http()
            .enqueue(include_bytes!("fixtures/models.json").as_slice());
        host.http()
            .enqueue_response(403, &[], queued_refusal_body());
        for _ in 0..20 {
            host.http().enqueue(queue_status_body(QUEUED_STATE));
        }

        let upstream = subscription(&host)
            .upstream(&id, host.context())
            .expect("upstream");
        let error = execute_advancing(upstream, request_for("dfmodel"), Duration::from_secs(30))
            .await
            .err()
            .expect("budget ends the wait");
        assert_eq!(error.status(), Some(503));
        assert_eq!(error.retry_after(), Some(Duration::from_secs(30)));
        assert!(
            error.message().contains("queued the request for dfmodel"),
            "{}",
            error.message()
        );

        let calls = host.http().calls();
        let chat_count = calls
            .iter()
            .filter(|call| call.uri().contains("agent_chat_generation"))
            .count();
        let status_count = calls
            .iter()
            .filter(|call| call.uri().contains("queue/status"))
            .count();
        assert_eq!(chat_count, 1, "{calls:?}");
        assert!(status_count >= 1, "{calls:?}");
    });
}

#[test]
fn r54e_queue_short_circuits_when_another_routing_candidate_follows() {
    let dir = tempfile::tempdir().expect("dir");
    let host = host_tokio(&dir);
    let id = seed(&host);
    host.http()
        .enqueue(include_bytes!("fixtures/models.json").as_slice());
    host.http()
        .enqueue_response(403, &[], queued_refusal_body());
    for _ in 0..5 {
        host.http().enqueue(queue_status_body(QUEUED_STATE));
    }

    let upstream = subscription(&host)
        .upstream(&id, host.context())
        .expect("upstream");
    let mut request = request_for("dfmodel");
    request
        .extensions
        .insert(ANOTHER_ROUTING_CANDIDATE, json::json!(true));

    let error = block_on(upstream.execute(request, ProtocolId::OPENAI_CHAT))
        .err()
        .expect("immediate queue refusal");
    assert_eq!(error.status(), Some(503));
    assert_eq!(error.retry_after(), Some(Duration::from_secs(30)));
    assert!(
        error.message().contains("queued the request for dfmodel"),
        "{}",
        error.message()
    );

    let calls = host.http().calls();
    let chat_count = calls
        .iter()
        .filter(|call| call.uri().contains("agent_chat_generation"))
        .count();
    let status_count = calls
        .iter()
        .filter(|call| call.uri().contains("queue/status"))
        .count();
    assert_eq!(chat_count, 1, "{calls:?}");
    assert_eq!(status_count, 0, "{calls:?}");
}
