use std::collections::BTreeSet;

use hofund_ir::Request;

use crate::json::{self, Edit, Kind, Node};
use crate::state::{RequestState, fold_object_roots, wanted_token_field, with_token_field};

/// The endpoint and protocol an encoded request is going to.
#[derive(Clone, Copy, Debug)]
pub struct EncodedContext<'a> {
    /// Configured upstream base URL.
    pub base_url: &'a str,
    /// Catalog preset id, when the endpoint came from one.
    pub preset: Option<&'a str>,
    /// Outbound codec id.
    pub protocol: &'a str,
}

/// Restores protocol fields that an automatic encoded-request rule needs.
pub fn apply_before_encode(_context: EncodedContext<'_>, request: &mut Request) {
    if let Some(original) = request
        .params
        .extensions
        .get("originalChatMaxTokens")
        .cloned()
    {
        request
            .params
            .extensions
            .insert("max_tokens".to_owned(), original);
    }
}

/// Applies the automatic vendor request fixes in their reference order.
/// Untouched bodies and every byte outside a targeted edit are preserved.
#[must_use]
pub fn apply_encoded(context: EncodedContext<'_>, body: &[u8]) -> Vec<u8> {
    let mut body = chat_token_field(context, &RequestState::default(), body);
    for rule in ENCODED_RULES {
        body = rule(context, &body);
    }
    body
}

/// Applies learned root folding and token-field selection before the
/// automatic encoded-request rules.
#[must_use]
pub fn apply_encoded_with_state(
    context: EncodedContext<'_>,
    state: &RequestState,
    body: &[u8],
) -> Vec<u8> {
    let model = json::parse(body)
        .and_then(|root| root.get("model").and_then(Node::text).map(str::to_owned))
        .unwrap_or_default();
    let (body, _) = fold_object_roots(context.base_url, context.protocol, &model, state, body);
    let body = chat_token_field(context, state, &body);
    apply_encoded(context, &body)
}

fn chat_token_field(context: EncodedContext<'_>, state: &RequestState, body: &[u8]) -> Vec<u8> {
    if context.protocol != "openai-chat" {
        return body.to_vec();
    }
    let Some(wanted) = wanted_token_field(context.base_url, state) else {
        return body.to_vec();
    };
    with_token_field(body, wanted).0
}

type EncodedRule = fn(EncodedContext<'_>, &[u8]) -> Vec<u8>;

const ENCODED_RULES: &[EncodedRule] = &[
    deepseek_tool_patterns,
    grok_tool_patterns,
    deepseek_token_field,
    tool_one_of_as_any_of,
    kimi_tool_enum_types,
    kimi_tool_ref_types,
    gemini_tool_types,
    kimi_locked_sampling,
];

fn deepseek_tool_patterns(context: EncodedContext<'_>, body: &[u8]) -> Vec<u8> {
    if !matches!(context.preset, Some("deepseek")) && host(context.base_url) != "api.deepseek.com" {
        return body.to_vec();
    }
    if !contains(body, br#"\\0"#) {
        return body.to_vec();
    }
    tool_patterns(body, unicode_null_escape)
}

fn grok_tool_patterns(context: EncodedContext<'_>, body: &[u8]) -> Vec<u8> {
    let Some(root) = json::parse(body) else {
        return body.to_vec();
    };
    let model = root.get("model").and_then(Node::text).unwrap_or_default();
    if host(context.base_url) != "api.x.ai" && !grok_model(model) {
        return body.to_vec();
    }
    if !contains(body, br#""pattern""#) {
        return body.to_vec();
    }
    tool_patterns(body, ecma_class_for_rust)
}

fn deepseek_token_field(context: EncodedContext<'_>, body: &[u8]) -> Vec<u8> {
    if context.protocol != "openai-chat"
        || !matches!(context.preset, Some("deepseek"))
            && host(context.base_url) != "api.deepseek.com"
    {
        return body.to_vec();
    }
    let Some(root) = json::parse(body) else {
        return body.to_vec();
    };
    if root.get("max_tokens").is_some() {
        return body.to_vec();
    }
    let Some(members) = root.members() else {
        return body.to_vec();
    };
    let Some(field) = members
        .iter()
        .find(|member| member.key == "max_completion_tokens")
    else {
        return body.to_vec();
    };
    json::apply(
        body,
        vec![Edit::new(
            field.key_start,
            field.key_end,
            br#""max_tokens""#.to_vec(),
        )],
    )
}

fn tool_one_of_as_any_of(context: EncodedContext<'_>, body: &[u8]) -> Vec<u8> {
    if !matches!(context.preset, Some("clinepass" | "kilo"))
        && host(context.base_url) != "openrouter.ai"
    {
        return body.to_vec();
    }
    if !contains(body, br#""oneOf""#) {
        return body.to_vec();
    }
    let Some(root) = json::parse(body) else {
        return body.to_vec();
    };
    let Some(tools) = root.get("tools") else {
        return body.to_vec();
    };
    let mut edits = Vec::new();
    collect_one_of(tools, false, &mut edits);
    json::apply(body, edits)
}

fn collect_one_of(node: &Node, in_schema: bool, edits: &mut Vec<Edit>) {
    if let Some(items) = node.items() {
        for item in items {
            collect_one_of(item, in_schema, edits);
        }
        return;
    }
    let Some(members) = node.members() else {
        return;
    };
    for member in members {
        if !in_schema {
            match member.key.as_str() {
                "parameters" | "input_schema" => collect_one_of(&member.value, true, edits),
                "function" => collect_one_of(&member.value, false, edits),
                _ => {}
            }
            continue;
        }
        match member.key.as_str() {
            "properties" | "$defs" | "definitions" | "dependentSchemas" | "patternProperties" => {
                if let Some(properties) = member.value.members() {
                    for property in properties {
                        collect_one_of(&property.value, true, edits);
                    }
                }
            }
            "oneOf" => {
                edits.push(Edit::new(
                    member.key_start,
                    member.key_end,
                    br#""anyOf""#.to_vec(),
                ));
                collect_one_of(&member.value, true, edits);
            }
            "items"
            | "additionalProperties"
            | "contains"
            | "propertyNames"
            | "not"
            | "if"
            | "then"
            | "else"
            | "allOf"
            | "anyOf"
            | "prefixItems"
            | "unevaluatedProperties"
            | "unevaluatedItems"
            | "contentSchema" => collect_one_of(&member.value, true, edits),
            _ => {}
        }
    }
}

fn kimi_tool_enum_types(context: EncodedContext<'_>, body: &[u8]) -> Vec<u8> {
    if !is_kimi(context) || !contains(body, br#""enum""#) && !contains(body, br#""const""#) {
        return body.to_vec();
    }
    let Some(root) = json::parse(body) else {
        return body.to_vec();
    };
    let Some(tools) = root.get("tools") else {
        return body.to_vec();
    };
    let mut edits = Vec::new();
    let mut roots = Vec::new();
    json::tool_roots(tools, &mut roots);
    for root in roots {
        json::walk_schema(root, &mut |node| {
            if node.get("type").is_some() {
                return;
            }
            let values = if let Some(values) = node.get("enum").and_then(Node::items) {
                values
            } else if let Some(value) = node.get("const") {
                std::slice::from_ref(value)
            } else {
                return;
            };
            if let Some(kind) = enum_type(values, body) {
                edits.push(json::type_edit(node, kind));
            }
        });
    }
    json::apply(body, edits)
}

fn kimi_tool_ref_types(context: EncodedContext<'_>, body: &[u8]) -> Vec<u8> {
    if !is_kimi(context) || !contains(body, br#""$ref""#) {
        return body.to_vec();
    }
    let Some(root) = json::parse(body) else {
        return body.to_vec();
    };
    let Some(tools) = root.get("tools") else {
        return body.to_vec();
    };
    let mut edits = Vec::new();
    let mut roots = Vec::new();
    json::tool_roots(tools, &mut roots);
    for schema_root in roots {
        if !schema_root.is_object() {
            continue;
        }
        json::walk_schema(schema_root, &mut |node| {
            let Some(reference) = node.get("$ref").and_then(Node::text) else {
                return;
            };
            let Some(target) = json::local_ref(schema_root, reference) else {
                return;
            };
            if !untyped_for_kimi(target) {
                return;
            }
            if let Some(kind) = kimi_schema_type(schema_root, target, body) {
                edits.push(json::type_edit(target, kind));
            }
        });
    }
    edits.sort_by_key(|edit| edit.start);
    edits.dedup_by_key(|edit| edit.start);
    json::apply(body, edits)
}

fn kimi_locked_sampling(context: EncodedContext<'_>, body: &[u8]) -> Vec<u8> {
    if !is_kimi(context) && !is_ark(context.base_url) {
        return body.to_vec();
    }
    let Some(root) = json::parse(body) else {
        return body.to_vec();
    };
    let model = root.get("model").and_then(Node::text).unwrap_or_default();
    if !kimi_locked_sampling_model(model) {
        return body.to_vec();
    }
    let edits = json::remove_fields(&root, &["temperature", "top_p"]);
    json::apply(body, edits)
}

fn is_kimi(context: EncodedContext<'_>) -> bool {
    if matches!(
        host(context.base_url).as_str(),
        "api.moonshot.ai" | "api.moonshot.cn" | "api.kimi.com" | "api.kimi.ai"
    ) {
        return true;
    }
    context
        .preset
        .is_some_and(|preset| preset.starts_with("moonshot") || preset.starts_with("kimi"))
}

fn is_ark(base_url: &str) -> bool {
    let host = host(base_url);
    host.starts_with("ark.") && host.ends_with(".volces.com")
}

fn grok_model(model: &str) -> bool {
    model
        .rsplit('/')
        .next()
        .unwrap_or(model)
        .to_ascii_lowercase()
        .starts_with("grok")
}

fn kimi_locked_sampling_model(model: &str) -> bool {
    let model = model
        .rsplit('/')
        .next()
        .unwrap_or(model)
        .to_ascii_lowercase();
    if model.starts_with("kimi-for-coding") {
        return true;
    }
    let Some(version) = model
        .strip_prefix("kimi-k")
        .or_else(|| model.strip_prefix('k'))
    else {
        return false;
    };
    let (major, rest) = leading_int(version);
    if major < 0 {
        return false;
    }
    if major != 2 {
        return major >= 3;
    }
    let Some(minor) = rest.strip_prefix('.') else {
        return false;
    };
    leading_int(minor).0 >= 5
}

fn leading_int(text: &str) -> (i32, &str) {
    let digits = text.bytes().take_while(u8::is_ascii_digit).count();
    if digits == 0 {
        return (-1, text);
    }
    (text[..digits].parse::<i32>().unwrap_or(-1), &text[digits..])
}

fn tool_patterns(body: &[u8], fix: fn(&str) -> String) -> Vec<u8> {
    let Some(root) = json::parse(body) else {
        return body.to_vec();
    };
    let Some(tools) = root.get("tools") else {
        return body.to_vec();
    };
    let mut edits = Vec::new();
    let mut roots = Vec::new();
    json::tool_roots(tools, &mut roots);
    for root in roots {
        json::walk_schema(root, &mut |node| {
            let Some(pattern) = node.get("pattern") else {
                return;
            };
            let Some(text) = pattern.text() else {
                return;
            };
            let replacement = fix(text);
            if replacement != text {
                edits.push(Edit::new(
                    pattern.start,
                    pattern.end,
                    encode_json_string(&replacement).into_bytes(),
                ));
            }
        });
    }
    json::apply(body, edits)
}

fn unicode_null_escape(pattern: &str) -> String {
    let bytes = pattern.as_bytes();
    let mut out = String::with_capacity(pattern.len());
    let mut at = 0;
    while at < bytes.len() {
        if bytes[at] != b'\\' || at + 1 == bytes.len() {
            out.push(char::from(bytes[at]));
            at += 1;
            continue;
        }
        let next = bytes[at + 1];
        if next == b'0' && (at + 2 == bytes.len() || !bytes[at + 2].is_ascii_digit()) {
            out.push_str(r#"\u0000"#);
        } else {
            out.push('\\');
            out.push(char::from(next));
        }
        at += 2;
    }
    out
}

fn ecma_class_for_rust(pattern: &str) -> String {
    let bytes = pattern.as_bytes();
    let mut out = String::with_capacity(pattern.len());
    let mut in_class = false;
    let mut at = 0;
    while at < bytes.len() {
        let byte = bytes[at];
        if byte == b'\\' && at + 1 < bytes.len() {
            out.push('\\');
            out.push(char::from(bytes[at + 1]));
            at += 2;
            continue;
        }
        match byte {
            b'[' if !in_class => in_class = true,
            b']' if in_class => in_class = false,
            b'[' if in_class => {
                out.push_str(r#"\["#);
                at += 1;
                continue;
            }
            b'&' | b'-' | b'~' if in_class && at + 1 < bytes.len() && bytes[at + 1] == byte => {
                out.push(char::from(byte));
                out.push('\\');
                out.push(char::from(byte));
                at += 2;
                continue;
            }
            _ => {}
        }
        out.push(char::from(byte));
        at += 1;
    }
    out
}

fn encode_json_string(text: &str) -> String {
    let mut out = String::with_capacity(text.len() + 2);
    out.push('"');
    for character in text.chars() {
        match character {
            '"' => out.push_str("\\\""),
            '\\' => out.push_str(r#"\\"#),
            '\n' => out.push_str(r#"\n"#),
            '\r' => out.push_str(r#"\r"#),
            '\t' => out.push_str(r#"\t"#),
            character if character < '\u{20}' => {
                use std::fmt::Write;
                write!(out, "\\u{:04x}", u32::from(character)).expect("write to string");
            }
            character => out.push(character),
        }
    }
    out.push('"');
    out
}

fn enum_type(values: &[Node], body: &[u8]) -> Option<String> {
    let mut null = false;
    let mut rest = Vec::new();
    for value in values {
        if value.is_null() {
            null = true;
        } else {
            rest.push(value);
        }
    }
    let kind = values_type(&rest, body)?;
    if null {
        Some(format!("[\"{kind}\",\"null\"]"))
    } else {
        Some(format!("\"{kind}\""))
    }
}

fn values_type(values: &[&Node], body: &[u8]) -> Option<&'static str> {
    let mut kind = None;
    for value in values {
        let value_kind = match &value.kind {
            Kind::String(_) => "string",
            Kind::Bool => "boolean",
            Kind::Number => {
                if body[value.start..value.end]
                    .iter()
                    .any(|byte| matches!(byte, b'.' | b'e' | b'E'))
                {
                    "number"
                } else {
                    "integer"
                }
            }
            _ => return None,
        };
        match (kind, value_kind) {
            (None, value_kind) => kind = Some(value_kind),
            (Some("integer"), "number") | (Some("number"), "integer") => kind = Some("number"),
            (Some(current), value_kind) if current == value_kind => {}
            _ => return None,
        }
    }
    kind
}

fn untyped_for_kimi(node: &Node) -> bool {
    if node.get("type").is_some() || node.get("anyOf").is_some() {
        return false;
    }
    let Some(members) = node.members() else {
        return false;
    };
    if members.is_empty() {
        return false;
    }
    let only_notes = members.iter().all(|member| {
        matches!(
            member.key.as_str(),
            "$ref" | "description" | "title" | "default" | "examples" | "$comment"
        )
    });
    !(only_notes && node.get("$ref").is_some())
}

fn kimi_schema_type(root: &Node, node: &Node, body: &[u8]) -> Option<String> {
    if let Some(values) = node.get("enum").and_then(Node::items) {
        return enum_type(values, body);
    }
    if let Some(value) = node.get("const") {
        return enum_type(std::slice::from_ref(value), body);
    }
    let mut types = schema_types(root, node, body, 0);
    if types.is_empty() {
        return Some(ALL_TYPES.to_owned());
    }
    if types.contains("integer") && types.contains("number") {
        types.remove("integer");
    }
    let names: Vec<&str> = TYPE_ORDER
        .iter()
        .copied()
        .filter(|kind| types.contains(kind))
        .collect();
    if names.len() == 1 {
        Some(format!("\"{}\"", names[0]))
    } else if names.is_empty() {
        None
    } else {
        Some(format!(
            "[{}]",
            names
                .iter()
                .map(|kind| format!("\"{kind}\""))
                .collect::<Vec<_>>()
                .join(",")
        ))
    }
}

const ALL_TYPES: &str = r#"["string","number","integer","boolean","object","array","null"]"#;
const TYPE_ORDER: &[&str] = &[
    "string", "number", "integer", "boolean", "object", "array", "null",
];

fn schema_types<'a>(
    root: &'a Node,
    node: &'a Node,
    body: &[u8],
    depth: u32,
) -> BTreeSet<&'static str> {
    if depth > 16 || !node.is_object() {
        return BTreeSet::new();
    }
    let mut types = BTreeSet::new();
    if let Some(kind) = node.get("type") {
        if let Some(kind) = kind.text() {
            if let Some(kind) = schema_type_name(kind) {
                types.insert(kind);
            }
        } else if let Some(kinds) = kind.items() {
            for kind in kinds
                .iter()
                .filter_map(Node::text)
                .filter_map(schema_type_name)
            {
                types.insert(kind);
            }
        }
        return types;
    }
    if let Some(reference) = node.get("$ref").and_then(Node::text) {
        return json::local_ref(root, reference)
            .map(|target| schema_types(root, target, body, depth + 1))
            .unwrap_or_default();
    }
    if node.get("enum").and_then(Node::items).is_some() || node.get("const").is_some() {
        let empty = Vec::new();
        let values = node.get("enum").and_then(Node::items).unwrap_or(&empty);
        let values: Vec<&Node> = node
            .get("const")
            .map_or_else(|| values.iter().collect(), |value| vec![value]);
        for value in values {
            match &value.kind {
                Kind::String(_) => {
                    types.insert("string");
                }
                Kind::Bool => {
                    types.insert("boolean");
                }
                Kind::Null => {
                    types.insert("null");
                }
                Kind::Number => {
                    if body[value.start..value.end]
                        .iter()
                        .any(|byte| matches!(byte, b'.' | b'e' | b'E'))
                    {
                        types.insert("number");
                    } else {
                        types.insert("integer");
                    }
                }
                Kind::Array(_) => {
                    types.insert("array");
                }
                Kind::Object(_) => {
                    types.insert("object");
                }
            }
        }
        return types;
    }
    for key in ["oneOf", "anyOf", "allOf"] {
        let Some(branches) = node.get(key).and_then(Node::items) else {
            continue;
        };
        let mut known = false;
        for branch in branches {
            let branch_types = schema_types(root, branch, body, depth + 1);
            if branch_types.is_empty() && key != "allOf" {
                return BTreeSet::new();
            }
            for kind in branch_types {
                types.insert(kind);
                known = true;
            }
        }
        if known {
            return types;
        }
    }
    if let Some(kind) = keyword_type(node) {
        types.insert(kind);
    }
    types
}

fn keyword_type(node: &Node) -> Option<&'static str> {
    const OBJECT: &[&str] = &[
        "properties",
        "required",
        "additionalProperties",
        "patternProperties",
        "minProperties",
        "maxProperties",
        "propertyNames",
        "dependentRequired",
        "dependentSchemas",
        "unevaluatedProperties",
    ];
    const ARRAY: &[&str] = &[
        "items",
        "prefixItems",
        "minItems",
        "maxItems",
        "uniqueItems",
        "contains",
        "unevaluatedItems",
    ];
    const STRING: &[&str] = &[
        "minLength",
        "maxLength",
        "pattern",
        "format",
        "contentEncoding",
        "contentMediaType",
    ];
    const NUMBER: &[&str] = &[
        "minimum",
        "maximum",
        "exclusiveMinimum",
        "exclusiveMaximum",
        "multipleOf",
    ];
    for (keys, kind) in [
        (OBJECT, "object"),
        (ARRAY, "array"),
        (STRING, "string"),
        (NUMBER, "number"),
    ] {
        if keys.iter().any(|key| node.get(key).is_some()) {
            return Some(kind);
        }
    }
    None
}

fn schema_type_name(kind: &str) -> Option<&'static str> {
    TYPE_ORDER
        .iter()
        .copied()
        .find(|candidate| *candidate == kind)
}

fn host(base_url: &str) -> String {
    let authority = base_url
        .split_once("://")
        .map_or(base_url, |(_, rest)| rest)
        .split('/')
        .next()
        .unwrap_or_default()
        .rsplit('@')
        .next()
        .unwrap_or_default();
    if authority.starts_with('[') {
        return authority
            .split(']')
            .next()
            .map(|host| format!("{host}]").to_ascii_lowercase())
            .unwrap_or_default();
    }
    authority
        .split(':')
        .next()
        .unwrap_or_default()
        .to_ascii_lowercase()
}

fn contains(haystack: &[u8], needle: &[u8]) -> bool {
    !needle.is_empty()
        && haystack
            .windows(needle.len())
            .any(|window| window == needle)
}

fn gemini_tool_types(context: EncodedContext<'_>, body: &[u8]) -> Vec<u8> {
    if context.protocol == "generative-language" {
        return body.to_vec();
    }
    let Some(root) = json::parse(body) else {
        return body.to_vec();
    };
    let model = root.get("model").and_then(Node::text).unwrap_or_default();
    if !gemini_named(model) {
        return body.to_vec();
    }
    let Some(tools) = root.get("tools") else {
        return body.to_vec();
    };
    let mut roots = Vec::new();
    json::tool_roots(tools, &mut roots);
    let mut cuts = Vec::new();
    for schema_root in roots {
        if schema_needs_shape(schema_root, body)
            && let Some(shaped) = shape_schema(schema_root, body)
        {
            cuts.push(Edit::new(schema_root.start, schema_root.end, shaped));
        }
    }
    let shaped = json::apply(body, cuts);
    let Some(root) = json::parse(&shaped) else {
        return body.to_vec();
    };
    let Some(tools) = root.get("tools") else {
        return body.to_vec();
    };
    let mut edits = Vec::new();
    let mut roots = Vec::new();
    json::tool_roots(tools, &mut roots);
    for schema_root in roots {
        collect_gemini_untyped(schema_root, schema_root, true, &shaped, &mut edits);
    }
    edits.sort_by_key(|edit| edit.start);
    edits.dedup_by_key(|edit| edit.start);
    json::apply(&shaped, edits)
}

fn gemini_named(model: &str) -> bool {
    model
        .rsplit('/')
        .next()
        .unwrap_or(model)
        .to_ascii_lowercase()
        .starts_with("gemini")
}

fn collect_gemini_untyped(
    root: &Node,
    node: &Node,
    is_root: bool,
    body: &[u8],
    edits: &mut Vec<Edit>,
) {
    if let Some(items) = node.items() {
        for item in items {
            collect_gemini_untyped(root, item, false, body, edits);
        }
        return;
    }
    let Some(members) = node.members() else {
        return;
    };
    if node.get("type").is_none()
        && let Some(kind) = gemini_schema_type(root, node, is_root, body)
    {
        edits.push(json::type_edit(node, format!("\"{kind}\"")));
    }
    for member in members {
        match member.key.as_str() {
            "properties" | "$defs" | "definitions" | "dependentSchemas" | "patternProperties" => {
                if let Some(properties) = member.value.members() {
                    for property in properties {
                        collect_gemini_untyped(root, &property.value, false, body, edits);
                    }
                }
            }
            "items"
            | "additionalProperties"
            | "contains"
            | "propertyNames"
            | "then"
            | "else"
            | "allOf"
            | "anyOf"
            | "oneOf"
            | "prefixItems"
            | "unevaluatedProperties"
            | "unevaluatedItems"
            | "contentSchema" => {
                collect_gemini_untyped(root, &member.value, false, body, edits);
            }
            _ => {}
        }
    }
}

fn gemini_schema_type(
    root: &Node,
    node: &Node,
    is_root: bool,
    body: &[u8],
) -> Option<&'static str> {
    if let Some(kind @ ("object" | "array")) = keyword_type(node) {
        return Some(kind);
    }
    if node.get("enum").and_then(Node::items).is_some() || node.get("const").is_some() {
        let empty = Vec::new();
        let values = node.get("enum").and_then(Node::items).unwrap_or(&empty);
        let values: Vec<&Node> = node
            .get("const")
            .map_or_else(|| values.iter().collect(), |value| vec![value]);
        let values: Vec<&Node> = values
            .into_iter()
            .filter(|value| !value.is_null())
            .collect();
        return values_type(&values, body);
    }
    let mut types = schema_types(root, node, body, 0);
    types.remove("null");
    if types.contains("integer") && types.contains("number") {
        types.remove("integer");
    }
    if types.len() == 1 {
        return types.into_iter().next();
    }
    if !types.is_empty() {
        return None;
    }
    if is_root {
        return Some("object");
    }
    if ["$ref", "anyOf", "oneOf", "allOf"]
        .iter()
        .any(|key| node.get(key).is_some())
    {
        return None;
    }
    let Some(default) = node.get("default") else {
        return Some("string");
    };
    if default.is_null() {
        return Some("string");
    }
    match &default.kind {
        Kind::Object(_) => Some("object"),
        Kind::Array(_) => Some("array"),
        _ => values_type(&[default], body).or(Some("string")),
    }
}

fn schema_needs_shape(node: &Node, body: &[u8]) -> bool {
    if is_boolean_schema(node, body) {
        return true;
    }
    let Some(members) = node.members() else {
        return false;
    };
    members.iter().any(|member| {
        member.key.starts_with("x-")
            || match member.key.as_str() {
                "properties" | "patternProperties" | "$defs" | "definitions"
                | "dependentSchemas" => member.value.members().is_some_and(|properties| {
                    properties
                        .iter()
                        .any(|property| schema_needs_shape(&property.value, body))
                }),
                "items" | "contains" | "propertyNames" | "not" | "if" | "then" | "else"
                | "contentSchema" => schema_needs_shape(&member.value, body),
                "anyOf" | "oneOf" | "allOf" | "prefixItems" => member
                    .value
                    .items()
                    .is_some_and(|items| items.iter().any(|item| schema_needs_shape(item, body))),
                _ => false,
            }
    })
}

fn shape_schema(node: &Node, body: &[u8]) -> Option<Vec<u8>> {
    let raw = &body[node.start..node.end];
    if raw == b"true" {
        return Some(b"{}".to_vec());
    }
    if raw == b"false" {
        return None;
    }
    if !node.is_object() || !schema_needs_shape(node, body) {
        return Some(raw.to_vec());
    }
    let mut out = vec![b'{'];
    let mut first = true;
    for member in node.members().unwrap_or_default() {
        let key = &body[member.key_start..member.key_end];
        match member.key.as_str() {
            key if key.starts_with("x-") => {}
            "properties" | "patternProperties" | "$defs" | "definitions" | "dependentSchemas" => {
                if let Some(properties) = member.value.members() {
                    let mut map = vec![b'{'];
                    let mut property_first = true;
                    for property in properties {
                        if let Some(value) = shape_schema(&property.value, body) {
                            put_raw(
                                &mut map,
                                &mut property_first,
                                &body[property.key_start..property.key_end],
                                &value,
                            );
                        }
                    }
                    map.push(b'}');
                    put_raw(&mut out, &mut first, key, &map);
                } else {
                    put_raw(
                        &mut out,
                        &mut first,
                        key,
                        &body[member.value.start..member.value.end],
                    );
                }
            }
            "items" | "contains" | "propertyNames" | "not" | "if" | "then" | "else"
            | "contentSchema" => {
                if let Some(value) = shape_schema(&member.value, body) {
                    put_raw(&mut out, &mut first, key, &value);
                }
            }
            "anyOf" | "oneOf" | "allOf" | "prefixItems" => {
                if let Some(items) = member.value.items() {
                    let mut list = vec![b'['];
                    let mut item_first = true;
                    for item in items {
                        if let Some(value) = shape_schema(item, body) {
                            if !item_first {
                                list.push(b',');
                            }
                            item_first = false;
                            list.extend(value);
                        }
                    }
                    list.push(b']');
                    put_raw(&mut out, &mut first, key, &list);
                } else {
                    put_raw(
                        &mut out,
                        &mut first,
                        key,
                        &body[member.value.start..member.value.end],
                    );
                }
            }
            _ => put_raw(
                &mut out,
                &mut first,
                key,
                &body[member.value.start..member.value.end],
            ),
        }
    }
    out.push(b'}');
    Some(out)
}

fn put_raw(out: &mut Vec<u8>, first: &mut bool, key: &[u8], value: &[u8]) {
    if !*first {
        out.push(b',');
    }
    *first = false;
    out.extend_from_slice(key);
    out.push(b':');
    out.extend_from_slice(value);
}

fn is_boolean_schema(node: &Node, body: &[u8]) -> bool {
    matches!(&body[node.start..node.end], b"true" | b"false")
}
