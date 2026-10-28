// Owned preset-fragment input, validation, and static Rust rendering.
// This file has no `crate::` imports so `build.rs` can `include!` it.

use std::collections::{BTreeMap, BTreeSet};

use serde::Deserialize;

#[derive(Clone, Debug)]
pub struct FragmentToml {
    pub plugin: String,
    pub source: String,
    pub presets: Vec<PresetToml>,
    pub aliases: Vec<AliasToml>,
}

#[derive(Clone, Debug, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct PresetToml {
    pub id: String,
    pub name: Option<String>,
    pub kind: Option<String>,
    #[serde(default)]
    pub short: String,
    #[serde(default)]
    pub note: String,
    #[serde(default)]
    pub catalog: String,
    #[serde(default)]
    pub website: String,
    #[serde(default)]
    pub keys_url: String,
    #[serde(default)]
    pub site_host: String,
    #[serde(default)]
    pub key_hint: String,
    #[serde(default)]
    pub no_key: bool,
    #[serde(default)]
    pub no_list: bool,
    #[serde(default)]
    pub hosts: bool,
    #[serde(default)]
    pub only: String,
    #[serde(default)]
    pub models: Vec<String>,
    #[serde(default)]
    pub header_hints: Vec<String>,
    #[serde(default)]
    pub endpoint: String,
    #[serde(default)]
    pub endpoint_hint: String,
    #[serde(default)]
    pub endpoint_needed: String,
    #[serde(default)]
    pub region_label: String,
    #[serde(default)]
    pub decide: String,
    #[serde(default)]
    pub chat: String,
    #[serde(default)]
    pub responses: String,
    #[serde(default)]
    pub anthropic: String,
    #[serde(default)]
    pub generative: String,
    #[serde(default)]
    pub config: BTreeMap<String, String>,
    #[serde(default, rename = "region")]
    pub regions: Vec<RegionToml>,
}

#[derive(Clone, Debug, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct RegionToml {
    pub id: String,
    #[serde(default)]
    pub name: String,
    #[serde(default)]
    pub chat: String,
    #[serde(default)]
    pub responses: String,
    #[serde(default)]
    pub anthropic: String,
    #[serde(default)]
    pub decide: String,
    #[serde(default)]
    pub lists: bool,
    #[serde(default)]
    pub keys_url: String,
    #[serde(default)]
    pub website: String,
    #[serde(default)]
    pub site_host: String,
    #[serde(default)]
    pub catalog: String,
    #[serde(default)]
    pub models: Option<Vec<String>>,
    #[serde(default)]
    pub embeddings: Vec<String>,
    #[serde(default)]
    pub drawers: Vec<String>,
    #[serde(default)]
    pub videos: Vec<String>,
}

#[derive(Clone, Debug, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct AliasToml {
    pub id: String,
    pub preset: String,
    #[serde(default)]
    pub region: Option<String>,
}

pub fn parse(text: &str, source: &str, plugin: &str) -> Result<FragmentToml, String> {
    let value: toml::Value = toml::from_str(text).map_err(|error| format!("{source}: {error}"))?;
    parse_value(&value, source, plugin)
}

pub fn parse_value(
    value: &toml::Value,
    source: &str,
    plugin: &str,
) -> Result<FragmentToml, String> {
    let table = value
        .as_table()
        .ok_or_else(|| format!("{source}: fragment must be a table"))?;
    for key in table.keys() {
        if !matches!(key.as_str(), "preset" | "alias") {
            return Err(format!("{source}: unknown fragment key `{key}`"));
        }
    }
    let mut presets = Vec::new();
    if let Some(value) = table.get("preset") {
        let rows = value
            .as_array()
            .ok_or_else(|| format!("{source}: preset must be an array of tables"))?;
        for row in rows {
            let id = row
                .get("id")
                .and_then(toml::Value::as_str)
                .unwrap_or("<missing id>");
            let preset: PresetToml = row
                .clone()
                .try_into()
                .map_err(|error| format!("{source}: preset `{id}` is invalid: {error}"))?;
            presets.push(preset);
        }
    }
    let mut aliases = Vec::new();
    if let Some(value) = table.get("alias") {
        let rows = value
            .as_array()
            .ok_or_else(|| format!("{source}: alias must be an array of tables"))?;
        for row in rows {
            let id = row
                .get("id")
                .and_then(toml::Value::as_str)
                .unwrap_or("<missing id>");
            let alias: AliasToml = row
                .clone()
                .try_into()
                .map_err(|error| format!("{source}: alias `{id}` is invalid: {error}"))?;
            aliases.push(alias);
        }
    }
    let fragment = FragmentToml {
        plugin: plugin.to_owned(),
        source: source.to_owned(),
        presets,
        aliases,
    };
    validate_file(&fragment)?;
    Ok(fragment)
}

fn validate_file(fragment: &FragmentToml) -> Result<(), String> {
    let source = &fragment.source;
    let preset_ids: Vec<&str> = fragment
        .presets
        .iter()
        .map(|preset| preset.id.as_str())
        .collect();
    if !super::check::strictly_sorted(&preset_ids) {
        return Err(format!("{source}: presets are not sorted by id"));
    }
    let alias_ids: Vec<&str> = fragment
        .aliases
        .iter()
        .map(|alias| alias.id.as_str())
        .collect();
    if !super::check::strictly_sorted(&alias_ids) {
        return Err(format!("{source}: aliases are not sorted by id"));
    }
    for preset in &fragment.presets {
        validate_preset(source, &fragment.plugin, preset)?;
    }
    for alias in &fragment.aliases {
        validate_id(source, "alias", &alias.id)?;
    }
    Ok(())
}

fn validate_preset(source: &str, plugin: &str, preset: &PresetToml) -> Result<(), String> {
    validate_id(source, "preset", &preset.id)?;
    let context = format!("{source}: preset `{}`", preset.id);
    let name = preset
        .name
        .as_deref()
        .ok_or_else(|| format!("{context} is missing name"))?;
    if name.is_empty() {
        return Err(format!("{context} has an empty name"));
    }
    let kind = preset
        .kind
        .as_deref()
        .ok_or_else(|| format!("{context} is missing kind"))?;
    if !matches!(kind, "vendor" | "relay" | "local" | "remote") {
        return Err(format!("{context} has invalid kind `{kind}`"));
    }
    let mut regions = BTreeSet::new();
    for region in &preset.regions {
        if region.id.is_empty() {
            return Err(format!("{context} has an empty region id"));
        }
        if !regions.insert(region.id.as_str()) {
            return Err(format!("{context} repeats region `{}`", region.id));
        }
        for (key, url) in [
            ("chat", &region.chat),
            ("responses", &region.responses),
            ("anthropic", &region.anthropic),
            ("decide", &region.decide),
            ("website", &region.website),
            ("keys_url", &region.keys_url),
        ] {
            validate_url(&context, key, url)?;
        }
    }
    for (key, url) in [
        ("chat", &preset.chat),
        ("responses", &preset.responses),
        ("anthropic", &preset.anthropic),
        ("generative", &preset.generative),
        ("decide", &preset.decide),
        ("endpoint", &preset.endpoint),
        ("website", &preset.website),
        ("keys_url", &preset.keys_url),
    ] {
        validate_url(&context, key, url)?;
    }
    if let Some(region) = preset.regions.first() {
        for (key, url, region_url) in [
            ("chat", &preset.chat, &region.chat),
            ("responses", &preset.responses, &region.responses),
            ("anthropic", &preset.anthropic, &region.anthropic),
            ("generative", &preset.generative, &String::new()),
        ] {
            if url != region_url {
                return Err(format!(
                    "{context} has mismatched {key} wire: preset URL `{url}` must equal first region URL `{region_url}`"
                ));
            }
        }
    }
    validate_placement(source, plugin, preset)?;
    Ok(())
}

fn validate_placement(source: &str, plugin: &str, preset: &PresetToml) -> Result<(), String> {
    if !super::check::builtin_factory_ids().contains(&plugin) {
        return Ok(());
    }
    let expected = default_wire_factory(preset);
    if plugin != expected {
        return Err(format!(
            "{source}: preset `{}` belongs in `plugins/providers/{expected}`, not `plugins/providers/{plugin}`",
            preset.id
        ));
    }
    let has_wire_url = [
        &preset.chat,
        &preset.responses,
        &preset.anthropic,
        &preset.generative,
    ]
    .into_iter()
    .chain(
        preset
            .regions
            .iter()
            .flat_map(|region| [&region.chat, &region.responses, &region.anthropic]),
    )
    .any(|url| !url.is_empty());
    let has_decide = !preset.decide.is_empty()
        || preset
            .regions
            .iter()
            .any(|region| !region.decide.is_empty());
    if !has_wire_url && preset.endpoint.is_empty() && !has_decide {
        return Err(format!(
            "{source}: preset `{}` has no wire URL and no endpoint",
            preset.id
        ));
    }
    Ok(())
}

fn default_wire_factory(preset: &PresetToml) -> &'static str {
    for (url, factory) in [
        (&preset.generative, "generative"),
        (&preset.chat, "openai-compat"),
        (&preset.responses, "openai-compat"),
        (&preset.anthropic, "anthropic"),
    ] {
        if !url.is_empty() {
            return factory;
        }
    }
    if let Some(region) = preset.regions.first() {
        for (url, factory) in [
            (&region.chat, "openai-compat"),
            (&region.responses, "openai-compat"),
            (&region.anthropic, "anthropic"),
        ] {
            if !url.is_empty() {
                return factory;
            }
        }
    }
    "openai-compat"
}

pub fn validate_preset_file(manifest: &str, file: &str) -> Result<(), String> {
    let valid = file.strip_suffix(".toml").is_some_and(|stem| {
        !stem.is_empty()
            && stem
                .bytes()
                .all(|byte| byte.is_ascii_lowercase() || byte.is_ascii_digit() || byte == b'-')
    });
    if !valid {
        return Err(format!(
            "{manifest}: presets entry `{file}` must be a plain .toml file name"
        ));
    }
    Ok(())
}

fn validate_id(source: &str, class: &str, id: &str) -> Result<(), String> {
    if super::check::invalid_preset_id(id) {
        return Err(format!("{source}: {class} `{id}` is not kebab-case"));
    }
    Ok(())
}

fn validate_url(context: &str, key: &str, url: &str) -> Result<(), String> {
    if url.is_empty() {
        return Ok(());
    }
    if !(url.starts_with("https://") || url.starts_with("http://"))
        || url.chars().any(char::is_whitespace)
    {
        return Err(format!(
            "{context} has invalid {key} URL `{url}`; expected http:// or https:// without whitespace"
        ));
    }
    Ok(())
}

pub fn validate_all(fragments: &[FragmentToml]) -> Result<(), String> {
    let mut owners = BTreeMap::new();
    let mut presets = BTreeMap::new();
    for fragment in fragments {
        for preset in &fragment.presets {
            insert_id(&mut owners, &fragment.source, "preset", &preset.id)?;
            presets.insert(preset.id.as_str(), preset);
        }
        for alias in &fragment.aliases {
            insert_id(&mut owners, &fragment.source, "alias", &alias.id)?;
        }
    }
    for fragment in fragments {
        for alias in &fragment.aliases {
            let Some(preset) = presets.get(alias.preset.as_str()) else {
                return Err(format!(
                    "{}: alias `{}` targets missing preset `{}`",
                    fragment.source, alias.id, alias.preset
                ));
            };
            if let Some(region) = alias.region.as_deref().filter(|region| !region.is_empty())
                && !preset.regions.iter().any(|item| item.id == region)
            {
                return Err(format!(
                    "{}: alias `{}` targets missing region `{}` of preset `{}`",
                    fragment.source, alias.id, region, alias.preset
                ));
            }
        }
    }
    Ok(())
}

fn insert_id<'a>(
    owners: &mut BTreeMap<&'a str, &'a str>,
    source: &'a str,
    class: &str,
    id: &'a str,
) -> Result<(), String> {
    if let Some(previous) = owners.insert(id, source) {
        return Err(format!(
            "{source}: duplicate {class} id `{id}`; first declared in {previous}"
        ));
    }
    Ok(())
}

pub fn valid_data_only_provider(id: &str, has_manifest: bool, has_presets: bool) -> bool {
    !has_manifest && has_presets && super::check::builtin_factory_ids().contains(&id)
}

pub fn render_preset(preset: &PresetToml, plugin: &str) -> String {
    let regions = preset
        .regions
        .iter()
        .map(render_region)
        .collect::<Vec<_>>()
        .join(", ");
    let config = preset
        .config
        .iter()
        .map(|(key, value)| format!("({key:?}, {value:?})"))
        .collect::<Vec<_>>()
        .join(", ");
    format!(
        "CompiledPreset {{ id: {:?}, plugin: {:?}, name: {:?}, kind: {:?}, short: {:?}, note: {:?}, catalog: {:?}, website: {:?}, keys_url: {:?}, site_host: {:?}, key_hint: {:?}, no_key: {}, no_list: {}, hosts: {}, only: {:?}, models: {}, header_hints: {}, endpoint: {:?}, endpoint_hint: {:?}, endpoint_needed: {:?}, region_label: {:?}, decide: {:?}, chat: {:?}, responses: {:?}, anthropic: {:?}, generative: {:?}, config: &[{}], regions: &[{}] }}",
        preset.id,
        plugin,
        preset.name.as_deref().unwrap_or(""),
        preset.kind.as_deref().unwrap_or(""),
        preset.short,
        preset.note,
        preset.catalog,
        preset.website,
        preset.keys_url,
        preset.site_host,
        preset.key_hint,
        preset.no_key,
        preset.no_list,
        preset.hosts,
        preset.only,
        strings(&preset.models),
        strings(&preset.header_hints),
        preset.endpoint,
        preset.endpoint_hint,
        preset.endpoint_needed,
        preset.region_label,
        preset.decide,
        preset.chat,
        preset.responses,
        preset.anthropic,
        preset.generative,
        config,
        regions,
    )
}

fn render_region(region: &RegionToml) -> String {
    let models = region.models.as_ref().map_or_else(
        || "None".to_owned(),
        |models| format!("Some({})", strings(models)),
    );
    format!(
        "CompiledRegion {{ id: {:?}, name: {:?}, chat: {:?}, responses: {:?}, anthropic: {:?}, decide: {:?}, lists: {}, keys_url: {:?}, website: {:?}, site_host: {:?}, catalog: {:?}, models: {}, embeddings: {}, drawers: {}, videos: {} }}",
        region.id,
        region.name,
        region.chat,
        region.responses,
        region.anthropic,
        region.decide,
        region.lists,
        region.keys_url,
        region.website,
        region.site_host,
        region.catalog,
        models,
        strings(&region.embeddings),
        strings(&region.drawers),
        strings(&region.videos),
    )
}

pub fn render_alias(alias: &AliasToml) -> String {
    let region = alias
        .region
        .as_deref()
        .filter(|region| !region.is_empty())
        .map_or_else(|| "None".to_owned(), |region| format!("Some({region:?})"));
    format!(
        "CompiledAlias {{ id: {:?}, preset: {:?}, region: {} }}",
        alias.id, alias.preset, region
    )
}

fn strings(values: &[String]) -> String {
    let values = values
        .iter()
        .map(|value| format!("{value:?}"))
        .collect::<Vec<_>>()
        .join(", ");
    format!("&[{values}]")
}

#[cfg(test)]
mod tests {
    use super::{FragmentToml, parse, render_preset, valid_data_only_provider, validate_all};

    fn valid() -> &'static str {
        "[[preset]]\nid = \"example\"\nname = \"Example\"\nkind = \"vendor\"\nendpoint = \"https://example.test\"\n"
    }

    fn error(text: &str) -> String {
        parse(text, "fixture/presets.toml", "openai-compat").expect_err("invalid fragment")
    }

    #[test]
    fn unknown_key_names_the_file_and_preset() {
        let text = error(
            "[[preset]]\nid = \"example\"\nname = \"Example\"\nkind = \"vendor\"\nurls = []\n",
        );
        assert!(text.contains("fixture/presets.toml"), "{text}");
        assert!(text.contains("preset `example`"), "{text}");
        assert!(text.contains("unknown field"), "{text}");
    }

    #[test]
    fn invalid_kind_and_missing_name_name_the_preset() {
        let text = error("[[preset]]\nid = \"example\"\nname = \"Example\"\nkind = \"other\"\n");
        assert!(text.contains("preset `example`"), "{text}");
        assert!(text.contains("invalid kind"), "{text}");
        let text = error("[[preset]]\nid = \"example\"\nkind = \"vendor\"\n");
        assert!(text.contains("preset `example`"), "{text}");
        assert!(text.contains("missing name"), "{text}");
    }

    #[test]
    fn presets_and_aliases_must_be_sorted() {
        let text = error(
            "[[preset]]\nid = \"z-last\"\nname = \"Z\"\nkind = \"vendor\"\n[[preset]]\nid = \"a-first\"\nname = \"A\"\nkind = \"vendor\"\n",
        );
        assert!(text.contains("not sorted"), "{text}");
        let text = error(
            "[[alias]]\nid = \"z-alias\"\npreset = \"example\"\n[[alias]]\nid = \"a-alias\"\npreset = \"example\"\n",
        );
        assert!(text.contains("aliases are not sorted"), "{text}");
    }

    #[test]
    fn region_ids_must_be_nonempty_and_unique() {
        let text = error(
            "[[preset]]\nid = \"example\"\nname = \"Example\"\nkind = \"vendor\"\n[[preset.region]]\nid = \"one\"\n[[preset.region]]\nid = \"one\"\n",
        );
        assert!(text.contains("repeats region `one`"), "{text}");
        let text = error(
            "[[preset]]\nid = \"example\"\nname = \"Example\"\nkind = \"vendor\"\n[[preset.region]]\nid = \"\"\n",
        );
        assert!(text.contains("empty region id"), "{text}");
    }

    #[test]
    fn urls_require_http_and_no_whitespace() {
        let text = error(
            "[[preset]]\nid = \"example\"\nname = \"Example\"\nkind = \"vendor\"\nchat = \"ftp://example.test\"\n",
        );
        assert!(text.contains("invalid chat URL"), "{text}");
        assert!(text.contains("preset `example`"), "{text}");
        let text = error(
            "[[preset]]\nid = \"example\"\nname = \"Example\"\nkind = \"vendor\"\nchat = \"https://example.test/a path\"\n",
        );
        assert!(text.contains("without whitespace"), "{text}");
        for key in ["website", "keys_url"] {
            for url in [
                "javascript:alert(1)",
                "data:text/plain,hello",
                "ftp://example.test",
            ] {
                let text = error(&format!(
                    "[[preset]]\nid = \"example\"\nname = \"Example\"\nkind = \"vendor\"\n{key} = \"{url}\"\n"
                ));
                assert!(text.contains(&format!("invalid {key} URL")), "{text}");
                let text = error(&format!(
                    "[[preset]]\nid = \"example\"\nname = \"Example\"\nkind = \"vendor\"\n[[preset.region]]\nid = \"one\"\n{key} = \"{url}\"\n"
                ));
                assert!(text.contains(&format!("invalid {key} URL")), "{text}");
            }
        }
    }

    #[test]
    fn regioned_presets_mirror_the_first_region_wires() {
        parse(
            "[[preset]]\nid = \"example\"\nname = \"Example\"\nkind = \"vendor\"\nchat = \"https://one.example.test\"\n[[preset.region]]\nid = \"one\"\nchat = \"https://one.example.test\"\n[[preset.region]]\nid = \"two\"\nchat = \"https://two.example.test\"\n",
            "fixture/presets.toml",
            "openai-compat",
        )
        .expect("mirrored first region");
        for (text, wire) in [
            (
                "[[preset]]\nid = \"example\"\nname = \"Example\"\nkind = \"vendor\"\nchat = \"https://one.example.test\"\n[[preset.region]]\nid = \"one\"\nchat = \"https://other.example.test\"\n",
                "chat",
            ),
            (
                "[[preset]]\nid = \"example\"\nname = \"Example\"\nkind = \"vendor\"\nchat = \"https://one.example.test\"\nresponses = \"https://one.example.test\"\n[[preset.region]]\nid = \"one\"\nchat = \"https://one.example.test\"\n",
                "responses",
            ),
            (
                "[[preset]]\nid = \"example\"\nname = \"Example\"\nkind = \"vendor\"\nchat = \"https://one.example.test\"\n[[preset.region]]\nid = \"one\"\nchat = \"https://one.example.test\"\nresponses = \"https://one.example.test\"\n",
                "responses",
            ),
        ] {
            let text = error(text);
            assert!(text.contains(&format!("mismatched {wire} wire")), "{text}");
            assert!(text.contains("fixture/presets.toml"), "{text}");
            assert!(text.contains("preset `example`"), "{text}");
        }
    }

    #[test]
    fn ids_must_be_kebab_case() {
        let text = error("[[preset]]\nid = \"Not_Valid\"\nname = \"Example\"\nkind = \"vendor\"\n");
        assert!(text.contains("not kebab-case"), "{text}");
        let text = error("[[alias]]\nid = \"Not_Valid\"\npreset = \"example\"\n");
        assert!(text.contains("alias `Not_Valid`"), "{text}");
    }

    #[test]
    fn aliases_must_target_a_preset_and_region() {
        let fragment = parse(
            "[[alias]]\nid = \"other\"\npreset = \"missing\"\n",
            "other.toml",
            "openai-compat",
        )
        .expect("file");
        let text = validate_all(&[fragment]).expect_err("missing preset");
        assert!(text.contains("alias `other`"), "{text}");
        assert!(text.contains("missing preset `missing`"), "{text}");
        let fragment = parse(
            "[[alias]]\nid = \"other\"\npreset = \"example\"\nregion = \"missing\"\n",
            "other.toml",
            "openai-compat",
        )
        .expect("file");
        let base = parse(valid(), "base.toml", "openai-compat").expect("base");
        let text = validate_all(&[base, fragment]).expect_err("missing region");
        assert!(text.contains("missing region `missing`"), "{text}");
    }

    #[test]
    fn duplicate_ids_across_files_include_aliases() {
        let one = parse(valid(), "one.toml", "openai-compat").expect("one");
        let two = parse(valid(), "two.toml", "openai-compat").expect("two");
        let text = validate_all(&[one, two]).expect_err("duplicate");
        assert!(text.contains("duplicate preset id `example`"), "{text}");
        assert!(text.contains("one.toml"), "{text}");
        assert!(text.contains("two.toml"), "{text}");
    }

    #[test]
    fn an_alias_may_not_reuse_a_preset_id() {
        let preset = parse(valid(), "one.toml", "openai-compat").expect("one");
        let alias = parse(
            "[[alias]]\nid = \"example\"\npreset = \"example\"\n",
            "two.toml",
            "openai-compat",
        )
        .expect("two");
        let text = validate_all(&[preset, alias]).expect_err("duplicate");
        assert!(text.contains("duplicate alias id `example`"), "{text}");
        assert!(text.contains("one.toml"), "{text}");
    }

    #[test]
    fn data_only_directories_are_builtin_factories_without_manifests() {
        assert!(valid_data_only_provider("openai-compat", false, true));
        assert!(!valid_data_only_provider("other", false, true));
        assert!(!valid_data_only_provider("openai-compat", true, true));
        assert!(!valid_data_only_provider("openai-compat", false, false));
    }

    #[test]
    fn builtin_rows_live_with_their_default_wire_factory() {
        for (plugin, url) in [
            ("generative", "generative = \"https://example.test\""),
            ("openai-compat", "chat = \"https://example.test\""),
            ("openai-compat", "responses = \"https://example.test\""),
            ("anthropic", "anthropic = \"https://example.test\""),
        ] {
            let text = format!(
                "[[preset]]\nid = \"example\"\nname = \"Example\"\nkind = \"vendor\"\n{url}\n"
            );
            parse(&text, "fixture/presets.toml", plugin).expect("default wire placement");
        }
        parse(
            "[[preset]]\nid = \"example\"\nname = \"Example\"\nkind = \"vendor\"\nanthropic = \"https://example.test\"\n[[preset.region]]\nid = \"one\"\nanthropic = \"https://example.test\"\n",
            "fixture/presets.toml",
            "anthropic",
        )
        .expect("first region determines placement");
        parse(valid(), "fixture/presets.toml", "openai-compat").expect("no-URL placement");
        let text = parse(
            "[[preset]]\nid = \"example\"\nname = \"Example\"\nkind = \"vendor\"\nchat = \"https://example.test\"\n",
            "fixture/presets.toml",
            "anthropic",
        )
        .expect_err("wrong factory directory");
        assert!(text.contains("fixture/presets.toml"), "{text}");
        assert!(text.contains("preset `example`"), "{text}");
        assert!(text.contains("plugins/providers/openai-compat"), "{text}");
    }

    #[test]
    fn builtin_no_url_rows_require_an_endpoint_unless_decide_only() {
        let text = error("[[preset]]\nid = \"example\"\nname = \"Example\"\nkind = \"vendor\"\n");
        assert!(text.contains("no wire URL and no endpoint"), "{text}");
        assert!(text.contains("fixture/presets.toml"), "{text}");
        assert!(text.contains("preset `example`"), "{text}");
        parse(valid(), "fixture/presets.toml", "openai-compat").expect("endpoint row");
        parse(
            "[[preset]]\nid = \"example\"\nname = \"Example\"\nkind = \"remote\"\ndecide = \"https://example.test/decide\"\n",
            "fixture/presets.toml",
            "openai-compat",
        )
        .expect("decide-only row");
    }

    #[test]
    fn preset_file_entries_are_plain_toml_file_names() {
        super::validate_preset_file("plugin.toml", "presets.toml").expect("plain file name");
        for file in [
            "nested/presets.toml",
            "nested\\presets.toml",
            "../presets.toml",
            "presets..toml",
            "presets.txt",
            "C:x.toml",
            "a b.toml",
            "X.toml",
        ] {
            let text = super::validate_preset_file("plugin.toml", file)
                .expect_err("invalid preset file name");
            assert!(text.contains("plugin.toml"), "{text}");
            assert!(text.contains(file), "{text}");
        }
    }

    #[test]
    fn rendering_escapes_config_keys_and_values() {
        let fragment: FragmentToml = parse("[[preset]]\nid = \"example\"\nname = \"Example\"\nkind = \"vendor\"\nendpoint = \"https://example.test\"\n[preset.config]\nquote = \"say \\\"hello\\\" and use C:\\\\tmp\"\n", "fixture.toml", "openai-compat").expect("valid");
        let rendered = render_preset(&fragment.presets[0], "openai-compat");
        assert!(
            rendered.contains("say \\\"hello\\\" and use C:\\\\tmp"),
            "{rendered}"
        );
    }

    #[test]
    fn non_string_config_values_are_rejected() {
        let text = error(
            "[[preset]]\nid = \"example\"\nname = \"Example\"\nkind = \"vendor\"\n[preset.config]\nnumber = 1\n",
        );
        assert!(text.contains("preset `example`"), "{text}");
        assert!(text.contains("invalid type"), "{text}");
    }
}
