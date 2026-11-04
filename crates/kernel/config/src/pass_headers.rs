//! Client request headers a provider may forward upstream.

/// Extension bag key for client headers copied onto a routed request.
pub const CLIENT_MAP: &str = "hofund_client_request_headers";

/// Names never forwarded, listed or not.
const NEVER: &[&str] = &[
    "authorization",
    "x-api-key",
    "x-goog-api-key",
    "api-key",
    "cookie",
    "set-cookie",
    "set-cookie2",
    "www-authenticate",
    "authentication-info",
    "host",
    "connection",
    "keep-alive",
    "te",
    "trailer",
    "transfer-encoding",
    "upgrade",
    "expect",
    "accept",
    "accept-encoding",
    "origin",
];

const NEVER_PREFIX: &[&str] = &["content-", "proxy-", "sec-", "x-hofund-"];

/// Whether `name` may appear in a provider's pass list or be copied from a client.
#[must_use]
pub fn passable(name: &str) -> bool {
    let name = name.trim();
    if !header_name(name) {
        return false;
    }
    if NEVER
        .iter()
        .any(|blocked| name.eq_ignore_ascii_case(blocked))
    {
        return false;
    }
    !NEVER_PREFIX.iter().any(|prefix| {
        name.len() >= prefix.len() && name[..prefix.len()].eq_ignore_ascii_case(prefix)
    })
}

fn header_name(name: &str) -> bool {
    let mut chars = name.chars();
    let Some(first) = chars.next() else {
        return false;
    };
    if !first.is_ascii_alphanumeric() {
        return false;
    }
    chars.all(|ch| {
        ch.is_ascii_alphanumeric()
            || matches!(
                ch,
                '!' | '#'
                    | '$'
                    | '%'
                    | '&'
                    | '\''
                    | '*'
                    | '+'
                    | '.'
                    | '^'
                    | '_'
                    | '`'
                    | '|'
                    | '~'
                    | '-'
            )
    })
}

/// Returns an error message when `names` is invalid, or `None` when ok.
#[must_use]
pub fn list_error(names: &[String]) -> Option<String> {
    for name in names {
        let trimmed = name.trim();
        if trimmed.is_empty() {
            continue;
        }
        if !header_name(trimmed) {
            return Some(format!("\"{trimmed}\" is not a header name"));
        }
        if !passable(trimmed) {
            return Some(format!(
                "{trimmed} is never passed on: it carries a key, or the gateway or the connection writes it"
            ));
        }
    }
    None
}

/// Saved list: canonical names, each once, non-passable names dropped.
#[must_use]
pub fn clean(names: &[String]) -> Vec<String> {
    let mut out = Vec::new();
    for name in names {
        let trimmed = name.trim();
        if trimmed.is_empty() || !passable(trimmed) {
            continue;
        }
        let canonical = trimmed.to_owned();
        if !out
            .iter()
            .any(|existing: &String| existing.eq_ignore_ascii_case(&canonical))
        {
            out.push(canonical);
        }
    }
    out
}

/// Reads a comma-separated pass list from TOML.
#[must_use]
pub fn parse_list(value: &str) -> Vec<String> {
    value
        .split(',')
        .map(str::trim)
        .filter(|part| !part.is_empty())
        .map(str::to_owned)
        .collect()
}

/// Validates and cleans a pass list for one provider.
pub fn validated(provider: &str, names: Vec<String>) -> Result<Vec<String>, super::ConfigError> {
    if let Some(message) = list_error(&names) {
        return Err(super::ConfigError::Invalid(format!(
            "provider `{provider}` pass_headers: {message}"
        )));
    }
    Ok(clean(&names))
}

#[cfg(test)]
mod tests {
    use super::{clean, list_error, passable, validated};

    #[test]
    fn d62_4_forbidden_names_rejected_at_load() {
        assert!(!passable("Authorization"));
        assert!(!passable("x-hofund-trace"));
        assert!(!passable("Content-Type"));
        assert!(list_error(&["Authorization".to_owned()]).is_some());
        assert!(validated("p", vec!["cookie".to_owned()]).is_err());
        assert!(clean(&["Authorization".to_owned()]).is_empty());
    }
}
