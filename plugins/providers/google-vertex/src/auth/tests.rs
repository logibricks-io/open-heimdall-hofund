use std::path::Path;
use std::time::{Duration, UNIX_EPOCH};

use hofund_kit_upstream::{TestHost, block_on};
use hofund_plugin_api::{PluginError, PluginErrorKind, PluginId};
use hofund_plugin_sdk::Call;
use hofund_plugin_sdk::json::{Value, json};

use super::{
    GoogleCreds, MAX_CREDENTIALS_BYTES, MintFail, SCOPE, classify_refresh, delegate_name,
    gcloud_dir, impersonated_account, parse_rfc3339_utc, percent_decode, post_token,
    service_assertion, sign_in_again, token_error_code,
};

use crate::tests::PKCS8;

/// Decodes unpadded base64url, the JWT encoding.
fn base64url(text: &str) -> Vec<u8> {
    let mut bits = 0_u32;
    let mut held = 0;
    let mut out = Vec::new();
    for byte in text.bytes() {
        let value = match byte {
            b'A'..=b'Z' => byte - b'A',
            b'a'..=b'z' => byte - b'a' + 26,
            b'0'..=b'9' => byte - b'0' + 52,
            b'-' => 62,
            b'_' => 63,
            other => panic!("not base64url: {other}"),
        };
        bits = (bits << 6) | u32::from(value);
        held += 6;
        if held >= 8 {
            held -= 8;
            out.push(u8::try_from((bits >> held) & 0xff).expect("byte"));
        }
    }
    out
}

fn part(text: &str) -> Value {
    hofund_plugin_sdk::json::from_slice(&base64url(text)).expect("json")
}

#[test]
fn a_service_account_assertion_names_the_account_and_the_token_url() {
    let creds = GoogleCreds::from_value(&json!({
        "type": "service_account",
        "client_email": "worker@p.iam.gserviceaccount.com",
        "private_key": PKCS8,
        "private_key_id": "key-1",
    }))
    .expect("creds");
    let now = UNIX_EPOCH + Duration::from_secs(1_000);
    let jwt = service_assertion(&creds, "https://oauth2.example/token", now).expect("jwt");
    let parts: Vec<&str> = jwt.split('.').collect();
    assert_eq!(parts.len(), 3);
    let header = part(parts[0]);
    assert_eq!(
        header,
        json!({"alg": "RS256", "typ": "JWT", "kid": "key-1"})
    );
    let claims = part(parts[1]);
    assert_eq!(claims["iss"], "worker@p.iam.gserviceaccount.com");
    assert_eq!(claims["aud"], "https://oauth2.example/token");
    assert_eq!(claims["scope"], SCOPE);
    assert_eq!(claims["iat"], 990);
    assert_eq!(claims["exp"], 990 + 3600);
    assert_eq!(base64url(parts[2]).len(), 256);
}

#[test]
fn an_assertion_without_a_key_id_has_no_kid_and_a_bad_key_is_named() {
    let mut creds = GoogleCreds::from_value(&json!({
        "type": "service_account",
        "client_email": "worker@p.iam.gserviceaccount.com",
        "private_key": PKCS8,
    }))
    .expect("creds");
    let jwt = service_assertion(&creds, "https://t", UNIX_EPOCH).expect("jwt");
    let header = part(jwt.split('.').next().expect("header"));
    assert!(header.get("kid").is_none(), "{header}");
    assert_eq!(part(jwt.split('.').nth(1).expect("claims"))["iat"], 0);
    "not a key".clone_into(&mut creds.private_key);
    let error = service_assertion(&creds, "https://t", UNIX_EPOCH).expect_err("no key");
    assert_eq!(error.message(), "their private_key is no PEM key");
}

#[test]
fn credentials_fields_are_strings_and_lists() {
    assert_eq!(
        GoogleCreds::from_value(&json!([])).err().as_deref(),
        Some("expected a JSON object")
    );
    assert_eq!(
        GoogleCreds::from_value(&json!({"type": 1}))
            .err()
            .as_deref(),
        Some("`type` is not a string")
    );
    assert_eq!(
        GoogleCreds::from_value(&json!({"delegates": "a"}))
            .err()
            .as_deref(),
        Some("`delegates` is not a list")
    );
    assert_eq!(
        GoogleCreds::from_value(&json!({"delegates": [1]}))
            .err()
            .as_deref(),
        Some("`delegates` holds a value that is not a string")
    );
    assert_eq!(
        GoogleCreds::from_value(&json!({"source_credentials": {"client_id": 2}}))
            .err()
            .as_deref(),
        Some("`client_id` is not a string")
    );
    let creds = GoogleCreds::from_value(&json!({
        "type": "impersonated_service_account",
        "client_id": null,
        "delegates": ["a@p", "projects/-/serviceAccounts/b@p"],
        "service_account_impersonation_url": "https://iam/v1/projects/-/serviceAccounts/w%40p:generateAccessToken",
        "source_credentials": {"type": "authorized_user", "refresh_token": "r"},
    }))
    .expect("creds");
    assert_eq!(creds.client_id, "");
    assert_eq!(creds.delegates, ["a@p", "projects/-/serviceAccounts/b@p"]);
    let source = creds.source_credentials.as_deref().expect("source");
    assert_eq!(source.kind, "authorized_user");
    assert_eq!(source.refresh_token, "r");
    assert_eq!(creds.impersonated_email().expect("email"), "w@p");
    let plain = GoogleCreds::from_value(&json!({"type": "authorized_user"})).expect("creds");
    assert_eq!(plain.impersonated_email().expect("none"), "");
}

#[test]
fn a_delegate_is_a_full_resource_name() {
    assert_eq!(delegate_name("a@p"), "projects/-/serviceAccounts/a@p");
    assert_eq!(
        delegate_name("projects/x/serviceAccounts/b@p"),
        "projects/x/serviceAccounts/b@p"
    );
}

#[test]
fn the_impersonated_account_is_the_url_segment_decoded() {
    let url = |tail: &str| {
        format!("https://iamcredentials.googleapis.com/v1/projects/-/serviceAccounts/{tail}")
    };
    assert_eq!(
        impersonated_account(&url("w%40p.iam.gserviceaccount.com:generateAccessToken"))
            .expect("ok"),
        Some("w@p.iam.gserviceaccount.com".to_owned())
    );
    for none in [
        "https://iam/v1/projects/-/w:generateAccessToken".to_owned(),
        url(":generateAccessToken"),
        url("w@p:signJwt"),
        url("a/b:generateAccessToken"),
        url("a:b:generateAccessToken"),
    ] {
        assert_eq!(impersonated_account(&none).expect("ok"), None, "{none}");
    }
    assert!(impersonated_account(&url("w%4:generateAccessToken")).is_err());
}

#[test]
fn percent_escapes_decode_as_utf8() {
    assert_eq!(percent_decode("a%40b").expect("ok"), "a@b");
    assert_eq!(percent_decode("%c3%A9").expect("ok"), "\u{e9}");
    assert_eq!(percent_decode("plain").expect("ok"), "plain");
    for (bad, message) in [
        ("%", "truncated percent escape"),
        ("a%4", "truncated percent escape"),
        ("%zz", "non-hex percent escape"),
        ("%4g", "non-hex percent escape"),
        ("%ff", "not valid UTF-8"),
    ] {
        let error = percent_decode(bad).expect_err(bad);
        assert!(
            error.message().contains(message),
            "{bad}: {}",
            error.message()
        );
    }
}

#[test]
fn an_expire_time_parses_as_utc() {
    let at = |seconds: u64| Some(UNIX_EPOCH + Duration::from_secs(seconds));
    assert_eq!(parse_rfc3339_utc("2026-10-25T07:00:00Z"), at(1_792_911_600));
    assert_eq!(
        parse_rfc3339_utc("2026-10-25T07:00:00.123456Z"),
        at(1_792_911_600)
    );
    assert_eq!(parse_rfc3339_utc("1970-01-01T00:00:00Z"), at(0));
    assert_eq!(parse_rfc3339_utc("2000-02-29T23:59:59Z"), at(951_868_799));
    assert_eq!(parse_rfc3339_utc("2000-03-01T00:00:01Z"), at(951_868_801));
    for bad in [
        "2026-10-25T07:00:00+00:00",
        "2026-10-25 07:00:00Z",
        "2026-13-25T07:00:00Z",
        "2026-00-25T07:00:00Z",
        "2026-10-00T07:00:00Z",
        "2026-10-32T07:00:00Z",
        "2026-10-25-1T07:00:00Z",
        "2026-10-25T24:00:00Z",
        "2026-10-25T07:60:00Z",
        "2026-10-25T07:00:61Z",
        "2026-10-25T07:00:00:00Z",
        "1969-12-31T23:59:59Z",
    ] {
        assert_eq!(parse_rfc3339_utc(bad), None, "{bad}");
    }
}

#[test]
fn only_a_refused_refresh_asks_the_person_to_sign_in() {
    let person = |status: Option<u16>| {
        let error = match status {
            Some(status) => PluginError::status_error(status, "refused"),
            None => PluginError::connect("down"),
        };
        matches!(classify_refresh(error), MintFail::Person(_))
    };
    assert!(person(Some(400)));
    assert!(person(Some(401)));
    assert!(person(Some(499)));
    assert!(!person(Some(408)));
    assert!(!person(Some(429)));
    assert!(!person(Some(500)));
    assert!(!person(None));
}

#[test]
fn the_sign_in_hint_follows_where_the_file_is() {
    let gcloud = gcloud_dir().expect("gcloud dir");
    let adc = gcloud.join("application_default_credentials.json");
    assert_eq!(
        sign_in_again(&adc, ""),
        "sign in again with `gcloud auth application-default login`"
    );
    assert_eq!(
        sign_in_again(&adc, "w@p"),
        "sign in again with `gcloud auth application-default login --impersonate-service-account=w@p`"
    );
    assert_eq!(
        sign_in_again(
            &gcloud.join("legacy_credentials/me@example.com/adc.json"),
            ""
        ),
        "sign in again with `gcloud auth login me@example.com`"
    );
    for other in [
        gcloud.join("legacy_credentials/adc.json"),
        gcloud.join("legacy_credentials/a/b/adc.json"),
        Path::new("/elsewhere/adc.json").to_path_buf(),
    ] {
        assert!(
            sign_in_again(&other, "w@p").starts_with("sign in again and write that file anew"),
            "{}",
            other.display()
        );
    }
}

#[test]
fn a_token_error_code_is_short_lowercase_ascii() {
    assert_eq!(
        token_error_code(br#"{"error":"invalid_grant"}"#).as_deref(),
        Some("invalid_grant")
    );
    let long = format!(r#"{{"error":"{}"}}"#, "a".repeat(65));
    for bad in [
        br#"{"error":"Bad Code"}"#.as_slice(),
        br#"{"error":""}"#,
        br#"{"error":{"code":400}}"#,
        b"not json",
        long.as_bytes(),
    ] {
        assert_eq!(token_error_code(bad), None);
    }
    assert!(token_error_code(format!(r#"{{"error":"{}"}}"#, "a".repeat(64)).as_bytes()).is_some());
}

fn host(root: &Path) -> TestHost {
    TestHost::open(PluginId::from_static("google-vertex"), root)
}

#[test]
fn a_token_error_redacts_the_idp_body() {
    block_on(async {
        let root = tempfile::tempdir().expect("scratch");
        let host = host(root.path());
        host.http().enqueue_response(
            400,
            &[],
            json!({"error": "invalid_grant", "error_description": "SECRET-IDP"}).to_string(),
        );
        host.http().enqueue_response(400, &[], "SECRET-RAW-BODY");
        for code in [Some("invalid_grant"), None] {
            let error = post_token(host.context(), Call::post("https://t/token"))
                .await
                .expect_err("refused");
            let text = error.message();
            assert_eq!(error.status(), Some(400));
            assert!(text.contains("the sign-in was refused (400)"), "{text}");
            assert_eq!(code.is_some_and(|code| text.contains(code)), code.is_some());
            assert!(!text.contains("SECRET-"), "{text}");
        }
    });
}

#[test]
fn a_refused_token_keeps_its_retry_hint() {
    block_on(async {
        let root = tempfile::tempdir().expect("scratch");
        let host = host(root.path());
        host.http()
            .enqueue_response(429, &[("retry-after", "120")], "{");
        let error = post_token(host.context(), Call::post("https://t/token"))
            .await
            .expect_err("limited");
        assert_eq!(error.kind(), PluginErrorKind::Status);
        assert_eq!(error.status(), Some(429));
        assert_eq!(error.retry_after(), Some(Duration::from_secs(120)));
    });
}

#[test]
fn a_token_body_is_capped_at_one_mebibyte() {
    block_on(async {
        let root = tempfile::tempdir().expect("scratch");
        let host = host(root.path());
        let cap = usize::try_from(MAX_CREDENTIALS_BYTES).expect("cap");
        host.http().enqueue(vec![b'x'; cap + 1]);
        host.http().enqueue(vec![b'x'; cap]);
        let error = post_token(host.context(), Call::post("https://t/token"))
            .await
            .expect_err("too large");
        assert!(
            error.message().contains("larger than 1 MiB"),
            "{}",
            error.message()
        );
        let bytes = post_token(host.context(), Call::post("https://t/token"))
            .await
            .expect("at the cap");
        assert_eq!(bytes.len(), cap);
    });
}
