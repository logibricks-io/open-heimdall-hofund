//! Which clients compact on a Responses stream failure, and the failure text.

use hofund_codec::{Codec, OpenAiResponses, compacts_in_stream_client};

fn marks(headers: &[(&str, &str)]) -> bool {
    compacts_in_stream_client(|name| {
        headers
            .iter()
            .find(|(key, _)| key.eq_ignore_ascii_case(name))
            .map(|(_, value)| *value)
    })
}

#[test]
fn d1l_client_prefixes_are_the_reference_spellings() {
    assert!(marks(&[("user-agent", "codex_cli_rs/0.162.3")]));
    assert!(marks(&[("user-agent", "acp-extension-codex/1.0")]));
    assert!(marks(&[("originator", "Codex_Desktop")]));
    assert!(!marks(&[("user-agent", "other-agent/1.0")]));
}

#[test]
fn d1m_metadata_headers_count_only_from_the_plugin_host() {
    for header in ["x-codex-window-id", "x-codex-turn-metadata"] {
        assert!(marks(&[
            ("user-agent", "claude code/1.0"),
            (header, "present")
        ]));
        assert!(!marks(&[
            ("user-agent", "other-agent/1.0"),
            (header, "present")
        ]));
        assert!(!marks(&[(header, "present")]));
    }
}

#[test]
fn d1n_byte_cap_hint_is_added_once() {
    let plain =
        String::from_utf8(OpenAiResponses.encode_stream_error(413, "too big")).expect("utf8");
    assert_eq!(plain.matches("request_too_large").count(), 1, "{plain}");
    let marked =
        String::from_utf8(OpenAiResponses.encode_stream_error(413, "too big (request_too_large)"))
            .expect("utf8");
    assert_eq!(marked.matches("request_too_large").count(), 1, "{marked}");
}

#[test]
fn d1o_unescaped_error_text_is_read_as_sent() {
    let body = r#"{"error":{"message":"plain words"}}"#;
    assert_eq!(hofund_ir::words_of(body), body);
}
