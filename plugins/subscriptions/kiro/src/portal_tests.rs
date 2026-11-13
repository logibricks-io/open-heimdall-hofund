//! Browser sign-in tests with an injected loopback listener and recorded HTTP.

use std::sync::Arc;

use hofund_kit_subscription::{TestHost, block_on, loopback_get};
use hofund_plugin_api::{
    AccountId, DynSubscriptionFactory, PluginId, PluginKind, RawTable, SignInChallenge, SignInCtx,
    SignInProof, Subscription,
};
use hofund_plugin_sdk::json::{self, Value};
use hofund_plugin_sdk::oauth::{SecretObject, secret_text};
use hofund_plugin_sdk::url;

use crate::{Endpoints, factory_with_endpoints};

fn host(dir: &tempfile::TempDir) -> TestHost {
    TestHost::open_for(
        PluginKind::Subscription,
        PluginId::from_static("kiro"),
        dir.path(),
    )
}

fn endpoints() -> Endpoints {
    Endpoints {
        auth: "https://auth.example.test".to_owned(),
        management: "https://management.{region}.example.test".to_owned(),
        oidc: "https://oidc.{region}.example.test".to_owned(),
        callback_ports: vec![0],
        ..Endpoints::default()
    }
}

fn ctx(site: Option<&str>) -> SignInCtx {
    let mut ctx = SignInCtx::new(PluginId::from_static("kiro"))
        .with_session("0123456789abcdef0123456789abcdef")
        .expect("session");
    if let Some(site) = site {
        ctx = ctx.with_option("site", site).expect("site");
    }
    ctx
}

fn subscription(host: &TestHost) -> Arc<dyn Subscription> {
    factory_with_endpoints(endpoints())
        .validate_and_build(&RawTable::new(), host.context())
        .expect("build")
}

fn open_url(challenge: SignInChallenge) -> url::Url {
    let SignInChallenge::OpenUrl { url } = challenge else {
        panic!("expected browser challenge");
    };
    url::Url::parse(&url).expect("challenge URL")
}

fn port(url: &url::Url) -> u16 {
    url.query_pairs()
        .find_map(|(key, value)| {
            if key != "redirect_uri" {
                return None;
            }
            url::Url::parse(&value).ok()?.port_or_known_default()
        })
        .expect("loopback port")
}

fn base64url(bytes: &[u8]) -> String {
    const ALPHABET: &[u8; 64] = b"ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz0123456789-_";
    let mut out = String::new();
    for chunk in bytes.chunks(3) {
        let value = (u32::from(chunk[0]) << 16)
            | (u32::from(*chunk.get(1).unwrap_or(&0)) << 8)
            | u32::from(*chunk.get(2).unwrap_or(&0));
        out.push(ALPHABET[(value >> 18) as usize & 63] as char);
        out.push(ALPHABET[(value >> 12) as usize & 63] as char);
        if chunk.len() > 1 {
            out.push(ALPHABET[(value >> 6) as usize & 63] as char);
        }
        if chunk.len() > 2 {
            out.push(ALPHABET[value as usize & 63] as char);
        }
    }
    out
}

async fn stored(host: &TestHost, id: &AccountId) -> SecretObject {
    let secret = host
        .context()
        .accounts()
        .secret(id)
        .await
        .expect("read")
        .expect("stored");
    SecretObject::parse(&secret).expect("secret")
}

fn callback_target(path: &str, query: &[(&str, &str)]) -> String {
    let mut query_string = url::form_urlencoded::Serializer::new(String::new());
    query_string.extend_pairs(query.iter().copied());
    format!("{path}?{}", query_string.finish())
}

#[test]
fn r51b_default_callback_ports_match_the_reference() {
    assert_eq!(
        Endpoints::default().callback_ports,
        [
            3128, 4649, 6588, 8008, 9091, 49153, 50153, 51153, 52153, 53153
        ]
    );
}

#[test]
fn r51b_google_and_github_exchange_codes_with_pkce() {
    for (option, provider, way) in [
        ("google", "Google", "Google"),
        ("github", "Github", "GitHub"),
    ] {
        let dir = tempfile::tempdir().expect("fixture directory");
        let host = host(&dir);
        let sub = subscription(&host);
        let ctx = ctx(None);
        block_on(async {
            let page = open_url(
                sub.start_sign_in(&ctx, host.context())
                    .await
                    .expect("start"),
            );
            assert_eq!(page.origin().ascii_serialization(), "https://app.kiro.dev");
            assert_eq!(page.path(), "/signin");
            assert_eq!(
                page.query_pairs()
                    .find(|(key, _)| key == "code_challenge_method")
                    .map(|(_, value)| value.into_owned()),
                Some("S256".to_owned())
            );
            let state = page
                .query_pairs()
                .find(|(key, _)| key == "state")
                .map(|(_, value)| value.into_owned())
                .expect("state");
            let challenge = page
                .query_pairs()
                .find(|(key, _)| key == "code_challenge")
                .map(|(_, value)| value.into_owned())
                .expect("challenge");
            let wrong = callback_target(
                "/oauth/callback",
                &[
                    ("login_option", option),
                    ("code", "wrong"),
                    ("state", "wrong"),
                ],
            );
            assert_eq!(
                loopback_get(port(&page), &wrong)
                    .await
                    .expect("wrong state")
                    .0,
                404
            );
            assert!(host.http().calls().is_empty());

            host.http().enqueue(
                br#"{"accessToken":"social-access","refreshToken":"social-refresh","expiresIn":3600,"profileArn":"arn:aws:codewhisperer:us-east-1:1:profile/P"}"#.to_vec(),
            );
            host.http()
                .enqueue(br#"{"userInfo":{"email":"person@example.test"}}"#.to_vec());
            let target = callback_target(
                "/oauth/callback",
                &[
                    ("login_option", option),
                    ("code", "portal code"),
                    ("state", &state),
                ],
            );
            assert_eq!(
                loopback_get(port(&page), &target)
                    .await
                    .expect("callback")
                    .0,
                200
            );
            let id = sub
                .complete_sign_in(&ctx, SignInProof::Await, host.context())
                .await
                .expect("complete");
            let calls = host.http().calls();
            assert_eq!(calls.len(), 2);
            assert_eq!(calls[0].uri(), "https://auth.example.test/oauth/token");
            let body: Value = json::from_slice(calls[0].body()).expect("exchange body");
            assert_eq!(body["code"], "portal code");
            assert_eq!(
                body["redirect_uri"],
                format!(
                    "http://localhost:{}/oauth/callback?login_option={option}",
                    port(&page)
                )
            );
            let verifier = body["code_verifier"].as_str().expect("verifier");
            assert_eq!(
                base64url(&hofund_plugin_sdk::digest::sha256(verifier.as_bytes())),
                challenge
            );
            let stored = stored(&host, &id).await;
            assert_eq!(stored.text("provider"), Some(provider));
            assert_eq!(
                stored.text("label"),
                Some(format!("person@example.test ({way})").as_str())
            );
            assert_eq!(
                stored.secret("access_token").as_ref().map(secret_text),
                Some("social-access")
            );
        });
    }
}

#[test]
fn r51b_portal_builder_id_registers_authorizes_and_exchanges_at_aws() {
    let dir = tempfile::tempdir().expect("fixture directory");
    let host = host(&dir);
    let sub = subscription(&host);
    let ctx = ctx(None);
    block_on(async {
        let page = open_url(
            sub.start_sign_in(&ctx, host.context())
                .await
                .expect("start"),
        );
        let state = page
            .query_pairs()
            .find(|(key, _)| key == "state")
            .map(|(_, value)| value.into_owned())
            .expect("state");
        host.http().enqueue(
            br#"{"clientId":"registered-client","clientSecret":"registered-secret"}"#.to_vec(),
        );
        let portal = callback_target(
            "/signin/callback",
            &[
                ("login_option", "builderid"),
                ("issuer_url", "https://view.awsapps.com/start"),
                ("idc_region", "us-east-1"),
                ("state", &state),
            ],
        );
        let (status, reply) = loopback_get(port(&page), &portal).await.expect("portal");
        assert_eq!(status, 302);
        let location = reply
            .lines()
            .find_map(|line| line.strip_prefix("Location: "))
            .expect("AWS redirect")
            .trim();
        let aws_page = url::Url::parse(location).expect("AWS URL");
        assert_eq!(aws_page.path(), "/authorize");
        assert_eq!(aws_page.host_str(), Some("oidc.us-east-1.example.test"));
        let aws_state = aws_page
            .query_pairs()
            .find(|(key, _)| key == "state")
            .map(|(_, value)| value.into_owned())
            .expect("AWS state");
        assert_ne!(aws_state, state);
        assert_eq!(
            aws_page
                .query_pairs()
                .find(|(key, _)| key == "client_id")
                .map(|(_, value)| value.into_owned()),
            Some("registered-client".to_owned())
        );
        assert!(
            loopback_get(port(&page), &portal)
                .await
                .expect("repeat")
                .1
                .contains(location)
        );
        assert_eq!(host.http().calls().len(), 1);
        let register: Value = json::from_slice(host.http().calls()[0].body()).expect("register");
        assert_eq!(register["clientName"], "Kiro IDE");
        assert_eq!(register["issuerUrl"], "https://view.awsapps.com/start");
        assert_eq!(
            register["redirectUris"][0],
            "http://127.0.0.1/oauth/callback"
        );

        let wrong = callback_target(
            "/oauth/callback",
            &[("code", "aws-code"), ("state", &state)],
        );
        assert_eq!(
            loopback_get(port(&page), &wrong)
                .await
                .expect("wrong AWS state")
                .0,
            404
        );
        host.http().enqueue(
            br#"{"accessToken":"aws-access","refreshToken":"aws-refresh","expiresIn":3600}"#
                .to_vec(),
        );
        host.http()
            .enqueue(br#"{"userInfo":{"email":"builder@example.test"}}"#.to_vec());
        let aws = callback_target(
            "/oauth/callback",
            &[("code", "aws-code"), ("state", &aws_state)],
        );
        assert_eq!(
            loopback_get(port(&page), &aws)
                .await
                .expect("AWS callback")
                .0,
            200
        );
        let id = sub
            .complete_sign_in(&ctx, SignInProof::Await, host.context())
            .await
            .expect("complete");
        let calls = host.http().calls();
        assert_eq!(calls.len(), 3);
        assert_eq!(calls[1].uri(), "https://oidc.us-east-1.example.test/token");
        let token: Value = json::from_slice(calls[1].body()).expect("token");
        assert_eq!(token["grantType"], "authorization_code");
        assert_eq!(token["code"], "aws-code");
        assert_eq!(token["clientId"], "registered-client");
        let verifier = token["codeVerifier"].as_str().expect("verifier");
        let challenge = aws_page
            .query_pairs()
            .find(|(key, _)| key == "code_challenge")
            .map(|(_, value)| value.into_owned())
            .expect("AWS challenge");
        assert_eq!(
            base64url(&hofund_plugin_sdk::digest::sha256(verifier.as_bytes())),
            challenge
        );
        let stored = stored(&host, &id).await;
        assert_eq!(stored.text("method"), Some("idc"));
        assert_eq!(stored.text("provider"), Some("BuilderId"));
        assert_eq!(
            stored.text("profile_arn"),
            Some(crate::creds::BUILDER_ID_PROFILE)
        );
        assert_eq!(
            stored.text("label"),
            Some("builder@example.test (Builder ID)")
        );
    });
}

#[test]
fn r51b_direct_enterprise_uses_start_url_region_and_listed_profile() {
    let dir = tempfile::tempdir().expect("fixture directory");
    let host = host(&dir);
    let sub = subscription(&host);
    let ctx = ctx(Some("enterprise"))
        .with_option("host", "https://d-example.awsapps.com/start")
        .expect("start URL")
        .with_option("region", "eu-west-1")
        .expect("region");
    block_on(async {
        host.http().enqueue(
            br#"{"clientId":"enterprise-client","clientSecret":"enterprise-secret"}"#.to_vec(),
        );
        let page = open_url(
            sub.start_sign_in(&ctx, host.context())
                .await
                .expect("start"),
        );
        assert_eq!(page.host_str(), Some("oidc.eu-west-1.example.test"));
        let register: Value = json::from_slice(host.http().calls()[0].body()).expect("register");
        assert_eq!(register["issuerUrl"], "https://d-example.awsapps.com/start");
        let state = page
            .query_pairs()
            .find(|(key, _)| key == "state")
            .map(|(_, value)| value.into_owned())
            .expect("state");
        host.http().enqueue(br#"{"accessToken":"enterprise-access","refreshToken":"enterprise-refresh","expiresIn":3600}"#.to_vec());
        host.http().enqueue(
            br#"{"profiles":[{"arn":"arn:aws:codewhisperer:eu-west-1:1:profile/E"}]}"#.to_vec(),
        );
        host.http()
            .enqueue(br#"{"userInfo":{"email":"enterprise@example.test"}}"#.to_vec());
        let callback = callback_target(
            "/oauth/callback",
            &[("code", "enterprise-code"), ("state", &state)],
        );
        assert_eq!(
            loopback_get(port(&page), &callback)
                .await
                .expect("callback")
                .0,
            200
        );
        let id = sub
            .complete_sign_in(&ctx, SignInProof::Await, host.context())
            .await
            .expect("complete");
        let stored = stored(&host, &id).await;
        assert_eq!(stored.text("provider"), Some("Enterprise"));
        assert_eq!(stored.text("region"), Some("eu-west-1"));
        assert_eq!(
            stored.text("profile_arn"),
            Some("arn:aws:codewhisperer:eu-west-1:1:profile/E")
        );
        assert_eq!(
            stored.text("label"),
            Some("enterprise@example.test (IAM Identity Center)")
        );
    });
}

#[test]
fn r51b_enterprise_and_portal_aws_inputs_fail_before_or_without_registration() {
    let dir = tempfile::tempdir().expect("fixture directory");
    let host = host(&dir);
    let sub = subscription(&host);
    for (start_url, region) in [
        ("http://example.test/start", "us-east-1"),
        ("https://example.test/start", "bad/host"),
    ] {
        let ctx = ctx(Some("enterprise"))
            .with_option("host", start_url)
            .expect("host")
            .with_option("region", region)
            .expect("region");
        block_on(sub.start_sign_in(&ctx, host.context())).expect_err("invalid enterprise");
    }
    assert!(host.http().calls().is_empty());

    let ctx = ctx(None);
    block_on(async {
        let page = open_url(
            sub.start_sign_in(&ctx, host.context())
                .await
                .expect("portal"),
        );
        let state = page
            .query_pairs()
            .find(|(key, _)| key == "state")
            .map(|(_, value)| value.into_owned())
            .expect("state");
        let callback = callback_target(
            "/signin/callback",
            &[
                ("login_option", "builderid"),
                ("issuer_url", "http://view.awsapps.com/start"),
                ("idc_region", "us-east-1"),
                ("state", &state),
            ],
        );
        assert_eq!(
            loopback_get(port(&page), &callback)
                .await
                .expect("callback")
                .0,
            200
        );
        sub.complete_sign_in(&ctx, SignInProof::Await, host.context())
            .await
            .expect_err("HTTP issuer refused");
    });
    assert!(host.http().calls().is_empty());
}
