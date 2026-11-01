//! Reads `plugins.toml` and each `plugin.toml`, then emits the registry.

use std::fs;
use std::path::{Path, PathBuf};

mod agent_toml {
    include!("src/agent_toml.rs");
}
use agent_toml::{AgentToml, parse, render};

#[allow(dead_code)]
mod check {
    include!("src/check.rs");
}
mod preset_toml {
    include!("src/preset_toml.rs");
}
use check::*;
use preset_toml::{FragmentToml, render_alias, render_preset};

struct Listed {
    id: String,
    kind: String,
    path: String,
    package: String,
    feature: String,
    max_accounts: u64,
    routes: Vec<DeclaredRoute>,
    fragments: Vec<FragmentToml>,
}

fn main() {
    if let Err(error) = run() {
        panic!("plugin registry: {error}");
    }
}

fn run() -> Result<(), String> {
    let workspace = PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("../..");
    println!("cargo:rerun-if-changed=plugins.toml");
    println!("cargo:rerun-if-changed=src/check.rs");
    println!("cargo:rerun-if-changed=src/agent_toml.rs");
    println!("cargo:rerun-if-changed=src/preset_toml.rs");
    println!("cargo:rerun-if-changed=Cargo.toml");
    let text = fs::read_to_string("plugins.toml").map_err(|error| error.to_string())?;
    let listed = parse_list(&workspace, &text)?;
    let discovered = discover(&workspace)?;
    let builtins = load_data_fragments(&workspace, "plugins/providers", true)?;
    let kind_ids: Vec<(&str, &str)> = listed
        .iter()
        .map(|plugin| (plugin.kind.as_str(), plugin.id.as_str()))
        .collect();
    if let Some((kind, id)) = duplicate_kind_id(&kind_ids) {
        return Err(format!("duplicate plugin id {id} in kind {kind}"));
    }
    let listed_ids: Vec<(&str, &str)> = listed
        .iter()
        .map(|plugin| (plugin.id.as_str(), plugin.kind.as_str()))
        .collect();
    if !strictly_sorted_plugin_ids(&listed_ids) {
        return Err("plugins.toml is not sorted by id and kind".to_owned());
    }
    let packages: Vec<&str> = listed
        .iter()
        .map(|plugin| plugin.package.as_str())
        .collect();
    if let Some(package) = duplicate_id(&packages) {
        return Err(format!("duplicate plugin package {package}"));
    }
    let features: Vec<&str> = listed
        .iter()
        .map(|plugin| plugin.feature.as_str())
        .collect();
    if let Some(feature) = duplicate_id(&features) {
        return Err(format!("duplicate plugin feature {feature}"));
    }
    for plugin in &listed {
        let expected_feature = expected_feature(&kind_ids, &plugin.kind, &plugin.id);
        if plugin.feature != expected_feature {
            return Err(format!(
                "{} feature is {}, expected {expected_feature}",
                plugin.id, plugin.feature
            ));
        }
        if reserved(&plugin.id) {
            return Err(format!("{} is a reserved plugin id", plugin.id));
        }
        if plugin.kind == "provider" && builtin_factory_ids().contains(&plugin.id.as_str()) {
            return Err(format!("{} is a builtin factory id", plugin.id));
        }
        let expected = expected_path(&plugin.kind, &plugin.id)?;
        if plugin.path != expected {
            return Err(format!(
                "{} path is {}, expected {expected}",
                plugin.id, plugin.path
            ));
        }
        let dir = workspace.join(&plugin.path);
        println!("cargo:rerun-if-changed=../../{}/plugin.toml", plugin.path);
        println!("cargo:rerun-if-changed=../../{}/Cargo.toml", plugin.path);
        check_package(&dir, &plugin.package)?;
        if !discovered
            .iter()
            .any(|(kind, id)| kind == &plugin.kind && id == &plugin.id)
        {
            return Err(format!("{}/{} is not on disk", plugin.kind, plugin.id));
        }
    }
    for (kind, id) in &discovered {
        if !listed
            .iter()
            .any(|plugin| &plugin.kind == kind && &plugin.id == id)
        {
            return Err(format!("{kind}/{id} is missing from plugins.toml"));
        }
    }
    let mut routes = Vec::new();
    for plugin in &listed {
        routes.extend(plugin.routes.iter().cloned());
    }
    if let Some(conflict) = route_conflicts(&routes) {
        return Err(conflict);
    }
    let fragments: Vec<FragmentToml> = listed
        .iter()
        .flat_map(|plugin| plugin.fragments.iter().cloned())
        .chain(builtins.iter().cloned())
        .collect();
    preset_toml::validate_all(&fragments)?;
    check_features(&listed)?;
    let agent_plugin_ids: Vec<String> = agent_collision_ids(&kind_ids)
        .into_iter()
        .map(str::to_owned)
        .collect();
    let inbound_ids: Vec<String> = listed
        .iter()
        .filter(|plugin| plugin.kind == "inbound")
        .map(|plugin| plugin.id.clone())
        .collect();
    let agents = load_agents(
        &workspace,
        "plugins/agents",
        &agent_plugin_ids,
        &[],
        &inbound_ids,
    )?;
    let out = PathBuf::from(std::env::var("OUT_DIR").map_err(|error| error.to_string())?);
    fs::write(out.join("registry.rs"), emit(&listed, &builtins))
        .map_err(|error| error.to_string())?;
    fs::write(out.join("agents.rs"), emit_agents(&agents, false))
        .map_err(|error| error.to_string())?;
    if std::env::var_os("CARGO_FEATURE_TEST_FIXTURES").is_some() {
        let agent_ids: Vec<String> = agents.iter().map(|agent| agent.id.clone()).collect();
        let fixtures = load_agents(
            &workspace,
            "plugins/registry/tests/fixtures/agents",
            &agent_plugin_ids,
            &agent_ids,
            &inbound_ids,
        )?;
        fs::write(out.join("fixture_agents.rs"), emit_agents(&fixtures, true))
            .map_err(|error| error.to_string())?;
        let preset_fixtures =
            load_data_fragments(&workspace, "plugins/registry/tests/fixtures/presets", false)?;
        preset_toml::validate_all(&preset_fixtures)?;
        fs::write(
            out.join("fixture_presets.rs"),
            emit_fixture_presets(&preset_fixtures),
        )
        .map_err(|error| error.to_string())?;
    }
    Ok(())
}

fn parse_list(workspace: &Path, text: &str) -> Result<Vec<Listed>, String> {
    let value: toml::Value = toml::from_str(text).map_err(|error| error.to_string())?;
    let Some(plugins) = value.get("plugin").and_then(toml::Value::as_array) else {
        return Err("plugins.toml has no [[plugin]] tables".to_owned());
    };
    let mut listed = Vec::new();
    for plugin in plugins {
        let id = field(plugin, "id")?;
        let kind = field(plugin, "kind")?;
        let path = field(plugin, "path")?;
        let package = field(plugin, "package")?;
        let feature = field(plugin, "feature")?;
        let manifest = fs::read_to_string(workspace.join(&path).join("plugin.toml"))
            .map_err(|error| format!("read {path}/plugin.toml: {error}"))?;
        let declared: toml::Value = toml::from_str(&manifest).map_err(|error| error.to_string())?;
        if field(&declared, "id")? != id || field(&declared, "kind")? != kind {
            return Err(format!("{path}/plugin.toml does not match plugins.toml"));
        }
        let max_accounts = declared
            .get("max_accounts")
            .map(|value| {
                value
                    .as_integer()
                    .and_then(|value| u64::try_from(value).ok())
                    .map(|value| value.min(1000))
                    .ok_or_else(|| {
                        format!("{path}/plugin.toml max_accounts must be a nonnegative integer")
                    })
            })
            .transpose()?
            .unwrap_or(0);
        let routes = routes_of(&id, &declared)?;
        if kind == "inbound" && routes.is_empty() {
            return Err(format!("{id} declares no routes"));
        }
        let fragments = presets_of(&id, &path, &workspace.join(&path), &declared)?;
        listed.push(Listed {
            id,
            kind,
            path,
            package,
            feature,
            max_accounts,
            routes,
            fragments,
        });
    }
    Ok(listed)
}

fn routes_of(plugin: &str, declared: &toml::Value) -> Result<Vec<DeclaredRoute>, String> {
    let Some(routes) = declared.get("route") else {
        return Ok(Vec::new());
    };
    let Some(routes) = routes.as_array() else {
        return Err(format!("{plugin} route table is not an array"));
    };
    let mut parsed = Vec::new();
    for route in routes {
        let method = field(route, "method")?;
        let prefix = field(route, "prefix")?;
        if invalid_method(&method) {
            return Err(format!("{plugin} has method {method}"));
        }
        if invalid_prefix(&prefix) {
            return Err(format!("{plugin} has prefix {prefix}"));
        }
        parsed.push(DeclaredRoute {
            plugin: plugin.to_owned(),
            method,
            prefix,
        });
    }
    Ok(parsed)
}

fn presets_of(
    plugin: &str,
    path: &str,
    directory: &Path,
    declared: &toml::Value,
) -> Result<Vec<FragmentToml>, String> {
    let source = format!("{path}/plugin.toml");
    let mut fragment = toml::map::Map::new();
    for key in ["preset", "alias"] {
        if let Some(value) = declared.get(key) {
            fragment.insert(key.to_owned(), value.clone());
        }
    }
    let mut fragments = vec![preset_toml::parse_value(
        &toml::Value::Table(fragment),
        &source,
        plugin,
    )?];
    let Some(files) = declared.get("presets") else {
        return Ok(fragments);
    };
    let files = files
        .as_array()
        .ok_or_else(|| format!("{source}: presets must be a string array"))?;
    for file in files {
        let file = file
            .as_str()
            .ok_or_else(|| format!("{source}: presets must be a string array"))?;
        preset_toml::validate_preset_file(&source, file)?;
        let source = format!("{path}/{file}");
        println!("cargo:rerun-if-changed=../../{source}");
        let text = fs::read_to_string(directory.join(file))
            .map_err(|error| format!("read {source}: {error}"))?;
        fragments.push(preset_toml::parse(&text, &source, plugin)?);
    }
    Ok(fragments)
}

fn load_data_fragments(
    workspace: &Path,
    root: &str,
    builtin_only: bool,
) -> Result<Vec<FragmentToml>, String> {
    println!("cargo:rerun-if-changed=../../{root}");
    let directory = workspace.join(root);
    if !directory.is_dir() {
        return Ok(Vec::new());
    }
    let mut paths = Vec::new();
    for entry in fs::read_dir(&directory).map_err(|error| error.to_string())? {
        let path = entry.map_err(|error| error.to_string())?.path();
        if path.is_dir() {
            paths.push(path);
        }
    }
    paths.sort();
    let mut fragments = Vec::new();
    for path in paths {
        let id = path
            .file_name()
            .map(|name| name.to_string_lossy().into_owned())
            .ok_or_else(|| format!("{} has no directory name", path.display()))?;
        let presets = path.join("presets.toml");
        if path.join("plugin.toml").is_file() || path.join("Cargo.toml").exists() {
            continue;
        }
        if !presets.is_file() {
            continue;
        }
        if builtin_only && !preset_toml::valid_data_only_provider(&id, false, true) {
            return Err(format!(
                "{root}/{id} is not a builtin factory data-only directory"
            ));
        }
        let source = format!("{root}/{id}/presets.toml");
        println!("cargo:rerun-if-changed=../../{source}");
        let text =
            fs::read_to_string(&presets).map_err(|error| format!("read {source}: {error}"))?;
        fragments.push(preset_toml::parse(&text, &source, &id)?);
    }
    Ok(fragments)
}

fn field(value: &toml::Value, key: &str) -> Result<String, String> {
    value
        .get(key)
        .and_then(toml::Value::as_str)
        .map(str::to_owned)
        .ok_or_else(|| format!("missing {key}"))
}

fn expected_path(kind: &str, id: &str) -> Result<String, String> {
    let directory = match kind {
        "provider" => "providers",
        "subscription" => "subscriptions",
        "agent" => "agents",
        "inbound" => "inbound",
        _ => return Err(format!("unknown plugin kind {kind}")),
    };
    Ok(format!("plugins/{directory}/{id}"))
}

fn check_package(dir: &Path, package: &str) -> Result<(), String> {
    let text = fs::read_to_string(dir.join("Cargo.toml"))
        .map_err(|error| format!("read {}: {error}", dir.display()))?;
    let value: toml::Value = toml::from_str(&text).map_err(|error| error.to_string())?;
    let name = value
        .get("package")
        .and_then(|package| package.get("name"))
        .and_then(toml::Value::as_str)
        .unwrap_or("");
    if name != package {
        return Err(format!("{} package is {name}", dir.display()));
    }
    Ok(())
}

fn discover(workspace: &Path) -> Result<Vec<(String, String)>, String> {
    // The plugins directory itself, so a new unlisted plugin directory reruns
    // the on-disk bijection check. A new plugin is created inside a kind
    // directory, and that directory's mtime is what Cargo sees.
    println!("cargo:rerun-if-changed=../../plugins");
    let mut ids = Vec::new();
    for kind in ["providers", "subscriptions", "agents", "inbound"] {
        println!("cargo:rerun-if-changed=../../plugins/{kind}");
        let dir = workspace.join("plugins").join(kind);
        if !dir.is_dir() {
            continue;
        }
        let entries = fs::read_dir(&dir).map_err(|error| error.to_string())?;
        for entry in entries {
            let entry = entry.map_err(|error| error.to_string())?;
            if !entry.path().is_dir() {
                continue;
            }
            let manifest = entry.path().join("plugin.toml");
            if !manifest.is_file() {
                if kind == "agents" {
                    let data_only = entry.path().join("agent.toml").is_file()
                        && !entry.path().join("Cargo.toml").exists();
                    if data_only {
                        continue;
                    }
                    return Err(format!(
                        "plugins/agents/{} must contain agent.toml or plugin.toml",
                        entry.file_name().to_string_lossy()
                    ));
                }
                continue;
            }
            println!(
                "cargo:rerun-if-changed=../../plugins/{kind}/{}/plugin.toml",
                entry.file_name().to_string_lossy()
            );
            let text = fs::read_to_string(&manifest).map_err(|error| error.to_string())?;
            let value: toml::Value = toml::from_str(&text).map_err(|error| error.to_string())?;
            ids.push((field(&value, "kind")?, field(&value, "id")?));
        }
    }
    Ok(ids)
}

fn load_agents(
    workspace: &Path,
    root: &str,
    plugin_ids: &[String],
    agent_ids: &[String],
    inbound_ids: &[String],
) -> Result<Vec<AgentToml>, String> {
    println!("cargo:rerun-if-changed=../../{root}");
    let directory = workspace.join(root);
    if !directory.is_dir() {
        return Ok(Vec::new());
    }
    let mut paths = Vec::new();
    for entry in fs::read_dir(&directory).map_err(|error| error.to_string())? {
        let path = entry.map_err(|error| error.to_string())?.path();
        if path.join("agent.toml").is_file() {
            paths.push(path);
        }
    }
    paths.sort();
    let mut occupied = agent_ids.to_vec();
    let mut agents = Vec::new();
    for path in paths {
        let id = path
            .file_name()
            .map(|name| name.to_string_lossy().into_owned())
            .ok_or_else(|| format!("{} has no directory name", path.display()))?;
        println!("cargo:rerun-if-changed=../../{root}/{id}/agent.toml");
        let source = format!("{root}/{id}/agent.toml");
        let text = fs::read_to_string(path.join("agent.toml"))
            .map_err(|error| format!("read {source}: {error}"))?;
        let agent = parse(&text, &source, &id, plugin_ids, &occupied, inbound_ids)?;
        occupied.push(agent.id.clone());
        agents.push(agent);
    }
    Ok(agents)
}

fn emit_agents(agents: &[AgentToml], fixtures: bool) -> String {
    let mut statics = String::new();
    let mut names = Vec::new();
    for agent in agents {
        let (name, source) = render(agent);
        names.push(name);
        statics.push_str(&source);
    }
    let list = names
        .iter()
        .map(|name| format!("&{name}"))
        .collect::<Vec<_>>()
        .join(", ");
    let functions = if fixtures {
        r#"/// Test-only compiled agent descriptions, sorted by id.
#[doc(hidden)]
#[must_use]
pub fn fixture_agents() -> &'static [&'static hofund_plugin_api::AgentDescription] {
    DESCRIPTIONS
}
"#
    } else {
        r#"/// Every compiled agent description, sorted by id.
#[must_use]
pub fn agents() -> &'static [&'static hofund_plugin_api::AgentDescription] {
    DESCRIPTIONS
}

/// The compiled agent description for `id`.
#[must_use]
pub fn agent(id: &str) -> Option<&'static hofund_plugin_api::AgentDescription> {
    agents().iter().copied().find(|agent| agent.id().as_str() == id)
}
"#
    };
    format!(
        "// @generated by plugins/registry/build.rs. Do not edit.\n\n{statics}\nstatic DESCRIPTIONS: &[&hofund_plugin_api::AgentDescription] = &[{list}];\n\n{functions}"
    )
}

fn check_features(listed: &[Listed]) -> Result<(), String> {
    let text = fs::read_to_string("Cargo.toml").map_err(|error| error.to_string())?;
    let value: toml::Value = toml::from_str(&text).map_err(|error| error.to_string())?;
    let features = value
        .get("features")
        .and_then(toml::Value::as_table)
        .ok_or_else(|| "registry Cargo.toml has no features".to_owned())?;
    let default = features
        .get("default")
        .and_then(toml::Value::as_array)
        .ok_or_else(|| "registry default features are missing".to_owned())?;
    for plugin in listed {
        if !default
            .iter()
            .any(|item| item.as_str() == Some(plugin.feature.as_str()))
        {
            return Err(format!("default features omit {}", plugin.feature));
        }
        let expected = format!("dep:{}", plugin.package);
        let enabled = features
            .get(&plugin.feature)
            .and_then(toml::Value::as_array)
            .is_some_and(|items| items.iter().any(|item| item.as_str() == Some(&expected)));
        if !enabled {
            return Err(format!(
                "feature {} does not enable {expected}",
                plugin.feature
            ));
        }
    }
    Ok(())
}

fn emit(listed: &[Listed], builtins: &[FragmentToml]) -> String {
    let mut routes = String::new();
    let mut calls = String::new();
    let mut accounts = String::new();
    let mut presets = String::new();
    let mut aliases = String::new();
    for plugin in listed {
        let rust_name = plugin.package.replace('-', "_");
        let kind = match plugin.kind.as_str() {
            "provider" => "Provider",
            "subscription" => "Subscription",
            "agent" => "Agent",
            "inbound" => "Inbound",
            _ => unreachable!("plugin kind was validated"),
        };
        calls.push_str(&format!(
            "        #[cfg(feature = \"{}\")]\n        (hofund_plugin_api::PluginKind::{kind}, \"{}\", {rust_name}::contribute()),\n",
            plugin.feature, plugin.id
        ));
        accounts.push_str(&format!(
            "        #[cfg(feature = \"{}\")]\n        ({:?}, {:?}, {}),\n",
            plugin.feature, plugin.kind, plugin.id, plugin.max_accounts
        ));
        for route in &plugin.routes {
            routes.push_str(&format!(
                "        #[cfg(feature = \"{}\")]\n        CompiledRoute {{ plugin: {:?}, method: {:?}, prefix: {:?} }},\n",
                plugin.feature, route.plugin, route.method, route.prefix
            ));
        }
        for fragment in &plugin.fragments {
            for preset in &fragment.presets {
                presets.push_str(&format!(
                    "        #[cfg(feature = \"{}\")]\n        {},\n",
                    plugin.feature,
                    render_preset(preset, &plugin.id)
                ));
            }
            for alias in &fragment.aliases {
                aliases.push_str(&format!(
                    "        #[cfg(feature = \"{}\")]\n        {},\n",
                    plugin.feature,
                    render_alias(alias)
                ));
            }
        }
    }
    for fragment in builtins {
        for preset in &fragment.presets {
            presets.push_str(&format!(
                "        {},\n",
                render_preset(preset, &fragment.plugin)
            ));
        }
        for alias in &fragment.aliases {
            aliases.push_str(&format!("        {},\n", render_alias(alias)));
        }
    }
    format!(
        r#"// @generated by plugins/registry/build.rs. Do not edit.

/// One route compiled from `plugin.toml`.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct CompiledRoute {{
    /// Plugin id.
    pub plugin: &'static str,
    /// HTTP method.
    pub method: &'static str,
    /// Path prefix.
    pub prefix: &'static str,
}}

/// One region compiled from a preset fragment.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct CompiledRegion {{
    /// Region id.
    pub id: &'static str,
    /// Display name.
    pub name: &'static str,
    /// Chat Completions base URL.
    pub chat: &'static str,
    /// Responses base URL.
    pub responses: &'static str,
    /// Anthropic Messages base URL.
    pub anthropic: &'static str,
    /// Route-classification URL.
    pub decide: &'static str,
    /// Whether this region supports a live model list.
    pub lists: bool,
    /// Key-creation page.
    pub keys_url: &'static str,
    /// Documentation page.
    pub website: &'static str,
    /// Partner site host.
    pub site_host: &'static str,
    /// Model-catalog id.
    pub catalog: &'static str,
    /// Region-specific model ids, replacing the preset ids when present.
    pub models: Option<&'static [&'static str]>,
    /// Embedding model ids.
    pub embeddings: &'static [&'static str],
    /// Image model ids.
    pub drawers: &'static [&'static str],
    /// Video model ids.
    pub videos: &'static [&'static str],
}}

/// One preset compiled from a fragment.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct CompiledPreset {{
    /// Preset id.
    pub id: &'static str,
    /// Plugin that serves this preset.
    pub plugin: &'static str,
    /// Display name.
    pub name: &'static str,
    /// Preset class.
    pub kind: &'static str,
    /// Short display name.
    pub short: &'static str,
    /// One-line note.
    pub note: &'static str,
    /// Model-catalog id.
    pub catalog: &'static str,
    /// Documentation page.
    pub website: &'static str,
    /// Key-creation page.
    pub keys_url: &'static str,
    /// Partner site host.
    pub site_host: &'static str,
    /// API-key input hint.
    pub key_hint: &'static str,
    /// Whether a key is optional.
    pub no_key: bool,
    /// Whether the preset has no ordinary live model list.
    pub no_list: bool,
    /// Whether the plugin builds its base URL from its own options, so a
    /// preset without an endpoint needs no `base_url`.
    pub options_base: bool,
    /// Whether the endpoint serves other makers' models.
    pub hosts: bool,
    /// Required live-model id prefix.
    pub only: &'static str,
    /// Static model ids.
    pub models: &'static [&'static str],
    /// Optional request-header names.
    pub header_hints: &'static [&'static str],
    /// Example caller-supplied endpoint.
    pub endpoint: &'static str,
    /// Endpoint input hint.
    pub endpoint_hint: &'static str,
    /// Missing-endpoint guidance.
    pub endpoint_needed: &'static str,
    /// Region selector label.
    pub region_label: &'static str,
    /// Route-classification URL.
    pub decide: &'static str,
    /// Chat Completions base URL.
    pub chat: &'static str,
    /// Responses base URL.
    pub responses: &'static str,
    /// Anthropic Messages base URL.
    pub anthropic: &'static str,
    /// Generative Language base URL.
    pub generative: &'static str,
    /// Static config entries, in key order.
    pub config: &'static [(&'static str, &'static str)],
    /// Regions in declared order.
    pub regions: &'static [CompiledRegion],
}}

/// One alias compiled from a fragment.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct CompiledAlias {{
    /// Alias id.
    pub id: &'static str,
    /// Target preset id.
    pub preset: &'static str,
    /// Default target region.
    pub region: Option<&'static str>,
}}

/// The `max_accounts` declaration for a linked provider or subscription.
#[must_use]
pub fn declared_max_accounts(kind: &str, id: &str) -> Option<u64> {{
    const DECLARED: &[(&str, &str, u64)] = &[
{accounts}    ];
    DECLARED.iter().copied().find(|(listed_kind, listed_id, _)| {{
        *listed_kind == kind && *listed_id == id
    }}).map(|(_, _, limit)| limit)
}}

/// Routes for the features enabled on this build.
#[must_use]
pub fn compiled_routes() -> &'static [CompiledRoute] {{
    &[
{routes}    ]
}}

/// Presets for the features enabled on this build.
#[must_use]
pub fn compiled_presets() -> &'static [CompiledPreset] {{
    &[
{presets}    ]
}}

/// Aliases for the features enabled on this build.
#[must_use]
pub fn compiled_aliases() -> &'static [CompiledAlias] {{
    &[
{aliases}    ]
}}

/// Kind, id, and contribution for the features enabled on this build.
#[must_use]
pub fn contributions() -> Vec<(hofund_plugin_api::PluginKind, &'static str, hofund_plugin_sdk::Contribution)> {{
    vec![
{calls}    ]
}}
"#
    )
}

fn emit_fixture_presets(fragments: &[FragmentToml]) -> String {
    let mut presets = String::new();
    let mut aliases = String::new();
    for fragment in fragments {
        for preset in &fragment.presets {
            presets.push_str(&format!(
                "        {},\n",
                render_preset(preset, &fragment.plugin)
            ));
        }
        for alias in &fragment.aliases {
            aliases.push_str(&format!("        {},\n", render_alias(alias)));
        }
    }
    format!(
        r#"// @generated by plugins/registry/build.rs. Do not edit.

use super::{{CompiledAlias, CompiledPreset, CompiledRegion}};

/// Test-only compiled presets.
#[doc(hidden)]
#[must_use]
pub fn fixture_presets() -> &'static [CompiledPreset] {{
    &[
{presets}    ]
}}

/// Test-only compiled aliases.
#[doc(hidden)]
#[must_use]
pub fn fixture_aliases() -> &'static [CompiledAlias] {{
    &[
{aliases}    ]
}}
"#
    )
}
