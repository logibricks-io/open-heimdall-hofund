//! Remote gateway upstream: another computer's gateway, shared on its network.
//!
//! The other gateway serves every protocol this one does. With no preferred
//! API on record, a request goes on in the protocol the client spoke. After
//! a list, a model that names one is asked there when the client spoke an
//! API that model does not serve. Chat the client itself sent stays Chat.
//!
//! - Chat Completions and Responses at `<address>/v1`.
//! - Messages at `<address>`, so the SDK posts `<address>/v1/messages`.
//! - Any other inbound protocol is encoded once, here, to the protocol the
//!   provider was built for (Chat unless the config selects another).
//!
//! The address may be typed as a bare `host:port` (plain `http`), as
//! `.../v1`, or as one of the gateway's own endpoint URLs; see [`root`].
//! The key is the other gateway's API key, sent as a bearer token (and on
//! Messages also in `x-api-key`, as any third-party Messages API is sent).
//!
//! The same-protocol body is relayed as the client wrote it, so the request
//! is turned into another protocol at most once, never on both computers.
//!
//! Reference: the remote gateway preset's endpoint rule in
//! `internal/provider` at commit cbbf3163cb3c87ee0c327a56bc5689ddc5eda947.

#![deny(missing_docs)]

mod list;

use std::collections::HashMap;
use std::sync::{Arc, Mutex};

use hofund_plugin_api::{
    BuildContext, BuildError, Completion, ConfigError, CountOutcome, CredentialStyle, HostContext,
    ListKind, ListQuery, ModelList, ModelSpec, PluginError, PluginId, ProbeQuery, ProbeReport,
    ProtocolId, RawTable, RawValue, Request, Schema, SecretString, UpstreamFactory,
    UpstreamProvider, UpstreamSpec, ValidConfig, preferred_api_name,
};
use hofund_plugin_sdk::json::{Value, json};
use hofund_plugin_sdk::oauth::secret_text;
use hofund_plugin_sdk::probe::{PROBE_WAIT, ProbePage, merge_extra, probe_report, read_probe};
use hofund_plugin_sdk::wire::{self, Target};
use hofund_plugin_sdk::{Call, Contribution};

/// The probe's output cap, as every other upstream's probe.
const PROBE_TOKENS: u64 = 16;

/// Registers the remote gateway factory.
#[must_use]
pub fn contribute() -> Contribution {
    Contribution::new().upstream(RemoteGatewayFactory)
}

/// The spec names the key's default placement: a bearer token.
static SPEC: UpstreamSpec = UpstreamSpec::from_static(
    PluginId::from_static("remote-gateway"),
    &[
        ProtocolId::OPENAI_CHAT,
        ProtocolId::OPENAI_RESPONSES,
        ProtocolId::ANTHROPIC_MESSAGES,
    ],
    CredentialStyle::Bearer,
    ListKind::Live,
);

/// The address suffixes [`root`] strips. None ends with another, so at most
/// one matches and the order does not matter. reference: internal/provider remote gateway endpoints, lines 91-97
const SUFFIXES: [&str; 14] = [
    "/v1/messages/count_tokens",
    "/v1/messages",
    "/v1/chat/completions",
    "/v1/responses",
    "/v1/systemone",
    "/v1/embeddings",
    "/v1/rerank",
    "/v1/audio/transcriptions",
    "/v1/audio/translations",
    "/v1/audio/speech",
    "/v1/images/generations",
    "/v1/images/edits",
    "/v1/videos",
    "/v1",
];

/// The other gateway's root, however its address was typed: trimmed, with
/// `http://` when it names no scheme, and without one endpoint suffix.
/// reference: internal/provider remote gateway endpoints, lines 75-103
#[must_use]
pub fn root(address: &str) -> String {
    let mut root = address.trim().trim_end_matches('/').to_owned();
    if root.is_empty() {
        return root;
    }
    if !root.contains("://") {
        root = format!("http://{root}");
    }
    for suffix in SUFFIXES {
        if let Some(stripped) = root.strip_suffix(suffix) {
            return stripped.to_owned();
        }
    }
    root
}

/// The validated table: shape only. Values are checked in `build`, after the
/// host has resolved `env:` references.
struct RemoteGatewayConfig {
    base_url: String,
    headers: Vec<(String, String)>,
}

impl ValidConfig for RemoteGatewayConfig {}

struct RemoteGatewayFactory;

impl UpstreamFactory for RemoteGatewayFactory {
    type Config = RemoteGatewayConfig;

    fn spec(&self) -> &'static UpstreamSpec {
        &SPEC
    }

    fn config_schema(&self) -> Schema {
        Schema::object(vec![("base_url".to_owned(), Schema::string())], &[])
            .expect("remote-gateway schema")
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
        Ok(RemoteGatewayConfig { base_url, headers })
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
        if !served(protocol) {
            return Err(BuildError::new(format!(
                "remote-gateway does not serve protocol `{}`",
                protocol.as_str()
            )));
        }
        let root = root(&cfg.base_url);
        if root.is_empty() {
            return Err(BuildError::new("provider base URL is empty"));
        }
        Ok(Arc::new(RemoteGateway {
            protocol,
            root,
            headers: cfg.headers.clone(),
            host: host.clone(),
            listed: Mutex::new(HashMap::new()),
        }))
    }
}

/// Whether the other gateway is asked in `protocol` itself.
fn served(protocol: ProtocolId) -> bool {
    SPEC.protocols().contains(&protocol)
}

struct RemoteGateway {
    /// The protocol the provider was built for: where an inbound protocol
    /// the other gateway is not asked in goes.
    protocol: ProtocolId,
    /// The other gateway's root, without `/v1`.
    root: String,
    headers: Vec<(String, String)>,
    host: HostContext,
    /// Preferred API, endpoints, and a catalog entry learned from the last list.
    listed: Mutex<HashMap<String, ListedFact>>,
}

#[derive(Clone)]
struct ListedFact {
    preferred: String,
    endpoints: Vec<String>,
    catalog_entry: Option<String>,
}

impl RemoteGateway {
    /// The stored gateway key, if any.
    async fn secret(&self) -> Result<Option<SecretString>, PluginError> {
        self.host.secrets().get("api_key").await
    }

    /// The protocol a request in `inbound` for `model` goes on.
    ///
    /// Chat the client sent stays Chat. An inbound protocol the model's
    /// listed endpoints include is relayed. Anything else, once a list has
    /// named a preferred API, goes on that API. With no preference on record
    /// the client's own protocol is used when this gateway speaks it.
    fn forward(&self, inbound: ProtocolId, model: &str) -> ProtocolId {
        let Some(fact) = self.fact(model) else {
            return self.plain(inbound);
        };
        let Some(preferred) = protocol_of(&fact.preferred) else {
            return self.plain(inbound);
        };
        if inbound == ProtocolId::OPENAI_CHAT {
            return ProtocolId::OPENAI_CHAT;
        }
        if endpoint_serves(&fact.endpoints, inbound) || inbound == preferred {
            return inbound;
        }
        if served(preferred) {
            return preferred;
        }
        self.plain(inbound)
    }

    fn plain(&self, inbound: ProtocolId) -> ProtocolId {
        if served(inbound) {
            inbound
        } else {
            self.protocol
        }
    }

    fn fact(&self, model: &str) -> Option<ListedFact> {
        let listed = self
            .listed
            .lock()
            .unwrap_or_else(|error| error.into_inner());
        if let Some(fact) = listed.get(model) {
            return Some(fact.clone());
        }
        listed
            .iter()
            .find(|(id, _)| id_matches(id, model))
            .map(|(_, fact)| fact.clone())
    }

    fn remember(&self, models: &[ModelSpec]) {
        let mut listed = self
            .listed
            .lock()
            .unwrap_or_else(|error| error.into_inner());
        listed.clear();
        for model in models {
            listed.insert(
                model.id().to_owned(),
                ListedFact {
                    preferred: model.preferred_api().unwrap_or("").to_owned(),
                    endpoints: model.endpoints().to_vec(),
                    catalog_entry: model.catalog_entry().map(str::to_owned),
                },
            );
        }
    }

    /// The target for `protocol`: Chat and Responses under `/v1`, Messages
    /// at the root, as the SDK adds `/v1/messages`.
    fn target(&self, protocol: ProtocolId) -> Target {
        let (base, credential) = if protocol == ProtocolId::ANTHROPIC_MESSAGES {
            (self.root.clone(), CredentialStyle::MessagesBoth)
        } else {
            (format!("{}/v1", self.root), CredentialStyle::Bearer)
        };
        Target::new(protocol, base)
            .credential(credential)
            .headers(self.headers.clone())
    }

    async fn probe_inner(&self, query: &ProbeQuery) -> Result<ProbeReport, PluginError> {
        let secret = self.secret().await?;
        let key = secret.as_ref().map_or("", secret_text);
        let mut body = json!({
            "model": query.model(),
            "messages": [{"role": "user", "content": "hi"}],
            "max_completion_tokens": PROBE_TOKENS,
        });
        merge_extra(&mut body, query)?;
        let stream = body.get("stream").and_then(Value::as_bool).unwrap_or(false);
        let url = format!("{}/v1/chat/completions", self.root);
        let page = self
            .post_probe(&url, &body.to_string(), stream, secret.as_ref())
            .await?;
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
        let configured = self
            .headers
            .iter()
            .any(|(header, _)| header.eq_ignore_ascii_case("authorization"));
        let mut call = Call::post(url).json(body);
        if let Some(secret) = secret.filter(|secret| !secret_text(secret).is_empty())
            && !configured
        {
            call = call.credential(CredentialStyle::Bearer, secret)?;
        }
        for (name, value) in &self.headers {
            call = call.sensitive_header(name.to_ascii_lowercase(), value.clone());
        }
        let response = call.open_raw(self.host.http(), 1).await?;
        read_probe(response, stream).await
    }
}

#[hofund_plugin_sdk::async_trait]
impl UpstreamProvider for RemoteGateway {
    async fn execute(
        &self,
        request: Request,
        inbound: ProtocolId,
    ) -> Result<Completion, PluginError> {
        let secret = self.secret().await?;
        let protocol = self.forward(inbound, request.model.as_str());
        wire::execute(
            self.host.http(),
            &self.target(protocol),
            secret.as_ref(),
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
        let protocol = self.forward(inbound, request.model.as_str());
        if protocol != ProtocolId::ANTHROPIC_MESSAGES {
            return Ok(CountOutcome::Unsupported);
        }
        let secret = self.secret().await?;
        wire::count_input(
            self.host.http(),
            &self.target(protocol),
            secret.as_ref(),
            request,
            inbound,
        )
        .await
    }

    async fn list_models(&self, _query: &ListQuery) -> Result<ModelList, PluginError> {
        let secret = self.secret().await?;
        let lister = list::Lister {
            http: self.host.http(),
            url: format!("{}/v1/models", self.root),
            headers: &self.headers,
            secret: secret.as_ref(),
        };
        let list = lister.list().await?;
        let entries = lister.codex_catalog().await;
        let models = list
            .models()
            .iter()
            .cloned()
            .map(|model| match entries.get(model.id()) {
                Some(entry) => model.with_catalog_entry(entry.clone()),
                None => model,
            })
            .collect::<Vec<_>>();
        self.remember(&models);
        let mut rebuilt = ModelList::new(models, list.next().map(str::to_owned))
            .with_dropped(list.dropped().to_vec())
            .with_kind(list.kind());
        if let Some(url) = list.source_url() {
            rebuilt = rebuilt.with_source_url(url);
        }
        Ok(rebuilt)
    }

    fn learned_preferred(&self, model: &str) -> Option<String> {
        self.fact(model)
            .and_then(|fact| preferred_api_name(&fact.preferred).map(str::to_owned))
    }

    fn learned_catalog_entry(&self, model: &str) -> Option<String> {
        self.fact(model).and_then(|fact| fact.catalog_entry)
    }

    async fn probe(&self, query: &ProbeQuery) -> Result<ProbeReport, PluginError> {
        hofund_plugin_sdk::timeout(PROBE_WAIT, self.probe_inner(query))
            .await
            .unwrap_or_else(|| Err(PluginError::timeout("provider probe exceeded 20 seconds")))
    }
}

fn protocol_of(name: &str) -> Option<ProtocolId> {
    match preferred_api_name(name)? {
        "chat" => Some(ProtocolId::OPENAI_CHAT),
        "responses" => Some(ProtocolId::OPENAI_RESPONSES),
        "anthropic" => Some(ProtocolId::ANTHROPIC_MESSAGES),
        _ => None,
    }
}

fn endpoint_serves(endpoints: &[String], protocol: ProtocolId) -> bool {
    let Some(wanted) = preferred_api_name(protocol.as_str()) else {
        return false;
    };
    endpoints
        .iter()
        .any(|endpoint| preferred_api_name(endpoint) == Some(wanted))
}

fn id_matches(listed: &str, asked: &str) -> bool {
    asked
        .strip_suffix(listed)
        .is_some_and(|prefix| prefix.ends_with('/'))
        || listed
            .strip_suffix(asked)
            .is_some_and(|prefix| prefix.ends_with('/'))
}

#[cfg(test)]
mod tests;
