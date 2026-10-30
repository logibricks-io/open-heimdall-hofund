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
    Append(AppendSlot),
    ModelMap(ModelMapSlot),
    Template(TemplateSlot),
    ModelPair(ModelPairSlot),
}

#[derive(Clone, Debug, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct TemplateSlot {
    pub template: String,
}

#[derive(Clone, Debug, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ModelPairSlot {
    pub model_pair: ModelPairToml,
}

#[derive(Clone, Debug, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ModelPairToml {
    pub value: String,
    #[serde(default)]
    pub unless: Vec<String>,
}

#[derive(Clone, Debug, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ModelMapSlot {
    pub model_map: ModelMapToml,
}

#[derive(Clone, Debug, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ModelMapToml {
    pub prefix: String,
    pub fields: Vec<ModelFieldToml>,
}

#[derive(Clone, Debug, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct AppendSlot {
    pub append: AppendToml,
}

#[derive(Clone, Debug, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct AppendToml {
    pub owner: Vec<OwnerToml>,
    pub each: String,
    pub fields: Vec<ModelFieldToml>,
}

#[derive(Clone, Debug, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct OwnerToml {
    pub key: String,
    pub equals: Option<String>,
    pub prefix: Option<String>,
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
    pub prefixed: Option<String>,
    pub models: Option<Vec<ModelFieldToml>>,
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
        if matches!(
            &set.slot,
            SlotToml::Json(_) | SlotToml::ModelArray(_) | SlotToml::Append(_)
        ) && !matches!(file.kind.as_str(), "json" | "yaml")
        {
            return Err(error(
                source,
                format!("{field}.slot"),
                "json, model_array, and append require a JSON or YAML file",
            ));
        }
        if matches!(&set.slot, SlotToml::ModelMap(_))
            && !matches!(file.kind.as_str(), "json" | "yaml" | "toml")
        {
            return Err(error(
                source,
                format!("{field}.slot"),
                "model_map requires a JSON, YAML, or TOML file",
            ));
        }
        validate_slot(&set.slot, source, &field)?;
        if matches!(&set.slot, SlotToml::ModelPair(_))
            && agent.model.as_ref().map(|model| model.file.as_str()) != Some(set.file.as_str())
        {
            return Err(error(
                source,
                format!("{field}.slot"),
                "model_pair requires the model key in the same file",
            ));
        }
        if matches!(&set.slot, SlotToml::ModelPair(_))
            && agent.model.as_ref().map(|model| model.key.as_str()) == Some(set.key.as_str())
        {
            return Err(error(
                source,
                format!("{field}.key"),
                "model_pair must sit beside the model key, not on it",
            ));
        }
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
        SlotToml::ModelArray(ModelArraySlot { model_array }) => validate_fields(
            &model_array.fields,
            true,
            source,
            &format!("{field}.slot.model_array.fields"),
        ),
        SlotToml::Append(AppendSlot { append }) => validate_append(append, source, field),
        SlotToml::ModelMap(ModelMapSlot { model_map }) => validate_fields(
            &model_map.fields,
            true,
            source,
            &format!("{field}.slot.model_map.fields"),
        ),
        SlotToml::Template(TemplateSlot { template }) => {
            validate_template(template, source, &format!("{field}.slot.template"))
        }
        SlotToml::ModelPair(ModelPairSlot { model_pair }) => {
            let field = format!("{field}.slot.model_pair");
            if model_pair.value.is_empty() {
                return Err(error(source, format!("{field}.value"), "must not be empty"));
            }
            let mut keys = BTreeSet::new();
            for (index, key) in model_pair.unless.iter().enumerate() {
                if key.is_empty() {
                    return Err(error(
                        source,
                        format!("{field}.unless[{index}]"),
                        "must not be empty",
                    ));
                }
                if !keys.insert(key.as_str()) {
                    return Err(error(
                        source,
                        format!("{field}.unless[{index}]"),
                        "duplicates another key",
                    ));
                }
            }
            Ok(())
        }
        _ => Ok(()),
    }
}

/// Checks a template: every `{` opens `{origin}`, `{v1}`, or `{token}`, and
/// at least one placeholder is present, or a literal slot would do.
fn validate_template(text: &str, source: &str, field: &str) -> Result<(), String> {
    let mut rest = text;
    let mut placeholders = 0;
    loop {
        let open = rest.find('{');
        let close = rest.find('}');
        match (open, close) {
            (None, None) => break,
            (Some(open), close) if close.is_none_or(|close| open < close) => {
                let tail = &rest[open..];
                let Some(name) = ["{origin}", "{v1}", "{token}"]
                    .into_iter()
                    .find(|name| tail.starts_with(name))
                else {
                    return Err(error(
                        source,
                        field,
                        "unknown placeholder; expected `{origin}`, `{v1}`, or `{token}`",
                    ));
                };
                placeholders += 1;
                rest = &tail[name.len()..];
            }
            _ => {
                return Err(error(source, field, "has a `}` that closes no placeholder"));
            }
        }
    }
    if placeholders == 0 {
        return Err(error(
            source,
            field,
            "needs a placeholder; use a literal slot for fixed text",
        ));
    }
    Ok(())
}

fn valid_json(text: &str, source: &str, field: &str) -> Result<(), String> {
    serde_json::from_str::<serde_json::Value>(text)
        .map(|_| ())
        .map_err(|_| error(source, field, "must be valid JSON"))
}

/// Checks the fields of one written object. `per_model` is whether a model id
/// is known for the object, so `id` and `prefixed` are allowed.
fn validate_fields(
    fields: &[ModelFieldToml],
    per_model: bool,
    source: &str,
    field: &str,
) -> Result<(), String> {
    if fields.is_empty() {
        return Err(error(source, field, "must not be empty"));
    }
    let mut keys = BTreeSet::new();
    for (index, item) in fields.iter().enumerate() {
        let item_field = format!("{field}[{index}]");
        if item.key.is_empty() {
            return Err(error(
                source,
                format!("{item_field}.key"),
                "must not be empty",
            ));
        }
        if !keys.insert(item.key.as_str()) {
            return Err(error(
                source,
                format!("{item_field}.key"),
                "duplicates another field",
            ));
        }
        match (
            &item.value,
            &item.literal,
            &item.json,
            &item.prefixed,
            &item.models,
        ) {
            (Some(value), None, None, None, None) => match value.as_str() {
                "id" if per_model => {}
                "v1" | "origin" | "token" => {}
                "id" => {
                    return Err(error(
                        source,
                        format!("{item_field}.value"),
                        "`id` needs one object per model",
                    ));
                }
                _ => {
                    return Err(error(
                        source,
                        format!("{item_field}.value"),
                        "unknown value; expected `id`, `v1`, `origin`, or `token`",
                    ));
                }
            },
            (None, Some(_), None, None, None) => {}
            (None, None, Some(text), None, None) => {
                valid_json(text, source, &format!("{item_field}.json"))?;
            }
            (None, None, None, Some(_), None) if per_model => {}
            (None, None, None, Some(_), None) => {
                return Err(error(
                    source,
                    format!("{item_field}.prefixed"),
                    "needs one object per model",
                ));
            }
            (None, None, None, None, Some(nested)) => {
                validate_fields(nested, true, source, &format!("{item_field}.models"))?;
            }
            _ => {
                return Err(error(
                    source,
                    item_field,
                    "needs exactly one of value, literal, json, prefixed, or models",
                ));
            }
        }
    }
    Ok(())
}

/// Checks an append slot. Every owner test must hold for each object hofund
/// writes, or a second connect would add the objects again.
fn validate_append(append: &AppendToml, source: &str, field: &str) -> Result<(), String> {
    let field = format!("{field}.slot.append");
    let per_model = match append.each.as_str() {
        "model" => true,
        "once" => false,
        _ => {
            return Err(error(
                source,
                format!("{field}.each"),
                "unknown value; expected `model` or `once`",
            ));
        }
    };
    validate_fields(
        &append.fields,
        per_model,
        source,
        &format!("{field}.fields"),
    )?;
    if append.owner.is_empty() {
        return Err(error(source, format!("{field}.owner"), "must not be empty"));
    }
    for (index, test) in append.owner.iter().enumerate() {
        let test_field = format!("{field}.owner[{index}]");
        let (text, prefix) = match (&test.equals, &test.prefix) {
            (Some(text), None) => (text, false),
            (None, Some(text)) => (text, true),
            _ => {
                return Err(error(
                    source,
                    test_field,
                    "needs exactly one of equals or prefix",
                ));
            }
        };
        if test.key.is_empty() || text.is_empty() {
            return Err(error(source, test_field, "key and text must not be empty"));
        }
        let written = append.fields.iter().find(|item| item.key == test.key);
        let holds = written.is_some_and(|item| match (&item.literal, &item.prefixed) {
            (Some(literal), _) if prefix => literal.starts_with(text.as_str()),
            (Some(literal), _) => literal == text,
            (None, Some(start)) => prefix && start.starts_with(text.as_str()),
            (None, None) => false,
        });
        if !holds {
            return Err(error(
                source,
                test_field,
                "must hold for every object written; test a literal or prefixed field",
            ));
        }
    }
    Ok(())
}

fn gateway_slot(slot: &SlotToml) -> bool {
    match slot {
        SlotToml::Name(name) => {
            matches!(name.as_str(), "origin" | "v1" | "token" | "catalog_models")
        }
        SlotToml::ModelArray(_)
        | SlotToml::Append(_)
        | SlotToml::ModelMap(_)
        | SlotToml::Template(_)
        | SlotToml::ModelPair(_) => true,
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
        SlotToml::ModelArray(slot) => format!(
            "hofund_plugin_api::Slot::ModelArray {{ fields: &[{}] }}",
            render_fields(&slot.model_array.fields)
        ),
        SlotToml::Append(slot) => {
            let owner: Vec<String> = slot
                .append
                .owner
                .iter()
                .map(|test| match (&test.equals, &test.prefix) {
                    (Some(text), _) => format!(
                        "hofund_plugin_api::OwnerTest::equals({:?}, {text:?})",
                        test.key
                    ),
                    (None, text) => format!(
                        "hofund_plugin_api::OwnerTest::prefix({:?}, {:?})",
                        test.key,
                        text.as_deref().unwrap_or_default()
                    ),
                })
                .collect();
            format!(
                "hofund_plugin_api::Slot::Append {{ owner: &[{}], each_model: {}, fields: &[{}] }}",
                owner.join(", "),
                slot.append.each == "model",
                render_fields(&slot.append.fields)
            )
        }
        SlotToml::ModelMap(slot) => format!(
            "hofund_plugin_api::Slot::ModelMap {{ prefix: {:?}, fields: &[{}] }}",
            slot.model_map.prefix,
            render_fields(&slot.model_map.fields)
        ),
        SlotToml::Template(slot) => {
            format!("hofund_plugin_api::Slot::Template({:?})", slot.template)
        }
        SlotToml::ModelPair(slot) => format!(
            "hofund_plugin_api::Slot::ModelPair {{ value: {:?}, unless: &{:?} }}",
            slot.model_pair.value, slot.model_pair.unless
        ),
    }
}

/// The `ModelField` constructors for `fields`, comma separated.
fn render_fields(fields: &[ModelFieldToml]) -> String {
    let rendered: Vec<String> = fields
        .iter()
        .map(|item| {
            let key = &item.key;
            match (&item.value, &item.literal, &item.json, &item.prefixed, &item.models) {
                (_, Some(text), ..) => {
                    format!("hofund_plugin_api::ModelField::literal({key:?}, {text:?})")
                }
                (_, None, Some(text), ..) => {
                    format!("hofund_plugin_api::ModelField::json({key:?}, {text:?})")
                }
                (_, None, None, Some(prefix), _) => {
                    format!("hofund_plugin_api::ModelField::prefixed({key:?}, {prefix:?})")
                }
                (_, None, None, None, Some(nested)) => format!(
                    "hofund_plugin_api::ModelField::models({key:?}, {{ const FIELDS: &[hofund_plugin_api::ModelField] = &[{}]; FIELDS }})",
                    render_fields(nested)
                ),
                (value, None, None, None, None) => format!(
                    "hofund_plugin_api::ModelField::{}({key:?})",
                    match value.as_deref() {
                        Some("v1") => "v1",
                        Some("origin") => "origin",
                        Some("token") => "token",
                        _ => "id",
                    }
                ),
            }
        })
        .collect();
    rendered.join(", ")
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
            (
                "{ json = \"nope\" }",
                "set[0].slot.json: must be valid JSON",
            ),
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
    fn model_map_slots_are_validated_and_rendered() {
        let map = "{ model_map = { prefix = \"hofund/\", fields = [ { key = \"model\", value = \"id\" }, { key = \"max_context_size\", json = \"128000\" }, { key = \"api_key\", value = \"token\" } ] } }";
        let rendered = "hofund_plugin_api::Slot::ModelMap { prefix: \"hofund/\", fields: &[hofund_plugin_api::ModelField::id(\"model\"), hofund_plugin_api::ModelField::json(\"max_context_size\", \"128000\"), hofund_plugin_api::ModelField::token(\"api_key\")] }";
        for (kind, name) in [
            ("json", "config.json"),
            ("yaml", "config.yaml"),
            ("toml", "config.toml"),
        ] {
            let text = with_slot(map)
                .replace("kind = \"json\"", &format!("kind = \"{kind}\""))
                .replace("config.json", name);
            let source = render(&validate(&text).expect("model map")).1;
            assert!(source.contains(rendered), "{source}");
        }
        let whole =
            "{ model_map = { prefix = \"\", fields = [ { key = \"id\", value = \"id\" } ] } }";
        let source = render(&validate(&with_slot(whole)).expect("empty prefix")).1;
        assert!(
            source.contains("hofund_plugin_api::Slot::ModelMap { prefix: \"\", fields: &[hofund_plugin_api::ModelField::id(\"id\")] }"),
            "{source}"
        );
        reject(
            with_slot(map)
                .replace("kind = \"json\"", "kind = \"env\"")
                .replace("config.json", "config.env"),
            "model_map requires a JSON, YAML, or TOML file",
        );
        reject(
            with_slot(map).replace("route = \"gateway\"", "route = \"none\""),
            "route `none` does not allow gateway slots",
        );
        reject(
            with_slot("{ model_map = { prefix = \"h/\", fields = [] } }"),
            "set[0].slot.model_map.fields: must not be empty",
        );
        reject(
            with_slot("{ model_map = { fields = [ { key = \"a\", value = \"id\" } ] } }"),
            "did not match any variant",
        );
    }

    #[test]
    fn template_slots_are_validated_and_rendered() {
        for kind in ["json", "toml", "yaml", "env"] {
            let text = with_slot("{ template = \"Bearer {token} at {v1}/{origin}\" }")
                .replace("kind = \"json\"", &format!("kind = \"{kind}\""));
            let source = render(&validate(&text).expect("template")).1;
            assert!(
                source.contains(
                    "hofund_plugin_api::Slot::Template(\"Bearer {token} at {v1}/{origin}\")"
                ),
                "{source}"
            );
        }
        validate(&with_slot("{ template = \"{v1}\" }")).expect("one placeholder");
        reject(
            with_slot("{ template = \"{token}\" }")
                .replace("route = \"gateway\"", "route = \"none\""),
            "route `none` does not allow gateway slots",
        );
        for (text, needle) in [
            ("Bearer", "set[0].slot.template: needs a placeholder"),
            ("", "set[0].slot.template: needs a placeholder"),
            ("{key}", "set[0].slot.template: unknown placeholder"),
            ("{token", "set[0].slot.template: unknown placeholder"),
            ("{token}{", "set[0].slot.template: unknown placeholder"),
            ("{{token}}", "set[0].slot.template: unknown placeholder"),
            (
                "{token}}",
                "set[0].slot.template: has a `}` that closes no placeholder",
            ),
            (
                "} {token}",
                "set[0].slot.template: has a `}` that closes no placeholder",
            ),
        ] {
            reject(with_slot(&format!("{{ template = {text:?} }}")), needle);
        }
    }

    #[test]
    fn model_pair_slots_are_validated_and_rendered() {
        let pair = "{ model_pair = { value = \"hofund\", unless = [\"GOOSE_PROVIDER\", \"b\"] } }";
        let source = render(&validate(&with_slot(pair)).expect("pair")).1;
        assert!(
            source.contains("hofund_plugin_api::Slot::ModelPair { value: \"hofund\", unless: &[\"GOOSE_PROVIDER\", \"b\"] }"),
            "{source}"
        );
        let source =
            render(&validate(&with_slot("{ model_pair = { value = \"h\" } }")).expect("no unless"))
                .1;
        assert!(
            source.contains("hofund_plugin_api::Slot::ModelPair { value: \"h\", unless: &[] }"),
            "{source}"
        );
        reject(
            with_slot(pair).replace("route = \"gateway\"", "route = \"none\""),
            "route `none` does not allow gateway slots",
        );
        reject(
            with_slot("{ model_pair = { value = \"\" } }"),
            "set[0].slot.model_pair.value: must not be empty",
        );
        reject(
            with_slot("{ model_pair = { value = \"h\", unless = [\"a\", \"\"] } }"),
            "set[0].slot.model_pair.unless[1]: must not be empty",
        );
        reject(
            with_slot("{ model_pair = { value = \"h\", unless = [\"a\", \"a\"] } }"),
            "set[0].slot.model_pair.unless[1]: duplicates another key",
        );
        let other_file = format!(
            "{}\n[[file]]\nid = \"other\"\nkind = \"json\"\nlinux = \"~/.o.json\"\nmacos = \"~/.o.json\"\nwindows = \"~/.o.json\"\n",
            with_slot(pair)
        )
        .replacen("file = \"config\"\nkey = \"model\"", "file = \"other\"\nkey = \"model\"", 1);
        reject(
            other_file,
            "set[0].slot: model_pair requires the model key in the same file",
        );
        let on_model = with_slot(pair).replacen("key = \"literal\"", "key = \"model\"", 1);
        reject(
            on_model,
            "set[0].key: model_pair must sit beside the model key, not on it",
        );
        let no_model = with_slot(pair).replace("[model]\nfile = \"config\"\nkey = \"model\"\n", "");
        reject(
            no_model,
            "set[0].slot: model_pair requires the model key in the same file",
        );
    }

    #[test]
    fn append_slots_are_validated_and_rendered() {
        let each = "{ append = { each = \"model\", owner = [ { key = \"id\", prefix = \"custom:hofund/\" } ], fields = [ { key = \"model\", value = \"id\" }, { key = \"id\", prefixed = \"custom:hofund/\" }, { key = \"baseUrl\", value = \"v1\" }, { key = \"apiKey\", value = \"token\" } ] } }";
        let source = render(&validate(&with_slot(each)).expect("append each")).1;
        assert!(
            source.contains(
                "hofund_plugin_api::Slot::Append { owner: &[hofund_plugin_api::OwnerTest::prefix(\"id\", \"custom:hofund/\")], each_model: true, fields: &[hofund_plugin_api::ModelField::id(\"model\"), hofund_plugin_api::ModelField::prefixed(\"id\", \"custom:hofund/\"), hofund_plugin_api::ModelField::v1(\"baseUrl\"), hofund_plugin_api::ModelField::token(\"apiKey\")] }"
            ),
            "{source}"
        );
        let once = "{ append = { each = \"once\", owner = [ { key = \"vendor\", equals = \"custom\" }, { key = \"name\", prefix = \"hof\" } ], fields = [ { key = \"vendor\", literal = \"custom\" }, { key = \"name\", literal = \"hofund\" }, { key = \"url\", value = \"origin\" }, { key = \"models\", models = [ { key = \"id\", value = \"id\" }, { key = \"label\", prefixed = \"h/\" } ] } ] } }";
        let source = render(&validate(&with_slot(once)).expect("append once")).1;
        assert!(
            source.contains(
                "hofund_plugin_api::Slot::Append { owner: &[hofund_plugin_api::OwnerTest::equals(\"vendor\", \"custom\"), hofund_plugin_api::OwnerTest::prefix(\"name\", \"hof\")], each_model: false, fields: &[hofund_plugin_api::ModelField::literal(\"vendor\", \"custom\"), hofund_plugin_api::ModelField::literal(\"name\", \"hofund\"), hofund_plugin_api::ModelField::origin(\"url\"), hofund_plugin_api::ModelField::models(\"models\", { const FIELDS: &[hofund_plugin_api::ModelField] = &[hofund_plugin_api::ModelField::id(\"id\"), hofund_plugin_api::ModelField::prefixed(\"label\", \"h/\")]; FIELDS })] }"
            ),
            "{source}"
        );
        let toml = with_slot(each)
            .replace("kind = \"json\"", "kind = \"toml\"")
            .replace("config.json", "config.toml");
        reject(toml, "require a JSON or YAML file");
        let none = with_slot(each).replace("route = \"gateway\"", "route = \"none\"");
        reject(none, "route `none`");

        let owner = "owner = [ { key = \"name\", equals = \"hofund\" } ]";
        let fields = "fields = [ { key = \"name\", literal = \"hofund\" } ]";
        let slot = |each: &str, owner: &str, fields: &str| {
            with_slot(&format!(
                "{{ append = {{ each = \"{each}\", {owner}, {fields} }} }}"
            ))
        };
        validate(&slot("once", owner, fields)).expect("minimal once");
        let cases = [
            (
                slot("twice", owner, fields),
                "slot.append.each: unknown value",
            ),
            (
                slot("once", "owner = []", fields),
                "slot.append.owner: must not be empty",
            ),
            (
                slot("once", "owner = [ { key = \"name\" } ]", fields),
                "owner[0]: needs exactly one of equals or prefix",
            ),
            (
                slot(
                    "once",
                    "owner = [ { key = \"name\", equals = \"a\", prefix = \"a\" } ]",
                    fields,
                ),
                "owner[0]: needs exactly one of equals or prefix",
            ),
            (
                slot(
                    "once",
                    "owner = [ { key = \"\", equals = \"hofund\" } ]",
                    fields,
                ),
                "owner[0]: key and text must not be empty",
            ),
            (
                slot(
                    "once",
                    "owner = [ { key = \"name\", equals = \"\" } ]",
                    fields,
                ),
                "owner[0]: key and text must not be empty",
            ),
            (
                slot(
                    "once",
                    "owner = [ { key = \"name\", equals = \"other\" } ]",
                    fields,
                ),
                "owner[0]: must hold for every object written",
            ),
            (
                slot(
                    "once",
                    "owner = [ { key = \"name\", prefix = \"x\" } ]",
                    fields,
                ),
                "owner[0]: must hold for every object written",
            ),
            (
                slot(
                    "once",
                    "owner = [ { key = \"missing\", equals = \"hofund\" } ]",
                    fields,
                ),
                "owner[0]: must hold for every object written",
            ),
            (
                slot(
                    "once",
                    "owner = [ { key = \"name\", equals = \"hofund\" } ]",
                    "fields = [ { key = \"name\", value = \"token\" } ]",
                ),
                "owner[0]: must hold for every object written",
            ),
            (
                slot(
                    "model",
                    "owner = [ { key = \"id\", equals = \"p/\" } ]",
                    "fields = [ { key = \"id\", prefixed = \"p/\" } ]",
                ),
                "owner[0]: must hold for every object written",
            ),
            (
                slot(
                    "model",
                    "owner = [ { key = \"id\", prefix = \"q\" } ]",
                    "fields = [ { key = \"id\", prefixed = \"p/\" } ]",
                ),
                "owner[0]: must hold for every object written",
            ),
            (
                slot("once", owner, "fields = []"),
                "slot.append.fields: must not be empty",
            ),
            (
                slot(
                    "once",
                    owner,
                    "fields = [ { key = \"name\", literal = \"hofund\" }, { key = \"m\", value = \"id\" } ]",
                ),
                "fields[1].value: `id` needs one object per model",
            ),
            (
                slot(
                    "once",
                    owner,
                    "fields = [ { key = \"name\", literal = \"hofund\" }, { key = \"m\", prefixed = \"p\" } ]",
                ),
                "fields[1].prefixed: needs one object per model",
            ),
            (
                slot(
                    "once",
                    owner,
                    "fields = [ { key = \"name\", literal = \"hofund\" }, { key = \"m\", models = [] } ]",
                ),
                "fields[1].models: must not be empty",
            ),
            (
                slot(
                    "once",
                    owner,
                    "fields = [ { key = \"name\", literal = \"hofund\" }, { key = \"m\", models = [ { key = \"x\", value = \"bad\" } ] } ]",
                ),
                "fields[1].models[0].value: unknown value; expected `id`, `v1`, `origin`, or `token`",
            ),
            (
                slot(
                    "once",
                    owner,
                    "fields = [ { key = \"name\", literal = \"hofund\", prefixed = \"x\" } ]",
                ),
                "fields[0]: needs exactly one of value, literal, json, prefixed, or models",
            ),
        ];
        for (text, needle) in cases {
            reject(text, needle);
        }
        validate(&slot(
            "model",
            "owner = [ { key = \"id\", prefix = \"p\" } ]",
            "fields = [ { key = \"id\", prefixed = \"p/\" } ]",
        ))
        .expect("prefix of a prefixed field");
        validate(&slot(
            "once",
            "owner = [ { key = \"name\", prefix = \"hof\" } ]",
            fields,
        ))
        .expect("prefix of a literal");
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
