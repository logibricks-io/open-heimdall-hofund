// Owned `agent.toml` input, validation, and static Rust rendering.
// This file has no `crate::` imports so `build.rs` can `include!` it.

use std::collections::{BTreeMap, BTreeSet};

use serde::Deserialize;

#[derive(Clone, Debug, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct AgentToml {
    pub id: String,
    pub display_name: String,
    pub binary: String,
    #[serde(default = "gateway")]
    pub route: String,
    #[serde(default)]
    pub visible: VisibleToml,
    #[serde(default)]
    pub requires: Vec<String>,
    #[serde(default, rename = "file")]
    pub files: Vec<FileToml>,
    #[serde(default, rename = "set")]
    pub sets: Vec<SetToml>,
    pub model: Option<KeyToml>,
    pub effort: Option<KeyToml>,
}

#[derive(Clone, Debug, Default, Deserialize)]
#[serde(untagged)]
pub enum VisibleToml {
    #[default]
    All,
    Name(String),
    Only(Vec<String>),
}

#[derive(Clone, Debug, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct FileToml {
    pub id: String,
    pub kind: String,
    pub linux: String,
    pub macos: String,
    pub windows: String,
    pub env_override: Option<String>,
    pub env_replaces: Option<String>,
    #[serde(default = "create_by_default")]
    pub create: bool,
}

#[derive(Clone, Debug, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct SetToml {
    pub file: String,
    pub key: String,
    pub slot: SlotToml,
}

#[derive(Clone, Debug, Deserialize)]
#[serde(untagged)]
pub enum SlotToml {
    Name(String),
    Literal(LiteralSlot),
    ModelIfUnset(ModelIfUnsetSlot),
    ModelTier(ModelTierSlot),
}

#[derive(Clone, Debug, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct LiteralSlot {
    pub literal: String,
}

#[derive(Clone, Debug, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ModelIfUnsetSlot {
    pub model_if_unset: ModelIfUnset,
}

#[derive(Clone, Debug, Default, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ModelIfUnset {
    pub from: Option<String>,
}

#[derive(Clone, Debug, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ModelTierSlot {
    pub model_tier: ModelTier,
}

#[derive(Clone, Debug, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ModelTier {
    pub tier: String,
    pub when_model_matches: Option<String>,
}

#[derive(Clone, Debug, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct KeyToml {
    pub file: String,
    pub key: String,
}

fn gateway() -> String {
    "gateway".to_owned()
}

fn create_by_default() -> bool {
    true
}

pub fn parse(
    text: &str,
    source: &str,
    directory: &str,
    plugin_ids: &[String],
    agent_ids: &[String],
    inbound_ids: &[String],
) -> Result<AgentToml, String> {
    let agent: AgentToml = toml::from_str(text).map_err(|error| format!("{source}: {error}"))?;
    validate(
        &agent,
        source,
        directory,
        plugin_ids,
        agent_ids,
        inbound_ids,
    )?;
    Ok(agent)
}

fn validate(
    agent: &AgentToml,
    source: &str,
    directory: &str,
    plugin_ids: &[String],
    agent_ids: &[String],
    inbound_ids: &[String],
) -> Result<(), String> {
    if !valid_id(&agent.id) {
        return Err(error(source, "id", "must be kebab-case ASCII"));
    }
    if agent.id != directory {
        return Err(error(
            source,
            "id",
            format!("must equal directory name `{directory}`"),
        ));
    }
    if plugin_ids.contains(&agent.id) {
        return Err(error(
            source,
            "id",
            "collides with a plugins.toml plugin id",
        ));
    }
    if agent_ids.contains(&agent.id) {
        return Err(error(source, "id", "collides with another agent id"));
    }
    if super::RESERVED_AGENT_IDS.contains(&agent.id.as_str()) {
        return Err(error(source, "id", "collides with a built-in agent id"));
    }
    if agent.display_name.is_empty() {
        return Err(error(source, "display_name", "must not be empty"));
    }
    if agent.binary.is_empty() || agent.binary.contains(['/', '\\']) {
        return Err(error(source, "binary", "must be one bare program name"));
    }
    if !matches!(agent.route.as_str(), "gateway" | "none") {
        return Err(error(source, "route", "must be `gateway` or `none`"));
    }
    if let VisibleToml::Name(name) = &agent.visible
        && name != "all"
    {
        return Err(error(
            source,
            "visible",
            "must be `all` or an array of model ids",
        ));
    }
    if let VisibleToml::Only(models) = &agent.visible {
        let mut unique = BTreeSet::new();
        for model in models {
            if model.is_empty() {
                return Err(error(source, "visible", "entries must not be empty"));
            }
            if !unique.insert(model) {
                return Err(error(
                    source,
                    "visible",
                    format!("contains duplicate entry `{model}`"),
                ));
            }
        }
    }
    for required in &agent.requires {
        if !inbound_ids.contains(required) {
            return Err(error(
                source,
                "requires",
                format!("references unknown inbound plugin id `{required}`"),
            ));
        }
    }

    let mut file_ids = BTreeSet::new();
    let mut file_kinds = BTreeMap::new();
    for (index, file) in agent.files.iter().enumerate() {
        let field = format!("file[{index}]");
        if file.id.is_empty() {
            return Err(error(source, format!("{field}.id"), "must not be empty"));
        }
        file_ids.insert(file.id.as_str());
        if !matches!(file.kind.as_str(), "toml" | "json" | "env") {
            return Err(error(
                source,
                format!("{field}.kind"),
                format!("unknown file kind `{}`", file.kind),
            ));
        }
        if let Some(previous) = file_kinds.insert(file.id.as_str(), file.kind.as_str())
            && previous != file.kind
        {
            return Err(error(
                source,
                format!("{field}.kind"),
                "candidates with the same file id must have the same kind",
            ));
        }
        for (name, path) in [
            ("linux", &file.linux),
            ("macos", &file.macos),
            ("windows", &file.windows),
        ] {
            if !valid_path(path) {
                return Err(error(
                    source,
                    format!("{field}.{name}"),
                    "invalid path template",
                ));
            }
        }
        if let Some(name) = &file.env_override
            && !valid_env_name(name)
        {
            return Err(error(
                source,
                format!("{field}.env_override"),
                "invalid environment variable name",
            ));
        }
        if let Some(prefix) = &file.env_replaces
            && (!valid_path(prefix)
                || ![&file.linux, &file.macos, &file.windows]
                    .into_iter()
                    .all(|path| segment_prefix(path, prefix)))
        {
            return Err(error(
                source,
                format!("{field}.env_replaces"),
                "must be a segment-boundary prefix of every OS path",
            ));
        }
    }

    let mut set_keys = BTreeSet::new();
    for (index, set) in agent.sets.iter().enumerate() {
        let field = format!("set[{index}]");
        if set.key.is_empty() {
            return Err(error(source, format!("{field}.key"), "must not be empty"));
        }
        let Some(file) = agent.files.iter().find(|file| file.id == set.file) else {
            return Err(error(
                source,
                format!("{field}.file"),
                format!("references unknown file id `{}`", set.file),
            ));
        };
        if !set_keys.insert((set.file.as_str(), set.key.as_str())) {
            return Err(error(
                source,
                format!("{field}.key"),
                "duplicates another set for the same file and key",
            ));
        }
        if matches!(&set.slot, SlotToml::Name(name) if name == "catalog_models")
            && file.kind != "json"
        {
            return Err(error(
                source,
                format!("{field}.slot"),
                "catalog_models requires a JSON file",
            ));
        }
        validate_slot(&set.slot, source, &field)?;
        if agent.route == "none" && gateway_slot(&set.slot) {
            return Err(error(
                source,
                format!("{field}.slot"),
                "route `none` does not allow gateway slots",
            ));
        }
    }
    for (field, key) in [("model", &agent.model), ("effort", &agent.effort)] {
        if let Some(key) = key {
            if key.key.is_empty() {
                return Err(error(source, format!("{field}.key"), "must not be empty"));
            }
            if !file_ids.contains(key.file.as_str()) {
                return Err(error(
                    source,
                    format!("{field}.file"),
                    format!("references unknown file id `{}`", key.file),
                ));
            }
        }
    }
    Ok(())
}

fn validate_slot(slot: &SlotToml, source: &str, field: &str) -> Result<(), String> {
    match slot {
        SlotToml::Name(name)
            if !matches!(name.as_str(), "origin" | "v1" | "token" | "catalog_models") =>
        {
            Err(error(source, format!("{field}.slot"), "unknown slot"))
        }
        SlotToml::ModelIfUnset(slot) if slot.model_if_unset.from.as_deref() == Some("") => {
            Err(error(
                source,
                format!("{field}.slot.model_if_unset.from"),
                "must not be empty",
            ))
        }
        SlotToml::ModelTier(ModelTierSlot { model_tier }) => {
            if model_tier.tier.is_empty()
                || !model_tier
                    .tier
                    .bytes()
                    .all(|byte| byte.is_ascii_lowercase() || byte.is_ascii_digit())
            {
                return Err(error(
                    source,
                    format!("{field}.slot.model_tier.tier"),
                    "invalid tier id; expected non-empty [a-z0-9]",
                ));
            }
            if let Some(needle) = &model_tier.when_model_matches {
                if needle.is_empty() {
                    return Err(error(
                        source,
                        format!("{field}.slot.model_tier.when_model_matches"),
                        "must not be empty",
                    ));
                }
                if needle != &needle.to_lowercase() {
                    return Err(error(
                        source,
                        format!("{field}.slot.model_tier.when_model_matches"),
                        "must be lowercase",
                    ));
                }
            }
            Ok(())
        }
        _ => Ok(()),
    }
}

fn gateway_slot(slot: &SlotToml) -> bool {
    matches!(
        slot,
        SlotToml::Name(name) if matches!(name.as_str(), "origin" | "v1" | "token" | "catalog_models")
    )
}

fn valid_id(id: &str) -> bool {
    let bytes = id.as_bytes();
    if bytes.is_empty() || bytes.first() == Some(&b'-') || bytes.last() == Some(&b'-') {
        return false;
    }
    let mut previous_hyphen = false;
    for byte in bytes {
        if *byte == b'-' {
            if previous_hyphen {
                return false;
            }
            previous_hyphen = true;
        } else if byte.is_ascii_lowercase() || byte.is_ascii_digit() {
            previous_hyphen = false;
        } else {
            return false;
        }
    }
    true
}

fn valid_path(path: &str) -> bool {
    if path.contains('\\') {
        return false;
    }
    let mut segments = path.split('/');
    if segments.next() != Some("~") {
        return false;
    }
    segments.all(|segment| {
        !segment.is_empty()
            && segment != ".."
            && !(segment.len() == 2
                && segment.as_bytes()[0].is_ascii_alphabetic()
                && segment.as_bytes()[1] == b':')
    })
}

fn valid_env_name(name: &str) -> bool {
    let mut bytes = name.bytes();
    bytes
        .next()
        .is_some_and(|byte| byte == b'_' || byte.is_ascii_uppercase())
        && bytes.all(|byte| byte == b'_' || byte.is_ascii_uppercase() || byte.is_ascii_digit())
}

fn segment_prefix(path: &str, prefix: &str) -> bool {
    path == prefix
        || path
            .strip_prefix(prefix)
            .is_some_and(|remainder| remainder.starts_with('/'))
}

fn error(source: &str, field: impl AsRef<str>, message: impl AsRef<str>) -> String {
    format!("{source}: {}: {}", field.as_ref(), message.as_ref())
}

pub fn render(agent: &AgentToml) -> (String, String) {
    let identifier = agent.id.replace('-', "_").to_uppercase();
    let static_name = format!("AGENT_{identifier}");
    let files_name = format!("{static_name}_FILES");
    let sets_name = format!("{static_name}_SETS");
    let requires_name = format!("{static_name}_REQUIRES");
    let mut output = format!("static {files_name}: &[hofund_plugin_api::AgentFile] = &[\n");
    for file in &agent.files {
        let kind = match file.kind.as_str() {
            "toml" => "Toml",
            "json" => "Json",
            "env" => "Env",
            _ => unreachable!("validated file kind"),
        };
        output.push_str(&format!(
            "    hofund_plugin_api::AgentFile::from_static({:?}, hofund_plugin_api::FileKind::{kind}, {:?}, {:?}, {:?}, {:?})",
            file.id,
            file.linux,
            file.macos,
            file.windows,
            file.env_override,
        ));
        if let Some(prefix) = &file.env_replaces {
            output.push_str(&format!(".with_env_replaces({prefix:?})"));
        }
        if !file.create {
            output.push_str(".with_create(false)");
        }
        output.push_str(",\n");
    }
    output.push_str("];\n");
    output.push_str(&format!(
        "static {sets_name}: &[hofund_plugin_api::AgentSet] = &[\n"
    ));
    for set in &agent.sets {
        output.push_str(&format!(
            "    hofund_plugin_api::AgentSet::from_static({:?}, {:?}, {}),\n",
            set.file,
            set.key,
            render_slot(&set.slot),
        ));
    }
    output.push_str("];\n");
    output.push_str(&format!(
        "static {requires_name}: &[hofund_plugin_api::SurfaceId] = &[\n"
    ));
    for required in &agent.requires {
        output.push_str(&format!(
            "    hofund_plugin_api::SurfaceId::from_static({required:?}),\n"
        ));
    }
    output.push_str("];\n");
    let visible = match &agent.visible {
        VisibleToml::All | VisibleToml::Name(_) => "hofund_plugin_api::Visible::All".to_owned(),
        VisibleToml::Only(models) => format!("hofund_plugin_api::Visible::Only(&{models:?})"),
    };
    let route = if agent.route == "none" {
        "None"
    } else {
        "Gateway"
    };
    output.push_str(&format!(
        "static {static_name}: hofund_plugin_api::AgentDescription = hofund_plugin_api::AgentDescription::from_static(hofund_plugin_api::PluginId::from_static({:?}), {:?}, {:?}, {files_name}, {sets_name}, {:?}, {:?}, {visible}, hofund_plugin_api::AgentRoute::{route}, {requires_name})",
        agent.id,
        agent.display_name,
        agent.binary,
        agent.model.as_ref().map(|model| model.key.as_str()),
        agent.effort.as_ref().map(|effort| effort.key.as_str()),
    ));
    if let Some(model) = &agent.model {
        output.push_str(&format!(".with_model_file({:?})", model.file));
    }
    if let Some(effort) = &agent.effort {
        output.push_str(&format!(".with_effort_file({:?})", effort.file));
    }
    output.push_str(";\n");
    (static_name, output)
}

fn render_slot(slot: &SlotToml) -> String {
    match slot {
        SlotToml::Name(name) => format!(
            "hofund_plugin_api::Slot::{}",
            match name.as_str() {
                "origin" => "Origin",
                "v1" => "V1",
                "token" => "Token",
                "catalog_models" => "CatalogModels",
                _ => unreachable!("validated slot"),
            }
        ),
        SlotToml::Literal(slot) => format!("hofund_plugin_api::Slot::Literal({:?})", slot.literal),
        SlotToml::ModelIfUnset(slot) => format!(
            "hofund_plugin_api::Slot::model_if_unset({:?})",
            slot.model_if_unset.from
        ),
        SlotToml::ModelTier(slot) => {
            let needle = slot.model_tier.when_model_matches.as_ref().map_or_else(
                || "None".to_owned(),
                |needle| format!("Some(hofund_plugin_api::MatchNeedle::new({needle:?}))"),
            );
            format!(
                "hofund_plugin_api::Slot::ModelTier {{ tier: hofund_plugin_api::TierId::from_static({:?}), when_model_matches: {needle} }}",
                slot.model_tier.tier
            )
        }
    }
}

#[cfg(test)]
mod tests {
    use super::{AgentToml, parse, render};

    fn base() -> String {
        r#"
id = "fixture-agent"
display_name = "Fixture Agent"
binary = "fixture"
route = "gateway"
requires = ["echo"]
[[file]]
id = "config"
kind = "json"
linux = "~/.fixture/config.json"
macos = "~/.fixture/config.json"
windows = "~/.fixture/config.json"
env_override = "FIXTURE_HOME"
env_replaces = "~/.fixture"
create = false
[[set]]
file = "config"
key = "literal"
slot = { literal = "value" }
[model]
file = "config"
key = "model"
[effort]
file = "config"
key = "effort"
"#
        .to_owned()
    }

    fn validate(text: &str) -> Result<AgentToml, String> {
        parse(
            text,
            "fixture/agent.toml",
            "fixture-agent",
            &["plugin-id".to_owned()],
            &["other-agent".to_owned()],
            &["echo".to_owned()],
        )
    }

    fn reject(text: String, needle: &str) {
        let error = validate(&text).expect_err(needle);
        assert!(error.contains("fixture/agent.toml"), "{error}");
        assert!(error.contains(needle), "{needle} missing in: {error}");
    }

    #[test]
    fn valid_defaults_and_no_creator_are_accepted() {
        let agent = validate(&base()).expect("valid agent");
        assert_eq!(agent.route, "gateway");
        assert!(!agent.files[0].create);
        let source = render(&agent).1;
        assert!(source.contains(".with_model_file(\"config\")"));
        assert!(source.contains(".with_effort_file(\"config\")"));
        assert!(source.contains(".with_create(false)"));
    }

    #[test]
    fn validation_rejects_every_invalid_field() {
        let cases = [
            ("Bad", "id", "kebab-case"),
            ("plugin-id", "id", "plugins.toml"),
            ("other-agent", "id", "another agent"),
            ("wrong-directory", "id", "directory name"),
        ];
        for (id, field, needle) in cases {
            let text = base().replacen("fixture-agent", id, 1);
            let directory = if id == "wrong-directory" {
                "fixture-agent"
            } else {
                id
            };
            let error = parse(
                &text,
                "fixture/agent.toml",
                directory,
                &["plugin-id".to_owned()],
                &["other-agent".to_owned()],
                &["echo".to_owned()],
            )
            .expect_err(field);
            assert!(error.contains(field), "{error}");
            assert!(error.contains(needle), "{error}");
        }

        reject(base().replace("Fixture Agent", ""), "display_name");
        reject(
            base().replace("binary = \"fixture\"", "binary = \"bin/fixture\""),
            "binary",
        );
        reject(base().replace("kind = \"json\"", "kind = \"yaml\""), "kind");
        reject(
            base().replace("~/.fixture/config.json", "/tmp/config.json"),
            "linux",
        );
        reject(
            base().replace("~/.fixture/config.json", "~/../config.json"),
            "linux",
        );
        reject(
            base().replace("~/.fixture/config.json", "~/.fixture\\config.json"),
            "linux",
        );
        reject(
            base().replace("~/.fixture/config.json", "~/.fixture//config.json"),
            "linux",
        );
        reject(
            base().replace("FIXTURE_HOME", "fixture-home"),
            "env_override",
        );
        reject(
            base().replace(
                "env_replaces = \"~/.fixture\"",
                "env_replaces = \"~/.other\"",
            ),
            "env_replaces",
        );
        reject(
            base().replace("requires = [\"echo\"]", "requires = [\"missing\"]"),
            "requires",
        );
        reject(
            base().replace(
                "file = \"config\"\nkey = \"literal\"",
                "file = \"missing\"\nkey = \"literal\"",
            ),
            "set[0].file",
        );
        reject(
            base().replace("key = \"literal\"", "key = \"\""),
            "set[0].key",
        );
        reject(
            base()
                .replace(
                    "slot = { literal = \"value\" }",
                    "slot = \"catalog_models\"",
                )
                .replace("kind = \"json\"", "kind = \"toml\""),
            "JSON file",
        );
        reject(
            base().replace(
                "slot = { literal = \"value\" }",
                "slot = { model_tier = { tier = \"Not-A-Tier\" } }",
            ),
            "tier",
        );
        reject(
            base().replace(
                "slot = { literal = \"value\" }",
                "slot = { model_tier = { tier = \"fable\", when_model_matches = \"\" } }",
            ),
            "must not be empty",
        );
        reject(
            base().replace(
                "slot = { literal = \"value\" }",
                "slot = { model_tier = { tier = \"fable\", when_model_matches = \"Fable-\" } }",
            ),
            "lowercase",
        );
        reject(
            base().replace("[model]\nfile = \"config\"", "[model]\nfile = \"missing\""),
            "model.file",
        );
        reject(
            base().replace(
                "[effort]\nfile = \"config\"",
                "[effort]\nfile = \"missing\"",
            ),
            "effort.file",
        );
        reject(
            base().replace("route = \"gateway\"", "route = \"other\""),
            "route",
        );
        reject(format!("{}\nunknown = true\n", base()), "unknown");
    }

    #[test]
    fn duplicate_set_keys_and_gateway_slots_without_a_route_are_rejected() {
        let duplicate = base().replace(
            "[model]",
            "[[set]]\nfile = \"config\"\nkey = \"literal\"\nslot = \"token\"\n[model]",
        );
        reject(duplicate, "duplicates");

        for slot in ["origin", "v1", "token", "catalog_models"] {
            let text = base()
                .replace(
                    "slot = { literal = \"value\" }",
                    &format!("slot = \"{slot}\""),
                )
                .replace("route = \"gateway\"", "route = \"none\"");
            reject(text, "route `none`");
        }
    }

    #[test]
    fn visible_and_slot_shapes_are_validated() {
        reject(
            base().replace("requires = [\"echo\"]", "visible = \"some\""),
            "visible",
        );
        reject(
            base().replace("slot = { literal = \"value\" }", "slot = \"unknown\""),
            "unknown slot",
        );
        reject(
            base().replace("slot = { literal = \"value\" }", "slot = { literal = 1 }"),
            "SlotToml",
        );
        reject(
            base().replace(
                "slot = { literal = \"value\" }",
                "slot = { model_if_unset = { from = 1 } }",
            ),
            "SlotToml",
        );

        let only = base().replace("requires = [\"echo\"]", "visible = [\"one\", \"two\"]");
        let agent = validate(&only).expect("visible list");
        assert!(render(&agent).1.contains("Visible::Only"));
    }

    #[test]
    fn validation_rejects_mixed_candidate_kinds() {
        let candidate = "[[file]]\nid = \"config\"\nkind = \"toml\"\nlinux = \"~/.fixture/config.toml\"\nmacos = \"~/.fixture/config.toml\"\nwindows = \"~/.fixture/config.toml\"\n[model]";
        reject(base().replace("[model]", candidate), "same file id");
    }

    #[test]
    fn validation_rejects_empty_model_source() {
        reject(
            base().replace(
                "slot = { literal = \"value\" }",
                "slot = { model_if_unset = { from = \"\" } }",
            ),
            "model_if_unset.from",
        );
    }

    #[test]
    fn validation_rejects_empty_or_duplicate_visible_entries() {
        for visible in ["visible = [\"one\", \"\"]", "visible = [\"one\", \"one\"]"] {
            reject(base().replace("requires = [\"echo\"]", visible), "visible");
        }
    }

    #[test]
    fn validation_rejects_builtin_agent_ids() {
        for id in super::super::RESERVED_AGENT_IDS {
            let error = parse(
                &base().replacen("fixture-agent", id, 1),
                "fixture/agent.toml",
                id,
                &[],
                &[],
                &["echo".to_owned()],
            )
            .expect_err("built-in id");
            assert!(error.contains("built-in agent id"), "{error}");
        }
    }
}
