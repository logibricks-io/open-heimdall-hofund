//! Amazon Bedrock upstream.
//!
//! Bedrock Runtime serves four wires from one regional host,
//! `https://bedrock-runtime.<region>.amazonaws.com`:
//!
//! - Chat and Responses under `/openai/v1`, with a Bedrock API key as a
//!   bearer token.
//! - Messages under `/anthropic`, with the key in `x-api-key` alone. Bedrock
//!   rejects some Anthropic betas and the `metadata` field, so this plugin
//!   fits the `anthropic-beta` header and the body's `anthropic_beta` list and
//!   drops `metadata` before sending.
//! - Converse at the runtime root, `POST /model/{modelId}/converse` or
//!   `/converse-stream`, signed with AWS Signature Version 4 from the
//!   `access_key_id`, `secret_access_key`, and optional `session_token`
//!   options. Converse takes no API key.
//!
//! Bedrock has no model list an API key can read, so the list is the static
//! catalog row.
//!
//! References:
//! <https://docs.aws.amazon.com/bedrock/latest/userguide/inference-chat-completions.html>,
//! <https://docs.aws.amazon.com/bedrock/latest/userguide/bedrock-mantle.html>,
//! <https://docs.aws.amazon.com/bedrock/latest/APIReference/API_runtime_Converse.html>,
//! <https://docs.aws.amazon.com/bedrock/latest/APIReference/API_runtime_ConverseStream.html>,
//! <https://docs.aws.amazon.com/bedrock/latest/userguide/endpoints.html>.
//! Checked 2026-10-25.

#![deny(missing_docs)]

mod betas;
mod converse;

use std::sync::Arc;

use hofund_plugin_api::{
    BuildContext, BuildError, Completion, ConfigError, CountOutcome, CredentialStyle, HostContext,
    ListKind, ListQuery, ModelList, ModelSpec, PluginError, PluginId, ProbeQuery, ProbeReport,
    ProtocolId, RawTable, RawValue, Request, Schema, SecretString, UpstreamFactory,
    UpstreamProvider, UpstreamSpec, ValidConfig,
};
use hofund_plugin_sdk::json::{Value, json};
use hofund_plugin_sdk::oauth::secret_text;
use hofund_plugin_sdk::probe::{PROBE_WAIT, merge_extra, probe_report, read_probe};
use hofund_plugin_sdk::wire::{self, Target};
use hofund_plugin_sdk::{Call, Contribution};

/// The probe's output cap, as every other upstream's probe.
const PROBE_TOKENS: u64 = 16;

/// The Anthropic API version the Messages wire sends.
const ANTHROPIC_VERSION: &str = "2023-06-01";

/// The string options `validate` reads besides `base_url`.
const OPTIONS: &[&str] = &[
    "region",
    "access_key_id",
    "secret_access_key",
    "session_token",
];

/// The catalog row's model ids. A test holds them equal to `presets.toml`.
pub(crate) const MODELS: &[&str] = &[
    "global.anthropic.claude-opus-5-5",
    "global.anthropic.claude-sonnet-5-5",
    "global.anthropic.claude-sonnet-5",
    "global.anthropic.claude-opus-5",
    "global.anthropic.claude-fable-5-1",
    "global.anthropic.claude-opus-4-8",
    "global.anthropic.claude-opus-4-7",
    "global.anthropic.claude-haiku-4-5-20251001-v1:0",
    "global.openai.gpt-6-astra",
    "global.openai.gpt-6-sol",
    "global.openai.gpt-6-luna",
    "openai.gpt-oss-120b-1:0",
    "openai.gpt-oss-20b-1:0",
    "qwen.qwen3-coder-480b-a35b-v1:0",
    "deepseek.v3.2",
    "moonshotai.kimi-k2.5",
    "zai.glm-5",
    "minimax.minimax-m2.5",
];

/// Registers the Amazon Bedrock factory.
#[must_use]
pub fn contribute() -> Contribution {
    Contribution::new().upstream(BedrockFactory)
}

/// The spec names the API key's default placement. Messages moves the key to
/// `x-api-key` and Converse signs with SigV4 instead.
static SPEC: UpstreamSpec = UpstreamSpec::from_static(
    PluginId::from_static("bedrock"),
    &[
        ProtocolId::OPENAI_CHAT,
        ProtocolId::OPENAI_RESPONSES,
        ProtocolId::ANTHROPIC_MESSAGES,
        ProtocolId::BEDROCK_CONVERSE,
    ],
    CredentialStyle::Bearer,
    ListKind::Catalog,
);

/// The validated table: shape only. Values are checked in `build`, after the
/// host has resolved `env:` references.
struct BedrockConfig {
    base_url: String,
    headers: Vec<(String, String)>,
    region: String,
    access_key_id: String,
    secret_access_key: String,
    session_token: String,
}

impl ValidConfig for BedrockConfig {}

struct BedrockFactory;

fn string(raw: &RawTable, key: &str) -> Result<String, ConfigError> {
    match raw.get(key) {
        None => Ok(String::new()),
        Some(RawValue::String(value)) => Ok(value.clone()),
        Some(_) => Err(ConfigError::new("must be a string").with_path(key)),
    }
}

impl UpstreamFactory for BedrockFactory {
    type Config = BedrockConfig;

    fn spec(&self) -> &'static UpstreamSpec {
        &SPEC
    }

    fn config_schema(&self) -> Schema {
        let mut fields = vec![("base_url".to_owned(), Schema::string())];
        fields.extend(
            OPTIONS
                .iter()
                .map(|key| ((*key).to_owned(), Schema::string())),
        );
        Schema::object(fields, &[]).expect("bedrock schema")
    }

    fn validate(&self, raw: &RawTable) -> Result<Self::Config, ConfigError> {
        for key in raw.keys() {
            if key != "base_url" && key != "headers" && !OPTIONS.contains(&key) {
                return Err(ConfigError::new("unknown key").with_path(key));
            }
        }
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
        Ok(BedrockConfig {
            base_url: string(raw, "base_url")?,
            headers,
            region: string(raw, "region")?,
            access_key_id: string(raw, "access_key_id")?,
            secret_access_key: string(raw, "secret_access_key")?,
            session_token: string(raw, "session_token")?,
        })
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
        let (base, headers, target) = match protocol {
            ProtocolId::OPENAI_CHAT | ProtocolId::OPENAI_RESPONSES => {
                let base = api_base(&cfg.base_url)?;
                let target = Target::new(protocol, base.clone())
                    .credential(CredentialStyle::Bearer)
                    .headers(cfg.headers.clone());
                (base, cfg.headers.clone(), target)
            }
            ProtocolId::ANTHROPIC_MESSAGES => {
                let base = api_base(&cfg.base_url)?;
                let headers = betas::fit_headers(&cfg.headers);
                let target = Target::new(protocol, base.clone())
                    .credential(CredentialStyle::XApiKey)
                    .headers(headers.clone());
                (base, headers, target)
            }
            ProtocolId::BEDROCK_CONVERSE => {
                let keys = converse::Keys::read(cfg)?;
                let base = converse::runtime_root(&cfg.base_url);
                if base.is_empty() {
                    return Err(BuildError::new("provider base URL is empty"));
                }
                let region = converse::signing_region(&cfg.region, &base)?;
                let target = Target::new(protocol, base.clone())
                    .headers(cfg.headers.clone())
                    .signer(keys.signer(region));
                (base, cfg.headers.clone(), target)
            }
            other => {
                return Err(BuildError::new(format!(
                    "bedrock does not serve protocol `{}`",
                    other.as_str()
                )));
            }
        };
        Ok(Arc::new(Bedrock {
            protocol,
            base,
            headers,
            target,
            host: host.clone(),
        }))
    }
}

/// The configured base for the API-key wires. The preset always supplies one.
fn api_base(base_url: &str) -> Result<String, BuildError> {
    let base = base_url.trim().trim_end_matches('/');
    if base.is_empty() {
        return Err(BuildError::new("provider base URL is empty"));
    }
    Ok(base.to_owned())
}

struct Bedrock {
    protocol: ProtocolId,
    base: String,
    headers: Vec<(String, String)>,
    target: Target,
    host: HostContext,
}

impl Bedrock {
    /// The stored Bedrock API key, if any. Converse refuses a non-empty one.
    async fn secret(&self) -> Result<Option<SecretString>, PluginError> {
        self.host.secrets().get("api_key").await
    }

    async fn probe_inner(&self, query: &ProbeQuery) -> Result<ProbeReport, PluginError> {
        let secret = self.secret().await?;
        let key = secret.as_ref().map_or("", secret_text);
        let model = query.model();
        let (url, mut body) = match self.protocol {
            ProtocolId::OPENAI_CHAT => (
                format!("{}/chat/completions", self.base),
                json!({
                    "model": model,
                    "messages": [{"role": "user", "content": "hi"}],
                    "max_completion_tokens": PROBE_TOKENS,
                }),
            ),
            ProtocolId::OPENAI_RESPONSES => (
                format!("{}/responses", self.base),
                json!({
                    "model": model,
                    "input": [{"type": "message", "role": "user", "content": [{"type": "input_text", "text": "hi"}]}],
                    "max_output_tokens": PROBE_TOKENS,
                }),
            ),
            ProtocolId::ANTHROPIC_MESSAGES => (
                format!("{}/v1/messages", self.base),
                json!({
                    "model": model,
                    "max_tokens": PROBE_TOKENS,
                    "messages": [{"role": "user", "content": "hi"}],
                }),
            ),
            _ => (
                String::new(),
                json!({
                    "messages": [{"role": "user", "content": [{"text": "hi"}]}],
                    "inferenceConfig": {"maxTokens": PROBE_TOKENS},
                }),
            ),
        };
        merge_extra(&mut body, query)?;
        let stream = body.get("stream").and_then(Value::as_bool).unwrap_or(false);
        let (url, call) = if self.protocol == ProtocolId::BEDROCK_CONVERSE {
            if !key.is_empty() {
                return Err(PluginError::config(
                    "Converse uses AWS credentials, not an API key",
                ));
            }
            wire::converse_call(&self.target, model, stream, body.to_string().into_bytes())?
        } else {
            // A configured header replaces the one this plugin would set.
            let configured = |name: &str| {
                self.headers
                    .iter()
                    .any(|(header, _)| header.eq_ignore_ascii_case(name))
            };
            let mut call = Call::post(url.as_str()).json(body.to_string());
            if self.protocol == ProtocolId::ANTHROPIC_MESSAGES && !configured("anthropic-version") {
                call = call.header("anthropic-version", ANTHROPIC_VERSION);
            }
            let style = self.target_credential();
            let credential_header = match style {
                CredentialStyle::XApiKey => "x-api-key",
                _ => "authorization",
            };
            if let Some(secret) = secret.as_ref().filter(|_| !key.is_empty())
                && !configured(credential_header)
            {
                call = call.credential(style, secret)?;
            }
            for (name, value) in &self.headers {
                call = call.sensitive_header(name.to_ascii_lowercase(), value.clone());
            }
            (url, call)
        };
        let response = call.open_raw(self.host.http(), 1).await?;
        let page = read_probe(response, stream).await?;
        let mut secrets: Vec<&str> = vec![key];
        secrets.extend(self.headers.iter().map(|(_, value)| value.as_str()));
        probe_report(&url, &page, stream, &secrets)
    }

    fn target_credential(&self) -> CredentialStyle {
        if self.protocol == ProtocolId::ANTHROPIC_MESSAGES {
            CredentialStyle::XApiKey
        } else {
            CredentialStyle::Bearer
        }
    }
}

#[hofund_plugin_sdk::async_trait]
impl UpstreamProvider for Bedrock {
    async fn execute(
        &self,
        mut request: Request,
        inbound: ProtocolId,
    ) -> Result<Completion, PluginError> {
        if self.protocol == ProtocolId::ANTHROPIC_MESSAGES {
            betas::fit_request(&mut request);
        }
        let secret = self.secret().await?;
        wire::execute(
            self.host.http(),
            &self.target,
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
        betas::fit_request(&mut request);
        let secret = self.secret().await?;
        wire::count_input(
            self.host.http(),
            &self.target,
            secret.as_ref(),
            request,
            inbound,
        )
        .await
    }

    async fn list_models(&self, query: &ListQuery) -> Result<ModelList, PluginError> {
        let _ = query;
        let specs = MODELS
            .iter()
            .map(|id| ModelSpec::new(*id, None))
            .collect::<Result<Vec<_>, _>>()?;
        Ok(ModelList::new(specs, None).with_kind(ListKind::Catalog))
    }

    async fn probe(&self, query: &ProbeQuery) -> Result<ProbeReport, PluginError> {
        hofund_plugin_sdk::timeout(PROBE_WAIT, self.probe_inner(query))
            .await
            .unwrap_or_else(|| Err(PluginError::timeout("provider probe exceeded 20 seconds")))
    }
}

#[cfg(test)]
mod tests;
