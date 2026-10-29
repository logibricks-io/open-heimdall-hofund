//! The per-model route prefix on a Mantle host.
//!
//! Mantle's Chat Completions and Responses API use two prefixes on
//! `bedrock-mantle.<region>.api.aws`. A closed OpenAI GPT id and an `xai.`
//! id are under `/openai/v1`. Every other model is under `/v1`. Messages stay
//! on `/anthropic`.
//!
//! Hosts: <https://docs.aws.amazon.com/bedrock/latest/userguide/endpoints.html>
//! (checked 2026-10-27). The prefix split is the rule each model's card
//! follows; a model asked at the other prefix is refused with a message that
//! it is not supported on that route.

use std::collections::{HashMap, VecDeque};
use std::sync::Mutex;

use hofund_plugin_api::ProtocolId;

/// The most learned routes one provider keeps.
pub(crate) const LEARNED_ROUTE_CAPACITY: usize = 256;

/// Learned prefix overrides. A learned model was refused at the guessed
/// prefix, so the next call uses the other one. The oldest learned route is
/// evicted when the map is over its capacity.
#[derive(Debug, Default)]
pub(crate) struct LearnedRoutes {
    memory: Mutex<RouteMemory>,
}

#[derive(Debug, Default)]
struct RouteMemory {
    other: HashMap<String, bool>,
    order: VecDeque<String>,
}

impl LearnedRoutes {
    /// Whether `model` was learned to use the other prefix.
    pub(crate) fn other(&self, model: &str) -> bool {
        self.memory
            .lock()
            .unwrap_or_else(|error| error.into_inner())
            .other
            .get(model)
            .copied()
            .unwrap_or(false)
    }

    /// Records that `model` uses the other prefix.
    pub(crate) fn learn_other(&self, model: &str) {
        let mut routes = self
            .memory
            .lock()
            .unwrap_or_else(|error| error.into_inner());
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

    /// Drops what was learned for `model`.
    pub(crate) fn forget(&self, model: &str) {
        let mut routes = self
            .memory
            .lock()
            .unwrap_or_else(|error| error.into_inner());
        if routes.other.remove(model).is_some() {
            routes.order.retain(|entry| entry != model);
        }
    }

    #[cfg(test)]
    pub(crate) fn len(&self) -> usize {
        self.memory
            .lock()
            .unwrap_or_else(|error| error.into_inner())
            .other
            .len()
    }
}

/// Whether `host` is a Mantle host, `bedrock-mantle.<region>.api.aws`. A port
/// is ignored and case does not matter.
#[must_use]
pub fn mantle_host(host: &str) -> bool {
    let host = host
        .rsplit_once(':')
        .filter(|(_, port)| port.chars().all(|c| c.is_ascii_digit()))
        .map_or(host, |(name, _)| name)
        .to_ascii_lowercase();
    host.starts_with("bedrock-mantle.") && host.ends_with(".api.aws")
}

/// Whether the base URL's host is a Mantle host.
pub(crate) fn is_mantle_base(base: &str) -> bool {
    host_of(base).is_some_and(|host| mantle_host(&host))
}

/// Normalizes the base of a Mantle-host provider for `protocol`.
///
/// Chat and Responses at the root, `/openai/v1`, or `/anthropic` move to
/// `/v1`. Messages at the root, `/v1`, or `/openai/v1` move to `/anthropic`.
/// Another host, or another path, keeps its base: only the prefix choice
/// applies there.
// reference: internal/provider/provider.go:1710-1739
pub(crate) fn normalize(base: &str, protocol: ProtocolId) -> String {
    let base = base.trim().trim_end_matches('/');
    if !is_mantle_base(base) {
        return base.to_owned();
    }
    let Some(root) = url_root(base) else {
        return base.to_owned();
    };
    let path = &base[root.len()..];
    match protocol {
        ProtocolId::OPENAI_CHAT | ProtocolId::OPENAI_RESPONSES
            if matches!(path, "" | "/openai/v1" | "/anthropic") =>
        {
            format!("{root}/v1")
        }
        ProtocolId::ANTHROPIC_MESSAGES if matches!(path, "" | "/v1" | "/openai/v1") => {
            format!("{root}/anthropic")
        }
        _ => base.to_owned(),
    }
}

/// The base a Chat or Responses call for `model` uses.
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
pub(crate) fn closed_model(model: &str) -> bool {
    let model = model.to_ascii_lowercase();
    model.starts_with("xai.") || (model.contains("openai.gpt-") && !model.contains("gpt-oss"))
}

/// Scheme and host of a base that ends in `/v1`, with a trailing `/openai` removed.
pub(crate) fn v1_root(base: &str) -> Option<String> {
    let trimmed = base.trim().trim_end_matches('/');
    let without_v1 = trimmed.strip_suffix("/v1")?;
    Some(
        without_v1
            .strip_suffix("/openai")
            .unwrap_or(without_v1)
            .to_owned(),
    )
}

/// Scheme and authority of `base`.
fn url_root(base: &str) -> Option<String> {
    let authority = base.find("://")? + 3;
    let end = base[authority..]
        .find(['/', '?', '#'])
        .map_or(base.len(), |offset| authority + offset);
    Some(base[..end].to_owned())
}

/// The lowercased host of `url`, without user info or port.
pub(crate) fn host_of(url: &str) -> Option<String> {
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
mod tests;
