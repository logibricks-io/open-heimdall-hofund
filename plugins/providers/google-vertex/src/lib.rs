//! Google Vertex AI upstream.
//!
//! Gemini on Vertex AI speaks the Generative Language wire under a Google
//! Cloud project: `https://<host>/v1/projects/<project>/locations/<location>`
//! followed by `publishers/google/models/<model>:generateContent`. It signs
//! with a Google access token, never an API key. The token comes from
//! Application Default Credentials (`gcloud auth application-default login`),
//! a service account key file, or either one trading its token for a service
//! account's through the IAM Credentials API.
//!
//! Vertex AI has no model list a key can read, so the list is the static set a
//! location serves: the catalog row's for `global`, the Gemini 3 Flash ids for
//! the `us` and `eu` multi-regions, and the Gemini 2.5 ids for a region.
//!
//! References:
//! <https://cloud.google.com/vertex-ai/generative-ai/docs/model-reference/inference>,
//! <https://cloud.google.com/vertex-ai/generative-ai/docs/learn/locations>.

#![deny(missing_docs)]

mod auth;
mod base;

use std::sync::Arc;

use hofund_plugin_api::{
    BuildContext, BuildError, Completion, ConfigError, CountOutcome, CredentialStyle, HostContext,
    ListKind, ListQuery, ModelList, ModelSpec, PluginError, PluginId, ProbeQuery, ProbeReport,
    ProtocolId, RawTable, RawValue, Request, Schema, SecretString, UpstreamFactory,
    UpstreamProvider, UpstreamSpec, ValidConfig,
};
use hofund_plugin_sdk::json::{Map, Value, json};
use hofund_plugin_sdk::oauth::secret_text;
use hofund_plugin_sdk::probe::{PROBE_WAIT, merge_extra, probe_report, read_probe};
use hofund_plugin_sdk::wire::{self, Target, model_segment};
use hofund_plugin_sdk::{Call, Contribution};

use crate::auth::Minter;

/// Where the Generative Language model paths sit under a Vertex AI root.
const MODEL_ROOT: &str = "publishers/google/models";

/// The probe's output cap, as every other upstream's probe.
const PROBE_TOKENS: u64 = 16;

/// The string options `validate` reads besides `base_url`.
const OPTIONS: &[&str] = &[
    "project",
    "location",
    "credentials",
    "impersonate",
    "token_url",
    "iam_root",
];

/// Registers the Google Vertex AI factory.
#[must_use]
pub fn contribute() -> Contribution {
    Contribution::new().upstream(VertexFactory)
}

static SPEC: UpstreamSpec = UpstreamSpec::from_static(
    PluginId::from_static("google-vertex"),
    &[ProtocolId::GENERATIVE_LANGUAGE],
    CredentialStyle::Bearer,
    ListKind::Catalog,
);

/// The validated table: shape only. Values are checked in `build`, after the
/// host has resolved `env:` references.
struct VertexConfig {
    base_url: String,
    headers: Vec<(String, String)>,
    project: String,
    location: String,
    credentials: String,
    impersonate: String,
    token_url: String,
    iam_root: String,
}

impl ValidConfig for VertexConfig {}

struct VertexFactory;

fn string(raw: &RawTable, key: &str) -> Result<String, ConfigError> {
    match raw.get(key) {
        None => Ok(String::new()),
        Some(RawValue::String(value)) => Ok(value.clone()),
        Some(_) => Err(ConfigError::new("must be a string").with_path(key)),
    }
}

impl UpstreamFactory for VertexFactory {
    type Config = VertexConfig;

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
        Schema::object(fields, &[]).expect("google-vertex schema")
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
        Ok(VertexConfig {
            base_url: string(raw, "base_url")?,
            headers,
            project: string(raw, "project")?,
            location: string(raw, "location")?,
            credentials: string(raw, "credentials")?.trim().to_owned(),
            impersonate: string(raw, "impersonate")?.trim().to_ascii_lowercase(),
            token_url: string(raw, "token_url")?,
            iam_root: string(raw, "iam_root")?,
        })
    }

    fn preflight(&self, cfg: &Self::Config) -> Result<(), ConfigError> {
        base::check_credentials_file(&cfg.credentials)
    }

    fn build(
        &self,
        cfg: &Self::Config,
        host: &HostContext,
    ) -> Result<Arc<dyn UpstreamProvider>, BuildError> {
        self.build_with(
            cfg,
            host,
            &BuildContext::new(ProtocolId::GENERATIVE_LANGUAGE),
        )
    }

    fn build_with(
        &self,
        cfg: &Self::Config,
        host: &HostContext,
        ctx: &BuildContext,
    ) -> Result<Arc<dyn UpstreamProvider>, BuildError> {
        if ctx.protocol() != ProtocolId::GENERATIVE_LANGUAGE {
            return Err(BuildError::new(format!(
                "google-vertex does not serve protocol `{}`",
                ctx.protocol().as_str()
            )));
        }
        base::check_token_urls(&cfg.token_url, &cfg.iam_root)?;
        base::check_settings(&cfg.credentials, &cfg.impersonate)?;
        let base =
            base::base_url(&cfg.project, &cfg.location, &cfg.base_url).map_err(BuildError::new)?;
        let location = base::location_of(&base);
        Ok(Arc::new(Vertex {
            target: Target::new(ProtocolId::GENERATIVE_LANGUAGE, base.clone())
                .credential(CredentialStyle::Bearer)
                .headers(cfg.headers.clone())
                .model_root(MODEL_ROOT),
            base,
            headers: cfg.headers.clone(),
            location,
            minter: Minter::new(
                cfg.credentials.clone(),
                cfg.impersonate.clone(),
                cfg.token_url.clone(),
                cfg.iam_root.clone(),
            ),
            host: host.clone(),
        }))
    }
}

struct Vertex {
    base: String,
    target: Target,
    headers: Vec<(String, String)>,
    location: String,
    minter: Minter,
    host: HostContext,
}

impl Vertex {
    /// A Google access token. A stored API key is refused: Vertex AI takes
    /// none, and sending one as a bearer would leak it to Google's IAM.
    async fn token(&self) -> Result<SecretString, PluginError> {
        if let Some(key) = self.host.secrets().get("api_key").await?
            && !secret_text(&key).is_empty()
        {
            return Err(PluginError::config(
                "Google Vertex AI uses Google credentials, not an API key",
            ));
        }
        self.minter.access_token(&self.host).await
    }

    async fn probe_inner(&self, query: &ProbeQuery) -> Result<ProbeReport, PluginError> {
        let token = self.token().await?;
        let model = model_segment(query.model())?;
        let url = format!(
            "{}/{MODEL_ROOT}/{model}:generateContent",
            self.base.trim_end_matches('/')
        );
        let mut generation = Map::new();
        generation.insert("maxOutputTokens".to_owned(), Value::from(PROBE_TOKENS));
        if let Some(thinking) = probe_thinking(query.model()) {
            generation.insert("thinkingConfig".to_owned(), thinking);
        }
        let mut body = json!({
            "contents": [{"role": "user", "parts": [{"text": "hi"}]}],
            "generationConfig": generation,
        });
        merge_extra(&mut body, query)?;
        let stream = body.get("stream").and_then(Value::as_bool).unwrap_or(false);
        let mut call = Call::post(url.as_str()).json(body.to_string());
        for (name, value) in &self.headers {
            call = call.sensitive_header(name.to_ascii_lowercase(), value.clone());
        }
        let call = call.bearer(&token)?;
        let response = call.open_raw(self.host.http(), 1).await?;
        let page = read_probe(response, stream).await?;
        let mut secrets: Vec<&str> = vec![secret_text(&token)];
        secrets.extend(self.headers.iter().map(|(_, value)| value.as_str()));
        probe_report(&url, &page, stream, &secrets)
    }
}

/// The thinking setting that keeps a probe short: Gemini 2.5 Pro cannot turn
/// thinking off, so it gets the smallest budget; other 2.5 models turn it
/// off; Gemini 3 and later take a low level.
fn probe_thinking(model: &str) -> Option<Value> {
    let lower = model.to_ascii_lowercase();
    if lower.starts_with("gemini-2.5-pro") {
        Some(json!({"thinkingBudget": 128}))
    } else if lower.starts_with("gemini-2.5") {
        Some(json!({"thinkingBudget": 0}))
    } else if generation(&lower) >= 3 {
        Some(json!({"thinkingLevel": "low"}))
    } else {
        None
    }
}

/// The major Gemini generation in a model id, or 0 when it names none.
fn generation(model: &str) -> u32 {
    let Some(rest) = model.strip_prefix("gemini-") else {
        return 0;
    };
    rest.split(['-', '.'])
        .next()
        .and_then(|generation| generation.parse().ok())
        .unwrap_or(0)
}

#[hofund_plugin_sdk::async_trait]
impl UpstreamProvider for Vertex {
    async fn execute(
        &self,
        request: Request,
        inbound: ProtocolId,
    ) -> Result<Completion, PluginError> {
        let token = self.token().await?;
        wire::execute(
            self.host.http(),
            &self.target,
            Some(&token),
            request,
            inbound,
        )
        .await
    }

    async fn count_input(
        &self,
        request: Request,
        inbound: ProtocolId,
    ) -> Result<CountOutcome, PluginError> {
        let token = self.token().await?;
        wire::count_input(
            self.host.http(),
            &self.target,
            Some(&token),
            request,
            inbound,
        )
        .await
    }

    async fn list_models(&self, query: &ListQuery) -> Result<ModelList, PluginError> {
        let _ = query;
        let specs = base::models_for(&self.location)
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
