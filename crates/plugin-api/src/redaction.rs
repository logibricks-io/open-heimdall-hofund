/// Masks standalone occurrences of `secret` with `[redacted]`.
///
/// Secrets shorter than eight characters are left unchanged so a short value
/// cannot garble unrelated text. A match adjacent to a letter, digit,
/// underscore, or hyphen is part of a larger token and is also left unchanged.
#[must_use]
pub fn redact_token(text: &str, secret: &str) -> String {
    if secret.chars().count() < 8 {
        return text.to_owned();
    }
    let mut clean = String::with_capacity(text.len());
    let mut remaining = text;
    while let Some(index) = remaining.find(secret) {
        let end = index + secret.len();
        let token_before = remaining[..index]
            .chars()
            .next_back()
            .is_some_and(token_char);
        let token_after = remaining[end..].chars().next().is_some_and(token_char);
        clean.push_str(&remaining[..index]);
        if token_before || token_after {
            clean.push_str(secret);
        } else {
            clean.push_str("[redacted]");
        }
        remaining = &remaining[end..];
    }
    clean.push_str(remaining);
    clean
}

fn token_char(character: char) -> bool {
    character.is_alphanumeric() || matches!(character, '_' | '-')
}

/// Removes URL userinfo and fragments and replaces the whole query with
/// `<redacted>`, including query values shorter than the token-redaction limit.
#[must_use]
pub fn safe_url(url: &str) -> String {
    let without_fragment = url.split('#').next().unwrap_or(url);
    let (base, query) = without_fragment
        .split_once('?')
        .map_or((without_fragment, None), |(base, query)| {
            (base, Some(query))
        });
    let base = without_userinfo(base);
    query.map_or(base.clone(), |_| format!("{base}?<redacted>"))
}

fn without_userinfo(url: &str) -> String {
    let authority = url.find("://").map_or_else(
        || url.starts_with("//").then_some(2),
        |index| Some(index + 3),
    );
    let Some(start) = authority else {
        return url.to_owned();
    };
    let end = url[start..]
        .find('/')
        .map_or(url.len(), |index| start + index);
    if let Some(index) = url[start..end].rfind('@') {
        return format!("{}{}", &url[..start], &url[start + index + 1..]);
    }
    url.to_owned()
}

#[cfg(test)]
mod tests {
    #[test]
    fn redaction_masks_only_whole_tokens() {
        assert_eq!(
            super::redact_token(
                "secret-token prefixsecret-token secret-tokensuffix prefix-secret-token-suffix secret-token",
                "secret-token",
            ),
            "[redacted] prefixsecret-token secret-tokensuffix prefix-secret-token-suffix [redacted]"
        );
    }

    #[test]
    fn redaction_ignores_values_shorter_than_eight_characters() {
        assert_eq!(
            super::redact_token("short shortish", "short"),
            "short shortish"
        );
    }

    #[test]
    fn safe_url_strips_userinfo_and_every_query_value() {
        assert_eq!(
            super::safe_url("https://user:pass@example.test/models?key=tiny#fragment"),
            "https://example.test/models?<redacted>"
        );
    }

    #[test]
    fn source_url_is_safe_for_debug_output() {
        let list = crate::ModelList::new(Vec::new(), None)
            .with_source_url("https://user:pass@example.test/models?key=query-secret#fragment");
        assert_eq!(
            list.source_url(),
            Some("https://example.test/models?<redacted>")
        );
        let debug = format!("{list:?}");
        assert!(!debug.contains("user:pass"));
        assert!(!debug.contains("query-secret"));
        assert!(!debug.contains("fragment"));
    }
}
