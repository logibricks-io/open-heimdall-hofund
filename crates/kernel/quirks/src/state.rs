use std::collections::BTreeSet;
use std::sync::Mutex;

use hofund_ir::fold_object_root;
use serde_json::Value;

use crate::json::{self, Edit};

/// Learned request-shape refusals for one provider instance.
#[derive(Debug, Default)]
pub struct RequestState {
    root_unions: Mutex<BTreeSet<String>>,
    token_field: Mutex<Option<TokenField>>,
}

/// The Chat Completions reply-length field.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum TokenField {
    /// `max_tokens`.
    MaxTokens,
    /// `max_completion_tokens`.
    CompletionTokens,
}

impl TokenField {
    /// The wire field name.
    #[must_use]
    pub fn as_str(self) -> &'static str {
        match self {
            Self::MaxTokens => "max_tokens",
            Self::CompletionTokens => "max_completion_tokens",
        }
    }

    fn other(self) -> Self {
        match self {
            Self::MaxTokens => Self::CompletionTokens,
            Self::CompletionTokens => Self::MaxTokens,
        }
    }
}

impl RequestState {
    /// Remembers that `model` refused a root union.
    pub fn learn_root_union(&self, model: &str) {
        self.root_unions
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .insert(model.to_owned());
    }

    /// Remembers which reply-length field the provider refused.
    pub fn learn_token_field(&self, wanted: TokenField) {
        *self
            .token_field
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner) = Some(wanted.other());
    }

    fn folds_root(&self, model: &str) -> bool {
        self.root_unions
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .contains(model)
    }

    fn wanted_token_field(&self) -> Option<TokenField> {
        *self
            .token_field
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
    }
}

/// Folds Chat or Responses function parameters when xAI or a learned refusal
/// requires it, and reports the resulting body and whether it changed.
#[must_use]
pub fn fold_object_roots(
    base_url: &str,
    protocol: &str,
    model: &str,
    state: &RequestState,
    body: &[u8],
) -> (Vec<u8>, bool) {
    if host(base_url) != "api.x.ai" && !state.folds_root(model) {
        return (body.to_vec(), false);
    }
    fold_encoded(protocol, body)
}

/// Folds every eligible function schema in a Chat or Responses body.
#[must_use]
pub fn fold_encoded(protocol: &str, body: &[u8]) -> (Vec<u8>, bool) {
    if !matches!(protocol, "openai-chat" | "openai-responses") {
        return (body.to_vec(), false);
    }
    let Ok(mut root) = serde_json::from_slice::<Value>(body) else {
        return (body.to_vec(), false);
    };
    let Some(tools) = root.get_mut("tools") else {
        return (body.to_vec(), false);
    };
    if !fold_tools(protocol, tools) {
        return (body.to_vec(), false);
    }
    match serde_json::to_vec(&root) {
        Ok(folded) => (folded, true),
        Err(_) => (body.to_vec(), false),
    }
}

fn fold_tools(protocol: &str, tools: &mut Value) -> bool {
    let Some(tools) = tools.as_array_mut() else {
        return false;
    };
    let mut changed = false;
    for tool in tools {
        if protocol == "openai-responses"
            && tool.get("type").and_then(Value::as_str) == Some("namespace")
        {
            if let Some(nested) = tool.get_mut("tools") {
                changed |= fold_tools(protocol, nested);
            }
            continue;
        }
        let function = if protocol == "openai-chat" {
            let Some(function) = tool.get_mut("function") else {
                continue;
            };
            function
        } else {
            tool
        };
        if let Some(parameters) = function.get_mut("parameters") {
            changed |= fold_object_root(parameters);
        }
    }
    changed
}

/// Whether an error body refuses a union at a tool schema's root.
#[must_use]
pub fn root_union_refusal(body: &[u8]) -> bool {
    let text = String::from_utf8_lossy(body).to_ascii_lowercase();
    text.contains("tool parameter root must be an object type")
        || text.contains("does not support oneof, allof, or anyof at the top level")
        || text.contains("schema must have type")
            && text.contains("object")
            && text.contains("and not have")
            && text.contains("oneof")
        || text.contains("schema must be a json schema of")
            && text.contains("type:")
            && text.contains("object")
}

/// Names the reply-length field a refusal asks for, if it names one.
#[must_use]
pub fn token_field_wanted(body: &[u8]) -> Option<TokenField> {
    let text = String::from_utf8_lossy(body).to_ascii_lowercase();
    for (at, _) in text.match_indices("use") {
        if at > 0 && text.as_bytes()[at - 1].is_ascii_alphanumeric() {
            continue;
        }
        let rest = &text[at + 3..];
        if !rest.starts_with(char::is_whitespace) {
            continue;
        }
        let rest = rest.trim_start();
        let rest = rest.trim_start_matches(['\'', '"', '`']);
        if rest.starts_with("max_completion_tokens") {
            return Some(TokenField::CompletionTokens);
        }
        if rest.starts_with("max_tokens") {
            return Some(TokenField::MaxTokens);
        }
    }
    None
}

/// Names a Chat reply-length field and reports whether the body changed.
#[must_use]
pub fn with_token_field(body: &[u8], wanted: TokenField) -> (Vec<u8>, bool) {
    let Some(root) = json::parse(body) else {
        return (body.to_vec(), false);
    };
    let other = wanted.other().as_str();
    let Some(members) = root.members() else {
        return (body.to_vec(), false);
    };
    let Some(field) = members.iter().find(|member| member.key == other) else {
        return (body.to_vec(), false);
    };
    if root.get(wanted.as_str()).is_some() {
        return (
            json::apply(body, json::remove_fields(&root, &[other])),
            true,
        );
    }
    let replacement = format!("\"{}\"", wanted.as_str());
    (
        json::apply(
            body,
            vec![Edit::new(field.key_start, field.key_end, replacement)],
        ),
        true,
    )
}

pub(crate) fn wanted_token_field(base_url: &str, state: &RequestState) -> Option<TokenField> {
    if openai_host(base_url) {
        return Some(TokenField::CompletionTokens);
    }
    state.wanted_token_field().map(TokenField::other)
}

fn openai_host(base_url: &str) -> bool {
    let host = host(base_url);
    host == "openai.com" || host.ends_with(".openai.com")
}

fn host(base_url: &str) -> String {
    base_url
        .split_once("://")
        .map_or(base_url, |(_, rest)| rest)
        .split('/')
        .next()
        .unwrap_or_default()
        .rsplit('@')
        .next()
        .unwrap_or_default()
        .split(':')
        .next()
        .unwrap_or_default()
        .to_ascii_lowercase()
}
