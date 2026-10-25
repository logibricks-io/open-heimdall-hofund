//! A canonical chat request.

use serde::{Deserialize, Serialize};

use crate::extensions::Extensions;
use crate::message::Message;
use crate::params::GenerationParams;
use crate::tools::{Tool, ToolChoice};

/// One chat-style request.
///
/// This type performs no I/O. Codecs translate protocol payloads into it and
/// back out of it.
#[non_exhaustive]
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct Request {
    /// Model identifier, as the caller named it.
    pub model: String,
    /// Conversation so far, in order.
    ///
    /// System and developer messages stay in this list at the position the
    /// source protocol used. A codec may hoist them when its protocol has a
    /// single system channel.
    pub messages: Vec<Message>,
    /// Tools the model may call.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub tools: Vec<Tool>,
    /// How the model may use tools. `None` means the caller did not say.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub tool_choice: Option<ToolChoice>,
    /// Sampling and length controls.
    #[serde(default, skip_serializing_if = "params_are_default")]
    pub params: GenerationParams,
    /// Whether the caller asked for a stream of events.
    #[serde(default, skip_serializing_if = "is_false")]
    pub stream: bool,
    /// Top-level fields this version does not model.
    #[serde(default, skip_serializing_if = "Extensions::is_empty")]
    pub extensions: Extensions,
}

fn params_are_default(params: &GenerationParams) -> bool {
    params == &GenerationParams::default()
}

fn is_false(value: &bool) -> bool {
    !*value
}

impl Request {
    /// A request with no messages and default parameters.
    ///
    /// Add the other parts with the `with_*` methods. A new field can take a
    /// default here without breaking callers. A positional constructor cannot.
    #[must_use]
    pub fn new(model: impl Into<String>) -> Self {
        Self {
            model: model.into(),
            messages: Vec::new(),
            tools: Vec::new(),
            tool_choice: None,
            params: GenerationParams::default(),
            stream: false,
            extensions: Extensions::new(),
        }
    }

    /// Replaces the conversation.
    #[must_use]
    pub fn with_messages(mut self, messages: Vec<Message>) -> Self {
        self.messages = messages;
        self
    }

    /// Replaces the tool list.
    #[must_use]
    pub fn with_tools(mut self, tools: Vec<Tool>) -> Self {
        self.tools = tools;
        self
    }

    /// Sets the tool choice.
    #[must_use]
    pub fn with_tool_choice(mut self, tool_choice: Option<ToolChoice>) -> Self {
        self.tool_choice = tool_choice;
        self
    }

    /// Replaces the generation parameters.
    #[must_use]
    pub fn with_params(mut self, params: GenerationParams) -> Self {
        self.params = params;
        self
    }

    /// Sets whether the caller asked for a stream.
    #[must_use]
    pub fn with_stream(mut self, stream: bool) -> Self {
        self.stream = stream;
        self
    }

    /// Replaces the extension bag.
    #[must_use]
    pub fn with_extensions(mut self, extensions: Extensions) -> Self {
        self.extensions = extensions;
        self
    }

    /// Drops every extension bag on the request.
    ///
    /// Use this before encoding with a different protocol when foreign fields
    /// must not leak into that protocol's JSON.
    pub fn strip_extensions(&mut self) {
        self.extensions.clear();
        self.params.extensions.clear();
        if let Some(choice) = &mut self.tool_choice {
            choice.extensions.clear();
        }
        for tool in &mut self.tools {
            tool.extensions.clear();
        }
        for message in &mut self.messages {
            message.strip_extensions();
        }
    }
}
