use std::sync::LazyLock;

use regex::Regex;

fn re(pattern: &str) -> Regex {
    Regex::new(pattern).expect("an error word list compiles")
}

/// The conversation no longer fits the model. The Chinese clauses name the
/// context, or what was sent (content, input, text, prompt, question,
/// conversation, message), beside its length, tokens, or character count.
static TOO_LONG: LazyLock<Regex> = LazyLock::new(|| {
    re(concat!(
        "(?i)context_length_exceeded|prompt is too long|input is too long|(exceeds?|exceeded|over|beyond)( the)?( model'?s?)?( maximum)? context|context (length|limit|window) (exceeded|is exceeded)|maximum context length|too many (input |prompt )?tokens|\u{4e0a}\u{4e0b}\u{6587}(\u{957f}\u{5ea6})?(\u{8d85}|\u{8fc7}\u{957f})|\u{8d85}(\u{8fc7}|\u{51fa})(\u{4e86})?(\u{6a21}\u{578b})?(\u{7684})?(\u{6700}\u{5927})?\u{4e0a}\u{4e0b}\u{6587}",
        "|input token count.*exceeds the maximum|maximum prompt length is [0-9]|reduce the length of the messages|maximum allowed input length|longer than the model'?s context length|available context size|greater than the context length|prompt token count of [0-9,]+ exceeds the limit|context window exceeds limit|exceeded model token limit|configured context size|prompt too long|prompt exceeds max length|context_window_exceeded|exceeded (max |maximum )?context length|range of input length should be|token limit exceeded|input tokens? (count )?(exceeds?|exceeded) (the )?(max(imum)? )?limit",
        "|context (window )?(is )?full",
        "|(\u{5185}\u{5bb9}|\u{8f93}\u{5165}|\u{6587}\u{672c}|\u{63d0}\u{793a}\u{8bcd}?|prompt|\u{95ee}\u{9898}|\u{5bf9}\u{8bdd}|\u{6d88}\u{606f})(\u{5185}\u{5bb9}|\u{6587}\u{672c})?(\u{957f}\u{5ea6})?\\s*(\u{592a}\u{957f}|\u{8fc7}\u{957f}|\u{8d85}\u{957f})|\u{5b57}\u{6570}(\u{592a}\u{591a}|\u{8fc7}\u{591a})",
        "|(\u{5185}\u{5bb9}|\u{8f93}\u{5165}|\u{6587}\u{672c}|\u{63d0}\u{793a}\u{8bcd}?|prompt|\u{95ee}\u{9898}|\u{5bf9}\u{8bdd}|\u{6d88}\u{606f})[^\u{ff0c}\u{3002},.;\u{ff1b}!\u{ff01}]{0,12}\u{8d85}(\u{8fc7}|\u{51fa})[^\u{ff0c}\u{3002},.;\u{ff1b}!\u{ff01}]{0,12}(\u{957f}\u{5ea6}|token|\u{4ee4}\u{724c}|\u{5b57}\u{6570})",
        "|(\u{8f93}\u{5165}|\u{63d0}\u{793a}\u{8bcd}?|prompt)\\s*\u{7684}?\\s*(\u{957f}\u{5ea6}|token|\u{4ee4}\u{724c}|\u{5b57}\u{6570})[^\u{ff0c}\u{3002},.;\u{ff1b}!\u{ff01}]{0,8}\u{8d85}(\u{8fc7}|\u{51fa})",
    ))
});

/// A limit on how fast or how much, not on one prompt's size. The Chinese
/// words are pace, frequent, frequency, throttling, concurrency, per
/// (second, minute, hour, day), and try again later.
static PACE_WORDS: LazyLock<Regex> = LazyLock::new(|| {
    re(
        "(?i)rate.?limit|per (minute|hour|day)|(?-u:\\b)TP[MD](?-u:\\b)|please wait|try again later|throttl|\u{901f}\u{7387}|\u{9891}\u{7e41}|\u{9891}\u{7387}|\u{9650}\u{6d41}|\u{5e76}\u{53d1}|\u{6bcf}(\u{79d2}|\u{5206}\u{949f}|\u{5c0f}\u{65f6}|\u{5929}|\u{65e5})|\u{7a0d}\u{540e}(\u{518d}\u{8bd5}|\u{91cd}\u{8bd5})",
    )
});

/// Whether a 4xx message says the conversation no longer fits the model.
/// Output-token caps and pace limits are not input overflows.
#[must_use]
pub fn too_long(status: u16, text: &str) -> bool {
    if !(400..500).contains(&status) || status == 429 {
        return false;
    }
    let lower = text.to_lowercase();
    if ["max_tokens", "max_output_tokens", "max_completion_tokens"]
        .into_iter()
        .any(|word| lower.contains(word))
        || PACE_WORDS.is_match(text)
    {
        return false;
    }
    TOO_LONG.is_match(text)
}

/// The text with each string of its JSON appended on its own line, when the
/// text escapes any as `\u` (`reference: internal/catalog/catalog.go:1653`).
#[must_use]
pub fn words_of(text: &str) -> String {
    if !text.contains("\\u") {
        return text.to_owned();
    }
    let Ok(value) = serde_json::from_str::<serde_json::Value>(text) else {
        return text.to_owned();
    };
    let mut out = text.to_owned();
    let mut stack = vec![&value];
    while let Some(value) = stack.pop() {
        match value {
            serde_json::Value::String(string) => {
                out.push('\n');
                out.push_str(string);
            }
            serde_json::Value::Array(items) => stack.extend(items.iter().rev()),
            serde_json::Value::Object(map) => stack.extend(map.values().rev()),
            _ => {}
        }
    }
    out
}

/// Whether an upstream failure is the vendor too busy for the request: HTTP
/// 529, or a 5xx whose message says overloaded. Rate limits and 4xx are not
/// overloads even when the text mentions busy servers.
#[must_use]
pub fn vendor_overloaded(status: u16, message: &str) -> bool {
    status == 529
        || (status >= 500
            && words_of(message)
                .to_ascii_lowercase()
                .contains("overloaded"))
}

#[cfg(test)]
mod tests {
    use super::{too_long, vendor_overloaded};

    #[test]
    fn d33_1_context_window_full_wording_is_overflow() {
        for message in [
            "Context window is full. Reduce conversation history, system prompt, or tools. (request id: abc-123)",
            "Context window is full. Reduce conversation history, system prompt, or tools.",
            "Input is too long. Reduce the size of your messages.",
            r#"{"error":{"message":"Context window is full. Reduce conversation history, system prompt, or tools.","type":"invalid_request_error"}}"#,
            r#"{"type":"error","error":{"message":"Input is too long. Reduce the size of your messages."}}"#,
        ] {
            assert!(too_long(400, message), "{message}");
            assert!(too_long(413, message), "{message}");
            assert!(too_long(422, message), "{message}");
            assert!(!too_long(500, message), "{message}");
        }
    }

    #[test]
    fn d33_2_ordinary_bad_requests_are_not_overflow() {
        for message in [
            r#"{"error":{"message":"Invalid model: demo"}}"#,
            r#"{"error":{"message":"The model `demo` does not exist","code":"model_not_found"}}"#,
            r#"{"error":{"message":"you must provide a messages parameter"}}"#,
        ] {
            assert!(!too_long(400, message), "{message}");
        }
    }

    #[test]
    fn d52_said_overloaded_matches_reference_table() {
        let cases = [
            (529, "busy", true),
            (503, "The model is overloaded", true),
            (503, "Service Unavailable", false),
            (502, "connect", false),
            (429, "overloaded", false),
            (400, "overloaded", false),
        ];
        for (status, message, want) in cases {
            assert_eq!(
                vendor_overloaded(status, message),
                want,
                "status={status} message={message:?}"
            );
        }
    }
}
