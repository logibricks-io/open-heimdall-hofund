use std::ops::Range;

use yaml_rust2::{Yaml, yaml::Hash};

#[derive(Clone)]
struct Edit {
    span: Range<usize>,
    text: String,
}

#[derive(Clone)]
struct Line {
    start: usize,
    content_end: usize,
    end: usize,
}

struct Node {
    value: Yaml,
    span: Range<usize>,
    indent: usize,
    inline: bool,
    sequence_item: bool,
    kind: NodeKind,
}

enum NodeKind {
    Scalar,
    Mapping { entries: Vec<Entry>, end: usize },
    Sequence { items: Vec<Node>, end: usize },
    Unsupported,
}

struct Entry {
    key: String,
    start: usize,
    value: Node,
}

pub(crate) fn reconcile(original: &str, before: &Yaml, after: &Yaml) -> Option<String> {
    if before == after {
        return Some(original.to_owned());
    }
    let lines = scan_lines(original);
    let Some(first) = next_content(original, &lines, 0) else {
        return insert_into_empty(original, after);
    };
    if original[lines[first].start..].starts_with("---") {
        return None;
    }
    let indent = indentation(original, &lines[first])?;
    let (node, _) = parse_node(original, &lines, first, indent, before.clone(), false)?;
    let mut edits = Vec::new();
    diff(original, &node, before, after, &mut edits)?;
    verified(apply(original, edits, after), after)
}

/// Keeps an in-place edit only when it reads back as the intended value, so a
/// scanner gap falls back to the full render instead of corrupting the file.
fn verified(text: String, after: &Yaml) -> Option<String> {
    (crate::edit::yaml_root(&text).ok()? == *after).then_some(text)
}

fn insert_into_empty(original: &str, after: &Yaml) -> Option<String> {
    let newline = newline(original);
    let mut output = original.to_owned();
    if !output.is_empty() && !output.ends_with('\n') {
        output.push_str(newline);
    }
    output.push_str(&crate::edit::emit_yaml(after).ok()?.replace('\n', newline));
    Some(output)
}

fn diff(text: &str, node: &Node, before: &Yaml, after: &Yaml, edits: &mut Vec<Edit>) -> Option<()> {
    if before == after {
        return Some(());
    }
    if &node.value != before {
        return None;
    }
    match (&node.kind, before, after) {
        (NodeKind::Mapping { entries, end }, Yaml::Hash(old), Yaml::Hash(new)) => {
            for entry in entries {
                let key = crate::edit::yaml_key(&entry.key);
                match new.get(&key) {
                    Some(value) => diff(text, &entry.value, old.get(&key)?, value, edits)?,
                    None => remove_mapping_entry(text, entry, edits),
                }
            }
            let added: Hash = new
                .iter()
                .filter(|(key, _)| !old.contains_key(*key))
                .map(|(key, value)| (key.clone(), value.clone()))
                .collect();
            if !added.is_empty() {
                let rendered = crate::edit::emit_yaml(&Yaml::Hash(added)).ok()?;
                edits.push(Edit {
                    span: *end..*end,
                    text: indent_block(&rendered, node.indent, newline(text)),
                });
            }
            Some(())
        }
        (NodeKind::Sequence { items, end }, Yaml::Array(old), Yaml::Array(new)) => {
            for (index, (old_value, new_value)) in old.iter().zip(new).enumerate() {
                diff(text, &items[index], old_value, new_value, edits)?;
            }
            for item in items.iter().skip(new.len()) {
                edits.push(Edit {
                    span: item.span.clone(),
                    text: String::new(),
                });
            }
            if new.len() > old.len() {
                let rendered =
                    crate::edit::emit_yaml(&Yaml::Array(new[old.len()..].to_vec())).ok()?;
                edits.push(Edit {
                    span: *end..*end,
                    text: indent_block(&rendered, node.indent, newline(text)),
                });
            }
            Some(())
        }
        (NodeKind::Unsupported, _, _) => None,
        _ => {
            let rendered = if node.sequence_item {
                render_sequence_item(after, node.indent, newline(text))?
            } else if node.inline {
                match after {
                    Yaml::Hash(hash) if !hash.is_empty() => {
                        let rendered = crate::edit::emit_yaml(after).ok()?;
                        format!(
                            "{}{}",
                            newline(text),
                            indent_block(&rendered, node.indent + 2, newline(text))
                                .trim_end_matches(newline(text))
                        )
                    }
                    Yaml::Array(items) if !items.is_empty() => {
                        let rendered = crate::edit::emit_yaml(after).ok()?;
                        format!(
                            "{}{}",
                            newline(text),
                            indent_block(&rendered, node.indent + 2, newline(text))
                                .trim_end_matches(newline(text))
                        )
                    }
                    _ => crate::edit::emit_yaml(after).ok()?.trim().to_owned(),
                }
            } else {
                indent_block(
                    &crate::edit::emit_yaml(after).ok()?,
                    node.indent,
                    newline(text),
                )
            };
            edits.push(Edit {
                span: node.span.clone(),
                text: rendered,
            });
            Some(())
        }
    }
}

fn remove_mapping_entry(text: &str, entry: &Entry, edits: &mut Vec<Edit>) {
    let start = text[..entry.start]
        .rfind('\n')
        .map_or(0, |position| position + 1);
    let end = if entry.value.span.end > entry.value.span.start
        && text[..entry.value.span.end].ends_with('\n')
    {
        entry.value.span.end
    } else {
        text[entry.value.span.end..]
            .find('\n')
            .map_or(text.len(), |position| entry.value.span.end + position + 1)
    };
    edits.push(Edit {
        span: start..end,
        text: String::new(),
    });
}

fn apply(original: &str, mut edits: Vec<Edit>, after: &Yaml) -> String {
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
            return crate::edit::emit_yaml(after).unwrap_or_else(|_| original.to_owned());
        }
        output.replace_range(edit.span.clone(), &edit.text);
        next_start = edit.span.start;
    }
    if crate::edit::yaml_root(&output).ok().as_ref() == Some(after) {
        output
    } else {
        crate::edit::emit_yaml(after).unwrap_or(output)
    }
}

fn parse_node(
    text: &str,
    lines: &[Line],
    index: usize,
    indent: usize,
    value: Yaml,
    sequence_item: bool,
) -> Option<(Node, usize)> {
    match value {
        Yaml::Hash(hash) => parse_mapping(text, lines, index, indent, hash, sequence_item),
        Yaml::Array(items) => parse_sequence(text, lines, index, indent, items, sequence_item),
        value => Some((
            Node {
                span: lines[index].start..lines[index].end,
                value,
                indent,
                inline: false,
                sequence_item,
                kind: NodeKind::Scalar,
            },
            index + 1,
        )),
    }
}

fn parse_mapping(
    text: &str,
    lines: &[Line],
    mut index: usize,
    indent: usize,
    hash: Hash,
    sequence_item: bool,
) -> Option<(Node, usize)> {
    let start = lines[index].start;
    let mut entries = Vec::new();
    let mut end = lines[index].end;
    while index < lines.len() {
        let line = &lines[index];
        let content = &text[line.start..line.content_end];
        if content.trim().is_empty() || content.trim_start().starts_with('#') {
            end = line.end;
            index += 1;
            continue;
        }
        let line_indent = indentation(text, line)?;
        if line_indent < indent {
            break;
        }
        if line_indent > indent || content[line_indent..].starts_with('-') {
            return None;
        }
        let (raw_key, colon) = mapping_key(content, line_indent)?;
        let key = parse_key(raw_key)?;
        if entries.iter().any(|entry: &Entry| entry.key == key) {
            return None;
        }
        let value = hash.get(&crate::edit::yaml_key(&key))?.clone();
        let value_offset = colon + 1;
        let (value_text, value_start, value_end) = inline_value(text, line, value_offset);
        let start = line.start + indent;
        index += 1;
        let value_node = if value_text.is_empty() {
            if let Some(child) = next_content(text, lines, index)
                && indentation(text, &lines[child])? > indent
                && !matches!(value, Yaml::Null | Yaml::BadValue)
            {
                let child_indent = indentation(text, &lines[child])?;
                let (node, next) = parse_node(text, lines, child, child_indent, value, false)?;
                index = next;
                node
            } else {
                Node {
                    value,
                    span: line.start + value_start..line.start + value_start,
                    indent,
                    inline: true,
                    sequence_item: false,
                    kind: NodeKind::Scalar,
                }
            }
        } else {
            let unsupported = value_text.starts_with(['{', '[', '&', '*', '!', '|', '>']);
            Node {
                value,
                span: line.start + value_start..line.start + value_end,
                indent,
                inline: true,
                sequence_item: false,
                kind: if unsupported {
                    NodeKind::Unsupported
                } else {
                    NodeKind::Scalar
                },
            }
        };
        end = value_node.span.end.max(line.end);
        entries.push(Entry {
            key,
            start,
            value: value_node,
        });
    }
    Some((
        Node {
            value: Yaml::Hash(hash),
            span: start..end,
            indent,
            inline: false,
            sequence_item,
            kind: NodeKind::Mapping { entries, end },
        },
        index,
    ))
}

fn parse_sequence(
    text: &str,
    lines: &[Line],
    mut index: usize,
    indent: usize,
    items: Vec<Yaml>,
    sequence_item: bool,
) -> Option<(Node, usize)> {
    let start = lines[index].start;
    let mut nodes = Vec::new();
    let mut end = lines[index].end;
    while index < lines.len() {
        let line = &lines[index];
        let content = &text[line.start..line.content_end];
        if content.trim().is_empty() || content.trim_start().starts_with('#') {
            end = line.end;
            index += 1;
            continue;
        }
        let line_indent = indentation(text, line)?;
        if line_indent < indent {
            break;
        }
        if line_indent != indent || !content[line_indent..].starts_with('-') {
            return None;
        }
        let value = items.get(nodes.len())?.clone();
        let start = line.start;
        index += 1;
        while index < lines.len() {
            let nested = &lines[index];
            let nested_text = &text[nested.start..nested.content_end];
            if !nested_text.trim().is_empty()
                && !nested_text.trim_start().starts_with('#')
                && indentation(text, nested)? <= indent
            {
                break;
            }
            index += 1;
        }
        end = lines[index - 1].end;
        let inline = &content[line_indent + 1..];
        let unsupported = inline.trim_start().starts_with(['{', '[', '&', '*', '!']);
        nodes.push(Node {
            value,
            span: start..end,
            indent,
            inline: false,
            sequence_item: true,
            kind: if unsupported {
                NodeKind::Unsupported
            } else {
                NodeKind::Scalar
            },
        });
    }
    if nodes.len() != items.len() {
        return None;
    }
    Some((
        Node {
            value: Yaml::Array(items),
            span: start..end,
            indent,
            inline: false,
            sequence_item,
            kind: NodeKind::Sequence { items: nodes, end },
        },
        index,
    ))
}

fn mapping_key(content: &str, indent: usize) -> Option<(&str, usize)> {
    let bytes = content.as_bytes();
    let mut single = false;
    let mut double = false;
    let mut escaped = false;
    for (index, byte) in bytes.iter().copied().enumerate().skip(indent) {
        if escaped {
            escaped = false;
            continue;
        }
        match byte {
            b'\\' if double => escaped = true,
            b'\'' if !double => single = !single,
            b'"' if !single => double = !double,
            b':' if !single
                && !double
                && bytes.get(index + 1).is_none_or(u8::is_ascii_whitespace) =>
            {
                return Some((content[indent..index].trim(), index));
            }
            _ => {}
        }
    }
    None
}

fn parse_key(raw: &str) -> Option<String> {
    if raw.is_empty() || raw.starts_with(['?', '&', '*', '!']) {
        return None;
    }
    if let Some(quoted) = raw.strip_prefix('"') {
        return serde_json::from_str(quoted).ok();
    }
    if let Some(quoted) = raw.strip_prefix('\'') {
        return Some(quoted.strip_suffix('\'')?.replace("''", "'"));
    }
    (!raw.contains('#')).then(|| raw.to_owned())
}

fn inline_value<'a>(text: &'a str, line: &Line, offset: usize) -> (&'a str, usize, usize) {
    let content = &text[line.start..line.content_end];
    let mut single = false;
    let mut double = false;
    let mut escaped = false;
    let mut comment = content.len();
    for (index, byte) in content.as_bytes().iter().copied().enumerate().skip(offset) {
        if escaped {
            escaped = false;
            continue;
        }
        match byte {
            b'\\' if double => escaped = true,
            b'\'' if !double => single = !single,
            b'"' if !single => double = !double,
            b'#' if !single
                && !double
                && (index == offset || content.as_bytes()[index - 1].is_ascii_whitespace()) =>
            {
                comment = index;
                break;
            }
            _ => {}
        }
    }
    let raw = &content[offset..comment];
    let leading = raw.len() - raw.trim_start().len();
    let trailing = raw.len() - raw.trim_end().len();
    (raw.trim(), offset + leading, comment - trailing)
}

fn scan_lines(text: &str) -> Vec<Line> {
    let mut lines = Vec::new();
    let mut start = 0;
    for (index, byte) in text.bytes().enumerate() {
        if byte == b'\n' {
            lines.push(Line {
                start,
                content_end: if index > start && text.as_bytes()[index - 1] == b'\r' {
                    index - 1
                } else {
                    index
                },
                end: index + 1,
            });
            start = index + 1;
        }
    }
    if start < text.len() {
        lines.push(Line {
            start,
            content_end: text.len(),
            end: text.len(),
        });
    }
    lines
}

fn next_content(text: &str, lines: &[Line], mut index: usize) -> Option<usize> {
    while index < lines.len() {
        let content = &text[lines[index].start..lines[index].content_end];
        if !content.trim().is_empty() && !content.trim_start().starts_with('#') {
            return Some(index);
        }
        index += 1;
    }
    None
}

fn indentation(text: &str, line: &Line) -> Option<usize> {
    let content = &text[line.start..line.content_end];
    let indent = content.len() - content.trim_start_matches(' ').len();
    (content.as_bytes().get(indent) != Some(&b'\t')).then_some(indent)
}

fn newline(text: &str) -> &'static str {
    if text.contains("\r\n") { "\r\n" } else { "\n" }
}

fn indent_block(rendered: &str, indent: usize, newline: &str) -> String {
    let prefix = " ".repeat(indent);
    let mut output = rendered
        .lines()
        .map(|line| format!("{prefix}{line}"))
        .collect::<Vec<_>>()
        .join(newline);
    output.push_str(newline);
    output
}

fn render_sequence_item(value: &Yaml, indent: usize, newline: &str) -> Option<String> {
    let rendered = crate::edit::emit_yaml(value).ok()?;
    let mut lines = rendered.lines();
    let first = lines.next()?;
    let prefix = " ".repeat(indent);
    let mut output = format!("{prefix}- {first}{newline}");
    for line in lines {
        output.push_str(&format!("{prefix}  {line}{newline}"));
    }
    Some(output)
}

#[cfg(test)]
mod tests {
    use yaml_rust2::Yaml;

    use super::{reconcile, verified};

    #[test]
    fn yaml_edit_that_reads_back_wrong_falls_back() {
        let after = before("model: new\n");
        assert_eq!(
            verified("# keep\nmodel: new\n".to_owned(), &after).as_deref(),
            Some("# keep\nmodel: new\n")
        );
        assert_eq!(verified("model: old\n".to_owned(), &after), None);
        assert_eq!(verified("model: [\n".to_owned(), &after), None);
    }

    fn before(text: &str) -> Yaml {
        crate::edit::yaml_root(text).expect("before")
    }

    #[test]
    fn yaml_set_keeps_inline_comment() {
        let original = "# top\nmodel: old # keep\nother:  1\n";
        let before = before(original);
        let mut after = before.clone();
        crate::edit::set_yaml(&mut after, &["model"], Yaml::String("new".to_owned())).expect("set");
        assert_eq!(
            reconcile(original, &before, &after).expect("edit"),
            "# top\nmodel: new # keep\nother:  1\n"
        );
    }

    #[test]
    fn yaml_insert_missing_nested_key() {
        let original = "model:\n  # mine\n  provider: old\nkeep: yes\n";
        let before = before(original);
        let mut after = before.clone();
        crate::edit::set_yaml(
            &mut after,
            &["model", "default"],
            Yaml::String("m".to_owned()),
        )
        .expect("set");
        let output = reconcile(original, &before, &after).expect("edit");
        assert_eq!(crate::edit::yaml_root(&output).expect("after"), after);
        assert!(output.contains("  # mine\n"));
        assert!(output.contains("  default: m\n"));
        assert!(output.ends_with("keep: yes\n"));
    }

    #[test]
    fn yaml_remove_nested_block_keeps_other_lines() {
        let original = "# top\nproviders:\n  # mine\n  hofund:\n    url: old\n    models:\n      - a\n  mine:\n    url: keep\n";
        let before = before(original);
        let mut after = before.clone();
        crate::edit::unset_yaml(&mut after, &["providers", "hofund"]).expect("remove");
        assert_eq!(
            reconcile(original, &before, &after).expect("edit"),
            "# top\nproviders:\n  # mine\n  mine:\n    url: keep\n"
        );
    }

    #[test]
    fn yaml_comment_is_kept_when_parent_is_inserted() {
        let original = "# my setup\nmodel:\n  provider: old\n";
        let before = before(original);
        let mut after = before.clone();
        crate::edit::set_yaml(
            &mut after,
            &["model", "default"],
            Yaml::String("m".to_owned()),
        )
        .expect("set");
        let output = reconcile(original, &before, &after).expect("edit");
        assert!(output.starts_with("# my setup\nmodel:\n"));
    }

    #[test]
    fn yaml_refresh_in_a_sequence_keeps_the_leading_comment() {
        let original = "# keep\ngroups:\n  - name: hofund\n    models:\n      - id: route-a\n        key: old-token\n";
        let before = before(original);
        let mut after = before.clone();
        let Yaml::Hash(root) = &mut after else {
            panic!("mapping")
        };
        let Some(Yaml::Array(groups)) = root.get_mut(&crate::edit::yaml_key("groups")) else {
            panic!("groups")
        };
        let Yaml::Hash(group) = &mut groups[0] else {
            panic!("group")
        };
        let Some(Yaml::Array(models)) = group.get_mut(&crate::edit::yaml_key("models")) else {
            panic!("models")
        };
        let Yaml::Hash(model) = &mut models[0] else {
            panic!("model")
        };
        model.insert(
            crate::edit::yaml_key("key"),
            Yaml::String("new-token".to_owned()),
        );
        let output = reconcile(original, &before, &after).expect("edit");
        assert!(output.starts_with("# keep\n"), "{output}");
        assert!(output.contains("key: new-token"));
        assert_eq!(crate::edit::yaml_root(&output).expect("after"), after);
    }

    #[test]
    fn yaml_fallback_on_flow_map() {
        let original = "model: { provider: old }\n";
        let before = before(original);
        let mut after = before.clone();
        crate::edit::set_yaml(
            &mut after,
            &["model", "provider"],
            Yaml::String("new".to_owned()),
        )
        .expect("set");
        assert!(reconcile(original, &before, &after).is_none());
    }
}
