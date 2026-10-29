//! Amazon Bedrock Mantle upstream.
//!
//! Mantle serves three wires from `https://bedrock-mantle.<region>.api.aws`
//! with a Bedrock API key:
//!
//! - Chat and Responses under `/v1` or `/openai/v1`, with the key as a bearer
//!   token. The prefix depends on the model: see `route`. A model refused at
//!   the guessed prefix is tried once at the other one, and a prefix that
//!   answered is remembered per provider.
//! - Messages under `/anthropic`, with the key in `x-api-key` alone. Bedrock
//!   rejects some Anthropic betas and the `metadata` field, so this plugin
//!   fits the `anthropic-beta` header and the body's `anthropic_beta` list and
//!   drops `metadata` before sending.
//!
//! A base on a Mantle host moves to the wire's path first: `/v1` for Chat and
//! Responses, `/anthropic` for Messages. The model list is Mantle's live
//! OpenAI-compatible list.
//!
//! References:
//! <https://docs.aws.amazon.com/bedrock/latest/userguide/bedrock-mantle.html>,
//! <https://docs.aws.amazon.com/bedrock/latest/userguide/endpoints.html>.
//! Checked 2026-10-27.

#![deny(missing_docs)]

mod list;
mod route;

use std::sync::Arc;

use hofund_plugin_api::{
    BuildContext, BuildError, Completion, ConfigError, CountOutcome, CredentialStyle, HostContext,
    ListKind, ListQuery, ModelList, PluginError, PluginErrorKind, PluginId, ProbeQuery,
    ProbeReport, ProtocolId, RawTable, RawValue, Request, Schema, SecretString, UpstreamFactory,
    UpstreamProvider, UpstreamSpec, ValidConfig,
};
use hofund_plugin_sdk::bedrock;
use hofund_plugin_sdk::json::{Value, json};
use hofund_plugin_sdk::oauth::secret_text;
use hofund_plugin_sdk::probe::{
    PROBE_WAIT, ProbePage, merge_extra, probe_error_message, probe_report, read_probe,
};
use hofund_plugin_sdk::wire::{self, Target};
use hofund_plugin_sdk::{Call, Contribution};

use route::LearnedRoutes;
pub use route::mantle_host;

/// The probe's output cap, as every other upstream's probe.
const PROBE_TOKENS: u64 = 16;

/// The Anthropic API version the Messages wire sends.
const ANTHROPIC_VERSION: &str = "2023-06-01";

/// Registers the Amazon Bedrock Mantle factory.
#[must_use]
pub fn contribute() -> Contribution {
    Contribution::new().upstream(MantleFactory)
}

/// The spec names the API key's default placement. Messages moves the key to
/// `x-api-key`.
static SPEC: UpstreamSpec = UpstreamSpec::from_static(
    PluginId::from_static("bedrock-mantle"),
    &[
        ProtocolId::OPENAI_CHAT,
        ProtocolId::OPENAI_RESPONSES,
        ProtocolId::ANTHROPIC_MESSAGES,
    ],
    CredentialStyle::Bearer,
    ListKind::Live,
);

/// The validated table: shape only. Values are checked in `build`, after the
/// host has resolved `env:` references.
struct MantleConfig {
    base_url: String,
    headers: Vec<(String, String)>,
}

impl ValidConfig for MantleConfig {}

struct MantleFactory;

impl UpstreamFactory for MantleFactory {
    type Config = MantleConfig;

    fn spec(&self) -> &'static UpstreamSpec {
        &SPEC
    }

    fn config_schema(&self) -> Schema {
        Schema::object(vec![("base_url".to_owned(), Schema::string())], &[])
            .expect("bedrock-mantle schema")
    }

    fn validate(&self, raw: &RawTable) -> Result<Self::Config, ConfigError> {
        for key in raw.keys() {
            if key != "base_url" && key != "headers" {
                return Err(ConfigError::new("unknown key").with_path(key));
            }
        }
        let base_url = match raw.get("base_url") {
            None => String::new(),
            Some(RawValue::String(value)) => value.clone(),
            Some(_) => return Err(ConfigError::new("must be a string").with_path("base_url")),
        };
        let mut headers = Vec::new();
        match raw.get("headers") {
            None => {}
            Some(RawValue::Table(table)) => {
                for (name, value) in table.iter() {
                    let RawValue::String(value) = value else {
                        return Err(ConfigError::new("header values must be strings")
                            .with_path(format!("headers.{name}")));
                    };
                    headers.push((name.to_owned(), value.clone()));
                }
            }
            Some(_) => return Err(ConfigError::new("headers must be a table").with_path("headers")),
        }
        Ok(MantleConfig { base_url, headers })
    }

    fn build(
        &self,
        cfg: &Self::Config,
        host: &HostContext,
    ) -> Result<Arc<dyn UpstreamProvider>, BuildError> {
        self.build_with(cfg, host, &BuildContext::new(ProtocolId::OPENAI_CHAT))
    }

    fn build_with(
        &self,
        cfg: &Self::Config,
        host: &HostContext,
        ctx: &BuildContext,
    ) -> Result<Arc<dyn UpstreamProvider>, BuildError> {
        let protocol = ctx.protocol();
        let (credential, headers) = match protocol {
            ProtocolId::OPENAI_CHAT | ProtocolId::OPENAI_RESPONSES => {
                (CredentialStyle::Bearer, cfg.headers.clone())
            }
            ProtocolId::ANTHROPIC_MESSAGES => {
                (CredentialStyle::XApiKey, bedrock::fit_headers(&cfg.headers))
            }
            other => {
                return Err(BuildError::new(format!(
                    "bedrock-mantle does not serve protocol `{}`",
                    other.as_str()
                )));
            }
        };
        let trimmed = cfg.base_url.trim().trim_end_matches('/');
        if trimmed.is_empty() {
            return Err(BuildError::new("provider base URL is empty"));
        }
        let base = route::normalize(trimmed, protocol);
        let prefixed =
            protocol != ProtocolId::ANTHROPIC_MESSAGES && route::v1_root(&base).is_some();
        Ok(Arc::new(Mantle {
            protocol,
            base,
            headers,
            credential,
            prefixed,
            routes: LearnedRoutes::default(),
            host: host.clone(),
        }))
    }
}

struct Mantle {
    protocol: ProtocolId,
    /// The base after normalization.
    base: String,
    /// The configured headers; on Messages, fitted to Bedrock.
    headers: Vec<(String, String)>,
    credential: CredentialStyle,
    /// Whether the base ends in `/v1`, so each Chat or Responses model picks
    /// its prefix.
    prefixed: bool,
    routes: LearnedRoutes,
    host: HostContext,
}

impl Mantle {
    /// The stored Bedrock API key, if any.
    async fn secret(&self) -> Result<Option<SecretString>, PluginError> {
        self.host.secrets().get("api_key").await
    }

    /// The base a call for `model` uses; `other` picks the prefix the id rule
    /// would not.
    fn base_for(&self, model: &str, other: bool) -> String {
        if self.prefixed {
            route::mantle_base(&self.base, model, other)
        } else {
            self.base.clone()
        }
    }

    fn target(&self, base: String) -> Target {
        Target::new(self.protocol, base)
            .credential(self.credential)
            .headers(self.headers.clone())
    }

    /// Chat and Responses with the prefix choice. A refusal that names the
    /// route is tried once at the other prefix and the answer is remembered.
    /// The first error comes back, and the route is forgotten, when the other
    /// prefix refuses the model too or cannot be reached. A failure while
    /// reading an accepted body keeps the route.
    async fn execute_prefixed(
        &self,
        request: Request,
        inbound: ProtocolId,
        secret: Option<&SecretString>,
    ) -> Result<Completion, PluginError> {
        let http = self.host.http();
        let model = request.model.clone();
        let learned = self.routes.other(&model);
        let first = wire::execute(
            http,
            &self.target(self.base_for(&model, learned)),
            secret,
            request.clone(),
            inbound,
        )
        .await;
        match first {
            Err(error)
                if !model.is_empty()
                    && !learned
                    && route::wrong_route(error.status(), error.message()) =>
            {
                self.routes.learn_other(&model);
                // reference: internal/gateway/mantle.go:54-72
                let other = self.target(self.base_for(&model, true));
                match wire::send(http, &other, secret, request, inbound).await {
                    Ok(sent) => sent.complete().await,
                    Err(second)
                        if route::wrong_route_message(second.message())
                            || matches!(
                                second.kind(),
                                PluginErrorKind::Connect
                                    | PluginErrorKind::Timeout
                                    | PluginErrorKind::Transport
                            ) =>
                    {
                        self.routes.forget(&model);
                        Err(error)
                    }
                    Err(second) => Err(second),
                }
            }
            other => other,
        }
    }

    async fn probe_inner(&self, query: &ProbeQuery) -> Result<ProbeReport, PluginError> {
        let secret = self.secret().await?;
        let key = secret.as_ref().map_or("", secret_text);
        let model = query.model();
        let (path, mut body) = match self.protocol {
            ProtocolId::OPENAI_CHAT => (
                "/chat/completions",
                json!({
                    "model": model,
                    "messages": [{"role": "user", "content": "hi"}],
                    "max_completion_tokens": PROBE_TOKENS,
                }),
            ),
            ProtocolId::OPENAI_RESPONSES => (
                "/responses",
                json!({
                    "model": model,
                    "input": [{"type": "message", "role": "user", "content": [{"type": "input_text", "text": "hi"}]}],
                    "max_output_tokens": PROBE_TOKENS,
                }),
            ),
            _ => (
                "/v1/messages",
                json!({
                    "model": model,
                    "max_tokens": PROBE_TOKENS,
                    "messages": [{"role": "user", "content": "hi"}],
                }),
            ),
        };
        merge_extra(&mut body, query)?;
        let stream = body.get("stream").and_then(Value::as_bool).unwrap_or(false);
        let body = body.to_string();
        let mut url = format!("{}{path}", self.base_for(model, false));
        let mut page = self
            .post_probe(&url, &body, stream, secret.as_ref())
            .await?;
        if self.protocol != ProtocolId::ANTHROPIC_MESSAGES
            && !model.is_empty()
            && route::wrong_route(Some(page.status), &probe_error_message(&page.body))
        {
            url = format!("{}{path}", self.base_for(model, true));
            page = self
                .post_probe(&url, &body, stream, secret.as_ref())
                .await?;
        }
        let mut secrets: Vec<&str> = vec![key];
        secrets.extend(self.headers.iter().map(|(_, value)| value.as_str()));
        probe_report(&url, &page, stream, &secrets)
    }

    async fn post_probe(
        &self,
        url: &str,
        body: &str,
        stream: bool,
        secret: Option<&SecretString>,
    ) -> Result<ProbePage, PluginError> {
        // A configured header replaces the one this plugin would set.
        let configured = |name: &str| {
            self.headers
                .iter()
                .any(|(header, _)| header.eq_ignore_ascii_case(name))
        };
        let mut call = Call::post(url).json(body);
        if self.protocol == ProtocolId::ANTHROPIC_MESSAGES && !configured("anthropic-version") {
            call = call.header("anthropic-version", ANTHROPIC_VERSION);
        }
        let credential_header = match self.credential {
            CredentialStyle::XApiKey => "x-api-key",
            _ => "authorization",
        };
        if let Some(secret) = secret.filter(|secret| !secret_text(secret).is_empty())
            && !configured(credential_header)
        {
            call = call.credential(self.credential, secret)?;
        }
        for (name, value) in &self.headers {
            call = call.sensitive_header(name.to_ascii_lowercase(), value.clone());
        }
        let response = call.open_raw(self.host.http(), 1).await?;
        read_probe(response, stream).await
    }
}

#[hofund_plugin_sdk::async_trait]
impl UpstreamProvider for Mantle {
    async fn execute(
        &self,
        mut request: Request,
        inbound: ProtocolId,
    ) -> Result<Completion, PluginError> {
        let secret = self.secret().await?;
        if self.protocol == ProtocolId::ANTHROPIC_MESSAGES {
            bedrock::fit_request(&mut request);
        } else if self.prefixed {
            return self
                .execute_prefixed(request, inbound, secret.as_ref())
                .await;
        }
        wire::execute(
            self.host.http(),
            &self.target(self.base.clone()),
            secret.as_ref(),
            request,
            inbound,
        )
        .await
    }

    async fn count_input(
        &self,
        mut request: Request,
        inbound: ProtocolId,
    ) -> Result<CountOutcome, PluginError> {
        if self.protocol != ProtocolId::ANTHROPIC_MESSAGES {
            return Ok(CountOutcome::Unsupported);
        }
        bedrock::fit_request(&mut request);
        let secret = self.secret().await?;
        wire::count_input(
            self.host.http(),
            &self.target(self.base.clone()),
            secret.as_ref(),
            request,
            inbound,
        )
        .await
    }

    async fn list_models(&self, query: &ListQuery) -> Result<ModelList, PluginError> {
        let secret = self.secret().await?;
        list::Lister {
            http: self.host.http(),
            base: &self.base,
            headers: &self.headers,
            messages: self.protocol == ProtocolId::ANTHROPIC_MESSAGES,
            secret: secret.as_ref(),
        }
        .list(query)
        .await
    }

    async fn probe(&self, query: &ProbeQuery) -> Result<ProbeReport, PluginError> {
        hofund_plugin_sdk::timeout(PROBE_WAIT, self.probe_inner(query))
            .await
            .unwrap_or_else(|| Err(PluginError::timeout("provider probe exceeded 20 seconds")))
    }
}

#[cfg(test)]
mod tests;
