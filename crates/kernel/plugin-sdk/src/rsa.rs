//! RSA-2048 native-app sign-in keys and encrypted access tokens.

use base64::Engine as _;
use base64::engine::general_purpose::{URL_SAFE, URL_SAFE_NO_PAD};
use hofund_plugin_api::{PluginError, SecretString};
use rsa::pkcs1::EncodeRsaPublicKey as _;
use rsa::sha2::Sha256;
use rsa::{Oaep, Pkcs1v15Encrypt, PrivateKey as RsaPrivateKey};

/// An RSA private key held in memory for one native-app sign-in.
pub struct PrivateKey {
    inner: RsaPrivateKey,
}

impl PrivateKey {
    /// Generates an RSA-2048 key from the operating system random source.
    ///
    /// # Errors
    ///
    /// Returns [`PluginError`] when key generation fails.
    pub fn generate() -> Result<Self, PluginError> {
        let inner = RsaPrivateKey::new(&mut getrandom::SysRng, 2048)
            .map_err(|_| PluginError::internal("RSA-2048 key generation failed"))?;
        Ok(Self { inner })
    }

    /// Returns the PKCS#1 DER public key as padded URL-safe base64.
    ///
    /// # Errors
    ///
    /// Returns [`PluginError`] when the public key cannot be encoded.
    pub fn public_key_pkcs1_base64url(&self) -> Result<String, PluginError> {
        let der = self
            .inner
            .to_public_key()
            .to_pkcs1_der()
            .map_err(|_| PluginError::internal("RSA public key encoding failed"))?;
        Ok(URL_SAFE.encode(der.as_bytes()))
    }

    /// Decrypts URL-safe base64 ciphertext with RSA-OAEP SHA-256, falling back
    /// to PKCS#1 v1.5. Padding in the base64 text is optional.
    ///
    /// # Errors
    ///
    /// Returns [`PluginError`] when decoding, the RSA operations, or UTF-8 fail.
    pub fn decrypt(&self, encoded: &str) -> Result<SecretString, PluginError> {
        let error = || PluginError::auth("the encrypted access token could not be decrypted");
        let ciphertext = URL_SAFE
            .decode(encoded)
            .or_else(|_| URL_SAFE_NO_PAD.decode(encoded))
            .map_err(|_| error())?;
        let plaintext = self
            .inner
            .decrypt(Oaep::<Sha256>::new(), &ciphertext)
            .or_else(|_| self.inner.decrypt(Pkcs1v15Encrypt, &ciphertext))
            .map_err(|_| error())?;
        let plaintext = String::from_utf8(plaintext).map_err(|_| error())?;
        Ok(SecretString::from(plaintext))
    }
}

/// Encrypts to a padded URL-safe PKCS#1 public key with OAEP-SHA256.
///
/// # Errors
///
/// Returns [`PluginError`] when the key or encryption is invalid.
#[cfg(feature = "rsa-test")]
pub fn encrypt_oaep_for_test(public_key: &str, plaintext: &str) -> Result<String, PluginError> {
    encrypt_for_test(public_key, plaintext, true)
}

/// Encrypts to a padded URL-safe PKCS#1 public key with PKCS#1 v1.5.
///
/// # Errors
///
/// Returns [`PluginError`] when the key or encryption is invalid.
#[cfg(feature = "rsa-test")]
pub fn encrypt_pkcs1_for_test(public_key: &str, plaintext: &str) -> Result<String, PluginError> {
    encrypt_for_test(public_key, plaintext, false)
}

#[cfg(feature = "rsa-test")]
fn encrypt_for_test(public_key: &str, plaintext: &str, oaep: bool) -> Result<String, PluginError> {
    use rsa::pkcs1::DecodeRsaPublicKey as _;

    let der = URL_SAFE
        .decode(public_key)
        .map_err(|_| PluginError::config("test RSA public key is not padded base64url"))?;
    let key = rsa::PublicKey::from_pkcs1_der(&der)
        .map_err(|_| PluginError::config("test RSA public key is not PKCS#1 DER"))?;
    let ciphertext = if oaep {
        key.encrypt(
            &mut getrandom::SysRng,
            Oaep::<Sha256>::new(),
            plaintext.as_bytes(),
        )
    } else {
        key.encrypt(
            &mut getrandom::SysRng,
            Pkcs1v15Encrypt,
            plaintext.as_bytes(),
        )
    }
    .map_err(|_| PluginError::internal("test RSA encryption failed"))?;
    Ok(URL_SAFE.encode(ciphertext))
}

#[cfg(test)]
mod tests {
    use base64::Engine as _;
    use rsa::pkcs1::DecodeRsaPublicKey as _;
    use rsa::sha2::{Sha256, Sha512};
    use rsa::traits::PublicKeyParts as _;
    use rsa::{Oaep, PublicKey as RsaPublicKey};

    use crate::oauth::secret_text;

    use super::{PrivateKey, URL_SAFE};

    #[test]
    fn public_key_is_padded_base64url_pkcs1_and_both_decryptions_work() {
        let key = PrivateKey::generate().expect("key");
        let encoded = key.public_key_pkcs1_base64url().expect("public key");
        assert!(!encoded.contains(['+', '/']));
        let der = URL_SAFE.decode(&encoded).expect("padded base64url");
        let public = RsaPublicKey::from_pkcs1_der(&der).expect("PKCS#1 DER");
        assert_eq!(public.size(), 256);

        let oaep = public
            .encrypt(&mut getrandom::SysRng, Oaep::<Sha256>::new(), b"oaep-token")
            .expect("OAEP");
        let decrypted = key.decrypt(&URL_SAFE.encode(oaep)).expect("decrypt OAEP");
        assert_eq!(secret_text(&decrypted), "oaep-token");

        let pkcs1 = public
            .encrypt(&mut getrandom::SysRng, rsa::Pkcs1v15Encrypt, b"pkcs1-token")
            .expect("PKCS#1");
        let unpadded = URL_SAFE.encode(pkcs1);
        let decrypted = key
            .decrypt(unpadded.trim_end_matches('='))
            .expect("decrypt PKCS#1 without base64 padding");
        assert_eq!(secret_text(&decrypted), "pkcs1-token");
    }

    #[test]
    fn wrong_oaep_hash_never_yields_the_token_and_truncated_ciphertext_fails() {
        let key = PrivateKey::generate().expect("key");
        let public = key.inner.to_public_key();
        let wrong_hash = public
            .encrypt(&mut getrandom::SysRng, Oaep::<Sha512>::new(), b"secret")
            .expect("encrypt");
        if let Ok(token) = key.decrypt(&URL_SAFE.encode(wrong_hash)) {
            assert_ne!(secret_text(&token), "secret");
        }

        let mut truncated = public
            .encrypt(&mut getrandom::SysRng, Oaep::<Sha256>::new(), b"secret")
            .expect("encrypt");
        truncated.pop();
        assert!(key.decrypt(&URL_SAFE.encode(truncated)).is_err());
        assert!(key.decrypt("not base64!").is_err());
    }
}
