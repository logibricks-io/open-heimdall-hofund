//! Hand-written protobuf codec for Cursor agent messages.
//!
//! Field numbers follow `docs/cursor-upstream-design.md`. The codec is
//! field-number based: unknown fields are skipped and counted, and a malformed
//! tail is dropped, not fatal. Varints decode as `u64`. Do not add `prost`.

use hofund_plugin_sdk::json::{Map, Number, Value};

/// Wire types the reference reader accepts. Any other type ends the walk.
const WIRE_VARINT: u32 = 0;
const WIRE_FIXED64: u32 = 1;
const WIRE_BYTES: u32 = 2;
const WIRE_FIXED32: u32 = 5;

/// A single parsed protobuf field.
#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) struct Field {
    /// Field number from the tag.
    pub number: u32,
    /// Decoded wire value.
    pub value: WireValue,
}

impl Field {
    /// Varint payload, when this field is wire type 0.
    #[must_use]
    pub(crate) fn as_varint(&self) -> Option<u64> {
        match self.value {
            WireValue::Varint(value) => Some(value),
            _ => None,
        }
    }

    /// Length-delimited payload, when this field is wire type 2.
    #[must_use]
    pub(crate) fn as_bytes(&self) -> Option<&[u8]> {
        match &self.value {
            WireValue::Bytes(bytes) => Some(bytes),
            _ => None,
        }
    }

    /// UTF-8 text of a length-delimited field. Invalid UTF-8 is lossy.
    #[must_use]
    pub(crate) fn as_string(&self) -> Option<String> {
        self.as_bytes()
            .map(|bytes| String::from_utf8_lossy(bytes).into_owned())
    }

    /// Parses a length-delimited field as a nested message (malformed tail dropped).
    #[must_use]
    pub(crate) fn as_message(&self) -> Option<Vec<Field>> {
        self.as_bytes().map(fields)
    }
}

/// A protobuf wire value. Packed repeated fields and groups are not invented:
/// wire type 2 is always raw bytes, and any type other than 0, 1, 2, and 5
/// stops the reader.
#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) enum WireValue {
    /// Wire type 0.
    Varint(u64),
    /// Wire type 1, little-endian 64-bit bits (also used for doubles).
    Fixed64(u64),
    /// Wire type 2.
    Bytes(Vec<u8>),
    /// Wire type 5, little-endian 32-bit bits.
    Fixed32(u32),
}

/// Walks `buf` as a protobuf message. An unknown wire type, a truncated length,
/// or a truncated fixed field ends the walk and keeps the fields already parsed.
#[must_use]
pub(crate) fn fields(buf: &[u8]) -> Vec<Field> {
    let mut out = Vec::new();
    let mut i = 0;
    while i < buf.len() {
        let Some((tag, next)) = read_varint(buf, i) else {
            break;
        };
        i = next;
        let number = u32::try_from(tag >> 3).unwrap_or(0);
        let wire = u32::try_from(tag & 7).unwrap_or(u32::MAX);
        let value = match wire {
            WIRE_VARINT => {
                let Some((value, next)) = read_varint(buf, i) else {
                    break;
                };
                i = next;
                WireValue::Varint(value)
            }
            WIRE_FIXED64 => {
                let Some(end) = i.checked_add(8) else {
                    break;
                };
                let Some(bytes) = buf.get(i..end) else {
                    break;
                };
                let bits = u64::from_le_bytes(bytes.try_into().unwrap_or([0; 8]));
                i = end;
                WireValue::Fixed64(bits)
            }
            WIRE_BYTES => {
                let Some((len, next)) = read_varint(buf, i) else {
                    break;
                };
                let Ok(len) = usize::try_from(len) else {
                    break;
                };
                i = next;
                let Some(end) = i.checked_add(len) else {
                    break;
                };
                let Some(bytes) = buf.get(i..end) else {
                    break;
                };
                let value = WireValue::Bytes(bytes.to_vec());
                i = end;
                value
            }
            WIRE_FIXED32 => {
                let Some(end) = i.checked_add(4) else {
                    break;
                };
                let Some(bytes) = buf.get(i..end) else {
                    break;
                };
                let bits = u32::from_le_bytes(bytes.try_into().unwrap_or([0; 4]));
                i = end;
                WireValue::Fixed32(bits)
            }
            _ => break,
        };
        out.push(Field { number, value });
    }
    out
}

/// Counts fields whose number is not in `known`, including nested walks the
/// caller already flattened. Used for empty-reply diagnostics.
#[must_use]
pub(crate) fn unknown_count(parsed: &[Field], known: &[u32]) -> usize {
    parsed
        .iter()
        .filter(|field| !known.contains(&field.number))
        .count()
}

/// Consuming protobuf writer, matching the reference field-number builder.
#[derive(Clone, Debug, Default)]
pub(crate) struct Pb {
    buf: Vec<u8>,
}

#[allow(dead_code)]
impl Pb {
    /// Empty message.
    #[must_use]
    pub(crate) fn new() -> Self {
        Self::default()
    }

    /// Field with wire type 0.
    #[must_use]
    pub(crate) fn varint(mut self, field: u32, value: u64) -> Self {
        self.write_tag(field, WIRE_VARINT);
        write_varint(&mut self.buf, value);
        self
    }

    /// Field with wire type 2.
    #[must_use]
    pub(crate) fn bytes(mut self, field: u32, value: &[u8]) -> Self {
        self.write_tag(field, WIRE_BYTES);
        write_varint(&mut self.buf, value.len() as u64);
        self.buf.extend_from_slice(value);
        self
    }

    /// UTF-8 string as a length-delimited field.
    #[must_use]
    pub(crate) fn string(self, field: u32, value: &str) -> Self {
        self.bytes(field, value.as_bytes())
    }

    /// Nested message as a length-delimited field.
    #[must_use]
    pub(crate) fn message(self, field: u32, inner: Pb) -> Self {
        self.bytes(field, &inner.finish())
    }

    /// Field with wire type 1 (fixed64 / double bits).
    #[must_use]
    pub(crate) fn fixed64(mut self, field: u32, bits: u64) -> Self {
        self.write_tag(field, WIRE_FIXED64);
        self.buf.extend_from_slice(&bits.to_le_bytes());
        self
    }

    /// Field with wire type 5 (fixed32).
    #[must_use]
    pub(crate) fn fixed32(mut self, field: u32, bits: u32) -> Self {
        self.write_tag(field, WIRE_FIXED32);
        self.buf.extend_from_slice(&bits.to_le_bytes());
        self
    }

    /// Writes `value` with the same wire type it was decoded as.
    #[must_use]
    pub(crate) fn wire(self, field: u32, value: &WireValue) -> Self {
        match value {
            WireValue::Varint(v) => self.varint(field, *v),
            WireValue::Fixed64(bits) => self.fixed64(field, *bits),
            WireValue::Bytes(bytes) => self.bytes(field, bytes),
            WireValue::Fixed32(bits) => self.fixed32(field, *bits),
        }
    }

    /// Finished message bytes.
    #[must_use]
    pub(crate) fn finish(self) -> Vec<u8> {
        self.buf
    }

    fn write_tag(&mut self, field: u32, wire: u32) {
        write_varint(&mut self.buf, (u64::from(field) << 3) | u64::from(wire));
    }
}

/// Encodes a `google.protobuf.Value`. Struct keys are sorted.
#[must_use]
#[allow(dead_code)]
pub(crate) fn pb_value(value: &Value) -> Vec<u8> {
    match value {
        Value::Null => Pb::new().varint(1, 0).finish(),
        Value::Number(number) => {
            let bits = number.as_f64().unwrap_or(0.0).to_bits();
            Pb::new().fixed64(2, bits).finish()
        }
        Value::String(text) => Pb::new().string(3, text).finish(),
        Value::Bool(flag) => Pb::new().varint(4, u64::from(*flag)).finish(),
        Value::Object(map) => {
            let mut keys: Vec<&String> = map.keys().collect();
            keys.sort_unstable();
            let mut entries = Pb::new();
            for key in keys {
                let entry = Pb::new()
                    .string(1, key)
                    .bytes(2, &pb_value(&map[key]))
                    .finish();
                entries = entries.bytes(1, &entry);
            }
            Pb::new().bytes(5, &entries.finish()).finish()
        }
        Value::Array(items) => {
            let mut list = Pb::new();
            for item in items {
                list = list.bytes(1, &pb_value(item));
            }
            Pb::new().bytes(6, &list.finish()).finish()
        }
    }
}

const MAX_DECODE_DEPTH: u32 = 64;

/// Decodes a `google.protobuf.Value`. Unknown or empty input becomes `Null`.
#[must_use]
#[allow(dead_code)]
pub(crate) fn pb_value_decode(buf: &[u8]) -> Value {
    pb_value_decode_at_depth(buf, 0)
}

fn pb_value_decode_at_depth(buf: &[u8], depth: u32) -> Value {
    if depth >= MAX_DECODE_DEPTH {
        return Value::Null;
    }
    let parsed = fields(buf);
    for field in parsed {
        match field.number {
            1 => return Value::Null,
            2 => {
                if let WireValue::Fixed64(bits) = field.value
                    && let Some(number) = Number::from_f64(f64::from_bits(bits))
                {
                    return Value::Number(number);
                }
            }
            3 => {
                if let Some(text) = field.as_string() {
                    return Value::String(text);
                }
            }
            4 => {
                if let Some(value) = field.as_varint() {
                    return Value::Bool(value != 0);
                }
            }
            5 => {
                if let Some(inner) = field.as_bytes() {
                    return Value::Object(decode_struct(inner, depth));
                }
            }
            6 => {
                if let Some(inner) = field.as_bytes() {
                    let items = fields(inner)
                        .into_iter()
                        .filter(|entry| entry.number == 1)
                        .filter_map(|entry| {
                            entry
                                .as_bytes()
                                .map(|inner| pb_value_decode_at_depth(inner, depth + 1))
                        })
                        .collect();
                    return Value::Array(items);
                }
            }
            _ => {}
        }
    }
    Value::Null
}

fn decode_struct(buf: &[u8], depth: u32) -> Map<String, Value> {
    let mut map = Map::new();
    for entry in fields(buf).into_iter().filter(|field| field.number == 1) {
        let Some(inner) = entry.as_message() else {
            continue;
        };
        let mut key = None;
        let mut value = None;
        for field in inner {
            match field.number {
                1 => key = field.as_string(),
                2 => {
                    value = field
                        .as_bytes()
                        .map(|inner| pb_value_decode_at_depth(inner, depth + 1));
                }
                _ => {}
            }
        }
        if let (Some(key), Some(value)) = (key, value) {
            map.insert(key, value);
        }
    }
    map
}

/// Maps Rust `std::env::consts::OS` onto the Node `platform()` names the wire
/// uses: `macos` → `darwin`, `windows` unchanged, otherwise pass through when
/// the name already matches a Node platform, else `linux`.
#[must_use]
#[allow(dead_code)]
pub(crate) fn goos_name_for(os: &str) -> &str {
    match os {
        "macos" => "darwin",
        "windows" => "windows",
        "aix" | "android" | "cygwin" | "darwin" | "freebsd" | "haiku" | "linux" | "netbsd"
        | "openbsd" | "sunos" => os,
        _ => "linux",
    }
}

/// Client heartbeat payload: field 7, length-delimited, empty (`0x3a 0x00`).
#[must_use]
pub(crate) fn heartbeat() -> Vec<u8> {
    Pb::new().bytes(7, &[]).finish()
}

/// Wraps an `AgentRunRequest` as client field 1.
#[must_use]
#[allow(dead_code)]
pub(crate) fn client_run(request: &[u8]) -> Vec<u8> {
    Pb::new().bytes(1, request).finish()
}

/// One MCP tool definition in the request context (namespace is always `tools`).
#[derive(Clone, Debug)]
#[allow(dead_code)]
pub(crate) struct ToolDef {
    /// Tool name.
    pub name: String,
    /// Tool description.
    pub description: String,
    /// JSON schema encoded as a protobuf Value (keys sorted) and as text.
    pub schema: Value,
}

/// Inputs for [`encode_agent_run_request`].
#[derive(Clone, Debug)]
#[allow(dead_code)]
pub(crate) struct AgentRunParams<'a> {
    /// Conversation id (field 5).
    pub conversation_id: &'a str,
    /// Model id, written on the model-details and requested-model messages.
    pub model_id: &'a str,
    /// SHA-256 ids of transcript blobs (conversation state field 1, repeated).
    pub transcript_blob_ids: &'a [[u8; 32]],
    /// User-turn blob id (conversation state field 8).
    pub user_turn_blob_id: &'a [u8; 32],
    /// OS name before [`goos_name_for`] mapping.
    pub os: &'a str,
    /// Temp directory sent as environment field 2.
    pub temp_dir: &'a str,
    /// MCP tool definitions (context field 7 and wrapper field 1).
    pub tools: &'a [ToolDef],
    /// When true, set field 19 to 1 (inline images). R-8 leaves this false.
    pub inline_images: bool,
}

/// Builds an `AgentRunRequest`. R-8 does not set Max Mode or effort parameters.
///
/// Field 4 (the MCP wrapper) is always present; when `tools` is empty it is an
/// empty message, not omitted. Field 19 is written only when `inline_images`.
#[must_use]
#[allow(dead_code)]
pub(crate) fn encode_agent_run_request(params: &AgentRunParams<'_>) -> Vec<u8> {
    let mut state = Pb::new();
    for id in params.transcript_blob_ids {
        state = state.bytes(1, id);
    }
    let state = state
        .bytes(8, params.user_turn_blob_id)
        .varint(10, 1)
        .string(22, "cli");

    let mut context = Pb::new().bytes(4, &environment(params.os, params.temp_dir));
    for tool in params.tools {
        context = context.bytes(7, &tool_def_bytes(tool));
    }
    let action = Pb::new().message(2, Pb::new().bytes(2, &context.finish()));
    let model = Pb::new()
        .string(1, params.model_id)
        .string(3, params.model_id)
        .string(4, params.model_id);
    let mut wrapper = Pb::new();
    for tool in params.tools {
        wrapper = wrapper.bytes(1, &tool_def_bytes(tool));
    }
    let mut request = Pb::new()
        .message(1, state)
        .message(2, action)
        .message(3, model)
        .message(4, wrapper)
        .string(5, params.conversation_id)
        .message(9, Pb::new().string(1, params.model_id));
    if params.inline_images {
        request = request.varint(19, 1);
    }
    request.finish()
}

/// Environment message: OS, temp directory, and `"UTC"`.
#[must_use]
#[allow(dead_code)]
pub(crate) fn environment(os: &str, temp_dir: &str) -> Vec<u8> {
    Pb::new()
        .string(1, goos_name_for(os))
        .string(2, temp_dir)
        .string(10, "UTC")
        .finish()
}

#[allow(dead_code)]
fn tool_def_bytes(tool: &ToolDef) -> Vec<u8> {
    let schema_text = tool.schema.to_string();
    Pb::new()
        .string(1, &tool.name)
        .string(2, &tool.description)
        .bytes(3, &pb_value(&tool.schema))
        .string(4, "tools")
        .string(5, &tool.name)
        .string(6, &schema_text)
        .finish()
}

/// Client field 3: blob found (inner field 1 = KV id, field 2 = result field 1).
#[must_use]
#[allow(dead_code)]
pub(crate) fn kv_get_found(id: &WireValue, blob: &[u8]) -> Vec<u8> {
    let result = Pb::new().bytes(1, blob).finish();
    Pb::new()
        .message(3, Pb::new().wire(1, id).bytes(2, &result))
        .finish()
}

/// Client field 3: blob missing (`"blob not found"`).
#[must_use]
#[allow(dead_code)]
pub(crate) fn kv_get_missing(id: &WireValue) -> Vec<u8> {
    let missing = Pb::new().string(1, "blob not found").finish();
    let result = Pb::new().bytes(2, &missing).finish();
    Pb::new()
        .message(3, Pb::new().wire(1, id).bytes(2, &result))
        .finish()
}

/// Client field 3: store ack (inner field 3 empty).
#[must_use]
#[allow(dead_code)]
pub(crate) fn kv_store_ack(id: &WireValue) -> Vec<u8> {
    Pb::new()
        .message(3, Pb::new().wire(1, id).bytes(3, &[]))
        .finish()
}

/// Client field 5, inner field 1: close an exec.
#[must_use]
#[allow(dead_code)]
pub(crate) fn close_exec(id: u32) -> Vec<u8> {
    Pb::new()
        .message(5, Pb::new().message(1, Pb::new().varint(1, u64::from(id))))
        .finish()
}

/// Client field 5, inner field 2: refuse an exec with `"not available"`.
#[must_use]
#[allow(dead_code)]
pub(crate) fn refuse_exec(id: u32) -> Vec<u8> {
    Pb::new()
        .message(
            5,
            Pb::new().message(
                2,
                Pb::new()
                    .varint(1, u64::from(id))
                    .string(2, "not available"),
            ),
        )
        .finish()
}

/// Client field 2: exec result for field 10 (environment), to be followed by
/// [`close_exec`].
#[must_use]
#[allow(dead_code)]
pub(crate) fn exec_env_result(id: u32, exec_id: &str, os: &str, temp_dir: &str) -> Vec<u8> {
    let inner = Pb::new().bytes(1, &Pb::new().bytes(4, &environment(os, temp_dir)).finish());
    exec_result(id, exec_id, 10, &Pb::new().message(1, inner).finish())
}

/// Client field 2: exec result for field 36 (connected `tools` server).
///
/// The inner message is never a bare empty message, even when no tool defs are
/// repeated on field 5.
#[must_use]
#[allow(dead_code)]
pub(crate) fn exec_tools_result(id: u32, exec_id: &str, tools: &[ToolDef]) -> Vec<u8> {
    let mut server = Pb::new()
        .string(1, "tools")
        .string(2, "tools")
        .string(7, "connected");
    for tool in tools {
        server = server.bytes(5, &tool_def_bytes(tool));
    }
    let wrapped = Pb::new()
        .bytes(1, &Pb::new().bytes(1, &server.finish()).finish())
        .finish();
    exec_result(id, exec_id, 36, &wrapped)
}

#[allow(dead_code)]
fn exec_result(id: u32, exec_id: &str, result_field: u32, result: &[u8]) -> Vec<u8> {
    Pb::new()
        .message(
            2,
            Pb::new()
                .varint(1, u64::from(id))
                .string(15, exec_id)
                .bytes(result_field, result),
        )
        .finish()
}

/// `BidiAppend` body: field 1 is the payload hex, field 2 wraps the request id,
/// field 3 is the sequence number.
#[must_use]
pub(crate) fn bidi_append(payload: &[u8], request_id: &str, sequence: u64) -> Vec<u8> {
    Pb::new()
        .string(1, &to_hex(payload))
        .message(2, Pb::new().string(1, request_id))
        .varint(3, sequence)
        .finish()
}

/// Lowercase hex encoding. No extra dependency.
#[must_use]
pub(crate) fn to_hex(bytes: &[u8]) -> String {
    const HEX: &[u8; 16] = b"0123456789abcdef";
    let mut out = String::with_capacity(bytes.len().saturating_mul(2));
    for byte in bytes {
        out.push(HEX[usize::from(byte >> 4)] as char);
        out.push(HEX[usize::from(byte & 0x0f)] as char);
    }
    out
}

/// Resolves an MCP exec tool name: field 5 when non-empty, otherwise strip a
/// leading `tools-` prefix from field 1, otherwise leave field 1 unchanged.
#[must_use]
pub(crate) fn mcp_tool_name(field1: &str, field5: &str) -> String {
    let field5 = field5.trim();
    if !field5.is_empty() {
        return field5.to_owned();
    }
    field1.strip_prefix("tools-").unwrap_or(field1).to_owned()
}

/// A decoded server payload (one protobuf message, possibly several events).
#[derive(Clone, Debug, PartialEq)]
pub(crate) enum ServerEvent {
    /// Interaction field 1, text delta (inner field 1 string).
    TextDelta(String),
    /// Interaction field 4. R-8 ignores the bytes when emitting, but still parses.
    ThinkingDelta(String),
    /// Interaction field 5.
    ThinkingCompleted,
    /// Interaction field 2, with the call id from inner field 1.
    ToolStarted(String),
    /// Interaction field 3, with the call id from inner field 1.
    ToolCompleted(String),
    /// Interaction field 7, with the call id from inner field 1.
    PartialTool(String),
    /// Interaction field 8.
    TokenDelta,
    /// Heartbeat: every top-level field is 1 and every inner field is 13.
    Heartbeat,
    /// Interaction field 14 (turn end), with usage counters.
    TurnEnd(TurnUsage),
    /// Interaction field 27, inner field 1.
    ToolCallCount(u64),
    /// Top-level field 2.
    Exec(ExecRequest),
    /// Top-level field 3, with the number of pending calls in state field 4.
    Checkpoint(usize),
    /// Top-level field 4.
    Kv(KvRequest),
    /// A top-level field the codec does not classify.
    Unknown(u32),
}

/// Turn-end usage (field 14): 1 input, 2 output, 3 cache read, 4 cache write, 5 reasoning.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub(crate) struct TurnUsage {
    /// Input tokens.
    pub input: u64,
    /// Output tokens.
    pub output: u64,
    /// Cache-read tokens.
    pub cache_read: u64,
    /// Cache-write tokens.
    pub cache_write: u64,
    /// Reasoning tokens.
    pub reasoning: u64,
}

/// Server exec (top-level field 2).
#[derive(Clone, Debug, PartialEq)]
pub(crate) struct ExecRequest {
    /// Field 1, uint32. Omitted means 0.
    pub id: u32,
    /// Field 15, copied into the result.
    pub exec_id: String,
    /// Discriminated by the remaining field numbers.
    pub kind: ExecKind,
}

/// Body of a server exec.
#[derive(Clone, Debug, PartialEq)]
pub(crate) enum ExecKind {
    /// Field 10: request context. Reply with the environment, then close.
    Environment,
    /// Field 11: MCP arguments. Selected caller tools close; all others refuse.
    McpArguments {
        /// Resolved tool name.
        name: String,
        /// Field 3 call id.
        call_id: String,
        /// Argument entries (field 2) as a JSON object.
        arguments: Value,
    },
    /// Field 36: MCP tool list. Reply with the connected-server message, then close.
    ListTools,
    /// Shell, file, and every other Cursor-owned tool. Refuse, then close.
    Other,
}

/// Server KV request (top-level field 4).
#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) struct KvRequest {
    /// Field 1, echoed in the client reply.
    pub id: WireValue,
    /// Get-blob or store-ack.
    pub kind: KvKind,
}

/// KV operation.
#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) enum KvKind {
    /// Field 2: inner field 1 is the raw digest.
    Get {
        /// Blob digest (raw 32 bytes when the server sends a SHA-256).
        digest: Vec<u8>,
    },
    /// Field 3: acknowledge a stored blob.
    Store,
}

/// Decodes one server payload into events, in field order. Unknown inner
/// interaction fields are skipped and do not produce an event.
#[must_use]
pub(crate) fn decode_server(buf: &[u8]) -> (Vec<ServerEvent>, usize) {
    let parsed = fields(buf);
    let unknown = unknown_count(&parsed, &[1, 2, 3, 4]);
    if is_heartbeat(&parsed) {
        return (vec![ServerEvent::Heartbeat], unknown);
    }
    let mut events = Vec::new();
    for field in parsed {
        match field.number {
            1 => events.extend(decode_interaction(field.as_message().unwrap_or_default())),
            2 => events.push(ServerEvent::Exec(decode_exec(
                field.as_message().unwrap_or_default(),
            ))),
            3 => events.push(ServerEvent::Checkpoint(
                field
                    .as_message()
                    .unwrap_or_default()
                    .iter()
                    .filter(|inner| inner.number == 4)
                    .count(),
            )),
            4 => {
                if let Some(kv) = decode_kv(field.as_message().unwrap_or_default()) {
                    events.push(ServerEvent::Kv(kv));
                }
            }
            other => events.push(ServerEvent::Unknown(other)),
        }
    }
    (events, unknown)
}

fn is_heartbeat(parsed: &[Field]) -> bool {
    if parsed.is_empty() {
        return false;
    }
    parsed.iter().all(|field| {
        field.number == 1
            && field.as_message().is_some_and(|inner| {
                !inner.is_empty() && inner.iter().all(|inner| inner.number == 13)
            })
    })
}

fn decode_interaction(inner: Vec<Field>) -> Vec<ServerEvent> {
    let mut events = Vec::new();
    for field in inner {
        match field.number {
            1 => events.push(ServerEvent::TextDelta(interaction_text(&field))),
            2 => events.push(ServerEvent::ToolStarted(interaction_text(&field))),
            3 => events.push(ServerEvent::ToolCompleted(interaction_text(&field))),
            4 => events.push(ServerEvent::ThinkingDelta(interaction_text(&field))),
            5 => events.push(ServerEvent::ThinkingCompleted),
            7 => events.push(ServerEvent::PartialTool(interaction_text(&field))),
            8 => events.push(ServerEvent::TokenDelta),
            13 => {
                // A lone inner 13 without the heartbeat rule is still not a
                // text event; the heartbeat path is handled at the top level.
            }
            14 => events.push(ServerEvent::TurnEnd(decode_usage(
                field.as_message().unwrap_or_default(),
            ))),
            27 => {
                let count = field
                    .as_message()
                    .into_iter()
                    .flatten()
                    .find(|inner| inner.number == 1)
                    .and_then(|inner| inner.as_varint())
                    .unwrap_or(0);
                events.push(ServerEvent::ToolCallCount(count));
            }
            _ => {}
        }
    }
    events
}

fn interaction_text(field: &Field) -> String {
    field
        .as_message()
        .into_iter()
        .flatten()
        .find(|inner| inner.number == 1)
        .and_then(|inner| inner.as_string())
        .unwrap_or_default()
}

fn decode_usage(inner: Vec<Field>) -> TurnUsage {
    let mut usage = TurnUsage::default();
    for field in inner {
        let Some(value) = field.as_varint() else {
            continue;
        };
        match field.number {
            1 => usage.input = value,
            2 => usage.output = value,
            3 => usage.cache_read = value,
            4 => usage.cache_write = value,
            5 => usage.reasoning = value,
            _ => {}
        }
    }
    usage
}

fn decode_exec(inner: Vec<Field>) -> ExecRequest {
    let mut id = 0_u32;
    let mut exec_id = String::new();
    let mut kind = ExecKind::Other;
    for field in inner {
        match field.number {
            1 => {
                if let Some(value) = field.as_varint() {
                    id = u32::try_from(value).unwrap_or(0);
                }
            }
            10 => kind = ExecKind::Environment,
            11 => kind = decode_mcp_arguments(field.as_message().unwrap_or_default()),
            15 => {
                if let Some(value) = field.as_string() {
                    exec_id = value;
                }
            }
            36 => kind = ExecKind::ListTools,
            _ => {}
        }
    }
    ExecRequest { id, exec_id, kind }
}

fn decode_mcp_arguments(inner: Vec<Field>) -> ExecKind {
    let mut field1 = String::new();
    let mut field5 = String::new();
    let mut call_id = String::new();
    let mut arguments = Map::new();
    for field in inner {
        match field.number {
            1 => field1 = field.as_string().unwrap_or_default(),
            2 => {
                if let Some(entries) = field.as_bytes() {
                    // Field 2 may itself be repeated entries. If this field is a
                    // single entry, parse it; if it wraps entries, walk them.
                    decode_argument_entries(entries, &mut arguments, 0);
                }
            }
            3 => call_id = field.as_string().unwrap_or_default(),
            5 => field5 = field.as_string().unwrap_or_default(),
            _ => {}
        }
    }
    ExecKind::McpArguments {
        name: mcp_tool_name(&field1, &field5),
        call_id,
        arguments: Value::Object(arguments),
    }
}

fn decode_argument_entries(buf: &[u8], arguments: &mut Map<String, Value>, depth: u32) {
    if depth >= MAX_DECODE_DEPTH {
        return;
    }
    let parsed = fields(buf);
    // Two shapes: this buffer is one entry (fields 1 and 2), or a list of
    // length-delimited entries (unknown field numbers). The reference writes
    // "field 2 is argument entries (field 1 key, field 2 a protobuf Value)".
    // Treat a buffer that already has field 1 as a string key as one entry.
    let has_direct_key = parsed
        .iter()
        .any(|field| field.number == 1 && field.as_bytes().is_some());
    if has_direct_key {
        let mut key = None;
        let mut value = None;
        for field in parsed {
            match field.number {
                1 => key = field.as_string(),
                2 => {
                    value = field
                        .as_bytes()
                        .map(|inner| pb_value_decode_at_depth(inner, depth + 1));
                }
                _ => {}
            }
        }
        if let (Some(key), Some(value)) = (key, value) {
            arguments.insert(key, value);
        }
        return;
    }
    for field in parsed {
        if let Some(bytes) = field.as_bytes() {
            decode_argument_entries(bytes, arguments, depth + 1);
        }
    }
}

fn decode_kv(inner: Vec<Field>) -> Option<KvRequest> {
    let mut id = None;
    let mut kind = None;
    for field in inner {
        match field.number {
            1 => id = Some(field.value.clone()),
            2 => {
                let digest = field
                    .as_message()
                    .into_iter()
                    .flatten()
                    .find(|inner| inner.number == 1)
                    .and_then(|inner| inner.as_bytes().map(<[u8]>::to_vec))
                    .unwrap_or_default();
                kind = Some(KvKind::Get { digest });
            }
            3 => kind = Some(KvKind::Store),
            _ => {}
        }
    }
    Some(KvRequest {
        id: id.unwrap_or(WireValue::Bytes(Vec::new())),
        kind: kind?,
    })
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

fn read_varint(buf: &[u8], mut i: usize) -> Option<(u64, usize)> {
    let mut value = 0_u64;
    for shift in 0..10 {
        let byte = *buf.get(i)?;
        i += 1;
        let bits = u64::from(byte & 0x7f);
        if shift == 9 && bits > 1 {
            return None;
        }
        value |= bits << (shift * 7);
        if byte & 0x80 == 0 {
            return Some((value, i));
        }
    }
    None
}

#[cfg(test)]
mod tests {
    use super::{
        AgentRunParams, ExecKind, Field, KvKind, MAX_DECODE_DEPTH, Pb, ServerEvent, ToolDef,
        TurnUsage, WireValue, bidi_append, client_run, close_exec, decode_server,
        encode_agent_run_request, environment, exec_env_result, exec_tools_result, fields,
        goos_name_for, heartbeat, kv_get_found, kv_get_missing, kv_store_ack, mcp_tool_name,
        pb_value, pb_value_decode, refuse_exec, to_hex, unknown_count,
    };
    use hofund_plugin_sdk::json::{Value, json};

    #[test]
    fn varint_above_2_pow_53_round_trips_as_u64() {
        let value = (1_u64 << 53) + 1;
        let buf = Pb::new().varint(1, value).finish();
        let parsed = fields(&buf);
        assert_eq!(parsed.len(), 1);
        assert_eq!(parsed[0].as_varint(), Some(value));
        assert_ne!(parsed[0].as_varint(), Some(1_u64 << 53));
    }

    #[test]
    fn fields_drop_a_malformed_tail_and_keep_what_was_parsed() {
        let mut buf = Pb::new().varint(1, 7).string(2, "ok").finish();
        // Unknown wire type 3 (group start) after a valid prefix must stop the walk.
        buf.push((3 << 3) | 3);
        buf.push(0xff);
        let parsed = fields(&buf);
        assert_eq!(parsed.len(), 2);
        assert_eq!(parsed[0].as_varint(), Some(7));
        assert_eq!(parsed[1].as_string().as_deref(), Some("ok"));

        // Truncated length-delimited field: tag + length 10, only one data byte.
        let mut truncated = Pb::new().varint(4, 1).finish();
        truncated.push((5 << 3) | 2);
        truncated.push(10);
        truncated.push(0x00);
        let parsed = fields(&truncated);
        assert_eq!(parsed.len(), 1);
        assert_eq!(parsed[0].number, 4);
    }

    #[test]
    fn fields_length_overflow_keeps_the_prefix() {
        let mut buf = Pb::new().varint(1, 7).finish();
        buf.push((2 << 3) | 2);
        buf.extend_from_slice(&[0xff; 9]);
        buf.push(0x01);
        let parsed = fields(&buf);
        assert_eq!(parsed.len(), 1);
        assert_eq!(parsed[0].as_varint(), Some(7));
    }

    #[test]
    fn fields_bad_varints_keep_the_prefix() {
        let mut unterminated = Pb::new().varint(1, 7).finish();
        unterminated.extend_from_slice(&[(2 << 3), 0x80]);
        let parsed = fields(&unterminated);
        assert_eq!(parsed.len(), 1);
        assert_eq!(parsed[0].as_varint(), Some(7));

        let mut overflow = Pb::new().varint(1, 7).finish();
        overflow.push(2 << 3);
        overflow.extend_from_slice(&[0x80; 9]);
        overflow.push(0x02);
        let parsed = fields(&overflow);
        assert_eq!(parsed.len(), 1);
        assert_eq!(parsed[0].as_varint(), Some(7));
    }

    #[test]
    fn fields_truncated_fixed_values_keep_the_prefix() {
        let mut fixed64 = Pb::new().varint(1, 7).finish();
        fixed64.push((2 << 3) | 1);
        fixed64.extend_from_slice(&[1, 2, 3]);
        let parsed = fields(&fixed64);
        assert_eq!(parsed.len(), 1);
        assert_eq!(parsed[0].as_varint(), Some(7));

        let mut fixed32 = Pb::new().varint(1, 7).finish();
        fixed32.push((2 << 3) | 5);
        fixed32.extend_from_slice(&[1, 2, 3]);
        let parsed = fields(&fixed32);
        assert_eq!(parsed.len(), 1);
        assert_eq!(parsed[0].as_varint(), Some(7));
    }

    #[test]
    fn fuzz_sized_truncated_buffer_returns_complete_prefix_fields() {
        let full = Pb::new()
            .varint(1, 42)
            .string(2, "prefix")
            .fixed32(3, 7)
            .fixed64(4, 9)
            .finish();
        let mut kept = 0;
        for end in 0..=full.len() {
            let parsed = fields(&full[..end]);
            for field in &parsed {
                assert!(field.number >= 1 && field.number <= 4);
            }
            if end == full.len() {
                assert_eq!(parsed.len(), 4);
            }
            assert!(parsed.len() >= kept);
            kept = parsed.len();
        }
    }

    #[test]
    fn pb_value_sorts_struct_keys() {
        let value = json!({"zeta": 1.5, "alpha": true, "mu": "x"});
        let encoded = pb_value(&value);
        let decoded = pb_value_decode(&encoded);
        assert_eq!(decoded, value);
        // Keys on the wire are alpha, mu, zeta in that order.
        let wrapper = fields(&encoded);
        assert_eq!(wrapper[0].number, 5);
        let entries = fields(wrapper[0].as_bytes().expect("struct"));
        let keys: Vec<String> = entries
            .iter()
            .filter_map(|entry| {
                entry
                    .as_message()
                    .into_iter()
                    .flatten()
                    .find(|field| field.number == 1)
                    .and_then(|field| field.as_string())
            })
            .collect();
        assert_eq!(keys, ["alpha", "mu", "zeta"]);
    }

    #[test]
    fn pb_value_decode_bounds_nesting_and_round_trips_shallow_values() {
        let mut shallow = json!("leaf");
        for _ in 0..8 {
            shallow = json!({"nested": shallow});
        }
        assert_eq!(pb_value_decode(&pb_value(&shallow)), shallow);

        let mut deep = json!("leaf");
        for _ in 0..MAX_DECODE_DEPTH + 8 {
            deep = json!({"nested": deep});
        }
        let decoded = pb_value_decode(&pb_value(&deep));
        let mut innermost = &decoded;
        for _ in 0..MAX_DECODE_DEPTH - 1 {
            let Value::Object(map) = innermost else {
                panic!("expected object before decode depth limit");
            };
            innermost = &map["nested"];
        }
        assert_eq!(innermost, &json!({"nested": null}));
    }

    #[test]
    fn heartbeat_tags_as_0x3a_empty() {
        assert_eq!(heartbeat(), [0x3a, 0x00]);
    }

    #[test]
    fn agent_run_request_nests_action_as_field_2_of_field_2() {
        let blob = [0x11_u8; 32];
        let user = [0x22_u8; 32];
        let encoded = encode_agent_run_request(&AgentRunParams {
            conversation_id: "conv-1",
            model_id: "composer-2",
            transcript_blob_ids: &[blob],
            user_turn_blob_id: &user,
            os: "macos",
            temp_dir: "/tmp/r8e",
            tools: &[],
            inline_images: false,
        });
        let top = fields(&encoded);
        let action = top
            .iter()
            .find(|field| field.number == 2)
            .and_then(Field::as_message)
            .expect("action");
        assert_eq!(action.len(), 1);
        assert_eq!(action[0].number, 2);
        let inner = action[0].as_message().expect("inner field 2");
        assert_eq!(inner.len(), 1);
        assert_eq!(inner[0].number, 2);
        let context = inner[0].as_message().expect("request context");
        let env_field = context
            .iter()
            .find(|field| field.number == 4)
            .and_then(Field::as_message)
            .expect("environment");
        let os = env_field
            .iter()
            .find(|field| field.number == 1)
            .and_then(Field::as_string)
            .expect("os");
        assert_eq!(os, "darwin");
        let tz = env_field
            .iter()
            .find(|field| field.number == 10)
            .and_then(Field::as_string)
            .expect("tz");
        assert_eq!(tz, "UTC");

        let outbound = client_run(&encoded);
        let lower = String::from_utf8_lossy(&outbound).to_ascii_lowercase();
        assert!(
            !lower.contains("hofund"),
            "outbound run bytes must not contain the product name"
        );
        let mut reference_name = String::from("mag");
        reference_name.push_str("pie");
        assert!(
            !lower.contains(&reference_name),
            "outbound run bytes must not contain the reference product name"
        );
    }

    #[test]
    fn goos_name_maps_macos_and_unknowns() {
        assert_eq!(goos_name_for("macos"), "darwin");
        assert_eq!(goos_name_for("windows"), "windows");
        assert_eq!(goos_name_for("linux"), "linux");
        assert_eq!(goos_name_for("freebsd"), "freebsd");
        assert_eq!(goos_name_for("solaris"), "linux");
        assert_eq!(goos_name_for("ios"), "linux");
    }

    #[test]
    fn kv_get_and_store_goldens() {
        let id = WireValue::Bytes(b"kv-1".to_vec());
        let found = kv_get_found(&id, b"blob-bytes");
        assert_eq!(found[0], 0x1a, "client field 3 tags as 0x1a");
        let (events, _) = decode_server(&server_kv_get(b"kv-1", b"\xab\xcd"));
        match &events[..] {
            [ServerEvent::Kv(req)] => match &req.kind {
                KvKind::Get { digest } => assert_eq!(digest, b"\xab\xcd"),
                other => panic!("expected get, got {other:?}"),
            },
            other => panic!("expected one kv event, got {other:?}"),
        }
        let reply = kv_get_found(&id, b"blob-bytes");
        let top = fields(&reply);
        let inner = top[0].as_message().expect("field 3");
        assert_eq!(inner[0].as_bytes(), Some(&b"kv-1"[..]));
        let result = inner
            .iter()
            .find(|field| field.number == 2)
            .and_then(Field::as_message)
            .expect("result");
        assert_eq!(result[0].number, 1);
        assert_eq!(result[0].as_bytes(), Some(&b"blob-bytes"[..]));

        let missing = kv_get_missing(&id);
        let inner = fields(&missing)[0].as_message().expect("field 3");
        let result = inner
            .iter()
            .find(|field| field.number == 2)
            .and_then(Field::as_message)
            .expect("result");
        assert_eq!(result[0].number, 2);
        let why = result[0].as_message().expect("missing");
        assert_eq!(why[0].as_string().as_deref(), Some("blob not found"));

        let ack = kv_store_ack(&id);
        let inner = fields(&ack)[0].as_message().expect("field 3");
        let stored = inner
            .iter()
            .find(|field| field.number == 3)
            .expect("ack field 3");
        assert_eq!(stored.as_bytes(), Some(&b""[..]));
    }

    fn server_kv_get(id: &[u8], digest: &[u8]) -> Vec<u8> {
        Pb::new()
            .message(
                4,
                Pb::new()
                    .bytes(1, id)
                    .message(2, Pb::new().bytes(1, digest)),
            )
            .finish()
    }

    #[test]
    fn text_thinking_heartbeat_turn_end_and_tool_count_decode() {
        let text = Pb::new()
            .message(1, Pb::new().message(1, Pb::new().string(1, "Hello")))
            .finish();
        let (events, unknown) = decode_server(&text);
        assert_eq!(events, [ServerEvent::TextDelta("Hello".into())]);
        assert_eq!(unknown, 0);

        let thinking = Pb::new()
            .message(1, Pb::new().message(4, Pb::new().string(1, "hmm")))
            .finish();
        let (events, _) = decode_server(&thinking);
        assert_eq!(events, [ServerEvent::ThinkingDelta("hmm".into())]);

        let beat = Pb::new().message(1, Pb::new().bytes(13, &[])).finish();
        let (events, _) = decode_server(&beat);
        assert_eq!(events, [ServerEvent::Heartbeat]);

        let usage = TurnUsage {
            input: 10,
            output: 3,
            cache_read: 2,
            cache_write: 1,
            reasoning: 4,
        };
        let end = Pb::new()
            .message(
                1,
                Pb::new().message(
                    14,
                    Pb::new()
                        .varint(1, 10)
                        .varint(2, 3)
                        .varint(3, 2)
                        .varint(4, 1)
                        .varint(5, 4),
                ),
            )
            .finish();
        let (events, _) = decode_server(&end);
        assert_eq!(events, [ServerEvent::TurnEnd(usage)]);

        let count = Pb::new()
            .message(1, Pb::new().message(27, Pb::new().varint(1, 2)))
            .finish();
        let (events, _) = decode_server(&count);
        assert_eq!(events, [ServerEvent::ToolCallCount(2)]);
    }

    #[test]
    fn exec_mcp_arguments_and_refusal_frame() {
        let args = Pb::new()
            .string(1, "tools-web_search")
            .bytes(
                2,
                &Pb::new()
                    .string(1, "query")
                    .bytes(2, &pb_value(&json!("weather")))
                    .finish(),
            )
            .string(3, "call-9")
            .string(5, "")
            .finish();
        let exec = Pb::new()
            .message(
                2,
                Pb::new().varint(1, 4).string(15, "ex-4").bytes(11, &args),
            )
            .finish();
        let (events, _) = decode_server(&exec);
        match &events[..] {
            [ServerEvent::Exec(req)] => {
                assert_eq!(req.id, 4);
                assert_eq!(req.exec_id, "ex-4");
                match &req.kind {
                    ExecKind::McpArguments {
                        name,
                        call_id,
                        arguments,
                    } => {
                        assert_eq!(name, "web_search");
                        assert_eq!(call_id, "call-9");
                        assert_eq!(arguments, &json!({"query": "weather"}));
                    }
                    other => panic!("expected mcp arguments, got {other:?}"),
                }
            }
            other => panic!("expected one exec, got {other:?}"),
        }
        let refusal = refuse_exec(4);
        let close = close_exec(4);
        let refuse_inner = fields(&refusal)[0].as_message().expect("field 5");
        assert_eq!(refuse_inner[0].number, 2);
        let body = refuse_inner[0].as_message().expect("refusal");
        assert_eq!(body[0].as_varint(), Some(4));
        assert_eq!(body[1].as_string().as_deref(), Some("not available"));
        let close_inner = fields(&close)[0].as_message().expect("field 5");
        assert_eq!(close_inner[0].number, 1);
        assert_eq!(
            close_inner[0].as_message().expect("id")[0].as_varint(),
            Some(4)
        );
    }

    #[test]
    fn exec_mcp_argument_entry_nesting_is_bounded() {
        let mut entries = Pb::new()
            .string(1, "query")
            .bytes(2, &pb_value(&json!("weather")))
            .finish();
        for _ in 0..MAX_DECODE_DEPTH + 8 {
            entries = Pb::new().bytes(9, &entries).finish();
        }
        let args = Pb::new()
            .string(1, "tools-web_search")
            .bytes(2, &entries)
            .finish();
        let exec = Pb::new().message(2, Pb::new().bytes(11, &args)).finish();
        let (events, _) = decode_server(&exec);
        match &events[..] {
            [ServerEvent::Exec(req)] => match &req.kind {
                ExecKind::McpArguments { arguments, .. } => {
                    assert_eq!(arguments, &json!({}));
                }
                other => panic!("expected mcp arguments, got {other:?}"),
            },
            other => panic!("expected one exec, got {other:?}"),
        }
    }

    #[test]
    fn exec_env_and_tools_replies_are_followed_by_close() {
        let env = exec_env_result(1, "ex-1", "linux", "/tmp/r8e");
        let close = close_exec(1);
        let result = fields(&env)[0].as_message().expect("field 2");
        assert_eq!(result[0].as_varint(), Some(1));
        assert_eq!(
            result
                .iter()
                .find(|f| f.number == 15)
                .and_then(Field::as_string)
                .as_deref(),
            Some("ex-1")
        );
        let field10 = result
            .iter()
            .find(|field| field.number == 10)
            .and_then(Field::as_message)
            .expect("field 10");
        let deepest = field10
            .first()
            .and_then(Field::as_message)
            .and_then(|first| first.first().and_then(Field::as_message))
            .and_then(|second| {
                second
                    .into_iter()
                    .find(|field| field.number == 4)
                    .and_then(|field| field.as_message())
            })
            .expect("environment");
        assert_eq!(
            deepest
                .iter()
                .find(|field| field.number == 1)
                .and_then(Field::as_string)
                .as_deref(),
            Some("linux")
        );
        assert_eq!(fields(&close)[0].number, 5);

        let tools = exec_tools_result(2, "ex-2", &[]);
        let inner = fields(&tools)[0].as_message().expect("field 2");
        let field36 = inner
            .iter()
            .find(|field| field.number == 36)
            .and_then(Field::as_bytes)
            .expect("field 36");
        assert!(
            !field36.is_empty(),
            "connected-server reply must not be a bare empty message"
        );
        let server = fields(field36)[0].as_message().expect("f1")[0]
            .as_message()
            .expect("inner");
        let names: Vec<_> = server
            .iter()
            .filter(|field| field.number == 1 || field.number == 2)
            .filter_map(Field::as_string)
            .collect();
        assert_eq!(names, ["tools", "tools"]);
        assert_eq!(
            server
                .iter()
                .find(|field| field.number == 7)
                .and_then(Field::as_string)
                .as_deref(),
            Some("connected")
        );
        let _ = close_exec(2);
        let _ = environment;
        let _ = ToolDef {
            name: "x".into(),
            description: String::new(),
            schema: Value::Null,
        };
    }

    #[test]
    fn bidi_append_sequences_start_at_zero() {
        let first = bidi_append(b"\x3a\x00", "req-1", 0);
        let second = bidi_append(b"\x1a\x00", "req-1", 1);
        let one = fields(&first);
        assert_eq!(one[0].as_string().as_deref(), Some("3a00"));
        assert_eq!(
            one[1].as_message().expect("id")[0].as_string().as_deref(),
            Some("req-1")
        );
        assert_eq!(one[2].as_varint(), Some(0));
        assert_eq!(fields(&second)[2].as_varint(), Some(1));
        assert_eq!(to_hex(b"\x1a\x00"), "1a00");
    }

    #[test]
    fn mcp_tool_name_strips_tools_prefix_only_when_field5_is_empty() {
        assert_eq!(mcp_tool_name("tools-shell", ""), "shell");
        assert_eq!(mcp_tool_name("tools-shell", "other"), "other");
        assert_eq!(mcp_tool_name("shell", ""), "shell");
        assert_eq!(
            unknown_count(
                &[Field {
                    number: 9,
                    value: WireValue::Varint(1)
                }],
                &[1, 2, 3, 4]
            ),
            1
        );
    }
}
