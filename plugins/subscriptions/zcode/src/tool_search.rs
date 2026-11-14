use hofund_plugin_api::Request;
use hofund_plugin_sdk::json::{Map, Value};

const SEARCH: &str = "ToolSearch";
const SEARCH_ALIAS: &str = "ToolSearchTool";
const PLACEHOLDER: &str = "DeferredToolPlaceholder";

pub(crate) fn needed(request: &Request) -> bool {
    request.tools.iter().any(|tool| tool.name == SEARCH)
        && !request.tools.iter().any(|tool| tool.name == SEARCH_ALIAS)
}

pub(crate) fn request(root: &mut Map<String, Value>) {
    let Some(tools) = root.get("tools").and_then(Value::as_array) else {
        return;
    };
    if !tools
        .iter()
        .any(|tool| tool.get("name").and_then(Value::as_str) == Some(SEARCH))
        || tools
            .iter()
            .any(|tool| tool.get("name").and_then(Value::as_str) == Some(SEARCH_ALIAS))
    {
        return;
    }
    let tools = root
        .get_mut("tools")
        .and_then(Value::as_array_mut)
        .expect("checked tools array");
    tools.retain(|tool| tool.get("name").and_then(Value::as_str) != Some(PLACEHOLDER));
    for tool in tools {
        let Some(tool) = tool.as_object_mut() else {
            continue;
        };
        tool.remove("defer_loading");
        if tool.get("name").and_then(Value::as_str) == Some(SEARCH) {
            tool.insert("name".to_owned(), Value::String(SEARCH_ALIAS.to_owned()));
        }
    }
    if let Some(messages) = root.get_mut("messages").and_then(Value::as_array_mut) {
        for message in messages {
            if message.get("role").and_then(Value::as_str) != Some("assistant") {
                continue;
            }
            if let Some(parts) = message.get_mut("content").and_then(Value::as_array_mut) {
                for part in parts {
                    rename_call(part, SEARCH, SEARCH_ALIAS);
                }
            }
        }
    }
    if let Some(choice) = root.get_mut("tool_choice").and_then(Value::as_object_mut)
        && choice.get("type").and_then(Value::as_str) == Some("tool")
        && choice.get("name").and_then(Value::as_str) == Some(SEARCH)
    {
        choice.insert("name".to_owned(), Value::String(SEARCH_ALIAS.to_owned()));
    }
}

pub(crate) fn response(mut value: Value) -> Value {
    if let Some(parts) = value.get_mut("content").and_then(Value::as_array_mut) {
        for part in parts {
            rename_call(part, SEARCH_ALIAS, SEARCH);
        }
    }
    if let Some(block) = value.get_mut("content_block") {
        rename_call(block, SEARCH_ALIAS, SEARCH);
    }
    value
}

fn rename_call(value: &mut Value, from: &str, to: &str) {
    if value.get("type").and_then(Value::as_str) == Some("tool_use")
        && value.get("name").and_then(Value::as_str) == Some(from)
    {
        value["name"] = Value::String(to.to_owned());
    }
}

#[cfg(test)]
mod tests {
    use hofund_plugin_sdk::json::json;

    use super::*;

    #[test]
    fn r57c_request_renames_search_and_undefer_tools() {
        let mut body = json!({
            "messages": [
                {"role": "user", "content": "find Bash"},
                {"role": "assistant", "content": [
                    {"type": "tool_use", "id": "toolu_s", "name": "ToolSearch", "input": {"query": "select:Bash"}}
                ]}
            ],
            "tools": [
                {"name": "Bash", "defer_loading": true},
                {"name": "ToolSearch"},
                {"name": "DeferredToolPlaceholder", "defer_loading": true}
            ],
            "tool_choice": {"type": "tool", "name": "ToolSearch"}
        });
        request(body.as_object_mut().expect("object"));
        assert_eq!(
            body["tools"],
            json!([{"name": "Bash"}, {"name": "ToolSearchTool"}])
        );
        assert_eq!(body["messages"][1]["content"][0]["name"], "ToolSearchTool");
        assert_eq!(body["tool_choice"]["name"], "ToolSearchTool");
    }

    #[test]
    fn r57c_alias_collision_and_unrelated_requests_stay_unchanged() {
        for mut body in [
            json!({"tools": [{"name": "ToolSearch"}, {"name": "ToolSearchTool", "defer_loading": true}]}),
            json!({"tools": [{"name": "Bash", "defer_loading": true}]}),
        ] {
            let before = body.clone();
            request(body.as_object_mut().expect("object"));
            assert_eq!(body, before);
        }
    }

    #[test]
    fn r57c_response_renames_only_tool_calls() {
        let mut unary = json!({"content": [
            {"type": "text", "text": "ToolSearchTool"},
            {"type": "tool_use", "name": "ToolSearchTool"},
            {"type": "tool_use", "name": "Bash"}
        ]});
        unary = response(unary);
        assert_eq!(unary["content"][0]["text"], "ToolSearchTool");
        assert_eq!(unary["content"][1]["name"], "ToolSearch");
        assert_eq!(unary["content"][2]["name"], "Bash");

        let stream =
            response(json!({"content_block": {"type": "tool_use", "name": "ToolSearchTool"}}));
        assert_eq!(stream["content_block"]["name"], "ToolSearch");
    }
}
