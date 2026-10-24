//! Content parts inside a message.

use serde::{Deserialize, Serialize};
use serde_json::Value;

use crate::extensions::Extensions;

/// One unit of message content.
///
/// `Unknown` keeps a part type this IR does not model yet. Codecs emit it again
/// under the original type name so a new part can pass through unchanged.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum ContentPart {
    /// Plain text.
    Text(TextPart),
    /// An image the model can see.
    Image(ImagePart),
    /// A call the assistant wants the client to run.
    ToolCall(ToolCallPart),
    /// The client's result for a previous tool call.
    ToolResult(ToolResultPart),
    /// Model reasoning, including a signature or a redacted blob when the
    /// protocol has one.
    Thinking(ThinkingPart),
    /// A part whose type is not one of the variants above.
    Unknown(UnknownPart),
}

/// Text content.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct TextPart {
    /// The text itself.
    pub text: String,
    /// Fields this version does not model.
    #[serde(default, skip_serializing_if = "Extensions::is_empty")]
    pub extensions: Extensions,
}

/// Image content.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct ImagePart {
    /// Where the bytes live.
    pub source: ImageSource,
    /// Hint about how much detail a consumer should spend on the image.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub detail: Option<ImageDetail>,
    /// Fields this version does not model.
    #[serde(default, skip_serializing_if = "Extensions::is_empty")]
    pub extensions: Extensions,
}

/// Where image bytes live.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum ImageSource {
    /// An `http`, `https`, or other URL.
    Url {
        /// The URL.
        url: String,
    },
    /// Base64-encoded bytes.
    Base64 {
        /// Media type, such as `image/png`.
        media_type: String,
        /// Base64 payload, without a `data:` prefix.
        data: String,
    },
}

/// How much detail to spend on an image.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum ImageDetail {
    /// Let the consumer choose.
    Auto,
    /// Prefer a smaller view.
    Low,
    /// Prefer a larger view.
    High,
    /// A value this version does not name.
    Other(String),
}

impl Serialize for ImageDetail {
    fn serialize<S>(&self, serializer: S) -> Result<S::Ok, S::Error>
    where
        S: serde::Serializer,
    {
        let text = match self {
            Self::Auto => "auto",
            Self::Low => "low",
            Self::High => "high",
            Self::Other(value) => value.as_str(),
        };
        serializer.serialize_str(text)
    }
}

impl<'de> Deserialize<'de> for ImageDetail {
    fn deserialize<D>(deserializer: D) -> Result<Self, D::Error>
    where
        D: serde::Deserializer<'de>,
    {
        let text = String::deserialize(deserializer)?;
        Ok(match text.as_str() {
            "auto" => Self::Auto,
            "low" => Self::Low,
            "high" => Self::High,
            _ => Self::Other(text),
        })
    }
}

/// A tool call requested by the assistant.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct ToolCallPart {
    /// Call identifier used to join the later result.
    pub id: String,
    /// Tool name.
    pub name: String,
    /// Arguments, either parsed JSON or the original text when it is not JSON.
    pub arguments: ToolArguments,
    /// Fields this version does not model.
    #[serde(default, skip_serializing_if = "Extensions::is_empty")]
    pub extensions: Extensions,
}

/// Tool arguments.
///
/// Parsed JSON keeps object key order. Whitespace outside that structure is not
/// significant and is not preserved.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(tag = "form", rename_all = "snake_case")]
pub enum ToolArguments {
    /// Arguments that parsed as JSON.
    Json {
        /// Parsed JSON value.
        value: Value,
    },
    /// Argument text that is not JSON, including a partial stream fragment
    /// stored on a finished message.
    Raw {
        /// Original argument text.
        text: String,
    },
}

impl ToolArguments {
    /// Parses `raw` as JSON. Invalid JSON stays raw so the text is not lost.
    #[must_use]
    pub fn from_text(raw: &str) -> Self {
        match serde_json::from_str::<Value>(raw) {
            Ok(value) => Self::Json { value },
            Err(_) => Self::Raw {
                text: raw.to_owned(),
            },
        }
    }

    /// Compact JSON for a parsed value, or the original text for raw arguments.
    #[must_use]
    pub fn to_text(&self) -> String {
        match self {
            Self::Json { value } => {
                serde_json::to_string(value).expect("a JSON value always serializes to a string")
            }
            Self::Raw { text } => text.clone(),
        }
    }

    /// The parsed JSON object, when the arguments are a JSON object.
    #[must_use]
    pub fn as_object(&self) -> Option<&serde_json::Map<String, Value>> {
        match self {
            Self::Json {
                value: Value::Object(object),
            } => Some(object),
            _ => None,
        }
    }
}

/// A tool result returned by the client.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct ToolResultPart {
    /// Identifier of the tool call this result answers.
    pub id: String,
    /// Whether the tool failed.
    #[serde(default, skip_serializing_if = "is_false")]
    pub is_error: bool,
    /// Result body. Text and images are the parts codecs know how to send.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub content: Vec<ContentPart>,
    /// Fields this version does not model.
    #[serde(default, skip_serializing_if = "Extensions::is_empty")]
    pub extensions: Extensions,
}

/// Reasoning content.
///
/// A redacted block has `redacted` set and carries the opaque `data` blob. The
/// readable `text` may be empty in that case. A signature is the protocol's
/// integrity token for the reasoning text, when it has one.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct ThinkingPart {
    /// Readable reasoning. Empty when the protocol only sent a redacted blob.
    #[serde(default, skip_serializing_if = "String::is_empty")]
    pub text: String,
    /// Integrity token for the reasoning text.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub signature: Option<String>,
    /// Whether `data` is an opaque redacted blob rather than readable text.
    #[serde(default, skip_serializing_if = "is_false")]
    pub redacted: bool,
    /// Opaque redacted payload.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub data: Option<String>,
    /// Fields this version does not model.
    #[serde(default, skip_serializing_if = "Extensions::is_empty")]
    pub extensions: Extensions,
}

/// A content part whose type string is not modeled.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct UnknownPart {
    /// Original type name.
    pub kind: String,
    /// Every field other than the type name.
    #[serde(default, skip_serializing_if = "Extensions::is_empty")]
    pub extensions: Extensions,
}

fn is_false(value: &bool) -> bool {
    !*value
}

impl ContentPart {
    /// A text part with an empty extension bag.
    #[must_use]
    pub fn text(text: impl Into<String>) -> Self {
        Self::Text(TextPart {
            text: text.into(),
            extensions: Extensions::new(),
        })
    }

    /// Readable reasoning with no signature and an empty extension bag.
    #[must_use]
    pub fn thinking(text: impl Into<String>) -> Self {
        Self::Thinking(ThinkingPart {
            text: text.into(),
            signature: None,
            redacted: false,
            data: None,
            extensions: Extensions::new(),
        })
    }

    /// Drops extension bags on this part and on nested tool-result content.
    pub fn strip_extensions(&mut self) {
        match self {
            Self::Text(part) => part.extensions.clear(),
            Self::Image(part) => part.extensions.clear(),
            Self::ToolCall(part) => part.extensions.clear(),
            Self::ToolResult(part) => {
                part.extensions.clear();
                for child in &mut part.content {
                    child.strip_extensions();
                }
            }
            Self::Thinking(part) => part.extensions.clear(),
            Self::Unknown(part) => part.extensions.clear(),
        }
    }
}
