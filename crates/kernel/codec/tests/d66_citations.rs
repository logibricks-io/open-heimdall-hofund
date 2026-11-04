//! D-66: offsetless citation relay and Codex input healing.

use hofund_codec::citation_tidy;
use hofund_codec::{Codec, OpenAiResponses};
use serde_json::json;

#[test]
fn d66_1_whole_reply_drops_offsetless_url_citation() {
    let good = json!({"type":"url_citation","start_index":0,"end_index":4,"url":"https://example.com/g","title":"G"});
    let bad =
        json!({"type":"url_citation","title":"V","url":"https://example.com/v","site_name":"Ark"});
    let whole = json!({
        "id": "resp_1",
        "object": "response",
        "status": "completed",
        "output": [{
            "type": "message",
            "role": "assistant",
            "content": [{
                "type": "output_text",
                "text": "Sun.",
                "annotations": [good, bad],
            }],
        }],
    });
    let tidied = citation_tidy::tidy_whole(whole.to_string().into_bytes());
    let text = String::from_utf8(tidied).expect("utf-8");
    assert!(!text.contains("site_name"), "{text}");
    assert!(text.contains("https://example.com/g"), "{text}");
    assert!(text.contains("Sun."), "{text}");
}

#[test]
fn d66_2_annotation_added_event_without_offsets_is_dropped() {
    let event = json!({
        "type": "response.output_text.annotation.added",
        "annotation": {"type":"url_citation","url":"https://example.com/v"},
    });
    let out = citation_tidy::tidy_event(event.to_string().as_bytes());
    assert_eq!(out, Some(Vec::new()));
}

#[test]
fn d66_3_file_citation_with_one_index_is_kept() {
    let note = json!({"type":"file_citation","file_id":"f","index":3});
    assert!(!citation_tidy::annotation_lacks_offsets(&note));
    let whole = json!({
        "object": "response",
        "output": [{
            "type": "message",
            "content": [{"type":"output_text","text":"x","annotations":[note.clone()]}],
        }],
    });
    let tidied = citation_tidy::tidy_whole(whole.to_string().into_bytes());
    assert!(
        String::from_utf8(tidied)
            .expect("utf-8")
            .contains("\"index\":3")
    );
}

#[test]
fn d66_4_container_file_citation_without_offsets_is_dropped() {
    let bad = json!({"type":"container_file_citation","container_id":"c1","filename":"x.txt"});
    let whole = json!({
        "object": "response",
        "output": [{
            "type": "message",
            "content": [{
                "type": "output_text",
                "text": "see file",
                "annotations": [bad],
            }],
        }],
    });
    let tidied = citation_tidy::tidy_whole(whole.to_string().into_bytes());
    let text = String::from_utf8(tidied).expect("utf-8");
    assert!(!text.contains("container_id"), "{text}");
    assert!(text.contains("see file"), "{text}");
}

#[test]
fn d66_5_citation_with_start_but_no_end_is_dropped() {
    let partial = json!({"type":"url_citation","start_index":0,"url":"https://example.com/p"});
    assert!(citation_tidy::annotation_lacks_offsets(&partial));
    let whole = json!({
        "object": "response",
        "output": [{
            "type": "message",
            "content": [{
                "type": "output_text",
                "text": "link",
                "annotations": [partial],
            }],
        }],
    });
    let tidied = citation_tidy::tidy_whole(whole.to_string().into_bytes());
    let text = String::from_utf8(tidied).expect("utf-8");
    assert!(!text.contains("example.com/p"), "{text}");
    assert!(text.contains("link"), "{text}");
}

#[test]
fn d66_6_non_output_text_part_annotations_are_untouched() {
    let bad = json!({"type":"url_citation","url":"https://example.com/v","title":"V"});
    let whole = json!({
        "object": "response",
        "output": [{
            "type": "message",
            "content": [{
                "type": "output_image",
                "image_url": "https://example.com/i.png",
                "annotations": [bad.clone()],
            }],
        }],
    });
    let tidied = citation_tidy::tidy_whole(whole.to_string().into_bytes());
    let text = String::from_utf8(tidied).expect("utf-8");
    assert!(text.contains("example.com/v"), "{text}");
    assert!(text.contains("output_image"), "{text}");
}

#[test]
fn d66_7_relay_tap_strips_bad_citation_from_stream_line() {
    let codec = OpenAiResponses;
    let mut tap = codec.raw_tap("vendor/m", "m");
    let good =
        r#"{"type":"url_citation","start_index":0,"end_index":1,"url":"https://example.com/g"}"#;
    let bad = r#"{"type":"url_citation","url":"https://example.com/v","title":"V"}"#;
    let line = format!(
        "data: {{\"type\":\"response.output_item.done\",\"item\":{{\"type\":\"message\",\"content\":[{{\"type\":\"output_text\",\"text\":\"hi\",\"annotations\":[{good},{bad}]}}]}}}}\n\n"
    );
    let out = tap.push(line.as_bytes());
    let text = String::from_utf8(out).expect("utf-8");
    assert!(!text.contains("example.com/v"), "{text}");
    assert!(text.contains("example.com/g"), "{text}");
}
