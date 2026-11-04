//! Anthropic beta refusals learned from upstream errors.

use serde_json::Value;

use crate::state::RequestState;

/// Parses beta names a provider refused from an error body.
#[must_use]
pub fn named_refused_betas(body: &[u8]) -> Vec<String> {
    let text = String::from_utf8_lossy(body);
    if let Some(names) = unsupported_beta_names(&text) {
        return names;
    }
    unexpected_beta_names(&text)
}

fn unsupported_beta_names(text: &str) -> Option<Vec<String>> {
    let lower = text.to_ascii_lowercase();
    let marker = "unsupported beta header";
    let at = lower.find(marker)?;
    let rest = &text[at + marker.len()..];
    let rest = rest.trim_start_matches(['s', '(']);
    let rest = rest.trim_start_matches(|c: char| c == ')' || c == ':' || c.is_whitespace());
    let end = rest.find(['\n', '"', '.']).unwrap_or(rest.len());
    let list = rest[..end].trim();
    if list.is_empty() {
        return None;
    }
    Some(
        list.split(',')
            .map(str::trim)
            .filter(|name| !name.is_empty())
            .map(str::to_owned)
            .collect(),
    )
}

fn unexpected_beta_names(text: &str) -> Vec<String> {
    let lower = text.to_ascii_lowercase();
    let Some(at) = lower.find("unexpected value") else {
        return Vec::new();
    };
    let rest = &text[at..];
    let Some(for_at) = rest.to_ascii_lowercase().find("for the") else {
        return Vec::new();
    };
    let segment = &rest[..for_at];
    segment
        .split('`')
        .enumerate()
        .filter_map(|(index, piece)| (index % 2 == 1).then_some(piece.trim()))
        .filter(|name| !name.is_empty() && !name.eq_ignore_ascii_case("anthropic-beta"))
        .map(str::to_owned)
        .collect()
}

/// Removes refused betas from a Messages body and header-shaped lists in JSON.
#[must_use]
pub fn strip_body_betas(body: &[u8], drop: impl Fn(&str) -> bool) -> (Vec<u8>, bool) {
    let Ok(mut value) = serde_json::from_slice::<Value>(body) else {
        return (body.to_vec(), false);
    };
    let Some(object) = value.as_object_mut() else {
        return (body.to_vec(), false);
    };
    let mut changed = false;
    if let Some(betas) = object.get_mut("anthropic_beta") {
        changed |= fit_beta_value(betas, &drop);
        if betas.as_array().is_some_and(|values| values.is_empty()) {
            object.remove("anthropic_beta");
            changed = true;
        }
    }
    if !changed {
        return (body.to_vec(), false);
    }
    match serde_json::to_vec(&value) {
        Ok(out) => (out, true),
        Err(_) => (body.to_vec(), false),
    }
}

fn fit_beta_value(value: &mut Value, drop: &impl Fn(&str) -> bool) -> bool {
    match value {
        Value::Array(values) => {
            let before = values.len();
            values.retain(|entry| entry.as_str().is_none_or(|name| !drop(name.trim())));
            before != values.len()
        }
        Value::String(name) if drop(name.trim()) => {
            *value = Value::Null;
            true
        }
        Value::String(_) => false,
        _ => false,
    }
}

/// Applies learned beta refusals before a Messages request is sent.
#[must_use]
pub fn apply_learned_betas(state: &RequestState, body: &[u8]) -> Vec<u8> {
    strip_body_betas(body, |name| !state.fits_beta(name)).0
}

/// Reads refused betas from `err_body`, remembers them, and returns a retry body.
#[must_use]
pub fn retry_without_refused_betas(
    state: &RequestState,
    request_body: &[u8],
    err_body: &[u8],
) -> Option<Vec<u8>> {
    let mut fresh = false;
    for name in named_refused_betas(err_body) {
        if state.learn_beta(&name) {
            fresh = true;
        }
    }
    if !fresh {
        return None;
    }
    let (stripped, changed) = strip_body_betas(request_body, |name| !state.fits_beta(name));
    changed.then_some(stripped)
}

#[cfg(test)]
mod tests {
    use super::{apply_learned_betas, named_refused_betas, retry_without_refused_betas};
    use crate::state::RequestState;

    #[test]
    fn d64_1_unexpected_beta_names_are_parsed() {
        let body = br#"Unexpected value(s) `tool-search-tool-2025-10-19`, `effort-2025-11-24` for the `anthropic-beta` header"#;
        assert_eq!(
            named_refused_betas(body),
            vec![
                "tool-search-tool-2025-10-19".to_owned(),
                "effort-2025-11-24".to_owned()
            ]
        );
    }

    #[test]
    fn d64_2_unsupported_beta_names_are_parsed() {
        let body =
            br#"Relay: unsupported beta header(s): tool-search-tool-2025-10-19, effort-2025-11-24"#;
        assert_eq!(
            named_refused_betas(body),
            vec![
                "tool-search-tool-2025-10-19".to_owned(),
                "effort-2025-11-24".to_owned()
            ]
        );
    }

    #[test]
    fn d64_3_refused_betas_are_stripped_and_remembered() {
        let state = RequestState::default();
        let body = br#"{"model":"demo","anthropic_beta":["keep","drop"],"messages":[]}"#;
        let retry =
            retry_without_refused_betas(&state, body, br#"unsupported beta header(s): drop"#)
                .expect("retry");
        assert!(!retry.windows(b"drop".len()).any(|window| window == b"drop"));
        assert!(retry.windows(b"keep".len()).any(|window| window == b"keep"));
        let again = apply_learned_betas(&state, body);
        assert_eq!(retry, again);
    }

    #[test]
    fn d64_4_unknown_refusal_does_not_retry() {
        let state = RequestState::default();
        let body = br#"{"model":"demo","anthropic_beta":["keep"],"messages":[]}"#;
        assert!(retry_without_refused_betas(&state, body, b"bad input").is_none());
    }
}
