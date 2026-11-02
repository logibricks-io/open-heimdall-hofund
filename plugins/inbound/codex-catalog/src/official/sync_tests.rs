//! Embedded copies stay aligned with the checked-in testdata snapshot.

use std::path::PathBuf;

use hofund_plugin_sdk::json;

use super::UNKNOWN_MODEL_PROMPT;

const TESTDATA: &str = concat!(env!("CARGO_MANIFEST_DIR"), "/testdata/codex-rust-v0.166.0");

#[test]
fn d34_7_embedded_instructions_match_testdata() {
    let bundled = include_str!("codex_instructions.json");
    let prompt = include_str!("prompt.md");
    let models_path = PathBuf::from(TESTDATA).join("models.json");
    let prompt_path = PathBuf::from(TESTDATA).join("prompt.md");
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
    assert_eq!(
        prompt,
        std::fs::read_to_string(prompt_path).expect("prompt.md")
    );
    assert_eq!(prompt, UNKNOWN_MODEL_PROMPT);
}
