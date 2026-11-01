//! SHA-256 digests and HMAC-SHA-256 tags for plugins that sign vendor calls.
//!
//! Plugins may depend only on the plugin API and this SDK, so signing helpers
//! live here rather than in each plugin.

use ring::{digest, hmac};

/// The `SHA-256` digest of `bytes`.
#[must_use]
pub fn sha256(bytes: &[u8]) -> [u8; 32] {
    let mut out = [0_u8; 32];
    out.copy_from_slice(digest::digest(&digest::SHA256, bytes).as_ref());
    out
}

/// The `HMAC-SHA-256` tag of `data` under `key`.
#[must_use]
pub fn hmac_sha256(key: &[u8], data: &[u8]) -> [u8; 32] {
    let key = hmac::Key::new(hmac::HMAC_SHA256, key);
    let mut out = [0_u8; 32];
    out.copy_from_slice(hmac::sign(&key, data).as_ref());
    out
}

#[cfg(test)]
mod tests {
    use super::{hmac_sha256, sha256};

    fn hex(bytes: &[u8]) -> String {
        bytes.iter().map(|byte| format!("{byte:02x}")).collect()
    }

    #[test]
    fn sha256_matches_the_standard_vector() {
        assert_eq!(
            hex(&sha256(b"abc")),
            "ba7816bf8f01cfea414140de5dae2223b00361a396177a9cb410ff61f20015ad"
        );
    }

    #[test]
    fn hmac_sha256_matches_rfc_4231_case_two() {
        assert_eq!(
            hex(&hmac_sha256(b"Jefe", b"what do ya want for nothing?")),
            "5bdcc146bf60754e6a042426089575c75a003f089d2739839dec58b964ec3843"
        );
    }
}
