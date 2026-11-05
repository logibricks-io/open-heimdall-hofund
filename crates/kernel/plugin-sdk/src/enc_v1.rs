//! AES-256-GCM blobs with an `enc:v1:` prefix and base64url segments.

use base64::Engine;
use base64::engine::general_purpose::URL_SAFE_NO_PAD;
use ring::aead::{AES_256_GCM, Aad, LessSafeKey, Nonce, UnboundKey};

/// Decrypts `enc:v1:<iv>.<tag>.<ciphertext>` with a 32-byte AES key.
#[must_use]
pub fn decrypt(key: &[u8], value: &str) -> Option<String> {
    let rest = value.strip_prefix("enc:v1:")?;
    let mut parts = rest.split('.');
    let iv_b64 = parts.next()?;
    let tag_b64 = parts.next()?;
    let ct_b64 = parts.next()?;
    if parts.next().is_some() {
        return None;
    }
    let iv = decode_b64url(iv_b64)?;
    let tag = decode_b64url(tag_b64)?;
    let ct = decode_b64url(ct_b64)?;
    let plain = decrypt_aes256_gcm(key, &iv, &ct, &tag)?;
    String::from_utf8(plain).ok()
}

fn decrypt_aes256_gcm(key: &[u8], nonce: &[u8], ciphertext: &[u8], tag: &[u8]) -> Option<Vec<u8>> {
    let unbound = UnboundKey::new(&AES_256_GCM, key).ok()?;
    let sealing_key = LessSafeKey::new(unbound);
    let nonce = Nonce::try_assume_unique_for_key(nonce).ok()?;
    let mut in_out = ciphertext.to_vec();
    in_out.extend_from_slice(tag);
    let plain = sealing_key
        .open_in_place(nonce, Aad::empty(), &mut in_out)
        .ok()?;
    Some(plain.to_vec())
}

fn decode_b64url(text: &str) -> Option<Vec<u8>> {
    let trimmed = text.trim_end_matches('=');
    URL_SAFE_NO_PAD.decode(trimmed).ok()
}

/// Builds a test vector matching the `enc:v1:` layout (12-byte random nonce).
///
/// # Panics
///
/// Panics when `key` is not a valid AES-256 key length or encryption fails.
#[cfg(any(test, feature = "enc-v1-test"))]
#[must_use]
pub fn encrypt_for_test(key: &[u8], plain: &str) -> String {
    use ring::rand::{SecureRandom, SystemRandom};
    let unbound = UnboundKey::new(&AES_256_GCM, key).expect("key length");
    let sealing_key = LessSafeKey::new(unbound);
    let rng = SystemRandom::new();
    let mut nonce_bytes = [0_u8; 12];
    rng.fill(&mut nonce_bytes).expect("rng");
    let nonce = Nonce::assume_unique_for_key(nonce_bytes);
    let mut in_out = plain.as_bytes().to_vec();
    let tag = sealing_key
        .seal_in_place_separate_tag(nonce, Aad::empty(), &mut in_out)
        .expect("seal");
    format!(
        "enc:v1:{}.{}.{}",
        URL_SAFE_NO_PAD.encode(nonce_bytes),
        URL_SAFE_NO_PAD.encode(tag.as_ref()),
        URL_SAFE_NO_PAD.encode(in_out)
    )
}

#[cfg(test)]
mod tests {
    use super::decrypt;
    use crate::digest::sha256;

    #[test]
    fn round_trip_matches_zcode_layout() {
        let key = sha256(b"test-seed");
        let enc = super::encrypt_for_test(&key, "secret-value");
        assert!(enc.starts_with("enc:v1:"));
        assert_eq!(decrypt(&key, &enc).as_deref(), Some("secret-value"));
    }
}
