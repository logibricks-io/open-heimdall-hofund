//! Unit tests against recording host backends; no network.

use std::pin::pin;
use std::sync::Arc;
use std::task::Poll;
use std::time::{Duration, UNIX_EPOCH};

use hofund_kit_subscription::{TestHost, block_on, join_all, poll_once, yield_times};
use hofund_plugin_api::{
    AccountId, AccountStatus, PluginErrorKind, PluginId, PluginKind, ProbeQuery, ProtocolId,
    RawTable, Request, SecretString, SignInChallenge, SignInCtx, SignInProof, Subscription,
    SubscriptionFactory, UpstreamProvider,
};
use hofund_plugin_sdk::oauth::{Pkce, SecretObject, same_secret};

use crate::tokens::{self, Need};
use crate::upstream::{self, CodexUpstream};
use crate::{Endpoints, factory_with_endpoints, sign_in};

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
    id
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
    CodexUpstream::new(id.clone(), host.context().clone(), Arc::new(endpoints()))
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
fn shaping_drops_sampling_and_rejects_unary() {
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

    let dir = tempfile::tempdir().expect("dir");
    let host = host(&dir);
    let id = seed(&host, &access_token("first", 10_000));
    let mut unary = streaming_request();
    unary.stream = false;
    let error = block_on(upstream_for(&host, &id).execute(unary, ProtocolId::OPENAI_RESPONSES))
        .err()
        .expect("unary");
    assert_eq!(error.kind(), PluginErrorKind::Unsupported);
    assert!(host.http().calls().is_empty());
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
