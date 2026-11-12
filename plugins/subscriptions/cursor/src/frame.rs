//! Connect framing for Cursor agent streams.
//!
//! Each frame is a 5-byte head plus a payload. Byte 0 is flags. Bytes 1-4 are a
//! big-endian payload length. Flag bit 0 means the payload is gzip. Flag bit 1
//! means this is the stream trailer, and the payload is JSON, not protobuf.

use hofund_plugin_api::PluginError;
use hofund_plugin_sdk::inflate::gzip;

/// Maximum Connect payload length, and the inflate output cap.
pub(crate) const MAX_FRAME_PAYLOAD: usize = 64 * 1024 * 1024;

/// Flag bit 0: the payload is gzip-compressed.
pub(crate) const FLAG_GZIP: u8 = 1;

/// Flag bit 1: this frame is the stream trailer (JSON, not protobuf).
pub(crate) const FLAG_TRAILER: u8 = 2;

/// One decoded Connect frame. The payload is already inflated when the gzip bit
/// was set.
#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) struct Frame {
    /// Original flag byte from the head.
    pub flags: u8,
    /// Frame payload. Gzip bodies are inflated and capped at [`MAX_FRAME_PAYLOAD`].
    pub payload: Vec<u8>,
}

impl Frame {
    /// Whether flag bit 1 is set (JSON trailer).
    #[must_use]
    pub(crate) fn is_trailer(&self) -> bool {
        self.flags & FLAG_TRAILER != 0
    }

    /// Whether flag bit 0 was set on the wire (payload is already inflated).
    #[must_use]
    pub(crate) fn is_gzip(&self) -> bool {
        self.flags & FLAG_GZIP != 0
    }
}

/// Encodes one Connect frame. Does not compress; callers that want the gzip bit
/// must pass already-compressed bytes and set [`FLAG_GZIP`].
///
/// # Errors
///
/// Returns a decode-class error when `payload` is longer than [`MAX_FRAME_PAYLOAD`].
pub(crate) fn encode(flags: u8, payload: &[u8]) -> Result<Vec<u8>, PluginError> {
    if payload.len() > MAX_FRAME_PAYLOAD {
        return Err(payload_too_large(payload.len()));
    }
    let len = u32::try_from(payload.len()).map_err(|_| payload_too_large(payload.len()))?;
    let mut out = Vec::with_capacity(5 + payload.len());
    out.push(flags);
    out.extend_from_slice(&len.to_be_bytes());
    out.extend_from_slice(payload);
    Ok(out)
}

/// Incremental decoder for a Connect frame stream.
#[derive(Clone, Debug, Default)]
pub(crate) struct FrameDecoder {
    buf: Vec<u8>,
}

impl FrameDecoder {
    /// Empty decoder.
    #[must_use]
    pub(crate) fn new() -> Self {
        Self::default()
    }

    /// Appends bytes and returns every complete frame now available.
    ///
    /// # Errors
    ///
    /// Returns a decode error when a frame length exceeds [`MAX_FRAME_PAYLOAD`],
    /// or when a gzip payload cannot be inflated inside the cap.
    pub(crate) fn push(&mut self, data: &[u8]) -> Result<Vec<Frame>, PluginError> {
        let mut frames = Vec::new();
        let mut input = data;
        while !input.is_empty() {
            if self.buf.len() < 5 {
                let take = (5 - self.buf.len()).min(input.len());
                self.buf.extend_from_slice(&input[..take]);
                input = &input[take..];
                if self.buf.len() < 5 {
                    break;
                }
            }
            let len =
                u32::from_be_bytes([self.buf[1], self.buf[2], self.buf[3], self.buf[4]]) as usize;
            if len > MAX_FRAME_PAYLOAD {
                return Err(payload_too_large(len));
            }
            let total = 5 + len;
            if self.buf.len() < total {
                let take = (total - self.buf.len()).min(input.len());
                self.buf.extend_from_slice(&input[..take]);
                input = &input[take..];
                if self.buf.len() < total {
                    break;
                }
            }
            frames.extend(self.take_complete()?);
        }
        Ok(frames)
    }

    /// Ends the stream. Any leftover byte is `unexpected EOF`.
    ///
    /// # Errors
    ///
    /// Returns a decode error on a truncated frame, a leftover byte, a too-large
    /// length, or a gzip inflate failure.
    pub(crate) fn finish(mut self) -> Result<Vec<Frame>, PluginError> {
        let frames = self.take_complete()?;
        if !self.buf.is_empty() {
            return Err(PluginError::decode("unexpected EOF"));
        }
        Ok(frames)
    }

    fn take_complete(&mut self) -> Result<Vec<Frame>, PluginError> {
        let mut frames = Vec::new();
        loop {
            if self.buf.len() < 5 {
                break;
            }
            let flags = self.buf[0];
            let len =
                u32::from_be_bytes([self.buf[1], self.buf[2], self.buf[3], self.buf[4]]) as usize;
            if len > MAX_FRAME_PAYLOAD {
                return Err(payload_too_large(len));
            }
            let total = 5 + len;
            if self.buf.len() < total {
                break;
            }
            let payload = self.buf[5..total].to_vec();
            self.buf.drain(..total);
            let payload = if flags & FLAG_GZIP != 0 {
                gzip(&payload)?
            } else {
                payload
            };
            frames.push(Frame { flags, payload });
        }
        Ok(frames)
    }
}

/// Decodes a complete buffer as a frame stream. A leftover or truncated tail is
/// `unexpected EOF`.
///
/// # Errors
///
/// Same as [`FrameDecoder::finish`].
pub(crate) fn decode_all(data: &[u8]) -> Result<Vec<Frame>, PluginError> {
    let mut decoder = FrameDecoder::new();
    let mut frames = decoder.push(data)?;
    frames.extend(decoder.finish()?);
    Ok(frames)
}

fn payload_too_large(len: usize) -> PluginError {
    PluginError::decode(format!(
        "frame payload exceeds {MAX_FRAME_PAYLOAD} bytes (got {len})"
    ))
}

#[cfg(test)]
mod tests {
    use super::{FLAG_GZIP, FLAG_TRAILER, FrameDecoder, MAX_FRAME_PAYLOAD, decode_all, encode};
    use crate::proto::fields;
    use hofund_plugin_sdk::inflate::gzip;

    /// gzip of protobuf field 1 varint 1 (`08 01`), mtime 0.
    const GZIP_FIELD1_VARINT1: &[u8] = &[
        0x1f, 0x8b, 0x08, 0x00, 0x00, 0x00, 0x00, 0x00, 0x02, 0xff, 0xe3, 0x60, 0x04, 0x00, 0x61,
        0xa8, 0x07, 0xfe, 0x02, 0x00, 0x00, 0x00,
    ];

    #[test]
    fn encode_writes_flag_and_big_endian_length() {
        let frame = encode(0, b"abc").expect("encode");
        assert_eq!(frame, [0, 0, 0, 0, 3, b'a', b'b', b'c']);
    }

    #[test]
    fn decode_all_reads_two_frames() {
        let mut body = encode(0, b"one").expect("first");
        body.extend(encode(FLAG_TRAILER, br#"{"error":{"code":"unavailable"}}"#).expect("trailer"));
        let frames = decode_all(&body).expect("decode");
        assert_eq!(frames.len(), 2);
        assert_eq!(frames[0].payload, b"one");
        assert!(!frames[0].is_trailer());
        assert!(frames[1].is_trailer());
        assert_eq!(frames[1].payload, br#"{"error":{"code":"unavailable"}}"#);
    }

    #[test]
    fn one_push_returns_two_complete_frames() {
        let mut body = encode(0, b"one").expect("first");
        body.extend(encode(0, b"two").expect("second"));
        let mut decoder = FrameDecoder::new();
        let frames = decoder.push(&body).expect("decode");
        assert_eq!(frames.len(), 2);
        assert_eq!(frames[0].payload, b"one");
        assert_eq!(frames[1].payload, b"two");
        assert!(decoder.finish().expect("end").is_empty());
    }

    #[test]
    fn leftover_byte_after_the_body_ends_is_unexpected_eof() {
        let mut body = encode(0, b"ok").expect("frame");
        body.push(0x00);
        let err = decode_all(&body).expect_err("leftover");
        assert!(
            err.message().contains("unexpected EOF"),
            "message was {}",
            err.message()
        );
    }

    #[test]
    fn truncated_frame_is_unexpected_eof() {
        let full = encode(0, b"hello").expect("frame");
        let err = decode_all(&full[..6]).expect_err("truncated");
        assert!(
            err.message().contains("unexpected EOF"),
            "message was {}",
            err.message()
        );
    }

    #[test]
    fn payload_one_byte_over_64_mib_is_rejected_without_buffering_the_body() {
        let len = u32::try_from(MAX_FRAME_PAYLOAD + 1).expect("fits u32");
        let mut input = vec![0_u8; 5 + 128];
        input[1..5].copy_from_slice(&len.to_be_bytes());
        let mut decoder = FrameDecoder::new();
        let err = decoder.push(&input).expect_err("too large");
        assert!(
            err.message().contains("64") && err.message().contains("exceeds"),
            "message was {}",
            err.message()
        );
        assert_eq!(decoder.buf.len(), 5);
    }

    #[test]
    fn gzip_flag_inflates_the_payload() {
        assert_eq!(gzip(GZIP_FIELD1_VARINT1).expect("fixture"), [0x08, 0x01]);
        let body = encode(FLAG_GZIP, GZIP_FIELD1_VARINT1).expect("encode");
        let frames = decode_all(&body).expect("decode");
        assert_eq!(frames.len(), 1);
        assert!(frames[0].is_gzip());
        assert_eq!(frames[0].payload, [0x08, 0x01]);
        let parsed = fields(&frames[0].payload);
        assert_eq!(parsed.len(), 1);
        assert_eq!(parsed[0].number, 1);
        assert_eq!(parsed[0].as_varint(), Some(1));
    }

    #[test]
    fn gzip_bit_with_a_truncated_body_is_a_decode_error() {
        let body = encode(FLAG_GZIP, b"not-gzip").expect("encode");
        let err = decode_all(&body).expect_err("bad gzip");
        assert!(
            !err.message().contains("unexpected EOF"),
            "inflate failure must not be reported as a truncated stream: {}",
            err.message()
        );
    }

    #[test]
    fn incremental_push_holds_a_partial_head() {
        let body = encode(0, b"xy").expect("frame");
        let mut decoder = FrameDecoder::new();
        assert!(decoder.push(&body[..3]).expect("partial head").is_empty());
        let frames = decoder.push(&body[3..]).expect("rest");
        assert_eq!(frames.len(), 1);
        assert_eq!(frames[0].payload, b"xy");
        assert!(decoder.finish().expect("end").is_empty());
    }

    #[test]
    fn empty_stream_is_ok() {
        assert!(decode_all(&[]).expect("empty").is_empty());
    }
}
