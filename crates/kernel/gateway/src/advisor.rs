//! Nested model calls made as the original gateway caller.
//!
//! The subscription bridge callback has no gateway key of its own. Advisor
//! and search calls go back through the router under the caller stamped on
//! the request.

use std::sync::{Arc, Weak};
use std::time::{Duration, Instant};

use async_trait::async_trait;
use axum::http::StatusCode;
use futures_util::StreamExt;
use hofund_ir::{
    AdvisorCall, CallerLimit, CallerStamp, ContentPart, Message, Request, Role, StreamEvent, Usage,
};
use hofund_plugin_api::{AskBackend, Completion, PluginError};
use hofund_secrets::{Identity, KeyLimit};

use crate::{Inner, ModelRequest, UsageMeta, route_scope, write_usage};

/// The reference bounds one advisor call at fifty-five seconds.
const DEADLINE: Duration = Duration::from_secs(55);

pub(super) fn backend(app: Weak<Inner>) -> Arc<dyn AskBackend> {
    Arc::new(GatewayAsk { app })
}

struct GatewayAsk {
    app: Weak<Inner>,
}

#[async_trait]
impl AskBackend for GatewayAsk {
    fn serves(&self, caller: &CallerStamp, model: &str) -> bool {
        let Some(app) = self.app.upgrade() else {
            return false;
        };
        let router = app.snapshot.load();
        let Some(wire) = hofund_codec::advisor_wire(model, &router.served_models()) else {
            return false;
        };
        router.admits(&wire.route, Some(&route_scope(&identity_from(caller))))
    }

    async fn ask(
        &self,
        caller: &CallerStamp,
        mut request: Request,
    ) -> Result<Completion, PluginError> {
        let app = self
            .app
            .upgrade()
            .ok_or_else(|| PluginError::unsupported("no nested model call is installed"))?;
        let router = app.snapshot.load();
        let Some(wire) = hofund_codec::advisor_wire(&request.model, &router.served_models()) else {
            return Err(PluginError::status_error(404, "no such model"));
        };
        request.model = wire.route.clone();
        request.stream = false;
        let identity = identity_from(caller);
        if let Some((status, message)) =
            crate::model_list::disabled_refusal(&app.model_catalog, &request.model)
        {
            return Err(PluginError::status_error(status.as_u16(), message));
        }
        let scope = route_scope(&identity);
        let prompt_bytes = serde_json::to_vec(&request)
            .map(|bytes| bytes.len())
            .unwrap_or(0);
        let _held = match app
            .budget
            .reserve(&identity, prompt_bytes, chrono::Local::now())
        {
            Ok(held) => held,
            Err(crate::limits::ReserveFailure::Refused(refusal)) => {
                return Err(PluginError::status_error(429, refusal.message()));
            }
            Err(crate::limits::ReserveFailure::Unavailable) => {
                return Err(PluginError::status_error(
                    503,
                    "usage limits cannot be checked",
                ));
            }
        };
        let started = Instant::now();
        let dispatched = tokio::time::timeout(DEADLINE, async {
            router
                .dispatch_scoped(&request, "", Some(&scope), None)
                .await
        })
        .await;
        let routed = match dispatched {
            Err(_elapsed) => return Err(PluginError::timeout("the advisor took too long")),
            Ok(Err(error)) => return Err(plugin_error(&error, &identity)),
            Ok(Ok(routed)) => routed,
        };
        let upstream = routed.upstream_id.clone();
        let served = routed.upstream_model.clone();
        let (text, usage) = take_text(routed.completion).await;
        if text.is_empty() {
            return Err(PluginError::status_error(502, "the advisor said nothing"));
        }
        record(
            &app, &request, &identity, &upstream, &served, &usage, started,
        );
        let mut message = Message::new(Role::Assistant);
        message.content = vec![ContentPart::Text(hofund_ir::TextPart {
            text: text.clone(),
            extensions: hofund_ir::Extensions::new(),
        })];
        Ok(Completion::Unary(Box::new(
            hofund_ir::Response::new(message)
                .with_model(Some(request.model))
                .with_usage(Some(usage)),
        )))
    }
}

fn plugin_error(error: &hofund_router::RouterError, identity: &Identity) -> PluginError {
    match error {
        hofund_router::RouterError::Upstream { error, .. } => {
            PluginError::status_error(error.status().unwrap_or(502), error.to_string())
        }
        hofund_router::RouterError::ModelScope { .. }
        | hofund_router::RouterError::AccountScope { .. } => {
            PluginError::status_error(403, crate::scope_error(error, identity))
        }
        hofund_router::RouterError::NoRoute { .. } => {
            PluginError::status_error(404, error.to_string())
        }
        other => PluginError::status_error(502, other.to_string()),
    }
}

async fn take_text(completion: Completion) -> (String, Usage) {
    match completion {
        Completion::Unary(response) => (
            text_of(&response.message.content),
            response.usage.unwrap_or_default(),
        ),
        Completion::Stream(mut stream) => {
            let mut text = String::new();
            let mut usage = Usage::default();
            while let Some(event) = stream.next().await {
                let Ok(event) = event else {
                    continue;
                };
                match event {
                    StreamEvent::TextDelta(delta) => text.push_str(&delta.text),
                    StreamEvent::MessageStart(start) => {
                        if let Some(next) = start.usage.as_ref() {
                            usage.merge_from(next);
                        }
                    }
                    StreamEvent::MessageDelta(delta) => {
                        if let Some(next) = delta.usage.as_ref() {
                            usage.merge_from(next);
                        }
                    }
                    _ => {}
                }
            }
            (text, usage)
        }
        Completion::Raw(_) | _ => (String::new(), Usage::default()),
    }
}

fn text_of(parts: &[ContentPart]) -> String {
    let mut text = String::new();
    for part in parts {
        if let ContentPart::Text(part) = part {
            text.push_str(&part.text);
        }
    }
    text
}

fn record(
    app: &Inner,
    request: &ModelRequest,
    identity: &Identity,
    upstream: &str,
    served: &str,
    usage: &Usage,
    started: Instant,
) {
    let mut meta = UsageMeta::new(
        request,
        upstream,
        &request.model,
        started,
        identity,
        "advisor",
    );
    meta.served_model = served.to_owned();
    crate::limits::observe_usage(&mut meta, usage, "", false);
    write_usage(
        &app.usage,
        &meta,
        Some((meta.input, meta.output)),
        StatusCode::OK,
        app.pricer.as_deref(),
        false,
        "",
    );
}

/// Copies the gateway key onto the request so a later callback can act as it.
pub(super) fn stamp_caller(identity: &Identity) -> CallerStamp {
    CallerStamp {
        key_id: identity.key_id.clone(),
        key_name: identity.key_name.clone(),
        issuer: identity.issuer.clone(),
        issuer_name: identity.issuer_name.clone(),
        models: identity.models.clone(),
        accounts: identity.accounts.clone(),
        limits: identity.limits.iter().map(limit_of).collect(),
        issuer_limits: identity.issuer_limits.iter().map(limit_of).collect(),
    }
}

fn limit_of(limit: &KeyLimit) -> CallerLimit {
    CallerLimit {
        period: limit.period.clone(),
        days: limit.days,
        hours: limit.hours,
        tokens: limit.tokens,
        cost: limit.cost,
        cache_reads: limit.cache_reads,
        since: limit.since,
    }
}

pub(super) fn identity_from(caller: &CallerStamp) -> Identity {
    Identity {
        key_id: caller.key_id.clone(),
        key_name: caller.key_name.clone(),
        issuer: caller.issuer.clone(),
        issuer_name: caller.issuer_name.clone(),
        models: caller.models.clone(),
        accounts: caller.accounts.clone(),
        limits: caller.limits.iter().map(key_limit).collect(),
        issuer_limits: caller.issuer_limits.iter().map(key_limit).collect(),
        ..Identity::default()
    }
}

fn key_limit(limit: &CallerLimit) -> KeyLimit {
    KeyLimit {
        period: limit.period.clone(),
        days: limit.days,
        hours: limit.hours,
        tokens: limit.tokens,
        cost: limit.cost,
        cache_reads: limit.cache_reads,
        since: limit.since,
    }
}

/// Bare advisor model named on the request, when one is served.
pub(super) fn advisor_model_of(request: &ModelRequest, router: &hofund_router::Router) -> String {
    let Some(raw) = request.tools.iter().find_map(|tool| {
        tool.extensions
            .get("type")
            .and_then(|value| value.as_str())
            .filter(|kind| kind.starts_with("advisor_"))
            .and_then(|_| tool.extensions.get("model"))
            .and_then(|value| value.as_str())
    }) else {
        return String::new();
    };
    hofund_codec::advisor_wire(raw, &router.served_models())
        .map(|wire| wire.bare)
        .unwrap_or_default()
}

/// One usage row per advisor iteration, priced at the advisor model.
pub(super) fn write_advisor_rows(
    usage: &crate::Ledger,
    meta: &UsageMeta,
    pricer: Option<&hofund_model_catalog::EffectivePricer>,
    provider_free: bool,
) {
    for call in &meta.advisor_calls {
        if call.input_tokens + call.output_tokens + call.cache_read_tokens + call.cache_write_tokens
            == 0
        {
            continue;
        }
        let input = i64::try_from(call.input_tokens).unwrap_or(i64::MAX);
        let output = i64::try_from(call.output_tokens).unwrap_or(i64::MAX);
        write_usage(
            usage,
            &row_for_call(meta, call),
            Some((Some(input), Some(output))),
            StatusCode::OK,
            pricer,
            provider_free,
            "",
        );
    }
}

fn row_for_call(meta: &UsageMeta, call: &AdvisorCall) -> UsageMeta {
    let model = if meta.advisor_model.is_empty() {
        call.model.clone()
    } else {
        meta.advisor_model.clone()
    };
    let counted = |value: u64| (value > 0).then_some(i64::try_from(value).unwrap_or(i64::MAX));
    let input = i64::try_from(call.input_tokens).unwrap_or(i64::MAX);
    // Messages `input_tokens` is already the uncached count. Cache read stays
    // on its own column and is not subtracted again.
    let details = hofund_usage::UsageDetails {
        cache_read_tokens: counted(call.cache_read_tokens),
        cache_write_tokens: counted(call.cache_write_tokens),
        cache_write_5m_tokens: counted(call.cache_write_5m_tokens),
        cache_write_1h_tokens: counted(call.cache_write_1h_tokens),
        uncached_input_tokens: Some(input),
        ..Default::default()
    };
    UsageMeta {
        model: model.clone(),
        upstream: meta.upstream.clone(),
        provider_key_id: meta.provider_key_id.clone(),
        wire_model: model,
        served_model: call.model.clone(),
        started: meta.started,
        identity: meta.identity.clone(),
        client_agent: meta.client_agent.clone(),
        session_id: meta.session_id.clone(),
        native_session_id: meta.native_session_id.clone(),
        endpoint: meta.endpoint.clone(),
        kind: "advisor".to_owned(),
        advisor_model: String::new(),
        advisor_calls: Vec::new(),
        input: Some(input),
        output: Some(i64::try_from(call.output_tokens).unwrap_or(i64::MAX)),
        ttft_ms: None,
        details,
        saw_reasoning: false,
        stream_timing: crate::stream_timing::StreamTiming::new(meta.started),
    }
}
