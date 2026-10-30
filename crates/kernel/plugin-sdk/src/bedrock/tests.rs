use hofund_plugin_api::Request;
use serde_json::{Value, json};

use super::{beta_text, fit_betas, fit_headers, fit_request};

fn request(extensions: Value) -> Request {
    let mut request = Request::new("claude");
    if let Value::Object(map) = extensions {
        for (key, value) in map {
            request.extensions.insert(key, value);
        }
    }
    request
}

#[test]
fn betas_are_split_trimmed_renamed_and_deduplicated() {
    assert_eq!(
        fit_betas([
            " a , advanced-tool-use-2025-11-20,,",
            "prompt-caching-scope-2026-01-05",
            "redact-thinking-2026-02-12, a",
            "tool-search-tool-2025-10-19",
        ]),
        ["a", "tool-search-tool-2025-10-19"]
    );
    assert_eq!(fit_betas(["", " , "]), Vec::<String>::new());
}

#[test]
fn header_betas_are_fitted_and_an_empty_one_is_dropped() {
    let headers = vec![
        (
            "Anthropic-Beta".to_owned(),
            "x, x,prompt-caching-scope-2026-01-05".to_owned(),
        ),
        (
            "anthropic-beta".to_owned(),
            "redact-thinking-2026-02-12".to_owned(),
        ),
        ("x-team".to_owned(), "a, a".to_owned()),
    ];
    assert_eq!(
        fit_headers(&headers),
        vec![
            ("Anthropic-Beta".to_owned(), "x".to_owned()),
            ("x-team".to_owned(), "a, a".to_owned()),
        ]
    );
}

#[test]
fn the_body_drops_metadata_and_fits_its_beta_list() {
    let mut fitted = request(json!({
        "metadata": {"user_id": "u"},
        "anthropic_beta": ["a,b", 7, "redact-thinking-2026-02-12"],
        "other": 1,
    }));
    fit_request(&mut fitted);
    assert!(fitted.extensions.get("metadata").is_none());
    assert_eq!(
        fitted.extensions.get("anthropic_beta"),
        Some(&json!(["a", "b", "7"]))
    );
    assert_eq!(fitted.extensions.get("other"), Some(&json!(1)));

    let mut kept = request(json!({"anthropic_beta": ["a", "b"]}));
    fit_request(&mut kept);
    assert_eq!(
        kept.extensions.get("anthropic_beta"),
        Some(&json!(["a", "b"]))
    );

    let mut emptied = request(json!({"anthropic_beta": ["prompt-caching-scope-2026-01-05", null]}));
    fit_request(&mut emptied);
    assert!(emptied.extensions.get("anthropic_beta").is_none());

    let mut scalar = request(json!({"anthropic_beta": "a"}));
    fit_request(&mut scalar);
    assert_eq!(scalar.extensions.get("anthropic_beta"), Some(&json!("a")));
}

#[test]
fn non_string_betas_become_their_json_text() {
    assert_eq!(beta_text(&json!("a")), "a");
    assert_eq!(beta_text(&Value::Null), "");
    assert_eq!(beta_text(&json!(true)), "true");
    assert_eq!(beta_text(&json!({"k": 1})), r#"{"k":1}"#);
}
