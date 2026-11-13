//! COSY request signing and the client's body alphabet.

use hofund_plugin_api::PluginError;
use hofund_plugin_sdk::json::json;
use hofund_plugin_sdk::oauth::secret_text;
use hofund_plugin_sdk::{cosy as crypto, digest, uuid};

use crate::creds::Credential;

const VERSION: &str = "1.1.69";
const RSA_KEY: &str = "-----BEGIN PUBLIC KEY-----\nMIGfMA0GCSqGSIb3DQEBAQUAA4GNADCBiQKBgQDA8iMH5c02LilrsERw9t6Pv5Nc\n4k6Pz1EaDicBMpdpxKduSZu5OANqUq8er4GM95omAGIOPOh+Nx0spthYA2BqGz+l\n6HRkPJ7S236FZz73In/KVuLnwI8JJ2CbuJap8kvheCCZpmAWpb/cPx/3Vr/J6I17\nXcW+ML9FoCI6AOvOzwIDAQAB\n-----END PUBLIC KEY-----";
const ALPHABET: &[u8; 64] = b"_doRTgHZBKcGVjlvpC,@aFSx#DPuNJme&i*MzLOEn)sUrthbf%Y^w.(kIQyXqWA!";

pub(crate) struct Signature {
    pub authorization: String,
    pub key: String,
    pub date: String,
}

pub(crate) fn sign(
    url: &str,
    credential: &Credential,
    body: &str,
    now: std::time::SystemTime,
) -> Result<Signature, PluginError> {
    if credential.machine_id.is_empty() {
        return Err(PluginError::auth(
            "Qoder account has no machine id; sign in again",
        ));
    }
    let random =
        uuid::v4().map_err(|_| PluginError::internal("the system random source failed"))?;
    let key_text = random.replace('-', "");
    let key = &key_text.as_bytes()[..16];
    let user = json!({
        "uid": credential.uid,
        "aid": "",
        "name": credential.name,
        "email": credential.email,
        "security_oauth_token": secret_text(&credential.token),
    })
    .to_string();
    let info = crypto::encrypt_user(key, user.as_bytes())?;
    let wrapped = crypto::wrap_key(RSA_KEY, key)?;
    let request_id =
        uuid::v4().map_err(|_| PluginError::internal("the system random source failed"))?;
    let payload = crypto::base64(
        json!({
            "version": "v1",
            "requestId": request_id,
            "info": info,
            "cosyVersion": VERSION,
            "ideVersion": "",
        })
        .to_string()
        .as_bytes(),
    );
    let parsed = hofund_plugin_sdk::url::Url::parse(url)
        .map_err(|_| PluginError::config("Qoder request URL is invalid"))?;
    let path = parsed.path().strip_prefix("/algo").unwrap_or(parsed.path());
    let date = now
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap_or_default()
        .as_secs()
        .to_string();
    let signature =
        crypto::md5_hex(format!("{payload}\n{wrapped}\n{date}\n{body}\n{path}").as_bytes());
    Ok(Signature {
        authorization: format!("Bearer COSY.{payload}.{signature}"),
        key: wrapped,
        date,
    })
}

pub(crate) fn signed_call(
    call: hofund_plugin_sdk::Call,
    signature: Signature,
) -> hofund_plugin_sdk::Call {
    signed_call_with_accept(call, signature, "application/json")
}

pub(crate) fn signed_call_with_accept(
    mut call: hofund_plugin_sdk::Call,
    signature: Signature,
    accept: &str,
) -> hofund_plugin_sdk::Call {
    call = call
        .header("Accept", accept)
        .header("Content-Type", "application/json")
        .sensitive_header("Authorization", signature.authorization)
        .sensitive_header("Cosy-Key", signature.key)
        .sensitive_header("Cosy-Date", signature.date);
    call
}

pub(crate) fn encode(text: &str) -> String {
    swap_thirds(&segment_encode(text.as_bytes()))
}

#[cfg(test)]
pub(crate) fn decode(wire: &str) -> Result<String, PluginError> {
    let wire = swap_thirds(wire);
    let mut out = Vec::new();
    for group in wire.as_bytes().chunks(4) {
        if group.len() != 4 {
            return Err(PluginError::decode("COSY body has a partial group"));
        }
        let values: Vec<u8> = group
            .iter()
            .filter(|byte| **byte != b'$')
            .map(|byte| {
                ALPHABET
                    .iter()
                    .position(|candidate| candidate == byte)
                    .map(|index| index as u8)
                    .ok_or_else(|| PluginError::decode("COSY body has an unknown character"))
            })
            .collect::<Result<_, _>>()?;
        let mut value = 0_u32;
        for digit in &values {
            value = value * 64 + u32::from(*digit);
        }
        let count = values.len() * 6 / 8;
        for index in 0..count {
            let shift = values.len() * 6 - 8 * (index + 1);
            out.push(((value >> shift) & 0xff) as u8);
        }
    }
    String::from_utf8(out).map_err(|_| PluginError::decode("COSY body is not UTF-8"))
}

fn segment_encode(bytes: &[u8]) -> String {
    let mut out = String::new();
    let mut accumulator = 0_u32;
    let mut bits = 0_u8;
    for byte in bytes {
        accumulator = ((accumulator << 8) | u32::from(*byte)) & 0x00ff_ffff;
        bits += 8;
        while bits >= 6 {
            bits -= 6;
            out.push(char::from(ALPHABET[((accumulator >> bits) & 63) as usize]));
        }
    }
    if bits > 0 {
        out.push(char::from(
            ALPHABET[((accumulator << (6 - bits)) & 63) as usize],
        ));
    }
    while !out.len().is_multiple_of(4) {
        out.push('$');
    }
    out
}

fn swap_thirds(text: &str) -> String {
    let third = text.len() / 3;
    if third == 0 {
        return text.to_owned();
    }
    format!(
        "{}{}{}",
        &text[text.len() - third..],
        &text[third..text.len() - third],
        &text[..third]
    )
}

pub(crate) fn digest_hex(text: &str) -> String {
    digest::sha256(text.as_bytes())
        .iter()
        .map(|byte| format!("{byte:02x}"))
        .collect()
}

#[cfg(test)]
mod tests {
    use super::{decode, encode};

    #[test]
    fn r54b_body_codec_round_trips_reference_alphabet_cases() {
        for text in [
            "",
            "a",
            "hello",
            "{\"key\":\"performance\"}",
            "text with spaces",
        ] {
            let encoded = encode(text);
            assert_eq!(decode(&encoded).expect("decode"), text);
        }
        assert_eq!(encode("hello"), encode("hello"));
    }
}
