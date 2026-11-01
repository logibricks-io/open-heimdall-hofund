//! Protocol codecs that encode and decode between external LLM APIs and the canonical
//! IR. The design is hub-and-spoke: each protocol is implemented once against `ir`,
//! so N client protocols and M upstream protocols compose without a direct N-by-M
//! translator matrix.
//!
//! [`AnthropicMessages`], [`OpenAiChat`], [`OpenAiResponses`],
//! [`GenerativeLanguage`], and [`Converse`] are implemented. Field mappings and lossy conversions
//! are documented in `docs/codecs.md`. Protocol names belong in this crate;
//! vendor capability gaps belong in `quirks`.

mod anthropic;
mod conversation;
mod converse;
mod custom_input;
mod error;
mod eventstream;
mod generative;
mod jsonutil;
mod openai_chat;
mod openai_responses;
mod passthrough;
mod sse;

pub use anthropic::AnthropicMessages;
pub use conversation::{SESSION_HEADERS, SUBAGENT_HEADER, Turn, conversation_key, turn_in};
pub use converse::Converse;
pub use error::CodecError;
pub use generative::{GenerativeLanguage, plain_code_assist_tools, plain_schema};
pub use openai_chat::OpenAiChat;
pub use openai_responses::{OpenAiResponses, UNMAPPED_TOOLS};
pub use passthrough::RawTap;

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

    /// Encodes a response for the client that sent `request`.
    ///
    /// The default ignores the request. A codec that has to restore names the
    /// client used, such as a Responses namespace flattened on the way out,
    /// overrides this.
    ///
    /// # Errors
    ///
    /// Returns [`CodecError`] when the response role or content cannot be represented.
    fn encode_response_for(
        &self,
        request: &Request,
        response: &Response,
    ) -> Result<Vec<u8>, CodecError> {
        let _ = request;
        self.encode_response(response)
    }

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

    /// An encoder that accepts one event at a time.
    ///
    /// The bytes from each [`StreamEncoder::push`] are the SSE for that event
    /// only. State such as an open block stays on the encoder.
    fn stream_encoder(&self) -> Box<dyn StreamEncoder>;

    /// An encoder for a reply to `request`.
    ///
    /// The default ignores the request. A codec that changes the stream based
    /// on what the client asked for, such as Chat Completions usage chunks,
    /// overrides this.
    fn stream_encoder_for(&self, _request: &Request) -> Box<dyn StreamEncoder> {
        self.stream_encoder()
    }

    /// JSON paths whose string value is the model name the client sees.
    ///
    /// Each path is object keys from the root of one JSON value. A same-protocol
    /// body is patched only at these paths. Protocols with no such field return
    /// an empty slice.
    fn model_paths(&self) -> &'static [&'static [&'static str]] {
        &[&["model"]]
    }

    /// Content type of a streaming body in this protocol.
    fn stream_content_type(&self) -> &'static str {
        "text/event-stream"
    }

    /// Content type of a unary or streaming body in this protocol.
    fn response_content_type(&self, stream: bool) -> &'static str {
        if stream {
            self.stream_content_type()
        } else {
            "application/json"
        }
    }

    /// Reads tokens from a raw stream.
    ///
    /// When `client_model` and `upstream_model` differ, the bytes of each
    /// [`Codec::model_paths`] string are spliced. Equal names are forwarded
    /// without parsing the body for a rewrite, including a model name the
    /// upstream returns that was not the name sent.
    fn raw_tap(&self, client_model: &str, upstream_model: &str) -> Box<dyn RawTap> {
        Box::new(passthrough::DecoderTap::new(
            self.stream_decoder(),
            self.model_paths(),
            client_model,
            upstream_model,
        ))
    }

    /// Input and output tokens in a unary body, when this protocol reports them.
    fn response_tokens(&self, bytes: &[u8]) -> (Option<i64>, Option<i64>) {
        let Ok(response) = self.decode_response(bytes) else {
            return (None, None);
        };
        let Some(usage) = response.usage else {
            return (None, None);
        };
        (
            usage
                .input_tokens
                .and_then(|value| i64::try_from(value).ok()),
            usage
                .output_tokens
                .and_then(|value| i64::try_from(value).ok()),
        )
    }

    /// Replaces model fields in a complete unary body.
    ///
    /// Equal names are returned unchanged. Differing names splice the model
    /// string at [`Codec::model_paths`] and leave every other byte as it
    /// arrived. A body that cannot be parsed is returned unchanged.
    fn rewrite_model(&self, bytes: &[u8], client_model: &str, upstream_model: &str) -> Vec<u8> {
        passthrough::rewrite_all(bytes, self.model_paths(), client_model, upstream_model)
    }

    /// Encodes a stream error as a protocol frame, not a clean end of stream.
    ///
    /// The default frame is `event: error` and the JSON from [`Codec::encode_error`].
    /// A stream that fails after the first event still ends with this frame.
    /// Chat, Responses, and Generative Language override the shape.
    fn encode_stream_error(&self, status: u16, message: &str) -> Vec<u8> {
        let json = self.encode_error(status, message);
        let mut out = Vec::with_capacity(json.len() + 16);
        out.extend_from_slice(b"event: error\ndata: ");
        out.extend_from_slice(&json);
        out.extend_from_slice(b"\n\n");
        out
    }

    /// Encodes an error body in this protocol's JSON shape.
    ///
    /// `status` is the HTTP status the caller will send. The returned bytes are
    /// the body only.
    fn encode_error(&self, status: u16, message: &str) -> Vec<u8> {
        let _ = status;
        serde_json::to_vec(&serde_json::json!({
            "error": { "type": "api_error", "message": message }
        }))
        .unwrap_or_else(|_| {
            b"{\"error\":{\"type\":\"api_error\",\"message\":\"request failed\"}}".to_vec()
        })
    }

    /// Encodes an input-token count.
    ///
    /// `estimated` is true when no upstream counted and the number is a
    /// character estimate. The default field is `input_tokens`.
    fn encode_token_count(&self, tokens: u64, estimated: bool) -> Vec<u8> {
        format!(r#"{{"input_tokens":{tokens},"estimated":{estimated}}}"#).into_bytes()
    }
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

/// Incremental SSE encode for one response stream.
pub trait StreamEncoder: Send {
    /// Writes the SSE produced by `event`.
    ///
    /// An empty buffer means the event has no wire form in this protocol.
    ///
    /// # Errors
    ///
    /// Returns [`CodecError`] when the event cannot be represented.
    fn push(&mut self, event: &StreamEvent) -> Result<Vec<u8>, CodecError>;

    /// Writes any terminal frames the encoder was holding.
    ///
    /// The default implementation writes nothing. An encoder that delays a
    /// closing event until the turn ends uses this to flush it.
    ///
    /// # Errors
    ///
    /// Returns [`CodecError`] when the trailing frame cannot be represented.
    fn finish(&mut self) -> Result<Vec<u8>, CodecError> {
        Ok(Vec::new())
    }
}
