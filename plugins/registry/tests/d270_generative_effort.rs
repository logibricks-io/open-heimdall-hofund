//! Advertised-level fitting for the custom Generative Language provider path.

use hofund_kit_upstream::{TestHost, block_on};
use hofund_plugin_api::{Effort, PluginId, ProtocolId, Request};
use hofund_plugin_registry::generative;
use hofund_provider::Endpoint;
use serde_json::Value;

const REPLY: &str = r#"{"candidates":[{"content":{"role":"model","parts":[{"text":"hello"}]},"finishReason":"STOP"}]}"#;

#[test]
fn d270_custom_generative_sends_xhigh_only_when_advertised() {
    block_on(async {
        let dir = tempfile::tempdir().expect("scratch");
        let host = TestHost::open(PluginId::from_static("generative"), dir.path());
        let provider = generative(
            Endpoint::new("custom", "http://upstream.test/v1beta"),
            host.context(),
        )
        .expect("provider");
        for (model, expected) in [
            ("gemini-4-neon", "xhigh"),
            ("gemini-4-argon", "high"),
            ("gemini-4-unknown", "high"),
        ] {
            host.http().enqueue(REPLY);
            let mut request = Request::new(model);
            request.params.effort = Some(Effort::Xhigh);
            provider
                .execute(request, ProtocolId::OPENAI_CHAT)
                .await
                .expect("completion");
            let calls = host.http().calls();
            let sent: Value =
                serde_json::from_slice(calls.last().expect("call").body()).expect("request json");
            assert_eq!(
                sent["generationConfig"]["thinkingConfig"]["thinkingLevel"], expected,
                "{model}"
            );
        }
    });
}
