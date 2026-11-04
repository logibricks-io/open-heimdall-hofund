//! Spend caps follow the key fingerprint membership already carries.

use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};
use std::time::{SystemTime, UNIX_EPOCH};

use hofund_usage::{
    Ledger, Row, RowPricing, SpendCapPeriod, TokenPrice, UsageDetails, spend_cap_retry_after,
};
use wiremock::matchers::{method, path};
use wiremock::{Mock, MockServer, ResponseTemplate};

use crate::provider_keys::key_id;
use crate::spend_caps::{SpendCapStore, parse_spend_cap};
use crate::{Running, start_for_test};

const CHAT_OK: &str = r#"{"choices":[{"index":0,"message":{"role":"assistant","content":"ok"},"finish_reason":"stop"}]}"#;
const ASK: &str = r#"{"model":"demo","messages":[{"role":"user","content":"hi"}]}"#;

struct Live {
    dir: tempfile::TempDir,
    running: Running,
    token: String,
    seen: Arc<Mutex<Vec<String>>>,
    _server: MockServer,
}

fn config_path(dir: &Path) -> PathBuf {
    dir.join("hofund.toml")
}

fn usage_path(dir: &Path) -> PathBuf {
    dir.join("usage.sqlite")
}

fn cap(dir: &Path, provider: &str, secret: &str, text: &str) {
    let parsed = parse_spend_cap(text).expect("cap");
    SpendCapStore::new(&config_path(dir))
        .set_key_spend_cap(provider, &key_id(secret), parsed)
        .expect("store cap");
}

fn seed_spend(dir: &Path, upstream: &str, secret: &str, usd: f64) {
    let now = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map_or(0, |duration| i64::try_from(duration.as_secs()).unwrap_or(0));
    let row = Row {
        created_unix: now,
        model: "demo".to_owned(),
        upstream: upstream.to_owned(),
        input_tokens: Some(1),
        output_tokens: Some(1),
        latency_ms: 1,
        status: "200".to_owned(),
        provider_key_id: key_id(secret),
        ..Row::default()
    };
    let price = RowPricing {
        priced: true,
        cost_usd: usd,
        price: TokenPrice::default(),
    };
    Ledger::open(&usage_path(dir))
        .expect("ledger")
        .record_with_details(&row, None, UsageDetails::default(), Some(price))
        .expect("seed");
}

async fn listen(text: &str) -> Live {
    let server = MockServer::start().await;
    let seen = Arc::new(Mutex::new(Vec::new()));
    let record = Arc::clone(&seen);
    Mock::given(method("POST"))
        .and(path("/chat/completions"))
        .respond_with(move |request: &wiremock::Request| {
            let auth = request
                .headers
                .get("authorization")
                .and_then(|value| value.to_str().ok())
                .unwrap_or_default()
                .to_owned();
            record.lock().expect("seen").push(auth);
            ResponseTemplate::new(200).set_body_raw(CHAT_OK, "application/json")
        })
        .mount(&server)
        .await;
    let dir = tempfile::tempdir().expect("dir");
    std::fs::write(
        config_path(dir.path()),
        text.replace("{base}", &server.uri()),
    )
    .expect("config");
    let running = start_for_test(&config_path(dir.path()))
        .await
        .expect("start");
    let token = std::fs::read_to_string(dir.path().join("token"))
        .expect("token")
        .trim()
        .to_owned();
    Live {
        dir,
        running,
        token,
        seen,
        _server: server,
    }
}

async fn ask(live: &Live) -> reqwest::Response {
    reqwest::Client::builder()
        .timeout(std::time::Duration::from_secs(5))
        .build()
        .expect("client")
        .post(format!(
            "http://{}/v1/chat/completions",
            live.running.local_addr
        ))
        .header("authorization", format!("Bearer {}", live.token))
        .body(ASK)
        .send()
        .await
        .expect("send")
}

fn bearers(live: &Live) -> Vec<String> {
    live.seen.lock().expect("seen").clone()
}

fn prepare(dir: &Path, text: &str, base: &str) {
    std::fs::write(config_path(dir), text.replace("{base}", base)).expect("config");
}

fn header() -> &'static str {
    "\
listen = \"127.0.0.1:0\"
token_file = \"token\"
usage_file = \"usage.sqlite\"
"
}

fn route() -> &'static str {
    "\
[[routes]]
model = \"demo\"
routing = \"order\"
upstreams = [\"primary\"]
"
}

async fn mounted() -> (MockServer, Arc<Mutex<Vec<String>>>) {
    let server = MockServer::start().await;
    let seen = Arc::new(Mutex::new(Vec::new()));
    let record = Arc::clone(&seen);
    Mock::given(method("POST"))
        .and(path("/chat/completions"))
        .respond_with(move |request: &wiremock::Request| {
            let auth = request
                .headers
                .get("authorization")
                .and_then(|value| value.to_str().ok())
                .unwrap_or_default()
                .to_owned();
            record.lock().expect("seen").push(auth);
            ResponseTemplate::new(200).set_body_raw(CHAT_OK, "application/json")
        })
        .mount(&server)
        .await;
    (server, seen)
}

#[tokio::test]
async fn d61_7_capped_key_sends_the_next_request_to_the_other_key() {
    let alpha = "spend-cap-key-alpha";
    let beta = "spend-cap-key-beta";
    let (server, seen) = mounted().await;
    let dir = tempfile::tempdir().expect("dir");
    prepare(
        dir.path(),
        &format!(
            "{header}\
[[providers]]
id = \"primary\"
protocol = \"openai-chat\"
base_url = \"{{base}}\"
api_key = \"{alpha}\"
[[providers.keys]]
key = \"{beta}\"
{route}",
            header = header(),
            route = route(),
        ),
        &server.uri(),
    );
    cap(dir.path(), "primary", alpha, "1/day");
    seed_spend(dir.path(), "primary", alpha, 5.0);
    let running = start_for_test(&config_path(dir.path()))
        .await
        .expect("start");
    let token = std::fs::read_to_string(dir.path().join("token"))
        .expect("token")
        .trim()
        .to_owned();
    let response = reqwest::Client::builder()
        .timeout(std::time::Duration::from_secs(5))
        .build()
        .expect("client")
        .post(format!("http://{}/v1/chat/completions", running.local_addr))
        .header("authorization", format!("Bearer {token}"))
        .body(ASK)
        .send()
        .await
        .expect("send");
    let status = response.status();
    let body = response.text().await.expect("body");
    let auths = seen.lock().expect("seen").clone();
    running.shutdown().await;
    assert_eq!(status, 200, "{body}");
    assert_eq!(auths, vec![format!("Bearer {beta}")]);
    let rows = Ledger::open(&usage_path(dir.path()))
        .expect("rows")
        .rows()
        .expect("rows");
    assert!(
        rows.iter()
            .any(|row| row.provider_key_id == key_id(beta) && row.upstream == "primary"),
        "{rows:?}"
    );
}

#[tokio::test]
async fn d61_8_single_key_over_its_day_cap_returns_retry_after_midnight() {
    let secret = "spend-cap-key-solo";
    let (server, seen) = mounted().await;
    let dir = tempfile::tempdir().expect("dir");
    prepare(
        dir.path(),
        &format!(
            "{header}\
[[providers]]
id = \"primary\"
protocol = \"openai-chat\"
base_url = \"{{base}}\"
api_key = \"{secret}\"
{route}",
            header = header(),
            route = route(),
        ),
        &server.uri(),
    );
    cap(dir.path(), "primary", secret, "1/day");
    seed_spend(dir.path(), "primary", secret, 5.0);
    let running = start_for_test(&config_path(dir.path()))
        .await
        .expect("start");
    let token = std::fs::read_to_string(dir.path().join("token"))
        .expect("token")
        .trim()
        .to_owned();
    let response = reqwest::Client::builder()
        .timeout(std::time::Duration::from_secs(5))
        .build()
        .expect("client")
        .post(format!("http://{}/v1/chat/completions", running.local_addr))
        .header("authorization", format!("Bearer {token}"))
        .body(ASK)
        .send()
        .await
        .expect("send");
    let status = response.status();
    let retry = response
        .headers()
        .get("retry-after")
        .and_then(|value| value.to_str().ok())
        .and_then(|value| value.parse::<u64>().ok());
    let body = response.text().await.expect("body");
    let auths = seen.lock().expect("seen").clone();
    running.shutdown().await;
    assert_eq!(status, 429, "{body}");
    assert!(body.contains("spend cap reached"), "{body}");
    assert!(auths.is_empty(), "{auths:?}");
    let expected = spend_cap_retry_after(SpendCapPeriod::Day).expect("midnight");
    let retry = retry.expect("retry-after");
    assert!(
        retry.abs_diff(expected) <= 5,
        "retry-after {retry} is not the next local midnight ({expected})"
    );
}

#[tokio::test]
async fn d61_9_capped_key_uses_provider_fallback_before_429() {
    let primary = "spend-cap-key-primary";
    let backup = "spend-cap-key-backup";
    let (server, seen) = mounted().await;
    let dir = tempfile::tempdir().expect("dir");
    prepare(
        dir.path(),
        &format!(
            "{header}\
[[providers]]
id = \"primary\"
protocol = \"openai-chat\"
base_url = \"{{base}}\"
api_key = \"{primary}\"
fallback = [\"backup/demo\"]
[[providers]]
id = \"backup\"
protocol = \"openai-chat\"
base_url = \"{{base}}\"
api_key = \"{backup}\"
{route}",
            header = header(),
            route = route(),
        ),
        &server.uri(),
    );
    cap(dir.path(), "primary", primary, "1/day");
    seed_spend(dir.path(), "primary", primary, 5.0);
    let running = start_for_test(&config_path(dir.path()))
        .await
        .expect("start");
    let token = std::fs::read_to_string(dir.path().join("token"))
        .expect("token")
        .trim()
        .to_owned();
    let response = reqwest::Client::builder()
        .timeout(std::time::Duration::from_secs(5))
        .build()
        .expect("client")
        .post(format!("http://{}/v1/chat/completions", running.local_addr))
        .header("authorization", format!("Bearer {token}"))
        .body(ASK)
        .send()
        .await
        .expect("send");
    let status = response.status();
    let body = response.text().await.expect("body");
    let auths = seen.lock().expect("seen").clone();
    running.shutdown().await;
    assert_eq!(status, 200, "{body}");
    assert_eq!(auths, vec![format!("Bearer {backup}")]);
}

#[tokio::test]
async fn d61_10_keyed_request_records_the_key_id() {
    let secret = "spend-cap-key-recorded";
    let live = listen(&format!(
        "{header}\
[[providers]]
id = \"primary\"
protocol = \"openai-chat\"
base_url = \"{{base}}\"
api_key = \"{secret}\"
{route}",
        header = header(),
        route = route(),
    ))
    .await;
    let response = ask(&live).await;
    let status = response.status();
    let body = response.text().await.expect("body");
    let auths = bearers(&live);
    live.running.shutdown().await;
    assert_eq!(status, 200, "{body}");
    assert_eq!(auths, vec![format!("Bearer {secret}")]);
    let rows = Ledger::open(&usage_path(live.dir.path()))
        .expect("rows")
        .rows()
        .expect("rows");
    assert!(
        rows.iter()
            .any(|row| row.provider_key_id == key_id(secret) && row.upstream == "primary"),
        "{rows:?}"
    );
}
