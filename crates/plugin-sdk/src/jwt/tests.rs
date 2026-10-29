use base64::Engine;
use base64::engine::general_purpose::URL_SAFE_NO_PAD;
use ring::signature::{RSA_PKCS1_2048_8192_SHA256, UnparsedPublicKey};

use super::{KeyError, RsaSigner, pem_to_pkcs8};

const PKCS8: &str = include_str!("test-pkcs8.pem");
const PKCS1: &str = include_str!("test-pkcs1.pem");

fn decode(part: &str) -> serde_json::Value {
    serde_json::from_slice(&URL_SAFE_NO_PAD.decode(part).expect("b64")).expect("json")
}

fn verifies(signer: &RsaSigner, token: &str) -> bool {
    let (unsigned, signature) = token.rsplit_once('.').expect("three parts");
    let signature = URL_SAFE_NO_PAD.decode(signature).expect("b64");
    let public = signer.key.public().as_ref().to_vec();
    UnparsedPublicKey::new(&RSA_PKCS1_2048_8192_SHA256, public)
        .verify(unsigned.as_bytes(), &signature)
        .is_ok()
}

#[test]
fn a_token_carries_the_header_and_claims_and_a_valid_signature() {
    let signer = RsaSigner::from_pem(PKCS8).expect("key");
    let header = serde_json::json!({"alg": "RS256", "typ": "JWT", "kid": "k"});
    let claims = serde_json::json!({"iss": "sa@example.com", "iat": 1, "exp": 3601});
    let token = signer.sign(&header, &claims).expect("sign");
    let parts = token.split('.').collect::<Vec<_>>();
    assert_eq!(parts.len(), 3);
    assert_eq!(decode(parts[0]), header);
    assert_eq!(decode(parts[1]), claims);
    assert!(verifies(&signer, &token));
    let mut forged = token.clone();
    forged.insert(parts[0].len() + 1, 'x');
    assert!(!verifies(&signer, &forged));
}

#[test]
fn a_pkcs1_key_is_the_same_key_as_its_pkcs8_form() {
    let pkcs8 = pem_to_pkcs8(PKCS8).expect("pkcs8");
    let wrapped = pem_to_pkcs8(PKCS1).expect("pkcs1");
    assert_eq!(pkcs8, wrapped);
    let one = RsaSigner::from_pem(PKCS1).expect("pkcs1 key");
    let eight = RsaSigner::from_pem(PKCS8).expect("pkcs8 key");
    assert_eq!(one.key.public().as_ref(), eight.key.public().as_ref());
    let token = one.sign(&serde_json::json!({}), &serde_json::json!({})).expect("sign");
    assert!(verifies(&eight, &token));
}

#[test]
fn unusable_keys_are_named() {
    let cases = [
        ("", KeyError::NotPem),
        ("no pem here", KeyError::NotPem),
        ("-----BEGIN PRIVATE KEY-----\n-----END PRIVATE KEY-----", KeyError::NotPem),
        ("-----BEGIN PRIVATE KEY-----\n!!!\n-----END PRIVATE KEY-----", KeyError::NotPem),
        (
            "-----BEGIN ENCRYPTED PRIVATE KEY-----\nAAAA\n-----END ENCRYPTED PRIVATE KEY-----",
            KeyError::Encrypted,
        ),
        (
            "-----BEGIN EC PRIVATE KEY-----\nAAAA\n-----END EC PRIVATE KEY-----",
            KeyError::NotRsa,
        ),
        (
            "-----BEGIN PRIVATE KEY-----\nAAAA\n-----END PRIVATE KEY-----",
            KeyError::NotRsa,
        ),
    ];
    for (text, want) in cases {
        assert_eq!(RsaSigner::from_pem(text).err(), Some(want), "{text:?}");
    }
    assert_eq!(KeyError::Encrypted.to_string(), "the private key is encrypted");
    assert_eq!(format!("{:?}", RsaSigner::from_pem(PKCS8).expect("key")), "RsaSigner");
}

#[test]
fn der_lengths_use_the_shortest_form() {
    assert_eq!(super::der_tlv(4, &[0; 127])[..2], [4, 127]);
    assert_eq!(super::der_tlv(4, &[0; 128])[..3], [4, 0x81, 128]);
    assert_eq!(super::der_tlv(4, &[0; 255])[..3], [4, 0x81, 255]);
    assert_eq!(super::der_tlv(4, &[0; 256])[..4], [4, 0x82, 1, 0]);
    assert_eq!(super::der_tlv(4, &[0; 0x1_0000])[..5], [4, 0x83, 1, 0, 0]);
    assert_eq!(super::der_tlv(4, &[0; 0x1_0000]).len(), 0x1_0000 + 5);
}
