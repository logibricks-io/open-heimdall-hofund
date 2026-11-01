#[derive(Debug)]
pub(crate) struct Node {
    pub(crate) start: usize,
    pub(crate) end: usize,
    pub(crate) kind: Kind,
}

#[derive(Debug)]
pub(crate) enum Kind {
    Object(Vec<Member>),
    Array(Vec<Node>),
    String(String),
    Number,
    Bool,
    Null,
}

#[derive(Debug)]
pub(crate) struct Member {
    pub(crate) key: String,
    pub(crate) key_start: usize,
    pub(crate) key_end: usize,
    pub(crate) value: Node,
}

#[derive(Clone, Debug)]
pub(crate) struct Edit {
    pub(crate) start: usize,
    pub(crate) end: usize,
    pub(crate) replacement: Vec<u8>,
}

impl Edit {
    pub(crate) fn new(start: usize, end: usize, replacement: impl Into<Vec<u8>>) -> Self {
        Self {
            start,
            end,
            replacement: replacement.into(),
        }
    }
}

impl Node {
    pub(crate) fn get(&self, key: &str) -> Option<&Node> {
        self.members()?
            .iter()
            .find(|member| member.key == key)
            .map(|member| &member.value)
    }

    pub(crate) fn members(&self) -> Option<&[Member]> {
        match &self.kind {
            Kind::Object(members) => Some(members),
            _ => None,
        }
    }

    pub(crate) fn items(&self) -> Option<&[Node]> {
        match &self.kind {
            Kind::Array(items) => Some(items),
            _ => None,
        }
    }

    pub(crate) fn text(&self) -> Option<&str> {
        match &self.kind {
            Kind::String(text) => Some(text),
            _ => None,
        }
    }

    pub(crate) fn is_object(&self) -> bool {
        matches!(self.kind, Kind::Object(_))
    }

    pub(crate) fn is_empty_object(&self) -> bool {
        self.members().is_some_and(<[Member]>::is_empty)
    }

    pub(crate) fn is_null(&self) -> bool {
        matches!(self.kind, Kind::Null)
    }
}

pub(crate) fn parse(body: &[u8]) -> Option<Node> {
    let mut parser = Parser { body, at: 0 };
    parser.space();
    let node = parser.node(0)?;
    parser.space();
    (parser.at == body.len()).then_some(node)
}

pub(crate) fn apply(body: &[u8], mut edits: Vec<Edit>) -> Vec<u8> {
    if edits.is_empty() {
        return body.to_vec();
    }
    edits.sort_by_key(|edit| (edit.start, edit.end));
    let mut out = Vec::with_capacity(body.len() + edits.len() * 16);
    let mut last = 0;
    for edit in edits {
        if edit.start < last || edit.end > body.len() || edit.start > edit.end {
            continue;
        }
        out.extend_from_slice(&body[last..edit.start]);
        out.extend_from_slice(&edit.replacement);
        last = edit.end;
    }
    out.extend_from_slice(&body[last..]);
    out
}

pub(crate) fn type_edit(node: &Node, json_type: String) -> Edit {
    let mut replacement = format!("\"type\":{json_type}").into_bytes();
    if !node.is_empty_object() {
        replacement.push(b',');
    }
    Edit::new(node.start + 1, node.start + 1, replacement)
}

pub(crate) fn remove_fields(root: &Node, names: &[&str]) -> Vec<Edit> {
    let Some(members) = root.members() else {
        return Vec::new();
    };
    let mut edits = Vec::new();
    let mut at = 0;
    while at < members.len() {
        if !names.contains(&members[at].key.as_str()) {
            at += 1;
            continue;
        }
        let first = at;
        while at < members.len() && names.contains(&members[at].key.as_str()) {
            at += 1;
        }
        let last = at - 1;
        if at < members.len() {
            edits.push(Edit::new(
                members[first].key_start,
                members[at].key_start,
                Vec::new(),
            ));
        } else if first > 0 {
            edits.push(Edit::new(
                members[first - 1].value.end,
                members[last].value.end,
                Vec::new(),
            ));
        } else {
            edits.push(Edit::new(
                members[first].key_start,
                members[last].value.end,
                Vec::new(),
            ));
        }
    }
    edits
}

pub(crate) fn walk_schema(node: &Node, callback: &mut impl FnMut(&Node)) {
    if node.items().is_some() {
        for item in node.items().unwrap_or_default() {
            walk_schema(item, callback);
        }
        return;
    }
    let Some(members) = node.members() else {
        return;
    };
    callback(node);
    for member in members {
        match member.key.as_str() {
            "properties" | "$defs" | "definitions" | "dependentSchemas" | "patternProperties" => {
                if let Some(properties) = member.value.members() {
                    for property in properties {
                        walk_schema(&property.value, callback);
                    }
                }
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
            | "oneOf"
            | "prefixItems"
            | "unevaluatedProperties"
            | "unevaluatedItems"
            | "contentSchema" => {
                walk_schema(&member.value, callback);
            }
            _ => {}
        }
    }
}

pub(crate) fn tool_roots<'a>(tools: &'a Node, roots: &mut Vec<&'a Node>) {
    if let Some(items) = tools.items() {
        for item in items {
            tool_roots(item, roots);
        }
        return;
    }
    let Some(members) = tools.members() else {
        return;
    };
    for member in members {
        match member.key.as_str() {
            "parameters" | "input_schema" => roots.push(&member.value),
            "function" => tool_roots(&member.value, roots),
            _ => {}
        }
    }
}

pub(crate) fn local_ref<'a>(root: &'a Node, reference: &str) -> Option<&'a Node> {
    if reference == "#" {
        return Some(root);
    }
    let pointer = reference.strip_prefix("#/")?;
    let mut current = root;
    for token in pointer.split('/') {
        let token = percent_decode(&token.replace("~1", "/").replace("~0", "~"));
        current = match &current.kind {
            Kind::Object(members) => &members.iter().find(|member| member.key == token)?.value,
            Kind::Array(items) => items.get(token.parse::<usize>().ok()?)?,
            _ => return None,
        };
    }
    Some(current)
}

fn percent_decode(token: &str) -> String {
    let bytes = token.as_bytes();
    let mut out = Vec::with_capacity(bytes.len());
    let mut at = 0;
    while at < bytes.len() {
        if bytes[at] == b'%' && at + 2 < bytes.len() {
            let Some(high) = hex(bytes[at + 1]) else {
                return token.to_owned();
            };
            let Some(low) = hex(bytes[at + 2]) else {
                return token.to_owned();
            };
            out.push(high << 4 | low);
            at += 3;
        } else {
            out.push(bytes[at]);
            at += 1;
        }
    }
    String::from_utf8(out).unwrap_or_else(|_| token.to_owned())
}

fn hex(byte: u8) -> Option<u8> {
    match byte {
        b'0'..=b'9' => Some(byte - b'0'),
        b'a'..=b'f' => Some(byte - b'a' + 10),
        b'A'..=b'F' => Some(byte - b'A' + 10),
        _ => None,
    }
}

struct Parser<'a> {
    body: &'a [u8],
    at: usize,
}

impl Parser<'_> {
    fn node(&mut self, depth: u32) -> Option<Node> {
        if depth > 128 {
            return None;
        }
        self.space();
        let start = self.at;
        let byte = *self.body.get(self.at)?;
        match byte {
            b'{' => self.object(start, depth),
            b'[' => self.array(start, depth),
            b'"' => {
                let value = self.string()?;
                Some(Node {
                    start,
                    end: self.at,
                    kind: Kind::String(value),
                })
            }
            b't' => {
                self.literal(b"true")?;
                Some(Node {
                    start,
                    end: self.at,
                    kind: Kind::Bool,
                })
            }
            b'f' => {
                self.literal(b"false")?;
                Some(Node {
                    start,
                    end: self.at,
                    kind: Kind::Bool,
                })
            }
            b'n' => {
                self.literal(b"null")?;
                Some(Node {
                    start,
                    end: self.at,
                    kind: Kind::Null,
                })
            }
            b'-' | b'0'..=b'9' => self.number(start),
            _ => None,
        }
    }

    fn object(&mut self, start: usize, depth: u32) -> Option<Node> {
        self.at += 1;
        self.space();
        let mut members = Vec::new();
        if self.take(b'}') {
            return Some(Node {
                start,
                end: self.at,
                kind: Kind::Object(members),
            });
        }
        loop {
            self.space();
            let key_start = self.at;
            let key = self.string()?;
            let key_end = self.at;
            self.space();
            if !self.take(b':') {
                return None;
            }
            let value = self.node(depth + 1)?;
            members.push(Member {
                key,
                key_start,
                key_end,
                value,
            });
            self.space();
            if self.take(b'}') {
                break;
            }
            if !self.take(b',') {
                return None;
            }
        }
        Some(Node {
            start,
            end: self.at,
            kind: Kind::Object(members),
        })
    }

    fn array(&mut self, start: usize, depth: u32) -> Option<Node> {
        self.at += 1;
        self.space();
        let mut items = Vec::new();
        if self.take(b']') {
            return Some(Node {
                start,
                end: self.at,
                kind: Kind::Array(items),
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
        Some(Node {
            start,
            end: self.at,
            kind: Kind::Array(items),
        })
    }

    fn string(&mut self) -> Option<String> {
        let start = self.at;
        if !self.take(b'"') {
            return None;
        }
        while let Some(byte) = self.body.get(self.at).copied() {
            match byte {
                b'"' => {
                    self.at += 1;
                    return serde_json::from_slice::<String>(&self.body[start..self.at]).ok();
                }
                b'\\' => {
                    self.at += 2;
                }
                0..=0x1f => return None,
                _ => self.at += 1,
            }
        }
        None
    }

    fn number(&mut self, start: usize) -> Option<Node> {
        while let Some(byte) = self.body.get(self.at).copied() {
            if matches!(byte, b' ' | b'\t' | b'\r' | b'\n' | b',' | b']' | b'}') {
                break;
            }
            self.at += 1;
        }
        serde_json::from_slice::<serde_json::Number>(&self.body[start..self.at]).ok()?;
        Some(Node {
            start,
            end: self.at,
            kind: Kind::Number,
        })
    }

    fn literal(&mut self, literal: &[u8]) -> Option<()> {
        if self.body.get(self.at..self.at + literal.len())? == literal {
            self.at += literal.len();
            Some(())
        } else {
            None
        }
    }

    fn space(&mut self) {
        while self
            .body
            .get(self.at)
            .is_some_and(|byte| matches!(byte, b' ' | b'\t' | b'\r' | b'\n'))
        {
            self.at += 1;
        }
    }

    fn take(&mut self, byte: u8) -> bool {
        if self.body.get(self.at) == Some(&byte) {
            self.at += 1;
            true
        } else {
            false
        }
    }
}
