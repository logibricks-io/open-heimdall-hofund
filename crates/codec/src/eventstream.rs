//! AWS binary event-stream frames.
//!
//! ConverseStream uses this framing. The prelude, header block, payload, and
//! both CRC-32 checksums are the layout documented for that encoding
//! (<https://docs.aws.amazon.com/AmazonS3/latest/API/RESTSelectObjectAppendix.html>,
//! checked 2026-10-25).

use crate::error::CodecError;

/// One decoded message. Exceptions are returned as errors and are not frames.
#[derive(Debug)]
pub(crate) struct Frame {
    /// `:event-type`, such as `contentBlockDelta`.
    pub event_type: String,
    /// JSON payload. Empty when the message has none.
    pub payload: Vec<u8>,
}

/// Incremental decoder. A chunk may end inside a message.
pub(crate) struct Decoder {
    buffer: Vec<u8>,
}

impl Decoder {
    pub(crate) fn new() -> Self {
        Self { buffer: Vec::new() }
    }

    /// Appends `chunk` and returns every message that is now complete.
    ///
    /// # Errors
    ///
    /// Returns [`CodecError`] when a complete message has a bad checksum, a
    /// header this decoder cannot skip, or an exception payload.
    pub(crate) fn push(&mut self, chunk: &[u8]) -> Result<Vec<Frame>, CodecError> {
        self.buffer.extend_from_slice(chunk);
        let mut frames = Vec::new();
        loop {
            if self.buffer.len() < 12 {
                break;
            }
            let total = u32::from_be_bytes(self.buffer[0..4].try_into().expect("4 bytes"));
            let total = usize::try_from(total).unwrap_or(usize::MAX);
            if !(16..=8 * 1024 * 1024).contains(&total) {
                return Err(stream_error("event-stream message length is not usable"));
            }
            if self.buffer.len() < total {
                break;
            }
            frames.push(decode_message(&self.buffer[..total])?);
            self.buffer.drain(..total);
        }
        Ok(frames)
    }

    /// Rejects a trailing partial message.
    ///
    /// # Errors
    ///
    /// Returns [`CodecError`] when bytes remain that are not a whole message.
    pub(crate) fn finish(&mut self) -> Result<(), CodecError> {
        if self.buffer.is_empty() {
            Ok(())
        } else {
            Err(stream_error("event stream ended inside a message"))
        }
    }
}

/// Encodes one event message. Tests and the stream encoder share this.
pub(crate) fn encode_event(event_type: &str, payload: &[u8]) -> Vec<u8> {
    encode_message(event_type, "event", payload)
}

fn encode_message(event_type: &str, message_type: &str, payload: &[u8]) -> Vec<u8> {
    let headers = encode_headers(&[
        (":content-type", "application/json"),
        (":event-type", event_type),
        (":message-type", message_type),
    ]);
    let headers_len = u32::try_from(headers.len()).unwrap_or(u32::MAX);
    let total = 12 + headers_len + u32::try_from(payload.len()).unwrap_or(u32::MAX) + 4;
    let mut message = Vec::with_capacity(usize::try_from(total).unwrap_or(payload.len() + 64));
    message.extend_from_slice(&total.to_be_bytes());
    message.extend_from_slice(&headers_len.to_be_bytes());
    let prelude = crc32(&message);
    message.extend_from_slice(&prelude.to_be_bytes());
    message.extend_from_slice(&headers);
    message.extend_from_slice(payload);
    let checksum = crc32(&message);
    message.extend_from_slice(&checksum.to_be_bytes());
    message
}

fn encode_headers(headers: &[(&str, &str)]) -> Vec<u8> {
    let mut out = Vec::new();
    for (name, value) in headers {
        let name = name.as_bytes();
        let value = value.as_bytes();
        out.push(u8::try_from(name.len()).unwrap_or(u8::MAX));
        out.extend_from_slice(name);
        out.push(7);
        let len = u16::try_from(value.len()).unwrap_or(u16::MAX);
        out.extend_from_slice(&len.to_be_bytes());
        out.extend_from_slice(value);
    }
    out
}

fn decode_message(message: &[u8]) -> Result<Frame, CodecError> {
    let headers_len = u32::from_be_bytes(message[4..8].try_into().expect("4 bytes"));
    let prelude_crc = u32::from_be_bytes(message[8..12].try_into().expect("4 bytes"));
    if crc32(&message[..8]) != prelude_crc {
        return Err(stream_error("event-stream prelude checksum does not match"));
    }
    let message_crc = u32::from_be_bytes(message[message.len() - 4..].try_into().expect("4 bytes"));
    if crc32(&message[..message.len() - 4]) != message_crc {
        return Err(stream_error("event-stream message checksum does not match"));
    }
    let headers_len = usize::try_from(headers_len).unwrap_or(usize::MAX);
    let headers_end = 12 + headers_len;
    if headers_end + 4 > message.len() {
        return Err(stream_error("event-stream headers overrun the message"));
    }
    let headers = parse_headers(&message[12..headers_end])?;
    let event_type = headers
        .iter()
        .find(|(name, _)| name == ":event-type")
        .map(|(_, value)| value.clone())
        .unwrap_or_default();
    let message_type = headers
        .iter()
        .find(|(name, _)| name == ":message-type")
        .map(|(_, value)| value.as_str())
        .unwrap_or("event");
    let payload = message[headers_end..message.len() - 4].to_vec();
    if message_type == "exception" || message_type == "error" {
        let text = String::from_utf8_lossy(&payload);
        return Err(stream_error(format!("event-stream exception: {text}")));
    }
    if event_type.is_empty() {
        return Err(stream_error("event-stream message has no :event-type"));
    }
    Ok(Frame {
        event_type,
        payload,
    })
}

fn parse_headers(bytes: &[u8]) -> Result<Vec<(String, String)>, CodecError> {
    let mut headers = Vec::new();
    let mut index = 0;
    while index < bytes.len() {
        let name_len = usize::from(bytes[index]);
        index += 1;
        if index + name_len + 1 > bytes.len() {
            return Err(stream_error("event-stream header name is truncated"));
        }
        let name = std::str::from_utf8(&bytes[index..index + name_len])
            .map_err(|_| stream_error("event-stream header name is not UTF-8"))?
            .to_owned();
        index += name_len;
        let kind = bytes[index];
        index += 1;
        let value = match kind {
            0 | 1 => String::new(),
            2 => skip(bytes, &mut index, 1)?,
            3 => skip(bytes, &mut index, 2)?,
            4 => skip(bytes, &mut index, 4)?,
            5 | 8 => skip(bytes, &mut index, 8)?,
            9 => skip(bytes, &mut index, 16)?,
            6 | 7 => {
                if index + 2 > bytes.len() {
                    return Err(stream_error("event-stream header value is truncated"));
                }
                let len = usize::from(u16::from_be_bytes(
                    bytes[index..index + 2].try_into().expect("2 bytes"),
                ));
                index += 2;
                if index + len > bytes.len() {
                    return Err(stream_error("event-stream header value is truncated"));
                }
                let raw = &bytes[index..index + len];
                index += len;
                if kind == 7 {
                    std::str::from_utf8(raw)
                        .map_err(|_| stream_error("event-stream header value is not UTF-8"))?
                        .to_owned()
                } else {
                    String::new()
                }
            }
            _ => {
                return Err(stream_error(format!(
                    "event-stream header type {kind} is not known"
                )));
            }
        };
        headers.push((name, value));
    }
    Ok(headers)
}

fn skip(bytes: &[u8], index: &mut usize, len: usize) -> Result<String, CodecError> {
    if *index + len > bytes.len() {
        return Err(stream_error("event-stream header value is truncated"));
    }
    *index += len;
    Ok(String::new())
}

fn stream_error(message: impl Into<String>) -> CodecError {
    CodecError::Sse {
        message: message.into(),
    }
}

/// ISO-HDLC CRC-32. `crc32(b"123456789")` is `0xCBF43926`.
fn crc32(data: &[u8]) -> u32 {
    let mut crc = 0xffff_ffffu32;
    for &byte in data {
        crc ^= u32::from(byte);
        for _ in 0..8 {
            let mask = (crc & 1).wrapping_neg();
            crc = (crc >> 1) ^ (0xEDB8_8320 & mask);
        }
    }
    !crc
}

#[cfg(test)]
mod tests {
    use super::{Decoder, crc32, encode_event};

    #[test]
    fn the_crc_check_value_matches() {
        assert_eq!(crc32(b"123456789"), 0xCBF4_3926);
    }

    #[test]
    fn a_frame_round_trips_and_a_bad_checksum_is_rejected() {
        let bytes = encode_event("contentBlockDelta", br#"{"delta":{"text":"Hi"}}"#);
        let mut decoder = Decoder::new();
        let frames = decoder.push(&bytes[..10]).expect("partial");
        assert!(frames.is_empty());
        let frames = decoder.push(&bytes[10..]).expect("rest");
        assert_eq!(frames.len(), 1);
        assert_eq!(frames[0].event_type, "contentBlockDelta");
        assert_eq!(frames[0].payload, br#"{"delta":{"text":"Hi"}}"#);
        decoder.finish().expect("done");

        let mut broken = bytes.clone();
        let last = broken.len() - 1;
        broken[last] ^= 0xff;
        let error = Decoder::new().push(&broken).expect_err("checksum");
        assert!(error.to_string().contains("checksum"), "{error}");
    }
}
