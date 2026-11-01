//! Observes completed replies and updates conversation affinity.

use futures_util::StreamExt;
use hofund_ir::{StreamEvent, Usage};
use hofund_plugin_api::{BodyStream, Completion};
use serde_json::Value;

use crate::affinity::{AffinityState, Answerer, CacheUsage};

pub(crate) struct Reply {
    pub(crate) state: AffinityState,
    pub(crate) key: String,
    pub(crate) turn: u64,
    pub(crate) strict: bool,
    pub(crate) member: String,
    pub(crate) model: String,
    pub(crate) effort: String,
    pub(crate) stood_in: bool,
}

impl Reply {
    fn answered(self, usage: CacheUsage) {
        self.state.answered_as(
            &self.key,
            Answerer {
                member: &self.member,
                model: &self.model,
                effort: &self.effort,
                stood_in: self.stood_in,
            },
            self.turn,
            usage,
        );
    }

    fn broken(self) {
        self.state.unanswered(&self.key, &self.member, self.strict);
    }
}

impl Drop for Reply {
    fn drop(&mut self) {
        self.state.opened(&self.key);
    }
}

pub(crate) fn watch(completion: Completion, reply: Reply) -> Completion {
    match completion {
        Completion::Unary(response) => {
            let usage = response
                .usage
                .as_ref()
                .map_or_else(CacheUsage::default, from_ir);
            reply.answered(usage);
            Completion::Unary(response)
        }
        Completion::Stream(stream) => {
            let stream = futures_util::stream::unfold(
                (stream, Some(reply), CacheUsage::default()),
                |(mut stream, mut reply, mut usage)| async move {
                    match stream.next().await {
                        Some(Ok(event)) => {
                            note_ir(&event, &mut usage);
                            Some((Ok(event), (stream, reply, usage)))
                        }
                        Some(Err(error)) => {
                            if let Some(reply) = reply.take() {
                                reply.broken();
                            }
                            Some((Err(error), (stream, reply, usage)))
                        }
                        None => {
                            if let Some(reply) = reply.take() {
                                reply.answered(usage);
                            }
                            None
                        }
                    }
                },
            );
            Completion::Stream(Box::pin(stream))
        }
        Completion::Raw(body) => {
            let stream = futures_util::stream::unfold(
                (body, Some(reply), RawUsage::default()),
                |(mut body, mut reply, mut raw)| async move {
                    match body.next_chunk().await {
                        Some(Ok(chunk)) => {
                            raw.push(&chunk);
                            Some((Ok(chunk), (body, reply, raw)))
                        }
                        Some(Err(error)) => {
                            if let Some(reply) = reply.take() {
                                reply.broken();
                            }
                            Some((Err(error), (body, reply, raw)))
                        }
                        None => {
                            if let Some(reply) = reply.take() {
                                reply.answered(raw.finish());
                            }
                            None
                        }
                    }
                },
            );
            Completion::Raw(BodyStream::from_stream(stream))
        }
        _ => completion,
    }
}

fn from_ir(usage: &Usage) -> CacheUsage {
    CacheUsage {
        input: usage.input_tokens,
        read: usage.cache_read_tokens.unwrap_or(0),
        write: usage.cache_write_tokens.unwrap_or(0),
        write_hour: usage
            .extensions
            .get("cache_creation")
            .and_then(|creation| creation.get("ephemeral_1h_input_tokens"))
            .and_then(Value::as_u64)
            .unwrap_or(0),
    }
}

fn note_ir(event: &StreamEvent, usage: &mut CacheUsage) {
    let patch = match event {
        StreamEvent::MessageStart(start) => start.usage.as_ref(),
        StreamEvent::MessageDelta(delta) => delta.usage.as_ref(),
        _ => None,
    };
    if let Some(patch) = patch {
        if patch.input_tokens.is_some() {
            usage.input = patch.input_tokens;
        }
        if let Some(read) = patch.cache_read_tokens {
            usage.read = read;
        }
        if let Some(write) = patch.cache_write_tokens {
            usage.write = write;
        }
        if patch.extensions.get("cache_creation").is_some() {
            usage.write_hour = patch
                .extensions
                .get("cache_creation")
                .and_then(|creation| creation.get("ephemeral_1h_input_tokens"))
                .and_then(Value::as_u64)
                .unwrap_or(0);
        }
    }
}

fn merge(usage: &mut CacheUsage, patch: CacheUsage) {
    if patch.input.is_some() {
        usage.input = patch.input;
    }
    usage.read = patch.read;
    usage.write = patch.write;
    usage.write_hour = patch.write_hour;
}

#[derive(Default)]
struct RawUsage {
    pending: Vec<u8>,
    usage: CacheUsage,
}

impl RawUsage {
    fn push(&mut self, chunk: &[u8]) {
        const MAX_PENDING: usize = 8 * 1024 * 1024;
        if self.pending.len() >= MAX_PENDING {
            return;
        }
        let remaining = MAX_PENDING - self.pending.len();
        self.pending
            .extend_from_slice(&chunk[..chunk.len().min(remaining)]);
        while let Some(end) = self.pending.iter().position(|byte| *byte == b'\n') {
            let line = self.pending.drain(..=end).collect::<Vec<_>>();
            self.note(&line);
        }
    }

    fn finish(mut self) -> CacheUsage {
        let pending = std::mem::take(&mut self.pending);
        self.note(&pending);
        self.usage
    }

    fn note(&mut self, bytes: &[u8]) {
        let Ok(text) = std::str::from_utf8(bytes) else {
            return;
        };
        let text = text.trim();
        let text = text.strip_prefix("data:").unwrap_or(text).trim();
        if text.is_empty() || text == "[DONE]" {
            return;
        }
        let Ok(value) = serde_json::from_str::<Value>(text) else {
            return;
        };
        for usage in [
            value.get("usage"),
            value.get("usageMetadata"),
            value
                .get("message")
                .and_then(|message| message.get("usage")),
            value
                .get("response")
                .and_then(|response| response.get("usage")),
        ]
        .into_iter()
        .flatten()
        {
            self.note_usage(usage);
        }
    }

    fn note_usage(&mut self, usage: &Value) {
        let input = number(
            usage,
            &["input_tokens", "prompt_tokens", "promptTokenCount"],
        );
        if input.is_some() {
            self.usage.input = input;
        }
        let read = number(
            usage,
            &[
                "cache_read_input_tokens",
                "cachedContentTokenCount",
                "cacheReadInputTokens",
            ],
        )
        .or_else(|| {
            ["prompt_tokens_details", "input_tokens_details"]
                .into_iter()
                .find_map(|name| {
                    usage
                        .get(name)
                        .and_then(|details| number(details, &["cached_tokens"]))
                })
        });
        let write = number(
            usage,
            &[
                "cache_creation_input_tokens",
                "cache_write_tokens",
                "cacheWriteInputTokens",
            ],
        );
        let hour = usage
            .get("cache_creation")
            .and_then(|creation| number(creation, &["ephemeral_1h_input_tokens"]));
        let previous = self.usage;
        merge(
            &mut self.usage,
            CacheUsage {
                input: previous.input,
                read: read.unwrap_or(previous.read),
                write: write.unwrap_or(previous.write),
                write_hour: hour.unwrap_or(previous.write_hour),
            },
        );
    }
}

fn number(value: &Value, names: &[&str]) -> Option<u64> {
    names
        .iter()
        .find_map(|name| value.get(*name).and_then(Value::as_u64))
}

#[cfg(test)]
mod tests {
    use hofund_ir::{Extensions, MessageDelta, MessageStart, StreamEvent, Usage};

    use super::{CacheUsage, note_ir};

    #[test]
    fn c2e_output_only_stream_usage_preserves_cache_counters() {
        let mut usage = CacheUsage::default();
        note_ir(
            &StreamEvent::MessageStart(MessageStart {
                id: None,
                model: None,
                usage: Some(Usage {
                    input_tokens: Some(5000),
                    cache_read_tokens: Some(2000),
                    cache_write_tokens: Some(3000),
                    ..Usage::default()
                }),
                extensions: Extensions::new(),
            }),
            &mut usage,
        );
        note_ir(
            &StreamEvent::MessageDelta(MessageDelta {
                stop: None,
                usage: Some(Usage {
                    output_tokens: Some(10),
                    ..Usage::default()
                }),
                extensions: Extensions::new(),
            }),
            &mut usage,
        );
        assert_eq!(usage.input, Some(5000));
        assert_eq!(usage.read, 2000);
        assert_eq!(usage.write, 3000);
    }
}
