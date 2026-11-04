//! What a Codex request's input becomes.

use hofund_plugin_sdk::json::{self, Value, json};

use super::input::{COMPACT_PROMPT, MARK, SUMMARY_PREFIX, for_backend, for_hofund, sealed};

fn body(input: &Value) -> Vec<u8> {
    json!({"model": "m", "input": input})
        .to_string()
        .into_bytes()
}

fn backend(input: &Value) -> Option<Value> {
    for_backend(&body(input))
        .map(|out| json::from_slice::<Value>(&out).expect("json")["input"].take())
}

fn hofund(value: &Value) -> (Option<Value>, bool) {
    let (out, compact) = for_hofund(value.to_string().as_bytes());
    (
        out.map(|out| json::from_slice(&out).expect("json")),
        compact,
    )
}

fn user(text: &str) -> Value {
    json!({"type": "message", "role": "user", "content": [{"type": "input_text", "text": text}]})
}

fn opened(summary: &str) -> Value {
    user(&format!("{SUMMARY_PREFIX}\n{summary}"))
}

const SEALED_REASONING: &str = "gAAAA";

#[test]
fn the_mark_and_the_prompts_are_hofunds_and_codexs() {
    assert_eq!(MARK, "hofund1:");
    assert_eq!(sealed("hi"), "hofund1:hi");
    assert!(SUMMARY_PREFIX.starts_with("Another language model started to solve this problem"));
    assert!(SUMMARY_PREFIX.ends_with("to assist with your own analysis:"));
    assert!(COMPACT_PROMPT.starts_with("You are performing a CONTEXT CHECKPOINT COMPACTION."));
    assert!(COMPACT_PROMPT.ends_with("seamlessly continue the work."));
}

#[test]
fn a_body_with_nothing_to_put_right_goes_as_it_came() {
    let input = json!([
        {"type": "message", "id": "msg_1-a", "role": "user", "content": []},
        {"type": "custom_tool_call", "id": "ctc_1", "call_id": "c".repeat(64)},
        {"type": "tool_search_call", "id": "", "call_id": "x"},
        {"type": "reasoning", "id": "rs_1", "encrypted_content": SEALED_REASONING, "content": []},
        {"type": "reasoning", "id": "rs_2", "encrypted_content": SEALED_REASONING, "content": null},
        {"type": "compaction", "encrypted_content": "gAAAAnative"},
        {"type": "compaction_trigger"},
    ]);
    assert_eq!(backend(&input), None);
    assert_eq!(for_backend(b"not json"), None);
    assert_eq!(for_backend(br#"{"input":"hi"}"#), None);
    assert_eq!(for_backend(br#"{"model":"m"}"#), None);
    assert_eq!(for_hofund(b"not json"), (None, false));
    assert_eq!(for_hofund(br#"{"input":"hi"}"#), (None, false));
    assert_eq!(
        for_hofund(&body(&json!([{"type": "reasoning", "content": ["x"]}]))),
        (None, false)
    );
}

#[test]
fn call_items_get_the_prefix_the_backend_wants() {
    let input = json!([
        {"type": "tool_search_call", "id": "fc_abc", "call_id": "call_1"},
        {"type": "custom_tool_call", "id": "abc", "call_id": "call_2"},
        {"type": "custom_tool_call", "id": "fc_", "call_id": "call_3"},
        {"type": "custom_tool_call", "id": "fc_a_b"},
        {"type": "function_call", "id": "fc_kept", "call_id": "call_4"},
    ]);
    assert_eq!(
        backend(&input).expect("changed"),
        json!([
            {"type": "tool_search_call", "id": "tsc_abc", "call_id": "call_1"},
            {"type": "custom_tool_call", "id": "ctc_abc", "call_id": "call_2"},
            {"type": "custom_tool_call", "id": "ctc_fc_", "call_id": "call_3"},
            {"type": "custom_tool_call", "id": "ctc_a_b"},
            {"type": "function_call", "id": "fc_kept", "call_id": "call_4"},
        ])
    );
}

#[test]
fn a_long_call_id_is_its_hash_for_the_call_and_its_output() {
    let long = "c".repeat(65);
    let hash = "b30855e510446ac34706c5acafeedefd5122fe9446b3f812db11e1ac3b4d0cf2";
    let input = json!([
        {"type": "function_call", "call_id": long},
        {"type": "function_call_output", "call_id": format!(" {long}\n")},
        {"type": "function_call", "call_id": "d".repeat(64)},
    ]);
    let out = backend(&input).expect("changed");
    assert_eq!(out[0]["call_id"], hash);
    assert_eq!(out[1]["call_id"], hash);
    assert_eq!(out[2]["call_id"], "d".repeat(64));
}

#[test]
fn item_ids_are_made_plain_and_foreign_reasoning_is_dropped() {
    let input = json!([
        {"type": "message", "id": "msg 1/\u{e9}", "role": "assistant"},
        {"type": "reasoning", "id": "rs:1", "encrypted_content": SEALED_REASONING},
        {"type": "reasoning", "id": "rs_ok-9", "encrypted_content": SEALED_REASONING},
        {"type": "custom_tool_call", "id": "ctc_a.b"},
    ]);
    assert_eq!(
        backend(&input).expect("changed"),
        json!([
            {"type": "message", "id": "msg_1__", "role": "assistant"},
            {"type": "reasoning", "id": "rs_ok-9", "encrypted_content": SEALED_REASONING},
            {"type": "custom_tool_call", "id": "ctc_a_b"},
        ])
    );
    let dropped =
        json!([{"type": "reasoning", "id": "rs.1", "encrypted_content": SEALED_REASONING}]);
    assert_eq!(backend(&dropped), Some(json!([])));
}

#[test]
fn reasoning_written_elsewhere_does_not_go_to_codexs_own_models() {
    let input = json!([
        {"type": "reasoning", "id": "a", "encrypted_content": SEALED_REASONING, "content": ["shown"]},
        {"type": "reasoning", "id": "b", "encrypted_content": SEALED_REASONING, "content": "shown"},
        {"type": "reasoning", "id": "c", "encrypted_content": ""},
        {"type": "reasoning", "id": "d"},
        {"type": "reasoning", "id": "e", "encrypted_content": SEALED_REASONING},
    ]);
    assert_eq!(
        backend(&input).expect("changed"),
        json!([{"type": "reasoning", "id": "e", "encrypted_content": SEALED_REASONING}])
    );
}

#[test]
fn hofunds_summaries_are_opened_and_others_are_kept() {
    let input = json!([
        {"type": "compaction", "id": "cmp_1", "encrypted_content": sealed("first")},
        {"type": "compaction_summary", "encrypted_content": sealed("second")},
        {"type": "compaction", "encrypted_content": MARK},
        {"type": "compaction", "encrypted_content": "gAAAAnative"},
        {"type": "message", "id": "m", "role": "user"},
    ]);
    let expected = json!([
        opened("first"),
        opened("second"),
        opened(""),
        {"type": "compaction", "encrypted_content": "gAAAAnative"},
        {"type": "message", "id": "m", "role": "user"},
    ]);
    assert_eq!(backend(&input).expect("changed"), expected);
    let (out, compact) = hofund(&json!({"model": "a/b", "input": input, "tools": [{"type": "x"}]}));
    assert!(!compact);
    let out = out.expect("changed");
    assert_eq!(out["input"], expected);
    assert_eq!(out["tools"], json!([{"type": "x"}]));
    assert_eq!(out.get("stream"), None);
}

#[test]
fn a_hofund_models_trigger_asks_for_a_summary_without_tools() {
    let (out, compact) = hofund(&json!({
        "model": "a/b",
        "stream": true,
        "tools": [{"type": "function", "name": "shell"}],
        "tool_choice": "required",
        "parallel_tool_calls": true,
        "input": [
            {"type": "message", "role": "user", "content": []},
            {"type": "reasoning", "content": ["kept for hofund"]},
            {"type": "additional_tools", "tools": []},
            {"type": "compaction_trigger"},
        ],
    }));
    assert!(compact);
    assert_eq!(
        out.expect("changed").to_string(),
        json!({
            "model": "a/b",
            "stream": false,
            "tools": [],
            "tool_choice": "auto",
            "parallel_tool_calls": false,
            "input": [
                {"type": "message", "role": "user", "content": []},
                {"type": "reasoning", "content": ["kept for hofund"]},
                user(COMPACT_PROMPT),
            ],
        })
        .to_string()
    );
}

#[test]
fn additional_tools_stay_when_there_is_nothing_to_compact() {
    let input = json!([
        {"type": "additional_tools", "tools": []},
        {"type": "compaction", "encrypted_content": sealed("s")},
    ]);
    let (out, compact) = hofund(&json!({"model": "a/b", "input": input}));
    assert!(!compact);
    assert_eq!(
        out.expect("changed")["input"],
        json!([{"type": "additional_tools", "tools": []}, opened("s")])
    );
}

#[test]
fn the_rest_of_the_body_keeps_its_order() {
    let raw = br#"{"z":1,"input":[{"type":"custom_tool_call","id":"x"}],"a":2}"#;
    assert_eq!(
        for_backend(raw).expect("changed"),
        br#"{"z":1,"input":[{"type":"custom_tool_call","id":"ctc_x"}],"a":2}"#
    );
}

#[test]
fn d35_1_sixty_four_char_ids_stay_and_sixty_five_are_bounded() {
    let ok = "a".repeat(64);
    let long = format!("{ok}x");
    let input = json!([{"type": "message", "id": ok, "role": "user", "content": []}]);
    assert_eq!(backend(&input), None);
    let input = json!([{"type": "message", "id": long, "role": "user", "content": []}]);
    let out = backend(&input).expect("changed");
    assert_eq!(out[0]["id"].as_str().expect("id").len(), 64);
    assert!(out[0]["id"].as_str().expect("id").starts_with("msg_"));
}

#[test]
fn d35_2_bounded_ids_are_deterministic_and_idempotent() {
    let id = format!("{}tail", "z".repeat(80));
    let item = json!([{"type": "message", "id": id, "role": "user", "content": []}]);
    let first = backend(&item).expect("changed")[0]["id"].clone();
    let again = backend(&item).expect("changed")[0]["id"].clone();
    assert_eq!(first, again);
    let raw = json!({"model": "m", "input": item}).to_string();
    let first = for_backend(raw.as_bytes()).expect("once");
    let second = for_backend(raw.as_bytes()).expect("again");
    assert_eq!(first, second);
}

#[test]
fn d35_3_ids_with_the_same_start_map_apart() {
    let shared = "q".repeat(80);
    let a = format!("{shared}a");
    let b = format!("{shared}b");
    let left = backend(&json!([{"type": "message", "id": a, "role": "user", "content": []}]))
        .expect("a")[0]["id"]
        .clone();
    let right = backend(&json!([{"type": "message", "id": b, "role": "user", "content": []}]))
        .expect("b")[0]["id"]
        .clone();
    assert_ne!(left, right);
}

#[test]
fn d35_4_vendor_reasoning_keeps_an_overlong_id() {
    let id = format!("rs_{}", "x".repeat(70));
    let input = json!([{
        "type": "reasoning",
        "id": id,
        "encrypted_content": SEALED_REASONING,
        "content": []
    }]);
    assert_eq!(backend(&input), None);
    assert_eq!(input[0]["id"], id);
    assert_eq!(input[0]["encrypted_content"], SEALED_REASONING);
}

#[test]
fn d35_5_vendor_reasoning_keeps_an_overlong_non_plain_id() {
    let id = format!("rs_{}:{}", "a".repeat(60), "b".repeat(10));
    assert!(id.len() > 64);
    assert!(id.contains(':'));
    let input = json!([{
        "type": "reasoning",
        "id": id,
        "encrypted_content": SEALED_REASONING,
        "content": []
    }]);
    assert_eq!(backend(&input), None);
    assert_eq!(input[0]["id"], id);
    assert_eq!(input[0]["encrypted_content"], SEALED_REASONING);
}

#[test]
fn d66_5_for_backend_heals_offsetless_url_citation() {
    use super::input::item_cites_without_offsets;
    let bad = json!({"type":"url_citation","title":"V","url":"https://example.com/v"});
    let good = json!({"type":"url_citation","start_index":90,"end_index":152,"url":"https://example.com/g"});
    let input = json!([{
        "type": "message",
        "role": "assistant",
        "content": [{
            "type": "output_text",
            "text": "Sun.",
            "annotations": [good, bad],
        }],
    }]);
    assert!(item_cites_without_offsets(&input[0]));
    let out = backend(&input).expect("healed");
    let text = out.to_string();
    assert!(!text.contains("site_name"), "{text}");
    assert!(text.contains("example.com/g"), "{text}");
}

#[test]
fn d66_6_input_with_only_good_citations_is_unchanged() {
    let good = json!({"type":"url_citation","start_index":0,"end_index":1,"url":"u"});
    let input = json!([{
        "type": "message",
        "content": [{"type":"output_text","text":"x","annotations":[good]}],
    }]);
    assert_eq!(backend(&input), None);
}
