//! Image blocks on a `-p` stream-json stdin (R-6f).
//!
//! URL images are emitted as URL source blocks (TUI fetch stays out of
//! this slice). Inline base64 is passed through as a base64 source. An
//! image that cannot be resized is sent as it came, matching the
//! reference `claudeImage` pass-through for unread or already-fitting
//! images. Pixel fitting (2000 px / 5 MiB) is applied when the `image`
//! crate is available later; this slice does not add that dependency.

use hofund_plugin_api::{ContentPart, ImageSource, Message, Role};
use serde_json::{Value, json};

/// Stream-json content blocks for one message, including image sources.
#[must_use]
pub(crate) fn content_blocks(message: &Message) -> Vec<Value> {
    let mut blocks = Vec::new();
    let mut text = String::new();
    for part in &message.content {
        match part {
            ContentPart::Text(part) => text.push_str(&part.text),
            ContentPart::Image(image) => {
                flush_text(&mut blocks, &mut text);
                if let Some(block) = image_block(&image.source) {
                    blocks.push(block);
                }
            }
            ContentPart::Thinking(thinking) => text.push_str(&thinking.text),
            _ => {}
        }
    }
    flush_text(&mut blocks, &mut text);
    blocks
}

/// True when this message should be written as a user stream-json line.
#[must_use]
pub(crate) fn is_prompt_message(message: &Message) -> bool {
    matches!(message.role, Role::System | Role::Developer | Role::User)
        && message.content.iter().any(|part| {
            matches!(
                part,
                ContentPart::Text(_) | ContentPart::Image(_) | ContentPart::Thinking(_)
            )
        })
}

fn flush_text(blocks: &mut Vec<Value>, text: &mut String) {
    if text.is_empty() {
        return;
    }
    blocks.push(json!({"type": "text", "text": std::mem::take(text)}));
}

fn image_block(source: &ImageSource) -> Option<Value> {
    match source {
        ImageSource::Base64 { media_type, data } if !data.is_empty() => Some(json!({
            "type": "image",
            "source": {
                "type": "base64",
                "media_type": media_type,
                "data": data,
            }
        })),
        ImageSource::Url { url } if !url.is_empty() => Some(json!({
            "type": "image",
            "source": {
                "type": "url",
                "url": url,
            }
        })),
        _ => None,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use hofund_plugin_api::{ImagePart, Message};

    #[test]
    fn a_text_and_base64_image_become_two_blocks() {
        let mut message = Message::new(Role::User);
        message.content = vec![
            ContentPart::text("look"),
            ContentPart::Image(ImagePart {
                source: ImageSource::Base64 {
                    media_type: "image/png".into(),
                    data: "YWJj".into(),
                },
                detail: None,
                extensions: Default::default(),
            }),
        ];
        let blocks = content_blocks(&message);
        assert_eq!(blocks.len(), 2);
        assert_eq!(blocks[0]["type"], "text");
        assert_eq!(blocks[0]["text"], "look");
        assert_eq!(blocks[1]["type"], "image");
        assert_eq!(blocks[1]["source"]["type"], "base64");
        assert_eq!(blocks[1]["source"]["data"], "YWJj");
        assert_eq!(blocks[1]["source"]["media_type"], "image/png");
    }

    #[test]
    fn a_url_image_is_a_url_source_block_not_fetched() {
        let mut message = Message::new(Role::User);
        message.content = vec![ContentPart::Image(ImagePart {
            source: ImageSource::Url {
                url: "https://example.com/a.png".into(),
            },
            detail: None,
            extensions: Default::default(),
        })];
        let blocks = content_blocks(&message);
        assert_eq!(blocks[0]["source"]["type"], "url");
        assert_eq!(blocks[0]["source"]["url"], "https://example.com/a.png");
        assert!(is_prompt_message(&message));
    }
}
