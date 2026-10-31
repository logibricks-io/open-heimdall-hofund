//! Messages object-root schema tests.

use hofund_codec::{AnthropicMessages, Codec};
use hofund_ir::{Extensions, Request, Tool};
use serde_json::{Value, json};

#[test]
fn v4_messages_encoder_folds_root_union() {
    let schema = json!({"type":"object","properties":{"pattern":{"type":"string"},"cursor":{"type":"string"}},"oneOf":[{"required":["pattern"],"not":{"required":["cursor"]}},{"required":["cursor"],"not":{"required":["pattern"]}}]});
    let request = Request::new("m").with_tools(vec![Tool {
        name: "rg".to_owned(),
        description: Some("search".to_owned()),
        input_schema: schema,
        extensions: Extensions::new(),
    }]);
    let body = AnthropicMessages
        .encode_request(&request)
        .expect("Messages request");
    let body: Value = serde_json::from_slice(&body).expect("JSON");
    let parameters = &body["tools"][0]["input_schema"];
    assert_eq!(parameters["type"], "object");
    assert!(parameters.get("oneOf").is_none());
    assert!(parameters.get("required").is_none());
    assert_eq!(parameters["properties"]["pattern"]["type"], "string");
    assert_eq!(parameters["properties"]["cursor"]["type"], "string");
}
