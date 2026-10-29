//! Azure OpenAI upstream.
//!
//! An Azure OpenAI resource is reached at its own host, never at one the
//! gateway knows: `https://<resource>.openai.azure.com`, or the
//! `cognitiveservices.azure.com` and `services.ai.azure.com` hosts newer
//! resources get. Calls go to the resource's v1 API under `/openai/v1`, where
//! chat completions, Responses, and the model list sit at OpenAI's paths and
//! the model in the body is the deployment's name. The key is sent in
//! `api-key` (`x-api-key` on Messages), never as a bearer token: a bearer on
//! Azure is an Entra ID token.
//!
//! The model list asks the data plane for the resource's deployments, whose
//! names are the model ids, and falls back to the v1 base-model list, labelled
//! as a fallback because those ids match deployments only when each is named
//! after its model.
//!
//! reference: `internal/provider/azure.go` at commit
//! cbbf3163cb3c87ee0c327a56bc5689ddc5eda947.

#![deny(missing_docs)]

use std::sync::Arc;

use hofund_plugin_api::{
    BuildContext, BuildError, Completion, ConfigError, CountOutcome, CredentialStyle, HostContext,
    ListKind, ListQuery, ModelList, ModelSpec, PluginError, PluginErrorKind, PluginId, ProbeQuery,
    ProbeReport, ProtocolId, RawTable, RawValue, Request, Schema, SecretString, UpstreamFactory,
    UpstreamProvider, UpstreamSpec, ValidConfig, safe_url,
};
use hofund_plugin_sdk::json::Value;
use hofund_plugin_sdk::wire::{self, Target};
use hofund_plugin_sdk::{
    Call, Contribution, PageError, PageErrorKind, after_id, classify::classify_model,
    json_error_message,
};

/// The data plane's `api-version` for the deployment list: later versions
/// dropped the list, this one still answers a key.
const DEPLOYMENTS_VERSION: &str = "2022-12-01";

/// Host suffixes of Azure OpenAI resources.
const AZURE_HOSTS: &[&str] = &[
    ".openai.azure.com",
    ".cognitiveservices.azure.com",
    ".services.ai.azure.com",
];

/// Appended to a failed list, so the user knows the manual path.
const MANUAL_ENTRY: &str = " — type your deployments' names in as model ids";

/// The probe's output cap, as every other upstream's probe.
const PROBE_TOKENS: u64 = 16;

/// Registers the Azure OpenAI factory.
#[must_use]
pub fn contribute() -> Contribution {
    Contribution::new().upstream(AzureFactory)
}

static SPEC: UpstreamSpec = UpstreamSpec::from_static(
    PluginId::from_static("azure"),
    &[
        ProtocolId::OPENAI_CHAT,
        ProtocolId::OPENAI_RESPONSES,
        ProtocolId::ANTHROPIC_MESSAGES,
    ],
    CredentialStyle::ApiKey,
    // The bound across instances: Chat and Responses list deployments,
    // Messages lists live; the host resolves each instance's kind.
    ListKind::Live,
);

/// Whether `host` is an Azure OpenAI resource's host. A port is ignored.
#[must_use]
pub fn azure_host(host: &str) -> bool {
    let host = host.to_ascii_lowercase();
    let host = host.split_once(':').map_or(host.as_str(), |(name, _)| name);
    AZURE_HOSTS
        .iter()
        .any(|suffix| host.len() > suffix.len() && host.ends_with(suffix))
}

/// The v1 API root of the resource `raw` names.
///
/// A pasted resource, `https://<resource>.openai.azure.com`, a classic
/// deployment URL, a `cognitiveservices.azure.com` or `services.ai.azure.com`
/// host, or the resource name alone, becomes `https://<host>/openai/v1`. A
/// loopback URL is kept, so a test server is not rewritten.
///
/// # Errors
///
/// Returns a message when `raw` is empty, has no host, or names no Azure
/// OpenAI host.
pub fn azure_base(raw: &str) -> Result<String, String> {
    let mut raw = raw.trim().to_owned();
    if raw.is_empty() {
        return Err("preset `azure` needs base_url".to_owned());
    }
    if !raw.contains("://") {
        raw = format!("https://{raw}");
    }
    let host =
        host_of(&raw).ok_or_else(|| format!("preset `azure` base URL `{raw}` has no host"))?;
    if matches!(host.as_str(), "localhost" | "127.0.0.1" | "[::1]") {
        return Ok(raw.trim_end_matches('/').to_owned());
    }
    let host = if host.contains('.') {
        host
    } else {
        format!("{host}.openai.azure.com")
    };
    if !azure_host(&host) {
        return Err(format!(
            "preset `azure` base URL `{raw}` is not an Azure OpenAI host"
        ));
    }
    Ok(format!("https://{host}/openai/v1"))
}

/// The lowercase host of `url` without its port, or `None` when it has none.
fn host_of(url: &str) -> Option<String> {
    let (_, rest) = url.split_once("://")?;
    let authority = rest.split(['/', '?', '#']).next().unwrap_or_default();
    let authority = authority
        .rsplit_once('@')
        .map_or(authority, |(_, host)| host);
    let host = if authority.starts_with('[') {
        authority
            .find(']')
            .map_or(authority, |end| &authority[..=end])
    } else {
        authority.split(':').next().unwrap_or_default()
    };
    (!host.is_empty()).then(|| host.to_ascii_lowercase())
}

/// The validated table: shape only. The URL is checked in `build`.
struct AzureConfig {
    base_url: String,
    headers: Vec<(String, String)>,
}

impl ValidConfig for AzureConfig {}

struct AzureFactory;

impl UpstreamFactory for AzureFactory {
    type Config = AzureConfig;

    fn spec(&self) -> &'static UpstreamSpec {
        &SPEC
    }

    fn config_schema(&self) -> Schema {
        Schema::object(
            vec![("base_url".to_owned(), Schema::string())],
            &["base_url"],
        )
        .expect("azure schema")
    }

    fn validate(&self, raw: &RawTable) -> Result<Self::Config, ConfigError> {
        for key in raw.keys() {
            if key != "base_url" && key != "headers" {
                return Err(ConfigError::new("unknown key").with_path(key));
            }
        }
        let base_url = match raw.get("base_url") {
            Some(RawValue::String(url)) => url.clone(),
            Some(_) => {
                return Err(ConfigError::new("base_url must be a string").with_path("base_url"));
            }
            None => {
                return Err(ConfigError::new("preset `azure` needs base_url").with_path("base_url"));
            }
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
        Ok(AzureConfig { base_url, headers })
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
        if !SPEC.protocols().contains(&protocol) {
            return Err(BuildError::new(format!(
                "azure does not serve protocol `{}`",
                protocol.as_str()
            )));
        }
        let base_url = azure_base(&cfg.base_url).map_err(BuildError::new)?;
        Ok(Arc::new(Azure {
            base_url,
            headers: cfg.headers.clone(),
            protocol,
            host: host.clone(),
        }))
    }
}

struct Azure {
    base_url: String,
    headers: Vec<(String, String)>,
    protocol: ProtocolId,
    host: HostContext,
}

impl Azure {
    async fn secret(&self) -> Result<Option<SecretString>, PluginError> {
        self.host.secrets().get("api_key").await
    }

    fn target(&self) -> Target {
        let credential = if self.protocol == ProtocolId::ANTHROPIC_MESSAGES {
            CredentialStyle::XApiKey
        } else {
            CredentialStyle::ApiKey
        };
        Target::new(self.protocol, self.base_url.clone())
            .credential(credential)
            .headers(self.headers.clone())
    }

    /// Removes the user's header values from `text`.
    fn redact(&self, text: &str) -> String {
        let mut clean = text.to_owned();
        for (_, value) in &self.headers {
            if !value.is_empty() {
                clean = clean.replace(value.as_str(), "[redacted]");
            }
        }
        clean
    }

    /// The list key header: `x-api-key` on Messages, else `api-key`.
    fn key_header(&self) -> (CredentialStyle, &'static str) {
        if self.protocol == ProtocolId::ANTHROPIC_MESSAGES {
            (CredentialStyle::XApiKey, "x-api-key")
        } else {
            (CredentialStyle::ApiKey, "api-key")
        }
    }

    /// One list page: the key alone, then the user's headers, which replace a
    /// header of the same name.
    async fn page(&self, url: &str, secret: Option<&SecretString>) -> Result<Vec<u8>, PluginError> {
        let (style, name) = self.key_header();
        let mut call = Call::get(url).header("accept", "application/json");
        let replaced = self
            .headers
            .iter()
            .any(|(header, _)| header.eq_ignore_ascii_case(name));
        if let Some(secret) = secret.filter(|_| !replaced) {
            call = call.credential(style, secret)?;
        }
        for (name, value) in &self.headers {
            call = call.sensitive_header(name.to_ascii_lowercase(), value.clone());
        }
        let reply = call.fetch(self.host.http()).await?;
        if reply.status() == 200 {
            return Ok(reply.into_body());
        }
        let detail = json_error_message(reply.body());
        let mut message = format!("{}: HTTP {}", safe_url(url), reply.status());
        if !detail.is_empty() {
            message.push_str(&format!(" ({detail})"));
        }
        Err(PluginError::status_error(
            reply.status(),
            self.redact(&message),
        ))
    }

    /// The list candidates in order. Chat and Responses ask the deployment
    /// list, then the base models; Messages asks the live candidates the
    /// kernel's Anthropic list used for this base (`/models`, `/v1/models`,
    /// then the root's `/models`).
    fn candidates(&self) -> Vec<Candidate> {
        let base = self.base_url.trim_end_matches('/');
        let root = base
            .strip_suffix("/v1")
            .unwrap_or(base)
            .trim_end_matches('/');
        let candidate = |url: String, kind, base_models, v1| Candidate {
            url,
            kind,
            base_models,
            v1,
        };
        if self.protocol == ProtocolId::ANTHROPIC_MESSAGES {
            let mut list: Vec<Candidate> = Vec::new();
            for (url, v1) in [
                (format!("{base}/models"), false),
                (format!("{base}/v1/models"), true),
                (format!("{root}/v1/models"), false),
                (format!("{root}/models"), false),
            ] {
                if !list.iter().any(|seen| seen.url == url) {
                    list.push(candidate(url, ListKind::Live, false, v1));
                }
            }
            return list;
        }
        vec![
            candidate(
                format!("{root}/deployments?api-version={DEPLOYMENTS_VERSION}"),
                ListKind::Deployments,
                false,
                false,
            ),
            candidate(format!("{base}/models"), ListKind::Deployments, true, false),
        ]
    }

    async fn list(&self) -> Result<ModelList, PluginError> {
        let secret = self.secret().await?;
        let mut errors = Vec::new();
        for candidate in self.candidates() {
            let rows = after_id(&candidate.url, |at| {
                let secret = secret.clone();
                async move { self.page(&at, secret.as_ref()).await.map_err(ListFailure) }
            })
            .await
            .map_err(|failure| failure.0);
            match rows {
                Ok(rows) => {
                    let specs = model_specs(&rows)?;
                    if specs.is_empty() {
                        errors.push(PluginError::decode(format!(
                            "{}: no models listed",
                            safe_url(&candidate.url)
                        )));
                        continue;
                    }
                    return Ok(ModelList::new(specs, None)
                        .with_kind(candidate.kind)
                        .with_base_model_fallback(candidate.base_models)
                        .with_v1_fallback(candidate.v1)
                        .with_source_url(candidate.url.as_str()));
                }
                Err(error) => {
                    let stop = stops(&error);
                    errors.push(error);
                    if stop {
                        break;
                    }
                }
            }
        }
        let suffix = if self.protocol == ProtocolId::ANTHROPIC_MESSAGES {
            ""
        } else {
            MANUAL_ENTRY
        };
        Err(join_errors(errors, suffix))
    }
}

/// Whether a failed candidate ends the list instead of trying the next one.
fn stops(error: &PluginError) -> bool {
    error.kind() == PluginErrorKind::Cancelled
}

/// One list URL and what a non-empty answer from it means.
struct Candidate {
    url: String,
    kind: ListKind,
    /// The ids are base models, not deployment names.
    base_models: bool,
    /// The URL added `/v1` to a base without it.
    v1: bool,
}

#[hofund_plugin_sdk::async_trait]
impl UpstreamProvider for Azure {
    async fn execute(
        &self,
        request: Request,
        inbound: ProtocolId,
    ) -> Result<Completion, PluginError> {
        let secret = self.secret().await?;
        wire::execute(
            self.host.http(),
            &self.target(),
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
        if self.protocol != ProtocolId::ANTHROPIC_MESSAGES {
            return Ok(CountOutcome::Unsupported);
        }
        let secret = self.secret().await?;
        wire::count_input(
            self.host.http(),
            &self.target(),
            secret.as_ref(),
            request,
            inbound,
        )
        .await
    }

    async fn list_models(&self, query: &ListQuery) -> Result<ModelList, PluginError> {
        let _ = query;
        self.list().await
    }

    /// Chat is capped with `max_completion_tokens`, which Azure's reasoning
    /// deployments require; the SDK chat encoder writes the cap there.
    async fn probe(&self, query: &ProbeQuery) -> Result<ProbeReport, PluginError> {
        let mut request = query.minimal_request();
        request.params.max_tokens = Some(PROBE_TOKENS);
        let completion = self.execute(request, query.protocol()).await?;
        ProbeReport::from_completion(completion).await
    }
}

/// A list failure carried through [`after_id`].
struct ListFailure(PluginError);

impl From<PageError> for ListFailure {
    fn from(error: PageError) -> Self {
        let message = error.to_string();
        if error.kind() == PageErrorKind::Budget {
            Self(PluginError::timeout(message))
        } else {
            Self(PluginError::decode(message))
        }
    }
}

/// The chat-usable rows of a deployment or base-model list.
fn model_specs(rows: &[Value]) -> Result<Vec<ModelSpec>, PluginError> {
    let mut specs = Vec::new();
    for row in rows {
        if let Some(spec) = model_spec(row)? {
            specs.push(spec);
        }
    }
    Ok(specs)
}

fn model_spec(row: &Value) -> Result<Option<ModelSpec>, PluginError> {
    if let Value::String(id) = row {
        if id.is_empty() || classify_model(id, "").is_none() {
            return Ok(None);
        }
        return ModelSpec::new(id.clone(), Some(id.clone())).map(Some);
    }
    let Some(row) = row.as_object() else {
        return Ok(None);
    };
    let text = |key: &str| {
        row.get(key)
            .and_then(Value::as_str)
            .filter(|v| !v.is_empty())
    };
    let Some(id) = text("id").or_else(|| text("name")) else {
        return Ok(None);
    };
    let Some(tags) = classify_model(id, text("kind").unwrap_or("")) else {
        return Ok(None);
    };
    let name = text("display_name")
        .or_else(|| text("name").filter(|name| !name.contains(": ")))
        .unwrap_or(id);
    let positive = |key: &str| {
        row.get(key)
            .and_then(Value::as_f64)
            .filter(|value| value.is_finite() && *value >= 1.0 && *value <= u64::MAX as f64)
            .map_or(0, |value| value as u64)
    };
    Ok(Some(
        ModelSpec::new(id, Some(name.to_owned()))?
            .with_draws(tags.draws())
            .with_films(tags.films())
            .with_decides(tags.decides())
            .with_context(positive("context_length"))
            .with_max_output(positive("max_output_tokens")),
    ))
}

/// Joins every URL's failure, keeping the last one's kind, status, and wait.
fn join_errors(errors: Vec<PluginError>, suffix: &str) -> PluginError {
    let Some(last) = errors.last() else {
        return PluginError::decode(format!("azure model list failed{suffix}"));
    };
    let mut message = errors
        .iter()
        .map(PluginError::message)
        .collect::<Vec<_>>()
        .join("; ");
    message.push_str(suffix);
    let mut joined = match last.kind() {
        PluginErrorKind::Connect => PluginError::connect(message),
        PluginErrorKind::Timeout => PluginError::timeout(message),
        PluginErrorKind::Transport => PluginError::transport(message),
        PluginErrorKind::Interrupted => PluginError::interrupted(message),
        PluginErrorKind::Status => PluginError::status_error(last.status().unwrap_or(502), message),
        PluginErrorKind::Unsupported => PluginError::unsupported(message),
        PluginErrorKind::Auth => PluginError::auth(message),
        PluginErrorKind::Quota => PluginError::quota(message),
        PluginErrorKind::Config => PluginError::config(message),
        PluginErrorKind::Cancelled => PluginError::cancelled(message),
        PluginErrorKind::Io => PluginError::io(message),
        PluginErrorKind::Internal => PluginError::internal(message),
        _ => PluginError::decode(message),
    };
    if let Some(after) = last.retry_after() {
        joined = joined.with_retry_after(after);
    }
    joined
}

#[cfg(test)]
mod tests {
    use hofund_kit_upstream::{TestHost, block_on};
    use hofund_plugin_api::{
        BuildContext, CountOutcome, DynUpstreamFactory, ListQuery, PluginErrorKind, PluginId,
        ProbeQuery, ProtocolId, RawTable, RawValue, Request, UpstreamFactory, UpstreamProvider,
    };
    use std::sync::Arc;

    use super::{AzureFactory, azure_base, azure_host, host_of};

    const DEPLOYMENTS: &str =
        "https://mine.openai.azure.com/openai/deployments?api-version=2022-12-01";
    const MODELS: &str = "https://mine.openai.azure.com/openai/v1/models";

    fn options(url: &str) -> RawTable {
        let mut raw = RawTable::new();
        raw.insert("base_url", RawValue::String(url.to_owned()));
        raw
    }

    fn provider(
        host: &TestHost,
        raw: &RawTable,
        protocol: ProtocolId,
    ) -> Arc<dyn UpstreamProvider> {
        DynUpstreamFactory::validate_and_build_with(
            &AzureFactory,
            raw,
            host.context(),
            &BuildContext::new(protocol),
        )
        .expect("build")
    }

    fn keyed(root: &std::path::Path) -> TestHost {
        let plugin = PluginId::from_static("azure");
        let host = TestHost::open(plugin.clone(), root);
        host.secrets().insert(&plugin, "api_key", "azure-key");
        host
    }

    #[test]
    fn a_resource_name_or_pasted_url_becomes_the_v1_root() {
        assert_eq!(
            azure_base("my-resource").expect("name"),
            "https://my-resource.openai.azure.com/openai/v1"
        );
        assert_eq!(
            azure_base(
                " https://mine.openai.azure.com/openai/deployments/demo?api-version=2024-10-21 "
            )
            .expect("pasted"),
            "https://mine.openai.azure.com/openai/v1"
        );
        assert_eq!(
            azure_base("https://Team.CognitiveServices.Azure.com:443/").expect("host"),
            "https://team.cognitiveservices.azure.com/openai/v1"
        );
        assert_eq!(
            azure_base("http://127.0.0.1:9/v1/").expect("loopback"),
            "http://127.0.0.1:9/v1"
        );
        assert_eq!(
            azure_base("http://localhost:9").expect("localhost"),
            "http://localhost:9"
        );
        for bad in [
            "",
            "  ",
            "https://api.openai.com/v1",
            "https://",
            "https://openai.azure.com",
        ] {
            assert!(azure_base(bad).is_err(), "{bad:?}");
        }
    }

    #[test]
    fn hosts_are_matched_by_suffix_without_port_or_user() {
        assert!(azure_host("x.openai.azure.com"));
        assert!(azure_host("X.SERVICES.AI.AZURE.COM:8443"));
        assert!(azure_host("x.cognitiveservices.azure.com"));
        assert!(!azure_host(".openai.azure.com"));
        assert!(!azure_host("openai.azure.com.evil.test"));
        assert_eq!(
            host_of("https://user:pw@Mine.openai.azure.com:443/x?y#z").as_deref(),
            Some("mine.openai.azure.com")
        );
        assert_eq!(host_of("http://[::1]:9/v1").as_deref(), Some("[::1]"));
        assert_eq!(host_of("no-scheme"), None);
        assert_eq!(host_of("https:///path"), None);
    }

    #[test]
    fn validate_checks_shape_and_build_checks_the_host() {
        let mut raw = options("my-resource");
        raw.insert("region", RawValue::String("us".to_owned()));
        let Err(error) = UpstreamFactory::validate(&AzureFactory, &raw) else {
            panic!("unknown key was accepted");
        };
        assert_eq!(error.path(), "region");
        let Err(error) = UpstreamFactory::validate(&AzureFactory, &RawTable::new()) else {
            panic!("missing base_url was accepted");
        };
        assert_eq!(error.path(), "base_url");
        let mut raw = options("my-resource");
        raw.insert("headers", RawValue::String("x".to_owned()));
        let Err(error) = UpstreamFactory::validate(&AzureFactory, &raw) else {
            panic!("non-table headers were accepted");
        };
        assert_eq!(error.path(), "headers");
        let mut headers = RawTable::new();
        headers.insert("x-ms-tenant", RawValue::Integer(1));
        let mut raw = options("my-resource");
        raw.insert("headers", RawValue::Table(headers));
        let Err(error) = UpstreamFactory::validate(&AzureFactory, &raw) else {
            panic!("a non-string header was accepted");
        };
        assert_eq!(error.path(), "headers.x-ms-tenant");
        assert!(
            UpstreamFactory::validate(&AzureFactory, &options("https://api.openai.com")).is_ok()
        );

        let root = tempfile::tempdir().expect("scratch");
        let host = keyed(root.path());
        let Err(error) = DynUpstreamFactory::validate_and_build(
            &AzureFactory,
            &options("https://api.openai.com/v1"),
            host.context(),
        ) else {
            panic!("a non-Azure host was built");
        };
        assert!(
            error.to_string().contains("not an Azure OpenAI host"),
            "{error}"
        );
        let Err(error) = DynUpstreamFactory::validate_and_build_with(
            &AzureFactory,
            &options("my-resource"),
            host.context(),
            &BuildContext::new(ProtocolId::GENERATIVE_LANGUAGE),
        ) else {
            panic!("an undeclared protocol was built");
        };
        assert!(error.to_string().contains("does not declare"), "{error}");
    }

    #[test]
    fn chat_and_responses_send_api_key_and_messages_sends_x_api_key() {
        block_on(async {
            for (protocol, path, header) in [
                (ProtocolId::OPENAI_CHAT, "chat/completions", "api-key"),
                (ProtocolId::OPENAI_RESPONSES, "responses", "api-key"),
                (ProtocolId::ANTHROPIC_MESSAGES, "v1/messages", "x-api-key"),
            ] {
                let root = tempfile::tempdir().expect("scratch");
                let host = keyed(root.path());
                host.http().enqueue(b"{}".to_vec());
                let provider = provider(&host, &options("mine"), protocol);
                let _ = provider.execute(Request::new("team-gpt"), protocol).await;
                let calls = host.http().calls();
                assert_eq!(calls.len(), 1, "{path}");
                assert_eq!(
                    calls[0].uri(),
                    format!("https://mine.openai.azure.com/openai/v1/{path}")
                );
                assert!(
                    calls[0]
                        .sensitive_headers()
                        .iter()
                        .any(|name| name == header),
                    "{path}: {:?}",
                    calls[0].sensitive_headers()
                );
                assert!(
                    !calls[0]
                        .sensitive_headers()
                        .iter()
                        .any(|name| name == "authorization"),
                    "{path}"
                );
            }
        });
    }

    #[test]
    fn count_is_unsupported_off_messages() {
        block_on(async {
            let root = tempfile::tempdir().expect("scratch");
            let host = keyed(root.path());
            let provider = provider(&host, &options("mine"), ProtocolId::OPENAI_CHAT);
            let outcome = provider
                .count_input(Request::new("m"), ProtocolId::OPENAI_CHAT)
                .await
                .expect("count");
            assert!(matches!(outcome, CountOutcome::Unsupported));
            assert!(host.http().calls().is_empty());
        });
    }

    #[test]
    fn the_chat_probe_caps_with_max_completion_tokens() {
        block_on(async {
            let root = tempfile::tempdir().expect("scratch");
            let host = keyed(root.path());
            host.http().enqueue(b"{}".to_vec());
            let provider = provider(&host, &options("mine"), ProtocolId::OPENAI_CHAT);
            let query = ProbeQuery::new(ProtocolId::OPENAI_CHAT, "team-gpt").expect("query");
            let _ = provider.probe(&query).await;
            let calls = host.http().calls();
            assert_eq!(calls.len(), 1);
            let body: hofund_plugin_sdk::json::Value =
                hofund_plugin_sdk::json::from_slice(calls[0].body()).expect("json");
            assert_eq!(body["max_completion_tokens"], 16);
            assert!(body.get("max_tokens").is_none());
        });
    }

    #[test]
    fn deployments_are_model_ids_listed_with_the_key_alone() {
        block_on(async {
            let root = tempfile::tempdir().expect("scratch");
            let host = keyed(root.path());
            host.http().enqueue(
                br#"{"object":"list","data":[
                    {"id":"team-gpt","model":"gpt-5","object":"deployment"},
                    {"id":"text-embedding-3-small","model":"text-embedding-3-small"},
                    {"id":"gpt-5-codex","model":"gpt-5-codex","context_length":4.0e5,"max_output_tokens":0.5}
                ]}"#
                .to_vec(),
            );
            let provider = provider(&host, &options("mine"), ProtocolId::OPENAI_RESPONSES);
            let listed = provider.list_models(&ListQuery::new()).await.expect("list");
            let ids: Vec<&str> = listed.models().iter().map(|model| model.id()).collect();
            assert_eq!(ids, ["team-gpt", "gpt-5-codex"]);
            assert_eq!(listed.models()[1].context(), 400_000);
            assert_eq!(listed.models()[1].max_output(), 0);
            assert!(!listed.base_model_fallback());
            assert_eq!(
                listed.source_url(),
                Some("https://mine.openai.azure.com/openai/deployments?<redacted>")
            );
            let calls = host.http().calls();
            assert_eq!(calls.len(), 1);
            assert_eq!(calls[0].method(), "GET");
            assert_eq!(calls[0].uri(), DEPLOYMENTS);
            assert_eq!(calls[0].sensitive_headers(), ["api-key"]);
        });
    }

    #[test]
    fn a_deployment_list_of_only_embeddings_falls_back_to_base_models() {
        block_on(async {
            let root = tempfile::tempdir().expect("scratch");
            let host = keyed(root.path());
            host.http().enqueue(
                br#"{"data":[{"id":"text-embedding-3-small","model":"text-embedding-3-small"}]}"#
                    .to_vec(),
            );
            host.http()
                .enqueue(br#"{"data":[{"id":"gpt-5"}]}"#.to_vec());
            let provider = provider(&host, &options("mine"), ProtocolId::OPENAI_CHAT);
            let listed = provider.list_models(&ListQuery::new()).await.expect("list");
            let ids: Vec<&str> = listed.models().iter().map(|model| model.id()).collect();
            assert_eq!(ids, ["gpt-5"]);
            assert!(listed.base_model_fallback());
            assert_eq!(host.http().calls().len(), 2);
        });
    }

    #[test]
    fn only_a_cancelled_list_stops_before_the_next_candidate() {
        assert!(super::stops(&super::PluginError::cancelled("gone")));
        assert!(!super::stops(&super::PluginError::status_error(
            403, "denied"
        )));
        assert!(!super::stops(&super::PluginError::decode("bad")));
    }

    #[test]
    fn a_refused_deployment_list_falls_back_to_base_models() {
        block_on(async {
            let root = tempfile::tempdir().expect("scratch");
            let host = keyed(root.path());
            host.http()
                .enqueue_response(403, &[], br#"{"error":{"message":"denied"}}"#.to_vec());
            host.http()
                .enqueue(br#"{"data":[{"id":"gpt-5","display_name":"GPT-5"}]}"#.to_vec());
            let provider = provider(&host, &options("mine"), ProtocolId::OPENAI_CHAT);
            let listed = provider.list_models(&ListQuery::new()).await.expect("list");
            assert_eq!(listed.models()[0].id(), "gpt-5");
            assert_eq!(listed.models()[0].display_name(), Some("GPT-5"));
            assert!(listed.base_model_fallback());
            assert_eq!(listed.source_url(), Some(MODELS));
            let calls = host.http().calls();
            assert_eq!(calls.len(), 2);
            assert_eq!(calls[1].uri(), MODELS);
        });
    }

    #[test]
    fn both_lists_failing_name_the_manual_path_and_keep_the_last_status() {
        block_on(async {
            let root = tempfile::tempdir().expect("scratch");
            let host = keyed(root.path());
            host.http().enqueue_response(
                401,
                &[],
                br#"{"error":{"message":"bad key secret-header-value"}}"#.to_vec(),
            );
            host.http().enqueue_response(403, &[], b"nope".to_vec());
            let mut headers = RawTable::new();
            headers.insert(
                "Ocp-Apim-Trace",
                RawValue::String("secret-header-value".to_owned()),
            );
            let mut raw = options("mine");
            raw.insert("headers", RawValue::Table(headers));
            let provider = provider(&host, &raw, ProtocolId::OPENAI_CHAT);
            let error = provider
                .list_models(&ListQuery::new())
                .await
                .expect_err("both fail");
            let text = error.to_string();
            assert_eq!(error.kind(), PluginErrorKind::Status);
            assert_eq!(error.status(), Some(403));
            assert!(text.contains("HTTP 401 (bad key [redacted])"), "{text}");
            assert!(text.contains("HTTP 403"), "{text}");
            assert!(!text.contains("secret-header-value"), "{text}");
            assert!(
                text.ends_with(" — type your deployments' names in as model ids"),
                "{text}"
            );
            let calls = host.http().calls();
            assert_eq!(calls.len(), 2);
            assert_eq!(calls[0].sensitive_headers(), ["api-key", "ocp-apim-trace"]);
        });
    }

    #[test]
    fn a_user_api_key_header_replaces_the_stored_key() {
        block_on(async {
            let root = tempfile::tempdir().expect("scratch");
            let host = keyed(root.path());
            host.http()
                .enqueue(br#"{"data":[{"id":"team-gpt"}]}"#.to_vec());
            let mut headers = RawTable::new();
            headers.insert("API-Key", RawValue::String("override".to_owned()));
            let mut raw = options("mine");
            raw.insert("headers", RawValue::Table(headers));
            let provider = provider(&host, &raw, ProtocolId::OPENAI_CHAT);
            provider.list_models(&ListQuery::new()).await.expect("list");
            assert_eq!(host.http().calls()[0].sensitive_headers(), ["api-key"]);
        });
    }

    #[test]
    fn a_connect_failure_still_tries_the_base_model_list() {
        block_on(async {
            let root = tempfile::tempdir().expect("scratch");
            let host = keyed(root.path());
            host.http().enqueue_connect("refused");
            host.http()
                .enqueue(br#"{"data":[{"id":"gpt-5"}]}"#.to_vec());
            let provider = provider(&host, &options("mine"), ProtocolId::OPENAI_CHAT);
            let listed = provider.list_models(&ListQuery::new()).await.expect("list");
            assert!(listed.base_model_fallback());
        });
    }

    #[test]
    fn joined_errors_keep_the_last_kind_and_retry_hint() {
        use hofund_plugin_api::PluginError;
        use std::time::Duration;
        let joined = super::join_errors(
            vec![
                PluginError::decode("a: bad"),
                PluginError::status_error(429, "b: slow").with_retry_after(Duration::from_secs(7)),
            ],
            super::MANUAL_ENTRY,
        );
        assert_eq!(joined.kind(), PluginErrorKind::Status);
        assert_eq!(joined.status(), Some(429));
        assert_eq!(joined.retry_after(), Some(Duration::from_secs(7)));
        assert_eq!(
            joined.message(),
            "a: bad; b: slow — type your deployments' names in as model ids"
        );
        let cancelled = super::join_errors(vec![PluginError::cancelled("stop")], "");
        assert_eq!(cancelled.kind(), PluginErrorKind::Cancelled);
        assert_eq!(cancelled.message(), "stop");
        assert!(
            super::join_errors(Vec::new(), super::MANUAL_ENTRY)
                .message()
                .ends_with("model ids")
        );
    }

    #[test]
    fn messages_lists_live_with_x_api_key_and_no_manual_suffix() {
        block_on(async {
            let root = tempfile::tempdir().expect("scratch");
            let host = keyed(root.path());
            host.http().enqueue_response(404, &[], b"".to_vec());
            host.http()
                .enqueue(br#"{"data":[{"id":"claude-sonnet"}]}"#.to_vec());
            let live = provider(&host, &options("mine"), ProtocolId::ANTHROPIC_MESSAGES);
            let listed = live.list_models(&ListQuery::new()).await.expect("list");
            assert_eq!(listed.models()[0].id(), "claude-sonnet");
            assert_eq!(listed.kind(), hofund_plugin_api::ListKind::Live);
            assert!(!listed.base_model_fallback());
            assert!(listed.v1_fallback());
            let calls = host.http().calls();
            assert_eq!(
                calls.iter().map(|call| call.uri()).collect::<Vec<_>>(),
                [MODELS, "https://mine.openai.azure.com/openai/v1/v1/models"]
            );
            assert_eq!(calls[0].sensitive_headers(), ["x-api-key"]);

            let host = keyed(root.path());
            for _ in 0..3 {
                host.http().enqueue_response(401, &[], b"".to_vec());
            }
            let refused = provider(&host, &options("mine"), ProtocolId::ANTHROPIC_MESSAGES);
            let error = refused
                .list_models(&ListQuery::new())
                .await
                .expect_err("all refused");
            assert!(!error.message().contains("deployments"), "{error}");
            assert_eq!(
                host.http().calls().last().map(|call| call.uri().to_owned()),
                Some("https://mine.openai.azure.com/openai/models".to_owned())
            );
        });
    }
}
