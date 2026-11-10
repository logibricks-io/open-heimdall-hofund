//! Vendor-neutral protobuf fields and Connect frame decoding.
//!
//! The protobuf reader accepts wire types 0, 1, 2, and 5. A malformed tail is
//! dropped while already parsed fields are retained. Connect frames have a
//! five-byte header: one flag byte and a big-endian payload length. Flag bit 0
//! inflates the payload; bit 1 marks an end frame whose inflated payload is
//! JSON. Compression of outgoing protobuf is deliberately left to the caller.

use hofund_plugin_api::PluginError;

use crate::inflate::gzip;

/// Maximum compressed or uncompressed Connect payload length.
pub const MAX_FRAME_PAYLOAD: usize = 64 * 1024 * 1024;

/// Connect flag bit 0: the wire payload is gzip-compressed.
pub const FLAG_GZIP: u8 = 1;

/// Connect flag bit 1: the payload is stream-end JSON rather than protobuf.
pub const FLAG_END: u8 = 2;

const WIRE_VARINT: u32 = 0;
const WIRE_FIXED64: u32 = 1;
const WIRE_BYTES: u32 = 2;
const WIRE_FIXED32: u32 = 5;

/// One parsed protobuf field.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Field {
    /// Field number from the tag.
    pub number: u32,
    /// Decoded wire value.
    pub value: WireValue,
}

impl Field {
    /// Returns the integer when this is a varint field.
    #[must_use]
    pub fn as_varint(&self) -> Option<u64> {
        match self.value {
            WireValue::Varint(value) => Some(value),
            _ => None,
        }
    }

    /// Returns the payload when this is a length-delimited field.
    #[must_use]
    pub fn as_bytes(&self) -> Option<&[u8]> {
        match &self.value {
            WireValue::Bytes(bytes) => Some(bytes),
            _ => None,
        }
    }

    /// Decodes a length-delimited field as UTF-8, replacing invalid sequences.
    #[must_use]
    pub fn as_string(&self) -> Option<String> {
        self.as_bytes()
            .map(|bytes| String::from_utf8_lossy(bytes).into_owned())
    }

    /// Parses a length-delimited field as a nested protobuf message.
    #[must_use]
    pub fn as_message(&self) -> Option<Vec<Field>> {
        self.as_bytes().map(fields)
    }
}

/// A decoded protobuf wire value.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum WireValue {
    /// Wire type 0, decoded without narrowing to a JavaScript-sized integer.
    Varint(u64),
    /// Wire type 1 as little-endian bits.
    Fixed64(u64),
    /// Wire type 2. Packed fields remain their raw payload bytes.
    Bytes(Vec<u8>),
    /// Wire type 5 as little-endian bits.
    Fixed32(u32),
}

/// Parses protobuf fields, dropping a malformed or truncated tail.
#[must_use]
pub fn fields(buf: &[u8]) -> Vec<Field> {
    let mut parsed = Vec::new();
    let mut at = 0;
    while at < buf.len() {
        let Some((tag, next)) = read_varint(buf, at) else {
            break;
        };
        at = next;
        let Ok(number) = u32::try_from(tag >> 3) else {
            break;
        };
        let wire = (tag & 7) as u32;
        let value = match wire {
            WIRE_VARINT => {
                let Some((value, next)) = read_varint(buf, at) else {
                    break;
                };
                at = next;
                WireValue::Varint(value)
            }
            WIRE_FIXED64 => {
                let Some(bytes) = buf.get(at..at + 8) else {
                    break;
                };
                at += 8;
                WireValue::Fixed64(u64::from_le_bytes(bytes.try_into().unwrap_or([0; 8])))
            }
            WIRE_BYTES => {
                let Some((len, next)) = read_varint(buf, at) else {
                    break;
                };
                let Ok(len) = usize::try_from(len) else {
                    break;
                };
                at = next;
                let Some(end) = at.checked_add(len) else {
                    break;
                };
                let Some(bytes) = buf.get(at..end) else {
                    break;
                };
                at = end;
                WireValue::Bytes(bytes.to_vec())
            }
            WIRE_FIXED32 => {
                let Some(bytes) = buf.get(at..at + 4) else {
                    break;
                };
                at += 4;
                WireValue::Fixed32(u32::from_le_bytes(bytes.try_into().unwrap_or([0; 4])))
            }
            _ => break,
        };
        parsed.push(Field { number, value });
    }
    parsed
}

/// Consuming protobuf message builder.
#[derive(Clone, Debug, Default)]
pub struct ProtoBuf {
    buf: Vec<u8>,
}

impl ProtoBuf {
    /// Creates an empty message.
    #[must_use]
    pub fn new() -> Self {
        Self::default()
    }

    /// Appends a varint field.
    #[must_use]
    pub fn varint(mut self, field: u32, value: u64) -> Self {
        self.write_tag(field, WIRE_VARINT);
        write_varint(&mut self.buf, value);
        self
    }

    /// Appends a length-delimited field.
    #[must_use]
    pub fn bytes(mut self, field: u32, value: &[u8]) -> Self {
        self.write_tag(field, WIRE_BYTES);
        write_varint(&mut self.buf, value.len() as u64);
        self.buf.extend_from_slice(value);
        self
    }

    /// Appends a UTF-8 string field.
    #[must_use]
    pub fn string(self, field: u32, value: &str) -> Self {
        self.bytes(field, value.as_bytes())
    }

    /// Appends a nested message field.
    #[must_use]
    pub fn message(self, field: u32, inner: Self) -> Self {
        self.bytes(field, &inner.finish())
    }

    /// Appends a fixed64 field from little-endian bits.
    #[must_use]
    pub fn fixed64(mut self, field: u32, bits: u64) -> Self {
        self.write_tag(field, WIRE_FIXED64);
        self.buf.extend_from_slice(&bits.to_le_bytes());
        self
    }

    /// Appends a double field.
    #[must_use]
    pub fn double(self, field: u32, value: f64) -> Self {
        self.fixed64(field, value.to_bits())
    }

    /// Appends a fixed32 field from little-endian bits.
    #[must_use]
    pub fn fixed32(mut self, field: u32, bits: u32) -> Self {
        self.write_tag(field, WIRE_FIXED32);
        self.buf.extend_from_slice(&bits.to_le_bytes());
        self
    }

    /// Returns the encoded message.
    #[must_use]
    pub fn finish(self) -> Vec<u8> {
        self.buf
    }

    fn write_tag(&mut self, field: u32, wire: u32) {
        write_varint(&mut self.buf, (u64::from(field) << 3) | u64::from(wire));
    }
}

/// One decoded Connect frame. A gzip payload has already been inflated.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Frame {
    /// Original flag byte.
    pub flags: u8,
    /// Frame payload after optional inflation.
    pub payload: Vec<u8>,
}

impl Frame {
    /// Whether flag bit 0 was set on the wire.
    #[must_use]
    pub fn is_gzip(&self) -> bool {
        self.flags & FLAG_GZIP != 0
    }

    /// Whether flag bit 1 marks this as an end-JSON frame.
    #[must_use]
    pub fn is_end(&self) -> bool {
        self.flags & FLAG_END != 0
    }
}

/// Wraps an already encoded payload in one Connect frame without compressing it.
///
/// # Errors
///
/// Returns a decode error when `payload` exceeds [`MAX_FRAME_PAYLOAD`].
pub fn frame(flags: u8, payload: &[u8]) -> Result<Vec<u8>, PluginError> {
    if payload.len() > MAX_FRAME_PAYLOAD {
        return Err(payload_too_large(payload.len()));
    }
    let len = u32::try_from(payload.len()).map_err(|_| payload_too_large(payload.len()))?;
    let mut encoded = Vec::with_capacity(5 + payload.len());
    encoded.push(flags);
    encoded.extend_from_slice(&len.to_be_bytes());
    encoded.extend_from_slice(payload);
    Ok(encoded)
}

/// Decodes exactly one Connect frame.
///
/// # Errors
///
/// Returns a decode error for an empty input, extra frames, a truncated tail,
/// an oversized payload, or invalid gzip.
pub fn deframe(input: &[u8]) -> Result<Frame, PluginError> {
    let mut all = frames(input);
    let first = all
        .next()
        .transpose()?
        .ok_or_else(|| PluginError::decode("unexpected EOF"))?;
    match all.next() {
        Some(Err(error)) => Err(error),
        Some(Ok(_)) => Err(PluginError::decode("expected exactly one Connect frame")),
        None => Ok(first),
    }
}

/// Iterates over a complete Connect response buffer.
///
/// Any trailing partial header or payload produces one `unexpected EOF` error
/// and ends iteration. Invalid gzip and oversized lengths also end iteration.
#[must_use]
pub fn frames(input: &[u8]) -> Frames<'_> {
    Frames { input }
}

/// Iterator returned by [`frames`].
#[derive(Clone, Debug)]
pub struct Frames<'a> {
    input: &'a [u8],
}

impl Iterator for Frames<'_> {
    type Item = Result<Frame, PluginError>;

    fn next(&mut self) -> Option<Self::Item> {
        if self.input.is_empty() {
            return None;
        }
        if self.input.len() < 5 {
            self.input = &[];
            return Some(Err(PluginError::decode("unexpected EOF")));
        }
        let flags = self.input[0];
        let len = u32::from_be_bytes([self.input[1], self.input[2], self.input[3], self.input[4]])
            as usize;
        if len > MAX_FRAME_PAYLOAD {
            self.input = &[];
            return Some(Err(payload_too_large(len)));
        }
        let Some(end) = 5_usize.checked_add(len) else {
            self.input = &[];
            return Some(Err(payload_too_large(len)));
        };
        if self.input.len() < end {
            self.input = &[];
            return Some(Err(PluginError::decode("unexpected EOF")));
        }
        let payload = &self.input[5..end];
        self.input = &self.input[end..];
        let payload = if flags & FLAG_GZIP != 0 {
            match gzip(payload) {
                Ok(inflated) => inflated,
                Err(error) => {
                    self.input = &[];
                    return Some(Err(error));
                }
            }
        } else {
            payload.to_vec()
        };
        Some(Ok(Frame { flags, payload }))
    }
}

impl std::iter::FusedIterator for Frames<'_> {}

fn payload_too_large(len: usize) -> PluginError {
    PluginError::decode(format!(
        "frame payload exceeds {MAX_FRAME_PAYLOAD} bytes (got {len})"
    ))
}

fn write_varint(buf: &mut Vec<u8>, mut value: u64) {
    loop {
        let mut byte = (value & 0x7f) as u8;
        value >>= 7;
        if value != 0 {
            byte |= 0x80;
        }
        buf.push(byte);
        if value == 0 {
            break;
        }
    }
}

fn read_varint(buf: &[u8], mut at: usize) -> Option<(u64, usize)> {
    let mut value = 0_u64;
    for shift in 0..10 {
        let byte = *buf.get(at)?;
        at += 1;
        let bits = u64::from(byte & 0x7f);
        if shift == 9 && bits > 1 {
            return None;
        }
        value |= bits << (shift * 7);
        if byte & 0x80 == 0 {
            return Some((value, at));
        }
    }
    None
}

#[cfg(test)]
mod tests {
    use super::{
        FLAG_END, FLAG_GZIP, Field, MAX_FRAME_PAYLOAD, ProtoBuf, WireValue, deframe, fields, frame,
        frames,
    };
    use crate::inflate::compress;

    #[test]
    fn frame_round_trip_writes_big_endian_length() {
        let encoded = frame(0, b"abc").expect("frame");
        assert_eq!(encoded, [0, 0, 0, 0, 3, b'a', b'b', b'c']);
        let decoded = deframe(&encoded).expect("deframe");
        assert_eq!(decoded.payload, b"abc");
        assert!(!decoded.is_gzip());
        assert!(!decoded.is_end());
    }

    #[test]
    fn gzip_flag_inflates_before_returning_the_frame() {
        let compressed = compress(b"hello gzip").expect("compress");
        let decoded = deframe(&frame(FLAG_GZIP, &compressed).expect("frame")).expect("deframe");
        assert!(decoded.is_gzip());
        assert_eq!(decoded.payload, b"hello gzip");
    }

    #[test]
    fn end_json_flag_preserves_json_payload() {
        let decoded = deframe(&frame(FLAG_END, br#"{"code":"unavailable"}"#).expect("frame"))
            .expect("deframe");
        assert!(decoded.is_end());
        assert!(!decoded.is_gzip());
        assert_eq!(decoded.payload, br#"{"code":"unavailable"}"#);
    }

    #[test]
    fn gzip_and_end_bits_work_together() {
        let compressed = compress(br#"{"error":{"code":"resource_exhausted"}}"#).expect("compress");
        let decoded =
            deframe(&frame(FLAG_GZIP | FLAG_END, &compressed).expect("frame")).expect("deframe");
        assert!(decoded.is_gzip());
        assert!(decoded.is_end());
        assert_eq!(
            decoded.payload,
            br#"{"error":{"code":"resource_exhausted"}}"#
        );
    }

    #[test]
    fn frames_walk_multiple_messages_and_stop_at_eof() {
        let mut body = frame(0, b"one").expect("first");
        body.extend(frame(FLAG_END, b"{}").expect("end"));
        let decoded = frames(&body)
            .collect::<Result<Vec<_>, _>>()
            .expect("frames");
        assert_eq!(decoded.len(), 2);
        assert_eq!(decoded[0].payload, b"one");
        assert_eq!(decoded[1].payload, b"{}");
    }

    #[test]
    fn frame_cap_is_inclusive_and_one_byte_over_is_rejected() {
        let at_cap = frame(0, &vec![0; MAX_FRAME_PAYLOAD]).expect("at cap");
        assert_eq!(at_cap.len(), MAX_FRAME_PAYLOAD + 5);

        let mut cap_header = vec![0];
        let cap = u32::try_from(MAX_FRAME_PAYLOAD).expect("cap fits u32");
        cap_header.extend_from_slice(&cap.to_be_bytes());
        let error = frames(&cap_header)
            .next()
            .expect("error")
            .expect_err("truncated at cap");
        assert_eq!(error.message(), "unexpected EOF");

        let error = frame(0, &vec![0; MAX_FRAME_PAYLOAD + 1]).expect_err("oversized");
        assert!(error.message().contains("exceeds 67108864 bytes"));
        let len = u32::try_from(MAX_FRAME_PAYLOAD + 1).expect("u32");
        let mut header = vec![0];
        header.extend_from_slice(&len.to_be_bytes());
        let error = frames(&header)
            .next()
            .expect("error")
            .expect_err("oversized");
        assert!(error.message().contains("got 67108865"));
    }

    #[test]
    fn truncated_frame_and_trailing_bytes_are_unexpected_eof() {
        let encoded = frame(0, b"hello").expect("frame");
        for truncated in [&encoded[..3], &encoded[..8]] {
            let error = deframe(truncated).expect_err("truncated");
            assert_eq!(error.message(), "unexpected EOF");
        }
        let mut trailing = encoded;
        trailing.push(0);
        let error = deframe(&trailing).expect_err("trailing");
        assert_eq!(error.message(), "unexpected EOF");
    }

    #[test]
    fn iterator_ends_after_an_error() {
        let mut iter = frames(&[0]);
        assert!(iter.next().is_some());
        assert!(iter.next().is_none());
    }

    #[test]
    fn protobuf_builder_round_trips_all_supported_wire_types() {
        let large = (1_u64 << 53) + 1;
        let encoded = ProtoBuf::new()
            .varint(1, large)
            .fixed64(2, 0x0102_0304_0506_0708)
            .string(3, "text")
            .fixed32(5, 0x1122_3344)
            .message(6, ProtoBuf::new().varint(1, 9))
            .double(7, 1.5)
            .finish();
        let parsed = fields(&encoded);
        assert_eq!(parsed.len(), 6);
        assert_eq!(parsed[0].as_varint(), Some(large));
        assert_eq!(parsed[1].value, WireValue::Fixed64(0x0102_0304_0506_0708));
        assert_eq!(parsed[2].as_string().as_deref(), Some("text"));
        assert_eq!(parsed[3].value, WireValue::Fixed32(0x1122_3344));
        assert_eq!(
            parsed[4].as_message().expect("nested")[0].as_varint(),
            Some(9)
        );
        assert_eq!(parsed[5].value, WireValue::Fixed64(1.5_f64.to_bits()));
        assert_eq!(parsed[0].as_bytes(), None);
    }

    #[test]
    fn malformed_protobuf_tail_keeps_the_valid_prefix() {
        let valid = ProtoBuf::new().varint(1, 7).string(2, "ok").finish();
        let tails: [&[u8]; 5] = [
            &[0x80],
            &[(3 << 3) | 3],
            &[4 << 3, 0x80],
            &[(5 << 3) | 2, 10, 0],
            &[(6 << 3) | 1, 1, 2, 3],
        ];
        for tail in tails {
            let mut encoded = valid.clone();
            encoded.extend_from_slice(tail);
            let parsed = fields(&encoded);
            assert_eq!(parsed.len(), 2, "tail {tail:?}");
            assert_eq!(parsed[0].as_varint(), Some(7));
            assert_eq!(parsed[1].as_string().as_deref(), Some("ok"));
        }
    }

    #[test]
    fn fixed32_truncation_is_also_a_dropped_tail() {
        let mut encoded = ProtoBuf::new().varint(1, 1).finish();
        encoded.extend_from_slice(&[(2 << 3) | 5, 1, 2]);
        let parsed = fields(&encoded);
        assert_eq!(
            parsed,
            [Field {
                number: 1,
                value: WireValue::Varint(1)
            }]
        );
    }

    #[test]
    fn deframe_rejects_an_empty_body() {
        assert_eq!(deframe(&[]).expect_err("empty").message(), "unexpected EOF");
    }
}
