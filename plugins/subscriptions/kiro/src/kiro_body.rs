//! Request bodies for Kiro `generateAssistantResponse`.

use hofund_plugin_api::{ContentPart, PluginError, Request, Role};
use hofund_plugin_sdk::json::{self, Value};

use crate::creds::Credential;

pub(crate) fn reject_unsupported(request: &Request) -> Result<(), PluginError> {
    if request.stream {
        return Err(PluginError::config(
            "Kiro upstream does not stream on this path yet",
        ));
    }
    if !request.tools.is_empty() || request.tool_choice.is_some() {
        return Err(PluginError::config(
            "Kiro upstream accepts text-only requests without tools",
        ));
    }
    for message in &request.messages {
        for part in &message.content {
            match part {
                ContentPart::ToolCall(_) | ContentPart::ToolResult(_) | ContentPart::Image(_) => {
                    return Err(PluginError::config(
                        "Kiro upstream accepts text-only requests without tools or images",
                    ));
                }
                ContentPart::Text(_) | ContentPart::Thinking(_) | ContentPart::Unknown(_) => {}
            }
        }
    }
    Ok(())
}

pub(crate) fn encode(request: &Request, credential: &Credential) -> Result<Vec<u8>, PluginError> {
    reject_unsupported(request)?;
    let profile_arn = crate::creds::effective_profile_arn(credential);
    let mut system = String::new();
    let mut turns: Vec<Value> = Vec::new();
    for message in &request.messages {
        match message.role {
            Role::System | Role::Developer => {
                for part in &message.content {
                    if let ContentPart::Text(text) = part {
                        if !system.is_empty() {
                            system.push('\n');
                        }
                        system.push_str(&text.text);
                    }
                }
            }
            Role::User => {
                let text = text_from_parts(&message.content);
                if text.is_empty() {
                    continue;
                }
                let mut content = text;
                if turns.is_empty() && !system.is_empty() {
                    content = format!("{system}\n\n{content}");
                }
                turns.push(json::json!({
                    "userInputMessage": {
                        "content": content,
                        "modelId": request.model,
                        "origin": "KIRO_CLI",
                        "profileArn": profile_arn,
                    }
                }));
            }
            Role::Assistant => {
                let text = text_from_parts(&message.content);
                if text.is_empty() {
                    continue;
                }
                turns.push(json::json!({
                    "assistantResponseMessage": {
                        "content": text,
                    }
                }));
            }
            Role::Tool => {
                let text = text_from_parts(&message.content);
                if text.is_empty() {
                    continue;
                }
                turns.push(json::json!({
                    "userInputMessage": {
                        "content": format!("[tool result]\n{text}"),
                        "modelId": request.model,
                        "origin": "KIRO_CLI",
                        "profileArn": profile_arn,
                    }
                }));
            }
        }
    }
    let current = turns.pop().unwrap_or_else(|| {
        json::json!({
            "userInputMessage": {
                "content": if system.is_empty() { "hi".to_owned() } else { system },
                "modelId": request.model,
                "origin": "KIRO_CLI",
                "profileArn": profile_arn,
            }
        })
    });
    let history: Vec<Value> = turns;
    let body = json::json!({
        "conversationState": {
            "history": history,
            "currentMessage": current,
        }
    });
    json::to_vec(&body).map_err(|_| PluginError::decode("Kiro request body could not be encoded"))
}

fn text_from_parts(parts: &[ContentPart]) -> String {
    let mut out = String::new();
    for part in parts {
        if let ContentPart::Text(text) = part {
            if !out.is_empty() {
                out.push('\n');
            }
            out.push_str(&text.text);
        }
    }
    out
}
