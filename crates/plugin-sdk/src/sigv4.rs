//! AWS Signature Version 4 for one Bedrock Runtime request.
//!
//! The canonical request, the string to sign, and the signing key follow
//! <https://docs.aws.amazon.com/IAM/latest/UserGuide/reference_sigv-create-signed-request.html>
//! (checked 2026-10-25). The service name for the runtime is `bedrock`.
//! Percent-encoding uses uppercase hex. The payload hash is lowercase hex.

use ring::digest::{SHA256, digest};
use ring::hmac;

/// One signed request's `Authorization` value and the headers that were signed
/// with it. The caller sends these headers unchanged.
pub struct Signature {
    /// `AWS4-HMAC-SHA256 Credential=...`.
    pub authorization: String,
}

/// Inputs to [`sign`], other than the headers.
pub struct SignInput<'a> {
    /// HTTP method, already uppercase.
    pub method: &'a str,
    /// Absolute path, already URI-encoded the way `SigV4` requires.
    pub canonical_uri: &'a str,
    /// Sorted, not-yet-encoded query. Empty when there is no query.
    pub canonical_query: &'a str,
    /// `YYYYMMDD'T'HHMMSS'Z'`.
    pub amz_date: &'a str,
    /// Signing region, such as `us-east-1`.
    pub region: &'a str,
    /// Signing service. Bedrock Runtime is `bedrock`.
    pub service: &'a str,
    /// Access key id. It is copied into `Authorization`.
    pub access_key_id: &'a str,
    /// Secret access key. It is not copied into the result.
    pub secret_access_key: &'a str,
    /// Raw body. The hash is what gets signed.
    pub payload: &'a [u8],
    /// Headers to sign, excluding `Authorization`. Names are matched
    /// case-insensitively and emitted in lowercase.
    pub headers: &'a [(&'a str, &'a str)],
}

/// Signs `input`. Header values are trimmed and their internal whitespace is
/// collapsed, which is the canonical form a verifier rebuilds.
///
/// The secret access key is not copied into [`Signature`].
#[must_use]
pub fn sign(input: &SignInput<'_>) -> Signature {
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
    Signature {
        authorization: format!(
            "AWS4-HMAC-SHA256 Credential={}/{scope}, SignedHeaders={signed_names}, Signature={signature}",
            input.access_key_id
        ),
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

/// `SigV4` path for one model id. `:` in the id becomes `%3A`.
#[must_use]
pub fn canonical_model_uri(model: &str, stream: bool) -> String {
    let method = if stream {
        "converse-stream"
    } else {
        "converse"
    };
    format!("/model/{}/{method}", uri_encode(model))
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
    use super::{SignInput, amz_date, canonical_model_uri, sign};

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
        let signed = sign(&SignInput {
            method: "GET",
            canonical_uri: "/test.txt",
            canonical_query: "",
            amz_date: "20130524T000000Z",
            region: "us-east-1",
            service: "s3",
            access_key_id: "AKIAIOSFODNN7EXAMPLE",
            secret_access_key: "wJalrXUtnFEMI/K7MDENG/bPxRfiCYEXAMPLEKEY",
            payload: b"",
            headers: &headers,
        });
        assert!(
            signed.authorization.ends_with(
                "Signature=f0e8bdb87c964420e857bd35b5d6ed310bd44f0170aba48dd91039c6036bdb41"
            ),
            "{}",
            signed.authorization
        );
        assert!(
            signed
                .authorization
                .contains("SignedHeaders=host;range;x-amz-content-sha256;x-amz-date")
        );
        assert!(
            !signed
                .authorization
                .contains("wJalrXUtnFEMI/K7MDENG/bPxRfiCYEXAMPLEKEY"),
            "secret access key leaked into Authorization"
        );
    }

    #[test]
    fn a_model_colon_is_percent_encoded() {
        assert_eq!(
            canonical_model_uri("anthropic.claude-3-sonnet-20240229-v1:0", false),
            "/model/anthropic.claude-3-sonnet-20240229-v1%3A0/converse"
        );
        assert!(canonical_model_uri("us.amazon.nova-pro-v1:0", true).ends_with("/converse-stream"));
    }

    #[test]
    fn the_example_instant_formats_as_the_sigv4_date() {
        assert_eq!(amz_date(0), "19700101T000000Z");
        assert_eq!(amz_date(1_369_353_600), "20130524T000000Z");
    }
}
