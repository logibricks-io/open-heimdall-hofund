//! Keychain `security` output parsing and exit-code mapping (unit-tested on all platforms).
#![cfg_attr(not(target_os = "macos"), allow(dead_code))]

use hofund_plugin_sdk::json::Value;

/// Maximum bytes read from a keychain helper process (matches auth file cap).
pub(crate) const KEYCHAIN_OUTPUT_CAP: usize = 1024 * 1024;

/// Outcome of a finished keychain helper process.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum SecurityExit {
    Ok,
    Missing,
    Refused,
    Failed,
}

/// Maps a `security` exit code after stdout is fully read.
pub(crate) fn classify_security_exit(code: i32) -> SecurityExit {
    match code {
        0 => SecurityExit::Ok,
        44 => SecurityExit::Missing,
        51 | 128 => SecurityExit::Refused,
        _ => SecurityExit::Failed,
    }
}

/// Decodes `-w` output: plain JSON or hex-encoded JSON when a newline was escaped.
pub(crate) fn decode_keychain_password(out: &[u8]) -> Vec<u8> {
    let trimmed = trim_ascii_whitespace(out);
    if trimmed.is_empty() || trimmed[0] == b'{' {
        return trimmed.to_vec();
    }
    if !trimmed.len().is_multiple_of(2) {
        return trimmed.to_vec();
    }
    let Ok(decoded) = decode_hex(trimmed) else {
        return trimmed.to_vec();
    };
    if json_object(&decoded) {
        decoded
    } else {
        trimmed.to_vec()
    }
}

/// Appends stream chunks until EOF or [`KEYCHAIN_OUTPUT_CAP`]; over-cap is an error.
pub(crate) fn append_capped(out: &mut Vec<u8>, chunk: &[u8]) -> Result<(), ()> {
    let next = out.len().saturating_add(chunk.len());
    if next > KEYCHAIN_OUTPUT_CAP {
        return Err(());
    }
    out.extend_from_slice(chunk);
    Ok(())
}

fn json_object(bytes: &[u8]) -> bool {
    hofund_plugin_sdk::json::from_slice::<Value>(bytes)
        .ok()
        .is_some_and(|value| value.is_object())
}

fn trim_ascii_whitespace(bytes: &[u8]) -> &[u8] {
    let start = bytes
        .iter()
        .position(|byte| !byte.is_ascii_whitespace())
        .unwrap_or(bytes.len());
    let end = bytes
        .iter()
        .rposition(|byte| !byte.is_ascii_whitespace())
        .map(|index| index + 1)
        .unwrap_or(start);
    &bytes[start..end]
}

fn decode_hex(bytes: &[u8]) -> Result<Vec<u8>, ()> {
    if !bytes.len().is_multiple_of(2) {
        return Err(());
    }
    let mut out = Vec::with_capacity(bytes.len() / 2);
    let mut index = 0;
    while index < bytes.len() {
        let high = hex_nibble(bytes[index])?;
        let low = hex_nibble(bytes[index + 1])?;
        out.push((high << 4) | low);
        index += 2;
    }
    Ok(out)
}

fn hex_nibble(byte: u8) -> Result<u8, ()> {
    match byte {
        b'0'..=b'9' => Ok(byte - b'0'),
        b'a'..=b'f' => Ok(byte - b'a' + 10),
        b'A'..=b'F' => Ok(byte - b'A' + 10),
        _ => Err(()),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn r47b_r1_security_exit_codes_map_like_codex_store() {
        assert_eq!(classify_security_exit(44), SecurityExit::Missing);
        assert_eq!(classify_security_exit(51), SecurityExit::Refused);
        assert_eq!(classify_security_exit(128), SecurityExit::Refused);
        assert_eq!(classify_security_exit(1), SecurityExit::Failed);
    }

    #[test]
    fn r47b_r1_hex_wrapped_json_is_decoded() {
        let json = br#"{"auth_mode":"chatgpt","tokens":{"access_token":"a"}}"#;
        let mut hex = String::new();
        for byte in json {
            hex.push_str(&format!("{byte:02x}"));
        }
        let decoded = decode_keychain_password(hex.as_bytes());
        assert_eq!(decoded, json);
    }

    #[test]
    fn r47b_r1_plain_json_is_unchanged() {
        let json = br#"{"tokens":{"access_token":"x"}}"#;
        assert_eq!(decode_keychain_password(json), json);
    }

    #[test]
    fn r47b_r1_read_cap_allows_large_credential() {
        let mut out = Vec::new();
        let chunk = vec![b'a'; 5000];
        for _ in 0..209 {
            append_capped(&mut out, &chunk).expect("under cap");
        }
        assert!(out.len() > 4096);
        assert!(append_capped(&mut out, &chunk).is_err());
    }
}
