//! Preset catalog for the control plane.

use hofund_control_proto::{PresetEntry, PresetKind, PresetListResponse};

use crate::registry::CompiledPreset;
use crate::registry::DaemonRegistry;

const VERTEX_OPTIONS: &[&str] = &["project", "location", "credentials", "impersonate"];

pub(crate) fn needs_address(preset: &CompiledPreset) -> bool {
    let endpoint_declared = !preset.endpoint.is_empty();
    let local_or_custom = matches!(preset.kind, "local" | "custom");
    (endpoint_declared || local_or_custom) && !preset.options_base
}

pub(crate) fn list_presets(registry: &dyn DaemonRegistry) -> PresetListResponse {
    let mut response = PresetListResponse::default();
    for preset in registry.compiled_presets() {
        let mut entry = PresetEntry::default();
        entry.id = preset.id.to_owned();
        entry.name = preset.name.to_owned();
        entry.kind = map_kind(preset.kind);
        entry.needs_key = !preset.no_key;
        entry.needs_address = needs_address(preset);
        entry.needs_project = preset.id == "google-vertex";
        entry.option_keys = option_keys_for(preset)
            .into_iter()
            .map(str::to_owned)
            .collect();
        response.presets.push(entry);
    }
    response
        .presets
        .sort_by(|left, right| left.id.cmp(&right.id));
    response
}

pub(crate) fn option_keys_for(preset: &CompiledPreset) -> Vec<&'static str> {
    if preset.id == "google-vertex" {
        return VERTEX_OPTIONS.to_vec();
    }
    preset.config.iter().map(|(key, _)| *key).collect()
}

fn map_kind(kind: &str) -> PresetKind {
    match kind {
        "local" => PresetKind::Local,
        "remote" => PresetKind::Remote,
        "custom" => PresetKind::Custom,
        _ => PresetKind::Preset,
    }
}

#[cfg(test)]
mod tests {
    use hofund_plugin_registry::compiled_presets;

    use super::needs_address;

    #[test]
    fn r7a_needs_address_vertex_false_local_lmstudio_true() {
        let vertex = compiled_presets()
            .iter()
            .find(|preset| preset.id == "google-vertex")
            .expect("google-vertex preset");
        assert!(!needs_address(vertex));
        let lmstudio = compiled_presets()
            .iter()
            .find(|preset| preset.id == "lmstudio")
            .expect("lmstudio preset");
        assert!(needs_address(lmstudio));
    }
}
