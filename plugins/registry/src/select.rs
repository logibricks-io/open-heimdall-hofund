//! Pure endpoint and model-policy selection for compiled presets.

use hofund_plugin_api::ProtocolId;
use hofund_provider::{ModelPlan, Wire};

use crate::{CompiledPreset, CompiledRegion};

/// A compiled preset after region and protocol selection.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Selection {
    /// Selected factory protocol.
    pub protocol: ProtocolId,
    /// Selected catalog wire.
    pub wire: Wire,
    /// Caller override or region-resolved base URL.
    pub base_url: String,
    /// Selected region, if one was named directly or by an alias.
    pub region: Option<&'static CompiledRegion>,
}

/// A compiled preset could not select an endpoint.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct SelectError {
    message: String,
}

impl SelectError {
    fn new(message: impl Into<String>) -> Self {
        Self {
            message: message.into(),
        }
    }
}

impl std::fmt::Display for SelectError {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter.write_str(&self.message)
    }
}

impl std::error::Error for SelectError {}

/// Maps a factory protocol to its catalog wire.
#[must_use]
pub fn wire_for_protocol(protocol: ProtocolId) -> Option<Wire> {
    if protocol == ProtocolId::OPENAI_CHAT {
        Some(Wire::Chat)
    } else if protocol == ProtocolId::OPENAI_RESPONSES {
        Some(Wire::Responses)
    } else if protocol == ProtocolId::ANTHROPIC_MESSAGES {
        Some(Wire::Messages)
    } else if protocol == ProtocolId::GENERATIVE_LANGUAGE {
        Some(Wire::Generative)
    } else if protocol == ProtocolId::BEDROCK_CONVERSE {
        Some(Wire::Converse)
    } else {
        None
    }
}

/// Resolves a region id, preserving declared order in diagnostics.
///
/// # Errors
///
/// Returns an error when the preset has no regions or does not contain `id`.
pub fn resolve_region(
    preset: &'static CompiledPreset,
    id: Option<&str>,
) -> Result<Option<&'static CompiledRegion>, SelectError> {
    let Some(id) = id else {
        return Ok(None);
    };
    if preset.regions.is_empty() {
        return Err(SelectError::new(format!(
            "preset `{}` has no regions",
            preset.id
        )));
    }
    preset
        .regions
        .iter()
        .find(|region| region.id == id)
        .map(Some)
        .ok_or_else(|| {
            let ids = preset
                .regions
                .iter()
                .map(|region| region.id)
                .collect::<Vec<_>>();
            SelectError::new(format!(
                "preset `{}` has no region `{id}`; expected {}",
                preset.id,
                alternatives(&ids)
            ))
        })
}

/// Selects one endpoint using the same wire and region policy as the catalog.
///
/// # Errors
///
/// Returns an error for an unknown region, an unserved or unavailable protocol,
/// a missing required base URL, or a preset without a callable endpoint.
pub fn select(
    preset: &'static CompiledPreset,
    region: Option<&str>,
    served: &[ProtocolId],
    selected: Option<ProtocolId>,
    base_url: &str,
) -> Result<Selection, SelectError> {
    let region = resolve_region(preset, region)?;
    let endpoints = Endpoints::of(preset, region);
    if !endpoints.any() && !preset.decide.is_empty() {
        return Err(SelectError::new(format!(
            "preset `{}` only classifies a route. It has no chat, responses, or messages endpoint.",
            preset.id
        )));
    }
    if let Some(protocol) = selected {
        if !served.contains(&protocol) {
            return Err(SelectError::new(format!(
                "preset `{}` is served by plugin `{}`, which does not speak `{protocol}`; expected {}",
                preset.id,
                preset.plugin,
                alternatives(
                    &served
                        .iter()
                        .map(|protocol| protocol.as_str())
                        .collect::<Vec<_>>()
                )
            )));
        }
        let wire = wire_for_protocol(protocol).ok_or_else(|| {
            SelectError::new(format!(
                "preset `{}` cannot use unknown protocol `{protocol}`",
                preset.id
            ))
        })?;
        if endpoints.any() {
            let base = endpoints
                .url(wire)
                .filter(|url| !url.is_empty())
                .ok_or_else(|| {
                    SelectError::new(format!(
                        "preset `{}` has no {} endpoint",
                        preset.id,
                        wire_name(wire)
                    ))
                })?;
            return Ok(finish(protocol, wire, override_or(base, base_url), region));
        }
        if base_url.is_empty() {
            return Err(SelectError::new(format!(
                "preset `{}` needs base_url",
                preset.id
            )));
        }
        return Ok(finish(protocol, wire, base_url.to_owned(), region));
    }
    for wire in [
        Wire::Generative,
        Wire::Chat,
        Wire::Responses,
        Wire::Messages,
    ] {
        let Some(protocol) = protocol_for(wire) else {
            continue;
        };
        if let Some(base) = endpoints.url(wire).filter(|url| !url.is_empty())
            && served.contains(&protocol)
        {
            return Ok(finish(protocol, wire, override_or(base, base_url), region));
        }
    }
    if endpoints.any() {
        return Err(SelectError::new(format!(
            "preset `{}` has no endpoint served by plugin `{}`",
            preset.id, preset.plugin
        )));
    }
    let protocol = served.first().copied().ok_or_else(|| {
        SelectError::new(format!("plugin `{}` declares no protocol", preset.plugin))
    })?;
    let wire = wire_for_protocol(protocol).ok_or_else(|| {
        SelectError::new(format!(
            "plugin `{}` speaks an unknown protocol `{protocol}`",
            preset.plugin
        ))
    })?;
    if base_url.is_empty() {
        return Err(SelectError::new(format!(
            "preset `{}` needs base_url",
            preset.id
        )));
    }
    Ok(finish(protocol, wire, base_url.to_owned(), region))
}

/// Builds the model policy and per-wire bases for a selected region.
#[must_use]
pub fn model_plan_of(
    preset: &'static CompiledPreset,
    region: Option<&'static CompiledRegion>,
) -> ModelPlan {
    let endpoints = Endpoints::of(preset, region);
    let models = region
        .and_then(|region| region.models)
        .unwrap_or(preset.models)
        .iter()
        .map(|model| (*model).to_owned())
        .collect();
    ModelPlan {
        chat: endpoints.chat.to_owned(),
        responses: endpoints.responses.to_owned(),
        anthropic: endpoints.anthropic.to_owned(),
        generative: endpoints.generative.to_owned(),
        no_list: preset.no_list,
        lists: region.is_some_and(|region| region.lists),
        only: preset.only.to_owned(),
        models,
        quirks: None,
        extra_body: None,
    }
}

fn finish(
    protocol: ProtocolId,
    wire: Wire,
    base_url: String,
    region: Option<&'static CompiledRegion>,
) -> Selection {
    Selection {
        protocol,
        wire,
        base_url,
        region,
    }
}

/// Maps a catalog wire to its factory protocol.
#[must_use]
pub fn protocol_for(wire: Wire) -> Option<ProtocolId> {
    match wire {
        Wire::Chat => Some(ProtocolId::OPENAI_CHAT),
        Wire::Responses => Some(ProtocolId::OPENAI_RESPONSES),
        Wire::Messages => Some(ProtocolId::ANTHROPIC_MESSAGES),
        Wire::Generative => Some(ProtocolId::GENERATIVE_LANGUAGE),
        Wire::Converse => Some(ProtocolId::BEDROCK_CONVERSE),
        Wire::Vertex => None,
    }
}

fn override_or(catalog: &str, override_url: &str) -> String {
    if override_url.is_empty() {
        catalog.to_owned()
    } else {
        override_url.to_owned()
    }
}

fn alternatives(values: &[&str]) -> String {
    let Some((last, rest)) = values.split_last() else {
        return String::new();
    };
    if rest.is_empty() {
        return (*last).to_owned();
    }
    format!("{}, or {last}", rest.join(", "))
}

fn wire_name(wire: Wire) -> &'static str {
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
    anthropic: &'a str,
    generative: &'a str,
}

impl<'a> Endpoints<'a> {
    fn of(preset: &'a CompiledPreset, region: Option<&'a CompiledRegion>) -> Self {
        let Some(region) = region else {
            return Self {
                chat: preset.chat,
                responses: preset.responses,
                anthropic: preset.anthropic,
                generative: preset.generative,
            };
        };
        Self {
            chat: or_preset(region.chat, preset.chat),
            responses: or_preset(region.responses, preset.responses),
            anthropic: or_preset(region.anthropic, preset.anthropic),
            generative: preset.generative,
        }
    }

    fn any(&self) -> bool {
        [self.chat, self.responses, self.anthropic, self.generative]
            .into_iter()
            .any(|url| !url.is_empty())
    }

    fn url(&self, wire: Wire) -> Option<&'a str> {
        match wire {
            Wire::Chat => Some(self.chat),
            Wire::Responses => Some(self.responses),
            Wire::Messages => Some(self.anthropic),
            Wire::Generative => Some(self.generative),
            Wire::Vertex | Wire::Converse => None,
        }
    }
}

fn or_preset<'a>(region: &'a str, preset: &'a str) -> &'a str {
    if region.is_empty() { preset } else { region }
}
