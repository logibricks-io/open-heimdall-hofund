//! Gateway session identity from client headers.

use crate::conversation::SESSION_HEADERS;

/// Header that echoes the session id the client named on the request.
pub const ECHO_SESSION_HEADER: &str = "x-hofund-session";

const SESSION_VALUE_MAX_BYTES: usize = 128;

/// Trims `value` and cuts it to at most 128 bytes on a UTF-8 boundary.
#[must_use]
pub fn trim_session_value(value: &str) -> String {
    let trimmed = value.trim();
    if trimmed.is_empty() {
        return String::new();
    }
    if trimmed.len() <= SESSION_VALUE_MAX_BYTES {
        return trimmed.to_owned();
    }
    let mut end = SESSION_VALUE_MAX_BYTES;
    while !trimmed.is_char_boundary(end) {
        end -= 1;
    }
    trimmed[..end].to_owned()
}

fn header<'a>(headers: &[(&str, &'a str)], name: &str) -> Option<&'a str> {
    headers
        .iter()
        .find(|(candidate, _)| candidate.eq_ignore_ascii_case(name))
        .map(|(_, value)| *value)
}

/// Returns the session id from the first non-empty header in `names`.
#[must_use]
pub fn session_of(names: &[&str], headers: &[(&str, &str)]) -> String {
    for name in names {
        if let Some(value) = header(headers, name)
            && let trimmed = trim_session_value(value)
            && !trimmed.is_empty()
        {
            return trimmed;
        }
    }
    String::new()
}

/// First non-empty value among kernel [`SESSION_HEADERS`], ignoring the echo header.
#[must_use]
pub fn native_session_of(headers: &[(&str, &str)]) -> String {
    for name in SESSION_HEADERS {
        if let Some(value) = header(headers, name)
            && let trimmed = trim_session_value(value)
            && !trimmed.is_empty()
        {
            return trimmed;
        }
    }
    String::new()
}

/// Kernel echo header, then [`SESSION_HEADERS`], for callers that add plugin names later.
#[must_use]
pub fn kernel_session_header_names() -> Vec<String> {
    let mut names = Vec::with_capacity(1 + SESSION_HEADERS.len());
    names.push(ECHO_SESSION_HEADER.to_owned());
    names.extend(SESSION_HEADERS.iter().map(|name| (*name).to_owned()));
    names
}

/// Appends plugin-registered names that are not already listed (case-insensitive).
#[must_use]
pub fn merge_extra_session_header_names(mut names: Vec<String>, extra: &[String]) -> Vec<String> {
    for candidate in extra {
        if names
            .iter()
            .any(|existing| existing.eq_ignore_ascii_case(candidate))
        {
            continue;
        }
        names.push(candidate.clone());
    }
    names
}

#[cfg(test)]
mod tests {
    use super::{
        ECHO_SESSION_HEADER, SESSION_HEADERS, kernel_session_header_names,
        merge_extra_session_header_names, native_session_of, session_of, trim_session_value,
    };

    #[test]
    fn r33_trim_session_value_respects_utf8_boundary() {
        let wide = "é".repeat(200);
        let trimmed = trim_session_value(&wide);
        assert!(trimmed.len() <= 128);
        assert!(std::str::from_utf8(trimmed.as_bytes()).is_ok());

        // "x" shifts the 128-byte cut into the middle of an `é`, so the
        // boundary walk actually runs (a pure "é" repeat lands on a boundary).
        let shifted = format!("x{}", "é".repeat(200));
        let trimmed = trim_session_value(&shifted);
        assert_eq!(trimmed.len(), 127);
        assert!(trimmed.starts_with('x'));
        assert!(std::str::from_utf8(trimmed.as_bytes()).is_ok());
        assert!(!shifted.is_char_boundary(128));
    }

    #[test]
    fn r33_kernel_session_header_names_are_echo_then_session_headers() {
        let names = kernel_session_header_names();
        assert_eq!(names.first().map(String::as_str), Some(ECHO_SESSION_HEADER));
        assert_eq!(names.len(), 1 + SESSION_HEADERS.len());
        for header in SESSION_HEADERS {
            assert!(
                names.iter().any(|name| name == header),
                "missing {header} in {names:?}"
            );
        }
    }

    #[test]
    fn r33_session_of_prefers_echo_header() {
        let names = [ECHO_SESSION_HEADER, SESSION_HEADERS[0]];
        let headers = [
            (ECHO_SESSION_HEADER, " echo-1 "),
            (SESSION_HEADERS[0], "native"),
        ];
        assert_eq!(session_of(&names, &headers), "echo-1");
        assert_eq!(native_session_of(&headers), "native");
    }

    #[test]
    fn r33_native_empty_when_only_echo_is_set() {
        let headers = [(ECHO_SESSION_HEADER, "only-echo")];
        assert_eq!(native_session_of(&headers), "");
    }

    #[test]
    fn r33_merge_extra_dedupes_kernel_names() {
        let names = merge_extra_session_header_names(
            super::kernel_session_header_names(),
            &[
                ECHO_SESSION_HEADER.to_uppercase(),
                "x-custom-session".to_owned(),
            ],
        );
        assert!(names.iter().any(|name| name == "x-custom-session"));
        assert_eq!(
            names
                .iter()
                .filter(|name| name.eq_ignore_ascii_case(ECHO_SESSION_HEADER))
                .count(),
            1
        );
    }
}
