//! Offsetless Responses citations in relayed replies and Codex replay input.
//!
//! Some Responses upstreams write `url_citation` or `container_file_citation`
//! annotations without `start_index` and `end_index`. Clients store them and
//! later replay fails where OpenAI's schema requires both offsets. Drop those
//! annotations on relay and heal stored history on the way to the ChatGPT
//! backend.

use serde_json::Value;

const ANNOTATIONS: &[u8] = b"\"annotations\"";

/// Whether `annotation` lacks both offsets required for replay.
#[must_use]
pub fn annotation_lacks_offsets(annotation: &Value) -> bool {
    let Some(kind) = annotation.get("type").and_then(Value::as_str) else {
        return false;
    };
    if kind != "url_citation" && kind != "container_file_citation" {
        return false;
    }
    lacks_index(annotation, "start_index") || lacks_index(annotation, "end_index")
}

fn lacks_index(annotation: &Value, key: &str) -> bool {
    match annotation.get(key) {
        None | Some(Value::Null) => true,
        Some(Value::Number(_)) => false,
        _ => true,
    }
}

/// Drops offsetless citations from one item's `output_text` parts; reports change.
pub fn drop_offsetless_citations(item: &mut Value) -> bool {
    let Some(parts) = item.get_mut("content").and_then(Value::as_array_mut) else {
        return false;
    };
    let mut changed = false;
    for part in parts {
        if part.get("type").and_then(Value::as_str) != Some("output_text") {
            continue;
        }
        let Some(notes) = part.get_mut("annotations").and_then(Value::as_array_mut) else {
            continue;
        };
        let before = notes.len();
        notes.retain(|note| !annotation_lacks_offsets(note));
        changed |= notes.len() != before;
    }
    changed
}

/// Tidies one SSE `data:` JSON value for relay; `None` when unchanged.
pub fn tidy_event(data: &[u8]) -> Option<Vec<u8>> {
    let mut value: Value = serde_json::from_slice(data).ok()?;
    let event = value.as_object_mut()?;
    let kind = event
        .get("type")
        .and_then(Value::as_str)
        .unwrap_or_default();
    if kind == "response.output_text.annotation.added" {
        let annotation = event.get("annotation").unwrap_or(&Value::Null);
        if annotation_lacks_offsets(annotation) {
            return Some(Vec::new());
        }
        return None;
    }
    let mut changed = event.get_mut("item").is_some_and(drop_offsetless_citations);
    let response = if event.get("response").is_some_and(Value::is_object) {
        event.get_mut("response").and_then(Value::as_object_mut)
    } else if event.get("object").and_then(Value::as_str) == Some("response") {
        Some(&mut *event)
    } else {
        None
    };
    if let Some(Value::Array(output)) = response.and_then(|response| response.get_mut("output")) {
        for item in output {
            changed |= drop_offsetless_citations(item);
        }
    }
    if !changed {
        return None;
    }
    serde_json::to_vec(&value).ok()
}

/// A whole relayed reply with offsetless citations removed.
#[must_use]
pub fn tidy_whole(bytes: Vec<u8>) -> Vec<u8> {
    if !contains(bytes.as_slice(), ANNOTATIONS) {
        return bytes;
    }
    tidy_event(&bytes).unwrap_or(bytes)
}

/// Whether an input item byte slice may carry an offsetless output citation.
#[must_use]
pub fn item_cites_without_offsets(item: &[u8]) -> bool {
    if !contains(item, ANNOTATIONS) {
        return false;
    }
    let Ok(value) = serde_json::from_slice::<Value>(item) else {
        return false;
    };
    value
        .get("content")
        .and_then(Value::as_array)
        .into_iter()
        .flatten()
        .any(|part| {
            part.get("type").and_then(Value::as_str) == Some("output_text")
                && part
                    .get("annotations")
                    .and_then(Value::as_array)
                    .is_some_and(|notes| notes.iter().any(annotation_lacks_offsets))
        })
}

fn contains(haystack: &[u8], needle: &[u8]) -> bool {
    haystack
        .windows(needle.len())
        .any(|window| window == needle)
}
