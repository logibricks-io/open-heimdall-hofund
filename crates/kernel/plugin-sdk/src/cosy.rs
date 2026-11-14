//! Cryptographic pieces for COSY-signed request envelopes.

use base64::Engine as _;
use base64::engine::general_purpose::STANDARD;
use cbc::cipher::block_padding::Pkcs7;
use cbc::cipher::{BlockEncryptMut as _, KeyIvInit as _};
use hofund_plugin_api::PluginError;
use md5::{Digest as _, Md5};
use rsa::pkcs8::DecodePublicKey as _;
use rsa::{Pkcs1v15Encrypt, PublicKey};

/// Encrypts one user envelope with AES-128-CBC, using the key as its IV, and
/// returns standard padded Base64.
///
/// # Errors
///
/// Returns [`PluginError`] when the key is not 16 bytes or encryption fails.
pub fn encrypt_user(key: &[u8], plaintext: &[u8]) -> Result<String, PluginError> {
    let mut buffer = vec![0_u8; plaintext.len() + 16];
    buffer[..plaintext.len()].copy_from_slice(plaintext);
    let encrypted = cbc::Encryptor::<aes::Aes128>::new_from_slices(key, key)
        .map_err(|_| PluginError::config("COSY needs a 16-byte AES key"))?
        .encrypt_padded_mut::<Pkcs7>(&mut buffer, plaintext.len())
        .map_err(|_| PluginError::internal("COSY AES encryption failed"))?;
    Ok(STANDARD.encode(encrypted))
}

/// Wraps a per-request key with the envelope's PEM public key using RSA
/// PKCS#1 v1.5 and returns standard padded Base64.
///
/// # Errors
///
/// Returns [`PluginError`] when the public key is invalid or encryption fails.
pub fn wrap_key(public_key: &str, key: &[u8]) -> Result<String, PluginError> {
    let key = PublicKey::from_public_key_pem(public_key)
        .map_err(|_| PluginError::config("COSY RSA public key is invalid"))?
        .encrypt(&mut getrandom::SysRng, Pkcs1v15Encrypt, key)
        .map_err(|_| PluginError::internal("COSY RSA encryption failed"))?;
    Ok(STANDARD.encode(key))
}

/// Encodes bytes as standard padded Base64.
#[must_use]
pub fn base64(bytes: &[u8]) -> String {
    STANDARD.encode(bytes)
}

/// Lowercase MD5 hex for the COSY request signature.
#[must_use]
pub fn md5_hex(bytes: &[u8]) -> String {
    use std::fmt::Write as _;

    Md5::digest(bytes)
        .iter()
        .fold(String::new(), |mut out, byte| {
            let _ = write!(out, "{byte:02x}");
            out
        })
}

#[cfg(test)]
mod tests {
    use super::{encrypt_user, md5_hex};

    #[test]
    fn aes_cbc_matches_the_nist_vector_with_key_as_iv() {
        let key = [
            0x2b, 0x7e, 0x15, 0x16, 0x28, 0xae, 0xd2, 0xa6, 0xab, 0xf7, 0x15, 0x88, 0x09, 0xcf,
            0x4f, 0x3c,
        ];
        let plain = [
            0x6b, 0xc1, 0xbe, 0xe2, 0x2e, 0x40, 0x9f, 0x96, 0xe9, 0x3d, 0x7e, 0x11, 0x73, 0x93,
            0x17, 0x2a,
        ];
        let encrypted = encrypt_user(&key, &plain).expect("AES");
        let decoded = STANDARD.decode(encrypted).expect("Base64");
        assert_eq!(
            decoded,
            [
                0x6e, 0x04, 0xb9, 0x69, 0x8a, 0x17, 0x9a, 0xa9, 0xdc, 0x4d, 0x76, 0x07, 0x80, 0x3b,
                0x44, 0xfb, 0x71, 0xa6, 0x3c, 0x5e, 0xe4, 0xe2, 0x57, 0x36, 0xc8, 0xc8, 0xa7, 0x1d,
                0x62, 0x87, 0x40, 0xf0,
            ]
        );
    }

    #[test]
    fn md5_matches_the_standard_vector() {
        assert_eq!(md5_hex(b"abc"), "900150983cd24fb0d6963f7d28e17f72");
    }

    use base64::Engine as _;
    use base64::engine::general_purpose::STANDARD;
}
