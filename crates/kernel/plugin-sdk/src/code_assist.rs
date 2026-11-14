//! Code Assist-only request history and thought-signature rules.

use std::collections::HashSet;

use serde_json::{Map, Value, json};

const SKIP_SIGNATURE: &str = "skip_thought_signature_validator";
const INTERRUPTED: &str =
    "[The result of this tool call is unavailable: the turn was interrupted.]";
const FIRST_TURN: &str = "[Continuing from previous AI thoughts...]";

/// Applies Code Assist's history rules to an encoded Generative Language body.
///
/// `model` selects Antigravity's Claude-specific tool-result shape when
/// `antigravity` is true. Generic Generative Language requests are unchanged.
pub fn shape_code_assist(body: &mut Map<String, Value>, model: &str, antigravity: bool) {
    let compacting = body
        .get("systemInstruction")
        .and_then(|system| system.get("parts"))
        .and_then(Value::as_array)
        .and_then(|parts| parts.first())
        .and_then(|part| part.get("text"))
        .and_then(Value::as_str)
        .is_some_and(|text| text.contains("summarizing conversations"));
    let Some(contents) = body.get_mut("contents").and_then(Value::as_array_mut) else {
        return;
    };
    let claude = antigravity && model.to_ascii_lowercase().contains("claude");
    sanitize(contents, antigravity, claude);
    drop_second_answers(contents);
    let mut turns = split_tool_turns(std::mem::take(contents), antigravity, claude);
    pair_function_turns(&mut turns, antigravity, claude);
    merge_adjacent(&mut turns);
    if turns.first().and_then(role) == Some("model") {
        turns.insert(0, text_turn(FIRST_TURN));
    }
    if turns.last().and_then(role) == Some("model") {
        let text = if compacting {
            "Write the summary of the conversation above that the system prompt asks for."
        } else {
            "Continue."
        };
        turns.push(text_turn(text));
    }
    *contents = turns;
    if !antigravity && let Some(tools) = body.get_mut("tools").and_then(Value::as_array_mut) {
        for declaration in tools
            .iter_mut()
            .filter_map(|tool| tool.get_mut("functionDeclarations"))
            .filter_map(Value::as_array_mut)
            .flatten()
            .filter_map(Value::as_object_mut)
        {
            if let Some(parameters) = declaration.remove("parameters") {
                declaration
                    .entry("parametersJsonSchema")
                    .or_insert(parameters);
            }
        }
    }
    if body
        .get("tools")
        .and_then(Value::as_array)
        .is_some_and(|tools| !tools.is_empty())
    {
        let config = body.entry("toolConfig").or_insert_with(|| json!({}));
        if !config.is_object() {
            *config = json!({});
        }
        if let Some(config) = config.as_object_mut() {
            let function = config
                .entry("functionCallingConfig")
                .or_insert_with(|| json!({}));
            if !function.is_object() {
                *function = json!({});
            }
            if let Some(function) = function.as_object_mut() {
                function
                    .entry("mode")
                    .or_insert_with(|| Value::String("AUTO".to_owned()));
            }
        }
    }
    if claude
        && let Some(config) = body
            .get_mut("toolConfig")
            .and_then(|config| config.get_mut("functionCallingConfig"))
            .and_then(Value::as_object_mut)
        && config.get("mode").and_then(Value::as_str) == Some("AUTO")
    {
        config.insert("mode".to_owned(), Value::String("VALIDATED".to_owned()));
    }
}

fn sanitize(contents: &mut [Value], antigravity: bool, claude: bool) {
    for turn in contents {
        let Some(parts) = turn.get_mut("parts").and_then(Value::as_array_mut) else {
            continue;
        };
        let signed = parts.iter().any(|part| {
            part.get("functionCall").is_some()
                && part
                    .get("thoughtSignature")
                    .and_then(Value::as_str)
                    .is_some_and(|signature| !signature.is_empty())
        });
        for part in parts.iter_mut() {
            if let Some(call) = part.get_mut("functionCall").and_then(Value::as_object_mut) {
                if antigravity && let Some(id) = call.get("id").and_then(Value::as_str) {
                    call.insert("id".to_owned(), Value::String(safe_id(id)));
                }
                let skip = claude || (part.get("thoughtSignature").is_none() && !signed);
                if skip && let Some(part) = part.as_object_mut() {
                    part.insert(
                        "thoughtSignature".to_owned(),
                        Value::String(SKIP_SIGNATURE.to_owned()),
                    );
                }
            } else if claude && let Some(part) = part.as_object_mut() {
                part.remove("thoughtSignature");
            }
            if let Some(response) = part
                .get_mut("functionResponse")
                .and_then(Value::as_object_mut)
            {
                if antigravity && let Some(id) = response.get("id").and_then(Value::as_str) {
                    response.insert("id".to_owned(), Value::String(safe_id(id)));
                }
                if let Some(body) = response.get_mut("response").and_then(Value::as_object_mut) {
                    if antigravity {
                        for old in ["output", "error"] {
                            if let Some(value) = body.remove(old) {
                                body.insert("result".to_owned(), value);
                            }
                        }
                    } else if let Some(value) = body.remove("result") {
                        body.insert("output".to_owned(), value);
                    }
                }
            }
        }
        parts.retain(|part| {
            part.get("thought").and_then(Value::as_bool) != Some(true)
                || part.get("functionCall").is_some()
        });
    }
}

fn safe_id(id: &str) -> String {
    id.chars()
        .map(|character| {
            if character.is_ascii_alphanumeric() || matches!(character, '_' | '-') {
                character
            } else {
                '_'
            }
        })
        .collect()
}

fn drop_second_answers(contents: &mut [Value]) {
    let mut answered = HashSet::new();
    for turn in contents {
        let user = turn.get("role").and_then(Value::as_str) == Some("user");
        let Some(parts) = turn.get_mut("parts").and_then(Value::as_array_mut) else {
            continue;
        };
        let mut drop_images = false;
        parts.retain(|part| {
            if let Some(call) = part.get("functionCall") {
                drop_images = false;
                if let Some(id) = call.get("id").and_then(Value::as_str)
                    && !id.is_empty()
                {
                    answered.remove(id);
                }
                return true;
            }
            if let Some(response) = part.get("functionResponse") {
                let id = response
                    .get("id")
                    .and_then(Value::as_str)
                    .unwrap_or_default();
                if user && !id.is_empty() {
                    if answered.contains(id) {
                        drop_images = true;
                        return false;
                    }
                    answered.insert(id.to_owned());
                }
                drop_images = false;
                return true;
            }
            if drop_images && is_attachment(part) {
                return false;
            }
            drop_images = false;
            true
        });
    }
}

fn split_tool_turns(contents: Vec<Value>, antigravity: bool, claude: bool) -> Vec<Value> {
    let mut out = Vec::new();
    for mut turn in contents {
        if role(&turn) != Some("user") || !has_response(&turn) {
            out.push(turn);
            continue;
        }
        let parts = turn
            .get_mut("parts")
            .and_then(Value::as_array_mut)
            .map(std::mem::take)
            .unwrap_or_default();
        if antigravity && claude {
            reorder_claude_images(&mut turn, parts);
            out.push(turn);
            continue;
        }
        let (tools, said): (Vec<_>, Vec<_>) = parts
            .into_iter()
            .partition(|part| part.get("functionResponse").is_some() || is_attachment(part));
        set_parts(&mut turn, tools);
        out.push(turn);
        if !said.is_empty() {
            let mut next = json!({"role": "user"});
            set_parts(&mut next, said);
            out.push(next);
        }
    }
    out
}

fn reorder_claude_images(turn: &mut Value, parts: Vec<Value>) {
    let mut responses = Vec::new();
    let mut rest = Vec::new();
    let mut images: Vec<(String, String, Vec<Value>)> = Vec::new();
    for part in parts {
        if let Some(response) = part.get("functionResponse") {
            let name = response
                .get("name")
                .and_then(Value::as_str)
                .unwrap_or("tool")
                .to_owned();
            let id = response
                .get("id")
                .and_then(Value::as_str)
                .unwrap_or_default()
                .to_owned();
            responses.push(part);
            images.push((name, id, Vec::new()));
        } else if is_attachment(&part)
            && let Some((_, _, current)) = images.last_mut()
        {
            current.push(part);
        } else {
            rest.push(part);
        }
    }
    let mut ordered = responses;
    for (name, id, returned) in images {
        if returned.is_empty() {
            continue;
        }
        let label = if returned.len() == 1 {
            format!("Image returned by tool {name} (call {id}):")
        } else {
            format!(
                "{} images returned by tool {name} (call {id}):",
                returned.len()
            )
        };
        ordered.push(json!({"text": label}));
        ordered.extend(returned);
    }
    ordered.extend(rest);
    set_parts(turn, ordered);
}

fn pair_function_turns(contents: &mut Vec<Value>, antigravity: bool, claude: bool) {
    let mut answered_ids = HashSet::new();
    let mut answered_names = HashSet::new();
    for turn in contents.iter() {
        for part in parts(turn) {
            if let Some(response) = part.get("functionResponse") {
                let id = response
                    .get("id")
                    .and_then(Value::as_str)
                    .unwrap_or_default();
                if id.is_empty() {
                    if let Some(name) = response.get("name").and_then(Value::as_str) {
                        answered_names.insert(name.to_owned());
                    }
                } else {
                    answered_ids.insert(id.to_owned());
                }
            }
        }
    }
    let key = if antigravity { "result" } else { "output" };
    let mut index = 0;
    while index < contents.len() {
        let mut synthetic = Vec::new();
        if role(&contents[index]) == Some("model") {
            for part in parts(&contents[index]) {
                let Some(call) = part.get("functionCall") else {
                    continue;
                };
                let id = call.get("id").and_then(Value::as_str).unwrap_or_default();
                let name = call
                    .get("name")
                    .and_then(Value::as_str)
                    .unwrap_or_default()
                    .to_owned();
                if (!id.is_empty() && answered_ids.contains(id))
                    || (id.is_empty() && answered_names.contains(&name))
                {
                    continue;
                }
                let mut response = json!({
                    "name": name,
                    "response": {(key): INTERRUPTED},
                });
                if !id.is_empty() {
                    response["id"] = Value::String(id.to_owned());
                }
                synthetic.push(json!({"functionResponse": response}));
            }
        }
        if !synthetic.is_empty() {
            if index + 1 < contents.len()
                && role(&contents[index + 1]) == Some("user")
                && (claude || has_response(&contents[index + 1]))
            {
                if let Some(parts) = contents[index + 1]
                    .get_mut("parts")
                    .and_then(Value::as_array_mut)
                {
                    parts.splice(0..0, synthetic);
                }
            } else {
                let mut turn = json!({"role": "user"});
                set_parts(&mut turn, synthetic);
                contents.insert(index + 1, turn);
            }
        }
        index += 1;
    }
}

fn merge_adjacent(contents: &mut Vec<Value>) {
    let mut merged: Vec<Value> = Vec::new();
    for turn in std::mem::take(contents) {
        let same = merged.last().is_some_and(|previous| {
            role(previous) == role(&turn)
                && (role(&turn) != Some("user") || has_response(previous) == has_response(&turn))
        });
        if same {
            if let Some(previous) = merged.last_mut() {
                let additions = turn
                    .get("parts")
                    .and_then(Value::as_array)
                    .cloned()
                    .unwrap_or_default();
                if let Some(parts) = previous.get_mut("parts").and_then(Value::as_array_mut) {
                    parts.extend(additions);
                }
            }
        } else {
            merged.push(turn);
        }
    }
    *contents = merged;
}

fn role(value: &Value) -> Option<&str> {
    value.get("role").and_then(Value::as_str)
}

fn parts(value: &Value) -> &[Value] {
    value
        .get("parts")
        .and_then(Value::as_array)
        .map_or(&[], Vec::as_slice)
}

fn set_parts(value: &mut Value, parts: Vec<Value>) {
    value["parts"] = Value::Array(parts);
}

fn has_response(value: &Value) -> bool {
    parts(value)
        .iter()
        .any(|part| part.get("functionResponse").is_some())
}

fn is_attachment(value: &Value) -> bool {
    value.get("inlineData").is_some() || value.get("fileData").is_some()
}

fn text_turn(text: &str) -> Value {
    json!({"role": "user", "parts": [{"text": text}]})
}

#[cfg(test)]
mod tests {
    use serde_json::{Value, json};

    use super::{INTERRUPTED, SKIP_SIGNATURE, shape_code_assist};

    fn body(contents: Value) -> serde_json::Map<String, Value> {
        let mut body = serde_json::Map::new();
        body.insert("contents".to_owned(), contents);
        body
    }

    #[test]
    fn r49d_signatures_thinking_and_history_boundaries_match_code_assist() {
        let mut body = body(json!([
            {"role":"model","parts":[
                {"text":"hidden","thought":true,"thoughtSignature":"thinking-sig"},
                {"functionCall":{"id":"a","name":"read","args":{}}},
                {"functionCall":{"id":"b","name":"write","args":{}}}
            ]},
            {"role":"model","parts":[
                {"functionCall":{"id":"c","name":"read","args":{}},"thoughtSignature":"call-sig"},
                {"functionCall":{"id":"d","name":"write","args":{}}}
            ]}
        ]));
        shape_code_assist(&mut body, "gemini-3.8-flash", false);
        let contents = body["contents"].as_array().expect("contents");
        assert_eq!(contents[0]["role"], "user");
        assert_eq!(
            contents[0]["parts"][0]["text"],
            "[Continuing from previous AI thoughts...]"
        );
        let first = contents[1]["parts"].as_array().expect("first");
        assert_eq!(first.len(), 2);
        assert_eq!(first[0]["thoughtSignature"], SKIP_SIGNATURE);
        assert_eq!(first[1]["thoughtSignature"], SKIP_SIGNATURE);
        let second = contents[3]["parts"].as_array().expect("second");
        assert_eq!(second[0]["thoughtSignature"], "call-sig");
        assert!(second[1].get("thoughtSignature").is_none());
        assert_eq!(
            contents.last().expect("last")["parts"][0]["functionResponse"]["response"]["output"],
            INTERRUPTED
        );

        let mut ending = self::body(json!([
            {"role":"user","parts":[{"text":"fix"}]},
            {"role":"model","parts":[{"text":"fixed"}]}
        ]));
        shape_code_assist(&mut ending, "gemini-3.8-flash", false);
        assert_eq!(
            ending["contents"]
                .as_array()
                .expect("contents")
                .last()
                .expect("last")["parts"][0]["text"],
            "Continue."
        );
    }

    #[test]
    fn r49d_duplicate_and_unanswered_calls_get_the_reference_shape() {
        let mut body = body(json!([
            {"role":"user","parts":[{"text":"look"}]},
            {"role":"model","parts":[
                {"functionCall":{"id":"a","name":"read","args":{}}},
                {"functionCall":{"id":"b","name":"write","args":{}}}
            ]},
            {"role":"user","parts":[
                {"functionResponse":{"id":"a","name":"read","response":{"result":"first"}}},
                {"functionResponse":{"id":"a","name":"read","response":{"result":"second"}}},
                {"inlineData":{"mimeType":"image/png","data":"c2Vjb25k"}},
                {"text":"continue"}
            ]}
        ]));
        shape_code_assist(&mut body, "gemini-3.8-flash", false);
        let text = body["contents"].to_string();
        assert!(text.contains("first"));
        assert!(!text.contains("second"));
        assert!(!text.contains("c2Vjb25k"));
        assert!(text.contains(INTERRUPTED));
        assert!(text.contains(r#""output":"first""#));
        let contents = body["contents"].as_array().expect("contents");
        assert_eq!(contents.len(), 4);
        assert_eq!(contents[2]["parts"].as_array().expect("answers").len(), 2);
        assert_eq!(contents[3]["role"], "user");
        assert_eq!(contents[3]["parts"][0]["text"], "continue");
    }

    #[test]
    fn r49d_antigravity_claude_sanitizes_and_orders_tool_images() {
        let mut body = body(json!([
            {"role":"user","parts":[{"text":"look"}]},
            {"role":"model","parts":[
                {"functionCall":{"id":"toolu_01:x","name":"Read","args":{}},"thoughtSignature":"foreign"}
            ]},
            {"role":"user","parts":[
                {"functionResponse":{"id":"toolu_01:x","name":"Read","response":{"output":"one"}}},
                {"inlineData":{"mimeType":"image/png","data":"QUFB"}},
                {"functionResponse":{"id":"toolu_02:y","name":"Read","response":{"error":"two"}}},
                {"fileData":{"mimeType":"image/png","fileUri":"https://example.test/b.png"}},
                {"text":"which?"}
            ]}
        ]));
        body.insert(
            "toolConfig".to_owned(),
            json!({"functionCallingConfig":{"mode":"AUTO"}}),
        );
        shape_code_assist(&mut body, "claude-opus-4-6-thinking", true);
        let contents = body["contents"].as_array().expect("contents");
        assert_eq!(contents[1]["parts"][0]["functionCall"]["id"], "toolu_01_x");
        assert_eq!(contents[1]["parts"][0]["thoughtSignature"], SKIP_SIGNATURE);
        let answers = contents[2]["parts"].as_array().expect("answers");
        assert_eq!(answers[0]["functionResponse"]["response"]["result"], "one");
        assert_eq!(answers[1]["functionResponse"]["id"], "toolu_02_y");
        assert_eq!(answers[1]["functionResponse"]["name"], "Read");
        assert_eq!(answers[1]["functionResponse"]["response"]["result"], "two");
        assert!(
            answers[2]["text"]
                .as_str()
                .is_some_and(|text| text.contains("Image returned"))
        );
        assert_eq!(answers[3]["inlineData"]["data"], "QUFB");
        assert!(
            answers[4]["text"]
                .as_str()
                .is_some_and(|text| text.contains("Image returned"))
        );
        assert_eq!(
            answers[5]["fileData"]["fileUri"],
            "https://example.test/b.png"
        );
        assert_eq!(answers.last().expect("last")["text"], "which?");
        assert_eq!(
            body["toolConfig"]["functionCallingConfig"]["mode"],
            "VALIDATED"
        );
    }
}
