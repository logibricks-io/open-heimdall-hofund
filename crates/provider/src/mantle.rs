//! Per-model prefix for Amazon Bedrock's Mantle endpoint.
//!
//! Mantle's chat completions and Responses API use two prefixes on
//! `bedrock-mantle.<region>.api.aws`. A closed OpenAI GPT id and an `xai.`
//! id are under `/openai/v1`. Every other model is under `/v1`. Messages stay
//! on `/anthropic` and do not use this module.
//!
//! Hosts: <https://docs.aws.amazon.com/bedrock/latest/userguide/endpoints.html>
//! (checked 2026-10-25). The prefix split is the rule each model's card
//! follows; a model asked at the other prefix is refused with a message that
//! it is not supported on that route. The API key stays a bearer token
//! (<https://docs.aws.amazon.com/bedrock/latest/userguide/api-keys-use.html>).

use std::collections::HashMap;
use std::sync::Mutex;

/// Learned prefix overrides. `true` means the model was refused at the
/// guessed prefix, so the next call uses the other one.
#[derive(Debug, Default)]
pub(crate) struct LearnedRoutes {
    other: Mutex<HashMap<String, bool>>,
}

impl LearnedRoutes {
    pub(crate) fn other(&self, model: &str) -> bool {
        self.other
            .lock()
            .unwrap_or_else(|error| error.into_inner())
            .get(model)
            .copied()
            .unwrap_or(false)
    }

    pub(crate) fn learn_other(&self, model: &str) {
        self.other
            .lock()
            .unwrap_or_else(|error| error.into_inner())
            .insert(model.to_owned(), true);
    }

    pub(crate) fn forget(&self, model: &str) {
        self.other
            .lock()
            .unwrap_or_else(|error| error.into_inner())
            .remove(model);
    }
}

/// Whether `base` is a Mantle chat or Responses root this adapter may retarget.
///
/// The host is `bedrock-mantle.<region>.api.aws`. The path ends in `/v1` or
/// `/openai/v1`. A proxy host, the Bedrock runtime host, and any other path
/// are left alone.
pub(crate) fn is_mantle_root(base: &str) -> bool {
    let Some(root) = v1_root(base) else {
        return false;
    };
    let Some(host) = host_of(&root) else {
        return false;
    };
    host.starts_with("bedrock-mantle.") && host.ends_with(".api.aws")
}

/// The base a Mantle chat or Responses call for `model` uses.
///
/// `other` selects the prefix the id rule would not. A base that does not end
/// in `/v1` is returned unchanged.
pub(crate) fn mantle_base(base: &str, model: &str, other: bool) -> String {
    let Some(root) = v1_root(base) else {
        return base.to_owned();
    };
    if closed_model(model) != other {
        format!("{root}/openai/v1")
    } else {
        format!("{root}/v1")
    }
}

/// A 4xx whose text says this prefix does not serve the model.
pub(crate) fn wrong_route(status: Option<u16>, message: &str) -> bool {
    let Some(status) = status else {
        return false;
    };
    if !(400..500).contains(&status) {
        return false;
    }
    let lower = message.to_ascii_lowercase();
    lower.contains("isn't supported on this route")
        || lower.contains("is not supported on this route")
        || lower.contains("not supported on this route")
}

/// OpenAI's closed GPT ids (`openai.gpt-` and not `gpt-oss`) and `xai.` ids.
fn closed_model(model: &str) -> bool {
    let model = model.to_ascii_lowercase();
    model.starts_with("xai.") || (model.contains("openai.gpt-") && !model.contains("gpt-oss"))
}

/// Scheme and host of a base that ends in `/v1`, with a trailing `/openai` removed.
fn v1_root(base: &str) -> Option<String> {
    let trimmed = base.trim().trim_end_matches('/');
    let without_v1 = trimmed.strip_suffix("/v1")?;
    Some(
        without_v1
            .strip_suffix("/openai")
            .unwrap_or(without_v1)
            .to_owned(),
    )
}

fn host_of(url: &str) -> Option<String> {
    let rest = url.split_once("://")?.1;
    let host = rest.split(['/', '?', '#']).next().unwrap_or(rest);
    let host = host.rsplit('@').next().unwrap_or(host);
    let host = host.split(':').next().unwrap_or(host);
    if host.is_empty() {
        None
    } else {
        Some(host.to_ascii_lowercase())
    }
}

#[cfg(test)]
mod tests {
    use super::{closed_model, is_mantle_root, mantle_base, wrong_route};

    const V1: &str = "https://bedrock-mantle.us-east-1.api.aws/v1";
    const OPENAI: &str = "https://bedrock-mantle.us-east-1.api.aws/openai/v1";

    #[test]
    fn a_closed_model_uses_the_openai_prefix() {
        let cases = [
            (V1, "openai.gpt-oss-120b", false, V1),
            (V1, "google.gemma-3-27b-it", false, V1),
            (V1, "openai.gpt-5.5", false, OPENAI),
            (V1, "openai.gpt-6-sol", false, OPENAI),
            (V1, "xai.grok-4.3", false, OPENAI),
            (V1, "XAI.grok-4.3", false, OPENAI),
            (OPENAI, "openai.gpt-oss-120b", false, V1),
            (
                "https://bedrock-mantle.us-east-1.api.aws/openai/v1/",
                "xai.grok-4.3",
                false,
                OPENAI,
            ),
            (V1, "newmaker.model-1", true, OPENAI),
            (V1, "openai.gpt-5.5", true, V1),
            (
                "https://proxy.example.com/mantle",
                "openai.gpt-5.5",
                false,
                "https://proxy.example.com/mantle",
            ),
        ];
        for (base, model, other, want) in cases {
            assert_eq!(
                mantle_base(base, model, other),
                want,
                "{model} other={other}"
            );
        }
    }

    #[test]
    fn only_a_mantle_host_is_retargeted() {
        assert!(is_mantle_root(V1));
        assert!(is_mantle_root(OPENAI));
        assert!(is_mantle_root(
            "https://bedrock-mantle.eu-west-1.api.aws/openai/v1/"
        ));
        assert!(!is_mantle_root(
            "https://bedrock-runtime.us-east-1.amazonaws.com/openai/v1"
        ));
        assert!(!is_mantle_root("https://bedrock-mantle.example.com/v1"));
        assert!(!is_mantle_root("https://api.aws/v1"));
        assert!(!is_mantle_root(
            "https://relay.example.com/bedrock-mantle.us-east-1.api.aws/v1"
        ));
        assert!(!is_mantle_root("https://api.openai.com/v1"));
    }

    #[test]
    fn the_route_refusal_is_a_client_error_that_names_the_route() {
        assert!(wrong_route(
            Some(400),
            "model `x` isn't supported on this route"
        ));
        assert!(wrong_route(
            Some(404),
            "Model is not supported on this route"
        ));
        assert!(wrong_route(Some(499), "not supported on this route"));
        assert!(!wrong_route(Some(500), "isn't supported on this route"));
        assert!(!wrong_route(Some(400), "invalid model id"));
        assert!(!wrong_route(None, "isn't supported on this route"));
        assert!(closed_model("global.openai.gpt-6-luna"));
        assert!(!closed_model("openai.gpt-oss-20b-1:0"));
    }
}
