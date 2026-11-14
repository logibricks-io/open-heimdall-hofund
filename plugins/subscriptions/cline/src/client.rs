//! Client identity and task headers used by the reference CLI.

use hofund_plugin_api::Request;
use hofund_plugin_sdk::digest::sha256;

pub(crate) const CLIENT_TYPE: &str = "cline-cli";
pub(crate) const CLIENT_VERSION: &str = "3.0.68";
pub(crate) const PLATFORM: &str = "cli";
pub(crate) const CORE_VERSION: &str = "0.0.90";

const CLIENT_HEADERS: &str = "hofund_client_request_headers";
const SESSION_HEADERS: &[&str] = &[
    "x-opencode-session",
    "x-session-affinity",
    "x-session-id",
    "session_id",
    "session-id",
    "x-claude-code-session-id",
    "x-conversation-id",
    "x-deepseek-harness-session-id",
];

/// Headers every Cline API call carries, optionally with a task id.
#[must_use]
pub(crate) fn headers(task_id: Option<&str>) -> Vec<(String, String)> {
    let mut out = vec![
        ("HTTP-Referer".to_owned(), "https://cline.bot".to_owned()),
        ("X-Title".to_owned(), "Cline".to_owned()),
        ("User-Agent".to_owned(), format!("Cline/{CLIENT_VERSION}")),
        ("X-IS-MULTIROOT".to_owned(), "false".to_owned()),
        ("X-CLIENT-TYPE".to_owned(), CLIENT_TYPE.to_owned()),
        ("X-CLIENT-VERSION".to_owned(), CLIENT_VERSION.to_owned()),
        ("X-PLATFORM".to_owned(), PLATFORM.to_owned()),
        ("X-PLATFORM-VERSION".to_owned(), CLIENT_VERSION.to_owned()),
        ("X-CORE-VERSION".to_owned(), CORE_VERSION.to_owned()),
    ];
    if let Some(task) = task_id.filter(|id| !id.is_empty()) {
        out.push(("X-Task-ID".to_owned(), task.to_owned()));
    }
    out
}

/// Derives the stable task id for the client session on a routed request.
#[must_use]
pub(crate) fn task_id(request: &Request) -> Option<String> {
    session_of(request).map(|session| task_id_of(&session))
}

fn session_of(request: &Request) -> Option<String> {
    let headers = request.extensions.get(CLIENT_HEADERS)?.as_object()?;
    for name in SESSION_HEADERS {
        let Some(session) = headers
            .iter()
            .find(|(key, _)| key.eq_ignore_ascii_case(name))
            .and_then(|(_, value)| value.as_str())
            .map(str::trim)
            .filter(|value| !value.is_empty())
        else {
            continue;
        };
        if synthetic(session) {
            return None;
        }
        return Some(truncate(session, 128).to_owned());
    }
    None
}

fn synthetic(session: &str) -> bool {
    let bytes = session.as_bytes();
    bytes.len() == 31
        && bytes[..6].iter().all(u8::is_ascii_lowercase)
        && bytes[6] == b'-'
        && bytes[7..]
            .iter()
            .all(|byte| byte.is_ascii_digit() || (b'a'..=b'f').contains(byte))
}

fn truncate(value: &str, maximum: usize) -> &str {
    if value.len() <= maximum {
        return value;
    }
    let mut end = maximum;
    while !value.is_char_boundary(end) {
        end -= 1;
    }
    &value[..end]
}

fn task_id_of(session: &str) -> String {
    let mut bytes = b"cline task\0".to_vec();
    bytes.extend_from_slice(session.as_bytes());
    let mut digest = sha256(&bytes);
    digest[6] = (digest[6] & 0x0f) | 0x40;
    digest[8] = (digest[8] & 0x3f) | 0x80;
    let hex = digest[..16]
        .iter()
        .map(|byte| format!("{byte:02x}"))
        .collect::<String>();
    format!(
        "{}-{}-{}-{}-{}",
        &hex[..8],
        &hex[8..12],
        &hex[12..16],
        &hex[16..20],
        &hex[20..]
    )
}

#[cfg(test)]
mod tests {
    use super::*;
    use hofund_plugin_sdk::json::{Value, json};

    fn request(headers: Value) -> Request {
        let mut request = Request::new("model");
        request.extensions.insert(CLIENT_HEADERS, headers);
        request
    }

    #[test]
    fn task_id_matches_the_reference_vector() {
        let first = request(json!({"x-session-id": " session-one "}));
        assert_eq!(
            task_id(&first).as_deref(),
            Some("c9263bb5-ed92-4871-8e30-db89ef159f5a")
        );
        let other = request(json!({"x-session-id": "session-two"}));
        assert_ne!(task_id(&first), task_id(&other));
    }

    #[test]
    fn empty_and_synthetic_sessions_have_no_task_id() {
        for headers in [
            json!({"x-session-id": "  "}),
            json!({"x-session-id": "hofund-0123456789abcdef01234567"}),
            json!({"x-session-id": "client-0123456789abcdef01234567"}),
            json!({}),
        ] {
            assert_eq!(task_id(&request(headers)), None);
        }
    }
}
