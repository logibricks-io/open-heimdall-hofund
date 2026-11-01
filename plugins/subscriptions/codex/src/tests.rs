//! Unit tests against recording host backends; no network.

use std::pin::pin;
use std::sync::Arc;
use std::task::Poll;
use std::time::{Duration, UNIX_EPOCH};

use hofund_kit_subscription::{TestHost, block_on, join_all, poll_once, yield_times};
use hofund_plugin_api::{
    AccountId, AccountStatus, Completion, PluginErrorKind, PluginId, PluginKind, ProbeQuery,
    ProtocolId, RawTable, Request, SecretString, SignInChallenge, SignInCtx, SignInProof,
    Subscription, SubscriptionFactory, UpstreamProvider,
};
use hofund_plugin_sdk::oauth::{Pkce, SecretObject, same_secret};

use crate::tokens::{self, Need};
use crate::upstream::{self, CodexUpstream};
use crate::{Endpoints, factory_with_endpoints, sign_in};
use crate::{models, shape};
use hofund_plugin_sdk::json::{self, Value, json};

const SESSION: &str = "0123456789abcdef0123456789abcdef";
const TOKEN_URL: &str = "https://auth.test/oauth/token";
const BASE: &str = "https://codex.test/backend-api/codex";

fn endpoints() -> Endpoints {
    Endpoints {
        authorize: "https://auth.test/oauth/authorize".to_owned(),
        token: TOKEN_URL.to_owned(),
        base: BASE.to_owned(),
        port: None,
    }
}

fn plugin() -> PluginId {
    PluginId::from_static("codex")
}

fn host(dir: &tempfile::TempDir) -> TestHost {
    TestHost::open_for(PluginKind::Subscription, plugin(), dir.path())
}

fn subscription(host: &TestHost) -> Arc<dyn Subscription> {
    let factory = factory_with_endpoints(endpoints());
    let cfg = factory.validate(&RawTable::new()).expect("config");
    factory.build(&cfg, host.context()).expect("build")
}

fn ctx() -> SignInCtx {
    SignInCtx::new(plugin())
        .with_session(SESSION)
        .expect("session")
}

fn b64url(bytes: &[u8]) -> String {
    const ALPHABET: &[u8; 64] = b"ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz0123456789-_";
    let mut out = String::new();
    for chunk in bytes.chunks(3) {
        let n = chunk
            .iter()
            .enumerate()
            .fold(0_u32, |acc, (i, b)| acc | (u32::from(*b) << (16 - 8 * i)));
        let count = chunk.len() + 1;
        for i in 0..count {
            out.push(char::from(ALPHABET[((n >> (18 - 6 * i)) & 63) as usize]));
        }
    }
    out
}

fn jwt(claims: &str) -> String {
    format!("e30.{}.sig", b64url(claims.as_bytes()))
}

fn id_token(email: &str, sub: &str, account: &str, plan: &str) -> String {
    jwt(&format!(
        r#"{{"email":"{email}","sub":"{sub}","https://api.openai.com/auth":{{"chatgpt_account_id":"{account}","chatgpt_plan_type":"{plan}"}}}}"#
    ))
}

/// An access token that expires at `exp` seconds after the epoch. The test
/// clock reads the epoch, so 100 is inside the margin and 10 000 is not.
fn access_token(tag: &str, exp: u64) -> String {
    jwt(&format!(r#"{{"exp":{exp},"tag":"{tag}"}}"#))
}

fn secret(text: &str) -> SecretString {
    SecretString::from(text.to_owned())
}

fn token_reply(access: &str, refresh: Option<&str>, id: Option<&str>) -> Vec<u8> {
    let mut body = format!(r#"{{"access_token":"{access}","token_type":"Bearer""#);
    if let Some(refresh) = refresh {
        body.push_str(&format!(r#","refresh_token":"{refresh}""#));
    }
    if let Some(id) = id {
        body.push_str(&format!(r#","id_token":"{id}""#));
    }
    body.push('}');
    body.into_bytes()
}

fn seed(host: &TestHost, access: &str) -> AccountId {
    let id = AccountId::from_static("a-0000000000000001");
    let blob = SecretObject::new()
        .with_number("v", 1)
        .with_secret(
            "id_token",
            &secret(&id_token("u@example.test", "sub-1", "acct-1", "plus")),
        )
        .with_secret("access_token", &secret(access))
        .with_secret("refresh_token", &secret("refresh-1"))
        .with_text("account_id", "acct-1")
        .with_number("last_refresh", 0);
    block_on(async {
        let accounts = host.context().accounts();
        let account = accounts
            .account(id.clone(), "u@example.test")
            .expect("account");
        accounts
            .upsert(account, blob.to_secret())
            .await
            .expect("upsert");
    });
    seed_prompts(host, &[("gpt-5", CODEX_PROMPT)]);
    id
}

const CODEX_PROMPT: &str = "You are Codex, a coding agent.\nBe brief.";

fn prompts_file(host: &TestHost) -> std::path::PathBuf {
    host.context()
        .dirs()
        .cache_dir()
        .expect("cache dir")
        .join(models::PROMPTS_FILE)
}

fn seed_prompts(host: &TestHost, prompts: &[(&str, &str)]) {
    let map: std::collections::BTreeMap<&str, &str> = prompts.iter().copied().collect();
    std::fs::write(prompts_file(host), json::to_vec(&map).expect("json")).expect("write");
}

fn stored(host: &TestHost, id: &AccountId) -> SecretObject {
    let secret = block_on(host.context().accounts().secret(id))
        .expect("read")
        .expect("stored");
    SecretObject::parse(&secret).expect("blob")
}

fn token_posts(host: &TestHost) -> usize {
    host.http()
        .calls()
        .iter()
        .filter(|call| call.uri() == TOKEN_URL)
        .count()
}

fn query_pairs(url: &str) -> Vec<(String, String)> {
    let query = url
        .split_once('?')
        .map(|(_, query)| query)
        .unwrap_or_default();
    query
        .split('&')
        .filter_map(|pair| pair.split_once('='))
        .map(|(key, value)| (key.to_owned(), value.to_owned()))
        .collect()
}

fn streaming_request() -> Request {
    let mut request = ProbeQuery::new(ProtocolId::OPENAI_RESPONSES, "gpt-5")
        .expect("query")
        .minimal_request();
    request.stream = true;
    request
}

#[test]
fn authorize_url_has_s256_state_and_codex_params() {
    let pkce = Pkce::generate().expect("pkce");
    let url = sign_in::authorize(
        "https://auth.test/oauth/authorize",
        "http://localhost:1455/auth/callback",
        &pkce,
        "state-1",
    )
    .expect("url");
    assert!(
        url.starts_with("https://auth.test/oauth/authorize?"),
        "{url}"
    );
    let pairs = query_pairs(&url);
    let get = |key: &str| {
        pairs
            .iter()
            .find(|(name, _)| name == key)
            .map(|(_, value)| value.clone())
            .unwrap_or_default()
    };
    assert_eq!(get("response_type"), "code");
    assert_eq!(get("client_id"), crate::CLIENT_ID);
    assert_eq!(get("code_challenge"), pkce.challenge);
    assert_eq!(get("code_challenge_method"), "S256");
    assert_eq!(get("state"), "state-1");
    assert_eq!(get("id_token_add_organizations"), "true");
    assert_eq!(get("codex_cli_simplified_flow"), "true");
    assert_eq!(get("originator"), "codex_cli_rs");
    assert!(
        get("redirect_uri").contains("localhost%3A1455%2Fauth%2Fcallback"),
        "{url}"
    );
    assert!(get("scope").contains("offline_access"), "{url}");
    assert!(get("scope").contains("api.connectors.invoke"), "{url}");
    assert_eq!(crate::Endpoints::default().port, Some(1455));
}

#[test]
fn start_binds_loopback_and_opens_the_authorize_page() {
    let dir = tempfile::tempdir().expect("dir");
    let host = host(&dir);
    let sub = subscription(&host);
    let challenge = block_on(sub.start_sign_in(&ctx(), host.context())).expect("start");
    let SignInChallenge::OpenUrl { url, .. } = challenge else {
        panic!("expected an address to open");
    };
    assert!(
        url.contains("redirect_uri=http%3A%2F%2Flocalhost%3A"),
        "{url}"
    );
    block_on(sub.cancel_sign_in(&ctx(), host.context())).expect("cancel");
    let error = block_on(sub.complete_sign_in(
        &ctx(),
        SignInProof::Callback {
            code: secret("code"),
            state: "x".to_owned(),
        },
        host.context(),
    ))
    .expect_err("cancelled sign-in is gone");
    assert_eq!(error.kind(), PluginErrorKind::Config);
}

fn started_state(host: &TestHost, sub: &Arc<dyn Subscription>) -> String {
    let challenge = block_on(sub.start_sign_in(&ctx(), host.context())).expect("start");
    let SignInChallenge::OpenUrl { url, .. } = challenge else {
        panic!("expected an address to open");
    };
    query_pairs(&url)
        .into_iter()
        .find(|(key, _)| key == "state")
        .map(|(_, value)| value)
        .expect("state")
}

fn complete(
    host: &TestHost,
    sub: &Arc<dyn Subscription>,
    state: &str,
) -> Result<AccountId, hofund_plugin_api::PluginError> {
    block_on(sub.complete_sign_in(
        &ctx(),
        SignInProof::Callback {
            code: secret("code-1"),
            state: state.to_owned(),
        },
        host.context(),
    ))
}

#[test]
fn complete_rejects_state_mismatch() {
    let dir = tempfile::tempdir().expect("dir");
    let host = host(&dir);
    let sub = subscription(&host);
    let _ = started_state(&host, &sub);
    let error = complete(&host, &sub, "not-the-state").expect_err("mismatch");
    assert_eq!(error.kind(), PluginErrorKind::Auth);
    assert!(host.http().calls().is_empty(), "no code exchange");
}

#[test]
fn exchange_requires_three_tokens() {
    let idt = id_token("u@example.test", "sub-1", "acct-1", "plus");
    for (reply, missing) in [
        (token_reply("acc", None, Some(&idt)), "no refresh token"),
        (token_reply("acc", Some("ref"), None), "no ID token"),
    ] {
        let dir = tempfile::tempdir().expect("dir");
        let host = host(&dir);
        let sub = subscription(&host);
        let state = started_state(&host, &sub);
        host.http().enqueue(reply);
        let error = complete(&host, &sub, &state).expect_err("missing token");
        assert_eq!(error.kind(), PluginErrorKind::Auth);
        assert!(error.message().contains(missing), "{}", error.message());
        assert!(
            block_on(host.context().accounts().list())
                .expect("list")
                .is_empty()
        );
    }
    let dir = tempfile::tempdir().expect("dir");
    let host = host(&dir);
    let sub = subscription(&host);
    let state = started_state(&host, &sub);
    host.http()
        .enqueue(token_reply("acc", Some("ref"), Some(&idt)));
    let id = complete(&host, &sub, &state).expect("sign-in");
    let call = &host.http().calls()[0];
    assert_eq!(call.uri(), TOKEN_URL);
    let body = String::from_utf8(call.body().to_vec()).expect("utf8");
    assert!(body.contains("grant_type=authorization_code"), "{body}");
    assert!(body.contains("code_verifier="), "{body}");
    let blob = stored(&host, &id);
    assert!(same_secret(
        &blob.secret("access_token").expect("access"),
        &secret("acc")
    ));
    assert!(same_secret(
        &blob.secret("refresh_token").expect("refresh"),
        &secret("ref")
    ));
    assert_eq!(blob.text("account_id"), Some("acct-1"));
    assert_eq!(
        block_on(sub.account_status(&id, host.context())).expect("status"),
        AccountStatus::Ready
    );
}

#[test]
fn same_account_replaces_not_duplicates() {
    let dir = tempfile::tempdir().expect("dir");
    let host = host(&dir);
    let sub = subscription(&host);
    let idt = id_token("u@example.test", "sub-1", "acct-1", "team");
    let mut ids = Vec::new();
    for access in ["acc-1", "acc-2"] {
        let state = started_state(&host, &sub);
        host.http()
            .enqueue(token_reply(access, Some("ref"), Some(&idt)));
        ids.push(complete(&host, &sub, &state).expect("sign-in"));
    }
    assert_eq!(ids[0], ids[1]);
    let accounts = block_on(host.context().accounts().list()).expect("list");
    assert_eq!(accounts.len(), 1);
    assert_eq!(accounts[0].label(), "u@example.test \u{b7} Team");
    let blob = stored(&host, &ids[0]);
    assert!(same_secret(
        &blob.secret("access_token").expect("access"),
        &secret("acc-2")
    ));
}

#[test]
fn labels_name_shared_workspaces_only() {
    assert_eq!(sign_in::label("a@b.test", "plus"), "a@b.test");
    assert_eq!(sign_in::label("a@b.test", "pro"), "a@b.test");
    assert_eq!(
        sign_in::label("a@b.test", "Business"),
        "a@b.test \u{b7} Business"
    );
    assert_eq!(
        sign_in::label("a@b.test", "enterprise"),
        "a@b.test \u{b7} Enterprise"
    );
    assert_eq!(sign_in::label("a@b.test", "edu"), "a@b.test \u{b7} Edu");
    let error = sign_in::identity(&secret(&jwt(r#"{"sub":"x"}"#)))
        .err()
        .expect("no email");
    assert_eq!(error.kind(), PluginErrorKind::Auth);
}

#[test]
fn fresh_uses_a_five_minute_margin() {
    let now = UNIX_EPOCH + Duration::from_secs(1_000);
    assert!(tokens::fresh(&secret(&access_token("t", 1_301)), now));
    assert!(!tokens::fresh(&secret(&access_token("t", 1_300)), now));
    assert!(!tokens::fresh(&secret(&access_token("t", 1_200)), now));
    assert!(
        tokens::fresh(&secret("opaque"), now),
        "no exp counts as fresh"
    );
}

#[test]
fn refresh_posts_once_under_concurrency() {
    let dir = tempfile::tempdir().expect("dir");
    let host = host(&dir);
    let stale = access_token("stale", 100);
    let id = seed(&host, &stale);
    let fresh = access_token("fresh", 10_000);
    host.http()
        .enqueue(token_reply(&fresh, Some("refresh-2"), None));
    let ep = endpoints();
    let results = block_on(join_all(vec![
        Box::pin(tokens::access(host.context(), &ep, &id, Need::IfExpiring)),
        Box::pin(tokens::access(host.context(), &ep, &id, Need::IfExpiring)),
    ]));
    for result in results {
        let access = result.expect("access");
        assert!(same_secret(&access.token, &secret(&fresh)));
        assert_eq!(access.account.as_deref(), Some("acct-1"));
    }
    assert_eq!(token_posts(&host), 1);
    let call = &host.http().calls()[0];
    let body = String::from_utf8(call.body().to_vec()).expect("utf8");
    assert!(body.contains(r#""scope":"openid profile email""#), "{body}");
    assert!(body.contains(r#""grant_type":"refresh_token""#), "{body}");
    assert!(body.contains(r#""refresh_token":"refresh-1""#), "{body}");
    let blob = stored(&host, &id);
    assert!(same_secret(
        &blob.secret("refresh_token").expect("refresh"),
        &secret("refresh-2")
    ));
    assert!(
        blob.secret("id_token").is_some(),
        "the old ID token is kept"
    );
}

#[test]
fn fresh_token_skips_the_refresh() {
    let dir = tempfile::tempdir().expect("dir");
    let host = host(&dir);
    let id = seed(&host, &access_token("fresh", 10_000));
    let access = block_on(tokens::access(
        host.context(),
        &endpoints(),
        &id,
        Need::IfExpiring,
    ))
    .expect("access");
    assert!(same_secret(
        &access.token,
        &secret(&access_token("fresh", 10_000))
    ));
    assert_eq!(token_posts(&host), 0);
    assert!(host.scheduler().running_once().is_empty());
}

#[test]
fn refresh_uses_token_rotated_by_other_process() {
    let dir = tempfile::tempdir().expect("dir");
    let host = host(&dir);
    let rotated = access_token("rotated", 10_000);
    let id = seed(&host, &rotated);
    // This caller read the old token before another process rotated it.
    let access = block_on(tokens::access(
        host.context(),
        &endpoints(),
        &id,
        Need::Replace(secret(&access_token("old", 100))),
    ))
    .expect("access");
    assert!(same_secret(&access.token, &secret(&rotated)));
    assert_eq!(token_posts(&host), 0);
    // The re-check happens under the lease, after the waiter read the old
    // secret: the job sees the rotated token and posts nothing.
    let access = block_on(tokens::rotate_for_test(
        host.context().clone(),
        TOKEN_URL.to_owned(),
        id,
        secret(&access_token("old", 100)),
    ))
    .expect("rotate");
    assert!(same_secret(&access.token, &secret(&rotated)));
    assert_eq!(token_posts(&host), 0);
}

#[test]
fn refresh_commits_when_waiter_dropped() {
    let dir = tempfile::tempdir().expect("dir");
    let host = host(&dir);
    let id = seed(&host, &access_token("stale", 100));
    let fresh = access_token("fresh", 10_000);
    host.http()
        .enqueue(token_reply(&fresh, Some("refresh-2"), None));
    let ep = endpoints();
    block_on(async {
        {
            let mut call = pin!(tokens::access(host.context(), &ep, &id, Need::IfExpiring));
            let mut polls = 0;
            // Poll until the refresh job is handed to the scheduler.
            while host.scheduler().running_once().is_empty() {
                assert!(
                    matches!(poll_once(call.as_mut()), Poll::Pending),
                    "still waiting"
                );
                yield_times(1).await;
                std::thread::sleep(Duration::from_millis(1));
                polls += 1;
                assert!(polls < 5_000, "the refresh never started");
            }
        }
        // The waiter is gone; the job still runs to its commit.
        let mut waits = 0;
        while !host.scheduler().running_once().is_empty() {
            yield_times(1).await;
            std::thread::sleep(Duration::from_millis(1));
            waits += 1;
            assert!(waits < 5_000, "the refresh job never finished");
        }
    });
    assert_eq!(token_posts(&host), 1);
    let blob = stored(&host, &id);
    assert!(same_secret(
        &blob.secret("access_token").expect("access"),
        &secret(&fresh)
    ));
}

#[test]
fn invalid_grant_marks_needs_sign_in() {
    let dir = tempfile::tempdir().expect("dir");
    let host = host(&dir);
    let sub = subscription(&host);
    let id = seed(&host, &access_token("stale", 100));
    host.http().enqueue_response(
        400,
        &[("content-type", "application/json")],
        br#"{"error":"invalid_grant"}"#.to_vec(),
    );
    let error = block_on(sub.refresh(&id, host.context())).expect_err("refused");
    assert_eq!(error.kind(), PluginErrorKind::Auth);
    assert!(stored(&host, &id).flag(tokens::LAPSED));
    let status = block_on(sub.account_status(&id, host.context())).expect("status");
    assert!(
        matches!(status, AccountStatus::NeedsSignIn { .. }),
        "{status:?}"
    );
    // A lapsed account does not post again.
    let error = block_on(sub.refresh(&id, host.context())).expect_err("lapsed");
    assert_eq!(error.kind(), PluginErrorKind::Auth);
    assert_eq!(token_posts(&host), 1);
}

#[test]
fn server_error_keeps_the_sign_in() {
    let dir = tempfile::tempdir().expect("dir");
    let host = host(&dir);
    let id = seed(&host, &access_token("stale", 100));
    host.http().enqueue_response(503, &[], b"busy".to_vec());
    let error = block_on(tokens::access(
        host.context(),
        &endpoints(),
        &id,
        Need::Always,
    ))
    .expect_err("server error");
    assert_eq!(error.status(), Some(503));
    assert!(!stored(&host, &id).flag(tokens::LAPSED));
}

fn upstream_for(host: &TestHost, id: &AccountId) -> CodexUpstream {
    CodexUpstream::new(
        id.clone(),
        host.context().clone(),
        Arc::new(endpoints()),
        Arc::default(),
    )
}

const EXPIRED_401: &[u8] = br#"{"error":{"message":"Provided authentication token is expired. Please try signing in again.","type":"invalid_request_error","code":"token_expired"}}"#;

#[test]
fn token_expired_401_retries_once() {
    let dir = tempfile::tempdir().expect("dir");
    let host = host(&dir);
    let id = seed(&host, &access_token("first", 10_000));
    let json = [("content-type", "application/json")];
    host.http()
        .enqueue_response(401, &json, EXPIRED_401.to_vec());
    host.http()
        .enqueue(token_reply(&access_token("second", 10_000), None, None));
    host.http()
        .enqueue_response(401, &json, EXPIRED_401.to_vec());
    let upstream = upstream_for(&host, &id);
    let error = block_on(upstream.execute(streaming_request(), ProtocolId::OPENAI_RESPONSES))
        .err()
        .expect("second 401 is returned");
    assert_eq!(error.status(), Some(401));
    let uris: Vec<String> = host
        .http()
        .calls()
        .iter()
        .map(|call| call.uri().to_owned())
        .collect();
    let responses = format!("{BASE}/responses");
    assert_eq!(
        uris,
        vec![responses.clone(), TOKEN_URL.to_owned(), responses]
    );
}

#[test]
fn token_expired_code_401_refreshes_and_retries() {
    let dir = tempfile::tempdir().expect("dir");
    let host = host(&dir);
    let id = seed(&host, &access_token("first", 10_000));
    let json = [("content-type", "application/json")];
    host.http().enqueue_response(
        401,
        &json,
        br#"{"error":{"code":"token_expired"}}"#.to_vec(),
    );
    host.http()
        .enqueue(token_reply(&access_token("second", 10_000), None, None));
    host.http()
        .enqueue(b"event: response.completed\ndata: {}\n\n".to_vec());
    let upstream = upstream_for(&host, &id);
    block_on(upstream.execute(streaming_request(), ProtocolId::OPENAI_RESPONSES))
        .expect("code-only expiry is refreshed and retried");
    let uris: Vec<String> = host
        .http()
        .calls()
        .iter()
        .map(|call| call.uri().to_owned())
        .collect();
    let responses = format!("{BASE}/responses");
    assert_eq!(
        uris,
        vec![responses.clone(), TOKEN_URL.to_owned(), responses]
    );
}

#[test]
fn token_expired_code_beside_a_message_401_refreshes_and_retries() {
    let dir = tempfile::tempdir().expect("dir");
    let host = host(&dir);
    let id = seed(&host, &access_token("first", 10_000));
    let json = [("content-type", "application/json")];
    host.http().enqueue_response(
        401,
        &json,
        br#"{"error":{"message":"Unauthorized","code":"token_expired"}}"#.to_vec(),
    );
    host.http()
        .enqueue(token_reply(&access_token("second", 10_000), None, None));
    host.http()
        .enqueue(b"event: response.completed\ndata: {}\n\n".to_vec());
    let upstream = upstream_for(&host, &id);
    block_on(upstream.execute(streaming_request(), ProtocolId::OPENAI_RESPONSES))
        .expect("an expiry code beside other words is refreshed and retried");
    assert_eq!(host.http().calls().len(), 3);
}

#[test]
fn unrelated_error_code_401_is_not_retried() {
    let dir = tempfile::tempdir().expect("dir");
    let host = host(&dir);
    let id = seed(&host, &access_token("first", 10_000));
    host.http().enqueue_response(
        401,
        &[("content-type", "application/json")],
        br#"{"error":{"code":"permission_denied"}}"#.to_vec(),
    );
    let upstream = upstream_for(&host, &id);
    let error = block_on(upstream.execute(streaming_request(), ProtocolId::OPENAI_RESPONSES))
        .err()
        .expect("401");
    assert_eq!(error.status(), Some(401));
    assert_eq!(host.http().calls().len(), 1);
}

#[test]
fn other_401_is_not_retried() {
    let dir = tempfile::tempdir().expect("dir");
    let host = host(&dir);
    let id = seed(&host, &access_token("first", 10_000));
    host.http().enqueue_response(
        401,
        &[("content-type", "application/json")],
        br#"{"error":{"message":"refresh_token_expired","code":"refresh_token_expired"}}"#.to_vec(),
    );
    let upstream = upstream_for(&host, &id);
    let error = block_on(upstream.execute(streaming_request(), ProtocolId::OPENAI_RESPONSES))
        .err()
        .expect("401");
    assert_eq!(error.status(), Some(401));
    assert_eq!(host.http().calls().len(), 1);
}

#[test]
fn expired_words_match_like_codex() {
    assert!(upstream::token_expired("code: token_expired"));
    assert!(upstream::token_expired(
        "Provided authentication token is expired."
    ));
    assert!(upstream::token_expired(
        "Your authentication token has expired"
    ));
    assert!(upstream::token_expired("Please try refreshing it"));
    assert!(upstream::token_expired("TOKEN_EXPIRED"));
    assert!(!upstream::token_expired("refresh_token_expired"));
    assert!(!upstream::token_expired("token_expired_soon"));
    assert!(!upstream::token_expired("unauthorized"));
}

#[test]
fn headers_carry_account_id_and_originator() {
    let headers = upstream::headers(Some("acct-1"));
    let get = |name: &str| {
        headers
            .iter()
            .find(|(key, _)| key == name)
            .map(|(_, value)| value.as_str())
    };
    assert_eq!(get("chatgpt-account-id"), Some("acct-1"));
    assert_eq!(get("originator"), Some("codex_cli_rs"));
    assert_eq!(get("OpenAI-Beta"), Some("responses=experimental"));
    assert_eq!(get("version"), Some(crate::CODEX_VERSION));
    assert_eq!(get("Accept"), Some("text/event-stream"));
    assert!(get("User-Agent").is_some_and(|agent| agent.starts_with("codex_cli_rs/0.159.0 (")));
    assert!(
        upstream::user_agent("linux", "x86_64")
            .starts_with("codex_cli_rs/0.159.0 (Linux; x86_64) ")
    );
    assert!(
        upstream::headers(Some(""))
            .iter()
            .all(|(key, _)| key != "chatgpt-account-id")
    );
    assert!(
        upstream::headers(None)
            .iter()
            .all(|(key, _)| key != "chatgpt-account-id")
    );
    assert!(
        upstream::user_agent("macos", "aarch64")
            .starts_with("codex_cli_rs/0.159.0 (Mac OS; arm64) ")
    );

    let dir = tempfile::tempdir().expect("dir");
    let host = host(&dir);
    let id = seed(&host, &access_token("first", 10_000));
    host.http()
        .enqueue(b"event: response.completed\ndata: {}\n\n".to_vec());
    let upstream = upstream_for(&host, &id);
    block_on(upstream.execute(streaming_request(), ProtocolId::OPENAI_RESPONSES)).expect("execute");
    let call = &host.http().calls()[0];
    assert_eq!(call.uri(), format!("{BASE}/responses"));
    for name in [
        "authorization",
        "chatgpt-account-id",
        "originator",
        "openai-beta",
        "version",
    ] {
        assert!(
            call.sensitive_headers().iter().any(|header| header == name),
            "{name}: {:?}",
            call.sensitive_headers()
        );
    }
    let body = String::from_utf8(call.body().to_vec()).expect("utf8");
    assert!(body.contains(r#""stream":true"#), "{body}");
    assert!(body.contains(r#""store":false"#), "{body}");
}

#[test]
fn shaping_drops_sampling() {
    let mut request = streaming_request();
    request.params.temperature = Some(0.5);
    request.params.top_p = Some(0.9);
    request.params.max_tokens = Some(10);
    for field in [
        "temperature",
        "max_output_tokens",
        "previous_response_id",
        "user",
        "safety_identifier",
        "prompt_cache_options",
        "store",
    ] {
        request.extensions.insert(field, true.into());
        request.params.extensions.insert(field, true.into());
    }
    upstream::shape(&mut request);
    assert_eq!(request.params.temperature, None);
    assert_eq!(request.params.top_p, None);
    assert_eq!(request.params.max_tokens, None);
    for field in [
        "temperature",
        "max_output_tokens",
        "previous_response_id",
        "user",
        "safety_identifier",
        "prompt_cache_options",
        "store",
    ] {
        assert!(request.extensions.get(field).is_none(), "{field}");
        assert!(request.params.extensions.get(field).is_none(), "{field}");
    }
}

#[test]
fn no_token_sentinel_in_debug_or_errors() {
    const SENTINEL: &str = "SENTINEL-6d1f";
    let dir = tempfile::tempdir().expect("dir");
    let host = host(&dir);
    let sub = subscription(&host);
    let state = started_state(&host, &sub);
    host.http().enqueue_response(
        400,
        &[],
        format!(r#"{{"error":"invalid_grant","error_description":"{SENTINEL}","access_token":"{SENTINEL}"}}"#)
            .into_bytes(),
    );
    let error = complete(&host, &sub, &state).expect_err("refused");
    assert!(!format!("{error} {error:?}").contains(SENTINEL));

    let id = seed(&host, &format!("{SENTINEL}.e30.x"));
    host.http().enqueue_response(
        500,
        &[],
        format!("upstream echoed {SENTINEL}?").into_bytes(),
    );
    let error = block_on(
        upstream_for(&host, &id).execute(streaming_request(), ProtocolId::OPENAI_RESPONSES),
    )
    .err()
    .expect("500");
    let blob = stored(&host, &id);
    let shown = format!("{error:?} {blob:?}");
    assert!(!shown.contains(&format!("{SENTINEL}.e30.x")), "{shown}");
    let status = block_on(sub.account_status(&id, host.context())).expect("status");
    assert!(!format!("{status:?}").contains(SENTINEL));
}

fn shaped(body: Value, codex: Option<&str>) -> (Value, Vec<(String, String)>) {
    let Value::Object(mut map) = body else {
        panic!("object")
    };
    let headers = shape::body(&mut map, codex);
    (Value::Object(map), headers)
}

#[test]
fn body_drops_sampling_and_forces_store_false() {
    let mut body = json!({
        "model": "gpt-5",
        "input": "hi",
        "store": true,
        "stream": false,
        "tool_choice": "required",
        "service_tier": "flex",
    });
    for key in shape::DELETED_KEYS {
        body[key] = json!(1);
    }
    let (out, _) = shaped(body, None);
    for key in shape::DELETED_KEYS {
        assert!(out.get(key).is_none(), "{key}");
    }
    assert!(out.get("service_tier").is_none());
    assert_eq!(out["store"], json!(false));
    assert_eq!(out["stream"], json!(true));
    assert_eq!(out["tool_choice"], json!("auto"));
    assert_eq!(out["parallel_tool_calls"], json!(true));
    assert_eq!(out["text"], json!({"verbosity": "medium"}));
    assert_eq!(out["include"], json!(["reasoning.encrypted_content"]));
    assert_eq!(
        out["input"],
        json!([{"type": "message", "role": "user", "content": [{"type": "input_text", "text": "hi"}]}])
    );
    for tier in ["priority", "ultrafast"] {
        let (out, _) = shaped(
            json!({"service_tier": tier, "parallel_tool_calls": false, "text": {"verbosity": "low"}}),
            None,
        );
        assert_eq!(out["service_tier"], json!(tier));
        assert_eq!(out["parallel_tool_calls"], json!(false));
        assert_eq!(out["text"], json!({"verbosity": "low"}));
        assert_eq!(out["input"], json!([]));
        assert!(out.get("prompt_cache_key").is_none());
    }
}

#[test]
fn reasoning_effort_and_include_follow_codex() {
    let (out, _) = shaped(
        json!({"reasoning": {"effort": "ultra"}, "include": ["x", "reasoning.encrypted_content"]}),
        None,
    );
    assert_eq!(
        out["reasoning"],
        json!({"effort": "max", "summary": "auto"})
    );
    assert_eq!(out["include"], json!(["x", "reasoning.encrypted_content"]));
    let (out, _) = shaped(
        json!({"reasoning": {"effort": "none"}, "include": ["x"]}),
        None,
    );
    assert_eq!(out["reasoning"], json!({"effort": "none"}));
    assert!(out.get("include").is_none());
    let (out, _) = shaped(
        json!({"reasoning": {"effort": "high", "summary": "detailed"}}),
        None,
    );
    assert_eq!(
        out["reasoning"],
        json!({"effort": "high", "summary": "detailed"})
    );
}

#[test]
fn system_role_becomes_developer() {
    let (out, _) = shaped(
        json!({"input": [
            {"role": "system", "content": "rules"},
            {"type": "message", "role": "system", "content": "more", "id": "msg_1", "status": "completed"},
            {"type": "reasoning", "id": "rs_1", "status": "done", "summary": []},
            {"type": "function_call", "call_id": "c", "name": "f", "arguments": "{}", "id": "fc_1", "status": "completed"},
            {"type": "item_reference", "id": "msg_0"},
            "loose",
        ]}),
        None,
    );
    assert_eq!(
        out["input"],
        json!([
            {"role": "developer", "content": "rules"},
            {"type": "message", "role": "developer", "content": "more"},
            {"type": "reasoning", "summary": []},
            {"type": "function_call", "call_id": "c", "name": "f", "arguments": "{}", "status": "completed"},
            "loose",
        ])
    );
}

#[test]
fn instructions_ride_first_unless_they_are_codex() {
    let input = json!([{"role": "user", "content": "hi"}]);
    let (out, _) = shaped(
        json!({"instructions": "Be a pirate.", "input": input.clone()}),
        Some(CODEX_PROMPT),
    );
    assert_eq!(out["instructions"], json!(CODEX_PROMPT));
    assert_eq!(
        out["input"][0],
        json!({"type": "message", "role": "developer", "content": [{"type": "input_text", "text": "Be a pirate."}]})
    );
    let own = "  You are Codex, a coding agent.\nAnd more.";
    let (out, _) = shaped(
        json!({"instructions": own, "input": input.clone()}),
        Some(CODEX_PROMPT),
    );
    assert_eq!(out["instructions"], json!(own));
    assert_eq!(out["input"].as_array().map(Vec::len), Some(1));
    let (out, _) = shaped(
        json!({"instructions": " \n", "input": input.clone()}),
        Some(CODEX_PROMPT),
    );
    assert_eq!(out["instructions"], json!(CODEX_PROMPT));
    assert_eq!(out["input"].as_array().map(Vec::len), Some(1));
    let (out, _) = shaped(
        json!({"instructions": "Be a pirate.", "input": input}),
        None,
    );
    assert_eq!(out["instructions"], json!("Be a pirate."));
    assert_eq!(out["input"].as_array().map(Vec::len), Some(1));
}

#[test]
fn long_call_id_hashed_to_64() {
    let fits = "c".repeat(64);
    assert_eq!(shape::bound_call_id(&fits), fits);
    let over = "c".repeat(65);
    assert_eq!(
        shape::bound_call_id(&over),
        hofund_plugin_sdk::sigv4::payload_hash(over.as_bytes())
    );
    let long = format!(" {} ", "d".repeat(70));
    let bound = shape::bound_call_id(&long);
    assert_eq!(bound.len(), 64);
    assert!(bound.bytes().all(|byte| byte.is_ascii_hexdigit()));
    assert_eq!(bound, shape::bound_call_id(long.trim()));
    assert_ne!(bound, shape::bound_call_id(&"d".repeat(71)));
    let (out, _) = shaped(
        json!({"input": [
            {"role": "user", "content": "hi"},
            {"type": "function_call", "call_id": long, "name": "f", "arguments": "{}"},
            {"type": "function_call_output", "call_id": long, "output": "ok"},
            {"type": "tool_search_call", "call_id": long},
            {"type": "message", "role": "user", "call_id": long, "content": "x"},
        ]}),
        None,
    );
    assert_eq!(out["input"][1]["call_id"], json!(bound));
    assert_eq!(
        out["input"][2],
        json!({"type": "function_call_output", "call_id": bound, "output": "ok"})
    );
    assert_eq!(out["input"][3]["call_id"], json!(bound));
    assert_eq!(out["input"][4]["call_id"], json!(long));
}

#[test]
fn orphan_output_becomes_note() {
    let (out, _) = shaped(
        json!({"input": [
            {"type": "local_shell_call", "call_id": "sh", "action": {}},
            {"type": "function_call_output", "call_id": "sh", "output": "paired"},
            {"type": "custom_tool_call_output", "call_id": "gone", "name": "grep", "output": {"n": 1}, "id": "x"},
            {"type": "local_shell_call_output", "call_id": " ", "output": "z"},
            {"type": "function_call_output", "output": "notice", "id": "keep"},
            {"type": "custom_tool_call_output", "call_id": null, "output": "notice"},
        ]}),
        None,
    );
    let input = out["input"].as_array().expect("input");
    assert_eq!(
        input[1],
        json!({"type": "function_call_output", "call_id": "sh", "output": "paired"})
    );
    assert_eq!(
        input[2],
        json!({"type": "message", "role": "assistant", "content": "[Previous grep result; call_id=gone]: {\"n\":1}"})
    );
    assert_eq!(
        input[3],
        json!({"type": "message", "role": "assistant", "content": "[Previous tool result; call_id=unknown]: z"})
    );
    assert_eq!(
        input[4],
        json!({"type": "function_call_output", "output": "notice", "id": "keep"})
    );
    assert_eq!(
        input[5],
        json!({"type": "custom_tool_call_output", "call_id": null, "output": "notice"})
    );

    let long = format!("{}é", "a".repeat(15_999));
    let note = shape::orphan_note(json!({"output": long}).as_object().expect("object"), "c");
    let content = note["content"].as_str().expect("content");
    let text = content
        .strip_prefix("[Previous tool result; call_id=c]: ")
        .expect("prefix");
    assert_eq!(text, format!("{}\n...[truncated]", "a".repeat(15_999)));
    let exact = "b".repeat(16_000);
    let note = shape::orphan_note(json!({"output": exact}).as_object().expect("object"), "c");
    assert!(
        note["content"]
            .as_str()
            .is_some_and(|content| content.ends_with(&exact))
    );
    let over = format!("{exact}b");
    let note = shape::orphan_note(json!({"output": over}).as_object().expect("object"), "c");
    assert!(
        note["content"]
            .as_str()
            .is_some_and(|content| content.ends_with(&format!("{exact}\n...[truncated]")))
    );
    let note = shape::orphan_note(json!({"name": ""}).as_object().expect("object"), "c");
    assert_eq!(
        note["content"],
        json!("[Previous tool result; call_id=c]: null")
    );
}

#[test]
fn cache_key_sets_session_headers() {
    let first = json!({"instructions": "sys", "input": [
        {"role": "developer", "content": "d"},
        {"content": "hi", "role": "user"},
    ]});
    let later = json!({"instructions": "sys", "input": [
        {"content": "d", "role": "developer"},
        {"role": "user", "content": "hi"},
        {"role": "assistant", "content": "hello"},
        {"role": "user", "content": "more"},
    ]});
    let (one, headers) = shaped(first, None);
    let (two, _) = shaped(later, None);
    let key = one["prompt_cache_key"].as_str().expect("key").to_owned();
    assert_eq!(two["prompt_cache_key"], json!(key));
    let parts: Vec<usize> = key.split('-').map(str::len).collect();
    assert_eq!(parts, vec![8, 4, 4, 4, 12]);
    assert_eq!(
        headers,
        vec![
            ("session-id".to_owned(), key.clone()),
            ("thread-id".to_owned(), key.clone()),
        ]
    );
    let (other, _) = shaped(
        json!({"instructions": "other", "input": [{"role": "user", "content": "hi"}]}),
        None,
    );
    assert_ne!(other["prompt_cache_key"], json!(key));
    let (kept, headers) = shaped(
        json!({"prompt_cache_key": "mine", "input": [{"role": "user", "content": "hi"}]}),
        None,
    );
    assert_eq!(kept["prompt_cache_key"], json!("mine"));
    assert_eq!(headers[0], ("session-id".to_owned(), "mine".to_owned()));
    let (none, headers) = shaped(
        json!({"input": [{"role": "assistant", "content": "hi"}]}),
        None,
    );
    assert!(none.get("prompt_cache_key").is_none());
    assert!(headers.is_empty());
    assert_eq!(
        shape::conversation_key(
            "",
            &[json!({"role": "user", "b": 1, "a": [{"y": 1, "x": 2}]})]
        ),
        shape::conversation_key(
            "",
            &[json!({"a": [{"x": 2, "y": 1}], "b": 1, "role": "user"})]
        ),
    );
}

#[test]
fn replayed_search_declares_the_cached_tool() {
    let cached = json!({"type": "web_search", "external_web_access": false});
    let searched =
        json!([{"role": "user", "content": "q"}, {"type": "web_search_call", "id": "ws"}]);
    let (out, _) = shaped(json!({"input": searched.clone()}), None);
    assert_eq!(out["tools"], json!([cached.clone()]));
    assert_eq!(out["tool_choice"], json!("none"));
    let function = json!({"type": "function", "name": "f"});
    let (out, _) = shaped(
        json!({"input": searched.clone(), "tools": [function.clone()]}),
        None,
    );
    assert_eq!(out["tools"], json!([function.clone(), cached.clone()]));
    assert_eq!(out["tool_choice"], json!("auto"));
    for declared in [
        json!({"type": "web_search_preview"}),
        json!({"type": "function", "name": "web.run"}),
        json!({"type": "namespace", "name": "web", "tools": [{"type": "function", "name": "run"}]}),
    ] {
        let (out, _) = shaped(
            json!({"input": searched.clone(), "tools": [declared.clone()]}),
            None,
        );
        assert_eq!(out["tools"], json!([declared]));
    }
    assert!(!shape::declares_search(&[
        json!({"type": "namespace", "name": "web", "tools": [{"type": "function", "name": "open"}]})
    ]));
    let (out, _) = shaped(json!({"input": [{"role": "user", "content": "q"}]}), None);
    assert!(out.get("tools").is_none());
    let (out, _) = shaped(
        json!({"input": searched.clone(), "reasoning": {"context": "all_turns"}}),
        None,
    );
    assert!(out.get("tools").is_none());
    assert_eq!(
        out["input"][0],
        json!({"type": "additional_tools", "role": "developer", "tools": [cached.clone()]})
    );
    let mut items = searched.as_array().expect("items").clone();
    items.push(json!({"type": "additional_tools", "tools": [function.clone()]}));
    let (out, _) = shaped(json!({"input": items}), None);
    assert_eq!(out["input"][2]["tools"], json!([function, cached]));
    assert_eq!(out["tool_choice"], json!("auto"));
    assert!(out.get("tools").is_none());
    let (out, _) = shaped(json!({"input": searched, "tools": "odd"}), None);
    assert_eq!(out["tools"], json!("odd"));
}

#[test]
fn shaped_request_goes_out_with_session_headers() {
    let dir = tempfile::tempdir().expect("dir");
    let host = host(&dir);
    let id = seed(&host, &access_token("first", 10_000));
    host.http()
        .enqueue(b"event: response.completed\ndata: {}\n\n".to_vec());
    block_on(upstream_for(&host, &id).execute(streaming_request(), ProtocolId::OPENAI_RESPONSES))
        .expect("execute");
    let call = &host.http().calls()[0];
    let body: Value = json::from_slice(call.body()).expect("json");
    assert_eq!(body["instructions"], json!(CODEX_PROMPT));
    assert!(
        body["prompt_cache_key"]
            .as_str()
            .is_some_and(|key| key.len() == 36)
    );
    assert_eq!(body["store"], json!(false));
    for name in ["session-id", "thread-id"] {
        assert!(
            call.sensitive_headers().iter().any(|header| header == name),
            "{name}"
        );
    }
}

const MODELS: &[u8] = br#"{"models":[
 {"slug":"gpt-late","display_name":"Late","priority":5,"context_window":200000,"input_modalities":["text"],"base_instructions":"late prompt"},
 {"slug":"gpt-5","display_name":"GPT-5","priority":1,"context_window":272000,"input_modalities":["text","image"],
  "supported_reasoning_levels":[{"effort":"low"},{"effort":"high"}],"service_tiers":[{"id":"priority"}],
  "base_instructions":"base","model_messages":{"instructions_template":"template prompt"}},
 {"slug":"gpt-hidden","visibility":"hide","base_instructions":"hidden prompt"},
 {"slug":""}
]}"#;

#[test]
fn missing_prompt_asks_the_model_list_once() {
    let dir = tempfile::tempdir().expect("dir");
    let host = host(&dir);
    let id = seed(&host, &access_token("first", 10_000));
    seed_prompts(&host, &[("other", "other prompt")]);
    host.http().enqueue(MODELS.to_vec());
    host.http()
        .enqueue(b"event: response.completed\ndata: {}\n\n".to_vec());
    let upstream = upstream_for(&host, &id);
    block_on(upstream.execute(streaming_request(), ProtocolId::OPENAI_RESPONSES)).expect("execute");
    let calls = host.http().calls();
    assert_eq!(
        calls[0].uri(),
        format!("{BASE}/models?client_version={}", crate::CODEX_VERSION)
    );
    assert!(
        calls[0]
            .sensitive_headers()
            .iter()
            .any(|name| name == "authorization")
    );
    let body: Value = json::from_slice(calls[1].body()).expect("json");
    assert_eq!(body["instructions"], json!("template prompt"));
    let kept: Value =
        json::from_slice(&std::fs::read(prompts_file(&host)).expect("file")).expect("json");
    assert_eq!(
        kept,
        json!({"gpt-5": "template prompt", "gpt-late": "late prompt", "gpt-hidden": "hidden prompt", "other": "other prompt"})
    );

    let mut request = streaming_request();
    request.model = "gpt-unknown".to_owned();
    host.http()
        .enqueue(b"event: response.completed\ndata: {}\n\n".to_vec());
    block_on(upstream.execute(request.clone(), ProtocolId::OPENAI_RESPONSES)).expect("execute");
    host.http()
        .enqueue(b"event: response.completed\ndata: {}\n\n".to_vec());
    block_on(upstream.execute(request, ProtocolId::OPENAI_RESPONSES)).expect("execute");
    let uris: Vec<String> = host
        .http()
        .calls()
        .iter()
        .map(|call| call.uri().to_owned())
        .collect();
    assert_eq!(
        uris.iter().filter(|uri| uri.contains("/models")).count(),
        1,
        "{uris:?}"
    );
    assert_eq!(uris.len(), 4);
}

#[test]
fn list_models_reads_the_account_list() {
    let dir = tempfile::tempdir().expect("dir");
    let host = host(&dir);
    let id = seed(&host, &access_token("first", 10_000));
    std::fs::remove_file(prompts_file(&host)).expect("remove");
    let sub = subscription(&host);
    host.http().enqueue(MODELS.to_vec());
    let list = block_on(sub.list_models(&id, host.context())).expect("list");
    let ids: Vec<&str> = list.models().iter().map(|model| model.id()).collect();
    assert_eq!(ids, vec!["gpt-5", "gpt-late"]);
    assert_eq!(list.dropped(), ["gpt-hidden".to_owned()]);
    let first = &list.models()[0];
    assert_eq!(first.display_name(), Some("GPT-5"));
    assert_eq!(first.context(), 272_000);
    assert_eq!(
        first.input_modalities(),
        ["text".to_owned(), "image".to_owned()]
    );
    assert_eq!(first.efforts(), ["low".to_owned(), "high".to_owned()]);
    assert_eq!(first.service_tiers(), ["priority".to_owned()]);
    assert_eq!(list.models()[1].input_modalities(), ["text".to_owned()]);
    let kept: Value =
        json::from_slice(&std::fs::read(prompts_file(&host)).expect("file")).expect("json");
    assert_eq!(kept["gpt-5"], json!("template prompt"));

    host.http().enqueue_response(403, &[], b"{}".to_vec());
    let error = block_on(sub.list_models(&id, host.context())).expect_err("403");
    assert_eq!(error.kind(), PluginErrorKind::Auth);
    host.http().enqueue_response(502, &[], b"{}".to_vec());
    let error = block_on(sub.list_models(&id, host.context())).expect_err("502");
    assert_eq!(error.status(), Some(502));
    host.http().enqueue(br#"{"models":[]}"#.to_vec());
    let error = block_on(sub.list_models(&id, host.context())).expect_err("empty");
    assert_eq!(error.kind(), PluginErrorKind::Decode);
    host.http().enqueue(b"nope".to_vec());
    let error = block_on(sub.list_models(&id, host.context())).expect_err("not json");
    assert_eq!(error.kind(), PluginErrorKind::Decode);
}

#[test]
fn a_missing_prompt_is_asked_for_once_per_ten_minutes_per_account() {
    let prompts = models::Prompts::default();
    let one = AccountId::from_static("a-0000000000000001");
    let two = AccountId::from_static("a-0000000000000002");
    let at = Duration::from_secs(1_000);
    assert!(prompts.may_ask(&one, at));
    assert!(!prompts.may_ask(&one, at));
    assert!(prompts.may_ask(&two, at));
    assert!(!prompts.may_ask(&one, at + models::ASK_AGAIN - Duration::from_secs(1)));
    assert!(prompts.may_ask(&one, at + models::ASK_AGAIN));
    assert!(!prompts.may_ask(&one, at + models::ASK_AGAIN));
    assert_eq!(models::ASK_AGAIN, Duration::from_secs(600));
}

const REPLY_STREAM: &str = concat!(
    "event: response.created\n",
    r#"data: {"type":"response.created","response":{"id":"resp_1","model":"gpt-5","status":"in_progress","output":[]}}"#,
    "\n\n",
    "event: codex.rate_limits\n",
    r#"data: {"type":"codex.rate_limits","rate_limits":{"primary":{"used_percent":99.5,"window_minutes":300,"reset_after_seconds":600}},"credits":{"has_credits":false,"unlimited":false}}"#,
    "\n\n",
    "event: response.output_text.delta\n",
    r#"data: {"type":"response.output_text.delta","item_id":"msg_1","output_index":0,"content_index":0,"delta":"Hi there"}"#,
    "\n\n",
    "event: response.completed\n",
    r#"data: {"type":"response.completed","response":{"id":"resp_1","model":"gpt-5","status":"completed","output":[{"type":"message","id":"msg_1","role":"assistant","content":[{"type":"output_text","text":"Hi there"}]}],"usage":{"input_tokens":3,"output_tokens":2,"total_tokens":5}}}"#,
    "\n\n",
);

fn limit_headers() -> Vec<(&'static str, &'static str)> {
    vec![
        ("content-type", "text/event-stream"),
        ("x-codex-primary-used-percent", "12.345"),
        ("x-codex-primary-window-minutes", "300"),
        ("x-codex-primary-reset-after-seconds", "120"),
        ("x-codex-secondary-used-percent", "100"),
        ("x-codex-secondary-window-minutes", "10080"),
        ("x-codex-secondary-reset-at", "3600"),
        ("x-codex-credits-has-credits", "True"),
        ("x-codex-credits-balance", "5"),
    ]
}

#[test]
fn header_limits_observe_basis_points() {
    let dir = tempfile::tempdir().expect("dir");
    let host = host(&dir);
    let id = seed(&host, &access_token("first", 10_000));
    host.http().enqueue_response(
        200,
        &limit_headers(),
        b"event: response.completed\ndata: {}\n\n".to_vec(),
    );
    block_on(upstream_for(&host, &id).execute(streaming_request(), ProtocolId::OPENAI_RESPONSES))
        .expect("execute");
    let seen = host.observe().observations();
    assert_eq!(seen.len(), 2, "{seen:?}");
    assert_eq!(seen[0].limit(), Some(10_000));
    assert_eq!(seen[0].remaining(), Some(10_000 - 1_235));
    assert_eq!(seen[0].window_reset(), Some(Duration::from_secs(120)));
    assert!(seen[0].spending_credits());
    assert_eq!(seen[1].remaining(), Some(0));
    assert_eq!(seen[1].window_reset(), Some(Duration::from_secs(3600)));

    // A window that does not say how long it runs is left out; a spent
    // balance is no credits.
    let mut headers =
        crate::limits::header_limits(&hofund_plugin_sdk::HeaderMap::new(), UNIX_EPOCH);
    assert!(headers.is_empty());
    let mut map = hofund_plugin_sdk::HeaderMap::new();
    map.insert("x-codex-primary-used-percent", "50".parse().expect("value"));
    map.insert(
        "x-codex-primary-window-minutes",
        "0".parse().expect("value"),
    );
    map.insert(
        "x-codex-secondary-used-percent",
        "-3".parse().expect("value"),
    );
    map.insert(
        "x-codex-secondary-window-minutes",
        "60".parse().expect("value"),
    );
    map.insert(
        "x-codex-credits-has-credits",
        "true".parse().expect("value"),
    );
    map.insert("x-codex-credits-balance", "0".parse().expect("value"));
    headers = crate::limits::header_limits(&map, UNIX_EPOCH);
    assert_eq!(headers.len(), 1);
    assert_eq!(headers[0].used(), 0);
    assert_eq!(headers[0].resets_in, None);
    assert!(!crate::limits::header_credits(&map));
}

#[test]
fn basis_points_round_and_clamp() {
    use crate::limits::basis_points;
    assert_eq!(basis_points(12.344), 1_234);
    assert_eq!(basis_points(12.345_1), 1_235);
    assert_eq!(basis_points(100.0), 10_000);
    assert_eq!(basis_points(250.0), 10_000);
    assert_eq!(basis_points(-1.0), 0);
    assert_eq!(basis_points(f64::NAN), 0);
    assert_eq!(basis_points(0.004), 0);
    assert_eq!(basis_points(0.006), 1);
    assert!(crate::limits::goes_on_credits(false, true, Some("0")));
    assert!(crate::limits::goes_on_credits(true, false, None));
    assert!(crate::limits::goes_on_credits(true, false, Some("lots")));
    assert!(crate::limits::goes_on_credits(true, false, Some(" 0.5 ")));
    assert!(!crate::limits::goes_on_credits(true, false, Some("-1")));
    assert!(!crate::limits::goes_on_credits(false, false, Some("9")));
}

#[test]
fn event_limits_ignore_metered_other() {
    use crate::limits::event_limits;
    let event = |metered: &str| {
        json!({
            "type": "codex.rate_limits",
            "metered_limit_name": metered,
            "rate_limits": {
                "primary": {"used_percent": 40.0, "window_minutes": 300, "reset_at": 100},
                "secondary": {"used_percent": 5.0, "window_minutes": 0},
            },
        })
    };
    let now = UNIX_EPOCH + Duration::from_secs(40);
    let own = event_limits(&event("Codex"), now).expect("own");
    assert_eq!(own.limits.len(), 1);
    assert_eq!(own.limits[0].used(), 4_000);
    assert_eq!(own.limits[0].resets_in, Some(Duration::from_secs(60)));
    assert_eq!(own.credits, None);
    assert!(event_limits(&event(""), now).is_some());
    assert!(event_limits(&event("codex-other"), now).is_none());
    assert!(event_limits(&event("gpt-5-codex-mini"), now).is_none());
    // A reset already passed is no reset.
    let late = event_limits(&event("codex"), UNIX_EPOCH + Duration::from_secs(100)).expect("own");
    assert_eq!(late.limits[0].resets_in, None);
}

#[test]
fn rate_limits_event_observed_and_unary_folded() {
    let dir = tempfile::tempdir().expect("dir");
    let host = host(&dir);
    let id = seed(&host, &access_token("first", 10_000));
    host.http().enqueue_response(
        200,
        &[("content-type", "text/event-stream")],
        REPLY_STREAM.as_bytes().to_vec(),
    );
    let mut unary = streaming_request();
    unary.stream = false;
    let completion =
        block_on(upstream_for(&host, &id).execute(unary, ProtocolId::OPENAI_RESPONSES))
            .expect("execute");
    let Completion::Unary(response) = completion else {
        panic!("a unary request gets one reply");
    };
    assert_eq!(response.model.as_deref(), Some("gpt-5"));
    assert!(format!("{:?}", response.message.content).contains("Hi there"));
    let body = String::from_utf8(host.http().calls()[0].body().to_vec()).expect("utf8");
    assert!(body.contains(r#""stream":true"#), "{body}");

    let seen = host.observe().observations();
    assert_eq!(seen.len(), 1, "{seen:?}");
    assert_eq!(seen[0].remaining(), Some(50));
    assert_eq!(seen[0].window_reset(), Some(Duration::from_secs(600)));
    assert!(!seen[0].spending_credits());
}

#[test]
fn streaming_reply_passes_through_while_watched() {
    let dir = tempfile::tempdir().expect("dir");
    let host = host(&dir);
    let id = seed(&host, &access_token("first", 10_000));
    host.http().enqueue_response(
        200,
        &[("content-type", "text/event-stream")],
        REPLY_STREAM.as_bytes().to_vec(),
    );
    let completion = block_on(
        upstream_for(&host, &id).execute(streaming_request(), ProtocolId::OPENAI_RESPONSES),
    )
    .expect("execute");
    let Completion::Raw(mut body) = completion else {
        panic!("a same-protocol stream passes through");
    };
    let mut bytes = Vec::new();
    block_on(async {
        while let Some(chunk) = body.next_chunk().await {
            bytes.extend_from_slice(&chunk.expect("chunk"));
        }
    });
    assert_eq!(bytes, REPLY_STREAM.as_bytes());
    assert_eq!(host.observe().observations().len(), 1);
}

const USAGE: &str = r#"{
  "plan_type": "plus",
  "rate_limit": {
    "primary_window": {"used_percent": 33.3, "limit_window_seconds": 18000, "reset_after_seconds": 90},
    "secondary_window": {"used_percent": 100, "limit_window_seconds": 604800, "reset_at": 7200}
  },
  "credits": {"has_credits": false, "unlimited": false, "balance": "0"}
}"#;

#[test]
fn wham_usage_maps_windows() {
    let dir = tempfile::tempdir().expect("dir");
    let host = host(&dir);
    let id = seed(&host, &access_token("first", 10_000));
    host.http().enqueue_response(
        200,
        &[("content-type", "application/json")],
        USAGE.as_bytes().to_vec(),
    );
    let windows = block_on(subscription(&host).quota(&id, host.context())).expect("quota");
    let call = &host.http().calls()[0];
    assert_eq!(call.uri(), "https://codex.test/backend-api/wham/usage");
    assert!(
        call.sensitive_headers()
            .iter()
            .any(|header| header == "authorization")
    );
    assert!(
        call.sensitive_headers()
            .iter()
            .any(|header| header == "chatgpt-account-id")
    );
    assert_eq!(windows.len(), 2);
    assert_eq!(windows[0].name(), "5 hours");
    assert_eq!(windows[0].used(), 3_330);
    assert_eq!(windows[0].limit(), Some(10_000));
    assert_eq!(
        windows[0].resets_at(),
        Some(UNIX_EPOCH + Duration::from_secs(90))
    );
    assert!(windows[0].blocking());
    assert_eq!(windows[1].name(), "7 days");
    assert_eq!(windows[1].used(), 10_000);
    assert_eq!(
        windows[1].resets_at(),
        Some(UNIX_EPOCH + Duration::from_secs(7200))
    );

    host.http().enqueue_response(401, &[], b"no".to_vec());
    let error = block_on(subscription(&host).quota(&id, host.context())).expect_err("refused");
    assert_eq!(error.kind(), PluginErrorKind::Auth);
    host.http().enqueue_response(500, &[], b"no".to_vec());
    assert!(block_on(subscription(&host).quota(&id, host.context())).is_err());
    host.http().enqueue_response(200, &[], b"<html>".to_vec());
    let error = block_on(subscription(&host).quota(&id, host.context())).expect_err("not json");
    assert_eq!(error.kind(), PluginErrorKind::Decode);
}

#[test]
fn credits_window_nonblocking() {
    let usage = |credits: &str| {
        format!(
            r#"{{"rate_limit":{{"primary_window":{{"used_percent":100,"limit_window_seconds":86400}},"secondary_window":null}},"credits":{credits}}}"#
        )
    };
    let windows = crate::usage::windows(
        usage(r#"{"has_credits":true,"balance":"12.5"}"#).as_bytes(),
        UNIX_EPOCH,
    )
    .expect("windows");
    assert_eq!(windows.len(), 1);
    assert_eq!(windows[0].name(), "1 day");
    assert_eq!(windows[0].resets_at(), None);
    assert!(!windows[0].blocking());
    let unlimited = crate::usage::windows(usage(r#"{"unlimited":true}"#).as_bytes(), UNIX_EPOCH)
        .expect("windows");
    assert!(!unlimited[0].blocking());
    let spent = crate::usage::windows(
        usage(r#"{"has_credits":true,"balance":0}"#).as_bytes(),
        UNIX_EPOCH,
    )
    .expect("windows");
    assert!(spent[0].blocking());
    assert!(
        crate::usage::windows(b"{}", UNIX_EPOCH)
            .expect("none")
            .is_empty()
    );

    assert_eq!(crate::usage::window_name(3600), "1 hour");
    assert_eq!(crate::usage::window_name(7200), "2 hours");
    assert_eq!(crate::usage::window_name(172_800), "2 days");
    assert_eq!(crate::usage::window_name(90), "Allowance");
    assert_eq!(crate::usage::window_name(0), "Allowance");
    assert_eq!(crate::usage::window_name(-86_400), "Allowance");
}

#[test]
fn a3a_9_codex_warmup_posts_one_minimal_responses_call() {
    let dir = tempfile::tempdir().expect("dir");
    let host = host(&dir);
    let id = seed(&host, &access_token("warm", 10_000));
    seed_prompts(&host, &[("gpt-5-mini", CODEX_PROMPT)]);
    host.http()
        .enqueue(b"event: response.completed\ndata: {}\n\n".to_vec());
    block_on(crate::warmup::run(
        &Arc::new(endpoints()),
        &Arc::new(models::Prompts::default()),
        &id,
        host.context(),
    ))
    .expect("warmup");
    let responses = format!("{BASE}/responses");
    assert!(
        host.http()
            .calls()
            .iter()
            .any(|call| call.uri() == responses),
        "expected one responses POST"
    );
}
