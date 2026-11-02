//! Embedded copies stay aligned with the checked-in testdata snapshot.

use std::path::PathBuf;

use hofund_plugin_sdk::json;

use super::unknown_model_prompt;

const TESTDATA: &str = concat!(env!("CARGO_MANIFEST_DIR"), "/testdata/codex-rust-v0.166.0");

#[test]
fn d34_7_embedded_instructions_match_testdata() {
    let bundled = include_str!("codex_instructions.json");
    let models_path = PathBuf::from(TESTDATA).join("models.json");
    let prompt_path = PathBuf::from(TESTDATA).join("prompt.json");
    let models: json::Value =
        json::from_slice(&std::fs::read(models_path).expect("models.json")).expect("json");
    let mut entries = Vec::new();
    for row in models["models"].as_array().expect("models array") {
        entries.push(json::json!({
            "slug": row["slug"],
            "instructions_template": row["model_messages"]["instructions_template"],
        }));
    }
    let expected = json::json!({"entries": entries});
    let got: json::Value = json::from_str(bundled).expect("embedded json");
    assert_eq!(got, expected);
    let snapshot: json::Value =
        json::from_slice(&std::fs::read(prompt_path).expect("prompt.json")).expect("json");
    assert_eq!(snapshot["prompt"].as_str(), Some(unknown_model_prompt()));
    // byte for byte the upstream prompt.md (20903 bytes), its non-ASCII kept
    assert_eq!(unknown_model_prompt().len(), 20903);
    assert!(unknown_model_prompt().starts_with("You are a coding agent"));
    assert!(unknown_model_prompt().contains("\u{3010}F:README.md\u{2020}L5-L14\u{3011}"));
}
