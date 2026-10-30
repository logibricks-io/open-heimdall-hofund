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
    pub model_prefix: Option<String>,
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
    pub create: Option<bool>,
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
    Json(JsonSlot),
    ModelArray(ModelArraySlot),
}

#[derive(Clone, Debug, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct JsonSlot {
    pub json: String,
}

#[derive(Clone, Debug, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ModelArraySlot {
    pub model_array: ModelArray,
}

#[derive(Clone, Debug, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ModelArray {
    pub fields: Vec<ModelFieldToml>,
}

#[derive(Clone, Debug, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ModelFieldToml {
    pub key: String,
    pub value: Option<String>,
    pub literal: Option<String>,
    pub json: Option<String>,
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
    if matches!(agent.id.as_str(), "api" | "sdk" | "host" | "registry") {
        return Err(error(source, "id", "is a reserved plugin id"));
    }
    if plugin_ids.contains(&agent.id) || agent_ids.contains(&agent.id) {
        return Err(error(source, "id", "collides with another agent id"));
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
    if let Some(prefix) = &agent.model_prefix
        && (prefix.is_empty() || !prefix.ends_with('/'))
    {
        return Err(error(
            source,
            "model_prefix",
            "must be non-empty and end with `/`",
        ));
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
        if file.create.is_some()
            && agent
                .files
                .iter()
                .filter(|candidate| candidate.id == file.id)
                .count()
                == 1
        {
            return Err(error(
                source,
                format!("{field}.create"),
                "is only allowed with multiple locations for the same file id",
            ));
        }
        if !matches!(file.kind.as_str(), "toml" | "json" | "env" | "yaml") {
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
        if matches!(&set.slot, SlotToml::Json(_) | SlotToml::ModelArray(_))
            && !matches!(file.kind.as_str(), "json" | "yaml")
        {
            return Err(error(
                source,
                format!("{field}.slot"),
                "json and model_array require a JSON or YAML file",
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
        SlotToml::Json(slot) => valid_json(&slot.json, source, &format!("{field}.slot.json")),
        SlotToml::ModelArray(ModelArraySlot { model_array }) => {
            validate_model_array(model_array, source, field)
        }
        _ => Ok(()),
    }
}

fn valid_json(text: &str, source: &str, field: &str) -> Result<(), String> {
    serde_json::from_str::<serde_json::Value>(text)
        .map(|_| ())
        .map_err(|_| error(source, field, "must be valid JSON"))
}

fn validate_model_array(array: &ModelArray, source: &str, field: &str) -> Result<(), String> {
    let field = format!("{field}.slot.model_array.fields");
    if array.fields.is_empty() {
        return Err(error(source, field, "must not be empty"));
    }
    let mut keys = BTreeSet::new();
    for (index, item) in array.fields.iter().enumerate() {
        let item_field = format!("{field}[{index}]");
        if item.key.is_empty() {
            return Err(error(source, format!("{item_field}.key"), "must not be empty"));
        }
        if !keys.insert(item.key.as_str()) {
            return Err(error(
                source,
                format!("{item_field}.key"),
                "duplicates another field",
            ));
        }
        match (&item.value, &item.literal, &item.json) {
            (Some(value), None, None) if value == "id" => {}
            (Some(_), None, None) => {
                return Err(error(
                    source,
                    format!("{item_field}.value"),
                    "unknown value; expected `id`",
                ));
            }
            (None, Some(_), None) => {}
            (None, None, Some(text)) => valid_json(text, source, &format!("{item_field}.json"))?,
            _ => {
                return Err(error(
                    source,
                    item_field,
                    "needs exactly one of value, literal, or json",
                ));
            }
        }
    }
    Ok(())
}

fn gateway_slot(slot: &SlotToml) -> bool {
    match slot {
        SlotToml::Name(name) => matches!(name.as_str(), "origin" | "v1" | "token" | "catalog_models"),
        SlotToml::ModelArray(_) => true,
        _ => false,
    }
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
            "yaml" => "Yaml",
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
        if file.create == Some(false) {
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
    if let Some(prefix) = &agent.model_prefix {
        output.push_str(&format!(".with_model_prefix({prefix:?})"));
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
        SlotToml::Json(slot) => format!("hofund_plugin_api::Slot::Json({:?})", slot.json),
        SlotToml::ModelArray(slot) => {
            let fields: Vec<String> = slot
                .model_array
                .fields
                .iter()
                .map(|item| match (&item.literal, &item.json) {
                    (Some(text), _) => format!(
                        "hofund_plugin_api::ModelField::literal({:?}, {text:?})",
                        item.key
                    ),
                    (None, Some(text)) => format!(
                        "hofund_plugin_api::ModelField::json({:?}, {text:?})",
                        item.key
                    ),
                    (None, None) => format!("hofund_plugin_api::ModelField::id({:?})", item.key),
                })
                .collect();
            format!(
                "hofund_plugin_api::Slot::ModelArray {{ fields: &[{}] }}",
                fields.join(", ")
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
[[file]]
id = "config"
kind = "json"
linux = "~/.fixture/config.json"
macos = "~/.fixture/config.json"
windows = "~/.fixture/config.json"
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
    fn model_prefix_is_validated_and_rendered() {
        let text = base().replace(
            "route = \"gateway\"",
            "route = \"gateway\"\nmodel_prefix = \"hofund/\"",
        );
        let agent = validate(&text).expect("model prefix");
        assert_eq!(agent.model_prefix.as_deref(), Some("hofund/"));
        assert!(render(&agent).1.contains(".with_model_prefix(\"hofund/\")"));

        for prefix in ["", "hofund"] {
            reject(
                base().replace(
                    "route = \"gateway\"",
                    &format!("route = \"gateway\"\nmodel_prefix = \"{prefix}\""),
                ),
                "model_prefix",
            );
        }
    }

    #[test]
    fn valid_defaults_and_no_creator_are_accepted() {
        let agent = validate(&base()).expect("valid agent");
        assert_eq!(agent.route, "gateway");
        assert_eq!(agent.files[0].create, Some(false));
        assert_eq!(agent.files[1].create, None);
        let source = render(&agent).1;
        assert!(source.contains(".with_model_file(\"config\")"));
        assert!(source.contains(".with_effort_file(\"config\")"));
        assert!(source.contains(".with_create(false)"));
    }

    #[test]
    fn yaml_files_are_accepted_and_rendered() {
        let agent = validate(
            &base()
                .replace("kind = \"json\"", "kind = \"yaml\"")
                .replace("config.json", "config.yaml"),
        )
        .expect("valid yaml agent");
        assert!(agent.files.iter().all(|file| file.kind == "yaml"));
        assert!(render(&agent).1.contains("FileKind::Yaml"));
    }

    fn with_slot(slot: &str) -> String {
        base().replace("slot = { literal = \"value\" }", &format!("slot = {slot}"))
    }

    #[test]
    fn json_and_model_array_slots_are_validated_and_rendered() {
        let array = "{ model_array = { fields = [ { key = \"id\", value = \"id\" }, { key = \"name\", literal = \"Hofund\" }, { key = \"reasoning\", json = \"true\" } ] } }";
        let source = render(&validate(&with_slot(array)).expect("model array")).1;
        assert!(
            source.contains(
                "hofund_plugin_api::Slot::ModelArray { fields: &[hofund_plugin_api::ModelField::id(\"id\"), hofund_plugin_api::ModelField::literal(\"name\", \"Hofund\"), hofund_plugin_api::ModelField::json(\"reasoning\", \"true\")] }"
            ),
            "{source}"
        );
        let json = with_slot("{ json = \"{\\\"a\\\": [1]}\" }");
        let source = render(&validate(&json).expect("json slot")).1;
        assert!(
            source.contains("hofund_plugin_api::Slot::Json(\"{\\\"a\\\": [1]}\")"),
            "{source}"
        );
        let yaml = with_slot("{ json = \"false\" }")
            .replace("kind = \"json\"", "kind = \"yaml\"")
            .replace("config.json", "config.yaml");
        validate(&yaml).expect("json slot in yaml");
        let toml = with_slot("{ json = \"false\" }")
            .replace("kind = \"json\"", "kind = \"toml\"")
            .replace("config.json", "config.toml");
        reject(toml.clone(), "require a JSON or YAML file");
        reject(
            toml.replace("{ json = \"false\" }", array),
            "require a JSON or YAML file",
        );

        let cases = [
            ("{ json = \"nope\" }", "set[0].slot.json: must be valid JSON"),
            (
                "{ model_array = { fields = [] } }",
                "set[0].slot.model_array.fields: must not be empty",
            ),
            (
                "{ model_array = { fields = [ { key = \"\", value = \"id\" } ] } }",
                "fields[0].key: must not be empty",
            ),
            (
                "{ model_array = { fields = [ { key = \"a\", value = \"id\" }, { key = \"a\", literal = \"x\" } ] } }",
                "fields[1].key: duplicates another field",
            ),
            (
                "{ model_array = { fields = [ { key = \"a\", value = \"name\" } ] } }",
                "fields[0].value: unknown value",
            ),
            (
                "{ model_array = { fields = [ { key = \"a\", json = \"{\" } ] } }",
                "fields[0].json: must be valid JSON",
            ),
            (
                "{ model_array = { fields = [ { key = \"a\" } ] } }",
                "fields[0]: needs exactly one",
            ),
            (
                "{ model_array = { fields = [ { key = \"a\", value = \"id\", literal = \"x\" } ] } }",
                "fields[0]: needs exactly one",
            ),
            (
                "{ model_array = { fields = [ { key = \"a\", literal = \"x\", json = \"1\" } ] } }",
                "fields[0]: needs exactly one",
            ),
        ];
        for (slot, needle) in cases {
            reject(with_slot(slot), needle);
        }

        let none = |slot: &str| with_slot(slot).replace("route = \"gateway\"", "route = \"none\"");
        reject(none(array), "route `none`");
        validate(&none("{ json = \"false\" }")).expect("json is not a gateway slot");
    }

    #[test]
    fn create_is_rejected_for_a_single_location() {
        let candidate = "create = false\n[[file]]\nid = \"config\"\nkind = \"json\"\nlinux = \"~/.fixture/config.json\"\nmacos = \"~/.fixture/config.json\"\nwindows = \"~/.fixture/config.json\"\n";
        let single = base().replace(candidate, "create = false\n");
        for value in ["true", "false"] {
            reject(
                single.replace("create = false", &format!("create = {value}")),
                "file[0].create",
            );
        }
    }

    #[test]
    fn validation_rejects_every_invalid_field() {
        let cases = [
            ("Bad", "id", "kebab-case"),
            ("plugin-id", "id", "another agent"),
            ("other-agent", "id", "another agent"),
            ("wrong-directory", "id", "directory name"),
            ("api", "id", "reserved"),
            ("sdk", "id", "reserved"),
            ("host", "id", "reserved"),
            ("registry", "id", "reserved"),
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
        reject(base().replace("kind = \"json\"", "kind = \"ini\""), "kind");
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
    fn validation_rejects_shipped_codex_fixture_collision() {
        let error = parse(
            &base().replacen("fixture-agent", "codex", 1),
            "fixtures/agents/codex/agent.toml",
            "codex",
            &[],
            &["codex".to_owned()],
            &["echo".to_owned()],
        )
        .expect_err("shipped codex id");
        assert!(error.contains("collides with another agent id"), "{error}");
    }

    #[test]
    fn validation_rejects_shipped_claude_code_fixture_collision() {
        let error = parse(
            &base().replacen("fixture-agent", "claude-code", 1),
            "fixtures/agents/claude-code/agent.toml",
            "claude-code",
            &[],
            &["claude-code".to_owned()],
            &["echo".to_owned()],
        )
        .expect_err("shipped claude-code id");
        assert!(error.contains("collides with another agent id"), "{error}");
    }

    #[test]
    fn validation_rejects_shipped_gemini_cli_fixture_collision() {
        let error = parse(
            &base().replacen("fixture-agent", "gemini-cli", 1),
            "fixtures/agents/gemini-cli/agent.toml",
            "gemini-cli",
            &[],
            &["gemini-cli".to_owned()],
            &["echo".to_owned()],
        )
        .expect_err("shipped gemini-cli id");
        assert!(error.contains("collides with another agent id"), "{error}");
    }

    #[test]
    fn validation_rejects_shipped_opencode_fixture_collision() {
        let error = parse(
            &base().replacen("fixture-agent", "opencode", 1),
            "fixtures/agents/opencode/agent.toml",
            "opencode",
            &[],
            &["opencode".to_owned()],
            &["echo".to_owned()],
        )
        .expect_err("shipped opencode id");
        assert!(error.contains("collides with another agent id"), "{error}");
    }
}
