//! Decode Kiro AWS event-stream replies.

use hofund_plugin_api::PluginError;
use hofund_plugin_sdk::json::{self, Value};

pub(crate) fn assistant_text(body: &[u8]) -> Result<String, PluginError> {
    let mut offset = 0;
    while offset + 12 <= body.len() {
        let total =
            u32::from_be_bytes(body[offset..offset + 4].try_into().expect("prelude")) as usize;
        if total < 16 || offset + total > body.len() {
            break;
        }
        let header_len =
            u32::from_be_bytes(body[offset + 4..offset + 8].try_into().expect("hlen")) as usize;
        let payload_start = offset + 12 + header_len;
        let payload_end = offset + total - 4;
        if payload_start > payload_end || payload_end > body.len() {
            break;
        }
        let headers = &body[offset + 12..offset + 12 + header_len];
        let payload = &body[payload_start..payload_end];
        if let Some(text) = frame_text(headers, payload)? {
            return Ok(text);
        }
        offset += total;
    }
    Err(PluginError::decode(
        "Kiro's event stream did not contain assistant text",
    ))
}

fn frame_text(headers: &[u8], payload: &[u8]) -> Result<Option<String>, PluginError> {
    let mut event_type = String::new();
    let mut cursor = headers;
    while cursor.len() > 2 {
        let name_len = cursor[0] as usize;
        if cursor.len() < 2 + name_len {
            break;
        }
        let name = std::str::from_utf8(&cursor[1..1 + name_len]).unwrap_or("");
        let value_type = cursor[1 + name_len];
        cursor = &cursor[2 + name_len..];
        if value_type == 7 && cursor.len() >= 2 {
            let value_len = u16::from_be_bytes(cursor[0..2].try_into().expect("vlen")) as usize;
            if cursor.len() >= 2 + value_len {
                if name == ":event-type" {
                    event_type = String::from_utf8_lossy(&cursor[2..2 + value_len]).into_owned();
                }
                cursor = &cursor[2 + value_len..];
                continue;
            }
        }
        break;
    }
    if event_type != "assistantResponseEvent" {
        return Ok(None);
    }
    let value: Value = json::from_slice(payload)
        .map_err(|_| PluginError::decode("Kiro event payload is not JSON"))?;
    Ok(value
        .get("content")
        .and_then(Value::as_str)
        .filter(|text| !text.is_empty())
        .map(str::to_owned))
}
