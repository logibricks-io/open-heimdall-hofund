//! Versioned provider presets.
//!
//! The catalog is data (`presets.toml`). A preset names the endpoints and the
//! header an API key uses. It does not open a socket. Adapters that need a
//! vendor credential flow, or that only classify a route, stay in the file and
//! [`resolve`] explains why they cannot be built yet.

use std::sync::OnceLock;

use serde::Deserialize;

use crate::http::Credential;

const CATALOG_TOML: &str = include_str!("../presets.toml");

/// Which protocol a resolved preset speaks.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Wire {
    /// `POST {base}/chat/completions`.
    Chat,
    /// `POST {base}/responses`.
    Responses,
    /// `POST {base}/v1/messages`.
    Messages,
    /// `POST {base}/models/{model}:generateContent`.
    Generative,
    /// Vertex AI publisher `generateContent`. The base includes the project
    /// and the location. The adapter appends `publishers/google/models/{model}`.
    Vertex,
    /// Bedrock Converse. The base is the runtime root. The adapter appends
    /// `/model/{model}/converse`.
    Converse,
}

/// What the caller asked the catalog to resolve.
pub struct Resolve<'a> {
    /// Preset id, or an id that preset used to have.
    pub id: &'a str,
    /// Region id. Absent uses the preset's own endpoints.
    pub region: Option<&'a str>,
    /// Protocol to select. Absent uses a generative URL when the preset has
    /// one, then chat, then responses, then messages.
    pub wire: Option<Wire>,
    /// Caller-supplied root. Required for a preset that has no URL of its own.
    /// When the preset already has a URL, this replaces it.
    pub base_url: &'a str,
    /// Google Cloud project id. Required for `google-vertex`. Empty otherwise.
    pub project: &'a str,
    /// Vertex AI location. Empty means `global`. Used by `google-vertex`.
    pub location: &'a str,
}

/// One preset after region and protocol selection.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Resolved {
    /// Catalog id. An alias resolves to the id it points at.
    pub preset_id: String,
    /// Display name.
    pub name: String,
    /// Protocol the base URL is for.
    pub wire: Wire,
    /// API root. The adapter appends its protocol path.
    pub base_url: String,
    /// Header the API key uses.
    pub credential: Credential,
}

/// The catalog could not build an endpoint.
#[derive(Debug)]
pub struct PresetError {
    message: String,
}

impl PresetError {
    fn new(message: impl Into<String>) -> Self {
        Self {
            message: message.into(),
        }
    }
}

impl std::fmt::Display for PresetError {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter.write_str(&self.message)
    }
}

impl std::error::Error for PresetError {}

/// Commit the catalog was recorded from.
#[must_use]
pub fn catalog_commit() -> &'static str {
    &catalog().source_commit
}

/// Resolves `request` to one endpoint.
///
/// # Errors
///
/// Returns [`PresetError`] when the id is unknown, the region or protocol is
/// not on that preset, or the preset cannot be called with the adapters this
/// build has.
pub fn resolve(request: &Resolve<'_>) -> Result<Resolved, PresetError> {
    let catalog = catalog();
    let (preset, alias_region) = find(catalog, request.id)?;
    if preset.id == "google-vertex" {
        return resolve_vertex(preset, request);
    }
    if preset.id == "remote-gateway" {
        return Err(unavailable(preset));
    }
    let region_id = request.region.or(alias_region);
    let region = match region_id {
        Some(region_id) => Some(region_of(preset, region_id)?),
        None => None,
    };
    let endpoints = Endpoints::of(preset, region);
    if request.wire == Some(Wire::Converse) {
        return resolve_converse(preset, request, &endpoints);
    }
    if let Some(wire) = request.wire {
        let base = endpoints
            .url(wire)
            .filter(|url| !url.is_empty())
            .map(str::to_owned);
        let base = match base {
            Some(base) => prefer_override(&base, request.base_url),
            None if preset.id == "azure" && !request.base_url.is_empty() => {
                azure_base(request.base_url)?
            }
            None => {
                return Err(PresetError::new(format!(
                    "preset `{}` has no {} endpoint",
                    preset.id,
                    wire_name(wire)
                )));
            }
        };
        return Ok(finish(preset, wire, base));
    }
    if let Some(base) = endpoints
        .url(Wire::Generative)
        .filter(|url| !url.is_empty())
    {
        return Ok(finish(
            preset,
            Wire::Generative,
            prefer_override(base, request.base_url),
        ));
    }
    for wire in [Wire::Chat, Wire::Responses, Wire::Messages] {
        if let Some(base) = endpoints.url(wire).filter(|url| !url.is_empty()) {
            return Ok(finish(
                preset,
                wire,
                prefer_override(base, request.base_url),
            ));
        }
    }
    if preset.id == "azure" || !preset.endpoint.is_empty() {
        if request.base_url.is_empty() {
            return Err(PresetError::new(format!(
                "preset `{}` needs base_url",
                preset.id
            )));
        }
        let base = if preset.id == "azure" {
            azure_base(request.base_url)?
        } else {
            request.base_url.to_owned()
        };
        return Ok(finish(preset, Wire::Chat, base));
    }
    Err(unavailable(preset))
}

/// Every protocol `request` can be probed on.
///
/// An explicit [`Resolve::wire`] returns that one protocol. With no wire, the
/// protocol [`resolve`] would pick comes first, then every other protocol the
/// preset has a URL for. Converse is included only when `wire` asks for it:
/// that path signs with AWS keys, not the API key.
///
/// # Errors
///
/// Returns [`PresetError`] when no protocol can be built. The message is the
/// one [`resolve`] returns for `request`.
pub fn probe_wires(request: &Resolve<'_>) -> Result<Vec<Resolved>, PresetError> {
    let primary = resolve(request)?;
    if request.wire.is_some() {
        return Ok(vec![primary]);
    }
    let mut out = vec![primary.clone()];
    for wire in [
        Wire::Chat,
        Wire::Responses,
        Wire::Messages,
        Wire::Generative,
    ] {
        if wire == primary.wire {
            continue;
        }
        let one = Resolve {
            id: request.id,
            region: request.region,
            wire: Some(wire),
            base_url: request.base_url,
            project: request.project,
            location: request.location,
        };
        if let Ok(resolved) = resolve(&one) {
            out.push(resolved);
        }
    }
    Ok(out)
}

/// Whether a provider's model list is asked live or read from the catalog.
///
/// A preset with `no_list` and no region that sets `lists` returns the
/// recorded ids. `google-vertex` returns [`crate::Vertex::models_for`].
/// Anything else is asked at the upstream. A provider with no preset is live.
///
/// # Errors
///
/// Returns [`PresetError`] when `preset_id` is not in the catalog, or the
/// region is not on that preset.
pub fn model_list_kind(
    preset_id: Option<&str>,
    region: Option<&str>,
    location: &str,
) -> Result<ModelListKind, PresetError> {
    let Some(preset_id) = preset_id.filter(|id| !id.is_empty()) else {
        return Ok(ModelListKind::Live);
    };
    let catalog = catalog();
    let (preset, alias_region) = find(catalog, preset_id)?;
    if preset.id == "google-vertex" {
        return Ok(ModelListKind::Catalog(crate::vertex::Vertex::models_for(
            location,
        )));
    }
    let region_id = region.or(alias_region);
    let region = match region_id {
        Some(region_id) => Some(region_of(preset, region_id)?),
        None => None,
    };
    if region.is_some_and(|region| region.lists) || !preset.no_list {
        return Ok(ModelListKind::Live);
    }
    let ids = region
        .map(|region| region.models.clone())
        .filter(|ids| !ids.is_empty())
        .unwrap_or_else(|| preset.models.clone());
    Ok(ModelListKind::Catalog(ids))
}

/// Where [`model_list_kind`] says the ids come from.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum ModelListKind {
    /// Ask the upstream.
    Live,
    /// Recorded ids. This preset has no list to ask.
    Catalog(Vec<String>),
}

fn finish(preset: &Preset, wire: Wire, base_url: String) -> Resolved {
    Resolved {
        credential: credential(&preset.id, wire, &base_url),
        preset_id: preset.id.clone(),
        name: preset.name.clone(),
        wire,
        base_url,
    }
}

fn prefer_override(catalog_url: &str, override_url: &str) -> String {
    if override_url.is_empty() {
        catalog_url.to_owned()
    } else {
        override_url.to_owned()
    }
}

fn resolve_vertex(preset: &Preset, request: &Resolve<'_>) -> Result<Resolved, PresetError> {
    if let Some(region) = request.region {
        return Err(PresetError::new(format!(
            "preset `google-vertex` has no region `{region}`"
        )));
    }
    if let Some(wire) = request.wire
        && wire != Wire::Vertex
    {
        return Err(PresetError::new(format!(
            "preset `google-vertex` has no {} endpoint",
            wire_name(wire)
        )));
    }
    let base = crate::vertex::base_url(request.project, request.location, request.base_url)
        .map_err(|error| PresetError::new(error.to_string()))?;
    Ok(Resolved {
        credential: Credential::Bearer,
        preset_id: preset.id.clone(),
        name: preset.name.clone(),
        wire: Wire::Vertex,
        base_url: base,
    })
}

/// Model ids recorded for one preset. `None` when the id is not in the catalog.
pub(crate) fn preset_models(id: &str) -> Option<&[String]> {
    catalog()
        .preset
        .iter()
        .find(|preset| preset.id == id)
        .map(|preset| preset.models.as_slice())
}

fn resolve_converse(
    preset: &Preset,
    request: &Resolve<'_>,
    endpoints: &Endpoints<'_>,
) -> Result<Resolved, PresetError> {
    if preset.id != "bedrock" {
        return Err(PresetError::new(format!(
            "preset `{}` has no converse endpoint",
            preset.id
        )));
    }
    let chat = endpoints.url(Wire::Chat).unwrap_or("");
    if chat.is_empty() && request.base_url.is_empty() {
        return Err(PresetError::new(
            "preset `bedrock` has no converse endpoint",
        ));
    }
    let base = if request.base_url.is_empty() {
        chat.to_owned()
    } else {
        request.base_url.to_owned()
    };
    Ok(finish(
        preset,
        Wire::Converse,
        crate::bedrock::runtime_root(&base),
    ))
}

fn unavailable(preset: &Preset) -> PresetError {
    if preset.id == "google-vertex" {
        return PresetError::new(
            "preset `google-vertex` signs with Google credentials. That adapter is not in this build.",
        );
    }
    if preset.id == "remote-gateway" {
        return PresetError::new(
            "preset `remote-gateway` forwards the client's own protocol. That adapter is not in this build.",
        );
    }
    if !preset.decide.is_empty() {
        return PresetError::new(format!(
            "preset `{}` only classifies a route. It has no chat, responses, or messages endpoint.",
            preset.id
        ));
    }
    PresetError::new(format!("preset `{}` has no endpoint", preset.id))
}

fn credential(preset_id: &str, wire: Wire, base_url: &str) -> Credential {
    if wire == Wire::Vertex || wire == Wire::Converse {
        return Credential::Bearer;
    }
    if wire == Wire::Generative {
        return Credential::GoogApiKey;
    }
    let host = host_of(base_url);
    if preset_id == "azure" || azure_host(&host) {
        return match wire {
            Wire::Messages => Credential::XApiKey,
            Wire::Chat | Wire::Responses | Wire::Vertex | Wire::Converse => Credential::ApiKey,
            Wire::Generative => Credential::GoogApiKey,
        };
    }
    if wire == Wire::Messages {
        if host.ends_with("anthropic.com") || is_bedrock(&host) {
            return Credential::XApiKey;
        }
        return Credential::MessagesBoth;
    }
    Credential::Bearer
}

/// The v1 API root of an Azure OpenAI resource.
///
/// A pasted resource, `https://<resource>.openai.azure.com`, a host of
/// `cognitiveservices.azure.com` or `services.ai.azure.com`, or the resource
/// name alone, becomes `https://<host>/openai/v1`. A localhost URL is kept, so
/// a test server is not rewritten. Checked against the v1 API
/// (<https://learn.microsoft.com/en-us/azure/foundry/openai/api-version-lifecycle>,
/// 2026-10-25): chat, responses, and the model list sit on that prefix, and
/// the key is the `api-key` header.
///
/// # Errors
///
/// Returns [`PresetError`] when the value is not an Azure OpenAI host.
pub fn azure_base(raw: &str) -> Result<String, PresetError> {
    let mut raw = raw.trim().to_owned();
    if raw.is_empty() {
        return Err(PresetError::new("preset `azure` needs base_url"));
    }
    if !raw.contains("://") {
        raw = format!("https://{raw}");
    }
    let url = reqwest::Url::parse(&raw).map_err(|error| {
        PresetError::new(format!("preset `azure` base URL is invalid: {error}"))
    })?;
    let host = url.host_str().unwrap_or("").to_ascii_lowercase();
    if host == "localhost" || host == "127.0.0.1" || host == "::1" {
        return Ok(raw.trim_end_matches('/').to_owned());
    }
    let host = if host.contains('.') {
        host
    } else {
        format!("{host}.openai.azure.com")
    };
    if !azure_host(&host) {
        return Err(PresetError::new(format!(
            "preset `azure` base URL `{raw}` is not an Azure OpenAI host"
        )));
    }
    Ok(format!("https://{host}/openai/v1"))
}

fn azure_host(host: &str) -> bool {
    const SUFFIXES: &[&str] = &[
        ".openai.azure.com",
        ".cognitiveservices.azure.com",
        ".services.ai.azure.com",
    ];
    SUFFIXES
        .iter()
        .any(|suffix| host.ends_with(suffix) && host.len() > suffix.len())
}

fn is_bedrock(host: &str) -> bool {
    (host.starts_with("bedrock-runtime.") && host.ends_with(".amazonaws.com"))
        || (host.starts_with("bedrock-mantle.") && host.ends_with(".api.aws"))
}

fn host_of(url: &str) -> String {
    reqwest::Url::parse(url)
        .ok()
        .and_then(|url| url.host_str().map(str::to_owned))
        .unwrap_or_default()
        .to_ascii_lowercase()
}

/// Short name of a protocol, used in command output.
#[must_use]
pub fn wire_name(wire: Wire) -> &'static str {
    match wire {
        Wire::Chat => "chat",
        Wire::Responses => "responses",
        Wire::Messages => "messages",
        Wire::Generative => "generative",
        Wire::Vertex => "vertex",
        Wire::Converse => "converse",
    }
}

struct Endpoints<'a> {
    chat: &'a str,
    responses: &'a str,
    messages: &'a str,
    generative: &'a str,
}

impl<'a> Endpoints<'a> {
    fn of(preset: &'a Preset, region: Option<&'a Region>) -> Self {
        let Some(region) = region else {
            return Self {
                chat: &preset.chat,
                responses: &preset.responses,
                messages: &preset.anthropic,
                generative: &preset.generative,
            };
        };
        Self {
            chat: or_preset(&region.chat, &preset.chat),
            responses: or_preset(&region.responses, &preset.responses),
            messages: or_preset(&region.anthropic, &preset.anthropic),
            generative: &preset.generative,
        }
    }

    fn url(&self, wire: Wire) -> Option<&str> {
        match wire {
            Wire::Chat => Some(self.chat),
            Wire::Responses => Some(self.responses),
            Wire::Messages => Some(self.messages),
            Wire::Generative => Some(self.generative),
            Wire::Vertex | Wire::Converse => None,
        }
    }
}

fn or_preset<'a>(region: &'a str, preset: &'a str) -> &'a str {
    if region.is_empty() { preset } else { region }
}

fn region_of<'a>(preset: &'a Preset, id: &str) -> Result<&'a Region, PresetError> {
    preset
        .region
        .iter()
        .find(|region| region.id == id)
        .ok_or_else(|| PresetError::new(format!("preset `{}` has no region `{id}`", preset.id)))
}

fn find<'a>(catalog: &'a Catalog, id: &str) -> Result<(&'a Preset, Option<&'a str>), PresetError> {
    let mut id = id;
    let mut region = None;
    if let Some(alias) = catalog.alias.iter().find(|alias| alias.id == id) {
        id = &alias.preset;
        region = alias.region.as_deref().filter(|region| !region.is_empty());
    }
    let preset = catalog
        .preset
        .iter()
        .find(|preset| preset.id == id)
        .ok_or_else(|| PresetError::new(format!("unknown preset `{id}`")))?;
    Ok((preset, region))
}

fn catalog() -> &'static Catalog {
    static CATALOG: OnceLock<Catalog> = OnceLock::new();
    CATALOG.get_or_init(|| {
        let catalog: Catalog = toml_edit::de::from_str(CATALOG_TOML)
            .unwrap_or_else(|error| panic!("preset catalog: {error}"));
        assert_eq!(catalog.version, 1, "preset catalog version");
        catalog
    })
}

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
struct Catalog {
    version: u32,
    source_commit: String,
    #[serde(default)]
    alias: Vec<Alias>,
    #[serde(default)]
    preset: Vec<Preset>,
}

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
struct Alias {
    id: String,
    preset: String,
    #[serde(default)]
    region: Option<String>,
}

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
#[allow(dead_code)]
struct Preset {
    id: String,
    name: String,
    #[serde(default)]
    short: String,
    kind: String,
    #[serde(default)]
    catalog: String,
    #[serde(default)]
    note: String,
    #[serde(default)]
    chat: String,
    #[serde(default)]
    responses: String,
    #[serde(default)]
    anthropic: String,
    #[serde(default)]
    generative: String,
    #[serde(default)]
    decide: String,
    #[serde(default)]
    website: String,
    #[serde(default)]
    keys_url: String,
    #[serde(default)]
    site_host: String,
    #[serde(default)]
    no_key: bool,
    #[serde(default)]
    key_hint: String,
    #[serde(default)]
    no_list: bool,
    #[serde(default)]
    hosts: bool,
    #[serde(default)]
    only: String,
    #[serde(default)]
    models: Vec<String>,
    #[serde(default)]
    endpoint: String,
    #[serde(default)]
    endpoint_hint: String,
    #[serde(default)]
    endpoint_needed: String,
    #[serde(default)]
    header_hints: Vec<String>,
    #[serde(default)]
    region_label: String,
    #[serde(default)]
    region: Vec<Region>,
}

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
#[allow(dead_code)]
struct Region {
    id: String,
    #[serde(default)]
    name: String,
    #[serde(default)]
    chat: String,
    #[serde(default)]
    responses: String,
    #[serde(default)]
    anthropic: String,
    #[serde(default)]
    decide: String,
    #[serde(default)]
    lists: bool,
    #[serde(default)]
    keys_url: String,
    #[serde(default)]
    website: String,
    #[serde(default)]
    site_host: String,
    #[serde(default)]
    catalog: String,
    #[serde(default)]
    models: Vec<String>,
    #[serde(default)]
    embeddings: Vec<String>,
    #[serde(default)]
    drawers: Vec<String>,
    #[serde(default)]
    videos: Vec<String>,
}

#[cfg(test)]
mod tests {
    use super::{Resolve, Wire, catalog, resolve};

    #[test]
    fn the_catalog_lists_every_preset_and_parses_its_endpoints() {
        let catalog = catalog();
        assert_eq!(catalog.preset.len(), 58);
        assert_eq!(
            super::catalog_commit(),
            "cbbf3163cb3c87ee0c327a56bc5689ddc5eda947"
        );
        let mut ids = std::collections::BTreeSet::new();
        for preset in &catalog.preset {
            assert!(ids.insert(preset.id.clone()), "repeated {}", preset.id);
            for url in [
                preset.chat.as_str(),
                preset.responses.as_str(),
                preset.anthropic.as_str(),
                preset.generative.as_str(),
                preset.decide.as_str(),
                preset.website.as_str(),
                preset.keys_url.as_str(),
                preset.endpoint.as_str(),
            ] {
                assert_url(&preset.id, url);
            }
            let mut regions = std::collections::BTreeSet::new();
            for region in &preset.region {
                assert!(
                    regions.insert(region.id.clone()),
                    "{} region {}",
                    preset.id,
                    region.id
                );
                for url in [
                    region.chat.as_str(),
                    region.responses.as_str(),
                    region.anthropic.as_str(),
                    region.decide.as_str(),
                    region.website.as_str(),
                    region.keys_url.as_str(),
                ] {
                    assert_url(&format!("{}/{}", preset.id, region.id), url);
                }
            }
        }
        for alias in &catalog.alias {
            assert!(
                ids.contains(&alias.preset),
                "alias {} -> {}",
                alias.id,
                alias.preset
            );
        }
    }

    #[test]
    fn known_presets_resolve_to_the_recorded_endpoint_and_header() {
        let openai = resolve(&Resolve {
            id: "openai",
            region: None,
            wire: None,
            base_url: "",
            project: "",
            location: "",
        })
        .expect("openai");
        assert_eq!(openai.wire, Wire::Chat);
        assert_eq!(openai.base_url, "https://api.openai.com/v1");
        assert_eq!(openai.credential, crate::Credential::Bearer);

        let responses = resolve(&Resolve {
            id: "openai",
            region: None,
            wire: Some(Wire::Responses),
            base_url: "",
            project: "",
            location: "",
        })
        .expect("responses");
        assert_eq!(responses.wire, Wire::Responses);
        assert_eq!(responses.base_url, "https://api.openai.com/v1");

        let anthropic = resolve(&Resolve {
            id: "anthropic",
            region: None,
            wire: None,
            base_url: "",
            project: "",
            location: "",
        })
        .expect("anthropic");
        assert_eq!(anthropic.wire, Wire::Messages);
        assert_eq!(anthropic.base_url, "https://api.anthropic.com");
        assert_eq!(anthropic.credential, crate::Credential::XApiKey);

        let deepseek = resolve(&Resolve {
            id: "deepseek",
            region: None,
            wire: Some(Wire::Messages),
            base_url: "",
            project: "",
            location: "",
        })
        .expect("deepseek");
        assert_eq!(deepseek.base_url, "https://api.deepseek.com/anthropic");
        assert_eq!(deepseek.credential, crate::Credential::MessagesBoth);

        let bedrock = resolve(&Resolve {
            id: "bedrock",
            region: Some("eu-west-1"),
            wire: Some(Wire::Messages),
            base_url: "",
            project: "",
            location: "",
        })
        .expect("bedrock");
        assert_eq!(
            bedrock.base_url,
            "https://bedrock-runtime.eu-west-1.amazonaws.com/anthropic"
        );
        assert_eq!(bedrock.credential, crate::Credential::XApiKey);
        let bedrock_chat = resolve(&Resolve {
            id: "bedrock",
            region: Some("eu-west-1"),
            wire: Some(Wire::Chat),
            base_url: "",
            project: "",
            location: "",
        })
        .expect("bedrock chat");
        assert_eq!(
            bedrock_chat.base_url,
            "https://bedrock-runtime.eu-west-1.amazonaws.com/openai/v1"
        );
        assert_eq!(bedrock_chat.credential, crate::Credential::Bearer);
        let converse = resolve(&Resolve {
            id: "bedrock",
            region: Some("eu-west-1"),
            wire: Some(Wire::Converse),
            base_url: "",
            project: "",
            location: "",
        })
        .expect("converse");
        assert_eq!(converse.wire, Wire::Converse);
        assert_eq!(
            converse.base_url,
            "https://bedrock-runtime.eu-west-1.amazonaws.com"
        );
        let error = resolve(&Resolve {
            id: "bedrock-mantle",
            region: None,
            wire: Some(Wire::Converse),
            base_url: "",
            project: "",
            location: "",
        })
        .expect_err("mantle converse");
        assert!(error.to_string().contains("no converse endpoint"));

        let mantle = resolve(&Resolve {
            id: "bedrock-mantle",
            region: Some("ap-southeast-2"),
            wire: Some(Wire::Responses),
            base_url: "",
            project: "",
            location: "",
        })
        .expect("mantle");
        assert_eq!(
            mantle.base_url,
            "https://bedrock-mantle.ap-southeast-2.api.aws/v1"
        );

        let azure = resolve(&Resolve {
            id: "azure",
            region: None,
            wire: None,
            base_url: "my-resource",
            project: "",
            location: "",
        })
        .expect("azure");
        assert_eq!(
            azure.base_url,
            "https://my-resource.openai.azure.com/openai/v1"
        );
        assert_eq!(azure.credential, crate::Credential::ApiKey);
        let pasted = resolve(&Resolve {
            id: "azure",
            region: None,
            wire: Some(Wire::Responses),
            base_url: "https://mine.openai.azure.com/openai/deployments/demo?api-version=2024-10-21",
            project: "",
            location: "",
        })
        .expect("pasted");
        assert_eq!(pasted.base_url, "https://mine.openai.azure.com/openai/v1");
        assert_eq!(pasted.wire, Wire::Responses);

        let alias = resolve(&Resolve {
            id: "tencent-tokenhub",
            region: None,
            wire: Some(Wire::Responses),
            base_url: "",
            project: "",
            location: "",
        })
        .expect("alias");
        assert_eq!(alias.preset_id, "tencent-cloud");
        assert_eq!(alias.base_url, "https://tokenhub-intl.tencentmaas.com/v1");

        let zhipu = resolve(&Resolve {
            id: "zhipu",
            region: Some("coding"),
            wire: Some(Wire::Responses),
            base_url: "",
            project: "",
            location: "",
        })
        .expect("zhipu");
        assert_eq!(zhipu.base_url, "https://open.bigmodel.cn/api/v1");

        let google = resolve(&Resolve {
            id: "google",
            region: None,
            wire: None,
            base_url: "",
            project: "",
            location: "",
        })
        .expect("google");
        assert_eq!(google.wire, Wire::Generative);
        assert_eq!(
            google.base_url,
            "https://generativelanguage.googleapis.com/v1beta"
        );
        assert_eq!(google.credential, crate::Credential::GoogApiKey);
        let google_chat = resolve(&Resolve {
            id: "google",
            region: None,
            wire: Some(Wire::Chat),
            base_url: "",
            project: "",
            location: "",
        })
        .expect("google chat");
        assert_eq!(google_chat.wire, Wire::Chat);
        assert_eq!(
            google_chat.base_url,
            "https://generativelanguage.googleapis.com/v1beta/openai"
        );
        assert_eq!(google_chat.credential, crate::Credential::Bearer);

        let vertex = resolve(&Resolve {
            id: "google-vertex",
            region: None,
            wire: None,
            base_url: "",
            project: "My-Project",
            location: "",
        })
        .expect("vertex");
        assert_eq!(vertex.wire, Wire::Vertex);
        assert_eq!(
            vertex.base_url,
            "https://aiplatform.googleapis.com/v1/projects/my-project/locations/global"
        );
        assert_eq!(vertex.credential, crate::Credential::Bearer);
        let regional = resolve(&Resolve {
            id: "google-vertex",
            region: None,
            wire: None,
            base_url: "",
            project: "my-project",
            location: "us-central1",
        })
        .expect("region");
        assert_eq!(
            regional.base_url,
            "https://us-central1-aiplatform.googleapis.com/v1/projects/my-project/locations/us-central1"
        );
        let error = resolve(&Resolve {
            id: "google-vertex",
            region: None,
            wire: None,
            base_url: "",
            project: "",
            location: "",
        })
        .expect_err("vertex project");
        assert!(error.to_string().contains("Google Cloud project"));
        let error = resolve(&Resolve {
            id: "typesafe",
            region: None,
            wire: None,
            base_url: "",
            project: "",
            location: "",
        })
        .expect_err("decide");
        assert!(error.to_string().contains("classifies"));
        let error = resolve(&Resolve {
            id: "nope",
            region: None,
            wire: None,
            base_url: "",
            project: "",
            location: "",
        })
        .expect_err("missing");
        assert!(error.to_string().contains("unknown preset"));
    }

    #[test]
    fn every_preset_either_probes_or_explains_why_it_cannot() {
        let mut live = 0;
        let mut blocked = 0;
        for preset in &catalog().preset {
            let mut request = Resolve {
                id: &preset.id,
                region: None,
                wire: None,
                base_url: "",
                project: "",
                location: "",
            };
            let wires = match super::probe_wires(&request) {
                Ok(wires) => wires,
                Err(error) => {
                    let message = error.to_string();
                    if message.contains("base_url") {
                        request.base_url = "http://127.0.0.1:9/v1";
                        super::probe_wires(&request).unwrap_or_else(|error| {
                            panic!("{} with a base URL: {error}", preset.id)
                        })
                    } else if message.contains("project") {
                        request.project = "my-project";
                        super::probe_wires(&request)
                            .unwrap_or_else(|error| panic!("{} with a project: {error}", preset.id))
                    } else {
                        assert!(
                            message.contains("classifies")
                                || message.contains("forwards")
                                || message.contains("no endpoint"),
                            "{}: {message}",
                            preset.id
                        );
                        blocked += 1;
                        continue;
                    }
                }
            };
            assert!(!wires.is_empty(), "{}", preset.id);
            live += 1;
        }
        assert_eq!(live, 52);
        assert_eq!(blocked, 6);
    }

    #[test]
    fn an_explicit_protocol_is_the_only_probe_and_openai_speaks_two() {
        let openai = super::probe_wires(&Resolve {
            id: "openai",
            region: None,
            wire: None,
            base_url: "",
            project: "",
            location: "",
        })
        .expect("openai");
        assert_eq!(
            openai.iter().map(|wire| wire.wire).collect::<Vec<_>>(),
            [Wire::Chat, Wire::Responses]
        );
        let chat = super::probe_wires(&Resolve {
            id: "openai",
            region: None,
            wire: Some(Wire::Chat),
            base_url: "",
            project: "",
            location: "",
        })
        .expect("chat");
        assert_eq!(chat.len(), 1);
        let converse = super::probe_wires(&Resolve {
            id: "bedrock",
            region: None,
            wire: None,
            base_url: "",
            project: "",
            location: "",
        })
        .expect("bedrock");
        assert!(converse.iter().all(|wire| wire.wire != Wire::Converse));
    }

    #[test]
    fn catalog_lists_stay_catalog_and_a_live_region_is_asked() {
        let vertex = super::model_list_kind(Some("google-vertex"), None, "global").expect("vertex");
        let super::ModelListKind::Catalog(ids) = vertex else {
            panic!("vertex is a catalog");
        };
        assert!(ids.iter().any(|id| id == "gemini-2.5-flash"));
        let coding =
            super::model_list_kind(Some("volcengine"), Some("coding"), "").expect("coding");
        let super::ModelListKind::Catalog(ids) = coding else {
            panic!("coding plan is a catalog");
        };
        assert!(ids.iter().any(|id| id == "ark-code-latest"));
        assert!(matches!(
            super::model_list_kind(Some("volcengine"), Some("api"), "").expect("api"),
            super::ModelListKind::Live
        ));
        assert!(matches!(
            super::model_list_kind(Some("openai"), None, "").expect("openai"),
            super::ModelListKind::Live
        ));
        assert!(matches!(
            super::model_list_kind(None, None, "").expect("kind"),
            super::ModelListKind::Live
        ));
    }

    fn assert_url(owner: &str, url: &str) {
        if url.is_empty() {
            return;
        }
        let usable = url
            .replace("{WorkspaceId}", "workspace")
            .replace("<resource>", "resource");
        reqwest::Url::parse(&usable).unwrap_or_else(|error| panic!("{owner} `{url}`: {error}"));
    }
}
