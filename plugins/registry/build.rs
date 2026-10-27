//! Reads `plugins.toml` and each `plugin.toml`, then emits the registry.

use std::fs;
use std::path::{Path, PathBuf};

mod agent_toml {
    include!("src/agent_toml.rs");
}
use agent_toml::{AgentToml, parse, render};

include!("src/reserved_agent_ids.rs");
include!("src/check.rs");

struct Listed {
    id: String,
    kind: String,
    path: String,
    package: String,
    feature: String,
    routes: Vec<DeclaredRoute>,
    presets: Vec<Preset>,
}

struct Preset {
    id: String,
    config: Vec<(String, String)>,
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
    println!("cargo:rerun-if-changed=src/reserved_agent_ids.rs");
    println!("cargo:rerun-if-changed=Cargo.toml");
    let text = fs::read_to_string("plugins.toml").map_err(|error| error.to_string())?;
    let listed = parse_list(&workspace, &text)?;
    let discovered = discover(&workspace)?;
    let listed_ids: Vec<&str> = listed.iter().map(|plugin| plugin.id.as_str()).collect();
    if let Some(id) = duplicate_id(&listed_ids) {
        return Err(format!("duplicate plugin id {id}"));
    }
    if !strictly_sorted(&listed_ids) {
        return Err("plugins.toml is not sorted by id".to_owned());
    }
    for plugin in &listed {
        if reserved(&plugin.id) {
            return Err(format!("{} is a reserved plugin id", plugin.id));
        }
        if plugin.feature != plugin.id {
            return Err(format!(
                "{} feature {} does not match the id",
                plugin.id, plugin.feature
            ));
        }
        if builtin_factory_ids().contains(&plugin.id.as_str()) {
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
        if !discovered.iter().any(|id| id == &plugin.id) {
            return Err(format!("{} is not on disk", plugin.id));
        }
    }
    for id in &discovered {
        if !listed.iter().any(|plugin| plugin.id == *id) {
            return Err(format!("{id} is missing from plugins.toml"));
        }
    }
    let mut routes = Vec::new();
    for plugin in &listed {
        routes.extend(plugin.routes.iter().cloned());
    }
    if let Some(conflict) = route_conflicts(&routes) {
        return Err(conflict);
    }
    let mut preset_ids = Vec::new();
    for plugin in &listed {
        for preset in &plugin.presets {
            if invalid_preset_id(&preset.id) {
                return Err(format!(
                    "{} preset {} is not kebab-case",
                    plugin.id, preset.id
                ));
            }
            preset_ids.push(preset.id.as_str());
        }
    }
    if let Some(id) = duplicate_id(&preset_ids) {
        return Err(format!("duplicate preset id {id}"));
    }
    check_features(&listed)?;
    let plugin_ids: Vec<String> = listed.iter().map(|plugin| plugin.id.clone()).collect();
    let inbound_ids: Vec<String> = listed
        .iter()
        .filter(|plugin| plugin.kind == "inbound")
        .map(|plugin| plugin.id.clone())
        .collect();
    let agents = load_agents(&workspace, "plugins/agents", &plugin_ids, &[], &inbound_ids)?;
    let out = PathBuf::from(std::env::var("OUT_DIR").map_err(|error| error.to_string())?);
    fs::write(out.join("registry.rs"), emit(&listed)).map_err(|error| error.to_string())?;
    fs::write(out.join("agents.rs"), emit_agents(&agents, false))
        .map_err(|error| error.to_string())?;
    if std::env::var_os("CARGO_FEATURE_TEST_FIXTURES").is_some() {
        let agent_ids: Vec<String> = agents.iter().map(|agent| agent.id.clone()).collect();
        let fixtures = load_agents(
            &workspace,
            "plugins/registry/tests/fixtures/agents",
            &plugin_ids,
            &agent_ids,
            &inbound_ids,
        )?;
        fs::write(out.join("fixture_agents.rs"), emit_agents(&fixtures, true))
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
        let routes = routes_of(&id, &declared)?;
        if kind == "inbound" && routes.is_empty() {
            return Err(format!("{id} declares no routes"));
        }
        let presets = presets_of(&id, &declared)?;
        listed.push(Listed {
            id,
            kind,
            path,
            package,
            feature,
            routes,
            presets,
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

fn presets_of(plugin: &str, declared: &toml::Value) -> Result<Vec<Preset>, String> {
    let Some(presets) = declared.get("preset") else {
        return Ok(Vec::new());
    };
    let Some(presets) = presets.as_array() else {
        return Err(format!("{plugin} preset table is not an array"));
    };
    let mut parsed = Vec::new();
    for preset in presets {
        let id = field(preset, "id")?;
        let mut config = Vec::new();
        if let Some(table) = preset.get("config") {
            let Some(table) = table.as_table() else {
                return Err(format!("{plugin} preset {id} config is not a table"));
            };
            for (key, value) in table {
                let Some(text) = value.as_str() else {
                    return Err(format!("{plugin} preset {id} config {key} is not a string"));
                };
                if key.is_empty() || key.contains(['"', '\\']) || text.contains(['"', '\\', '\n']) {
                    return Err(format!(
                        "{plugin} preset {id} config {key} is not plain text"
                    ));
                }
                config.push((key.clone(), text.to_owned()));
            }
            config.sort_by(|left, right| left.0.cmp(&right.0));
        }
        parsed.push(Preset { id, config });
    }
    let ids: Vec<&str> = parsed.iter().map(|preset| preset.id.as_str()).collect();
    if let Some(id) = duplicate_id(&ids) {
        return Err(format!("{plugin} repeats preset {id}"));
    }
    if !strictly_sorted(&ids) {
        return Err(format!("{plugin} presets are not sorted by id"));
    }
    Ok(parsed)
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

fn discover(workspace: &Path) -> Result<Vec<String>, String> {
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
            ids.push(field(&value, "id")?);
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

fn emit(listed: &[Listed]) -> String {
    let mut routes = String::new();
    let mut calls = String::new();
    let mut presets = String::new();
    for plugin in listed {
        let rust_name = plugin.package.replace('-', "_");
        calls.push_str(&format!(
            "        #[cfg(feature = \"{}\")]\n        (\"{}\", {rust_name}::contribute()),\n",
            plugin.feature, plugin.id
        ));
        for route in &plugin.routes {
            routes.push_str(&format!(
                "        #[cfg(feature = \"{}\")]\n        CompiledRoute {{ plugin: \"{}\", method: \"{}\", prefix: \"{}\" }},\n",
                plugin.feature, route.plugin, route.method, route.prefix
            ));
        }
        for preset in &plugin.presets {
            let mut pairs = String::new();
            for (key, value) in &preset.config {
                pairs.push_str(&format!("(\"{key}\", \"{value}\"), "));
            }
            presets.push_str(&format!(
                "        #[cfg(feature = \"{}\")]\n        CompiledPreset {{ id: \"{}\", plugin: \"{}\", config: &[{pairs}] }},\n",
                plugin.feature, preset.id, plugin.id
            ));
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

/// One preset id compiled from `plugin.toml`.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct CompiledPreset {{
    /// Preset id.
    pub id: &'static str,
    /// Plugin that serves this preset.
    pub plugin: &'static str,
    /// Static config entries, in key order.
    pub config: &'static [(&'static str, &'static str)],
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

/// Contributions for the features enabled on this build.
#[must_use]
pub fn contributions() -> Vec<(&'static str, hofund_plugin_sdk::Contribution)> {{
    vec![
{calls}    ]
}}
"#
    )
}
