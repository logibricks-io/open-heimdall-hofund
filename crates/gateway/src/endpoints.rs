//! Path matching for the Generative Language API.
//!
//! The HTTP router cannot register a dynamic suffix, so
//! `{model}:generateContent` is not a routable pattern. The process binds
//! [`GENERATIVE_CATCHALL`] instead. [`GENERATIVE_UNARY`] and
//! [`GENERATIVE_STREAM`] stay the codec-map keys: the handler parses the
//! concrete path and looks the codec up with one of those keys.
//!
//! `{model}` is one path segment. A slash in the model id does not match.

use serde_json::Value;

/// Codec-map key for a unary Generative Language request.
///
/// The bound route is `GENERATIVE_CATCHALL`. The handler copies the model
/// segment onto the JSON body before the codec runs, and it removes a `stream`
/// field so this path stays unary.
pub const GENERATIVE_UNARY: &str = "/v1beta/models/{model}:generateContent";

/// Codec-map key for a streamed Generative Language request.
///
/// The bound route is `GENERATIVE_CATCHALL`. `streamGenerateContent` is a
/// stream even when the query does not contain `alt=sse`. The handler sets
/// `stream` to true on the JSON body before the codec runs.
pub const GENERATIVE_STREAM: &str = "/v1beta/models/{model}:streamGenerateContent";

/// `GET` path that lists models for the Generative Language API.
pub const GENERATIVE_LIST: &str = "/v1beta/models";

/// Live `POST` route that covers both generate methods.
pub(crate) const GENERATIVE_CATCHALL: &str = "/v1beta/models/{*target}";

pub(crate) struct GenerateRoute {
    pub model: String,
    pub stream: bool,
    pub key: &'static str,
}

/// Parses `/v1beta/models/{model}:countTokens`.
pub(crate) fn match_count(path: &str) -> Option<String> {
    let rest = path.strip_prefix("/v1beta/models/")?;
    let encoded = rest.strip_suffix(":countTokens")?;
    let model = percent_decode(encoded);
    if model.is_empty() || model.contains('/') {
        return None;
    }
    Some(model)
}

/// Parses `/v1beta/models/{model}:generateContent` and the stream method.
pub(crate) fn match_generate(path: &str) -> Option<GenerateRoute> {
    let rest = path.strip_prefix("/v1beta/models/")?;
    let (encoded, stream, key) = if let Some(model) = rest.strip_suffix(":streamGenerateContent") {
        (model, true, GENERATIVE_STREAM)
    } else {
        let model = rest.strip_suffix(":generateContent")?;
        (model, false, GENERATIVE_UNARY)
    };
    let model = percent_decode(encoded);
    if model.is_empty() || model.contains('/') {
        return None;
    }
    Some(GenerateRoute { model, stream, key })
}

/// Writes the path model onto the body. The path wins when the body already
/// has `model`. `stream` is set for the stream method and removed otherwise.
///
/// A body that is not a JSON object is returned unchanged so the codec can
/// report the parse error.
pub(crate) fn inject_target(bytes: &[u8], model: &str, stream: bool) -> Vec<u8> {
    let Ok(Value::Object(mut object)) = serde_json::from_slice::<Value>(bytes) else {
        return bytes.to_vec();
    };
    object.insert("model".to_owned(), Value::String(model.to_owned()));
    if stream {
        object.insert("stream".to_owned(), Value::Bool(true));
    } else {
        object.remove("stream");
    }
    serde_json::to_vec(&Value::Object(object)).unwrap_or_else(|_| bytes.to_vec())
}

/// Decodes `%HH` sequences. Invalid sequences are copied through.
pub(crate) fn percent_decode(input: &str) -> String {
    let bytes = input.as_bytes();
    let mut out = Vec::with_capacity(bytes.len());
    let mut index = 0;
    while index < bytes.len() {
        if bytes[index] == b'%'
            && let Some(decoded) =
                hex_byte(bytes.get(index + 1).copied(), bytes.get(index + 2).copied())
        {
            out.push(decoded);
            index += 3;
            continue;
        }
        out.push(bytes[index]);
        index += 1;
    }
    String::from_utf8_lossy(&out).into_owned()
}

fn hex_byte(high: Option<u8>, low: Option<u8>) -> Option<u8> {
    Some((hex_value(high?)? << 4) | hex_value(low?)?)
}

fn hex_value(byte: u8) -> Option<u8> {
    match byte {
        b'0'..=b'9' => Some(byte - b'0'),
        b'a'..=b'f' => Some(byte - b'a' + 10),
        b'A'..=b'F' => Some(byte - b'A' + 10),
        _ => None,
    }
}

/// Reads the first `key` query value, percent-decoded.
pub(crate) fn query_key(query: &str) -> Option<String> {
    for pair in query.split('&') {
        let Some((name, value)) = pair.split_once('=') else {
            continue;
        };
        if name == "key" {
            return Some(percent_decode(value));
        }
    }
    None
}

#[cfg(test)]
mod tests {
    use super::{inject_target, match_generate, percent_decode, query_key};

    #[test]
    fn matches_unary_and_stream_methods() {
        let unary = match_generate("/v1beta/models/demo:generateContent").expect("unary");
        assert_eq!(unary.model, "demo");
        assert!(!unary.stream);
        let stream = match_generate("/v1beta/models/demo:streamGenerateContent").expect("stream");
        assert_eq!(stream.model, "demo");
        assert!(stream.stream);
        assert!(match_generate("/v1beta/models/:generateContent").is_none());
        assert!(match_generate("/v1beta/models/a/b:generateContent").is_none());
        assert!(match_generate("/v1/models").is_none());
        assert_eq!(percent_decode("a%2Fb"), "a/b");
        assert!(match_generate("/v1beta/models/a%2Fb:generateContent").is_none());
    }

    #[test]
    fn path_model_replaces_a_body_model() {
        let bytes = inject_target(
            br#"{"model":"other","stream":true,"contents":[]}"#,
            "demo",
            false,
        );
        let value: serde_json::Value = serde_json::from_slice(&bytes).expect("json");
        assert_eq!(value["model"], "demo");
        assert!(value.get("stream").is_none());
        let bytes = inject_target(br#"{"contents":[]}"#, "demo", true);
        let value: serde_json::Value = serde_json::from_slice(&bytes).expect("json");
        assert_eq!(value["model"], "demo");
        assert_eq!(value["stream"], true);
        assert_eq!(query_key("alt=sse&key=abc%2B"), Some("abc+".to_owned()));
    }
}
