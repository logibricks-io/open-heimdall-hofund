//! Copilot Gemini image-cap refusals, against a scripted upstream.

use std::sync::Arc;

use hofund_kit_subscription::{TestHost, block_on};
use hofund_plugin_api::{
    AccountId, ContentPart, Message, PluginId, PluginKind, ProtocolId, Request, Role,
    UpstreamProvider,
};
use hofund_plugin_sdk::json::{self, json};
use hofund_plugin_sdk::oauth::{SecretObject, TokenCache};

use crate::models::Book;
use crate::session::MARGIN;
use crate::upstream::CopilotUpstream;

const API: &str = "https://api.individual.githubcopilot.com";
const MODEL: &str = "gemini-4-carbon";
const OMITTED: &str =
    "[An earlier image was omitted here to keep this request within the model's image limit.]";

struct Fix {
    _dir: tempfile::TempDir,
    host: TestHost,
    book: Arc<Book>,
}

fn fix() -> Fix {
    let dir = tempfile::tempdir().expect("dir");
    let host = TestHost::open_for(
        PluginKind::Subscription,
        PluginId::from_static("copilot"),
        dir.path(),
    );
    Fix {
        _dir: dir,
        host,
        book: Arc::new(Book::default()),
    }
}

impl Fix {
    fn account(&self, id: &'static str, oauth: &str) -> CopilotUpstream {
        let account = AccountId::from_static(id);
        let blob = SecretObject::new()
            .with_number("v", 1)
            .with_secret("oauth_token", &oauth.to_owned().into())
            .with_text("host", "github.com")
            .with_text("login", "octo")
            .to_secret();
        block_on(async {
            let accounts = self.host.context().accounts();
            let row = accounts.account(account.clone(), "octo").expect("account");
            accounts.upsert(row, blob).await.expect("seed");
        });
        CopilotUpstream::new(
            account,
            self.host.context().clone(),
            Arc::new(TokenCache::new(MARGIN)),
            Arc::clone(&self.book),
        )
    }

    fn enqueue_session(&self) {
        self.host.http().enqueue(
            json!({
                "token": "tid=session",
                "expires_at": 4_000_000_000_u64,
                "endpoints": {"api": API}
            })
            .to_string()
            .into_bytes(),
        );
    }

    fn prime(&self) {
        self.enqueue_session();
        self.host.http().enqueue(br#"{"data":[]}"#.to_vec());
    }
}

fn shot(n: u32) -> ContentPart {
    json::from_value(json!({
        "type": "image",
        "source": {
            "type": "base64",
            "media_type": "image/png",
            "data": format!("SHOT{n:02}{}", "A".repeat(8))
        }
    }))
    .expect("image part")
}

fn tool_shot(id: &str, n: u32) -> ContentPart {
    json::from_value(json!({
        "type": "tool_result",
        "id": id,
        "content": [{
            "type": "image",
            "source": {
                "type": "base64",
                "media_type": "image/png",
                "data": format!("SHOT{n:02}")
            }
        }]
    }))
    .expect("tool image")
}

fn user_message(text: &str, images: &[u32]) -> Message {
    let mut message = Message::new(Role::User);
    message.content.push(ContentPart::text(text));
    for n in images {
        message.content.push(shot(*n));
    }
    message
}

/// `history` earlier user turns with one image each, then the latest user
/// message with `latest` images of its own.
fn image_chat(model: &str, history: u32, latest: u32) -> Request {
    let mut messages = Vec::new();
    for index in 0..history {
        messages.push(user_message(&format!("look at screen {index}"), &[index]));
        let mut reply = Message::new(Role::Assistant);
        reply
            .content
            .push(ContentPart::text(format!("seen {index}")));
        messages.push(reply);
    }
    let latest_shots: Vec<u32> = (0..latest).map(|index| 90 + index).collect();
    messages.push(user_message("the latest ask", &latest_shots));
    Request::new(model).with_messages(messages)
}

fn too_many(model: &str, cap: u32, got: u32) -> Vec<u8> {
    json!({
        "error": {
            "message": format!(
                "too many images: maximum allowed for model {model} is {cap}, got {got}"
            ),
            "code": "invalid_request_body"
        }
    })
    .to_string()
    .into_bytes()
}

fn chat_ok() -> Vec<u8> {
    json!({
        "id": "chat-1",
        "object": "chat.completion",
        "created": 0,
        "model": MODEL,
        "choices": [{
            "index": 0,
            "message": {"role": "assistant", "content": "ok"},
            "finish_reason": "stop"
        }]
    })
    .to_string()
    .into_bytes()
}

fn chat_bodies(host: &TestHost) -> Vec<Vec<u8>> {
    host.http()
        .calls()
        .iter()
        .filter(|call| call.uri().contains("/chat/completions"))
        .map(|call| call.body().to_vec())
        .collect()
}

fn shots(body: &[u8]) -> Vec<String> {
    let value: json::Value = json::from_slice(body).expect("chat json");
    let mut found = Vec::new();
    collect_shots(&value, &mut found);
    found
}

fn collect_shots(value: &json::Value, found: &mut Vec<String>) {
    match value {
        json::Value::Object(map) => {
            if map.get("type").and_then(json::Value::as_str) == Some("image_url") {
                let url = map
                    .get("image_url")
                    .and_then(|image| image.get("url"))
                    .and_then(json::Value::as_str);
                if let Some(url) = url
                    && let Some(id) = shot_id(url)
                {
                    found.push(id);
                }
                return;
            }
            for child in map.values() {
                collect_shots(child, found);
            }
        }
        json::Value::Array(items) => {
            for child in items {
                collect_shots(child, found);
            }
        }
        _ => {}
    }
}

fn shot_id(url: &str) -> Option<String> {
    let rest = url.split_once("SHOT")?.1;
    let digits: String = rest.chars().take_while(char::is_ascii_digit).collect();
    (!digits.is_empty()).then_some(digits)
}

fn placeholder_count(body: &[u8]) -> usize {
    String::from_utf8_lossy(body).matches(OMITTED).count()
}

fn assert_images(body: &[u8], gone: &[u32], kept: &[u32], placeholders: usize) {
    let found = shots(body);
    for n in gone {
        let id = format!("{n:02}");
        assert!(!found.contains(&id), "SHOT{id} was still sent in {found:?}");
    }
    for n in kept {
        let id = format!("{n:02}");
        assert!(found.contains(&id), "SHOT{id} missing from {found:?}");
    }
    assert_eq!(found.len(), kept.len(), "images {found:?}");
    assert_eq!(placeholder_count(body), placeholders, "placeholders");
}

fn assert_history_text(body: &[u8], turns: u32) {
    let text = String::from_utf8_lossy(body);
    for index in 0..turns {
        let label = format!("look at screen {index}");
        assert!(text.contains(&label), "missing {label}");
    }
    assert!(text.contains("the latest ask"));
}

fn kept_after(history: u32, latest: u32, cap: usize) -> (Vec<u32>, Vec<u32>) {
    let mut all = Vec::new();
    for index in 0..history {
        all.push(index);
    }
    for index in 0..latest {
        all.push(90 + index);
    }
    let over = all.len().saturating_sub(cap);
    let history_len = usize::try_from(history).expect("history");
    let drop_n = over.min(history_len);
    let gone = all[..drop_n].to_vec();
    let kept = all[drop_n..].to_vec();
    (gone, kept)
}

#[test]
fn d229_gemini_over_cap_omits_oldest_images_and_prefits_the_next_turn() {
    let fix = fix();
    let upstream = fix.account("a-d229-image-cap", "gho-d229");
    fix.prime();
    fix.host
        .http()
        .enqueue_response(400, &[], too_many(MODEL, 10, 12));
    fix.host.http().enqueue(chat_ok());
    fix.host.http().enqueue(chat_ok());

    let request = image_chat(MODEL, 11, 1);
    block_on(upstream.execute(request.clone(), ProtocolId::OPENAI_CHAT)).expect("refitted turn");
    let bodies = chat_bodies(&fix.host);
    assert_eq!(bodies.len(), 2, "one refusal and one resend");
    let (gone, kept) = kept_after(11, 1, 10);
    assert_images(
        &bodies[0],
        &[],
        &(gone.iter().chain(&kept).copied().collect::<Vec<_>>()),
        0,
    );
    assert_images(&bodies[1], &gone, &kept, gone.len());
    assert_history_text(&bodies[1], 11);
    assert!(String::from_utf8_lossy(&bodies[1]).contains("SHOT02"));

    block_on(upstream.execute(request, ProtocolId::OPENAI_CHAT)).expect("pre-fitted turn");
    let bodies = chat_bodies(&fix.host);
    assert_eq!(bodies.len(), 3, "the next turn is sent once");
    assert_images(&bodies[2], &gone, &kept, gone.len());
    assert_history_text(&bodies[2], 11);
}

#[test]
fn d229_at_cap_stays_whole_and_a_latest_message_over_cap_is_returned_once() {
    let fix = fix();
    let upstream = fix.account("a-d229-image-cap", "gho-d229");
    fix.prime();
    fix.host
        .http()
        .enqueue_response(400, &[], too_many(MODEL, 10, 12));
    fix.host.http().enqueue(chat_ok());
    fix.host.http().enqueue(chat_ok());
    fix.host
        .http()
        .enqueue_response(400, &[], too_many(MODEL, 10, 13));

    block_on(upstream.execute(image_chat(MODEL, 11, 1), ProtocolId::OPENAI_CHAT)).expect("learn");

    let at_cap = image_chat(MODEL, 9, 1);
    block_on(upstream.execute(at_cap, ProtocolId::OPENAI_CHAT)).expect("at the cap");
    let bodies = chat_bodies(&fix.host);
    let (gone, kept) = kept_after(9, 1, 10);
    assert!(gone.is_empty());
    assert_images(bodies.last().expect("at-cap body"), &[], &kept, 0);

    let before = bodies.len();
    let error = block_on(upstream.execute(image_chat(MODEL, 1, 12), ProtocolId::OPENAI_CHAT))
        .err()
        .expect("latest message over the cap");
    assert_eq!(error.status(), Some(400));
    assert!(
        error
            .message()
            .to_ascii_lowercase()
            .contains("too many images")
    );
    let bodies = chat_bodies(&fix.host);
    assert_eq!(bodies.len(), before + 1, "asked once");
    let mut every = Vec::new();
    every.push(0);
    for index in 0..12 {
        every.push(90 + index);
    }
    assert_images(bodies.last().expect("refused body"), &[], &every, 0);
}

#[test]
fn d229_image_cap_refusal_is_resent_only_once() {
    let fix = fix();
    let upstream = fix.account("a-d229-image-cap", "gho-d229");
    fix.prime();
    let mixed = br#"{"error":{"message":"Too Many Images: Maximum allowed for model gemini-4-carbon is 10, got 12"}}"#;
    fix.host.http().enqueue_response(400, &[], mixed.to_vec());
    fix.host
        .http()
        .enqueue_response(400, &[], too_many(MODEL, 10, 10));

    let error = block_on(upstream.execute(image_chat(MODEL, 11, 1), ProtocolId::OPENAI_CHAT))
        .err()
        .expect("the resend is still refused");
    assert_eq!(error.status(), Some(400));
    assert!(
        error
            .message()
            .to_ascii_lowercase()
            .contains("too many images")
    );
    let bodies = chat_bodies(&fix.host);
    assert_eq!(bodies.len(), 2, "exactly one resend");
    let (gone, kept) = kept_after(11, 1, 10);
    assert_images(&bodies[1], &gone, &kept, gone.len());
}

#[test]
fn d229_unrelated_refusal_is_not_an_image_refit() {
    let fix = fix();
    let upstream = fix.account("a-d229-image-cap", "gho-d229");
    fix.prime();
    fix.host.http().enqueue_response(
        400,
        &[],
        br#"{"error":{"message":"something else broke"}}"#.to_vec(),
    );
    fix.host
        .http()
        .enqueue_response(500, &[], too_many(MODEL, 10, 12));

    let first = block_on(upstream.execute(image_chat(MODEL, 11, 1), ProtocolId::OPENAI_CHAT))
        .err()
        .expect("other 400");
    assert_eq!(first.status(), Some(400));
    assert_eq!(chat_bodies(&fix.host).len(), 1);
    assert_eq!(shots(&chat_bodies(&fix.host)[0]).len(), 12);

    let second = block_on(upstream.execute(image_chat(MODEL, 11, 1), ProtocolId::OPENAI_CHAT))
        .err()
        .expect("non-400");
    assert_eq!(second.status(), Some(500));
    let bodies = chat_bodies(&fix.host);
    assert_eq!(bodies.len(), 2, "a non-400 is not resent");
    assert_eq!(shots(&bodies[1]).len(), 12);
    assert_eq!(placeholder_count(&bodies[1]), 0);
}

#[test]
fn d229_learned_cap_does_not_change_another_model_or_account() {
    let fix = fix();
    let gemini = fix.account("a-d229-image-cap", "gho-d229");
    let other = fix.account("a-d229-other", "gho-other");
    fix.prime();
    fix.host
        .http()
        .enqueue_response(400, &[], too_many(MODEL, 10, 12));
    fix.host.http().enqueue(chat_ok());
    block_on(gemini.execute(image_chat(MODEL, 11, 1), ProtocolId::OPENAI_CHAT)).expect("learn");

    fix.host.http().enqueue(chat_ok());
    block_on(gemini.execute(
        image_chat("claude-sonnet-5", 11, 1),
        ProtocolId::OPENAI_CHAT,
    ))
    .expect("other model");
    let model_body = chat_bodies(&fix.host).pop().expect("claude body");
    assert_eq!(shots(&model_body).len(), 12);
    assert_eq!(placeholder_count(&model_body), 0);

    fix.prime();
    fix.host.http().enqueue(chat_ok());
    block_on(other.execute(image_chat(MODEL, 11, 1), ProtocolId::OPENAI_CHAT))
        .expect("other account");
    let account_body = chat_bodies(&fix.host).pop().expect("other account body");
    assert_eq!(shots(&account_body).len(), 12);
    assert_eq!(placeholder_count(&account_body), 0);
}

#[test]
fn d229_a_smaller_stated_cap_replaces_the_one_learned_before() {
    let fix = fix();
    let upstream = fix.account("a-d229-image-cap", "gho-d229");
    fix.prime();
    fix.host
        .http()
        .enqueue_response(400, &[], too_many(MODEL, 10, 12));
    fix.host.http().enqueue(chat_ok());
    fix.host
        .http()
        .enqueue_response(400, &[], too_many(MODEL, 8, 12));
    fix.host.http().enqueue(chat_ok());

    block_on(upstream.execute(image_chat(MODEL, 11, 1), ProtocolId::OPENAI_CHAT)).expect("cap 10");
    let error = block_on(upstream.execute(image_chat(MODEL, 0, 12), ProtocolId::OPENAI_CHAT))
        .err()
        .expect("latest message alone");
    assert_eq!(error.status(), Some(400));
    assert_eq!(
        chat_bodies(&fix.host).len(),
        3,
        "the latest message is not stripped or resent"
    );

    block_on(upstream.execute(image_chat(MODEL, 11, 1), ProtocolId::OPENAI_CHAT)).expect("cap 8");
    let bodies = chat_bodies(&fix.host);
    assert_eq!(bodies.len(), 4, "fitted before sending");
    let (gone, kept) = kept_after(11, 1, 8);
    assert_images(bodies.last().expect("fitted"), &gone, &kept, gone.len());
}

#[test]
fn d229_tool_and_assistant_images_outside_the_latest_user_message_can_be_omitted() {
    let fix = fix();
    let upstream = fix.account("a-d229-image-cap", "gho-d229");
    fix.prime();
    fix.host
        .http()
        .enqueue_response(400, &[], too_many(MODEL, 10, 11));
    fix.host.http().enqueue(chat_ok());

    let mut messages = vec![user_message("look at screen 0", &[])];
    let mut assistant = Message::new(Role::Assistant);
    assistant.content.push(shot(0));
    messages.push(assistant);
    let mut tool_only = Message::new(Role::User);
    tool_only.content.push(tool_shot("call-1", 1));
    messages.push(tool_only);
    let latest: Vec<u32> = (0..9).map(|index| 90 + index).collect();
    messages.push(user_message("the latest ask", &latest));
    let request = Request::new(MODEL).with_messages(messages);

    block_on(upstream.execute(request, ProtocolId::OPENAI_CHAT)).expect("refit");
    let bodies = chat_bodies(&fix.host);
    assert_eq!(bodies.len(), 2);
    assert_images(
        &bodies[0],
        &[],
        &[0, 1, 90, 91, 92, 93, 94, 95, 96, 97, 98],
        0,
    );
    assert_images(
        &bodies[1],
        &[0],
        &[1, 90, 91, 92, 93, 94, 95, 96, 97, 98],
        1,
    );
    assert!(String::from_utf8_lossy(&bodies[1]).contains("look at screen 0"));
    assert!(String::from_utf8_lossy(&bodies[1]).contains("the latest ask"));
}

#[test]
fn d229_model_list_max_prompt_images_is_not_enforced() {
    let fix = fix();
    let upstream = fix.account("a-d229-image-cap", "gho-d229");
    fix.enqueue_session();
    fix.host.http().enqueue(
        json!({"data":[{
            "id": MODEL,
            "model_picker_enabled": true,
            "capabilities": {"type": "chat", "limits": {"max_prompt_images": 1}},
            "max_prompt_images": 1,
            "supported_endpoints": ["/chat/completions"]
        }]})
        .to_string()
        .into_bytes(),
    );
    fix.host.http().enqueue(chat_ok());

    block_on(upstream.execute(image_chat(MODEL, 2, 1), ProtocolId::OPENAI_CHAT))
        .expect("under a refusal");
    let bodies = chat_bodies(&fix.host);
    assert_eq!(bodies.len(), 1);
    assert_images(&bodies[0], &[], &[0, 1, 90], 0);
}
