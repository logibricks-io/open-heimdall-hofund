use std::ops::Range;

use serde_json::{Map, Value};

#[derive(Clone)]
struct Edit {
    span: Range<usize>,
    text: String,
}

struct Node {
    value: Value,
    span: Range<usize>,
    kind: NodeKind,
}

enum NodeKind {
    Scalar,
    Object {
        open: usize,
        close: usize,
        members: Vec<Member>,
    },
    Array {
        open: usize,
        close: usize,
        items: Vec<Element>,
    },
}

struct Member {
    key: String,
    start: usize,
    value: Node,
    comma: Option<usize>,
}

struct Element {
    value: Node,
    comma: Option<usize>,
}

pub(crate) fn reconcile(original: &str, before: &Value, after: &Value) -> Option<String> {
    if before == after {
        return Some(original.to_owned());
    }
    let mut parser = Parser::new(original);
    let node = parser.value()?;
    parser.skip_layout().ok()?;
    if parser.position != original.len() || &node.value != before {
        return None;
    }
    let mut edits = Vec::new();
    diff(original, &node, before, after, &mut edits)?;
    verified(apply(original, edits, after), after)
}

/// Keeps an in-place edit only when it reads back as the intended value, so a
/// scanner gap falls back to the full render instead of corrupting the file.
fn verified(text: String, after: &Value) -> Option<String> {
    (crate::edit::json_root(&text).ok()? == *after).then_some(text)
}

fn diff(
    text: &str,
    node: &Node,
    before: &Value,
    after: &Value,
    edits: &mut Vec<Edit>,
) -> Option<()> {
    if before == after {
        return Some(());
    }
    if &node.value != before {
        return None;
    }
    match (&node.kind, before, after) {
        (
            NodeKind::Object {
                open,
                close,
                members,
            },
            Value::Object(old),
            Value::Object(new),
        ) => {
            for (index, member) in members.iter().enumerate() {
                match new.get(&member.key) {
                    Some(value) => diff(text, &member.value, old.get(&member.key)?, value, edits)?,
                    None => {
                        let last_removed = !members[index + 1..]
                            .iter()
                            .any(|member| new.contains_key(&member.key));
                        let previous = (last_removed
                            && index > 0
                            && new.contains_key(&members[index - 1].key))
                        .then(|| members[index - 1].comma)
                        .flatten();
                        remove(
                            text,
                            member.start,
                            &member.value,
                            member.comma,
                            previous,
                            edits,
                        );
                    }
                }
            }
            let added: Vec<(&String, &Value)> = new
                .iter()
                .filter(|(key, _)| !old.contains_key(*key))
                .collect();
            if !added.is_empty() {
                insert_object(text, *open, *close, members, &added, edits)?;
            }
            Some(())
        }
        (NodeKind::Array { open, close, items }, Value::Array(old), Value::Array(new)) => {
            for (index, (old_value, new_value)) in old.iter().zip(new).enumerate() {
                diff(text, &items[index].value, old_value, new_value, edits)?;
            }
            for (index, item) in items.iter().enumerate().skip(new.len()) {
                let previous = (index > 0 && index - 1 < new.len())
                    .then(|| items[index - 1].comma)
                    .flatten();
                remove(
                    text,
                    item.value.span.start,
                    &item.value,
                    item.comma,
                    previous,
                    edits,
                );
            }
            if new.len() > old.len() {
                insert_array(text, *open, *close, items, &new[old.len()..], edits)?;
            }
            Some(())
        }
        _ => {
            let indent = line_indent(text, node.span.start).to_owned();
            let unit = indent_unit(text, node.span.start);
            edits.push(Edit {
                span: node.span.clone(),
                text: render(after, &indent, &unit),
            });
            Some(())
        }
    }
}

fn remove(
    text: &str,
    start: usize,
    value: &Node,
    comma: Option<usize>,
    previous_comma: Option<usize>,
    edits: &mut Vec<Edit>,
) {
    let end = comma.map_or(value.span.end, |comma| comma + 1);
    let line_start = text[..start].rfind('\n').map_or(0, |position| position + 1);
    let whole_line = text[line_start..start].trim().is_empty();
    let end = if whole_line {
        text[end..]
            .find('\n')
            .map_or(end, |newline| end + newline + 1)
    } else {
        end
    };
    let start = if whole_line { line_start } else { start };
    edits.push(Edit {
        span: start..end,
        text: String::new(),
    });
    if let Some(previous) = previous_comma {
        edits.push(Edit {
            span: previous..previous + 1,
            text: String::new(),
        });
    }
}

fn insert_object(
    text: &str,
    open: usize,
    close: usize,
    members: &[Member],
    added: &[(&String, &Value)],
    edits: &mut Vec<Edit>,
) -> Option<()> {
    let multiline = text[open..close].contains('\n');
    let base = line_indent(text, open).to_owned();
    let unit = members.first().map_or_else(
        || indent_unit(text, open),
        |member| indentation_step(text, open, member.start),
    );
    let child = members.first().map_or_else(
        || format!("{base}{unit}"),
        |member| line_indent(text, member.start).to_owned(),
    );
    if multiline {
        let insertion = closing_line_start(text, close)?;
        if members.last().is_some_and(|member| member.comma.is_none()) {
            let last = members.last()?;
            edits.push(Edit {
                span: last.value.span.end..last.value.span.end,
                text: ",".to_owned(),
            });
        }
        let mut text_out = String::new();
        for (key, value) in added {
            text_out.push_str(&child);
            text_out.push_str(&serde_json::to_string(key).ok()?);
            text_out.push_str(": ");
            text_out.push_str(&render(value, &child, &unit));
            text_out.push('\n');
        }
        edits.push(Edit {
            span: insertion..insertion,
            text: text_out,
        });
    } else {
        let body = added
            .iter()
            .map(|(key, value)| {
                Some(format!(
                    "{}: {}",
                    serde_json::to_string(key).ok()?,
                    render(value, "", &unit)
                ))
            })
            .collect::<Option<Vec<_>>>()?
            .join(", ");
        let prefix = if members.is_empty() {
            ""
        } else if members.last()?.comma.is_some() {
            " "
        } else {
            ", "
        };
        edits.push(Edit {
            span: close..close,
            text: format!("{prefix}{body}"),
        });
    }
    Some(())
}

fn insert_array(
    text: &str,
    open: usize,
    close: usize,
    items: &[Element],
    added: &[Value],
    edits: &mut Vec<Edit>,
) -> Option<()> {
    let multiline = text[open..close].contains('\n');
    let base = line_indent(text, open).to_owned();
    let unit = items.first().map_or_else(
        || indent_unit(text, open),
        |item| indentation_step(text, open, item.value.span.start),
    );
    let child = items.first().map_or_else(
        || format!("{base}{unit}"),
        |item| line_indent(text, item.value.span.start).to_owned(),
    );
    if multiline {
        let insertion = closing_line_start(text, close)?;
        if items.last().is_some_and(|item| item.comma.is_none()) {
            let last = items.last()?;
            edits.push(Edit {
                span: last.value.span.end..last.value.span.end,
                text: ",".to_owned(),
            });
        }
        let mut text_out = String::new();
        for value in added {
            text_out.push_str(&child);
            text_out.push_str(&render(value, &child, &unit));
            text_out.push('\n');
        }
        edits.push(Edit {
            span: insertion..insertion,
            text: text_out,
        });
    } else {
        let body = added
            .iter()
            .map(|value| render(value, "", &unit))
            .collect::<Vec<_>>()
            .join(", ");
        let prefix = if items.is_empty() {
            ""
        } else if items.last()?.comma.is_some() {
            " "
        } else {
            ", "
        };
        edits.push(Edit {
            span: close..close,
            text: format!("{prefix}{body}"),
        });
    }
    Some(())
}

fn apply(original: &str, mut edits: Vec<Edit>, after: &Value) -> String {
    edits.sort_by(|a, b| {
        b.span
            .start
            .cmp(&a.span.start)
            .then_with(|| b.span.end.cmp(&a.span.end))
    });
    let mut output = original.to_owned();
    let mut next_start = original.len();
    for edit in edits {
        if edit.span.end > next_start {
            return crate::edit::pretty_json(after).unwrap_or_else(|_| original.to_owned());
        }
        output.replace_range(edit.span.clone(), &edit.text);
        next_start = edit.span.start;
    }
    if crate::edit::json_root(&output).ok().as_ref() == Some(after) {
        output
    } else {
        crate::edit::pretty_json(after).unwrap_or(output)
    }
}

fn render(value: &Value, indent: &str, unit: &str) -> String {
    match value {
        Value::Array(items) if !items.is_empty() => {
            let child = format!("{indent}{unit}");
            let mut output = String::from("[\n");
            for (index, item) in items.iter().enumerate() {
                output.push_str(&child);
                output.push_str(&render(item, &child, unit));
                if index + 1 < items.len() {
                    output.push(',');
                }
                output.push('\n');
            }
            output.push_str(indent);
            output.push(']');
            output
        }
        Value::Object(object) if !object.is_empty() => {
            let child = format!("{indent}{unit}");
            let mut output = String::from("{\n");
            for (index, (key, item)) in object.iter().enumerate() {
                output.push_str(&child);
                output.push_str(&serde_json::to_string(key).expect("JSON key"));
                output.push_str(": ");
                output.push_str(&render(item, &child, unit));
                if index + 1 < object.len() {
                    output.push(',');
                }
                output.push('\n');
            }
            output.push_str(indent);
            output.push('}');
            output
        }
        _ => serde_json::to_string(value).expect("JSON value"),
    }
}

fn line_indent(text: &str, position: usize) -> &str {
    let start = text[..position]
        .rfind('\n')
        .map_or(0, |position| position + 1);
    let end = text[start..]
        .find(|character: char| character != ' ' && character != '\t')
        .map_or(position, |offset| start + offset);
    &text[start..end]
}

fn indentation_step(text: &str, open: usize, child: usize) -> String {
    let base = line_indent(text, open);
    line_indent(text, child)
        .strip_prefix(base)
        .filter(|step| !step.is_empty())
        .unwrap_or("  ")
        .to_owned()
}

fn indent_unit(text: &str, position: usize) -> String {
    if text[..position].contains("\n\t") || line_indent(text, position).contains('\t') {
        "\t".to_owned()
    } else {
        "  ".to_owned()
    }
}

fn closing_line_start(text: &str, close: usize) -> Option<usize> {
    let start = text[..close].rfind('\n').map_or(0, |position| position + 1);
    text[start..close].trim().is_empty().then_some(start)
}

struct Parser<'a> {
    text: &'a str,
    position: usize,
}

impl<'a> Parser<'a> {
    fn new(text: &'a str) -> Self {
        Self { text, position: 0 }
    }

    fn value(&mut self) -> Option<Node> {
        self.skip_layout().ok()?;
        let start = self.position;
        match self.peek()? {
            b'{' => self.object(),
            b'[' => self.array(),
            b'"' => {
                let (value, span) = self.string()?;
                Some(Node {
                    value: Value::String(value),
                    span,
                    kind: NodeKind::Scalar,
                })
            }
            _ => {
                while let Some(byte) = self.peek() {
                    if byte.is_ascii_whitespace() || matches!(byte, b',' | b']' | b'}' | b'/') {
                        break;
                    }
                    self.position += 1;
                }
                if self.position == start {
                    return None;
                }
                let value = serde_json::from_str(&self.text[start..self.position]).ok()?;
                Some(Node {
                    value,
                    span: start..self.position,
                    kind: NodeKind::Scalar,
                })
            }
        }
    }

    fn object(&mut self) -> Option<Node> {
        let open = self.position;
        self.position += 1;
        let mut members = Vec::new();
        let mut object = Map::new();
        self.skip_layout().ok()?;
        let close = if self.consume(b'}') {
            self.position - 1
        } else {
            loop {
                self.skip_layout().ok()?;
                let start = self.position;
                let (key, _) = self.string()?;
                if members.iter().any(|member: &Member| member.key == key) {
                    return None;
                }
                self.skip_layout().ok()?;
                if !self.consume(b':') {
                    return None;
                }
                let value = self.value()?;
                object.insert(key.clone(), value.value.clone());
                self.skip_layout().ok()?;
                let comma = if self.consume(b',') {
                    Some(self.position - 1)
                } else {
                    None
                };
                members.push(Member {
                    key,
                    start,
                    value,
                    comma,
                });
                self.skip_layout().ok()?;
                if self.consume(b'}') {
                    break self.position - 1;
                }
                comma?;
            }
        };
        Some(Node {
            value: Value::Object(object),
            span: open..close + 1,
            kind: NodeKind::Object {
                open,
                close,
                members,
            },
        })
    }

    fn array(&mut self) -> Option<Node> {
        let open = self.position;
        self.position += 1;
        let mut items = Vec::new();
        let mut values = Vec::new();
        self.skip_layout().ok()?;
        let close = if self.consume(b']') {
            self.position - 1
        } else {
            loop {
                let value = self.value()?;
                values.push(value.value.clone());
                self.skip_layout().ok()?;
                let comma = if self.consume(b',') {
                    Some(self.position - 1)
                } else {
                    None
                };
                items.push(Element { value, comma });
                self.skip_layout().ok()?;
                if self.consume(b']') {
                    break self.position - 1;
                }
                comma?;
            }
        };
        Some(Node {
            value: Value::Array(values),
            span: open..close + 1,
            kind: NodeKind::Array { open, close, items },
        })
    }

    fn string(&mut self) -> Option<(String, Range<usize>)> {
        let start = self.position;
        if !self.consume(b'"') {
            return None;
        }
        let mut escaped = false;
        while let Some(byte) = self.peek() {
            self.position += 1;
            if escaped {
                escaped = false;
            } else if byte == b'\\' {
                escaped = true;
            } else if byte == b'"' {
                let span = start..self.position;
                let value = serde_json::from_str(&self.text[span.clone()]).ok()?;
                return Some((value, span));
            }
        }
        None
    }

    fn skip_layout(&mut self) -> Result<(), ()> {
        loop {
            while self.peek().is_some_and(|byte| byte.is_ascii_whitespace()) {
                self.position += 1;
            }
            if self.text.as_bytes().get(self.position..self.position + 2) == Some(b"//") {
                self.position += 2;
                while self.peek().is_some_and(|byte| byte != b'\n') {
                    self.position += 1;
                }
                continue;
            }
            if self.text.as_bytes().get(self.position..self.position + 2) == Some(b"/*") {
                self.position += 2;
                let Some(end) = self.text[self.position..].find("*/") else {
                    return Err(());
                };
                self.position += end + 2;
                continue;
            }
            return Ok(());
        }
    }

    fn peek(&self) -> Option<u8> {
        self.text.as_bytes().get(self.position).copied()
    }

    fn consume(&mut self, byte: u8) -> bool {
        if self.peek() == Some(byte) {
            self.position += 1;
            true
        } else {
            false
        }
    }
}

#[cfg(test)]
mod tests {
    use serde_json::{Value, json};

    use super::{reconcile, verified};

    #[test]
    fn jsonc_edit_that_reads_back_wrong_falls_back() {
        let after = json!({"model": "new"});
        assert_eq!(
            verified("{\"model\": \"new\"}".to_owned(), &after).as_deref(),
            Some("{\"model\": \"new\"}")
        );
        assert_eq!(verified("{\"model\": \"old\"}".to_owned(), &after), None);
        assert_eq!(verified("{\"model\": ".to_owned(), &after), None);
    }

    fn edited(original: &str, before: Value, after: &Value) -> String {
        reconcile(original, &before, after).expect("comment-preserving edit")
    }

    #[test]
    fn jsonc_set_existing_keeps_comment_and_untouched_bytes() {
        let original = "{\n  // mine\n  \"model\": \"old\", // note\n  \"keep\":  1\n}\n";
        let before = crate::edit::json_root(original).expect("before");
        let mut after = before.clone();
        after["model"] = json!("new");
        assert_eq!(
            edited(original, before, &after),
            "{\n  // mine\n  \"model\": \"new\", // note\n  \"keep\":  1\n}\n"
        );
    }

    #[test]
    fn jsonc_insert_missing_nested_keeps_sibling_comment() {
        let original =
            "{\n  // outer\n  \"agent\": {\n    // inner\n    \"theme\": \"dark\"\n  }\n}\n";
        let before = crate::edit::json_root(original).expect("before");
        let mut after = before.clone();
        after["agent"]["model"] = json!({"name": "route-a"});
        let output = edited(original, before, &after);
        assert_eq!(crate::edit::json_root(&output).expect("after"), after);
        assert!(output.contains("// outer"));
        assert!(output.contains("// inner"));
        assert!(output.contains("    \"model\": {\n      \"name\": \"route-a\"\n    }"));
    }

    #[test]
    fn jsonc_remove_key_keeps_neighbour_comments() {
        let original =
            "{\n  // first\n  \"a\": 1,\n  // second\n  \"b\": 2,\n  // third\n  \"c\": 3\n}\n";
        let before = crate::edit::json_root(original).expect("before");
        let mut after = before.clone();
        after.as_object_mut().expect("object").remove("b");
        assert_eq!(
            edited(original, before, &after),
            "{\n  // first\n  \"a\": 1,\n  // second\n  // third\n  \"c\": 3\n}\n"
        );
    }

    #[test]
    fn jsonc_trailing_comma_survives_set_and_insert() {
        let original = "{\n  // mine\n  \"a\": 1,\n}\n";
        let before = crate::edit::json_root(original).expect("before");
        let mut after = before.clone();
        after["a"] = json!(2);
        after["b"] = json!(3);
        let output = edited(original, before, &after);
        assert!(output.contains("\"a\": 2,"));
        assert!(output.contains("\"b\": 3"));
        assert!(output.contains("// mine"));
        assert_eq!(crate::edit::json_root(&output).expect("after"), after);
    }

    #[test]
    fn jsonc_block_comment_survives_edit() {
        let original = "{ /* keep */ \"a\": 1, \"b\": 2 }";
        let before = crate::edit::json_root(original).expect("before");
        let mut after = before.clone();
        after["b"] = json!(3);
        assert_eq!(
            edited(original, before, &after),
            "{ /* keep */ \"a\": 1, \"b\": 3 }"
        );
    }

    #[test]
    fn jsonc_string_containing_slashes_is_not_a_comment() {
        let original = "{\n  \"url\": \"https://example.test//keep\", // real\n  \"a\": 1\n}\n";
        let before = crate::edit::json_root(original).expect("before");
        let mut after = before.clone();
        after["a"] = json!(2);
        let output = edited(original, before, &after);
        assert!(output.contains("https://example.test//keep"));
        assert!(output.contains("// real"));
    }
}
