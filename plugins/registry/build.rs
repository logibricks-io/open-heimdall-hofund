//! Reads `plugins.toml` and each `plugin.toml`, then emits the registry.

use std::fs;
use std::path::{Path, PathBuf};

include!("src/check.rs");

struct Listed {
    id: String,
    kind: String,
    path: String,
    package: String,
    feature: String,
    routes: Vec<DeclaredRoute>,
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
    check_features(&listed)?;
    let generated = emit(&listed);
    let out = PathBuf::from(std::env::var("OUT_DIR").map_err(|error| error.to_string())?);
    fs::write(out.join("registry.rs"), generated).map_err(|error| error.to_string())?;
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
        listed.push(Listed {
            id,
            kind,
            path,
            package,
            feature,
            routes,
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
    let mut ids = Vec::new();
    for kind in ["providers", "subscriptions", "agents", "inbound"] {
        let dir = workspace.join("plugins").join(kind);
        if !dir.is_dir() {
            continue;
        }
        let entries = fs::read_dir(&dir).map_err(|error| error.to_string())?;
        for entry in entries {
            let entry = entry.map_err(|error| error.to_string())?;
            let manifest = entry.path().join("plugin.toml");
            if !manifest.is_file() {
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

/// Routes for the features enabled on this build.
#[must_use]
pub fn compiled_routes() -> &'static [CompiledRoute] {{
    &[
{routes}    ]
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
