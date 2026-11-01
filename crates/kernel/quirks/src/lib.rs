//! Data-driven adjustments applied to a canonical request before a provider
//! encodes it.
//!
//! A profile is a list of rules looked up by id. Provider adapters apply the
//! profile they were given. Gateway and router code do not read profiles and
//! do not branch on these ids.

mod encoded;
mod json;
mod passthrough;
mod state;

pub use encoded::{EncodedContext, apply_before_encode, apply_encoded, apply_encoded_with_state};
pub use passthrough::{refused_passthrough, without_passthrough};
pub use state::{
    RequestState, TokenField, fold_encoded, fold_object_roots, root_union_refusal,
    token_field_wanted, with_token_field,
};

use hofund_ir::{ContentPart, Request, Role};

/// A named list of request adjustments.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Profile {
    /// Stable id used by configuration.
    pub id: &'static str,
    /// Rules applied in order.
    pub rules: &'static [Rule],
}

/// One adjustment.
///
/// Rules only remove or fill fields. They do not invent protocol objects.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Rule {
    /// Drop sampling temperature.
    OmitTemperature,
    /// Drop nucleus sampling.
    OmitTopP,
    /// Set `max_tokens` when the request did not.
    DefaultMaxTokens(u64),
    /// Drop reasoning text that is not a sealed item from the same protocol.
    ///
    /// Some Responses upstreams reject a reasoning item they did not seal.
    OmitUnsealedReasoning,
    /// Rewrite the developer role as system.
    ///
    /// Upstreams that have no developer role use this. It changes the prompt
    /// prefix, so it is not a default.
    FoldDeveloperIntoSystem,
}

/// Profiles shipped with this crate.
///
/// Unknown ids are the caller's error. This list is not a catalog of vendors.
pub fn named(id: &str) -> Option<&'static Profile> {
    PROFILES.iter().find(|profile| profile.id == id)
}

/// models.dev provider ids whose APIs accept an explicit `none` reasoning level when the catalog lists a toggle.
///
/// The daemon passes this list into the model catalog when `[catalog]` does not set
/// `reasoning_toggle_providers`.
pub fn catalog_reasoning_toggle_providers() -> &'static [&'static str] {
    &["deepseek", "kimi-code-plan-cn", "kimi-code-plan-global"]
}

/// Applies `profile` to `request`.
pub fn apply(profile: &Profile, request: &mut Request) {
    for rule in profile.rules {
        apply_rule(*rule, request);
    }
}

const PROFILES: &[Profile] = &[
    Profile {
        id: "responses-default",
        rules: &[Rule::OmitUnsealedReasoning],
    },
    Profile {
        id: "messages-default",
        rules: &[Rule::DefaultMaxTokens(4096)],
    },
    Profile {
        id: "no-sampling",
        rules: &[Rule::OmitTemperature, Rule::OmitTopP],
    },
];

fn apply_rule(rule: Rule, request: &mut Request) {
    match rule {
        Rule::OmitTemperature => request.params.temperature = None,
        Rule::OmitTopP => request.params.top_p = None,
        Rule::DefaultMaxTokens(value) => {
            if request.params.max_tokens.is_none() {
                request.params.max_tokens = Some(value);
            }
        }
        Rule::OmitUnsealedReasoning => {
            for message in &mut request.messages {
                message.content.retain(|part| match part {
                    ContentPart::Thinking(thinking) => {
                        thinking.extensions.get("sealed_item").is_some()
                    }
                    _ => true,
                });
            }
        }
        Rule::FoldDeveloperIntoSystem => {
            for message in &mut request.messages {
                if message.role == Role::Developer {
                    message.role = Role::System;
                }
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::{Rule, apply, named};
    use hofund_ir::{ContentPart, Extensions, Message, Request, Role, ThinkingPart};

    fn sample_request() -> Request {
        let mut request = Request::new("demo");
        request.messages.push(Message::new(Role::Developer));
        request.messages[0].content.push(ContentPart::text("rules"));
        request.messages.push(Message::new(Role::Assistant));
        request.messages[1]
            .content
            .push(ContentPart::Thinking(ThinkingPart {
                text: "draft".to_owned(),
                signature: None,
                redacted: false,
                data: None,
                extensions: Extensions::new(),
            }));
        request.params.temperature = Some(0.4);
        request.params.top_p = Some(0.8);
        request
    }

    #[test]
    fn named_profiles_are_stable() {
        assert!(named("missing").is_none());
        assert_eq!(
            named("responses-default").map(|profile| profile.rules),
            Some([Rule::OmitUnsealedReasoning].as_slice())
        );
    }

    #[test]
    fn rules_only_change_what_they_name() {
        let profile = named("no-sampling").expect("profile");
        let mut request = sample_request();
        apply(profile, &mut request);
        assert!(request.params.temperature.is_none());
        assert!(request.params.top_p.is_none());
        assert_eq!(request.messages[0].role, Role::Developer);

        let mut request = sample_request();
        apply(named("messages-default").expect("profile"), &mut request);
        assert_eq!(request.params.max_tokens, Some(4096));
        request.params.max_tokens = Some(10);
        apply(named("messages-default").expect("profile"), &mut request);
        assert_eq!(request.params.max_tokens, Some(10));

        let mut request = sample_request();
        apply(named("responses-default").expect("profile"), &mut request);
        assert!(request.messages[1].content.is_empty());

        let mut request = sample_request();
        let mut sealed = ThinkingPart {
            text: "kept".to_owned(),
            signature: None,
            redacted: false,
            data: None,
            extensions: Extensions::new(),
        };
        sealed
            .extensions
            .insert("sealed_item", serde_json::json!({"type": "reasoning"}));
        request.messages[1]
            .content
            .push(ContentPart::Thinking(sealed));
        apply(named("responses-default").expect("profile"), &mut request);
        assert_eq!(request.messages[1].content.len(), 1);

        apply(
            &super::Profile {
                id: "fold",
                rules: &[Rule::FoldDeveloperIntoSystem],
            },
            &mut request,
        );
        assert_eq!(request.messages[0].role, Role::System);
    }
}
