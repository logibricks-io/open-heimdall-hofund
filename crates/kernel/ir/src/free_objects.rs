use std::collections::BTreeMap;

use serde_json::Value;

use crate::{
    BlockStart, ContentPart, Extensions, Response, StreamEvent, Tool, ToolArguments,
    ToolArgumentsDelta,
};

/// One step toward an object encoded as JSON text in tool arguments.
#[derive(Clone, Debug, PartialEq, Eq, PartialOrd, Ord)]
pub enum FreeObjectSegment {
    /// An object property.
    Property(String),
    /// Every element of an array.
    Items,
}

/// Locations of free-form objects in the schemas of the offered tools.
#[derive(Clone, Debug, Default)]
pub struct FreeObjectPaths {
    by_tool: BTreeMap<String, Vec<Vec<FreeObjectSegment>>>,
}

impl FreeObjectPaths {
    /// Finds free-form objects below each tool's schema root.
    #[must_use]
    pub fn from_tools(tools: &[Tool]) -> Option<Self> {
        let mut by_tool: BTreeMap<String, Vec<Vec<FreeObjectSegment>>> = BTreeMap::new();
        for tool in tools {
            let mut paths = Vec::new();
            collect_schema_paths(&tool.input_schema, &mut Vec::new(), 0, &mut paths);
            if !paths.is_empty() {
                by_tool.entry(tool.name.clone()).or_default().extend(paths);
            }
        }
        (!by_tool.is_empty()).then_some(Self { by_tool })
    }

    /// Restores JSON-text objects in calls made to the declared tools.
    pub fn restore_response(&self, response: &mut Response) {
        for part in &mut response.message.content {
            let ContentPart::ToolCall(call) = part else {
                continue;
            };
            let arguments = call.arguments.to_text();
            let restored = self.restore_text(&call.name, &arguments);
            if restored != arguments {
                call.arguments = ToolArguments::from_text(&restored);
            }
        }
    }

    /// Restores JSON-text objects in one tool call's argument text.
    ///
    /// Bytes outside each replaced string are preserved. Invalid JSON, arrays,
    /// scalars, and strings that do not contain an object stay unchanged.
    #[must_use]
    pub fn restore_text(&self, tool: &str, arguments: &str) -> String {
        let Some(paths) = self.by_tool.get(tool) else {
            return arguments.to_owned();
        };
        let mut parser = SpanParser::new(arguments);
        let Some(root) = parser.parse() else {
            return arguments.to_owned();
        };
        let mut edits = Vec::new();
        for path in paths {
            collect_restores(&root, path, arguments, &mut edits);
        }
        if edits.is_empty() {
            return arguments.to_owned();
        }
        edits.sort_by_key(|edit| edit.start);
        edits.dedup_by_key(|edit| edit.start);
        let mut out = String::with_capacity(arguments.len());
        let mut last = 0;
        for edit in edits {
            if edit.start < last || edit.end > arguments.len() {
                continue;
            }
            out.push_str(&arguments[last..edit.start]);
            out.push_str(&edit.replacement);
            last = edit.end;
        }
        out.push_str(&arguments[last..]);
        out
    }

    /// Returns the free-form object paths declared for `tool`.
    #[must_use]
    pub fn paths(&self, tool: &str) -> Option<&[Vec<FreeObjectSegment>]> {
        self.by_tool.get(tool).map(Vec::as_slice)
    }
}

/// Buffers tool argument deltas until their JSON-text objects can be restored.
#[derive(Debug)]
pub struct FreeObjectEvents {
    paths: FreeObjectPaths,
    held: BTreeMap<u32, HeldCall>,
}

#[derive(Debug)]
struct HeldCall {
    name: String,
    text: String,
    extensions: Extensions,
}

impl FreeObjectEvents {
    /// Creates an event buffer for one upstream reply.
    #[must_use]
    pub fn new(paths: FreeObjectPaths) -> Self {
        Self {
            paths,
            held: BTreeMap::new(),
        }
    }

    /// Accepts one event and returns the events ready for the client.
    pub fn push(&mut self, event: StreamEvent) -> Vec<StreamEvent> {
        match event {
            StreamEvent::ContentBlockStart(start) => {
                let mut out = self.release(start.index);
                if let BlockStart::ToolCall { name, .. } = &start.block
                    && self.paths.paths(name).is_some()
                {
                    self.held.insert(
                        start.index,
                        HeldCall {
                            name: name.clone(),
                            text: String::new(),
                            extensions: Extensions::new(),
                        },
                    );
                }
                out.push(StreamEvent::ContentBlockStart(start));
                out
            }
            StreamEvent::ToolArgumentsDelta(delta) => {
                let Some(held) = self.held.get_mut(&delta.index) else {
                    return vec![StreamEvent::ToolArgumentsDelta(delta)];
                };
                held.text.push_str(&delta.partial_json);
                held.extensions.absorb(&delta.extensions);
                Vec::new()
            }
            StreamEvent::ContentBlockStop(stop) => {
                let mut out = self.release(stop.index);
                out.push(StreamEvent::ContentBlockStop(stop));
                out
            }
            StreamEvent::MessageDelta(delta) if delta.stop.is_some() => {
                let mut out = self.drain();
                out.push(StreamEvent::MessageDelta(delta));
                out
            }
            StreamEvent::MessageStop(stop) => {
                let mut out = self.drain();
                out.push(StreamEvent::MessageStop(stop));
                out
            }
            other => vec![other],
        }
    }

    /// Releases calls whose upstream stream ended without a block stop.
    pub fn finish(&mut self) -> Vec<StreamEvent> {
        self.drain()
    }

    fn release(&mut self, index: u32) -> Vec<StreamEvent> {
        let Some(held) = self.held.remove(&index) else {
            return Vec::new();
        };
        if held.text.is_empty() && held.extensions.is_empty() {
            return Vec::new();
        }
        vec![StreamEvent::ToolArgumentsDelta(ToolArgumentsDelta {
            index,
            partial_json: self.paths.restore_text(&held.name, &held.text),
            extensions: held.extensions,
        })]
    }

    fn drain(&mut self) -> Vec<StreamEvent> {
        let indexes: Vec<u32> = self.held.keys().copied().collect();
        let mut out = Vec::new();
        for index in indexes {
            out.extend(self.release(index));
        }
        out
    }
}

fn collect_schema_paths(
    schema: &Value,
    path: &mut Vec<FreeObjectSegment>,
    depth: u32,
    paths: &mut Vec<Vec<FreeObjectSegment>>,
) {
    if depth > 32 {
        return;
    }
    if let Some(properties) = schema.get("properties").and_then(Value::as_object) {
        for (name, child) in properties {
            path.push(FreeObjectSegment::Property(name.clone()));
            visit_schema(child, path, depth, paths);
            path.pop();
        }
    }
    if let Some(items) = schema.get("items").filter(|items| items.is_object()) {
        path.push(FreeObjectSegment::Items);
        visit_schema(items, path, depth, paths);
        path.pop();
    }
}

fn visit_schema(
    schema: &Value,
    path: &mut Vec<FreeObjectSegment>,
    depth: u32,
    paths: &mut Vec<Vec<FreeObjectSegment>>,
) {
    if is_free_object(schema) {
        paths.push(path.clone());
    } else {
        collect_schema_paths(schema, path, depth + 1, paths);
    }
}

fn is_free_object(schema: &Value) -> bool {
    let Some(object) = schema.as_object() else {
        return false;
    };
    if object.get("type").and_then(Value::as_str) != Some("object") {
        return false;
    }
    if object
        .get("properties")
        .is_some_and(|properties| properties.as_object().is_none_or(|map| !map.is_empty()))
    {
        return false;
    }
    if object.get("additionalProperties") == Some(&Value::Bool(false)) {
        return false;
    }
    ![
        "patternProperties",
        "anyOf",
        "oneOf",
        "allOf",
        "$ref",
        "enum",
        "const",
        "not",
        "if",
    ]
    .iter()
    .any(|key| object.contains_key(*key))
}

#[derive(Debug)]
struct SpanNode {
    start: usize,
    end: usize,
    kind: SpanKind,
}

#[derive(Debug)]
enum SpanKind {
    Object(Vec<SpanMember>),
    Array(Vec<SpanNode>),
    String,
    Other,
}

#[derive(Debug)]
struct SpanMember {
    key: String,
    value: SpanNode,
}

#[derive(Debug)]
struct RestoreEdit {
    start: usize,
    end: usize,
    replacement: String,
}

fn collect_restores(
    node: &SpanNode,
    path: &[FreeObjectSegment],
    arguments: &str,
    edits: &mut Vec<RestoreEdit>,
) {
    let Some((first, rest)) = path.split_first() else {
        if !matches!(node.kind, SpanKind::String) {
            return;
        }
        let Ok(text) = serde_json::from_str::<String>(&arguments[node.start..node.end]) else {
            return;
        };
        let trimmed = text.trim();
        if !trimmed.starts_with('{')
            || !matches!(serde_json::from_str::<Value>(trimmed), Ok(Value::Object(_)))
        {
            return;
        }
        edits.push(RestoreEdit {
            start: node.start,
            end: node.end,
            replacement: trimmed.to_owned(),
        });
        return;
    };
    match first {
        FreeObjectSegment::Items => {
            if let SpanKind::Array(items) = &node.kind {
                for item in items {
                    collect_restores(item, rest, arguments, edits);
                }
            }
        }
        FreeObjectSegment::Property(name) => {
            if let SpanKind::Object(members) = &node.kind {
                for member in members.iter().filter(|member| member.key == *name) {
                    collect_restores(&member.value, rest, arguments, edits);
                }
            }
        }
    }
}

struct SpanParser<'a> {
    text: &'a str,
    at: usize,
}

impl<'a> SpanParser<'a> {
    fn new(text: &'a str) -> Self {
        Self { text, at: 0 }
    }

    fn parse(&mut self) -> Option<SpanNode> {
        self.space();
        let node = self.node(0)?;
        self.space();
        (self.at == self.text.len()).then_some(node)
    }

    fn node(&mut self, depth: u32) -> Option<SpanNode> {
        if depth > 128 {
            return None;
        }
        self.space();
        let start = self.at;
        match self.byte()? {
            b'{' => self.object(start, depth),
            b'[' => self.array(start, depth),
            b'"' => {
                self.string()?;
                Some(SpanNode {
                    start,
                    end: self.at,
                    kind: SpanKind::String,
                })
            }
            b't' => {
                self.literal("true")?;
                Some(self.other(start))
            }
            b'f' => {
                self.literal("false")?;
                Some(self.other(start))
            }
            b'n' => {
                self.literal("null")?;
                Some(self.other(start))
            }
            b'-' | b'0'..=b'9' => self.number(start),
            _ => None,
        }
    }

    fn object(&mut self, start: usize, depth: u32) -> Option<SpanNode> {
        self.at += 1;
        self.space();
        let mut members = Vec::new();
        if self.take(b'}') {
            return Some(SpanNode {
                start,
                end: self.at,
                kind: SpanKind::Object(members),
            });
        }
        loop {
            self.space();
            let key = self.string()?;
            self.space();
            if !self.take(b':') {
                return None;
            }
            let value = self.node(depth + 1)?;
            members.push(SpanMember { key, value });
            self.space();
            if self.take(b'}') {
                break;
            }
            if !self.take(b',') {
                return None;
            }
        }
        Some(SpanNode {
            start,
            end: self.at,
            kind: SpanKind::Object(members),
        })
    }

    fn array(&mut self, start: usize, depth: u32) -> Option<SpanNode> {
        self.at += 1;
        self.space();
        let mut items = Vec::new();
        if self.take(b']') {
            return Some(SpanNode {
                start,
                end: self.at,
                kind: SpanKind::Array(items),
            });
        }
        loop {
            items.push(self.node(depth + 1)?);
            self.space();
            if self.take(b']') {
                break;
            }
            if !self.take(b',') {
                return None;
            }
        }
        Some(SpanNode {
            start,
            end: self.at,
            kind: SpanKind::Array(items),
        })
    }

    fn string(&mut self) -> Option<String> {
        let start = self.at;
        if !self.take(b'"') {
            return None;
        }
        while let Some(byte) = self.byte() {
            match byte {
                b'"' => {
                    self.at += 1;
                    return serde_json::from_str(&self.text[start..self.at]).ok();
                }
                b'\\' => self.at = self.at.checked_add(2)?,
                0..=0x1f => return None,
                _ => self.at += 1,
            }
        }
        None
    }

    fn number(&mut self, start: usize) -> Option<SpanNode> {
        while let Some(byte) = self.byte() {
            if matches!(byte, b' ' | b'\t' | b'\r' | b'\n' | b',' | b']' | b'}') {
                break;
            }
            self.at += 1;
        }
        serde_json::from_str::<serde_json::Number>(&self.text[start..self.at]).ok()?;
        Some(self.other(start))
    }

    fn literal(&mut self, literal: &str) -> Option<()> {
        if self.text.get(self.at..self.at + literal.len())? == literal {
            self.at += literal.len();
            Some(())
        } else {
            None
        }
    }

    fn other(&self, start: usize) -> SpanNode {
        SpanNode {
            start,
            end: self.at,
            kind: SpanKind::Other,
        }
    }

    fn space(&mut self) {
        while self
            .byte()
            .is_some_and(|byte| matches!(byte, b' ' | b'\t' | b'\r' | b'\n'))
        {
            self.at += 1;
        }
    }

    fn take(&mut self, byte: u8) -> bool {
        if self.byte() == Some(byte) {
            self.at += 1;
            true
        } else {
            false
        }
    }

    fn byte(&self) -> Option<u8> {
        self.text.as_bytes().get(self.at).copied()
    }
}

#[cfg(test)]
mod tests {
    use serde_json::json;

    use super::{FreeObjectEvents, FreeObjectPaths, FreeObjectSegment};
    use crate::{
        BlockStart, ContentBlockStart, ContentBlockStop, ContentPart, Extensions, Message,
        MessageDelta, MessageStop, Response, Role, Stop, StopReason, StreamEvent, Tool,
        ToolArguments, ToolArgumentsDelta, ToolCallPart,
    };

    fn paths() -> FreeObjectPaths {
        FreeObjectPaths::from_tools(&[Tool {
            name: "call".to_owned(),
            description: None,
            input_schema: json!({
                "type": "object",
                "properties": {
                    "free": {"type": "object"},
                    "list": {"type": "array", "items": {"type": "object"}}
                }
            }),
            extensions: Extensions::new(),
        }])
        .expect("free paths")
    }

    #[test]
    fn d249_schema_walk_selects_only_free_objects_below_the_root() {
        let tools = vec![Tool {
            name: "call".to_owned(),
            description: None,
            input_schema: json!({
                "type": "object",
                "properties": {
                    "free": {"type": "object"},
                    "empty": {"type": "object", "properties": {}},
                    "more": {"type": "object", "additionalProperties": true},
                    "none": {"type": "object", "additionalProperties": false},
                    "shaped": {"type": "object", "properties": {"nested": {"type": "object"}}},
                    "pattern": {"type": "object", "patternProperties": {"^x": {}}},
                    "union": {"type": "object", "anyOf": [{"type": "object"}]},
                    "list": {"type": "array", "items": {"type": "object"}},
                    "untyped": {"description": "not selected"}
                }
            }),
            extensions: Extensions::new(),
        }];
        let paths = FreeObjectPaths::from_tools(&tools).expect("paths");
        let mut selected = Vec::new();
        for path in paths.paths("call").expect("call paths") {
            selected.push(
                path.iter()
                    .map(|segment| match segment {
                        FreeObjectSegment::Property(name) => name.as_str(),
                        FreeObjectSegment::Items => "[]",
                    })
                    .collect::<Vec<_>>()
                    .join("."),
            );
        }
        assert_eq!(
            selected,
            ["free", "empty", "more", "shaped.nested", "list.[]"]
        );
        assert!(
            FreeObjectPaths::from_tools(&[Tool {
                name: "root".to_owned(),
                description: None,
                input_schema: json!({"type": "object"}),
                extensions: Extensions::new(),
            }])
            .is_none()
        );
    }

    #[test]
    fn d249_restore_text_replaces_only_object_strings_and_preserves_other_bytes() {
        let input = r#"{ "free": " {\"a\":1} ", "list": ["{\"b\":[2]}", "plain", {"c":3}, "{bad"], "other": "{\"x\":1}" }"#;
        let expected = r#"{ "free": {"a":1}, "list": [{"b":[2]}, "plain", {"c":3}, "{bad"], "other": "{\"x\":1}" }"#;
        assert_eq!(paths().restore_text("call", input), expected);
        assert_eq!(paths().restore_text("other", input), input);
        assert_eq!(paths().restore_text("call", "not json"), "not json");
    }

    #[test]
    fn d249_response_restores_only_named_tool_calls() {
        let arguments = r#"{"free":"{\"a\":1}","list":[]}"#;
        let mut response = Response {
            id: None,
            model: None,
            message: Message::new(Role::Assistant),
            stop: None,
            usage: None,
            extensions: Extensions::new(),
        };
        for name in ["call", "other"] {
            response
                .message
                .content
                .push(ContentPart::ToolCall(ToolCallPart {
                    id: name.to_owned(),
                    name: name.to_owned(),
                    arguments: ToolArguments::from_text(arguments),
                    extensions: Extensions::new(),
                }));
        }
        paths().restore_response(&mut response);
        let ContentPart::ToolCall(call) = &response.message.content[0] else {
            panic!("tool call");
        };
        assert_eq!(
            call.arguments.as_object().expect("object")["free"],
            json!({"a": 1})
        );
        let ContentPart::ToolCall(other) = &response.message.content[1] else {
            panic!("other tool call");
        };
        assert_eq!(other.arguments.to_text(), arguments);
    }

    #[test]
    fn d249_event_buffer_holds_pieces_until_stop_and_drains_at_eof() {
        let mut events = FreeObjectEvents::new(paths());
        let start = StreamEvent::ContentBlockStart(ContentBlockStart {
            index: 2,
            block: BlockStart::ToolCall {
                id: "id".to_owned(),
                name: "call".to_owned(),
            },
            extensions: Extensions::new(),
        });
        assert_eq!(events.push(start.clone()), vec![start.clone()]);
        for piece in [r#"{"free":"{\"a\""#, r#":1}"}"#] {
            assert!(
                events
                    .push(StreamEvent::ToolArgumentsDelta(ToolArgumentsDelta {
                        index: 2,
                        partial_json: piece.to_owned(),
                        extensions: Extensions::new(),
                    }))
                    .is_empty()
            );
        }
        let stop = StreamEvent::ContentBlockStop(ContentBlockStop {
            index: 2,
            extensions: Extensions::new(),
        });
        let out = events.push(stop.clone());
        assert_eq!(out.len(), 2);
        let StreamEvent::ToolArgumentsDelta(delta) = &out[0] else {
            panic!("arguments");
        };
        assert_eq!(delta.partial_json, r#"{"free":{"a":1}}"#);
        assert_eq!(out[1], stop);

        events.push(start);
        events.push(StreamEvent::ToolArgumentsDelta(ToolArgumentsDelta {
            index: 2,
            partial_json: r#"{"free":"{\"b\":2}"}"#.to_owned(),
            extensions: Extensions::new(),
        }));
        let terminal = StreamEvent::MessageDelta(MessageDelta {
            stop: Some(Stop {
                reason: StopReason::ToolUse,
                sequence: None,
            }),
            usage: None,
            extensions: Extensions::new(),
        });
        let out = events.push(terminal.clone());
        assert_eq!(out.len(), 2);
        assert_eq!(out[1], terminal);

        events.push(StreamEvent::ContentBlockStart(ContentBlockStart {
            index: 2,
            block: BlockStart::ToolCall {
                id: "id".to_owned(),
                name: "call".to_owned(),
            },
            extensions: Extensions::new(),
        }));
        events.push(StreamEvent::ToolArgumentsDelta(ToolArgumentsDelta {
            index: 2,
            partial_json: r#"{"free":"{\"c\":3}"}"#.to_owned(),
            extensions: Extensions::new(),
        }));
        let out = events.finish();
        assert_eq!(out.len(), 1);
        assert!(
            events.push(StreamEvent::MessageStop(MessageStop::default()))[0]
                == StreamEvent::MessageStop(MessageStop::default())
        );
    }
}
