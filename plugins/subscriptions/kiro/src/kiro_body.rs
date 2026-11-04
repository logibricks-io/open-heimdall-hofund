//! Request bodies for Kiro `generateAssistantResponse`.

use hofund_plugin_api::{ContentPart, PluginError, Request, Role};
use hofund_plugin_sdk::json;

use crate::creds::Credential;

pub(crate) fn encode(request: &Request, credential: &Credential) -> Result<Vec<u8>, PluginError> {
    let mut content = String::new();
    for message in &request.messages {
        if message.role != Role::User {
            continue;
        }
        for part in &message.content {
            if let ContentPart::Text(text) = part {
                if !content.is_empty() {
                    content.push('\n');
                }
                content.push_str(&text.text);
            }
        }
    }
    if content.is_empty() {
        content.push_str("hi");
    }
    let body = json::json!({
        "conversationState": {
            "history": [],
            "currentMessage": {
                "userInputMessage": {
                    "content": content,
                    "modelId": request.model,
                    "origin": "KIRO_CLI",
                    "profileArn": credential.profile_arn,
                }
            }
        }
    });
    json::to_vec(&body).map_err(|_| PluginError::decode("Kiro request body could not be encoded"))
}
