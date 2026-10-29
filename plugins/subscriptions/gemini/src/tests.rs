//! Unit tests against recording host backends; no network.

use std::sync::Arc;
use std::time::Duration;

use hofund_kit_subscription::{TestHost, block_on, join_all};
use hofund_plugin_api::{
    AccountId, AccountStatus, Completion, PluginErrorKind, PluginId, PluginKind, ProtocolId,
    RawTable, RawValue, SecretString, SignInChallenge, SignInCtx, SignInProof, Subscription,
    SubscriptionFactory,
};
use hofund_plugin_sdk::oauth::{SecretObject, TokenCache, account_id_from};

use crate::project::{self, Setup};
use crate::{CLIENT_SECRET, Endpoints, factory_with_endpoints, sign_in, tokens, upstream};

const SESSION: &str = "0123456789abcdef0123456789abcdef";
const TOKEN_URL: &str = "https://oauth.test/token";
const USERINFO_URL: &str = "https://oauth.test/userinfo";
const CODE_ASSIST: &str = "https://code-assist.test";

fn endpoints() -> Endpoints {
    Endpoints {
        authorize: "https://accounts.test/o/oauth2/v2/auth".to_owned(),
        token: TOKEN_URL.to_owned(),
        userinfo: USERINFO_URL.to_owned(),
        code_assist: CODE_ASSIST.to_owned(),
        onboard_poll: Duration::from_millis(1),
    }
}

fn plugin() -> PluginId {
    PluginId::from_static("gemini")
}

fn host(dir: &tempfile::TempDir) -> TestHost {
    TestHost::open_for(PluginKind::Subscription, plugin(), dir.path())
}

fn config(project: Option<&str>) -> RawTable {
    let mut raw = RawTable::new();
    if let Some(project) = project {
        raw.insert("project", RawValue::String(project.to_owned()));
    }
    raw
}

fn subscription_with(host: &TestHost, project: Option<&str>) -> Arc<dyn Subscription> {
    let factory = factory_with_endpoints(endpoints());
    let cfg = factory.validate(&config(project)).expect("config");
    factory.build(&cfg, host.context()).expect("build")
}

fn subscription(host: &TestHost) -> Arc<dyn Subscription> {
    subscription_with(host, None)
}

fn ctx() -> SignInCtx {
    SignInCtx::new(plugin())
        .with_session(SESSION)
        .expect("session")
}

fn secret(text: &str) -> SecretString {
    SecretString::from(text.to_owned())
}

fn token_reply(access: &str, refresh: Option<&str>, expires_in: u64) -> Vec<u8> {
    let mut body =
        format!(r#"{{"access_token":"{access}","token_type":"Bearer","expires_in":{expires_in}"#);
    if let Some(refresh) = refresh {
        body.push_str(&format!(r#","refresh_token":"{refresh}""#));
    }
    body.push('}');
    body.into_bytes()
}

fn seeded_id() -> AccountId {
    AccountId::from_static("a-0000000000000001")
}

fn seed(host: &TestHost, blob: &SecretObject) -> AccountId {
    let id = seeded_id();
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

fn signed_in_blob(project: Option<&str>) -> SecretObject {
    let blob = SecretObject::new()
        .with_number("v", 1)
        .with_secret("refresh_token", &secret("refresh-1"));
    match project {
        Some(project) => blob.with_text(project::PROJECT, project),
        None => blob,
    }
}

fn stored(host: &TestHost, id: &AccountId) -> SecretObject {
    let secret = block_on(host.context().accounts().secret(id))
        .expect("read")
        .expect("stored");
    SecretObject::parse(&secret).expect("blob")
}

fn posts_to(host: &TestHost, uri: &str) -> Vec<String> {
    host.http()
        .calls()
        .iter()
        .filter(|call| call.uri() == uri)
        .map(|call| String::from_utf8_lossy(call.body()).into_owned())
        .collect()
}

fn method_url(method: &str) -> String {
    format!("{CODE_ASSIST}/v1internal:{method}")
}

fn decode(text: &str) -> String {
    let bytes = text.as_bytes();
    let mut out = Vec::new();
    let mut i = 0;
    while i < bytes.len() {
        match bytes[i] {
            b'+' => out.push(b' '),
            b'%' if i + 2 < bytes.len() => {
                let hex = std::str::from_utf8(&bytes[i + 1..i + 3]).expect("hex");
                out.push(u8::from_str_radix(hex, 16).expect("hex digit"));
                i += 2;
            }
            byte => out.push(byte),
        }
        i += 1;
    }
    String::from_utf8(out).expect("utf-8")
}

fn query_pairs(url: &str) -> Vec<(String, String)> {
    let query = url
        .split_once('?')
        .map(|(_, query)| query)
        .unwrap_or_default();
    query
        .split('&')
        .filter_map(|pair| pair.split_once('='))
        .map(|(key, value)| (decode(key), decode(value)))
        .collect()
}

fn param(pairs: &[(String, String)], key: &str) -> Option<String> {
    pairs
        .iter()
        .find(|(name, _)| name == key)
        .map(|(_, value)| value.clone())
}

fn started_url(host: &TestHost, sub: &Arc<dyn Subscription>) -> String {
    let challenge = block_on(sub.start_sign_in(&ctx(), host.context())).expect("start");
    let SignInChallenge::OpenUrl { url, .. } = challenge else {
        panic!("expected an address to open");
    };
    url
}

fn started_state(host: &TestHost, sub: &Arc<dyn Subscription>) -> String {
    param(&query_pairs(&started_url(host, sub)), "state").expect("state")
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

fn userinfo(email: &str) -> Vec<u8> {
    format!(r#"{{"id":"1234","email":"{email}","verified_email":true}}"#).into_bytes()
}

const STANDARD_NEEDS_PROJECT: &str = r#"{"allowedTiers":[{"id":"standard-tier","name":"Gemini Code Assist","isDefault":true,"userDefinedCloudaicompanionProject":true}],"ineligibleTiers":[{"reasonCode":"INELIGIBLE_ACCOUNT","reasonMessage":""},{"reasonCode":"INELIGIBLE_ACCOUNT","reasonMessage":"Not eligible for the free tier"}]}"#;

#[test]
fn redirect_is_ipv4_ephemeral() {
    let dir = tempfile::tempdir().expect("dir");
    let host = host(&dir);
    let sub = subscription(&host);
    let url = started_url(&host, &sub);
    assert!(
        url.starts_with("https://accounts.test/o/oauth2/v2/auth?"),
        "{url}"
    );
    let pairs = query_pairs(&url);
    let redirect = param(&pairs, "redirect_uri").expect("redirect");
    let rest = redirect
        .strip_prefix("http://127.0.0.1:")
        .unwrap_or_else(|| panic!("an IPv4 loopback redirect: {redirect}"));
    let (port, path) = rest.split_once('/').expect("path");
    let port: u16 = port.parse().expect("port");
    assert_ne!(port, 0);
    assert_eq!(path, "oauth2callback");
    assert_eq!(
        param(&pairs, "client_id").as_deref(),
        Some(crate::CLIENT_ID)
    );
    assert_eq!(param(&pairs, "response_type").as_deref(), Some("code"));
    assert_eq!(param(&pairs, "access_type").as_deref(), Some("offline"));
    assert_eq!(param(&pairs, "prompt").as_deref(), Some("consent"));
    assert_eq!(
        param(&pairs, "code_challenge_method").as_deref(),
        Some("S256")
    );
    assert!(param(&pairs, "code_challenge").is_some_and(|c| c.len() == 43));
    assert_eq!(
        param(&pairs, "scope").as_deref(),
        Some(
            "https://www.googleapis.com/auth/cloud-platform \
             https://www.googleapis.com/auth/userinfo.email \
             https://www.googleapis.com/auth/userinfo.profile"
        )
    );
    block_on(sub.cancel_sign_in(&ctx(), host.context())).expect("cancel");
    let error = complete(&host, &sub, "x").expect_err("cancelled sign-in is gone");
    assert_eq!(error.kind(), PluginErrorKind::Config);
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
fn exchange_requires_refresh_token() {
    let dir = tempfile::tempdir().expect("dir");
    let host = host(&dir);
    let sub = subscription(&host);
    let state = started_state(&host, &sub);
    host.http().enqueue(token_reply("acc", None, 3600));
    let error = complete(&host, &sub, &state).expect_err("missing token");
    assert_eq!(error.kind(), PluginErrorKind::Auth);
    assert_eq!(host.http().calls().len(), 1, "no account lookup");
    let accounts = block_on(host.context().accounts().list()).expect("list");
    assert!(accounts.is_empty());
}

#[test]
fn exchange_needs_the_account_email() {
    for reply in [
        (200, br#"{"id":"1234"}"#.to_vec()),
        (200, br#"{"email":"  "}"#.to_vec()),
        (401, userinfo("u@example.test")),
    ] {
        let dir = tempfile::tempdir().expect("dir");
        let host = host(&dir);
        let sub = subscription(&host);
        let state = started_state(&host, &sub);
        host.http().enqueue(token_reply("acc", Some("ref"), 3600));
        host.http().enqueue_response(reply.0, &[], reply.1);
        let error = complete(&host, &sub, &state).expect_err("no email");
        assert_eq!(error.kind(), PluginErrorKind::Auth);
        let accounts = block_on(host.context().accounts().list()).expect("list");
        assert!(accounts.is_empty());
    }
}

#[test]
fn sign_in_stores_refresh_token_and_project_only() {
    let dir = tempfile::tempdir().expect("dir");
    let host = host(&dir);
    let sub = subscription(&host);
    let state = started_state(&host, &sub);
    host.http()
        .enqueue(token_reply("acc-1", Some("ref-1"), 3600));
    host.http().enqueue(userinfo("U@Example.test"));
    host.http().enqueue(
        br#"{"currentTier":{"id":"standard-tier","name":"Gemini Code Assist"},"cloudaicompanionProject":"proj-1"}"#
            .to_vec(),
    );
    let id = complete(&host, &sub, &state).expect("signed in");
    assert_eq!(id, account_id_from(&["google", "u@example.test"]));

    let [exchange] = posts_to(&host, TOKEN_URL).try_into().expect("one exchange");
    let form = query_pairs(&format!("?{exchange}"));
    assert_eq!(
        param(&form, "grant_type").as_deref(),
        Some("authorization_code")
    );
    assert_eq!(param(&form, "code").as_deref(), Some("code-1"));
    assert_eq!(
        param(&form, "client_secret").as_deref(),
        Some(CLIENT_SECRET)
    );
    assert!(param(&form, "code_verifier").is_some_and(|v| v.len() == 43));
    assert!(param(&form, "redirect_uri").is_some_and(|r| r.starts_with("http://127.0.0.1:")));
    assert_eq!(posts_to(&host, USERINFO_URL).len(), 1);
    let [load] = posts_to(&host, &method_url("loadCodeAssist"))
        .try_into()
        .expect("one load");
    let load: hofund_plugin_sdk::json::Value =
        hofund_plugin_sdk::json::from_str(&load).expect("json");
    assert_eq!(load["metadata"]["pluginType"], "GEMINI");
    assert!(load.get("cloudaicompanionProject").is_none());

    let blob = stored(&host, &id);
    assert_eq!(blob.text(project::PROJECT), Some("proj-1"));
    assert_eq!(blob.text(project::PLAN), Some("Gemini Code Assist"));
    assert!(blob.secret("refresh_token").is_some());
    assert!(blob.secret("access_token").is_none(), "never persisted");
    let accounts = block_on(host.context().accounts().list()).expect("list");
    assert_eq!(accounts.len(), 1);
    assert_eq!(accounts[0].label(), "U@Example.test");
    let status = block_on(sub.account_status(&id, host.context())).expect("status");
    assert_eq!(status, AccountStatus::Ready);
}

fn setup_with(
    load: &str,
    extra: &[&str],
    wanted: Option<&str>,
) -> (TestHost, Setup, tempfile::TempDir) {
    let dir = tempfile::tempdir().expect("dir");
    let host = host(&dir);
    host.http().enqueue(load.as_bytes().to_vec());
    for reply in extra {
        host.http().enqueue(reply.as_bytes().to_vec());
    }
    let outcome = block_on(project::setup(
        host.context(),
        &endpoints(),
        &secret("acc"),
        wanted,
    ))
    .expect("setup");
    (host, outcome, dir)
}

fn project_of(id: &str, plan: &str) -> Setup {
    Setup::Project {
        id: id.to_owned(),
        plan: plan.to_owned(),
    }
}

#[test]
fn project_from_load_code_assist() {
    let (host, outcome, _dir) = setup_with(
        r#"{"currentTier":{"id":"free-tier","name":"Gemini Code Assist for individuals"},"paidTier":{"id":"g1-pro-tier","name":"Google AI Pro"},"cloudaicompanionProject":{"id":"proj-2","name":"x"}}"#,
        &[],
        Some("mine"),
    );
    assert_eq!(outcome, project_of("proj-2", "Google AI Pro"));
    let [load] = posts_to(&host, &method_url("loadCodeAssist"))
        .try_into()
        .expect("one load");
    let load: hofund_plugin_sdk::json::Value =
        hofund_plugin_sdk::json::from_str(&load).expect("json");
    assert_eq!(load["cloudaicompanionProject"], "mine");
    assert_eq!(load["metadata"]["duetProject"], "mine");
    assert_eq!(load["metadata"]["ideType"], "IDE_UNSPECIFIED");
    assert_eq!(load["metadata"]["platform"], "PLATFORM_UNSPECIFIED");
    assert!(posts_to(&host, &method_url("onboardUser")).is_empty());

    // A current tier without a project uses the one wanted, named by id.
    let (_, outcome, _dir) = setup_with(
        r#"{"currentTier":{"id":"standard-tier"}}"#,
        &[],
        Some("mine"),
    );
    assert_eq!(outcome, project_of("mine", "standard-tier"));
    let (_, outcome, _dir) = setup_with(r#"{"currentTier":{"id":"standard-tier"}}"#, &[], None);
    assert_eq!(outcome, Setup::NeedsProject { why: String::new() });
    // An empty paid tier name falls back to the current tier.
    let (_, outcome, _dir) = setup_with(
        r#"{"currentTier":{"id":"free-tier","name":"Free"},"paidTier":{"id":"p"},"cloudaicompanionProject":"proj-3"}"#,
        &[],
        None,
    );
    assert_eq!(outcome, project_of("proj-3", "Free"));
}

#[test]
fn onboards_to_the_default_tier() {
    let (host, outcome, _dir) = setup_with(
        r#"{"allowedTiers":[{"id":"legacy-tier","userDefinedCloudaicompanionProject":true},{"id":"free-tier","name":"Free","isDefault":true}]}"#,
        &[
            r#"{"done":false}"#,
            r#"{"done":true,"response":{"cloudaicompanionProject":{"id":"proj-free"}}}"#,
        ],
        None,
    );
    assert_eq!(outcome, project_of("proj-free", "Free"));
    let asks = posts_to(&host, &method_url("onboardUser"));
    assert_eq!(asks.len(), 2, "asked again until done");
    let ask: hofund_plugin_sdk::json::Value =
        hofund_plugin_sdk::json::from_str(&asks[0]).expect("json");
    assert_eq!(ask["tierId"], "free-tier");
    assert_eq!(ask["metadata"]["pluginType"], "GEMINI");
    assert!(
        ask.get("cloudaicompanionProject").is_none(),
        "free tier names none"
    );

    // Without a default the first tier is taken, and a non-free tier names
    // the wanted project.
    let (host, outcome, _dir) = setup_with(
        r#"{"allowedTiers":[{"id":"standard-tier","name":"Std","userDefinedCloudaicompanionProject":true},{"id":"free-tier"}]}"#,
        &[r#"{"done":true,"response":{}}"#],
        Some("mine"),
    );
    assert_eq!(outcome, project_of("mine", "Std"));
    let [ask] = posts_to(&host, &method_url("onboardUser"))
        .try_into()
        .expect("one ask");
    let ask: hofund_plugin_sdk::json::Value =
        hofund_plugin_sdk::json::from_str(&ask).expect("json");
    assert_eq!(ask["tierId"], "standard-tier");
    assert_eq!(ask["cloudaicompanionProject"], "mine");

    // No tiers at all is the legacy tier, which needs a project named.
    let (host, outcome, _dir) = setup_with("{}", &[], None);
    assert_eq!(outcome, Setup::NeedsProject { why: String::new() });
    assert!(posts_to(&host, &method_url("onboardUser")).is_empty());

    // Onboarding that gives no project and has none wanted needs one.
    let (_, outcome, _dir) = setup_with(
        r#"{"allowedTiers":[{"id":"free-tier","isDefault":true}]}"#,
        &[r#"{"done":true}"#],
        None,
    );
    assert_eq!(outcome, Setup::NeedsProject { why: String::new() });
}

#[test]
fn onboarding_gives_up_after_five_asks() {
    let dir = tempfile::tempdir().expect("dir");
    let host = host(&dir);
    host.http()
        .enqueue(br#"{"allowedTiers":[{"id":"free-tier","isDefault":true}]}"#.to_vec());
    for _ in 0..6 {
        host.http().enqueue(br#"{"done":false}"#.to_vec());
    }
    let error = block_on(project::setup(
        host.context(),
        &endpoints(),
        &secret("acc"),
        None,
    ))
    .expect_err("too long");
    assert_eq!(error.kind(), PluginErrorKind::Timeout);
    assert_eq!(posts_to(&host, &method_url("onboardUser")).len(), 5);
}

#[test]
fn code_assist_refusal_is_an_error() {
    for (status, kind) in [
        (401, PluginErrorKind::Auth),
        (429, PluginErrorKind::Quota),
        (500, PluginErrorKind::Status),
    ] {
        let dir = tempfile::tempdir().expect("dir");
        let host = host(&dir);
        host.http().enqueue_response(
            status,
            &[],
            br#"{"error":{"message":"secret detail"}}"#.to_vec(),
        );
        let error = block_on(project::setup(
            host.context(),
            &endpoints(),
            &secret("acc"),
            None,
        ))
        .expect_err("refused");
        assert_eq!(error.kind(), kind);
        assert!(!error.message().contains("secret detail"));
    }
}

#[test]
fn standard_tier_without_project_needs_sign_in() {
    let dir = tempfile::tempdir().expect("dir");
    let host = host(&dir);
    let sub = subscription(&host);
    let state = started_state(&host, &sub);
    host.http()
        .enqueue(token_reply("acc-1", Some("ref-1"), 3600));
    host.http().enqueue(userinfo("u@example.test"));
    host.http()
        .enqueue(STANDARD_NEEDS_PROJECT.as_bytes().to_vec());
    let id = complete(&host, &sub, &state).expect("the sign-in is kept");
    assert!(posts_to(&host, &method_url("onboardUser")).is_empty());
    let blob = stored(&host, &id);
    assert!(blob.flag(project::NEEDS_PROJECT));
    assert!(blob.text(project::PROJECT).is_none());
    let status = block_on(sub.account_status(&id, host.context())).expect("status");
    let AccountStatus::NeedsSignIn { reason } = status else {
        panic!("needs a project: {status:?}");
    };
    assert!(
        reason.starts_with("Not eligible for the free tier: "),
        "{reason}"
    );
    assert!(reason.contains("`project`"), "{reason}");

    // Naming the project in the config makes it usable, and a refresh
    // onboards it there.
    let named = subscription_with(&host, Some("  p-9 "));
    let status = block_on(named.account_status(&id, host.context())).expect("status");
    assert_eq!(status, AccountStatus::Ready);
    host.http().enqueue(token_reply("acc-2", None, 3600));
    host.http()
        .enqueue(STANDARD_NEEDS_PROJECT.as_bytes().to_vec());
    host.http()
        .enqueue(br#"{"done":true,"response":{"cloudaicompanionProject":"p-9"}}"#.to_vec());
    block_on(named.refresh(&id, host.context())).expect("refresh");
    let [ask] = posts_to(&host, &method_url("onboardUser"))
        .try_into()
        .expect("one ask");
    let ask: hofund_plugin_sdk::json::Value =
        hofund_plugin_sdk::json::from_str(&ask).expect("json");
    assert_eq!(ask["cloudaicompanionProject"], "p-9");
    let blob = stored(&host, &id);
    assert_eq!(blob.text(project::PROJECT), Some("p-9"));
    assert!(!blob.flag(project::NEEDS_PROJECT));
    assert!(blob.secret("access_token").is_none());
}

#[test]
fn failed_setup_keeps_sign_in_and_refresh_retries() {
    let dir = tempfile::tempdir().expect("dir");
    let host = host(&dir);
    let sub = subscription(&host);
    let state = started_state(&host, &sub);
    host.http()
        .enqueue(token_reply("acc-1", Some("ref-1"), 3600));
    host.http().enqueue(userinfo("u@example.test"));
    host.http().enqueue_response(503, &[], Vec::new());
    let id = complete(&host, &sub, &state).expect("kept");
    let status = block_on(sub.account_status(&id, host.context())).expect("status");
    assert_eq!(
        status,
        AccountStatus::NeedsSignIn {
            reason: project::PENDING_REASON.to_owned()
        }
    );

    host.http().enqueue(token_reply("acc-2", None, 3600));
    host.http().enqueue(
        br#"{"currentTier":{"id":"free-tier"},"cloudaicompanionProject":"proj-1"}"#.to_vec(),
    );
    block_on(sub.refresh(&id, host.context())).expect("refresh");
    assert_eq!(stored(&host, &id).text(project::PROJECT), Some("proj-1"));
    let status = block_on(sub.account_status(&id, host.context())).expect("status");
    assert_eq!(status, AccountStatus::Ready);

    // With a project stored, a refresh only refreshes.
    let before = host.http().calls().len();
    host.http().enqueue(token_reply("acc-3", None, 3600));
    block_on(sub.refresh(&id, host.context())).expect("refresh");
    let calls = host.http().calls();
    assert_eq!(calls.len(), before + 1);
    assert_eq!(calls[before].uri(), TOKEN_URL);
}

#[test]
fn setup_keeps_a_project_stored_while_it_asked() {
    let dir = tempfile::tempdir().expect("dir");
    let host = host(&dir);
    let sub = subscription(&host);
    let id = seed(&host, &signed_in_blob(None));
    host.http().enqueue(token_reply("acc-2", None, 3600));
    host.http().enqueue(
        br#"{"currentTier":{"id":"free-tier"},"cloudaicompanionProject":"proj-1"}"#.to_vec(),
    );
    block_on(async {
        let accounts = host.context().accounts();
        let held = accounts
            .lease(&id, std::time::Duration::from_secs(5))
            .await
            .expect("lease");
        let mut refresh = sub.refresh(&id, host.context());
        // The setup asks Code Assist, then waits for the lease held here.
        for _ in 0..1000 {
            assert!(hofund_kit_subscription::poll_once(refresh.as_mut()).is_pending());
            if host.http().calls().len() == 2 {
                break;
            }
            hofund_kit_subscription::yield_times(1).await;
        }
        assert_eq!(host.http().calls().len(), 2, "the setup asked Code Assist");
        held.commit(None, signed_in_blob(Some("proj-other")).to_secret())
            .await
            .expect("commit");
        refresh.await.expect("refresh");
    });
    assert_eq!(
        stored(&host, &id).text(project::PROJECT),
        Some("proj-other")
    );
}

#[test]
fn refresh_needing_a_project_says_so() {
    let dir = tempfile::tempdir().expect("dir");
    let host = host(&dir);
    let sub = subscription(&host);
    let id = seed(&host, &signed_in_blob(None));
    host.http().enqueue(token_reply("acc-2", None, 3600));
    host.http()
        .enqueue(STANDARD_NEEDS_PROJECT.as_bytes().to_vec());
    let error = block_on(sub.refresh(&id, host.context())).expect_err("needs a project");
    assert_eq!(error.kind(), PluginErrorKind::Auth);
    assert!(error.message().contains("`project`"), "{}", error.message());
    assert!(stored(&host, &id).flag(project::NEEDS_PROJECT));
}

#[test]
fn refresh_not_persisted_and_single_flight() {
    let dir = tempfile::tempdir().expect("dir");
    let host = host(&dir);
    let id = seed(&host, &signed_in_blob(Some("proj-1")));
    let before = stored(&host, &id).to_body();
    let cache = TokenCache::new(tokens::REFRESH_MARGIN);
    host.http().enqueue(token_reply("acc-1", None, 3600));
    let endpoints = endpoints();
    let calls = (0..3)
        .map(|_| {
            Box::pin(tokens::access(host.context(), &endpoints, &cache, &id))
                as std::pin::Pin<
                    Box<
                        dyn std::future::Future<
                                Output = Result<SecretString, hofund_plugin_api::PluginError>,
                            >,
                    >,
                >
        })
        .collect();
    for token in block_on(join_all(calls)) {
        let token = token.expect("token");
        assert!(hofund_plugin_sdk::oauth::same_secret(
            &token,
            &secret("acc-1")
        ));
    }
    let [form] = posts_to(&host, TOKEN_URL).try_into().expect("one refresh");
    let form = query_pairs(&format!("?{form}"));
    assert_eq!(param(&form, "grant_type").as_deref(), Some("refresh_token"));
    assert_eq!(param(&form, "refresh_token").as_deref(), Some("refresh-1"));
    assert_eq!(param(&form, "client_id").as_deref(), Some(crate::CLIENT_ID));
    assert_eq!(
        param(&form, "client_secret").as_deref(),
        Some(CLIENT_SECRET)
    );
    // The fresh token is served from memory and the secret is untouched.
    block_on(tokens::access(host.context(), &endpoints, &cache, &id)).expect("cached");
    assert_eq!(posts_to(&host, TOKEN_URL).len(), 1);
    assert_eq!(stored(&host, &id).to_body(), before);
}

#[test]
fn token_inside_the_margin_is_refreshed() {
    let dir = tempfile::tempdir().expect("dir");
    let host = host(&dir);
    let id = seed(&host, &signed_in_blob(Some("proj-1")));
    let cache = TokenCache::new(tokens::REFRESH_MARGIN);
    let endpoints = endpoints();
    // The test clock reads the epoch: 100 s is inside the two minutes and
    // 130 s is not.
    host.http().enqueue(token_reply("acc-1", None, 130));
    block_on(tokens::access(host.context(), &endpoints, &cache, &id)).expect("first");
    block_on(tokens::access(host.context(), &endpoints, &cache, &id)).expect("cached");
    assert_eq!(posts_to(&host, TOKEN_URL).len(), 1);

    let dir = tempfile::tempdir().expect("dir");
    let host = self::host(&dir);
    let id = seed(&host, &signed_in_blob(Some("proj-1")));
    let cache = TokenCache::new(tokens::REFRESH_MARGIN);
    host.http().enqueue(token_reply("acc-1", None, 100));
    host.http().enqueue(token_reply("acc-2", None, 100));
    block_on(tokens::access(host.context(), &endpoints, &cache, &id)).expect("first");
    let second = block_on(tokens::access(host.context(), &endpoints, &cache, &id)).expect("again");
    assert!(hofund_plugin_sdk::oauth::same_secret(
        &second,
        &secret("acc-2")
    ));
    assert_eq!(posts_to(&host, TOKEN_URL).len(), 2);
}

#[test]
fn invalid_grant_needs_sign_in() {
    let dir = tempfile::tempdir().expect("dir");
    let host = host(&dir);
    let sub = subscription(&host);
    let id = seed(&host, &signed_in_blob(Some("proj-1")));
    host.http()
        .enqueue_response(400, &[], br#"{"error":"invalid_grant"}"#.to_vec());
    let error = block_on(sub.refresh(&id, host.context())).expect_err("refused");
    assert_eq!(error.kind(), PluginErrorKind::Auth);
    assert!(stored(&host, &id).flag(tokens::LAPSED));
    let status = block_on(sub.account_status(&id, host.context())).expect("status");
    assert_eq!(
        status,
        AccountStatus::NeedsSignIn {
            reason: tokens::LAPSED_REASON.to_owned()
        }
    );
    // A lapsed sign-in sends nothing more.
    let cache = TokenCache::new(tokens::REFRESH_MARGIN);
    let error =
        block_on(tokens::access(host.context(), &endpoints(), &cache, &id)).expect_err("lapsed");
    assert_eq!(error.kind(), PluginErrorKind::Auth);
    assert_eq!(posts_to(&host, TOKEN_URL).len(), 1);
}

#[test]
fn server_error_does_not_lapse() {
    let dir = tempfile::tempdir().expect("dir");
    let host = host(&dir);
    let sub = subscription(&host);
    let id = seed(&host, &signed_in_blob(Some("proj-1")));
    host.http().enqueue_response(503, &[], Vec::new());
    let error = block_on(sub.refresh(&id, host.context())).expect_err("unavailable");
    assert_eq!(error.kind(), PluginErrorKind::Status);
    assert!(!stored(&host, &id).flag(tokens::LAPSED));
}

#[test]
fn lapse_spares_a_newer_sign_in() {
    let dir = tempfile::tempdir().expect("dir");
    let host = host(&dir);
    let id = seed(&host, &signed_in_blob(Some("proj-1")));
    block_on(tokens::mark_lapsed(
        host.context(),
        &id,
        &secret("an-older-token"),
    ))
    .expect("checked");
    assert!(!stored(&host, &id).flag(tokens::LAPSED));
    block_on(tokens::mark_lapsed(
        host.context(),
        &id,
        &secret("refresh-1"),
    ))
    .expect("marked");
    assert!(stored(&host, &id).flag(tokens::LAPSED));
}

#[test]
fn a_new_sign_in_drops_the_cached_token() {
    let dir = tempfile::tempdir().expect("dir");
    let host = host(&dir);
    let sub = subscription(&host);
    let sign_in = |refresh: &str| {
        let state = started_state(&host, &sub);
        host.http().enqueue(token_reply("acc", Some(refresh), 3600));
        host.http().enqueue(userinfo("u@example.test"));
        host.http().enqueue(
            br#"{"currentTier":{"id":"free-tier"},"cloudaicompanionProject":"proj-1"}"#.to_vec(),
        );
        complete(&host, &sub, &state).expect("signed in")
    };
    let id = sign_in("ref-1");
    host.http()
        .enqueue_response(400, &[], br#"{"error":"invalid_grant"}"#.to_vec());
    let _ = block_on(sub.refresh(&id, host.context())).expect_err("lapsed");
    let id = sign_in("ref-2");
    assert!(!stored(&host, &id).flag(tokens::LAPSED));
    host.http().enqueue(token_reply("acc-2", None, 3600));
    block_on(sub.refresh(&id, host.context())).expect("the failure is not replayed");
}

#[test]
fn user_agent_names_the_platform_as_node_does() {
    assert_eq!(
        project::user_agent_for(None, "macos", "aarch64"),
        "GeminiCLI/0.61.0/gemini-2.5-pro (darwin; arm64; terminal)"
    );
    assert_eq!(
        project::user_agent_for(Some("gemini-3-pro-preview"), "linux", "x86_64"),
        "GeminiCLI/0.61.0/gemini-3-pro-preview (linux; x64; terminal)"
    );
    assert_eq!(
        project::user_agent_for(None, "windows", "x86_64"),
        "GeminiCLI/0.61.0/gemini-2.5-pro (win32; x64; terminal)"
    );
    assert!(project::user_agent(None).starts_with("GeminiCLI/0.61.0/gemini-2.5-pro ("));
}

#[test]
fn config_takes_an_optional_project() {
    let factory = factory_with_endpoints(endpoints());
    let dir = tempfile::tempdir().expect("dir");
    let host = host(&dir);
    assert!(factory.validate(&config(Some("  "))).is_ok());
    assert!(factory.validate(&config(Some("p-1"))).is_ok());
    let mut wrong = RawTable::new();
    wrong.insert("project", RawValue::Boolean(true));
    assert_eq!(
        factory.validate(&wrong).expect_err("not a string").path(),
        "project"
    );
    let mut unknown = RawTable::new();
    unknown.insert("base", RawValue::String("https://example.test".to_owned()));
    assert_eq!(
        factory.validate(&unknown).expect_err("unknown").path(),
        "base"
    );
    assert_eq!(factory.config_schema().field_names(), vec!["project"]);

    // A blank project lets Code Assist pick, like none at all.
    let id = seed(&host, &signed_in_blob(None));
    let blank = subscription_with(&host, Some("  "));
    let status = block_on(blank.account_status(&id, host.context())).expect("status");
    assert!(
        matches!(status, AccountStatus::NeedsSignIn { .. }),
        "{status:?}"
    );
}

#[test]
fn exchange_form_carries_the_client_secret() {
    let form = sign_in::exchange_form(
        "http://127.0.0.1:1/oauth2callback",
        &secret("c"),
        &secret("v"),
    );
    let pairs = query_pairs(&format!("?{form}"));
    assert_eq!(
        param(&pairs, "client_secret").as_deref(),
        Some(CLIENT_SECRET)
    );
    assert_eq!(param(&pairs, "code_verifier").as_deref(), Some("v"));
}

fn gl_request(model: &str, stream: bool) -> hofund_plugin_api::Request {
    let mut request = hofund_plugin_api::ProbeQuery::new(ProtocolId::GENERATIVE_LANGUAGE, model)
        .expect("query")
        .minimal_request();
    request.stream = stream;
    request
}

fn raw_text(completion: Completion) -> String {
    let Completion::Raw(mut body) = completion else {
        panic!("a same-protocol reply is passed on raw");
    };
    block_on(async move {
        let mut out = Vec::new();
        while let Some(chunk) = body.next_chunk().await {
            out.extend_from_slice(&chunk.expect("chunk"));
        }
        String::from_utf8(out).expect("utf8")
    })
}

fn ready(host: &TestHost, project: Option<&str>) -> (Arc<dyn Subscription>, AccountId) {
    let id = seed(host, &signed_in_blob(project));
    host.http().enqueue(token_reply("acc-1", None, 3600));
    (subscription(host), id)
}

const UNARY_REPLY: &str = r#"{"response":{"candidates":[{"content":{"role":"model","parts":[{"text":"hi"}]},"finishReason":"STOP"}],"usageMetadata":{"promptTokenCount":3,"candidatesTokenCount":1,"totalTokenCount":4}},"traceId":"t-1"}"#;

#[test]
fn envelope_adds_project_and_prompt_id() {
    let dir = tempfile::tempdir().expect("dir");
    let host = host(&dir);
    let (sub, id) = ready(&host, Some("proj-9"));
    host.http().enqueue(UNARY_REPLY.as_bytes().to_vec());
    let upstream = sub.upstream(&id, host.context()).expect("upstream");
    block_on(upstream.execute(
        gl_request("models/gemini-2.5-pro", false),
        ProtocolId::GENERATIVE_LANGUAGE,
    ))
    .expect("execute");
    let call = &host.http().calls()[1];
    assert_eq!(call.uri(), method_url("generateContent"));
    let sent: hofund_plugin_sdk::json::Value =
        hofund_plugin_sdk::json::from_slice(call.body()).expect("json body");
    assert_eq!(sent["model"], "gemini-2.5-pro");
    assert_eq!(sent["project"], "proj-9");
    let prompt = sent["user_prompt_id"].as_str().expect("prompt id");
    assert!(prompt.len() >= 12, "{prompt}");
    assert!(sent["request"]["contents"].is_array(), "{sent}");
    assert!(sent["request"].get("model").is_none(), "{sent}");
    let sensitive = call.sensitive_headers();
    assert!(
        sensitive.iter().any(|name| name == "authorization"),
        "{sensitive:?}"
    );
    assert!(
        sensitive.iter().any(|name| name == "user-agent"),
        "{sensitive:?}"
    );
    assert_eq!(
        upstream::headers("models/gemini-2.5-pro"),
        vec![(
            "user-agent".to_owned(),
            project::user_agent(Some("gemini-2.5-pro"))
        )]
    );
}

#[test]
fn the_configured_project_is_used_when_none_is_stored() {
    let dir = tempfile::tempdir().expect("dir");
    let host = host(&dir);
    let id = seed(&host, &signed_in_blob(None));
    host.http().enqueue(token_reply("acc-1", None, 3600));
    host.http().enqueue(UNARY_REPLY.as_bytes().to_vec());
    let sub = subscription_with(&host, Some("from-config"));
    let upstream = sub.upstream(&id, host.context()).expect("upstream");
    block_on(upstream.execute(
        gl_request("gemini-2.5-pro", false),
        ProtocolId::GENERATIVE_LANGUAGE,
    ))
    .expect("execute");
    let sent = posts_to(&host, &method_url("generateContent"));
    assert_eq!(sent.len(), 1);
    assert!(
        sent[0].contains(r#""project":"from-config""#),
        "{}",
        sent[0]
    );
}

#[test]
fn sse_response_unwrapped() {
    let dir = tempfile::tempdir().expect("dir");
    let host = host(&dir);
    let (sub, id) = ready(&host, Some("proj-9"));
    host.http().enqueue_response(
        200,
        &[("content-type", "text/event-stream")],
        "data: {\"response\":{\"candidates\":[{\"content\":{\"parts\":[{\"text\":\"a\"}]}}]}}\r\n\r\n"
            .as_bytes()
            .to_vec(),
    );
    let upstream = sub.upstream(&id, host.context()).expect("upstream");
    let completion = block_on(upstream.execute(
        gl_request("gemini-2.5-pro", true),
        ProtocolId::GENERATIVE_LANGUAGE,
    ))
    .expect("execute");
    assert_eq!(
        raw_text(completion),
        "data: {\"candidates\":[{\"content\":{\"parts\":[{\"text\":\"a\"}]}}]}\r\n\r\n"
    );
    assert_eq!(
        host.http().calls()[1].uri(),
        format!("{CODE_ASSIST}/v1internal:streamGenerateContent?alt=sse")
    );
}

#[test]
fn a_unary_reply_is_unwrapped_before_decoding() {
    let dir = tempfile::tempdir().expect("dir");
    let host = host(&dir);
    let (sub, id) = ready(&host, Some("proj-9"));
    host.http().enqueue(UNARY_REPLY.as_bytes().to_vec());
    let upstream = sub.upstream(&id, host.context()).expect("upstream");
    let mut request = gl_request("gemini-2.5-pro", false);
    request.stream = false;
    let completion = block_on(upstream.execute(request, ProtocolId::OPENAI_CHAT)).expect("execute");
    let Completion::Unary(response) = completion else {
        panic!("a unary reply decodes");
    };
    let usage = response.usage.expect("usage from the lifted reply");
    assert_eq!(usage.input_tokens, Some(3));
}

#[test]
fn a_401_refreshes_once_and_resends() {
    let dir = tempfile::tempdir().expect("dir");
    let host = host(&dir);
    let (sub, id) = ready(&host, Some("proj-9"));
    host.http().enqueue_response(401, &[], b"{}".to_vec());
    host.http().enqueue(token_reply("acc-2", None, 3600));
    host.http().enqueue(UNARY_REPLY.as_bytes().to_vec());
    let upstream = sub.upstream(&id, host.context()).expect("upstream");
    block_on(upstream.execute(
        gl_request("gemini-2.5-pro", false),
        ProtocolId::GENERATIVE_LANGUAGE,
    ))
    .expect("execute");
    let calls = host.http().calls();
    let uris: Vec<&str> = calls.iter().map(|call| call.uri()).collect();
    let generate = method_url("generateContent");
    assert_eq!(uris, vec![TOKEN_URL, &generate, TOKEN_URL, &generate]);
}

#[test]
fn resource_exhausted_429_maps_retry_after() {
    let dir = tempfile::tempdir().expect("dir");
    let host = host(&dir);
    let (sub, id) = ready(&host, Some("proj-9"));
    host.http().enqueue_response(
        429,
        &[("content-type", "application/json")],
        br#"{"error":{"code":429,"status":"RESOURCE_EXHAUSTED","message":"quota","details":[{"@type":"type.googleapis.com/google.rpc.RetryInfo","retryDelay":"12s"}]}}"#.to_vec(),
    );
    let upstream = sub.upstream(&id, host.context()).expect("upstream");
    let error = block_on(upstream.execute(
        gl_request("gemini-2.5-pro", false),
        ProtocolId::GENERATIVE_LANGUAGE,
    ))
    .err()
    .expect("429");
    assert_eq!(error.retry_after(), Some(Duration::from_secs(12)));
    assert_eq!(host.http().calls().len(), 2, "a 429 is not retried here");
}

const QUOTA_REPLY: &str = r#"{"buckets":[
    {"modelId":"gemini-2.5-pro","remainingFraction":0.25,"resetTime":"2026-11-01T00:00:00Z","tokenType":"REQUESTS"},
    {"modelId":"gemini-2.5-pro_vertex","remainingFraction":0.5},
    {"modelId":"gemini-2.5-flash","remainingFraction":1}
]}"#;

#[test]
fn quota_windows_are_model_scoped() {
    let dir = tempfile::tempdir().expect("dir");
    let host = host(&dir);
    let (sub, id) = ready(&host, Some("proj-9"));
    host.http().enqueue(QUOTA_REPLY.as_bytes().to_vec());
    let windows = block_on(sub.quota(&id, host.context())).expect("quota");
    assert_eq!(
        posts_to(&host, &method_url("retrieveUserQuota")),
        vec![r#"{"project":"proj-9"}"#.to_owned()]
    );
    assert_eq!(windows.len(), 2);
    assert_eq!(windows[0].name(), "gemini-2.5-pro");
    assert_eq!(windows[0].scope_model(), Some("gemini-2.5-pro"));
    assert_eq!(windows[0].used(), 7_500);
    assert_eq!(windows[0].limit(), Some(10_000));
    assert_eq!(
        windows[0].resets_at(),
        Some(std::time::UNIX_EPOCH + Duration::from_secs(1_793_491_200))
    );
    assert_eq!(windows[1].scope_model(), Some("gemini-2.5-flash"));
    assert_eq!(windows[1].used(), 0);
    assert_eq!(windows[1].resets_at(), None);
}

#[test]
fn models_come_from_the_quota_buckets() {
    let dir = tempfile::tempdir().expect("dir");
    let host = host(&dir);
    let (sub, id) = ready(&host, Some("proj-9"));
    host.http().enqueue(QUOTA_REPLY.as_bytes().to_vec());
    let list = block_on(sub.list_models(&id, host.context())).expect("models");
    let ids: Vec<&str> = list.models().iter().map(|model| model.id()).collect();
    assert_eq!(ids, vec!["gemini-2.5-pro", "gemini-2.5-flash"]);
}

#[test]
fn envelope_and_lift_shapes() {
    let mut body = hofund_plugin_sdk::json::Map::new();
    body.insert("contents".to_owned(), hofund_plugin_sdk::json::json!([]));
    body.insert("model".to_owned(), hofund_plugin_sdk::json::json!("x"));
    upstream::envelope(&mut body, "m", "p", "id-1");
    assert_eq!(
        hofund_plugin_sdk::json::Value::Object(body),
        hofund_plugin_sdk::json::json!({
            "model": "m", "project": "p", "user_prompt_id": "id-1",
            "request": {"contents": []},
        })
    );
    assert_eq!(
        upstream::lift(hofund_plugin_sdk::json::json!({"response": {"a": 1}, "traceId": "t"})),
        hofund_plugin_sdk::json::json!({"a": 1})
    );
    assert_eq!(
        upstream::lift(hofund_plugin_sdk::json::json!({"a": 1})),
        hofund_plugin_sdk::json::json!({"a": 1})
    );
    assert_eq!(upstream::bare(" models/gemini-x "), "gemini-x");
    assert_eq!(upstream::bare("gemini-x"), "gemini-x");
}
