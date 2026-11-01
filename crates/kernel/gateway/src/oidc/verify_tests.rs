use std::collections::BTreeMap;
use std::sync::Arc;
use std::time::{Duration, UNIX_EPOCH};

use hofund_secrets::oidc::{RuleDraft, RuleStore};
use hofund_secrets::KeyStore;
use serde_json::{Number, Value};

use super::test_issuer::TestIssuer;
use super::{claim_str, match_token, oidc_key_name, reset_oidc_verifier_for_tests, set_fetch_client_for_tests, set_now_for_tests};
use crate::oidc::verify::parse_jwks;
use base64::engine::general_purpose::URL_SAFE_NO_PAD;
use base64::Engine as _;
use ring::signature::{UnparsedPublicKey, RSA_PKCS1_2048_8192_SHA256};

#[test]
fn d28_jwks_spki_verifies_test_issuer_tokens() {
    let issuer = TestIssuer::new();
    let jwks = issuer
        .fetch(&format!("{}/jwks", issuer.url))
        .expect("jwks");
    let keys = parse_jwks(&jwks).expect("parse");
    let der = keys.get("k1").expect("k1");
    let token = issuer.sign("k1", issuer.claims("hofund-x", &BTreeMap::new()));
    let parts: Vec<&str> = token.split('.').collect();
    let signed = format!("{}.{}", parts[0], parts[1]);
    let sig = URL_SAFE_NO_PAD.decode(parts[2]).expect("sig");
    let key = UnparsedPublicKey::new(&RSA_PKCS1_2048_8192_SHA256, der.as_slice());
    key.verify(signed.as_bytes(), &sig)
        .expect("signature should verify");
}

fn issuer_key(keys: &KeyStore) -> String {
    let parent = keys.add("CI", None).expect("add");
    keys.set_issuer(&parent.key.id, true).expect("issuer");
    parent.key.id
}

async fn fixture() -> (tempfile::TempDir, RuleStore, KeyStore, Arc<TestIssuer>) {
    reset_oidc_verifier_for_tests().await;
    let dir = tempfile::tempdir().expect("temp");
    let token = dir.path().join("hofund.token");
    let keys = KeyStore::beside_token(&token);
    keys.list().expect("migrate");
    let rules = RuleStore::beside_token(&token);
    let issuer = Arc::new(TestIssuer::new());
    let _ = set_fetch_client_for_tests(Arc::new({
        let issuer = Arc::clone(&issuer);
        move |url| issuer.fetch(url)
    }));
    (dir, rules, keys, issuer)
}

#[tokio::test]
async fn d28_match_accepts_valid_token_and_refuses_replay() {
    let (_dir, rules, keys, issuer) = fixture().await;
    let key = issuer_key(&keys);
    let rule = rules
        .add(
            RuleDraft {
                key,
                issuer: issuer.url.clone(),
                claims: BTreeMap::from([
                    ("repository".to_owned(), "acme/app".to_owned()),
                    ("ref".to_owned(), "refs/heads/*".to_owned()),
                ]),
                ..RuleDraft::default()
            },
            &keys,
        )
        .expect("add");
    assert!(rule.audience.starts_with("hofund-"));
    let token = issuer.sign("k1", issuer.claims(&rule.audience, &BTreeMap::new()));
    let (matched, claims) = match_token(&token, &rules).await.expect("match");
    assert_eq!(matched.id, rule.id);
    assert_eq!(claim_str(&claims, "run_id"), "42");
    let err = match_token(&token, &rules).await.expect_err("replay");
    assert!(err.contains("already"));
}

#[tokio::test]
async fn d28_match_refuses_each_bad_claim() {
    let (_dir, rules, keys, issuer) = fixture().await;
    let key = issuer_key(&keys);
    let rule = rules
        .add(
            RuleDraft {
                key,
                issuer: issuer.url.clone(),
                claims: BTreeMap::from([
                    ("repository".to_owned(), "acme/app".to_owned()),
                    ("ref".to_owned(), "refs/heads/*".to_owned()),
                ]),
                ..RuleDraft::default()
            },
            &keys,
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
                &BTreeMap::from([(
                    "exp".to_owned(),
                    Value::Number(Number::from(now - 120)),
                )]),
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
                &BTreeMap::from([(
                    "nbf".to_owned(),
                    Value::Number(Number::from(now + 600)),
                )]),
            ),
        ),
        (
            "issued in the future",
            issuer.claims(
                &rule.audience,
                &BTreeMap::from([(
                    "iat".to_owned(),
                    Value::Number(Number::from(now + 600)),
                )]),
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
        assert!(match_token(&token, &rules).await.is_err(), "{name} matched");
    }
    assert!(match_token("abc.def", &rules).await.is_err());
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
        &rules,
    )
    .await
    .expect_err("describe");
    assert!(err.contains("repository="));
    assert!(match_token(&good, &rules).await.is_ok());
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
    assert!(match_token(&issuer.sign("k1", list_aud), &rules).await.is_ok());
}

#[tokio::test]
async fn d28_jwks_cache_refetch_and_stale_serving() {
    let (_dir, rules, keys, issuer) = fixture().await;
    let key = issuer_key(&keys);
    let rule = rules
        .add(
            RuleDraft {
                key,
                issuer: issuer.url.clone(),
                claims: BTreeMap::from([("repository".to_owned(), "acme/app".to_owned())]),
                ..RuleDraft::default()
            },
            &keys,
        )
        .expect("add");
    let token = || issuer.sign("k1", issuer.claims(&rule.audience, &BTreeMap::new()));
    for _ in 0..8 {
        match_token(&token(), &rules).await.expect("parallel");
    }
    assert_eq!(issuer.fetch_count(), 1);
    issuer.add_key("k2");
    set_now_for_tests(Some(
        std::time::SystemTime::now() + Duration::from_secs(11),
    ));
    match_token(
        &issuer.sign("k2", issuer.claims(&rule.audience, &BTreeMap::new())),
        &rules,
    )
    .await
    .expect("rotated");
    set_now_for_tests(Some(
        std::time::SystemTime::now() + Duration::from_secs(22),
    ));
    for _ in 0..5 {
        let _ = match_token(
            &issuer.sign("zz", issuer.claims(&rule.audience, &BTreeMap::new())),
            &rules,
        )
        .await;
    }
    assert!(issuer.fetch_count() >= 3);
    issuer.set_down(true);
    set_now_for_tests(Some(
        std::time::SystemTime::now() + Duration::from_secs(7200),
    ));
    assert!(match_token(&token(), &rules).await.is_ok());
    set_now_for_tests(None);
}

#[tokio::test]
async fn d28_unread_keys_refuse_tokens() {
    let (_dir, rules, keys, issuer) = fixture().await;
    issuer.set_down(true);
    let key = issuer_key(&keys);
    let rule = rules
        .add(
            RuleDraft {
                key,
                issuer: issuer.url.clone(),
                claims: BTreeMap::from([("repository".to_owned(), "acme/app".to_owned())]),
                ..RuleDraft::default()
            },
            &keys,
        )
        .expect("add");
    let err = match_token(
        &issuer.sign("k1", issuer.claims(&rule.audience, &BTreeMap::new())),
        &rules,
    )
    .await
    .expect_err("down");
    assert!(err.contains("can't be read"));
}

#[tokio::test]
async fn d28_issuer_without_rule_is_never_fetched() {
    let (_dir, rules, keys, issuer) = fixture().await;
    let key = issuer_key(&keys);
    let rule = rules
        .add(
            RuleDraft {
                key,
                issuer: issuer.url.clone(),
                claims: BTreeMap::from([("repository".to_owned(), "acme/app".to_owned())]),
                ..RuleDraft::default()
            },
            &keys,
        )
        .expect("add");
    let other = TestIssuer::new();
    let before = issuer.fetch_count();
    assert!(
        match_token(
            &other.sign("k1", other.claims(&rule.audience, &BTreeMap::new())),
            &rules,
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
        ("repository".to_owned(), Value::String("acme/app".to_owned())),
        ("ref".to_owned(), Value::String("refs/heads/main".to_owned())),
        ("run_id".to_owned(), Value::String("42".to_owned())),
    ]);
    assert_eq!(oidc_key_name(&claims), "acme/app@main (run 42)");
}
