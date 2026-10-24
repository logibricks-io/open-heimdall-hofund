//! Fold a stream of events into one assistant response.

use std::collections::BTreeMap;

use crate::content::{ContentPart, TextPart, ThinkingPart, ToolArguments, ToolCallPart};
use crate::extensions::Extensions;
use crate::message::{Message, Role};
use crate::response::Response;
use crate::stream::{BlockStart, StreamEvent};

#[derive(Default)]
struct Block {
    kind: Option<BlockStart>,
    text: String,
    signature: Option<String>,
    redacted_data: Option<String>,
    tool_id: String,
    tool_name: String,
    tool_args: String,
    unknown_kind: Option<String>,
}

/// Folds `events` into one assistant response.
///
/// Content blocks are ordered by index. Tool-argument fragments are
/// concatenated and then parsed; text that is not JSON is kept raw. Ping
/// events are ignored. A later usage event overwrites counters it sets.
///
/// This function does not fail on incomplete JSON arguments. Callers that need
/// a hard failure can inspect [`ToolArguments`].
#[must_use]
pub fn assemble(events: &[StreamEvent]) -> Response {
    let mut id = None;
    let mut model = None;
    let mut usage = None;
    let mut stop = None;
    let mut extensions = Extensions::new();
    let mut blocks: BTreeMap<u32, Block> = BTreeMap::new();

    for event in events {
        match event {
            StreamEvent::MessageStart(start) => {
                if id.is_none() {
                    id.clone_from(&start.id);
                }
                if model.is_none() {
                    model.clone_from(&start.model);
                }
                merge_usage(&mut usage, start.usage.clone());
                extensions.absorb(&start.extensions);
            }
            StreamEvent::ContentBlockStart(start) => {
                let block = blocks.entry(start.index).or_default();
                block.kind = Some(start.block.clone());
                if let BlockStart::ToolCall { id, name } = &start.block {
                    block.tool_id.clone_from(id);
                    block.tool_name.clone_from(name);
                }
                if let BlockStart::Thinking {
                    data: Some(data), ..
                } = &start.block
                {
                    block.redacted_data = Some(data.clone());
                }
                if let BlockStart::Unknown { kind } = &start.block {
                    block.unknown_kind = Some(kind.clone());
                }
            }
            StreamEvent::TextDelta(delta) => {
                blocks
                    .entry(delta.index)
                    .or_default()
                    .text
                    .push_str(&delta.text);
            }
            StreamEvent::ThinkingDelta(delta) => {
                let block = blocks.entry(delta.index).or_default();
                if block.kind.is_none() {
                    block.kind = Some(BlockStart::Thinking {
                        redacted: false,
                        data: None,
                    });
                }
                block.text.push_str(&delta.text);
            }
            StreamEvent::SignatureDelta(delta) => {
                blocks.entry(delta.index).or_default().signature = Some(delta.signature.clone());
            }
            StreamEvent::ToolArgumentsDelta(delta) => {
                blocks
                    .entry(delta.index)
                    .or_default()
                    .tool_args
                    .push_str(&delta.partial_json);
            }
            StreamEvent::ContentBlockStop(_)
            | StreamEvent::Ping(_)
            | StreamEvent::Unknown(_)
            | StreamEvent::MessageStop(_) => {}
            StreamEvent::MessageDelta(delta) => {
                if delta.stop.is_some() {
                    stop.clone_from(&delta.stop);
                }
                merge_usage(&mut usage, delta.usage.clone());
                extensions.absorb(&delta.extensions);
            }
        }
    }

    let mut content = Vec::new();
    for (_, block) in blocks {
        if let Some(part) = finish_block(block) {
            content.push(part);
        }
    }

    Response {
        id,
        model,
        message: Message {
            role: Role::Assistant,
            content,
            extensions: Extensions::new(),
        },
        stop,
        usage,
        extensions,
    }
}

fn merge_usage(slot: &mut Option<crate::usage::Usage>, patch: Option<crate::usage::Usage>) {
    let Some(patch) = patch else {
        return;
    };
    match slot {
        Some(current) => current.merge_from(&patch),
        None => *slot = Some(patch),
    }
}

fn finish_block(block: Block) -> Option<ContentPart> {
    let kind = block.kind.clone().or_else(|| infer_kind(&block))?;
    Some(match kind {
        BlockStart::Text => ContentPart::Text(TextPart {
            text: block.text,
            extensions: Extensions::new(),
        }),
        BlockStart::Thinking { redacted, data } => ContentPart::Thinking(ThinkingPart {
            text: block.text,
            signature: block.signature,
            redacted,
            data: block.redacted_data.or(data),
            extensions: Extensions::new(),
        }),
        BlockStart::ToolCall { id, name } => ContentPart::ToolCall(ToolCallPart {
            id: if block.tool_id.is_empty() {
                id
            } else {
                block.tool_id
            },
            name: if block.tool_name.is_empty() {
                name
            } else {
                block.tool_name
            },
            arguments: ToolArguments::from_text(&block.tool_args),
            extensions: Extensions::new(),
        }),
        BlockStart::Unknown { kind } => ContentPart::Unknown(crate::content::UnknownPart {
            kind: block.unknown_kind.unwrap_or(kind),
            extensions: Extensions::new(),
        }),
    })
}

fn infer_kind(block: &Block) -> Option<BlockStart> {
    if !block.tool_args.is_empty() || !block.tool_name.is_empty() {
        return Some(BlockStart::ToolCall {
            id: block.tool_id.clone(),
            name: block.tool_name.clone(),
        });
    }
    if block.signature.is_some() || block.redacted_data.is_some() {
        return Some(BlockStart::Thinking {
            redacted: block.redacted_data.is_some(),
            data: block.redacted_data.clone(),
        });
    }
    if !block.text.is_empty() {
        return Some(BlockStart::Text);
    }
    None
}
