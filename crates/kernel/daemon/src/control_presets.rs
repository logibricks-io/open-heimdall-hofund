//! Preset catalog for the control plane.

use hofund_control_proto::{PresetEntry, PresetKind, PresetListResponse};

use crate::registry::DaemonRegistry;

pub(crate) fn list_presets(registry: &dyn DaemonRegistry) -> PresetListResponse {
    let mut response = PresetListResponse::default();
    for preset in registry.compiled_presets() {
        let mut entry = PresetEntry::default();
        entry.id = preset.id.to_owned();
        entry.name = preset.name.to_owned();
        entry.kind = map_kind(preset.kind);
        entry.needs_key = !preset.no_key;
        entry.needs_address = !preset.endpoint.is_empty();
        entry.needs_project = preset.config.iter().any(|(key, _)| *key == "project");
        entry.option_keys = preset
            .config
            .iter()
            .map(|(key, _)| (*key).to_owned())
            .collect();
        response.presets.push(entry);
    }
    response
        .presets
        .sort_by(|left, right| left.id.cmp(&right.id));
    response
}

fn map_kind(kind: &str) -> PresetKind {
    match kind {
        "local" => PresetKind::Local,
        "remote" => PresetKind::Remote,
        "custom" => PresetKind::Custom,
        _ => PresetKind::Preset,
    }
}
