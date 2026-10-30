//! Stable account ids derived from what the provider says about the user.

use std::fmt::Write as _;

use hofund_plugin_api::AccountId;
use ring::digest::{SHA256, digest};

/// `"a-"` and 16 lowercase hex digits of the SHA-256 of `parts` joined with NUL.
///
/// Pass stable provider facts, such as the issuer and the subject. The same
/// parts always give the same id. The parts themselves are not recoverable
/// from it, so an email address does not end up in a file name.
///
/// # Panics
///
/// Does not panic: `a-` followed by hex digits is always a valid kebab id.
#[must_use]
pub fn account_id_from(parts: &[&str]) -> AccountId {
    let joined = parts.join("\0");
    let hash = digest(&SHA256, joined.as_bytes());
    let mut id = String::from("a-");
    for byte in &hash.as_ref()[..8] {
        let _ = write!(id, "{byte:02x}");
    }
    AccountId::try_from(id.as_str()).expect("a- and hex digits form a kebab id")
}

#[cfg(test)]
mod tests {
    use super::account_id_from;

    #[test]
    fn account_id_kebab_and_stable() {
        let id = account_id_from(&["https://issuer.example", "user-1"]);
        assert_eq!(id, account_id_from(&["https://issuer.example", "user-1"]));
        let text = id.as_str();
        assert_eq!(text.len(), 18, "{text}");
        assert!(text.starts_with("a-"), "{text}");
        assert!(
            text[2..]
                .chars()
                .all(|c| c.is_ascii_digit() || ('a'..='f').contains(&c)),
            "{text}"
        );
        // Known vectors: the first eight bytes of SHA-256 over the NUL-joined parts.
        assert_eq!(text, "a-04d6c966c1e8cbcd");
        assert_eq!(account_id_from(&["x", "y"]).as_str(), "a-ce3890a816f5237a");
        // The separator matters: ["ab"] and ["a", "b"] differ.
        assert_ne!(account_id_from(&["ab"]), account_id_from(&["a", "b"]));
        assert_ne!(account_id_from(&["ab", ""]), account_id_from(&["a", "b"]));
    }
}
