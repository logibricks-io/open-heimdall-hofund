//! AWS Signature Version 4 for one Bedrock Runtime request.
//!
//! The canonical request, the string to sign, and the signing key follow
//! <https://docs.aws.amazon.com/IAM/latest/UserGuide/reference_sigv-create-signed-request.html>
//! (checked 2026-10-25). The service name for the runtime is `bedrock`.
//! Percent-encoding uses uppercase hex. The payload hash is lowercase hex.

use ring::digest::{SHA256, digest};
use ring::hmac;

use std::fmt;

/// One signed request's `Authorization` value and the headers that were signed
/// with it. The caller sends these headers unchanged.
#[derive(Debug)]
#[non_exhaustive]
pub struct Signature {
    authorization: String,
}

impl Signature {
    /// `AWS4-HMAC-SHA256 Credential=...`.
    #[must_use]
    pub fn authorization(&self) -> &str {
        &self.authorization
    }
}

/// Inputs to [`sign`], built with [`SignInput::new`] and its setters.
#[non_exhaustive]
pub struct SignInput<'a> {
    method: &'a str,
    canonical_uri: &'a str,
    canonical_query: &'a str,
    amz_date: &'a str,
    region: &'a str,
    service: &'a str,
    access_key_id: &'a str,
    secret_access_key: &'a str,
    payload: &'a [u8],
    headers: &'a [(&'a str, &'a str)],
}

impl<'a> SignInput<'a> {
    /// Empty inputs to populate with the required setters.
    #[must_use]
    pub const fn new() -> Self {
        Self {
            method: "",
            canonical_uri: "",
            canonical_query: "",
            amz_date: "",
            region: "",
            service: "",
            access_key_id: "",
            secret_access_key: "",
            payload: &[],
            headers: &[],
        }
    }

    /// HTTP method, already uppercase.
    #[must_use]
    pub const fn method(mut self, method: &'a str) -> Self {
        self.method = method;
        self
    }

    /// Canonical URI from [`canonical_uri`].
    #[must_use]
    pub const fn canonical_uri(mut self, canonical_uri: &'a str) -> Self {
        self.canonical_uri = canonical_uri;
        self
    }

    /// Canonically encoded and sorted query. Empty when there is no query.
    #[must_use]
    pub const fn canonical_query(mut self, canonical_query: &'a str) -> Self {
        self.canonical_query = canonical_query;
        self
    }

    /// `YYYYMMDD'T'HHMMSS'Z'`.
    #[must_use]
    pub const fn amz_date(mut self, amz_date: &'a str) -> Self {
        self.amz_date = amz_date;
        self
    }

    /// Signing region and service. Bedrock Runtime uses service `bedrock`.
    #[must_use]
    pub const fn scope(mut self, region: &'a str, service: &'a str) -> Self {
        self.region = region;
        self.service = service;
        self
    }

    /// AWS credentials. The secret is not copied into the result.
    #[must_use]
    pub const fn credentials(mut self, access_key_id: &'a str, secret_access_key: &'a str) -> Self {
        self.access_key_id = access_key_id;
        self.secret_access_key = secret_access_key;
        self
    }

    /// Raw body. Its hash is what gets signed.
    #[must_use]
    pub const fn payload(mut self, payload: &'a [u8]) -> Self {
        self.payload = payload;
        self
    }

    /// Headers to sign, excluding `Authorization`.
    #[must_use]
    pub const fn headers(mut self, headers: &'a [(&'a str, &'a str)]) -> Self {
        self.headers = headers;
        self
    }
}

impl Default for SignInput<'_> {
    fn default() -> Self {
        Self::new()
    }
}

/// Why a request could not be signed.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
#[non_exhaustive]
pub enum SignError {
    /// A required input is empty. The value is never copied into the error.
    MissingInput(&'static str),
    /// The canonical URI is not an absolute path.
    InvalidCanonicalUri,
    /// The timestamp is not exactly `YYYYMMDD'T'HHMMSS'Z'` in ASCII.
    InvalidAmzDate,
}

impl fmt::Display for SignError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::MissingInput(field) => write!(formatter, "SigV4 input `{field}` is required"),
            Self::InvalidCanonicalUri => {
                formatter.write_str("SigV4 canonical URI must start with `/`")
            }
            Self::InvalidAmzDate => formatter.write_str("SigV4 date must be YYYYMMDDTHHMMSSZ"),
        }
    }
}

impl std::error::Error for SignError {}

/// Signs `input`. Header values are trimmed and their internal whitespace is
/// collapsed, which is the canonical form a verifier rebuilds.
///
/// The secret access key is not copied into [`Signature`].
///
/// # Errors
///
/// Returns [`SignError::MissingInput`] for an empty required field,
/// [`SignError::InvalidCanonicalUri`] for a relative URI, or
/// [`SignError::InvalidAmzDate`] when the timestamp is malformed. Error text
/// contains only the affected field name, never its value.
pub fn sign(input: &SignInput<'_>) -> Result<Signature, SignError> {
    validate_required(input)?;
    validate_amz_date(input.amz_date)?;
    let payload_hash = payload_hash(input.payload);
    let mut headers: Vec<(String, String)> = input
        .headers
        .iter()
        .map(|(name, value)| (name.to_ascii_lowercase(), normalize_header_value(value)))
        .collect();
    headers.sort_by(|left, right| left.0.cmp(&right.0));
    let signed_names = headers
        .iter()
        .map(|(name, _)| name.as_str())
        .collect::<Vec<_>>()
        .join(";");
    let mut canonical = String::new();
    canonical.push_str(input.method);
    canonical.push('\n');
    canonical.push_str(input.canonical_uri);
    canonical.push('\n');
    canonical.push_str(input.canonical_query);
    canonical.push('\n');
    for (name, value) in &headers {
        canonical.push_str(name);
        canonical.push(':');
        canonical.push_str(value);
        canonical.push('\n');
    }
    canonical.push('\n');
    canonical.push_str(&signed_names);
    canonical.push('\n');
    canonical.push_str(&payload_hash);
    let canonical_hash = hex_encode(digest(&SHA256, canonical.as_bytes()).as_ref());
    let date = &input.amz_date[..8];
    let scope = format!("{date}/{}/{}/aws4_request", input.region, input.service);
    let to_sign = format!(
        "AWS4-HMAC-SHA256\n{}\n{scope}\n{canonical_hash}",
        input.amz_date
    );
    let key = signing_key(input.secret_access_key, date, input.region, input.service);
    let signature = hex_encode(hmac_sha256(key.as_ref(), to_sign.as_bytes()).as_ref());
    Ok(Signature {
        authorization: format!(
            "AWS4-HMAC-SHA256 Credential={}/{scope}, SignedHeaders={signed_names}, Signature={signature}",
            input.access_key_id
        ),
    })
}

fn validate_required(input: &SignInput<'_>) -> Result<(), SignError> {
    let required = [
        ("method", input.method),
        ("canonical_uri", input.canonical_uri),
        ("region", input.region),
        ("service", input.service),
        ("access_key_id", input.access_key_id),
        ("secret_access_key", input.secret_access_key),
    ];
    for (field, value) in required {
        if value.is_empty() {
            return Err(SignError::MissingInput(field));
        }
    }
    if !input.canonical_uri.starts_with('/') {
        return Err(SignError::InvalidCanonicalUri);
    }
    Ok(())
}

fn validate_amz_date(value: &str) -> Result<(), SignError> {
    let bytes = value.as_bytes();
    let valid = bytes.len() == 16
        && bytes[8] == b'T'
        && bytes[15] == b'Z'
        && bytes
            .iter()
            .enumerate()
            .all(|(index, byte)| matches!(index, 8 | 15) || byte.is_ascii_digit());
    if valid {
        Ok(())
    } else {
        Err(SignError::InvalidAmzDate)
    }
}

/// `YYYYMMDD'T'HHMMSS'Z'` for `instant`, which is seconds since the Unix epoch.
#[must_use]
pub fn amz_date(unix_secs: u64) -> String {
    let days = unix_secs / 86_400;
    let time = unix_secs % 86_400;
    let (year, month, day) = civil_from_days(i64::try_from(days).unwrap_or(i64::MAX));
    let hour = time / 3_600;
    let minute = (time % 3_600) / 60;
    let second = time % 60;
    format!("{year:04}{month:02}{day:02}T{hour:02}{minute:02}{second:02}Z")
}

/// Single-encoded wire path for one model id. `:` in the id becomes `%3A`.
#[must_use]
pub fn model_wire_uri(model: &str, stream: bool) -> String {
    let method = if stream {
        "converse-stream"
    } else {
        "converse"
    };
    format!("/model/{}/{method}", uri_encode(model))
}

/// URI encoding applied to an already single-encoded wire path.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
#[non_exhaustive]
pub enum UriEncoding {
    /// Encode each segment again, preserving `/`; required except for S3.
    Double,
    /// Keep the wire path unchanged; required for S3.
    Single,
}

/// Canonical URI for a single-encoded wire path.
#[must_use]
pub fn canonical_uri(wire_path: &str, encoding: UriEncoding) -> String {
    match encoding {
        UriEncoding::Double => wire_path
            .split('/')
            .map(uri_encode)
            .collect::<Vec<_>>()
            .join("/"),
        UriEncoding::Single => wire_path.to_owned(),
    }
}

/// Encodes every byte except the unreserved set. Hex digits are uppercase.
#[must_use]
pub(crate) fn uri_encode(value: &str) -> String {
    const HEX: &[u8; 16] = b"0123456789ABCDEF";
    let mut out = String::new();
    for byte in value.bytes() {
        if byte.is_ascii_alphanumeric() || matches!(byte, b'-' | b'_' | b'.' | b'~') {
            out.push(char::from(byte));
        } else {
            out.push('%');
            out.push(char::from(HEX[usize::from(byte >> 4)]));
            out.push(char::from(HEX[usize::from(byte & 0x0f)]));
        }
    }
    out
}

fn signing_key(secret: &str, date: &str, region: &str, service: &str) -> hmac::Tag {
    let secret = format!("AWS4{secret}");
    let date_key = hmac_sha256(secret.as_bytes(), date.as_bytes());
    let region_key = hmac_sha256(date_key.as_ref(), region.as_bytes());
    let service_key = hmac_sha256(region_key.as_ref(), service.as_bytes());
    hmac_sha256(service_key.as_ref(), b"aws4_request")
}

fn hmac_sha256(key: &[u8], data: &[u8]) -> hmac::Tag {
    let key = hmac::Key::new(hmac::HMAC_SHA256, key);
    hmac::sign(&key, data)
}

fn normalize_header_value(value: &str) -> String {
    let mut out = String::new();
    let mut pending_space = false;
    for character in value.trim().chars() {
        if character.is_whitespace() {
            pending_space = true;
            continue;
        }
        if pending_space && !out.is_empty() {
            out.push(' ');
        }
        pending_space = false;
        out.push(character);
    }
    out
}

/// Lowercase hex `SHA-256` of `payload`.
#[must_use]
pub fn payload_hash(payload: &[u8]) -> String {
    hex_encode(digest(&SHA256, payload).as_ref())
}

fn hex_encode(bytes: &[u8]) -> String {
    const HEX: &[u8; 16] = b"0123456789abcdef";
    let mut out = String::with_capacity(bytes.len() * 2);
    for byte in bytes {
        out.push(char::from(HEX[usize::from(byte >> 4)]));
        out.push(char::from(HEX[usize::from(byte & 0x0f)]));
    }
    out
}

/// Days since the Unix epoch to a Gregorian year, month, and day.
///
/// This is Howard Hinnant's `civil_from_days`. The casts are the algorithm's
/// narrowing steps. The inputs that reach it are day counts, not untrusted
/// sizes.
#[allow(
    clippy::cast_possible_truncation,
    clippy::cast_possible_wrap,
    clippy::cast_sign_loss
)]
fn civil_from_days(days: i64) -> (i32, u32, u32) {
    let z = days + 719_468;
    let era = if z >= 0 { z } else { z - 146_096 } / 146_097;
    let day_of_era = (z - era * 146_097) as u64;
    let year_of_era =
        (day_of_era - day_of_era / 1_460 + day_of_era / 36_524 - day_of_era / 146_096) / 365;
    let year = i64::from(year_of_era as i32) + era * 400;
    let day_of_year = day_of_era - (365 * year_of_era + year_of_era / 4 - year_of_era / 100);
    let month_part = (5 * day_of_year + 2) / 153;
    let day = day_of_year - (153 * month_part + 2) / 5 + 1;
    let month = if month_part < 10 {
        month_part + 3
    } else {
        month_part - 9
    };
    let year = if month <= 2 { year + 1 } else { year };
    (year as i32, month as u32, day as u32)
}

#[cfg(test)]
mod tests {
    use super::{
        SignError, SignInput, UriEncoding, amz_date, canonical_uri, hex_encode, model_wire_uri,
        payload_hash, sign,
    };
    use ring::digest::{SHA256, digest};

    #[test]
    fn the_published_get_example_matches() {
        // https://docs.aws.amazon.com/AmazonS3/latest/API/sig-v4-header-based-auth.html
        // The empty-body hash and the signature below are the worked example.
        let payload_hash = "e3b0c44298fc1c149afbf4c8996fb92427ae41e4649b934ca495991b7852b855";
        let headers = [
            ("host", "examplebucket.s3.amazonaws.com"),
            ("range", "bytes=0-9"),
            ("x-amz-content-sha256", payload_hash),
            ("x-amz-date", "20130524T000000Z"),
        ];
        let wire_path = "/test.txt";
        let canonical_path = canonical_uri(wire_path, UriEncoding::Single);
        let input = SignInput::new()
            .method("GET")
            .canonical_uri(&canonical_path)
            .amz_date("20130524T000000Z")
            .scope("us-east-1", "s3")
            .credentials(
                "AKIAIOSFODNN7EXAMPLE",
                "wJalrXUtnFEMI/K7MDENG/bPxRfiCYEXAMPLEKEY",
            )
            .payload(b"")
            .headers(&headers);
        let signed = sign(&input).expect("signature");
        assert_eq!(canonical_path, wire_path);
        assert!(
            signed.authorization().ends_with(
                "Signature=f0e8bdb87c964420e857bd35b5d6ed310bd44f0170aba48dd91039c6036bdb41"
            ),
            "{}",
            signed.authorization()
        );
        assert!(
            signed
                .authorization()
                .contains("SignedHeaders=host;range;x-amz-content-sha256;x-amz-date")
        );
        assert!(
            !signed
                .authorization()
                .contains("wJalrXUtnFEMI/K7MDENG/bPxRfiCYEXAMPLEKEY"),
            "secret access key leaked into Authorization"
        );
    }

    #[test]
    fn aws_get_vanilla_matches_the_botocore_signature() {
        let headers = [
            ("host", "example.amazonaws.com"),
            ("x-amz-date", "20150830T123600Z"),
        ];
        let signed = sign(&aws_suite_input(b"", &headers).method("GET")).expect("signature");
        assert_eq!(
            signed.authorization(),
            "AWS4-HMAC-SHA256 Credential=AKIDEXAMPLE/20150830/us-east-1/service/aws4_request, SignedHeaders=host;x-amz-date, Signature=5fa00fa31553b73ebf1942676e86291e8372ff2a2260956d9b8aae1d763fbf31"
        );
    }

    #[test]
    fn aws_post_sts_token_signs_the_security_token_header() {
        // Literal input and expected value from the AWS SigV4 test suite case
        // post-sts-token/post-sts-header-before.
        let headers = [
            ("host", "example.amazonaws.com"),
            ("x-amz-date", "20150830T123600Z"),
            (
                "x-amz-security-token",
                "AQoDYXdzEPT//////////wEXAMPLEtc764bNrC9SAPBSM22wDOk4x4HIZ8j4FZTwdQWLWsKWHGBuFqwAeMicRXmxfpSPfIeoIYRqTflfKD8YUuwthAx7mSEI/qkPpKPi/kMcGdQrmGdeehM4IC1NtBmUpp2wUE8phUZampKsburEDy0KPkyQDYwT7WZ0wq5VSXDvp75YU9HFvlRd8Tx6q6fE8YQcHNVXAkiY9q6d+xo0rKwT38xVqr7ZD0u0iPPkUL64lIZbqBAz+scqKmlzm8FDrypNC9Yjc8fPOLn9FX9KSYvKTr4rvx3iSIlTJabIQwj2ICCR/oLxBA==",
            ),
        ];
        let signed = sign(&aws_suite_input(b"", &headers)).expect("signature");
        assert_eq!(
            signed.authorization(),
            "AWS4-HMAC-SHA256 Credential=AKIDEXAMPLE/20150830/us-east-1/service/aws4_request, SignedHeaders=host;x-amz-date;x-amz-security-token, Signature=85d96828115b5dc0cfc3bd16ad9e210dd772bbebba041836c64533a82be05ead"
        );
    }

    #[test]
    fn aws_post_x_www_form_urlencoded_matches_the_botocore_signature() {
        let headers = [
            ("content-type", "application/x-www-form-urlencoded"),
            ("host", "example.amazonaws.com"),
            ("x-amz-date", "20150830T123600Z"),
        ];
        let signed = sign(&aws_suite_input(b"Param1=value1", &headers)).expect("signature");
        assert_eq!(
            signed.authorization(),
            "AWS4-HMAC-SHA256 Credential=AKIDEXAMPLE/20150830/us-east-1/service/aws4_request, SignedHeaders=content-type;host;x-amz-date, Signature=ff11897932ad3f4e8b18135d722051e5ac45fc38421b1da7b9d196a0fe09473a"
        );
    }

    #[test]
    fn aws_post_signs_the_payload_hash_header_and_body() {
        let payload = b"hello world";
        let payload_hash = "b94d27b9934d3e08a52e52d7da7dabfac484efe37a5380ee9088f7ace2efcde9";
        assert_eq!(super::payload_hash(payload), payload_hash);
        let headers = [
            ("content-type", "text/plain"),
            ("host", "example.amazonaws.com"),
            ("x-amz-content-sha256", payload_hash),
            ("x-amz-date", "20150830T123600Z"),
        ];
        let signed = sign(&aws_suite_input(payload, &headers)).expect("signature");
        assert_eq!(
            signed.authorization(),
            "AWS4-HMAC-SHA256 Credential=AKIDEXAMPLE/20150830/us-east-1/service/aws4_request, SignedHeaders=content-type;host;x-amz-content-sha256;x-amz-date, Signature=fb75efa750088b7dba527c9728573483563dc8d3a2a4b924d2a49fff245bf8f1"
        );
    }

    fn aws_suite_input<'a>(payload: &'a [u8], headers: &'a [(&'a str, &'a str)]) -> SignInput<'a> {
        SignInput::new()
            .method("POST")
            .canonical_uri("/")
            .amz_date("20150830T123600Z")
            .scope("us-east-1", "service")
            .credentials("AKIDEXAMPLE", "wJalrXUtnFEMI/K7MDENG+bPxRfiCYEXAMPLEKEY")
            .payload(payload)
            .headers(headers)
    }

    #[test]
    fn a_bedrock_colon_path_double_encodes_only_the_canonical_uri() {
        let body = br#"{"messages":[{"role":"user","content":[{"text":"hi"}]}]}"#;
        let wire_path = "/model/anthropic.claude-3-5-sonnet-20240620-v1%3A0/converse";
        let canonical_path = canonical_uri(wire_path, UriEncoding::Double);
        assert_eq!(
            canonical_path,
            "/model/anthropic.claude-3-5-sonnet-20240620-v1%253A0/converse"
        );
        assert_eq!(
            payload_hash(body),
            "c3335e07d6e89650bf94cb98664be69f8774dbc3198df221f113db0a59ea14f0"
        );
        let canonical = format!(
            "POST\n{canonical_path}\n\ncontent-type:application/json\nhost:bedrock-runtime.us-east-1.amazonaws.com\nx-amz-date:20150830T123600Z\nx-amz-security-token:SESSIONTOKENEXAMPLE\n\ncontent-type;host;x-amz-date;x-amz-security-token\n{}",
            payload_hash(body)
        );
        assert_eq!(
            hex_encode(digest(&SHA256, canonical.as_bytes()).as_ref()),
            "3a817f5d8a46943678ec48ddb779d03ffabebf26b48536b865a531a486c1144e"
        );
        let headers = [
            ("content-type", "application/json"),
            ("host", "bedrock-runtime.us-east-1.amazonaws.com"),
            ("x-amz-date", "20150830T123600Z"),
            ("x-amz-security-token", "SESSIONTOKENEXAMPLE"),
        ];
        let input = SignInput::new()
            .method("POST")
            .canonical_uri(&canonical_path)
            .amz_date("20150830T123600Z")
            .scope("us-east-1", "bedrock")
            .credentials("AKIDEXAMPLE", "wJalrXUtnFEMI/K7MDENG+bPxRfiCYEXAMPLEKEY")
            .payload(body)
            .headers(&headers);
        let signed = sign(&input).expect("signature");
        assert_eq!(
            signed.authorization(),
            "AWS4-HMAC-SHA256 Credential=AKIDEXAMPLE/20150830/us-east-1/bedrock/aws4_request, SignedHeaders=content-type;host;x-amz-date;x-amz-security-token, Signature=263842d2c13efd2400d401a36125c20f9bcc0f5e6a53e3431a9489b9da75e991"
        );
    }

    #[test]
    fn a_model_colon_is_percent_encoded_on_the_wire_path() {
        assert_eq!(
            model_wire_uri("anthropic.claude-3-sonnet-20240229-v1:0", false),
            "/model/anthropic.claude-3-sonnet-20240229-v1%3A0/converse"
        );
        assert!(model_wire_uri("us.amazon.nova-pro-v1:0", true).ends_with("/converse-stream"));
    }

    #[test]
    fn missing_required_inputs_are_rejected_without_exposing_the_secret() {
        assert_eq!(
            sign(&SignInput::new()).expect_err("empty input"),
            SignError::MissingInput("method")
        );
        let cases = [
            ("method", valid_input().method("")),
            ("canonical_uri", valid_input().canonical_uri("")),
            ("region", valid_input().scope("", "bedrock")),
            ("service", valid_input().scope("us-east-1", "")),
            (
                "access_key_id",
                valid_input().credentials("", "TEST-SECRET-VALUE"),
            ),
            (
                "secret_access_key",
                valid_input().credentials("AKIDEXAMPLE", ""),
            ),
        ];
        for (field, input) in cases {
            let error = sign(&input).expect_err(field);
            assert_eq!(error, SignError::MissingInput(field));
            let text = error.to_string();
            assert!(text.contains(field), "{text}");
            assert!(!text.contains("TEST-SECRET-VALUE"), "{text}");
        }
    }

    #[test]
    fn a_relative_canonical_uri_is_rejected() {
        let error = sign(&valid_input().canonical_uri("model/test")).expect_err("relative");
        assert_eq!(error, SignError::InvalidCanonicalUri);
        assert!(!error.to_string().contains("TEST-SECRET-VALUE"));
    }

    #[test]
    fn a_malformed_sigv4_date_is_an_error() {
        for date in ["", "20150830", "20150830T12360Z", "20150830X123600Z"] {
            let error = sign(&valid_input().amz_date(date)).expect_err("date");
            assert_eq!(error, SignError::InvalidAmzDate);
        }
    }

    fn valid_input() -> SignInput<'static> {
        SignInput::new()
            .method("POST")
            .canonical_uri("/model/test")
            .amz_date("20150830T123600Z")
            .scope("us-east-1", "bedrock")
            .credentials("AKIDEXAMPLE", "TEST-SECRET-VALUE")
    }

    #[test]
    fn the_example_instant_formats_as_the_sigv4_date() {
        assert_eq!(amz_date(0), "19700101T000000Z");
        assert_eq!(amz_date(1_369_353_600), "20130524T000000Z");
    }
}
