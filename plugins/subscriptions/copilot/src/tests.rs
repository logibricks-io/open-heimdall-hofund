//! Unit tests against recording host backends; no network.

use std::sync::Arc;
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

use hofund_kit_subscription::{TestHost, block_on, join_all};
use hofund_plugin_api::{
    AccountId, AccountStatus, ContentPart, PluginErrorKind, PluginId, PluginKind, ProbeQuery,
    ProtocolId, QuotaWindow, RawTable, Request, Role, SecretString, SignInChallenge, SignInCtx,
    SignInProof, Subscription, SubscriptionFactory, UpstreamProvider,
};
use hofund_plugin_sdk::json::{self, json};
use hofund_plugin_sdk::oauth::{SecretObject, TokenCache, account_id_from};

use crate::models::{self, Book};
use crate::session::{self, LAPSED, LAPSED_REASON, MARGIN, Session};
use crate::site::Site;
use crate::upstream::{CopilotUpstream, has_image, initiator, request_headers};
use crate::{CopilotFactory, quota};

const SESSION: &str = "0123456789abcdef0123456789abcdef";
const API: &str = "https://api.individual.githubcopilot.com";

fn plugin() -> PluginId {
    PluginId::from_static("copilot")
}

fn host(dir: &tempfile::TempDir) -> TestHost {
    TestHost::open_for(PluginKind::Subscription, plugin(), dir.path())
}

fn subscription(host: &TestHost) -> Arc<dyn Subscription> {
    let cfg = CopilotFactory.validate(&RawTable::new()).expect("config");
    CopilotFactory.build(&cfg, host.context()).expect("build")
}

fn ctx() -> SignInCtx {
    SignInCtx::new(plugin())
        .with_session(SESSION)
        .expect("session")
}

fn secret(text: &str) -> SecretString {
    SecretString::from(text.to_owned())
}

fn uris(host: &TestHost) -> Vec<String> {
    host.http()
        .calls()
        .iter()
        .map(|call| call.uri().to_owned())
        .collect()
}

fn site(text: &str) -> Site {
    Site::parse(text).expect("site")
}

fn seed_on(host: &TestHost, site_name: &str, oauth: &str) -> AccountId {
    let id = AccountId::from_static("a-0000000000000001");
    let blob = SecretObject::new()
        .with_number("v", 1)
        .with_secret("oauth_token", &secret(oauth))
        .with_text("host", site_name)
        .with_text("login", "octo");
    block_on(async {
        let accounts = host.context().accounts();
        let account = accounts.account(id.clone(), "octo").expect("account");
        accounts
            .upsert(account, blob.to_secret())
            .await
            .expect("upsert");
    });
    id
}

fn seed(host: &TestHost) -> AccountId {
    seed_on(host, "github.com", "gho-1")
}

fn stored(host: &TestHost, id: &AccountId) -> SecretObject {
    let secret = block_on(host.context().accounts().secret(id))
        .expect("read")
        .expect("stored");
    SecretObject::parse(&secret).expect("blob")
}

fn session_reply(token: &str, expires_at: u64) -> Vec<u8> {
    json!({"token": token, "expires_at": expires_at, "endpoints": {"api": API}})
        .to_string()
        .into_bytes()
}

fn device_reply() -> Vec<u8> {
    json!({
        "device_code": "dev-1",
        "user_code": "ABCD-1234",
        "verification_uri": "https://evil.example/device",
        "expires_in": 900,
        "interval": 1
    })
    .to_string()
    .into_bytes()
}

fn token_reply() -> Vec<u8> {
    br#"{"access_token":"gho-new","token_type":"bearer","scope":"read:user"}"#.to_vec()
}

fn user_reply() -> Vec<u8> {
    br#"{"login":"octo","id":42}"#.to_vec()
}

fn part(value: json::Value) -> ContentPart {
    json::from_value(value).expect("part")
}

fn chat_request() -> Request {
    ProbeQuery::new(ProtocolId::OPENAI_CHAT, "gpt-4.1")
        .expect("query")
        .minimal_request()
}

fn challenge_url(challenge: &SignInChallenge) -> String {
    format!("{challenge:?}")
}

#[test]
fn ghe_urls_derived_from_host() {
    let github = site("");
    assert_eq!(github, Site::github());
    for same in [
        "github.com",
        " GitHub.com ",
        "www.github.com",
        "https://github.com/",
    ] {
        assert_eq!(site(same), github, "{same}");
    }
    assert!(!github.is_enterprise());
    assert_eq!(
        github.device_code_url(),
        "https://github.com/login/device/code"
    );
    assert_eq!(
        github.token_url(),
        "https://github.com/login/oauth/access_token"
    );
    assert_eq!(github.device_page(), "https://github.com/login/device");
    assert_eq!(
        github.session_url(),
        "https://api.github.com/copilot_internal/v2/token"
    );
    assert_eq!(
        github.copilot_user_url(),
        "https://api.github.com/copilot_internal/user"
    );
    assert_eq!(github.user_url(), "https://api.github.com/user");
    assert_eq!(github.api_base(), "https://api.githubcopilot.com");

    let acme = site("https://Acme.GHE.com/");
    assert_eq!(acme, site("acme.ghe.com"));
    assert_eq!(acme, site("http://acme.ghe.com"));
    assert!(acme.is_enterprise());
    assert_eq!(acme.name(), "acme.ghe.com");
    assert_eq!(
        acme.device_code_url(),
        "https://acme.ghe.com/login/device/code"
    );
    assert_eq!(
        acme.token_url(),
        "https://acme.ghe.com/login/oauth/access_token"
    );
    assert_eq!(acme.device_page(), "https://acme.ghe.com/login/device");
    assert_eq!(
        acme.session_url(),
        "https://api.acme.ghe.com/copilot_internal/v2/token"
    );
    assert_eq!(
        acme.copilot_user_url(),
        "https://api.acme.ghe.com/copilot_internal/user"
    );
    assert_eq!(acme.user_url(), "https://api.acme.ghe.com/user");
    assert_eq!(acme.api_base(), "https://copilot-api.acme.ghe.com");

    assert_eq!(site("a.ghe.com").name(), "a.ghe.com");
    assert_eq!(site("a-1.ghe.com").name(), "a-1.ghe.com");
    let longest = format!("{}.ghe.com", "a".repeat(63));
    assert_eq!(site(&longest).name(), longest);
    for bad in [
        "evil.example",
        "ghe.com",
        ".ghe.com",
        "-acme.ghe.com",
        "acme-.ghe.com",
        "a.b.ghe.com",
        "acme_x.ghe.com",
        "acme.ghe.com.evil.example",
        "ftp://acme.ghe.com",
        "https://acme.ghe.com:8443",
        "https://user@acme.ghe.com",
        "https://acme.ghe.com/path",
        "https://acme.ghe.com?q",
        "https://acme.ghe.com#f",
        "https://",
        "acme.ghe.com/",
        "api.github.com",
    ] {
        let error = Site::parse(bad).expect_err(bad);
        assert_eq!(error.kind(), PluginErrorKind::Config, "{bad}");
    }
    assert!(Site::parse(&format!("{}.ghe.com", "a".repeat(64))).is_err());
}

#[test]
fn untrusted_endpoints_api_rejected() {
    let github = Site::github();
    let acme = site("acme.ghe.com");
    for (good, base) in [
        (
            "https://api.githubcopilot.com",
            "https://api.githubcopilot.com",
        ),
        (
            "https://API.Individual.GitHubCopilot.com/",
            "https://api.individual.githubcopilot.com",
        ),
        ("https://githubcopilot.com", "https://githubcopilot.com"),
    ] {
        assert_eq!(github.trusted_api(good).as_deref(), Some(base), "{good}");
        assert_eq!(acme.trusted_api(good).as_deref(), Some(base), "{good}");
    }
    assert_eq!(
        acme.trusted_api("https://copilot-api.acme.ghe.com")
            .as_deref(),
        Some("https://copilot-api.acme.ghe.com")
    );
    assert_eq!(github.trusted_api("https://copilot-api.acme.ghe.com"), None);
    assert_eq!(github.trusted_api("https://copilot-api.github.com"), None);
    for bad in [
        "http://api.githubcopilot.com",
        "https://evilgithubcopilot.com",
        "https://.githubcopilot.com",
        "https://githubcopilot.com.evil.example",
        "https://api.githubcopilot.com:444",
        "https://user@api.githubcopilot.com",
        "https://api.githubcopilot.com/v1",
        "https://api.githubcopilot.com?x",
        "https://api.githubcopilot.com#x",
        "https://api.githubcopilot.com\\@evil.example",
        "https://copilot-api.other.ghe.com",
        "https://api.acme.ghe.com",
        "https://",
        "api.githubcopilot.com",
    ] {
        assert_eq!(acme.trusted_api(bad), None, "{bad}");
    }

    let reply =
        br#"{"token":"tid=1","expires_at":3600,"endpoints":{"api":"https://evil.example"}}"#;
    let error = session::parse_session(&github, reply).expect_err("untrusted");
    assert_eq!(error.kind(), PluginErrorKind::Auth);
    let (session, expires) =
        session::parse_session(&acme, br#"{"token":"tid=1","expires_at":3600}"#)
            .expect("default base");
    assert_eq!(session.base, "https://copilot-api.acme.ghe.com");
    assert_eq!(expires, Some(UNIX_EPOCH + Duration::from_secs(3600)));
    let (session, expires) =
        session::parse_session(&github, br#"{"token":"tid=1","endpoints":{"api":""}}"#)
            .expect("empty api");
    assert_eq!(session.base, "https://api.githubcopilot.com");
    assert_eq!(expires, Some(UNIX_EPOCH));
    for empty in [&br#"{"token":""}"#[..], br#"{"error":"nope"}"#] {
        let error = session::parse_session(&github, empty).expect_err("no token");
        assert_eq!(error.kind(), PluginErrorKind::Auth);
    }
    let error = session::parse_session(&github, b"[]").expect_err("not an object");
    assert_eq!(error.kind(), PluginErrorKind::Decode);
}

#[test]
fn github_calls_use_the_token_scheme() {
    assert_eq!(
        session::github_authorization(&secret("gho-1")),
        "token gho-1"
    );
    let dir = tempfile::tempdir().expect("dir");
    let host = host(&dir);
    let id = seed(&host);
    host.http()
        .enqueue(br#"{"copilot_plan":"individual"}"#.to_vec());
    let sub = subscription(&host);
    block_on(sub.quota(&id, host.context())).expect("quota");
    let calls = host.http().calls();
    assert_eq!(calls[0].method(), "GET");
    assert_eq!(calls[0].sensitive_headers(), ["authorization".to_owned()]);
}

#[test]
fn device_sign_in_on_github_com() {
    let dir = tempfile::tempdir().expect("dir");
    let host = host(&dir);
    let sub = subscription(&host);
    host.http().enqueue(device_reply());
    let challenge = block_on(sub.start_sign_in(&ctx(), host.context())).expect("start");
    let shown = challenge_url(&challenge);
    assert!(shown.contains("ABCD-1234"), "{shown}");
    assert!(shown.contains("https://github.com/login/device"), "{shown}");
    assert!(!shown.contains("evil.example"), "{shown}");
    let start = &host.http().calls()[0];
    assert_eq!(start.method(), "POST");
    let form = String::from_utf8_lossy(start.body()).into_owned();
    assert!(form.contains("client_id=Iv1.b507a08c87ecfe98"), "{form}");
    assert!(form.contains("scope=read%3Auser"), "{form}");

    host.http().enqueue(token_reply());
    host.http().enqueue(user_reply());
    host.http()
        .enqueue(br#"{"copilot_plan":"individual"}"#.to_vec());
    let id = block_on(sub.complete_sign_in(&ctx(), SignInProof::Await, host.context()))
        .expect("signed in");
    assert_eq!(id, account_id_from(&["github.com", "42"]));
    assert_eq!(
        uris(&host),
        vec![
            "https://github.com/login/device/code",
            "https://github.com/login/oauth/access_token",
            "https://api.github.com/user",
            "https://api.github.com/copilot_internal/user",
        ]
    );
    let poll = String::from_utf8_lossy(host.http().calls()[1].body()).into_owned();
    assert!(poll.contains("device_code=dev-1"), "{poll}");
    let blob = stored(&host, &id);
    assert_eq!(blob.text("host"), Some("github.com"));
    assert_eq!(blob.text("login"), Some("octo"));
    assert_eq!(blob.number("v"), Some(1));
    assert!(blob.secret("oauth_token").is_some());
    let accounts = block_on(host.context().accounts().list()).expect("list");
    assert_eq!(accounts.len(), 1);
    assert_eq!(accounts[0].label(), "octo");
    let status = block_on(sub.account_status(&id, host.context())).expect("status");
    assert_eq!(status, AccountStatus::Ready);
}

#[test]
fn slow_down_adds_five_seconds() {
    let dir = tempfile::tempdir().expect("dir");
    let host = host(&dir);
    let sub = subscription(&host);
    let ctx = ctx().with_option("host", "acme.ghe.com").expect("option");
    host.http().enqueue(device_reply());
    let challenge = block_on(sub.start_sign_in(&ctx, host.context())).expect("start");
    assert!(challenge_url(&challenge).contains("https://acme.ghe.com/login/device"));
    host.http()
        .enqueue_response(400, &[], br#"{"error":"slow_down"}"#.to_vec());
    host.http().enqueue(token_reply());
    host.http().enqueue(br#"{"login":"octo"}"#.to_vec());
    host.http()
        .enqueue(br#"{"copilot_plan":"business"}"#.to_vec());
    let began = Instant::now();
    let id = block_on(sub.complete_sign_in(&ctx, SignInProof::Await, host.context()))
        .expect("signed in");
    // One second, then six after slow_down.
    assert!(
        began.elapsed() >= Duration::from_secs(7),
        "{:?}",
        began.elapsed()
    );
    assert_eq!(id, account_id_from(&["acme.ghe.com", "octo"]));
    assert_eq!(
        uris(&host),
        vec![
            "https://acme.ghe.com/login/device/code",
            "https://acme.ghe.com/login/oauth/access_token",
            "https://acme.ghe.com/login/oauth/access_token",
            "https://api.acme.ghe.com/user",
            "https://api.acme.ghe.com/copilot_internal/user",
        ]
    );
    let accounts = block_on(host.context().accounts().list()).expect("list");
    assert_eq!(accounts[0].label(), "octo@acme.ghe.com");
    assert_eq!(stored(&host, &id).text("host"), Some("acme.ghe.com"));
}

#[test]
fn user_without_copilot_is_refused() {
    for status in [401, 403, 404] {
        let dir = tempfile::tempdir().expect("dir");
        let host = host(&dir);
        let sub = subscription(&host);
        host.http().enqueue(device_reply());
        block_on(sub.start_sign_in(&ctx(), host.context())).expect("start");
        host.http().enqueue(token_reply());
        host.http().enqueue(user_reply());
        host.http().enqueue_response(status, &[], b"{}".to_vec());
        let error = block_on(sub.complete_sign_in(&ctx(), SignInProof::Await, host.context()))
            .expect_err("no seat");
        assert_eq!(error.kind(), PluginErrorKind::Auth);
        assert_eq!(error.message(), "octo has no Copilot subscription");
        assert!(
            block_on(host.context().accounts().list())
                .expect("list")
                .is_empty()
        );
    }
}

/// A user reply, an optional seat reply, and the error kind expected.
type SignInCase = (Vec<u8>, Option<(u16, Vec<u8>)>, PluginErrorKind);

#[test]
fn sign_in_needs_a_login_and_a_seat_reply() {
    let cases: [SignInCase; 3] = [
        (br#"{"id":42}"#.to_vec(), None, PluginErrorKind::Decode),
        (
            br#"{"login":"  ","id":42}"#.to_vec(),
            None,
            PluginErrorKind::Decode,
        ),
        (
            user_reply(),
            Some((500, b"{}".to_vec())),
            PluginErrorKind::Status,
        ),
    ];
    for (user, seat, kind) in cases {
        let dir = tempfile::tempdir().expect("dir");
        let host = host(&dir);
        let sub = subscription(&host);
        host.http().enqueue(device_reply());
        block_on(sub.start_sign_in(&ctx(), host.context())).expect("start");
        host.http().enqueue(token_reply());
        host.http().enqueue(user);
        if let Some((status, body)) = seat {
            host.http().enqueue_response(status, &[], body);
        }
        let error = block_on(sub.complete_sign_in(&ctx(), SignInProof::Await, host.context()))
            .expect_err("refused");
        assert_eq!(error.kind(), kind);
        assert!(
            block_on(host.context().accounts().list())
                .expect("list")
                .is_empty()
        );
    }
}

#[test]
fn sign_in_session_rules() {
    let dir = tempfile::tempdir().expect("dir");
    let host = host(&dir);
    let sub = subscription(&host);
    let error = block_on(sub.complete_sign_in(&ctx(), SignInProof::Await, host.context()))
        .expect_err("nothing pending");
    assert_eq!(error.kind(), PluginErrorKind::Config);

    let bad = ctx().with_option("host", "evil.example").expect("option");
    let error = block_on(sub.start_sign_in(&bad, host.context())).expect_err("bad host");
    assert_eq!(error.kind(), PluginErrorKind::Config);
    assert!(host.http().calls().is_empty());

    host.http().enqueue(device_reply());
    block_on(sub.start_sign_in(&ctx(), host.context())).expect("start");
    let error =
        block_on(sub.complete_sign_in(&ctx(), SignInProof::Token(secret("pat")), host.context()))
            .expect_err("wrong proof");
    assert_eq!(error.kind(), PluginErrorKind::Config);

    host.http().enqueue(device_reply());
    block_on(sub.start_sign_in(&ctx(), host.context())).expect("start");
    block_on(sub.cancel_sign_in(&ctx(), host.context())).expect("cancel");
    let error = block_on(sub.complete_sign_in(&ctx(), SignInProof::Await, host.context()))
        .expect_err("cancelled");
    assert_eq!(error.kind(), PluginErrorKind::Config);
    assert_eq!(host.http().calls().len(), 2);
}

#[test]
fn session_single_flight() {
    let dir = tempfile::tempdir().expect("dir");
    let host = host(&dir);
    let id = seed(&host);
    host.http().enqueue(session_reply("tid=1", 3600));
    let cache = TokenCache::<Session>::new(MARGIN);
    let results = block_on(join_all(vec![
        Box::pin(session::session(&cache, host.context(), &id)),
        Box::pin(session::session(&cache, host.context(), &id)),
        Box::pin(session::session(&cache, host.context(), &id)),
    ]));
    for result in results {
        let session: Session = result.expect("session");
        assert_eq!(session.base, API);
    }
    assert_eq!(
        uris(&host),
        vec!["https://api.github.com/copilot_internal/v2/token"]
    );
}

#[test]
fn session_traded_again_within_two_minutes_of_expiry() {
    // The test clock reads the epoch.
    for (expires_at, trades) in [(121, 1), (119, 2), (3600, 1)] {
        let dir = tempfile::tempdir().expect("dir");
        let host = host(&dir);
        let id = seed(&host);
        host.http().enqueue(session_reply("tid=1", expires_at));
        host.http().enqueue(session_reply("tid=2", expires_at));
        let cache = TokenCache::<Session>::new(MARGIN);
        block_on(session::session(&cache, host.context(), &id)).expect("first");
        block_on(session::session(&cache, host.context(), &id)).expect("second");
        assert_eq!(host.http().calls().len(), trades, "expires_at {expires_at}");
    }
}

#[test]
fn refresh_trades_again() {
    let dir = tempfile::tempdir().expect("dir");
    let host = host(&dir);
    let id = seed(&host);
    let sub = subscription(&host);
    host.http().enqueue(session_reply("tid=1", 3600));
    host.http().enqueue(session_reply("tid=2", 3600));
    block_on(sub.refresh(&id, host.context())).expect("refresh");
    block_on(sub.refresh(&id, host.context())).expect("refresh again");
    assert_eq!(host.http().calls().len(), 2);
}

#[test]
fn token_trade_401_needs_sign_in() {
    for status in [401, 403] {
        let dir = tempfile::tempdir().expect("dir");
        let host = host(&dir);
        let id = seed(&host);
        let sub = subscription(&host);
        host.http().enqueue_response(status, &[], b"{}".to_vec());
        let error = block_on(sub.refresh(&id, host.context())).expect_err("refused");
        assert_eq!(error.kind(), PluginErrorKind::Auth);
        assert_eq!(error.message(), LAPSED_REASON);
        let blob = stored(&host, &id);
        assert!(blob.flag(LAPSED));
        assert!(blob.secret("oauth_token").is_some());
        let status = block_on(sub.account_status(&id, host.context())).expect("status");
        assert_eq!(
            status,
            AccountStatus::NeedsSignIn {
                reason: LAPSED_REASON.to_owned()
            }
        );
        let quota = block_on(sub.quota(&id, host.context())).expect_err("lapsed");
        assert_eq!(quota.kind(), PluginErrorKind::Auth);
        assert_eq!(host.http().calls().len(), 1);
    }
}

#[test]
fn replaced_token_is_not_marked_lapsed() {
    let dir = tempfile::tempdir().expect("dir");
    let host = host(&dir);
    let id = seed(&host);
    let sub = subscription(&host);
    // The trade sent gho-1; by the time GitHub refused it, a new sign-in
    // stored another token.
    block_on(session::lapse(host.context(), &id, &secret("gho-old"))).expect("lapse");
    assert!(!stored(&host, &id).flag(LAPSED));
    let status = block_on(sub.account_status(&id, host.context())).expect("status");
    assert_eq!(status, AccountStatus::Ready);
    block_on(session::lapse(host.context(), &id, &secret("gho-1"))).expect("lapse");
    assert!(stored(&host, &id).flag(LAPSED));
}

#[test]
fn server_error_keeps_the_sign_in() {
    let dir = tempfile::tempdir().expect("dir");
    let host = host(&dir);
    let id = seed(&host);
    let sub = subscription(&host);
    host.http().enqueue_response(500, &[], b"busy".to_vec());
    let error = block_on(sub.refresh(&id, host.context())).expect_err("500");
    assert_eq!(error.status(), Some(500));
    assert!(!stored(&host, &id).flag(LAPSED));
    let status = block_on(sub.account_status(&id, host.context())).expect("status");
    assert_eq!(status, AccountStatus::Ready);
}

#[test]
fn stored_blob_problems_need_sign_in() {
    let dir = tempfile::tempdir().expect("dir");
    let host = host(&dir);
    let sub = subscription(&host);
    let id = AccountId::from_static("a-0000000000000002");
    for blob in [
        SecretObject::new().with_number("v", 1).to_secret(),
        SecretObject::new()
            .with_secret("oauth_token", &secret("gho-1"))
            .with_text("host", "evil.example")
            .to_secret(),
        secret("not json"),
    ] {
        block_on(async {
            let accounts = host.context().accounts();
            let account = accounts.account(id.clone(), "octo").expect("account");
            accounts.upsert(account, blob).await.expect("upsert");
        });
        let status = block_on(sub.account_status(&id, host.context())).expect("status");
        assert!(
            matches!(status, AccountStatus::NeedsSignIn { .. }),
            "{status:?}"
        );
        let error = block_on(sub.refresh(&id, host.context())).expect_err("refresh");
        assert_eq!(error.kind(), PluginErrorKind::Auth);
    }
    assert!(host.http().calls().is_empty());
    // A blob without a host is a github.com sign-in.
    let blob = SecretObject::new().with_secret("oauth_token", &secret("gho-1"));
    let read = session::read_blob(&blob.to_secret()).expect("read");
    assert_eq!(read.site, Site::github());
}

fn upstream_for(host: &TestHost, id: &AccountId) -> CopilotUpstream {
    upstream_with(host, id, Arc::new(Book::default()))
}

fn upstream_with(host: &TestHost, id: &AccountId, book: Arc<Book>) -> CopilotUpstream {
    CopilotUpstream::new(
        id.clone(),
        host.context().clone(),
        Arc::new(TokenCache::new(MARGIN)),
        book,
    )
}

/// A `/models` reply with no models: every model goes to Chat Completions.
fn no_models() -> Vec<u8> {
    br#"{"data":[]}"#.to_vec()
}

#[test]
fn chat_401_trades_again_once() {
    let dir = tempfile::tempdir().expect("dir");
    let host = host(&dir);
    let id = seed(&host);
    host.http().enqueue(session_reply("tid=1", 3600));
    host.http().enqueue(no_models());
    host.http().enqueue_response(401, &[], b"{}".to_vec());
    host.http().enqueue(session_reply("tid=2", 3600));
    host.http().enqueue_response(401, &[], b"{}".to_vec());
    let error = block_on(upstream_for(&host, &id).execute(chat_request(), ProtocolId::OPENAI_CHAT))
        .err()
        .expect("second 401 is returned");
    assert_eq!(error.status(), Some(401));
    let token = "https://api.github.com/copilot_internal/v2/token".to_owned();
    let chat = format!("{API}/chat/completions");
    let listed = format!("{API}/models");
    assert_eq!(
        uris(&host),
        vec![token.clone(), listed, chat.clone(), token, chat]
    );
    let chat_call = &host.http().calls()[2];
    assert!(
        chat_call
            .sensitive_headers()
            .contains(&"authorization".to_owned())
    );
}

#[test]
fn other_chat_errors_are_not_retried() {
    let dir = tempfile::tempdir().expect("dir");
    let host = host(&dir);
    let id = seed(&host);
    host.http().enqueue(session_reply("tid=1", 3600));
    host.http().enqueue(no_models());
    host.http().enqueue_response(403, &[], b"{}".to_vec());
    let error = block_on(upstream_for(&host, &id).execute(chat_request(), ProtocolId::OPENAI_CHAT))
        .err()
        .expect("403");
    assert_eq!(error.status(), Some(403));
    assert_eq!(host.http().calls().len(), 3);
}

#[test]
fn request_headers_name_the_editor_and_initiator() {
    let header = |request: &Request, name: &str| {
        request_headers(request)
            .into_iter()
            .find(|(key, _)| key == name)
            .map(|(_, value)| value)
    };
    let typed = chat_request();
    assert_eq!(initiator(&typed), "user");
    assert_eq!(header(&typed, "X-Initiator").as_deref(), Some("user"));
    assert_eq!(
        header(&typed, "Editor-Version").as_deref(),
        Some("vscode/1.140.0")
    );
    assert_eq!(
        header(&typed, "Editor-Plugin-Version").as_deref(),
        Some("copilot-chat/0.68.0")
    );
    assert_eq!(
        header(&typed, "Copilot-Integration-Id").as_deref(),
        Some("vscode-chat")
    );
    assert_eq!(
        header(&typed, "User-Agent").as_deref(),
        Some("GitHubCopilotChat/0.68.0")
    );
    assert_eq!(
        header(&typed, "X-GitHub-Api-Version").as_deref(),
        Some("2026-01-09")
    );
    assert_eq!(
        header(&typed, "Openai-Intent").as_deref(),
        Some("conversation-panel")
    );
    assert_eq!(header(&typed, "Copilot-Vision-Request"), None);
    assert!(!has_image(&typed));

    let mut tool_result = chat_request();
    tool_result.messages[0].content = vec![part(json!({
        "type": "tool_result",
        "id": "call-1",
        "content": [{"type": "text", "text": "done"}]
    }))];
    assert_eq!(initiator(&tool_result), "agent");
    assert!(!has_image(&tool_result));

    let mut assistant_last = chat_request();
    let mut reply = assistant_last.messages[0].clone();
    reply.role = Role::Assistant;
    assistant_last.messages.push(reply.clone());
    assert_eq!(initiator(&assistant_last), "agent");
    reply.role = Role::Tool;
    assistant_last.messages.push(reply);
    assert_eq!(initiator(&assistant_last), "agent");
    let mut empty = chat_request();
    empty.messages.clear();
    assert_eq!(initiator(&empty), "agent");

    let image =
        json!({"type": "image", "source": {"type": "url", "url": "https://img.test/a.png"}});
    let mut seen = chat_request();
    seen.messages[0].content.push(part(image.clone()));
    assert!(has_image(&seen));
    assert_eq!(initiator(&seen), "user");
    assert_eq!(
        header(&seen, "Copilot-Vision-Request").as_deref(),
        Some("true")
    );
    let mut nested = chat_request();
    nested.messages[0].content = vec![part(json!({
        "type": "tool_result", "id": "call-1", "content": [image]
    }))];
    assert!(has_image(&nested));
}

fn window<'a>(windows: &'a [QuotaWindow], name: &str) -> &'a QuotaWindow {
    windows
        .iter()
        .find(|window| window.name() == name)
        .unwrap_or_else(|| panic!("{name} in {windows:?}"))
}

fn at(secs: u64) -> Option<SystemTime> {
    Some(UNIX_EPOCH + Duration::from_secs(secs))
}

#[test]
fn quota_reads_snapshots() {
    let body = json!({
        "copilot_plan": "individual",
        "quota_reset_date_utc": "2026-11-01T00:00:00Z",
        "quota_snapshots": {
            "chat": {"unlimited": true},
            "completions": {"entitlement": 2000, "quota_remaining": "1500"},
            "premium_interactions": {"entitlement": 300, "quota_remaining": 280.4, "quota_reset_at": 1_800_000_000}
        }
    });
    let windows = quota::windows(body.to_string().as_bytes()).expect("windows");
    let names: Vec<&str> = windows.iter().map(QuotaWindow::name).collect();
    assert_eq!(names, ["Completions", "Premium requests", "Chat requests"]);
    let completions = window(&windows, "Completions");
    assert_eq!((completions.used(), completions.limit()), (500, Some(2000)));
    assert!(!completions.blocking());
    assert_eq!(completions.resets_at(), at(1_793_491_200));
    let premium = window(&windows, "Premium requests");
    assert_eq!((premium.used(), premium.limit()), (20, Some(300)));
    assert!(premium.blocking());
    assert_eq!(premium.resets_at(), at(1_800_000_000));
    let chat = window(&windows, "Chat requests");
    assert_eq!(
        (chat.used(), chat.limit(), chat.resets_at()),
        (0, None, None)
    );
    assert!(!chat.blocking());
}

#[test]
fn quota_falls_back_and_skips() {
    let body = json!({
        "quota_reset_date": "2026-11-01",
        "quota_snapshots": {
            "chat": {"entitlement": 50, "percent_remaining": 75.0},
            "completions": {"entitlement": 0, "quota_remaining": 0},
            "premium_interactions": {"entitlement": 10}
        }
    });
    let windows = quota::windows(body.to_string().as_bytes()).expect("windows");
    assert_eq!(windows.len(), 1, "{windows:?}");
    let chat = window(&windows, "Chat requests");
    assert_eq!((chat.used(), chat.limit()), (25, Some(100)));
    assert!(chat.blocking());
    assert_eq!(chat.resets_at(), at(1_793_491_200));

    let body = json!({"quota_snapshots": {
        "chat": {"entitlement": 10, "quota_remaining": 15},
        "completions": {"entitlement": 10, "quota_remaining": -5},
        "premium_interactions": {"entitlement": "x", "quota_remaining": 1}
    }});
    let windows = quota::windows(body.to_string().as_bytes()).expect("windows");
    assert_eq!(windows.len(), 2);
    assert_eq!(window(&windows, "Chat requests").used(), 0);
    assert_eq!(window(&windows, "Completions").used(), 10);
    assert_eq!(window(&windows, "Chat requests").resets_at(), None);

    let body = json!({"quota_snapshots": {"chat": {"entitlement": 10, "percent_remaining": 140}}});
    let windows = quota::windows(body.to_string().as_bytes()).expect("windows");
    assert_eq!(windows[0].used(), 0);
    let body = json!({"quota_snapshots": {"chat": {"entitlement": 10, "percent_remaining": -1}}});
    let windows = quota::windows(body.to_string().as_bytes()).expect("windows");
    assert_eq!(windows[0].used(), 100);
    assert!(quota::windows(b"{}").expect("empty").is_empty());
    assert_eq!(
        quota::windows(b"null").expect_err("not an object").kind(),
        PluginErrorKind::Decode
    );
}

#[test]
fn premium_exhausted_blocks_unless_overage() {
    // (plan, overage, has_quota false on unlimited, blocking)
    for (plan, overage, blocking) in [
        ("individual", false, true),
        ("individual", true, false),
        ("business", true, true),
        ("Enterprise", true, true),
        ("enterprise", false, true),
    ] {
        let limited = json!({
            "copilot_plan": plan,
            "quota_snapshots": {"premium_interactions": {
                "entitlement": 300, "quota_remaining": 0, "overage_permitted": overage
            }}
        });
        let windows = quota::windows(limited.to_string().as_bytes()).expect("windows");
        let premium = window(&windows, "Premium requests");
        assert_eq!(premium.used(), 300);
        assert_eq!(premium.blocking(), blocking, "{plan} {overage}");

        let exhausted = json!({
            "copilot_plan": plan,
            "quota_reset_date_utc": "2026-11-01T00:00:00Z",
            "quota_snapshots": {"premium_interactions": {
                "unlimited": true, "has_quota": false, "overage_permitted": overage
            }}
        });
        let windows = quota::windows(exhausted.to_string().as_bytes()).expect("windows");
        let premium = window(&windows, "Premium requests");
        assert_eq!((premium.used(), premium.limit()), (100, Some(100)));
        assert_eq!(premium.blocking(), blocking, "{plan} {overage}");
        assert_eq!(premium.resets_at(), at(1_793_491_200));
    }
    let open = json!({"quota_snapshots": {"premium_interactions": {
        "unlimited": true, "has_quota": true
    }}});
    let windows = quota::windows(open.to_string().as_bytes()).expect("windows");
    assert_eq!(windows[0].limit(), None);
    assert!(!windows[0].blocking());
    let chat_exhausted = json!({"quota_snapshots": {"chat": {
        "unlimited": true, "has_quota": false
    }}});
    let windows = quota::windows(chat_exhausted.to_string().as_bytes()).expect("windows");
    assert_eq!(windows[0].limit(), None);
}

#[test]
fn quota_reads_the_account_host() {
    let dir = tempfile::tempdir().expect("dir");
    let host = host(&dir);
    let id = seed_on(&host, "acme.ghe.com", "gho-1");
    let sub = subscription(&host);
    host.http().enqueue(
        json!({"quota_snapshots": {"chat": {"entitlement": 10, "quota_remaining": 4}}})
            .to_string()
            .into_bytes(),
    );
    let windows = block_on(sub.quota(&id, host.context())).expect("quota");
    assert_eq!(windows[0].used(), 6);
    assert_eq!(
        uris(&host),
        vec!["https://api.acme.ghe.com/copilot_internal/user"]
    );
    for (status, kind) in [
        (401, PluginErrorKind::Auth),
        (403, PluginErrorKind::Auth),
        (502, PluginErrorKind::Status),
    ] {
        host.http().enqueue_response(status, &[], b"{}".to_vec());
        let error = block_on(sub.quota(&id, host.context())).expect_err("refused");
        assert_eq!(error.kind(), kind, "{status}");
    }
}

#[test]
fn reset_times_parse() {
    let parse = quota::parse_time;
    assert_eq!(parse("2026-11-01"), at(1_793_491_200));
    assert_eq!(parse("2026-11-01T00:00:00Z"), at(1_793_491_200));
    assert_eq!(parse("2026-11-01T00:00:00.000Z"), at(1_793_491_200));
    assert_eq!(parse("2024-02-29T12:30:15Z"), at(1_709_209_815));
    assert_eq!(parse("2024-02-29t12:30:15z"), at(1_709_209_815));
    assert_eq!(parse("2024-02-29T20:30:15+08:00"), at(1_709_209_815));
    assert_eq!(parse("2024-02-29T04:00:15-08:30"), at(1_709_209_815));
    assert_eq!(parse("2000-03-01"), at(951_868_800));
    assert_eq!(parse("1970-01-01"), at(0));
    for bad in [
        "",
        "2026-11-0",
        "2026/11/01",
        "2026-13-01",
        "2026-00-01",
        "2026-02-29",
        "2026-11-00",
        "2026-11-31",
        "1969-12-31",
        "2026-11-01X",
        "2026-11-01T24:00:00Z",
        "2026-11-01T00:60:00Z",
        "2026-11-01T00:00:61Z",
        "2026-11-01T00-00-00Z",
        "2026-11-01T00:00:00",
        "2026-11-01T00:00:00.Z",
        "2026-11-01T00:00:00+0800",
        "2026-11-01T00:00:00+24:00",
        "2026-11-01T00:00:00+08:60",
        "2026-11-01T00:00:00*08:00",
        "+026-11-01",
    ] {
        assert_eq!(parse(bad), None, "{bad}");
    }
    assert_eq!(parse("2100-03-01"), at(4_107_542_400));
    assert_eq!(parse("2000-02-29"), at(951_782_400));
    assert_eq!(parse("2100-02-29"), None);
}

#[test]
fn no_token_in_debug_or_errors() {
    const SENTINEL: &str = "SENTINEL-6d1f";
    let dir = tempfile::tempdir().expect("dir");
    let host = host(&dir);
    let id = seed_on(&host, "github.com", SENTINEL);
    let sub = subscription(&host);
    host.http()
        .enqueue_response(500, &[], format!("upstream echoed {SENTINEL}").into_bytes());
    let error = block_on(upstream_for(&host, &id).execute(chat_request(), ProtocolId::OPENAI_CHAT))
        .err()
        .expect("500");
    let (session, _) =
        session::parse_session(&Site::github(), &session_reply(SENTINEL, 1)).expect("session");
    let status = block_on(sub.account_status(&id, host.context())).expect("status");
    let shown = format!(
        "{error:?} {error} {session:?} {status:?} {:?}",
        stored(&host, &id)
    );
    assert!(!shown.contains(SENTINEL), "{shown}");
}

fn models_reply() -> Vec<u8> {
    json!({"data": [
        {"id": "claude-sonnet", "name": "Claude Sonnet", "model_picker_enabled": true,
         "capabilities": {"type": "chat"},
         "supported_endpoints": ["/v1/messages", "/chat/completions"]},
        {"id": "gpt-5", "name": "GPT-5", "model_picker_enabled": true,
         "capabilities": {"type": "chat", "supports": {"reasoning_effort": ["low", "high"]}},
         "supported_endpoints": ["/responses", "/chat/completions", "ws:/responses"]},
        {"id": "gpt-4.1", "model_picker_enabled": true, "capabilities": {"type": "chat"}},
        {"id": "o-terms", "model_picker_enabled": true, "capabilities": {"type": "chat"},
         "supported_endpoints": ["/responses"],
         "policy": {"state": "unconfigured", "terms": "Enable access to o-terms."}},
        {"id": "hidden-picked", "model_picker_category": "powerful",
         "capabilities": {"type": "chat"},
         "policy": {"state": "enabled", "terms": ""}},
        {"id": "trajectory-1", "model_picker_enabled": true, "capabilities": {"type": "chat"},
         "supported_endpoints": ["/responses"]}
    ]})
    .to_string()
    .into_bytes()
}

fn request_for(model: &str, protocol: ProtocolId) -> Request {
    let mut request = ProbeQuery::new(protocol, model)
        .expect("query")
        .minimal_request();
    request.stream = false;
    request
}

#[test]
fn endpoints_select_protocol() {
    let (_, _, listed) = models::parse(&models_reply()).expect("parse");
    let cases = [
        (
            "claude-sonnet",
            ProtocolId::ANTHROPIC_MESSAGES,
            ProtocolId::ANTHROPIC_MESSAGES,
        ),
        (
            "claude-sonnet",
            ProtocolId::OPENAI_CHAT,
            ProtocolId::OPENAI_CHAT,
        ),
        (
            "claude-sonnet",
            ProtocolId::OPENAI_RESPONSES,
            ProtocolId::ANTHROPIC_MESSAGES,
        ),
        (
            "gpt-5",
            ProtocolId::ANTHROPIC_MESSAGES,
            ProtocolId::OPENAI_RESPONSES,
        ),
        ("gpt-5", ProtocolId::OPENAI_CHAT, ProtocolId::OPENAI_CHAT),
        (
            "o-terms",
            ProtocolId::OPENAI_CHAT,
            ProtocolId::OPENAI_RESPONSES,
        ),
        // Internal models are not offered, but a call naming one still
        // goes to the API the list gave it.
        (
            "trajectory-1",
            ProtocolId::OPENAI_CHAT,
            ProtocolId::OPENAI_RESPONSES,
        ),
    ];
    for (model, inbound, expected) in cases {
        assert_eq!(
            listed.protocol(model, inbound),
            expected,
            "{model} {inbound:?}"
        );
    }
    assert_eq!(
        models::apis(&[
            "/v1/chat/completions/".to_owned(),
            "/v1/responses".to_owned(),
            "/messages".to_owned(),
            "/chat/completions".to_owned(),
            "/v1/embeddings".to_owned(),
        ]),
        [
            ProtocolId::OPENAI_CHAT,
            ProtocolId::OPENAI_RESPONSES,
            ProtocolId::ANTHROPIC_MESSAGES
        ]
    );

    // End to end: the call goes to the path of the chosen API.
    let dir = tempfile::tempdir().expect("dir");
    let host = host(&dir);
    let id = seed(&host);
    let upstream = upstream_for(&host, &id);
    host.http().enqueue(session_reply("tid=1", 3600));
    host.http().enqueue(models_reply());
    host.http().enqueue(
        br#"{"id":"r1","object":"response","status":"completed","model":"gpt-5","output":[]}"#
            .to_vec(),
    );
    block_on(upstream.execute(
        request_for("gpt-5", ProtocolId::ANTHROPIC_MESSAGES),
        ProtocolId::ANTHROPIC_MESSAGES,
    ))
    .expect("responses call");
    host.http().enqueue(
        br#"{"id":"m1","type":"message","role":"assistant","model":"claude-sonnet","content":[],"stop_reason":"end_turn","usage":{"input_tokens":1,"output_tokens":1}}"#
            .to_vec(),
    );
    block_on(upstream.execute(
        request_for("claude-sonnet", ProtocolId::ANTHROPIC_MESSAGES),
        ProtocolId::ANTHROPIC_MESSAGES,
    ))
    .expect("messages call");
    let calls = host.http().calls();
    assert_eq!(
        uris(&host),
        vec![
            "https://api.github.com/copilot_internal/v2/token".to_owned(),
            format!("{API}/models"),
            format!("{API}/responses"),
            format!("{API}/v1/messages"),
        ]
    );
    assert!(
        calls[1]
            .sensitive_headers()
            .contains(&"authorization".to_owned())
    );
    assert_eq!(calls[1].method(), "GET");
}

#[test]
fn no_endpoints_means_chat() {
    assert_eq!(models::apis(&[]), [ProtocolId::OPENAI_CHAT]);
    let (models, _, listed) = models::parse(&models_reply()).expect("parse");
    let plain = models
        .iter()
        .find(|model| model.id() == "gpt-4.1")
        .expect("listed");
    assert_eq!(plain.endpoints(), ["chat"]);
    assert_eq!(
        listed.protocol("gpt-4.1", ProtocolId::ANTHROPIC_MESSAGES),
        ProtocolId::OPENAI_CHAT
    );
    // A model the list never named goes to Chat Completions too.
    assert_eq!(
        listed.protocol("unknown", ProtocolId::OPENAI_RESPONSES),
        ProtocolId::OPENAI_CHAT
    );

    // A list that fails leaves the call on Chat Completions, and the next
    // call asks for the list again.
    let dir = tempfile::tempdir().expect("dir");
    let host = host(&dir);
    let id = seed(&host);
    let upstream = upstream_for(&host, &id);
    host.http().enqueue(session_reply("tid=1", 3600));
    host.http().enqueue_response(500, &[], b"down".to_vec());
    host.http().enqueue_response(403, &[], b"{}".to_vec());
    host.http().enqueue(no_models());
    host.http().enqueue_response(403, &[], b"{}".to_vec());
    for _ in 0..2 {
        let error = block_on(upstream.execute(
            request_for("gpt-5", ProtocolId::OPENAI_RESPONSES),
            ProtocolId::OPENAI_RESPONSES,
        ))
        .err()
        .expect("403");
        assert_eq!(error.status(), Some(403));
    }
    let chat = format!("{API}/chat/completions");
    let listed = format!("{API}/models");
    assert_eq!(
        uris(&host)[1..],
        [listed.clone(), chat.clone(), listed, chat]
    );
}

#[test]
fn internal_ids_dropped() {
    for id in [
        "copilot-search-1",
        "exec-agent",
        "trajectory-2",
        "gpt-5-mini-secondary",
        "claude-tertiary",
        "gpt-4th",
        "gpt-5-free-auto",
    ] {
        assert!(models::internal(id), "{id}");
    }
    for id in [
        "my-copilot-search",
        "gpt-4th-gen",
        "secondary-model",
        "gpt-5",
    ] {
        assert!(!models::internal(id), "{id}");
    }
    let body = json!({"data": [
        {"id": "embed", "model_picker_enabled": true, "capabilities": {"type": "embeddings"}},
        {"id": "exp", "vendor": "Experimental", "model_picker_enabled": true, "capabilities": {"type": "chat"}},
        {"id": "snapshot", "capabilities": {"type": "chat"}},
        {"id": "seatless", "model_picker_enabled": true, "capabilities": {"type": "chat"},
         "policy": {"state": "disabled"}},
        {"id": "gpt-5-mini-secondary", "model_picker_enabled": true, "capabilities": {"type": "chat"}},
        {"id": "", "model_picker_enabled": true, "capabilities": {"type": "chat"}},
        {"id": "kept", "model_picker_enabled": true, "capabilities": {"type": "chat"}}
    ]})
    .to_string();
    let (models, dropped, listed) = models::parse(body.as_bytes()).expect("parse");
    let ids: Vec<&str> = models.iter().map(|model| model.id()).collect();
    assert_eq!(ids, ["kept"]);
    assert_eq!(
        dropped,
        [
            "embed",
            "exp",
            "snapshot",
            "seatless",
            "gpt-5-mini-secondary"
        ]
    );
    assert!(!listed.waits("seatless"));
    assert!(models::parse(b"not json").is_err());
    assert!(models::parse(br#"{"models":[]}"#).is_err());
}

#[test]
fn list_models_reads_the_account_list() {
    let dir = tempfile::tempdir().expect("dir");
    let host = host(&dir);
    let id = seed(&host);
    let sub = subscription(&host);
    host.http().enqueue(session_reply("tid=1", 3600));
    host.http().enqueue(models_reply());
    let list = block_on(sub.list_models(&id, host.context())).expect("list");
    let ids: Vec<&str> = list.models().iter().map(|model| model.id()).collect();
    assert_eq!(
        ids,
        [
            "claude-sonnet",
            "gpt-5",
            "gpt-4.1",
            "o-terms",
            "hidden-picked"
        ]
    );
    assert_eq!(list.dropped(), ["trajectory-1"]);
    let gpt5 = &list.models()[1];
    assert_eq!(gpt5.display_name(), Some("GPT-5"));
    assert_eq!(gpt5.endpoints(), ["responses", "chat"]);
    assert_eq!(gpt5.efforts(), ["low", "high"]);
    assert_eq!(list.models()[0].endpoints(), ["anthropic", "chat"]);
    assert_eq!(uris(&host)[1], format!("{API}/models"));

    for (status, kind) in [
        (401, PluginErrorKind::Auth),
        (403, PluginErrorKind::Auth),
        (500, PluginErrorKind::Status),
    ] {
        host.http().enqueue_response(status, &[], b"{}".to_vec());
        let error = block_on(sub.list_models(&id, host.context())).expect_err("refused");
        assert_eq!(error.kind(), kind, "{status}");
    }
}

#[test]
fn terms_accepted_once() {
    let dir = tempfile::tempdir().expect("dir");
    let host = host(&dir);
    let id = seed(&host);
    let book = Arc::new(Book::default());
    let upstream = upstream_with(&host, &id, Arc::clone(&book));
    let reply =
        br#"{"id":"r1","object":"response","status":"completed","model":"o-terms","output":[]}"#;
    host.http().enqueue(session_reply("tid=1", 3600));
    host.http().enqueue(models_reply());
    // The first acceptance is refused: the call still goes out, and the
    // next call asks again.
    host.http().enqueue_response(403, &[], b"{}".to_vec());
    host.http().enqueue(reply.to_vec());
    host.http().enqueue_response(204, &[], Vec::new());
    host.http().enqueue(reply.to_vec());
    host.http().enqueue(reply.to_vec());
    for _ in 0..3 {
        block_on(upstream.execute(
            request_for("o-terms", ProtocolId::OPENAI_RESPONSES),
            ProtocolId::OPENAI_RESPONSES,
        ))
        .expect("call");
    }
    let policy = format!("{API}/models/o-terms/policy");
    let responses = format!("{API}/responses");
    assert_eq!(
        uris(&host)[2..],
        [
            policy.clone(),
            responses.clone(),
            policy,
            responses.clone(),
            responses
        ]
    );
    let calls = host.http().calls();
    assert_eq!(calls[2].method(), "POST");
    assert_eq!(calls[2].body(), br#"{"state":"enabled"}"#);
    assert!(
        calls[2]
            .sensitive_headers()
            .contains(&"authorization".to_owned())
    );
    let listed = book.get(&id).expect("listed");
    assert!(!listed.waits("o-terms"));
}

#[test]
fn policy_path_escapes_the_model() {
    let dir = tempfile::tempdir().expect("dir");
    let host = host(&dir);
    let id = seed(&host);
    let upstream = upstream_for(&host, &id);
    let body = json!({"data": [
        {"id": "a/b c", "model_picker_enabled": true, "capabilities": {"type": "chat"},
         "policy": {"state": "unconfigured", "terms": "t"}}
    ]})
    .to_string();
    host.http().enqueue(session_reply("tid=1", 3600));
    host.http().enqueue(body.into_bytes());
    host.http().enqueue(Vec::new());
    host.http().enqueue_response(403, &[], b"{}".to_vec());
    let _ = block_on(upstream.execute(
        request_for("a/b c", ProtocolId::OPENAI_CHAT),
        ProtocolId::OPENAI_CHAT,
    ));
    assert_eq!(uris(&host)[2], format!("{API}/models/a%2Fb%20c/policy"));
}
