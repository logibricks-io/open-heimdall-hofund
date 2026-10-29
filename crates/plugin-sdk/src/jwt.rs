//! RS256 JSON Web Tokens signed with a PEM RSA key.
//!
//! A Google service-account key is a PKCS#8 `PRIVATE KEY`; an older key file
//! can hold a PKCS#1 `RSA PRIVATE KEY`, which is wrapped into PKCS#8 here. An
//! encrypted key is refused. The token is `header.claims.signature`, each part
//! base64url without padding.
//!
//! Reference: <https://www.rfc-editor.org/rfc/rfc7515#appendix-A.2> and
//! <https://developers.google.com/identity/protocols/oauth2/service-account>.

use std::fmt;

use base64::Engine;
use base64::engine::general_purpose::{STANDARD, URL_SAFE_NO_PAD};
use ring::rand::SystemRandom;
use ring::signature::{RSA_PKCS1_SHA256, RsaKeyPair};
use serde_json::Value;

/// Why a key could not be read or could not sign.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
#[non_exhaustive]
pub enum KeyError {
    /// The text holds no PEM block, or its body is not base64.
    NotPem,
    /// The PEM block is an encrypted private key.
    Encrypted,
    /// The PEM block is not an RSA private key.
    NotRsa,
    /// Signing failed.
    Sign,
}

impl fmt::Display for KeyError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str(match self {
            Self::NotPem => "the private key is no PEM key",
            Self::Encrypted => "the private key is encrypted",
            Self::NotRsa => "the private key is no RSA key",
            Self::Sign => "the private key could not sign",
        })
    }
}

impl std::error::Error for KeyError {}

/// An RSA key that signs RS256 tokens. [`Debug`] does not show the key.
pub struct RsaSigner {
    key: RsaKeyPair,
}

impl fmt::Debug for RsaSigner {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str("RsaSigner")
    }
}

impl RsaSigner {
    /// Reads the first PEM block of `text`: `PRIVATE KEY` (PKCS#8) or
    /// `RSA PRIVATE KEY` (PKCS#1).
    ///
    /// # Errors
    ///
    /// Returns [`KeyError`] when there is no PEM block, it is encrypted, or
    /// it is not an RSA key.
    pub fn from_pem(text: &str) -> Result<Self, KeyError> {
        let pkcs8 = pem_to_pkcs8(text)?;
        let key = RsaKeyPair::from_pkcs8(&pkcs8).map_err(|_| KeyError::NotRsa)?;
        Ok(Self { key })
    }

    /// `header.claims.signature` for these JSON values, signed with RS256.
    ///
    /// # Errors
    ///
    /// Returns [`KeyError::Sign`] when signing fails.
    pub fn sign(&self, header: &Value, claims: &Value) -> Result<String, KeyError> {
        let header = URL_SAFE_NO_PAD.encode(header.to_string());
        let claims = URL_SAFE_NO_PAD.encode(claims.to_string());
        let unsigned = format!("{header}.{claims}");
        let mut signature = vec![0u8; self.key.public().modulus_len()];
        self.key
            .sign(
                &RSA_PKCS1_SHA256,
                &SystemRandom::new(),
                unsigned.as_bytes(),
                &mut signature,
            )
            .map_err(|_| KeyError::Sign)?;
        Ok(format!("{unsigned}.{}", URL_SAFE_NO_PAD.encode(signature)))
    }
}

fn pem_to_pkcs8(text: &str) -> Result<Vec<u8>, KeyError> {
    let mut body = String::new();
    let mut label = None;
    for line in text.lines() {
        let line = line.trim();
        if let Some(rest) = line.strip_prefix("-----BEGIN ") {
            let rest = rest.strip_suffix("-----").unwrap_or(rest);
            label = Some(rest.to_owned());
            continue;
        }
        if line.starts_with("-----END ") {
            break;
        }
        if label.is_some() {
            body.push_str(line);
        }
    }
    let Some(label) = label else {
        return Err(KeyError::NotPem);
    };
    if body.is_empty() {
        return Err(KeyError::NotPem);
    }
    let der = STANDARD
        .decode(body.as_bytes())
        .map_err(|_| KeyError::NotPem)?;
    match label.as_str() {
        "PRIVATE KEY" => Ok(der),
        "RSA PRIVATE KEY" => Ok(pkcs1_to_pkcs8(&der)),
        "ENCRYPTED PRIVATE KEY" => Err(KeyError::Encrypted),
        _ => Err(KeyError::NotRsa),
    }
}

/// Wraps a PKCS#1 `RSAPrivateKey` in a PKCS#8 `PrivateKeyInfo` with the
/// `rsaEncryption` algorithm and NULL parameters.
fn pkcs1_to_pkcs8(pkcs1: &[u8]) -> Vec<u8> {
    let algorithm = der_seq(&[
        der_tlv(
            0x06,
            &[0x2a, 0x86, 0x48, 0x86, 0xf7, 0x0d, 0x01, 0x01, 0x01],
        ),
        vec![0x05, 0x00],
    ]);
    der_seq(&[der_tlv(0x02, &[0x00]), algorithm, der_tlv(0x04, pkcs1)])
}

fn der_seq(parts: &[Vec<u8>]) -> Vec<u8> {
    let mut body = Vec::new();
    for part in parts {
        body.extend_from_slice(part);
    }
    der_tlv(0x30, &body)
}

fn der_tlv(tag: u8, body: &[u8]) -> Vec<u8> {
    let mut out = Vec::with_capacity(body.len() + 4);
    out.push(tag);
    let len = body.len();
    if let Ok(short) = u8::try_from(len)
        && short < 0x80
    {
        out.push(short);
    } else {
        // Long form: 0x80 | the count of big-endian length bytes, then them.
        let bytes = len.to_be_bytes();
        let skip = bytes.iter().take_while(|byte| **byte == 0).count();
        let count = u8::try_from(bytes.len() - skip).unwrap_or(u8::MAX);
        out.push(0x80 | count);
        out.extend_from_slice(&bytes[skip..]);
    }
    out.extend_from_slice(body);
    out
}

#[cfg(test)]
mod tests;
