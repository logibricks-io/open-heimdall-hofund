// Build-time checks shared by `build.rs` and the registry tests.
// This file has no `crate::` imports so `build.rs` can `include!` it.

use std::collections::BTreeSet;

/// A route declared in `plugin.toml`.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct DeclaredRoute {
    /// Plugin id.
    pub plugin: String,
    /// HTTP method, such as `GET`.
    pub method: String,
    /// Path prefix.
    pub prefix: String,
}

/// Whether `id` is reserved for infrastructure.
#[must_use]
pub fn reserved(id: &str) -> bool {
    matches!(id, "api" | "sdk" | "host" | "registry")
}

/// Factory ids compiled into the registry beside the plugins.
#[must_use]
pub fn builtin_factory_ids() -> &'static [&'static str] {
    &["anthropic", "generative", "openai-compat"]
}

/// True when `id` is not a kebab-case preset id.
#[must_use]
pub fn invalid_preset_id(id: &str) -> bool {
    let bytes = id.as_bytes();
    if bytes.is_empty() {
        return true;
    }
    let mut prev_hyphen = true;
    for byte in bytes {
        if *byte == b'-' {
            if prev_hyphen {
                return true;
            }
            prev_hyphen = true;
        } else if byte.is_ascii_lowercase() || byte.is_ascii_digit() {
            prev_hyphen = false;
        } else {
            return true;
        }
    }
    prev_hyphen
}

/// The first repeated id, if any.
#[must_use]
pub fn duplicate_id<'a>(ids: &[&'a str]) -> Option<&'a str> {
    let mut seen = BTreeSet::new();
    for id in ids {
        if !seen.insert(*id) {
            return Some(*id);
        }
    }
    None
}

/// True when `ids` is strictly increasing.
#[must_use]
pub fn strictly_sorted(ids: &[&str]) -> bool {
    ids.windows(2).all(|pair| pair[0] < pair[1])
}

/// True when `prefix` is not a route prefix.
#[must_use]
pub fn invalid_prefix(prefix: &str) -> bool {
    let bytes = prefix.as_bytes();
    if bytes.first() != Some(&b'/') || prefix.contains(['"', '\\']) {
        return true;
    }
    bytes
        .iter()
        .any(|byte| *byte == b'?' || *byte == b'#' || byte.is_ascii_whitespace())
}

/// True when `method` is not an HTTP method the gateway can mount.
#[must_use]
pub fn invalid_method(method: &str) -> bool {
    !matches!(method, "GET" | "POST" | "PUT" | "PATCH" | "DELETE")
}

/// True when two prefixes are equal or one is a boundary prefix of the other.
///
/// `/api` conflicts with `/api/tags`. `/api` does not conflict with `/apiv2`.
#[must_use]
pub fn prefixes_conflict(left: &str, right: &str) -> bool {
    if left == right || left == "/" || right == "/" {
        return true;
    }
    let (shorter, longer) = if left.len() <= right.len() {
        (left, right)
    } else {
        (right, left)
    };
    longer.starts_with(shorter) && longer.as_bytes().get(shorter.len()) == Some(&b'/')
}

/// True when `path` is `prefix` or a path under it. `/` matches every path.
#[must_use]
pub fn path_matches(prefix: &str, path: &str) -> bool {
    path == prefix
        || prefix == "/"
        || (path.len() > prefix.len()
            && path.starts_with(prefix)
            && path.as_bytes()[prefix.len()] == b'/')
}

/// A description of the first same-method prefix conflict.
#[must_use]
pub fn route_conflicts(routes: &[DeclaredRoute]) -> Option<String> {
    for (index, left) in routes.iter().enumerate() {
        for right in routes.iter().skip(index + 1) {
            if left.method.eq_ignore_ascii_case(&right.method)
                && prefixes_conflict(&left.prefix, &right.prefix)
            {
                return Some(format!(
                    "{} {} {} conflicts with {} {} {}",
                    left.plugin, left.method, left.prefix, right.plugin, right.method, right.prefix
                ));
            }
        }
    }
    None
}

#[cfg(test)]
mod tests {
    use super::{
        DeclaredRoute, duplicate_id, invalid_prefix, path_matches, prefixes_conflict,
        route_conflicts, strictly_sorted,
    };

    fn route(plugin: &str, method: &str, prefix: &str) -> DeclaredRoute {
        DeclaredRoute {
            plugin: plugin.to_owned(),
            method: method.to_owned(),
            prefix: prefix.to_owned(),
        }
    }

    #[test]
    fn prefixes_conflict_on_a_boundary_only() {
        assert!(prefixes_conflict("/api", "/api"));
        assert!(prefixes_conflict("/api", "/api/tags"));
        assert!(prefixes_conflict("/", "/hofund/echo"));
        assert!(!prefixes_conflict("/api", "/apiv2"));
        assert!(!prefixes_conflict("/hofund/echo", "/hofund/echov2"));
    }

    #[test]
    fn the_same_path_on_two_methods_is_not_a_conflict() {
        let routes = vec![
            route("echo", "GET", "/hofund/echo"),
            route("write", "POST", "/hofund/echo"),
        ];
        assert!(route_conflicts(&routes).is_none());
        let nested = vec![
            route("echo", "GET", "/hofund/echo"),
            route("other", "GET", "/hofund/echo/more"),
        ];
        assert!(route_conflicts(&nested).is_some());
    }

    #[test]
    fn path_match_requires_a_boundary() {
        assert!(path_matches("/hofund/echo", "/hofund/echo"));
        assert!(path_matches("/hofund/echo", "/hofund/echo/nested"));
        assert!(!path_matches("/hofund/echo", "/hofund/echov2"));
        assert!(path_matches("/", "/anywhere"));
    }

    #[test]
    fn ids_must_be_unique_and_sorted() {
        assert_eq!(duplicate_id(&["echo", "relay", "echo"]), Some("echo"));
        assert!(duplicate_id(&["echo", "relay"]).is_none());
        assert!(strictly_sorted(&["echo", "relay"]));
        assert!(!strictly_sorted(&["relay", "echo"]));
        assert!(!strictly_sorted(&["echo", "echo"]));
        assert!(invalid_prefix("hofund"));
        assert!(invalid_prefix("/hofund echo"));
        assert!(!invalid_prefix("/hofund/echo"));
        assert!(super::invalid_preset_id(""));
        assert!(super::invalid_preset_id("OpenAI"));
        assert!(super::invalid_preset_id("-relay"));
        assert!(!super::invalid_preset_id("relay"));
        assert!(!super::invalid_preset_id("openai-compat"));
    }
}
