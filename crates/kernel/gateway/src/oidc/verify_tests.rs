use std::collections::BTreeMap;
use std::sync::Arc;
use std::time::{Duration, UNIX_EPOCH};

use hofund_secrets::KeyStore;
use hofund_secrets::oidc::{RuleDraft, RuleStore};
use serde_json::{Number, Value, json};

use super::test_issuer::TestIssuer;
use super::{
    claim_str, lock_oidc_tests, match_token, oidc_key_name, reset_oidc_verifier_for_tests,
    set_fetch_client_for_tests, set_now_for_tests,
};
use crate::oidc::verify::parse_jwks;
use base64::Engine as _;
use base64::engine::general_purpose::URL_SAFE_NO_PAD;
use ring::signature::{RSA_PKCS1_2048_8192_SHA256, UnparsedPublicKey};

#[test]
fn d28_jwks_spki_verifies_test_issuer_tokens() {
    let rt = tokio::runtime::Runtime::new().expect("runtime");
    let _guard = rt.block_on(lock_oidc_tests());
    let issuer = TestIssuer::new();
    let jwks = issuer.fetch(&format!("{}/jwks", issuer.url)).expect("jwks");
    let keys = parse_jwks(&jwks).expect("parse");
    let der = keys.get("k1").expect("k1");
    assert_eq!(
        der.as_slice(),
        issuer.spki(),
        "JWKS-derived SPKI should match the signing key"
    );
    let token = issuer.sign("k1", issuer.claims("hofund-x", &BTreeMap::new()));
    let parts: Vec<&str> = token.split('.').collect();
    let signed = format!("{}.{}", parts[0], parts[1]);
    let sig = URL_SAFE_NO_PAD.decode(parts[2]).expect("sig");
    let key = UnparsedPublicKey::new(&RSA_PKCS1_2048_8192_SHA256, der.as_slice());
    key.verify(signed.as_bytes(), &sig)
        .expect("signature should verify");
}

#[test]
fn d28_test_issuer_sign_roundtrip() {
    let rt = tokio::runtime::Runtime::new().expect("runtime");
    let _guard = rt.block_on(lock_oidc_tests());
    let issuer = TestIssuer::new();
    let token = issuer.sign("k1", issuer.claims("hofund-x", &BTreeMap::new()));
    issuer.verify_token(&token).expect("roundtrip");
}

fn issuer_key(keys: &KeyStore) -> String {
    let parent = keys.add("CI", None).expect("add");
    keys.set_issuer(&parent.key.id, true).expect("issuer");
    parent.key.id
}

struct OidcFixture {
    _guard: tokio::sync::MutexGuard<'static, ()>,
    _dir: tempfile::TempDir,
    rules: RuleStore,
    keys: KeyStore,
    issuer: Arc<TestIssuer>,
}

async fn fixture() -> OidcFixture {
    let guard = lock_oidc_tests().await;
    reset_oidc_verifier_for_tests().await;
    let dir = tempfile::tempdir().expect("temp");
    let token = dir.path().join("hofund.token");
    let keys = KeyStore::beside_token(&token);
    keys.list().expect("migrate");
    let rules = RuleStore::beside_token(&token);
    let issuer = Arc::new(TestIssuer::new());
    set_fetch_client_for_tests(Arc::new({
        let issuer = Arc::clone(&issuer);
        move |url| issuer.fetch(url)
    }));
    OidcFixture {
        _guard: guard,
        _dir: dir,
        rules,
        keys,
        issuer,
    }
}

#[tokio::test]
async fn d28_match_accepts_valid_token_and_refuses_replay() {
    let fixture = fixture().await;
    let issuer = &fixture.issuer;
    let key = issuer_key(&fixture.keys);
    let rule = fixture
        .rules
        .add(
            RuleDraft {
                key,
                issuer: fixture.issuer.url.clone(),
                claims: BTreeMap::from([
                    ("repository".to_owned(), "acme/app".to_owned()),
                    ("ref".to_owned(), "refs/heads/*".to_owned()),
                ]),
                ..RuleDraft::default()
            },
            &fixture.keys,
        )
        .expect("add");
    assert!(rule.audience.starts_with("hofund-"));
    let token = issuer.sign("k1", issuer.claims(&rule.audience, &BTreeMap::new()));
    let (matched, claims) = match_token(&token, &fixture.rules).await.expect("match");
    assert_eq!(matched.id, rule.id);
    assert_eq!(claim_str(&claims, "run_id"), "42");
    let err = match_token(&token, &fixture.rules)
        .await
        .expect_err("replay");
    assert!(err.contains("already"));
}

#[tokio::test]
async fn d28_match_refuses_each_bad_claim() {
    let fixture = fixture().await;
    let issuer = &fixture.issuer;
    let key = issuer_key(&fixture.keys);
    let rule = fixture
        .rules
        .add(
            RuleDraft {
                key,
                issuer: fixture.issuer.url.clone(),
                claims: BTreeMap::from([
                    ("repository".to_owned(), "acme/app".to_owned()),
                    ("ref".to_owned(), "refs/heads/*".to_owned()),
                ]),
                ..RuleDraft::default()
            },
            &fixture.keys,
        )
        .expect("add");
    let now = std::time::SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .expect("time")
        .as_secs() as i64;
    let good = issuer.sign("k1", issuer.claims(&rule.audience, &BTreeMap::new()));
    let cases: Vec<(&str, BTreeMap<String, Value>)> = vec![
        (
            "another repository",
            issuer.claims(
                &rule.audience,
                &BTreeMap::from([(
                    "repository".to_owned(),
                    Value::String("evil/app".to_owned()),
                )]),
            ),
        ),
        (
            "another audience",
            issuer.claims("hofund-other", &BTreeMap::new()),
        ),
        (
            "expired",
            issuer.claims(
                &rule.audience,
                &BTreeMap::from([("exp".to_owned(), Value::Number(Number::from(now - 120)))]),
            ),
        ),
        (
            "no expiry",
            issuer.claims(
                &rule.audience,
                &BTreeMap::from([("exp".to_owned(), Value::Null)]),
            ),
        ),
        (
            "not yet valid",
            issuer.claims(
                &rule.audience,
                &BTreeMap::from([("nbf".to_owned(), Value::Number(Number::from(now + 600)))]),
            ),
        ),
        (
            "issued in the future",
            issuer.claims(
                &rule.audience,
                &BTreeMap::from([("iat".to_owned(), Value::Number(Number::from(now + 600)))]),
            ),
        ),
        (
            "another issuer",
            issuer.claims(
                &rule.audience,
                &BTreeMap::from([(
                    "iss".to_owned(),
                    Value::String("https://evil.example".to_owned()),
                )]),
            ),
        ),
    ];
    for (name, claims) in cases {
        let token = issuer.sign("k1", claims);
        assert!(
            match_token(&token, &fixture.rules).await.is_err(),
            "{name} matched"
        );
    }
    assert!(match_token("abc.def", &fixture.rules).await.is_err());
    let err = match_token(
        &issuer.sign(
            "k1",
            issuer.claims(
                &rule.audience,
                &BTreeMap::from([(
                    "repository".to_owned(),
                    Value::String("evil/app".to_owned()),
                )]),
            ),
        ),
        &fixture.rules,
    )
    .await
    .expect_err("describe");
    assert!(err.contains("repository="));
    assert!(match_token(&good, &fixture.rules).await.is_ok());
    let list_aud = issuer.claims(
        "",
        &BTreeMap::from([(
            "aud".to_owned(),
            Value::Array(vec![
                Value::String("x".to_owned()),
                Value::String(rule.audience.clone()),
            ]),
        )]),
    );
    assert!(
        match_token(&issuer.sign("k1", list_aud), &fixture.rules)
            .await
            .is_ok()
    );
}

#[tokio::test]
async fn d28_jwks_cache_refetch_and_stale_serving() {
    let fixture = fixture().await;
    let issuer = &fixture.issuer;
    let key = issuer_key(&fixture.keys);
    let rule = fixture
        .rules
        .add(
            RuleDraft {
                key,
                issuer: fixture.issuer.url.clone(),
                claims: BTreeMap::from([("repository".to_owned(), "acme/app".to_owned())]),
                ..RuleDraft::default()
            },
            &fixture.keys,
        )
        .expect("add");
    let token = || issuer.sign("k1", issuer.claims(&rule.audience, &BTreeMap::new()));
    for _ in 0..8 {
        match_token(&token(), &fixture.rules)
            .await
            .expect("parallel");
    }
    assert_eq!(issuer.fetch_count(), 1);
    issuer.add_key("k2");
    set_now_for_tests(Some(std::time::SystemTime::now() + Duration::from_secs(11)));
    match_token(
        &issuer.sign("k2", issuer.claims(&rule.audience, &BTreeMap::new())),
        &fixture.rules,
    )
    .await
    .expect("rotated");
    set_now_for_tests(Some(std::time::SystemTime::now() + Duration::from_secs(22)));
    for _ in 0..5 {
        let _ = match_token(
            &issuer.sign("zz", issuer.claims(&rule.audience, &BTreeMap::new())),
            &fixture.rules,
        )
        .await;
    }
    assert!(issuer.fetch_count() >= 3);
    issuer.set_down(true);
    let clock = std::time::SystemTime::now() + Duration::from_secs(7200);
    set_now_for_tests(Some(clock));
    let exp = clock.duration_since(UNIX_EPOCH).expect("clock").as_secs() + 300;
    let stale = issuer.sign(
        "k1",
        issuer.claims(
            &rule.audience,
            &BTreeMap::from([("exp".to_owned(), Value::Number(Number::from(exp)))]),
        ),
    );
    assert!(match_token(&stale, &fixture.rules).await.is_ok());
    set_now_for_tests(None);
}

#[tokio::test]
async fn d28_unread_keys_refuse_tokens() {
    let fixture = fixture().await;
    fixture.issuer.set_down(true);
    let key = issuer_key(&fixture.keys);
    let rule = fixture
        .rules
        .add(
            RuleDraft {
                key,
                issuer: fixture.issuer.url.clone(),
                claims: BTreeMap::from([("repository".to_owned(), "acme/app".to_owned())]),
                ..RuleDraft::default()
            },
            &fixture.keys,
        )
        .expect("add");
    let err = match_token(
        &fixture.issuer.sign(
            "k1",
            fixture.issuer.claims(&rule.audience, &BTreeMap::new()),
        ),
        &fixture.rules,
    )
    .await
    .expect_err("down");
    assert!(err.contains("can't be read"));
}

#[tokio::test]
async fn d28_issuer_without_rule_is_never_fetched() {
    let fixture = fixture().await;
    let issuer = &fixture.issuer;
    let key = issuer_key(&fixture.keys);
    let rule = fixture
        .rules
        .add(
            RuleDraft {
                key,
                issuer: fixture.issuer.url.clone(),
                claims: BTreeMap::from([("repository".to_owned(), "acme/app".to_owned())]),
                ..RuleDraft::default()
            },
            &fixture.keys,
        )
        .expect("add");
    let other = TestIssuer::new();
    let before = issuer.fetch_count();
    assert!(
        match_token(
            &other.sign("k1", other.claims(&rule.audience, &BTreeMap::new())),
            &fixture.rules,
        )
        .await
        .is_err()
    );
    assert_eq!(other.fetch_count(), 0);
    assert_eq!(issuer.fetch_count(), before);
}

#[test]
fn d28_oidc_key_name_from_workflow_claims() {
    let claims = BTreeMap::from([
        (
            "repository".to_owned(),
            Value::String("acme/app".to_owned()),
        ),
        (
            "ref".to_owned(),
            Value::String("refs/heads/main".to_owned()),
        ),
        ("run_id".to_owned(), Value::String("42".to_owned())),
    ]);
    assert_eq!(oidc_key_name(&claims), "acme/app@main (run 42)");
}

#[tokio::test]
async fn d28_01_match_refuses_non_rs256_algorithms() {
    let fixture = fixture().await;
    let issuer = &fixture.issuer;
    let key = issuer_key(&fixture.keys);
    let rule = fixture
        .rules
        .add(
            RuleDraft {
                key,
                issuer: fixture.issuer.url.clone(),
                claims: BTreeMap::from([("repository".to_owned(), "acme/app".to_owned())]),
                ..RuleDraft::default()
            },
            &fixture.keys,
        )
        .expect("add");
    let claims = issuer.claims(&rule.audience, &BTreeMap::new());
    for alg in ["HS256", "none", "RS384"] {
        let token = issuer.sign_with_alg("k1", alg, claims.clone());
        let err = match_token(&token, &fixture.rules).await.expect_err(alg);
        assert!(
            err.contains("only RS256"),
            "{alg} should name RS256 refusal, got {err:?}"
        );
    }
}

#[tokio::test]
async fn d28_02_match_refuses_invalid_signature() {
    let fixture = fixture().await;
    let issuer = &fixture.issuer;
    let key = issuer_key(&fixture.keys);
    let rule = fixture
        .rules
        .add(
            RuleDraft {
                key,
                issuer: fixture.issuer.url.clone(),
                claims: BTreeMap::from([("repository".to_owned(), "acme/app".to_owned())]),
                ..RuleDraft::default()
            },
            &fixture.keys,
        )
        .expect("add");
    let claims = issuer.claims(&rule.audience, &BTreeMap::new());
    let good = issuer.sign("k1", claims.clone());
    match_token(&good, &fixture.rules)
        .await
        .expect("valid signature");
    let tampered = TestIssuer::token_with_tampered_payload(&good);
    let err = match_token(&tampered, &fixture.rules)
        .await
        .expect_err("tampered payload");
    assert!(err.contains("signature is not"), "{err}");
    let foreign = TestIssuer::foreign_pair();
    let wrong_key = issuer.sign_with_pair(&foreign, "k1", "RS256", claims);
    let err = match_token(&wrong_key, &fixture.rules)
        .await
        .expect_err("foreign signer");
    assert!(err.contains("signature is not"), "{err}");
}

#[tokio::test]
async fn d28_08_match_refuses_http_jwks_uri_without_jwks_fetch() {
    let fixture = fixture().await;
    let host = fixture
        .issuer
        .url
        .strip_prefix("https://")
        .expect("issuer url");
    fixture.issuer.set_jwks_uri(format!("http://{host}/jwks"));
    let key = issuer_key(&fixture.keys);
    let rule = fixture
        .rules
        .add(
            RuleDraft {
                key,
                issuer: fixture.issuer.url.clone(),
                claims: BTreeMap::from([("repository".to_owned(), "acme/app".to_owned())]),
                ..RuleDraft::default()
            },
            &fixture.keys,
        )
        .expect("add");
    let token = fixture.issuer.sign(
        "k1",
        fixture.issuer.claims(&rule.audience, &BTreeMap::new()),
    );
    let err = match_token(&token, &fixture.rules)
        .await
        .expect_err("http jwks");
    assert!(err.contains("jwks_uri isn't https"), "{err}");
    assert_eq!(fixture.issuer.fetch_count(), 0);
}

#[test]
fn d28_09_parse_jwks_skips_rsa_modulus_shorter_than_2048_bits() {
    let rt = tokio::runtime::Runtime::new().expect("runtime");
    let _guard = rt.block_on(lock_oidc_tests());
    let mut modulus = vec![0_u8; 128];
    modulus[0] = 0x80;
    let jwks = json!({
        "keys": [{
            "kty": "RSA",
            "kid": "weak",
            "use": "sig",
            "n": URL_SAFE_NO_PAD.encode(modulus),
            "e": URL_SAFE_NO_PAD.encode([1_u8, 0, 1]),
        }]
    });
    let err = parse_jwks(jwks.to_string().as_bytes()).expect_err("1024-bit key");
    assert!(err.contains("lists no RSA signing key"), "{err}");
}

#[tokio::test]
async fn d28_10_jwks_refetch_after_keys_fresh_elapsed() {
    let fixture = fixture().await;
    let issuer = &fixture.issuer;
    let key = issuer_key(&fixture.keys);
    let rule = fixture
        .rules
        .add(
            RuleDraft {
                key,
                issuer: fixture.issuer.url.clone(),
                claims: BTreeMap::from([("repository".to_owned(), "acme/app".to_owned())]),
                ..RuleDraft::default()
            },
            &fixture.keys,
        )
        .expect("add");
    let token = || issuer.sign("k1", issuer.claims(&rule.audience, &BTreeMap::new()));
    match_token(&token(), &fixture.rules)
        .await
        .expect("initial fetch");
    assert_eq!(issuer.fetch_count(), 1);
    let advanced = std::time::SystemTime::now() + Duration::from_secs(3601);
    set_now_for_tests(Some(advanced));
    let exp = advanced
        .duration_since(UNIX_EPOCH)
        .expect("clock")
        .as_secs()
        + 300;
    let fresh = issuer.sign(
        "k1",
        issuer.claims(
            &rule.audience,
            &BTreeMap::from([("exp".to_owned(), Value::Number(Number::from(exp)))]),
        ),
    );
    match_token(&fresh, &fixture.rules)
        .await
        .expect("refetch after cache age");
    assert_eq!(issuer.fetch_count(), 2);
    set_now_for_tests(None);
}

#[tokio::test]
async fn d28_11_jwks_refetch_gap_limits_unknown_kid_fetches() {
    let fixture = fixture().await;
    let issuer = &fixture.issuer;
    let key = issuer_key(&fixture.keys);
    let rule = fixture
        .rules
        .add(
            RuleDraft {
                key,
                issuer: fixture.issuer.url.clone(),
                claims: BTreeMap::from([("repository".to_owned(), "acme/app".to_owned())]),
                ..RuleDraft::default()
            },
            &fixture.keys,
        )
        .expect("add");
    let base = std::time::SystemTime::now();
    set_now_for_tests(Some(base));
    let claims = || issuer.claims(&rule.audience, &BTreeMap::new());
    assert!(
        match_token(&issuer.sign("ghost-a", claims()), &fixture.rules)
            .await
            .is_err()
    );
    assert_eq!(issuer.fetch_count(), 1);
    assert!(
        match_token(&issuer.sign("ghost-b", claims()), &fixture.rules)
            .await
            .is_err()
    );
    assert_eq!(
        issuer.fetch_count(),
        1,
        "second unknown kid within the refetch gap must not fetch again"
    );
    set_now_for_tests(Some(base + Duration::from_secs(11)));
    assert!(
        match_token(&issuer.sign("ghost-c", claims()), &fixture.rules)
            .await
            .is_err()
    );
    assert_eq!(issuer.fetch_count(), 2);
    set_now_for_tests(None);
}
