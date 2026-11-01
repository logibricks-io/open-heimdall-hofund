//! Reasoning items the gateway hands a Responses client (D-20): one summary
//! and no content, and no other vendor's seal, whether the gateway wrote
//! the reply or relayed it. No network.
//!
//! reference: internal/gateway/reasoning_content.go:9-206,
//! internal/gateway/responses.go:2200-2340

use hofund_codec::{Codec, OpenAiResponses};
use hofund_ir::{
    BlockStart, ContentBlockStart, ContentBlockStop, ContentPart, Extensions, Message,
    MessageStart, MessageStop, Request, Response, Role, StreamEvent, ThinkingDelta, ThinkingPart,
};
use serde_json::{Value, json};

fn frames(bytes: &[u8]) -> Vec<Value> {
    String::from_utf8(bytes.to_vec())
        .expect("utf8")
        .split("\n\n")
        .filter(|frame| !frame.trim().is_empty())
        .filter_map(|frame| {
            frame
                .lines()
                .find_map(|line| line.strip_prefix("data: "))
                .map(|data| serde_json::from_str(data).expect("json"))
        })
        .collect()
}

fn of_type<'a>(frames: &'a [Value], kind: &str) -> Vec<&'a Value> {
    frames
        .iter()
        .filter(|frame| frame["type"] == kind)
        .collect()
}

fn keys(item: &Value) -> Vec<String> {
    let mut keys: Vec<String> = item.as_object().expect("object").keys().cloned().collect();
    keys.sort();
    keys
}

fn made_keys() -> Vec<String> {
    ["id", "status", "summary", "type"]
        .map(str::to_owned)
        .to_vec()
}

fn reply_with(item: &Value) -> Value {
    json!({
        "id": "resp_1",
        "object": "response",
        "model": "m",
        "status": "completed",
        "output": [
            item,
            {"id": "msg_1", "type": "message", "role": "assistant", "status": "completed",
             "content": [{"type": "output_text", "text": "hi", "annotations": []}]},
        ],
    })
}

/// The reply's first output item after an upstream reply holding `item` is
/// decoded and encoded again.
fn reencoded(item: &Value) -> Value {
    let upstream = reply_with(item).to_string();
    let response = OpenAiResponses
        .decode_response(upstream.as_bytes())
        .expect("decode");
    let bytes = OpenAiResponses.encode_response(&response).expect("encode");
    let reply: Value = serde_json::from_slice(&bytes).expect("json");
    reply["output"][0].clone()
}

fn foreign_item() -> Value {
    json!({
        "id": "rs_foreign",
        "type": "reasoning",
        "content": [{"type": "reasoning_text", "text": "deep thought"}],
        "summary": [{"type": "summary_text", "text": "deep thought"}],
        "encrypted_content": "vendor-seal",
    })
}

fn thinking_reply(text: &str) -> Response {
    Response::new(Message {
        role: Role::Assistant,
        content: vec![ContentPart::Thinking(ThinkingPart {
            text: text.to_owned(),
            signature: None,
            redacted: false,
            data: None,
            extensions: Extensions::new(),
        })],
        extensions: Extensions::new(),
    })
}

#[test]
fn d20_a_reply_reasoning_item_is_one_summary_and_no_content() {
    let bytes = OpenAiResponses
        .encode_response(&thinking_reply("plan"))
        .expect("encode");
    let reply: Value = serde_json::from_slice(&bytes).expect("json");
    let item = &reply["output"][0];
    assert_eq!(keys(item), made_keys(), "{item}");
    assert_eq!(item["type"], "reasoning");
    assert_eq!(item["status"], "completed");
    assert!(
        item["id"].as_str().expect("id").starts_with("rs_"),
        "{item}"
    );
    assert_eq!(
        item["summary"],
        json!([{"type": "summary_text", "text": "plan"}])
    );
}

#[test]
fn d20_a_request_reasoning_item_keeps_its_text_as_content() {
    let request = Request::new("m").with_messages(vec![thinking_reply("plan").message]);
    let bytes = OpenAiResponses.encode_request(&request).expect("encode");
    let body: Value = serde_json::from_slice(&bytes).expect("json");
    let item = &body["input"][0];
    assert_eq!(item["type"], "reasoning", "{body}");
    assert_eq!(
        item["content"],
        json!([{"type": "reasoning_text", "text": "plan"}])
    );
}

#[test]
fn d20_another_vendors_sealed_item_is_made_over() {
    let item = reencoded(&foreign_item());
    assert_eq!(keys(&item), made_keys(), "{item}");
    assert_eq!(item["id"], "rs_foreign");
    assert_eq!(item["status"], "completed");
    assert!(item.get("encrypted_content").is_none());
    assert!(item.get("content").is_none());
}

#[test]
fn d20_the_text_goes_once_and_wins_over_the_summaries() {
    let mut upstream = foreign_item();
    upstream["summary"] = json!([{"type": "summary_text", "text": "short"}]);
    let item = reencoded(&upstream);
    assert_eq!(
        item["summary"],
        json!([{"type": "summary_text", "text": "deep thought"}])
    );
}

#[test]
fn d20_the_summaries_stand_in_for_empty_content_text() {
    let mut upstream = foreign_item();
    upstream["content"] = json!([{"type": "reasoning_text", "text": ""}]);
    upstream["summary"] = json!([
        {"type": "summary_text", "text": "one "},
        {"type": "summary_text", "text": "two"},
    ]);
    let item = reencoded(&upstream);
    assert_eq!(
        item["summary"],
        json!([{"type": "summary_text", "text": "one two"}])
    );
}

#[test]
fn d20_an_item_with_no_summary_key_is_made_over() {
    let mut upstream = foreign_item();
    upstream.as_object_mut().expect("object").remove("summary");
    let item = reencoded(&upstream);
    assert_eq!(
        item["summary"],
        json!([{"type": "summary_text", "text": "deep thought"}])
    );
}

#[test]
fn d20_a_made_item_with_no_id_gets_a_fresh_rs_id() {
    let mut upstream = foreign_item();
    upstream.as_object_mut().expect("object").remove("id");
    let item = reencoded(&upstream);
    assert!(
        item["id"].as_str().expect("id").starts_with("rs_"),
        "{item}"
    );
}

#[test]
fn d20_an_item_with_an_empty_content_array_goes_as_it_came() {
    let own = json!({
        "id": "rs_own",
        "type": "reasoning",
        "content": [],
        "summary": [{"type": "summary_text", "text": "brief"}],
        "encrypted_content": "own-seal",
    });
    assert_eq!(reencoded(&own), own);
}

#[test]
fn d20_the_upstreams_own_sealed_item_goes_as_it_came() {
    let own = json!({
        "id": "rs_own",
        "type": "reasoning",
        "summary": [{"type": "summary_text", "text": "brief"}],
        "encrypted_content": "own-seal",
    });
    assert_eq!(reencoded(&own), own);
}

fn thinking_events(deltas: &[&str]) -> Vec<StreamEvent> {
    let mut events = vec![
        StreamEvent::MessageStart(MessageStart {
            id: Some("resp_s".to_owned()),
            model: Some("m".to_owned()),
            usage: None,
            extensions: Extensions::new(),
        }),
        StreamEvent::ContentBlockStart(ContentBlockStart {
            index: 0,
            block: BlockStart::Thinking {
                redacted: false,
                data: None,
            },
            extensions: Extensions::new(),
        }),
    ];
    for delta in deltas {
        events.push(StreamEvent::ThinkingDelta(ThinkingDelta {
            index: 0,
            text: (*delta).to_owned(),
            extensions: Extensions::new(),
        }));
    }
    events.push(StreamEvent::ContentBlockStop(ContentBlockStop {
        index: 0,
        extensions: Extensions::new(),
    }));
    events.push(StreamEvent::MessageStop(MessageStop::default()));
    events
}

#[test]
fn d20_a_streamed_reasoning_item_is_summary_events_and_no_content() {
    let bytes = OpenAiResponses
        .encode_stream(&thinking_events(&["deep ", "thought"]))
        .expect("encode");
    let frames = frames(&bytes);
    let text = String::from_utf8(bytes).expect("utf8");
    assert!(!text.contains("reasoning_text"), "{text}");
    let added = of_type(&frames, "response.output_item.added");
    let item_id = added[0]["item"]["id"].as_str().expect("id").to_owned();
    assert!(item_id.starts_with("rs_"), "{item_id}");
    assert_eq!(added[0]["item"]["summary"], json!([]));
    assert_eq!(added[0]["item"]["status"], "in_progress");
    assert_eq!(keys(&added[0]["item"]), made_keys());
    let part_added = of_type(&frames, "response.reasoning_summary_part.added");
    assert_eq!(part_added.len(), 1);
    assert_eq!(part_added[0]["item_id"], item_id.as_str());
    assert_eq!(part_added[0]["output_index"], 0);
    assert_eq!(part_added[0]["summary_index"], 0);
    assert_eq!(
        part_added[0]["part"],
        json!({"type": "summary_text", "text": ""})
    );
    let deltas = of_type(&frames, "response.reasoning_summary_text.delta");
    let said: Vec<&str> = deltas
        .iter()
        .map(|delta| delta["delta"].as_str().expect("delta"))
        .collect();
    assert_eq!(said, ["deep ", "thought"]);
    assert!(
        deltas
            .iter()
            .all(|delta| delta["item_id"] == item_id.as_str()
                && delta["summary_index"] == 0
                && delta["output_index"] == 0)
    );
    let text_done = of_type(&frames, "response.reasoning_summary_text.done");
    assert_eq!(text_done.len(), 1);
    assert_eq!(text_done[0]["text"], "deep thought");
    assert_eq!(text_done[0]["item_id"], item_id.as_str());
    assert_eq!(text_done[0]["summary_index"], 0);
    let part_done = of_type(&frames, "response.reasoning_summary_part.done");
    assert_eq!(part_done.len(), 1);
    assert_eq!(
        part_done[0]["part"],
        json!({"type": "summary_text", "text": "deep thought"})
    );
    assert_eq!(part_done[0]["item_id"], item_id.as_str());
    assert_eq!(part_done[0]["summary_index"], 0);
    let order: Vec<&str> = frames
        .iter()
        .map(|frame| frame["type"].as_str().expect("type"))
        .collect();
    let at = |kind: &str| order.iter().position(|seen| *seen == kind).expect(kind);
    assert!(
        at("response.reasoning_summary_part.added") < at("response.reasoning_summary_text.delta")
    );
    assert!(
        at("response.reasoning_summary_text.done") < at("response.reasoning_summary_part.done")
    );
    assert!(at("response.reasoning_summary_part.done") < at("response.output_item.done"));
    let done = of_type(&frames, "response.output_item.done");
    let item = &done[0]["item"];
    assert_eq!(keys(item), made_keys(), "{item}");
    assert_eq!(item["id"], item_id.as_str());
    assert_eq!(
        item["summary"],
        json!([{"type": "summary_text", "text": "deep thought"}])
    );
    let completed = of_type(&frames, "response.completed");
    assert_eq!(completed[0]["response"]["output"][0], *item);
}

#[test]
fn d20_a_streamed_reasoning_item_with_only_empty_deltas_has_no_summary() {
    let bytes = OpenAiResponses
        .encode_stream(&thinking_events(&["", ""]))
        .expect("encode");
    let frames = frames(&bytes);
    assert!(of_type(&frames, "response.reasoning_summary_part.added").is_empty());
    assert!(of_type(&frames, "response.reasoning_summary_text.delta").is_empty());
    assert!(of_type(&frames, "response.reasoning_summary_text.done").is_empty());
    assert!(of_type(&frames, "response.reasoning_summary_part.done").is_empty());
    let done = of_type(&frames, "response.output_item.done");
    assert_eq!(done.len(), 1, "{frames:?}");
    assert_eq!(keys(&done[0]["item"]), made_keys());
    assert_eq!(done[0]["item"]["summary"], json!([]));
}

fn upstream_stream(item: &Value) -> String {
    let added = json!({"type": "response.output_item.added", "sequence_number": 1,
        "output_index": 0, "item": {"id": item["id"], "type": "reasoning", "summary": []}});
    let done = json!({"type": "response.output_item.done", "sequence_number": 2,
        "output_index": 0, "item": item});
    let completed = json!({"type": "response.completed", "sequence_number": 3,
        "response": {"id": "resp_u", "object": "response", "model": "m",
            "status": "completed", "output": [item]}});
    format!(
        "event: response.created\ndata: {}\n\nevent: response.output_item.added\ndata: {added}\n\nevent: response.output_item.done\ndata: {done}\n\nevent: response.completed\ndata: {completed}\n\n",
        json!({"type": "response.created", "sequence_number": 0,
            "response": {"id": "resp_u", "object": "response", "model": "m",
                "status": "in_progress", "output": []}}),
    )
}

#[test]
fn d20_a_streamed_foreign_sealed_item_is_made_over_under_its_announced_id() {
    let events = OpenAiResponses
        .decode_stream(upstream_stream(&foreign_item()).as_bytes())
        .expect("decode");
    let bytes = OpenAiResponses.encode_stream(&events).expect("encode");
    let text = String::from_utf8(bytes.clone()).expect("utf8");
    assert!(!text.contains("vendor-seal"), "{text}");
    let frames = frames(&bytes);
    let added = of_type(&frames, "response.output_item.added");
    let done = of_type(&frames, "response.output_item.done");
    let item = &done[0]["item"];
    assert_eq!(keys(item), made_keys(), "{item}");
    assert_eq!(item["id"], added[0]["item"]["id"]);
    assert_eq!(
        item["summary"],
        json!([{"type": "summary_text", "text": "deep thought"}])
    );
}

#[test]
fn d20_a_streamed_made_item_keeps_the_id_its_events_carried() {
    let mut item = foreign_item();
    item.as_object_mut().expect("object").remove("id");
    let upstream =
        upstream_stream(&item).replace(r#""item":{"id":null,"#, r#""item":{"id":"rs_announced","#);
    assert!(upstream.contains("rs_announced"), "{upstream}");
    let events = OpenAiResponses
        .decode_stream(upstream.as_bytes())
        .expect("decode");
    let frames = frames(&OpenAiResponses.encode_stream(&events).expect("encode"));
    let added = of_type(&frames, "response.output_item.added");
    let done = of_type(&frames, "response.output_item.done");
    assert_eq!(added[0]["item"]["id"], "rs_announced");
    assert_eq!(done[0]["item"]["id"], "rs_announced");
    assert_eq!(keys(&done[0]["item"]), made_keys());
}

#[test]
fn d20_a_sealed_block_with_no_item_id_is_announced_under_the_seal_id() {
    let mut extensions = Extensions::new();
    extensions.insert(
        "sealed_item",
        json!({"id": "rs_own", "type": "reasoning", "summary": [],
            "encrypted_content": "own-seal"}),
    );
    let mut events = thinking_events(&[]);
    if let StreamEvent::ContentBlockStart(start) = &mut events[1] {
        start.extensions = extensions.clone();
    }
    if let StreamEvent::ContentBlockStop(stop) = &mut events[2] {
        stop.extensions = extensions;
    }
    let frames = frames(&OpenAiResponses.encode_stream(&events).expect("encode"));
    let added = of_type(&frames, "response.output_item.added");
    assert_eq!(added[0]["item"]["id"], "rs_own");
    assert!(added[0]["item"].get("encrypted_content").is_none());
    let done = of_type(&frames, "response.output_item.done");
    assert_eq!(done[0]["item"]["encrypted_content"], "own-seal");
}

#[test]
fn d20_a_streamed_own_sealed_item_goes_as_it_came_and_is_not_announced() {
    let own = json!({
        "id": "rs_own",
        "type": "reasoning",
        "summary": [],
        "encrypted_content": "own-seal",
    });
    let events = OpenAiResponses
        .decode_stream(upstream_stream(&own).as_bytes())
        .expect("decode");
    let bytes = OpenAiResponses.encode_stream(&events).expect("encode");
    let frames = frames(&bytes);
    let added = of_type(&frames, "response.output_item.added");
    assert!(
        added[0]["item"].get("encrypted_content").is_none(),
        "{}",
        added[0]
    );
    let done = of_type(&frames, "response.output_item.done");
    assert_eq!(done[0]["item"]["encrypted_content"], "own-seal");
    assert_eq!(done[0]["item"]["id"], "rs_own");
    assert_eq!(added[0]["item"]["id"], "rs_own");
}

#[test]
fn d20_a_relayed_stream_makes_over_only_the_lines_with_foreign_items() {
    let upstream = upstream_stream(&foreign_item());
    let mut tap = OpenAiResponses.raw_tap("m", "m");
    let mut out = Vec::new();
    // chunk boundaries inside lines and inside the item
    for chunk in upstream.as_bytes().chunks(37) {
        out.extend(tap.push(chunk));
    }
    out.extend(tap.finish());
    let text = String::from_utf8(out.clone()).expect("utf8");
    assert!(!text.contains("vendor-seal"), "{text}");
    assert!(!text.contains("reasoning_text"), "{text}");
    let up_lines: Vec<&str> = upstream.lines().collect();
    let out_lines: Vec<&str> = text.lines().collect();
    assert_eq!(up_lines.len(), out_lines.len());
    for (up, out) in up_lines.iter().zip(&out_lines) {
        if up.contains("reasoning_text") {
            assert_ne!(up, out);
        } else {
            assert_eq!(up, out);
        }
    }
    let frames = frames(&out);
    let done = of_type(&frames, "response.output_item.done");
    assert_eq!(keys(&done[0]["item"]), made_keys());
    assert_eq!(done[0]["item"]["id"], "rs_foreign");
    assert_eq!(done[0]["sequence_number"], 2);
    let completed = of_type(&frames, "response.completed");
    assert_eq!(completed[0]["response"]["output"][0], done[0]["item"]);
    assert_eq!(completed[0]["sequence_number"], 3);
}

#[test]
fn d20_a_relayed_stream_keeps_line_ends_and_a_last_unended_line() {
    let line = json!({"type": "response.output_item.done", "output_index": 0,
        "item": foreign_item()});
    let upstream = format!("event: response.output_item.done\r\ndata:{line}\r\n\r\ndata: {line}");
    let mut tap = OpenAiResponses.raw_tap("m", "m");
    let mut out = tap.push(upstream.as_bytes());
    out.extend(tap.finish());
    let text = String::from_utf8(out).expect("utf8");
    assert!(!text.contains("vendor-seal"), "{text}");
    assert!(
        text.starts_with("event: response.output_item.done\r\ndata: {"),
        "{text}"
    );
    assert_eq!(text.matches("\r\n").count(), 3, "{text}");
    assert!(text.ends_with('}'), "{text}");
}

#[test]
fn d20_a_relayed_stream_without_foreign_items_is_byte_for_byte() {
    let own = json!({"id": "rs_own", "type": "reasoning", "summary": [],
        "encrypted_content": "own-seal"});
    // spaced JSON: a line parsed and written again would lose the spaces
    let upstream = upstream_stream(&own).replace("\"type\":", "\"type\": ");
    let mut tap = OpenAiResponses.raw_tap("m", "m");
    let mut out = Vec::new();
    for chunk in upstream.as_bytes().chunks(11) {
        out.extend(tap.push(chunk));
    }
    out.extend(tap.finish());
    assert_eq!(String::from_utf8(out).expect("utf8"), upstream);
}

#[test]
fn d20_a_relayed_whole_reply_is_made_over() {
    let upstream = reply_with(&foreign_item()).to_string();
    let out = OpenAiResponses.rewrite_model(upstream.as_bytes(), "client", "m");
    let reply: Value = serde_json::from_slice(&out).expect("json");
    let item = &reply["output"][0];
    assert_eq!(keys(item), made_keys(), "{item}");
    assert_eq!(item["id"], "rs_foreign");
    assert_eq!(reply["model"], "client");
    assert_eq!(reply["output"][1]["content"][0]["text"], "hi");
}

#[test]
fn d20_a_relayed_whole_reply_without_foreign_items_is_byte_for_byte() {
    let own = json!({"id": "rs_own", "type": "reasoning", "summary": [],
        "encrypted_content": "own-seal"});
    let upstream = reply_with(&own)
        .to_string()
        .replace("\"type\":", "\"type\": ");
    let out = OpenAiResponses.rewrite_model(upstream.as_bytes(), "m", "m");
    assert_eq!(out, upstream.as_bytes());
}
