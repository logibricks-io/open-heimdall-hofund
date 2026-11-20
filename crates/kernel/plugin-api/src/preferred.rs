//! The API a gateway asks a model on best, and a catalog row learned with it.
//!
//! Short names in this crate are `chat`, `responses`, and `anthropic`. The
//! wire field and the request header that a remote list requires keep the
//! reference list's spelling, and only as these constants.

/// JSON field on a `/v1/models` row naming the API this gateway asks that
/// model on best.
///
/// The spelling is the one a remote that speaks the reference list already
/// writes. It stays a string so both sides read the same field.
pub const PREFERRED_API_FIELD: &str = "magpie_prefers";

/// Request header a remote that speaks the reference list requires before it
/// writes [`PREFERRED_API_FIELD`].
///
/// The spelling is that list's. Send [`PREFERRED_API_ASK_VALUE`].
pub const PREFERRED_API_ASK: &str = "X-Magpie-Drawers";

/// Value sent with [`PREFERRED_API_ASK`]. The remote checks that the header is present.
pub const PREFERRED_API_ASK_VALUE: &str = "1";

/// JSON object on a `/v1/models` row: a coding-agent catalog entry a member learned.
pub const CATALOG_ENTRY_FIELD: &str = "catalog_entry";

/// `chat`, `responses`, or `anthropic` for a path, a short name, or a codec id.
///
/// Anything else is unknown.
#[must_use]
pub fn preferred_api_name(raw: &str) -> Option<&'static str> {
    let value = raw.trim().trim_matches('/').to_ascii_lowercase();
    if value.is_empty() {
        return None;
    }
    if value == "chat" || value == "openai-chat" || value.ends_with("chat/completions") {
        return Some("chat");
    }
    if value == "responses" || value == "openai-responses" || value.ends_with("responses") {
        return Some("responses");
    }
    if value == "anthropic" || value == "anthropic-messages" || value.ends_with("messages") {
        return Some("anthropic");
    }
    None
}

/// The `/v1/models` path for a preferred API name, or for a value [`preferred_api_name`] accepts.
#[must_use]
pub fn preferred_api_path(name: &str) -> Option<&'static str> {
    match preferred_api_name(name)? {
        "chat" => Some("/v1/chat/completions"),
        "responses" => Some("/v1/responses"),
        "anthropic" => Some("/v1/messages"),
        _ => None,
    }
}

#[cfg(test)]
mod tests {
    use super::{preferred_api_name, preferred_api_path};

    #[test]
    fn paths_and_codec_ids_share_one_short_name() {
        assert_eq!(preferred_api_name("/v1/responses"), Some("responses"));
        assert_eq!(preferred_api_name("openai-chat"), Some("chat"));
        assert_eq!(preferred_api_name("/v1/messages"), Some("anthropic"));
        assert_eq!(preferred_api_name("nope"), None);
        assert_eq!(preferred_api_path("responses"), Some("/v1/responses"));
        assert_eq!(
            preferred_api_path("/v1/chat/completions"),
            Some("/v1/chat/completions")
        );
    }
}
