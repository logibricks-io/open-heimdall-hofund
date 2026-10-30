//! Remote gateway upstream: another computer's gateway, shared on its network.
//!
//! The other gateway serves every protocol this one does, so a request goes
//! on in the protocol the client spoke:
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

use std::sync::Arc;

use hofund_plugin_api::{
    BuildContext, BuildError, Completion, ConfigError, CountOutcome, CredentialStyle, HostContext,
    ListKind, ListQuery, ModelList, PluginError, PluginId, ProbeQuery, ProbeReport, ProtocolId,
    RawTable, RawValue, Request, Schema, SecretString, UpstreamFactory, UpstreamProvider,
    UpstreamSpec, ValidConfig,
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
const SUFFIXES: [&str; 13] = [
    "/v1/messages/count_tokens",
    "/v1/messages",
    "/v1/chat/completions",
    "/v1/responses",
    "/v1/systemone",
    "/v1/embeddings",
    "/v1/rerank",
    "/v1/audio/transcriptions",
    "/v1/audio/translations",
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
}

impl RemoteGateway {
    /// The stored gateway key, if any.
    async fn secret(&self) -> Result<Option<SecretString>, PluginError> {
        self.host.secrets().get("api_key").await
    }

    /// The protocol a request in `inbound` goes on: the client's own when
    /// the other gateway is asked in it, else the built protocol.
    fn forward(&self, inbound: ProtocolId) -> ProtocolId {
        if served(inbound) {
            inbound
        } else {
            self.protocol
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
        wire::execute(
            self.host.http(),
            &self.target(self.forward(inbound)),
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
        let protocol = self.forward(inbound);
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
        list::Lister {
            http: self.host.http(),
            url: format!("{}/v1/models", self.root),
            headers: &self.headers,
            secret: secret.as_ref(),
        }
        .list()
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
