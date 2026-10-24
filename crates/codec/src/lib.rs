//! Protocol codecs that encode and decode between external LLM APIs and the canonical
//! IR. The design is hub-and-spoke: each protocol is implemented once against `ir`,
//! so N client protocols and M upstream protocols compose without a direct N-by-M
//! translator matrix.
//!
//! [`AnthropicMessages`] and [`OpenAiChat`] are implemented. OpenAI Responses and
//! Gemini are not. Field mappings and lossy conversions are documented in
//! `docs/codecs.md`. Protocol names belong in this crate; vendor capability gaps
//! belong in `quirks`.

mod anthropic;
mod error;
mod jsonutil;
mod openai_chat;
mod sse;

pub use anthropic::AnthropicMessages;
pub use error::CodecError;
pub use openai_chat::OpenAiChat;

use hofund_ir::{Request, Response, StreamEvent};

/// One protocol's encode and decode functions.
///
/// Implementations borrow nothing from the caller. Streaming decode is stateful
/// and is exposed through [`Codec::stream_decoder`] so chunk boundaries can fall
/// in the middle of an SSE line.
pub trait Codec: Send + Sync {
    /// Stable protocol id, such as `anthropic-messages`.
    fn id(&self) -> &'static str;

    /// Decodes a non-streaming request body.
    ///
    /// # Errors
    ///
    /// Returns [`CodecError`] when the body is empty, not a JSON object, or
    /// missing a field this protocol requires.
    fn decode_request(&self, bytes: &[u8]) -> Result<Request, CodecError>;

    /// Encodes a request.
    ///
    /// # Errors
    ///
    /// Returns [`CodecError`] when the request uses a feature this protocol
    /// cannot send without dropping it, or when a required field is absent.
    fn encode_request(&self, request: &Request) -> Result<Vec<u8>, CodecError>;

    /// Decodes a non-streaming response body.
    ///
    /// # Errors
    ///
    /// Returns [`CodecError`] when the body is not a response this protocol can read.
    fn decode_response(&self, bytes: &[u8]) -> Result<Response, CodecError>;

    /// Encodes a response.
    ///
    /// # Errors
    ///
    /// Returns [`CodecError`] when the response role or content cannot be represented.
    fn encode_response(&self, response: &Response) -> Result<Vec<u8>, CodecError>;

    /// Decodes a complete SSE byte buffer.
    ///
    /// # Errors
    ///
    /// Returns [`CodecError`] when a frame is not UTF-8 or its data is not JSON.
    fn decode_stream(&self, bytes: &[u8]) -> Result<Vec<StreamEvent>, CodecError> {
        let mut decoder = self.stream_decoder();
        let mut events = decoder.push(bytes)?;
        events.extend(decoder.finish()?);
        Ok(events)
    }

    /// Encodes stream events as SSE.
    ///
    /// # Errors
    ///
    /// Returns [`CodecError`] when an event cannot be represented in this protocol.
    fn encode_stream(&self, events: &[StreamEvent]) -> Result<Vec<u8>, CodecError>;

    /// A decoder that accepts arbitrary chunk boundaries.
    fn stream_decoder(&self) -> Box<dyn StreamDecoder>;
}

/// Incremental SSE decode for one response stream.
pub trait StreamDecoder: Send {
    /// Feeds the next chunk. A chunk may end in the middle of a line.
    ///
    /// # Errors
    ///
    /// Returns [`CodecError`] when a completed frame is not usable.
    fn push(&mut self, chunk: &[u8]) -> Result<Vec<StreamEvent>, CodecError>;

    /// Flushes a trailing partial line and a started event.
    ///
    /// # Errors
    ///
    /// Returns [`CodecError`] when the trailing frame is not usable.
    fn finish(&mut self) -> Result<Vec<StreamEvent>, CodecError>;
}
