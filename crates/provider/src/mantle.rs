//! Per-model prefix for Amazon Bedrock's Mantle endpoint.
//!
//! Mantle's chat completions and Responses API use two prefixes on
//! `bedrock-mantle.<region>.api.aws`. A closed OpenAI GPT id and an `xai.`
//! id are under `/openai/v1`. Every other model is under `/v1`. Messages stay
//! on `/anthropic` and do not use this module.
//!
//! Hosts: <https://docs.aws.amazon.com/bedrock/latest/userguide/endpoints.html>
//! (checked 2026-10-27). The prefix split is the rule each model's card
//! follows; a model asked at the other prefix is refused with a message that
//! it is not supported on that route. The API key stays a bearer token
//! (<https://docs.aws.amazon.com/bedrock/latest/userguide/api-keys-use.html>).

use std::collections::{HashMap, VecDeque};
use std::sync::Mutex;

use crate::http::Endpoint;
use crate::presets::Wire;

const LEARNED_ROUTE_CAPACITY: usize = 256;

/// Learned prefix overrides. `true` means the model was refused at the
/// guessed prefix, so the next call uses the other one. The oldest learned
/// route is evicted when the map reaches its capacity.
#[derive(Debug, Default)]
pub(crate) struct LearnedRoutes {
    other: Mutex<RouteMemory>,
}

#[derive(Debug, Default)]
struct RouteMemory {
    other: HashMap<String, bool>,
    order: VecDeque<String>,
}

impl LearnedRoutes {
    pub(crate) fn other(&self, model: &str) -> bool {
        self.other
            .lock()
            .unwrap_or_else(|error| error.into_inner())
            .other
            .get(model)
            .copied()
            .unwrap_or(false)
    }

    pub(crate) fn learn_other(&self, model: &str) {
        let mut routes = self.other.lock().unwrap_or_else(|error| error.into_inner());
        if routes.other.insert(model.to_owned(), true).is_some() {
            return;
        }
        routes.order.push_back(model.to_owned());
        if routes.other.len() > LEARNED_ROUTE_CAPACITY
            && let Some(oldest) = routes.order.pop_front()
        {
            routes.other.remove(&oldest);
        }
    }

    pub(crate) fn forget(&self, model: &str) {
        let mut routes = self.other.lock().unwrap_or_else(|error| error.into_inner());
        if routes.other.remove(model).is_some() {
            routes.order.retain(|entry| entry != model);
        }
    }
}

/// Whether this endpoint is Mantle by preset or by host.
pub(crate) fn is_mantle(endpoint: &Endpoint) -> bool {
    endpoint.preset.as_deref() == Some("bedrock-mantle") || is_mantle_host(&endpoint.base_url)
}

/// Whether the endpoint is Bedrock's runtime or Mantle for Messages behavior.
pub(crate) fn is_bedrock(endpoint: &Endpoint) -> bool {
    matches!(
        endpoint.preset.as_deref(),
        Some("bedrock" | "bedrock-mantle")
    ) || is_bedrock_host(&host_of(&endpoint.base_url).unwrap_or_default())
}

pub(crate) fn is_bedrock_host(host: &str) -> bool {
    (host.starts_with("bedrock-runtime.") && host.ends_with(".amazonaws.com"))
        || (host.starts_with("bedrock-mantle.") && host.ends_with(".api.aws"))
}

fn is_mantle_host(base: &str) -> bool {
    host_of(base)
        .is_some_and(|host| host.starts_with("bedrock-mantle.") && host.ends_with(".api.aws"))
}

/// Whether this adapter may replace the OpenAI `/v1` prefix per model.
pub(crate) fn is_mantle_root(endpoint: &Endpoint) -> bool {
    is_mantle(endpoint) && v1_root(&endpoint.base_url).is_some()
}

/// Normalizes the one configured wire of an address-added Mantle provider.
///
/// A preset override on another host keeps its path: only the prefix choice
/// applies there. Another preset at a Mantle address keeps its own endpoints.
// reference: internal/provider/provider.go:1710-1739
pub(crate) fn normalize(endpoint: &mut Endpoint, wire: Wire) {
    if !endpoint.preset.as_deref().is_none_or(str::is_empty)
        && endpoint.preset.as_deref() != Some("bedrock-mantle")
    {
        return;
    }
    if !is_mantle_host(&endpoint.base_url) {
        return;
    }
    let base = endpoint.base_url.trim().trim_end_matches('/');
    let Some(root) = url_root(base) else {
        return;
    };
    let path = &base[root.len()..];
    let normalized = match wire {
        Wire::Chat | Wire::Responses if matches!(path, "" | "/openai/v1" | "/anthropic") => {
            Some(format!("{root}/v1"))
        }
        Wire::Messages if matches!(path, "" | "/v1" | "/openai/v1") => {
            Some(format!("{root}/anthropic"))
        }
        _ => None,
    };
    if let Some(normalized) = normalized {
        endpoint.base_url = normalized;
    }
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
    (400..500).contains(&status) && wrong_route_message(message)
}

/// Text that says this prefix does not serve the model, at any status.
// reference: internal/gateway/mantle.go:54-72
pub(crate) fn wrong_route_message(message: &str) -> bool {
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

fn url_root(base: &str) -> Option<String> {
    let authority = base.find("://")? + 3;
    let end = base[authority..]
        .find(['/', '?', '#'])
        .map_or(base.len(), |offset| authority + offset);
    Some(base[..end].to_owned())
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
    use super::{
        LEARNED_ROUTE_CAPACITY, LearnedRoutes, closed_model, is_mantle, is_mantle_root,
        mantle_base, normalize, wrong_route,
    };
    use crate::{Endpoint, Wire};

    const V1: &str = "https://bedrock-mantle.us-east-1.api.aws/v1";
    const OPENAI: &str = "https://bedrock-mantle.us-east-1.api.aws/openai/v1";

    fn endpoint(preset: Option<&str>, base: &str) -> Endpoint {
        let mut endpoint = Endpoint::new("test", base);
        endpoint.preset = preset.map(str::to_owned);
        endpoint
    }

    #[test]
    fn learned_routes_evict_the_oldest_entry_at_the_capacity() {
        let routes = LearnedRoutes::default();
        for index in 0..=LEARNED_ROUTE_CAPACITY {
            routes.learn_other(&format!("model-{index}"));
        }
        assert!(!routes.other("model-0"));
        assert!(routes.other("model-1"));
        assert!(routes.other(&format!("model-{LEARNED_ROUTE_CAPACITY}")));
        assert_eq!(
            routes
                .other
                .lock()
                .unwrap_or_else(|error| error.into_inner())
                .other
                .len(),
            LEARNED_ROUTE_CAPACITY
        );
    }

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
    fn only_a_mantle_host_is_retargeted_by_address() {
        for base in [
            V1,
            OPENAI,
            "https://bedrock-mantle.eu-west-1.api.aws/openai/v1/",
        ] {
            assert!(is_mantle_root(&endpoint(None, base)), "{base}");
        }
        for base in [
            "https://bedrock-runtime.us-east-1.amazonaws.com/openai/v1",
            "https://bedrock-mantle.us-east-1.amazonaws.com/v1",
            "https://bedrock-mantle.example.com/v1",
            "https://api.aws/v1",
            "https://relay.example.com/bedrock-mantle.us-east-1.api.aws/v1",
            "https://api.openai.com/v1",
        ] {
            assert!(!is_mantle_root(&endpoint(None, base)), "{base}");
        }
    }

    #[test]
    fn mantle_detection_uses_the_preset_or_host() {
        let relay = endpoint(Some("bedrock-mantle"), "https://relay.example/v1");
        assert!(is_mantle(&relay));
        assert!(is_mantle_root(&relay));
        let mut other = endpoint(Some("openrouter"), V1);
        assert!(is_mantle(&other));
        assert!(is_mantle_root(&other));
        normalize(&mut other, Wire::Chat);
        assert_eq!(other.base_url, V1);
    }

    #[test]
    fn address_added_mantle_normalizes_each_configured_wire() {
        let root = "https://bedrock-mantle.eu-west-1.api.aws";
        for (wire, input, expected) in [
            (Wire::Chat, V1, V1),
            (
                Wire::Chat,
                "https://bedrock-mantle.us-east-1.api.aws/openai/v1/",
                V1,
            ),
            (
                Wire::Responses,
                root,
                "https://bedrock-mantle.eu-west-1.api.aws/v1",
            ),
            (
                Wire::Responses,
                "https://bedrock-mantle.eu-west-1.api.aws/anthropic",
                "https://bedrock-mantle.eu-west-1.api.aws/v1",
            ),
            (
                Wire::Messages,
                root,
                "https://bedrock-mantle.eu-west-1.api.aws/anthropic",
            ),
            (
                Wire::Messages,
                "https://bedrock-mantle.eu-west-1.api.aws/v1",
                "https://bedrock-mantle.eu-west-1.api.aws/anthropic",
            ),
            (
                Wire::Messages,
                "https://bedrock-mantle.eu-west-1.api.aws/openai/v1",
                "https://bedrock-mantle.eu-west-1.api.aws/anthropic",
            ),
        ] {
            let mut endpoint = endpoint(None, input);
            normalize(&mut endpoint, wire);
            assert_eq!(endpoint.base_url, expected, "{wire:?} {input}");
        }
    }

    #[test]
    fn another_preset_or_an_unrelated_path_keeps_its_base() {
        let mut other = endpoint(Some("openrouter"), V1);
        normalize(&mut other, Wire::Chat);
        assert_eq!(other.base_url, V1);
        let mut relay = endpoint(Some("bedrock-mantle"), "https://relay.example/openai/v1");
        normalize(&mut relay, Wire::Chat);
        assert_eq!(relay.base_url, "https://relay.example/openai/v1");
        let mut custom = endpoint(None, "https://bedrock-mantle.us-east-1.api.aws/custom");
        normalize(&mut custom, Wire::Chat);
        assert_eq!(
            custom.base_url,
            "https://bedrock-mantle.us-east-1.api.aws/custom"
        );
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
