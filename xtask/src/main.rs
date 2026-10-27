//! Local maintenance tasks for the hofund workspace.
//!
//! Run `cargo xtask help` for the task list. This crate is not part of the gateway
//! runtime, and no workspace crate may depend on it. Architecture checks live in
//! this crate's integration tests.

use std::path::{Path, PathBuf};
use std::process::{Command, ExitCode};

use xtask::boundary;

fn main() -> ExitCode {
    match std::env::args().nth(1).as_deref() {
        None | Some("help" | "--help" | "-h") => {
            print_help();
            ExitCode::SUCCESS
        }
        Some("check-plugins") => check_plugins(),
        Some(task) => {
            eprintln!("unknown xtask `{task}`");
            ExitCode::from(2)
        }
    }
}

fn print_help() {
    println!(
        "\
hofund xtask

Tasks:
  help            Show this help
  check-plugins   Check each plugin: allow-list, source boundary, Clippy, cargo tree

Dependency direction and the vendor-name rule are enforced by
`cargo test --workspace`."
    );
}

const PLUGIN_KINDS: &[&str] = &["providers", "subscriptions", "agents", "inbound"];

/// Directory of the workspace that contains `xtask`, independent of the
/// process working directory.
fn workspace_root() -> PathBuf {
    PathBuf::from(env!("CARGO_MANIFEST_DIR"))
        .parent()
        .expect("xtask lives one directory below the workspace root")
        .to_path_buf()
}

/// Package names under `plugins/<kind>/<id>/Cargo.toml`.
///
/// A missing `plugins/` directory is an empty list. A directory that exists
/// and contains no package, an unknown kind, or a manifest that cannot be
/// read is an error: the check fails closed.
fn load_plugin_packages(plugins: &Path) -> Result<Vec<(String, PathBuf)>, String> {
    if !plugins.exists() {
        return Ok(Vec::new());
    }
    if !plugins.is_dir() {
        return Err(format!("{} is not a directory", plugins.display()));
    }
    let mut names = Vec::new();
    let mut data_only = false;
    let entries = std::fs::read_dir(plugins)
        .map_err(|error| format!("read {}: {error}", plugins.display()))?;
    for entry in entries {
        let entry = entry.map_err(|error| format!("read {}: {error}", plugins.display()))?;
        let kind = entry.file_name();
        let kind = kind.to_string_lossy();
        if kind.starts_with('.') {
            continue;
        }
        let kind_path = entry.path();
        if kind == "registry" {
            if boundary::registry_has_package(&kind_path)? {
                return Err("plugins/registry must not contain a package".to_owned());
            }
            continue;
        }
        if !kind_path.is_dir() {
            return Err(format!("plugins/{kind} is not a plugin kind directory"));
        }
        if !PLUGIN_KINDS.contains(&kind.as_ref()) {
            return Err(format!("unknown plugin kind {kind}"));
        }
        let children = std::fs::read_dir(&kind_path)
            .map_err(|error| format!("read {}: {error}", kind_path.display()))?;
        for child in children {
            let child = child.map_err(|error| format!("read {}: {error}", kind_path.display()))?;
            if !child.path().is_dir() {
                continue;
            }
            let id = child.file_name().to_string_lossy().into_owned();
            let manifest_path = child.path().join("Cargo.toml");
            let plugin_manifest = child.path().join("plugin.toml").is_file();
            if kind == "providers"
                && !plugin_manifest
                && !manifest_path.exists()
                && child.path().join("presets.toml").is_file()
            {
                if !matches!(id.as_str(), "anthropic" | "generative" | "openai-compat") {
                    return Err(format!(
                        "providers/{id} is not a builtin factory data-only directory"
                    ));
                }
                data_only = true;
                continue;
            }
            if kind == "agents" {
                let agent = child.path().join("agent.toml").is_file();
                let plugin = child.path().join("plugin.toml").is_file();
                if agent && !plugin && !manifest_path.exists() {
                    data_only = true;
                    continue;
                }
                if !agent && !plugin {
                    return Err(format!(
                        "agents/{id} must contain agent.toml or plugin.toml"
                    ));
                }
            }
            if !manifest_path.is_file() {
                return Err(format!("{kind}/{id} manifest is missing"));
            }
            let text = std::fs::read_to_string(&manifest_path)
                .map_err(|error| format!("read {}: {error}", manifest_path.display()))?;
            let value = toml::from_str::<toml::Value>(&text)
                .map_err(|error| format!("{kind}/{id} manifest is not valid toml: {error}"))?;
            let Some(name) = value
                .get("package")
                .and_then(|package| package.get("name"))
                .and_then(toml::Value::as_str)
            else {
                return Err(format!("{kind}/{id} manifest has no package name"));
            };
            audit_manifest(name, &text, own_kit(&kind))?;
            names.push((name.to_owned(), child.path()));
        }
    }
    if names.is_empty() && !data_only {
        return Err(format!(
            "{} exists but contains no plugin packages",
            plugins.display()
        ));
    }
    names.sort();
    names.dedup();
    Ok(names)
}

fn own_kit(kind: &str) -> Option<&'static str> {
    match kind {
        "providers" => Some("hofund-kit-upstream"),
        "subscriptions" => Some("hofund-kit-subscription"),
        "agents" => Some("hofund-kit-agent"),
        _ => None,
    }
}

fn audit_manifest(package: &str, text: &str, own_kit: Option<&str>) -> Result<(), String> {
    let value: toml::Value = toml::from_str(text)
        .map_err(|error| format!("{package} manifest is not valid toml: {error}"))?;
    let mut keys = std::collections::BTreeMap::new();
    collect_dep_keys(&value, &mut keys);
    for (key, name) in &keys {
        if name == "hofund-plugin-api" && key != "hofund-plugin-api" {
            return Err(format!("{package} renames hofund-plugin-api to {key}"));
        }
        if name == "tokio" && key != "tokio" {
            return Err(format!("{package} renames tokio to {key}"));
        }
        if key == "std" || name == "std" {
            return Err(format!("{package} depends on a std shim"));
        }
    }
    let errors = boundary::manifest_dependency_errors(text, own_kit)?;
    if !errors.is_empty() {
        return Err(format!("{package} {}", errors.join("; ")));
    }
    if feature_forwards_kernel(&value, &keys) {
        return Err(format!(
            "{package} enables the hofund-plugin-api kernel feature"
        ));
    }
    if !boundary::lints_follow_workspace(text) {
        return Err(format!("{package} must set [lints] workspace = true"));
    }
    Ok(())
}

fn collect_dep_keys(value: &toml::Value, keys: &mut std::collections::BTreeMap<String, String>) {
    let Some(table) = value.as_table() else {
        return;
    };
    for (section, child) in table {
        if matches!(
            section.as_str(),
            "dependencies" | "dev-dependencies" | "build-dependencies"
        ) {
            push_dep_keys(child, keys);
        } else {
            collect_dep_keys(child, keys);
        }
    }
}

fn push_dep_keys(value: &toml::Value, keys: &mut std::collections::BTreeMap<String, String>) {
    let Some(table) = value.as_table() else {
        return;
    };
    for (key, spec) in table {
        match spec {
            toml::Value::String(_) => {
                keys.insert(key.clone(), key.clone());
            }
            toml::Value::Table(body) => {
                let name = body
                    .get("package")
                    .and_then(toml::Value::as_str)
                    .unwrap_or(key);
                let enables_kernel = body
                    .get("features")
                    .and_then(toml::Value::as_array)
                    .is_some_and(|items| {
                        items
                            .iter()
                            .filter_map(toml::Value::as_str)
                            .any(|feature| matches!(feature, "kernel" | "kernel-internal"))
                    });
                if name == "hofund-plugin-api" && enables_kernel {
                    keys.insert("__kernel__".to_owned(), "direct".to_owned());
                }
                keys.insert(key.clone(), name.to_owned());
            }
            toml::Value::Array(_)
            | toml::Value::Integer(_)
            | toml::Value::Float(_)
            | toml::Value::Boolean(_)
            | toml::Value::Datetime(_) => {}
        }
    }
}

fn feature_forwards_kernel(
    value: &toml::Value,
    keys: &std::collections::BTreeMap<String, String>,
) -> bool {
    if keys.contains_key("__kernel__") {
        return true;
    }
    let Some(features) = value.get("features").and_then(toml::Value::as_table) else {
        return false;
    };
    features.keys().any(|name| {
        feature_list_enables_kernel(name, features, keys, &mut std::collections::BTreeSet::new())
    })
}

fn feature_list_enables_kernel(
    name: &str,
    features: &toml::map::Map<String, toml::Value>,
    keys: &std::collections::BTreeMap<String, String>,
    seen: &mut std::collections::BTreeSet<String>,
) -> bool {
    if !seen.insert(name.to_owned()) {
        return false;
    }
    let Some(items) = features.get(name).and_then(toml::Value::as_array) else {
        return false;
    };
    for item in items {
        let Some(text) = item.as_str() else {
            continue;
        };
        if is_kernel_forward(text, keys)
            || (!text.contains('/') && feature_list_enables_kernel(text, features, keys, seen))
        {
            return true;
        }
    }
    false
}

fn is_kernel_forward(text: &str, keys: &std::collections::BTreeMap<String, String>) -> bool {
    let text = text.strip_prefix("dep:").unwrap_or(text);
    let Some((dep, feature)) = text.split_once('/') else {
        return false;
    };
    let dep = dep.strip_suffix('?').unwrap_or(dep);
    let package = keys.get(dep).map(String::as_str).unwrap_or(dep);
    package == "hofund-plugin-api" && matches!(feature, "kernel" | "kernel-internal")
}

fn resolved_kernel_feature(package: &str) -> Result<bool, String> {
    let output = Command::new("cargo")
        .args([
            "tree",
            "-p",
            package,
            "-e",
            "normal,features",
            "-i",
            "hofund-plugin-api",
            "--prefix",
            "none",
            "--target",
            "all",
        ])
        .current_dir(workspace_root())
        .output()
        .map_err(|error| format!("cargo tree -p {package}: {error}"))?;
    if !output.status.success() {
        return Err(format!(
            "cargo tree -p {package} failed: {}",
            String::from_utf8_lossy(&output.stderr)
        ));
    }
    let stdout = String::from_utf8_lossy(&output.stdout);
    Ok(stdout.lines().any(|line| {
        line.contains("hofund-plugin-api feature \"kernel\"")
            || line.contains("hofund-plugin-api feature \"kernel-internal\"")
    }))
}

fn check_plugins() -> ExitCode {
    let plugins = workspace_root().join("plugins");
    if !plugins.is_absolute() {
        eprintln!("check-plugins: plugins path is not absolute");
        return ExitCode::from(1);
    }
    let packages = match load_plugin_packages(&plugins) {
        Ok(packages) if packages.is_empty() => {
            println!("no plugin packages to check");
            return ExitCode::SUCCESS;
        }
        Ok(packages) => packages,
        Err(error) => {
            eprintln!("check-plugins: {error}");
            return ExitCode::from(1);
        }
    };
    for (package, dir) in &packages {
        // Scan before any Cargo command. `cargo check` and Clippy both run
        // `build.rs`, so a script must be rejected here or it executes.
        let hits = boundary::source_violations(dir);
        if !hits.is_empty() {
            eprintln!("{package} {}", hits.join("; "));
            return ExitCode::from(1);
        }
        println!("cargo check -p {package}");
        let status = Command::new("cargo")
            .args(["check", "-p", package])
            .current_dir(workspace_root())
            .status();
        match status {
            Ok(status) if status.success() => {}
            Ok(status) => {
                eprintln!("cargo check -p {package} failed ({status})");
                return ExitCode::from(1);
            }
            Err(error) => {
                eprintln!("could not run cargo check -p {package}: {error}");
                return ExitCode::from(1);
            }
        }
        if let Err(error) = clippy_plugin(package, dir) {
            eprintln!("{error}");
            return ExitCode::from(1);
        }
        if let Err(error) = boundary::target_roots_under_src(&workspace_root(), dir) {
            eprintln!("{package} {error}");
            return ExitCode::from(1);
        }
        if let Err(error) = boundary::scan_compiled_sources(&workspace_root(), dir) {
            eprintln!("{package} {error}");
            return ExitCode::from(1);
        }
        match resolved_kernel_feature(package) {
            Ok(false) => {}
            Ok(true) => {
                eprintln!("{package} resolves hofund-plugin-api with the kernel feature");
                return ExitCode::from(1);
            }
            Err(error) => {
                eprintln!("{error}");
                return ExitCode::from(1);
            }
        }
        if let Err(error) = boundary::link_allowance(&workspace_root(), package) {
            eprintln!("{error}");
            return ExitCode::from(1);
        }
    }
    ExitCode::SUCCESS
}

fn clippy_plugin(package: &str, dir: &std::path::Path) -> Result<(), String> {
    let manifest = std::fs::read_to_string(dir.join("Cargo.toml")).unwrap_or_default();
    let defines_features = boundary::manifest_defines_features(&manifest);
    for (profile, features) in boundary::clippy_passes(defines_features) {
        let feature_label = if features.is_empty() {
            "default features"
        } else {
            features
        };
        println!("cargo clippy -p {package} --profile {profile} ({feature_label})");
        let mut command = Command::new("cargo");
        command.args([
            "clippy",
            "-p",
            package,
            "--all-targets",
            "--no-deps",
            "--profile",
            profile,
        ]);
        if !features.is_empty() {
            command.arg(features);
        }
        let status = command
            .arg("--")
            .args(boundary::CLIPPY_ARGS)
            .env(
                "CLIPPY_CONF_DIR",
                boundary::clippy_conf_dir(&workspace_root()),
            )
            .current_dir(workspace_root())
            .status()
            .map_err(|error| format!("could not run cargo clippy -p {package}: {error}"))?;
        if !status.success() {
            return Err(format!(
                "cargo clippy -p {package} --profile {profile} ({feature_label}) failed the plugin boundary ({status})"
            ));
        }
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::{load_plugin_packages, resolved_kernel_feature, workspace_root};

    #[test]
    fn data_only_agents_pass_but_empty_agent_directories_fail() {
        let root = std::env::temp_dir().join(format!(
            "hofund-data-only-agent-packages-{}",
            std::process::id()
        ));
        let _ = std::fs::remove_dir_all(&root);
        let plugins = root.join("plugins");
        std::fs::create_dir_all(plugins.join("agents/example")).expect("agent");
        std::fs::write(
            plugins.join("agents/example/agent.toml"),
            "id = \"example\"\n",
        )
        .expect("agent manifest");
        assert!(
            load_plugin_packages(&plugins)
                .expect("data-only")
                .is_empty()
        );

        std::fs::create_dir_all(plugins.join("agents/empty")).expect("empty agent");
        let error = load_plugin_packages(&plugins).expect_err("empty agent");
        assert!(error.contains("agent.toml or plugin.toml"), "{error}");
        let _ = std::fs::remove_dir_all(&root);
    }

    #[test]
    fn data_only_builtin_provider_directories_pass() {
        let root = std::env::temp_dir().join(format!(
            "hofund-data-only-provider-packages-{}",
            std::process::id()
        ));
        let _ = std::fs::remove_dir_all(&root);
        let plugins = root.join("plugins");
        std::fs::create_dir_all(plugins.join("providers/openai-compat")).expect("provider");
        std::fs::write(
            plugins.join("providers/openai-compat/presets.toml"),
            "[[preset]]\nid = \"example\"\n",
        )
        .expect("presets");
        assert!(
            load_plugin_packages(&plugins)
                .expect("data-only")
                .is_empty()
        );
        std::fs::create_dir_all(plugins.join("providers/other")).expect("other");
        std::fs::write(plugins.join("providers/other/presets.toml"), "").expect("presets");
        let error = load_plugin_packages(&plugins).expect_err("non-builtin directory");
        assert!(error.contains("not a builtin factory"), "{error}");
        let _ = std::fs::remove_dir_all(&root);
    }

    #[test]
    fn plugin_packages_fail_closed() {
        let root =
            std::env::temp_dir().join(format!("hofund-plugin-packages-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&root);
        let plugin = root.join("plugins/providers/example");
        std::fs::create_dir_all(&plugin).expect("dir");
        std::fs::write(
            plugin.join("Cargo.toml"),
            "[package]\nname = \"hofund-plugin-example\"\n[dependencies]\nhofund-plugin-api = { workspace = true }\n[lints]\nworkspace = true\n",
        )
        .expect("manifest");
        let names = load_plugin_packages(&root.join("plugins")).expect("packages");
        let bare = root.join("plugins-bare");
        std::fs::create_dir_all(bare.join("providers/bare")).expect("bare");
        std::fs::write(
            bare.join("providers/bare/Cargo.toml"),
            "[package]\nname = \"bare\"\n[dependencies]\nhofund-plugin-api = { workspace = true }\n",
        )
        .expect("bare manifest");
        let bare_error = load_plugin_packages(&bare).expect_err("lints");
        assert!(
            bare_error.contains("must set [lints] workspace = true"),
            "{bare_error}"
        );
        assert_eq!(
            names
                .iter()
                .map(|(name, _)| name.as_str())
                .collect::<Vec<_>>(),
            ["hofund-plugin-example"]
        );

        let missing = root.join("missing");
        assert!(
            load_plugin_packages(&missing)
                .expect("missing dir")
                .is_empty(),
            "a missing plugins directory may succeed"
        );

        let empty = root.join("empty");
        std::fs::create_dir_all(&empty).expect("empty");
        assert!(
            load_plugin_packages(&empty).is_err(),
            "an existing plugins directory with zero packages must fail"
        );

        let unknown = root.join("unknown");
        std::fs::create_dir_all(unknown.join("unknownkind/x")).expect("unknown");
        std::fs::write(
            unknown.join("unknownkind/x/Cargo.toml"),
            "[package]\nname = \"x\"\n",
        )
        .expect("unknown manifest");
        let unknown_error = load_plugin_packages(&unknown).expect_err("unknown kind");
        assert!(
            unknown_error.contains("unknown plugin kind"),
            "{unknown_error}"
        );

        let broken = root.join("broken");
        std::fs::create_dir_all(broken.join("providers/bad")).expect("broken");
        std::fs::write(broken.join("providers/bad/Cargo.toml"), "not toml [\n").expect("toml");
        let broken_error = load_plugin_packages(&broken).expect_err("bad toml");
        assert!(broken_error.contains("not valid toml"), "{broken_error}");

        let renamed = root.join("renamed");
        std::fs::create_dir_all(renamed.join("providers/api")).expect("renamed");
        std::fs::write(
            renamed.join("providers/api/Cargo.toml"),
            "[package]\nname = \"renamed\"\n[dependencies]\napi = { package = \"hofund-plugin-api\" }\n[features]\nfast = [\"api/kernel\"]\n",
        )
        .expect("renamed manifest");
        let renamed_error = load_plugin_packages(&renamed).expect_err("rename");
        assert!(
            renamed_error.contains("renames hofund-plugin-api"),
            "{renamed_error}"
        );

        let weak = root.join("weak");
        std::fs::create_dir_all(weak.join("providers/weak")).expect("weak");
        std::fs::write(
            weak.join("providers/weak/Cargo.toml"),
            "[package]\nname = \"weak\"\n[dependencies]\nhofund-plugin-api = { workspace = true, optional = true }\n[features]\nfast = [\"hofund-plugin-api?/kernel\"]\n",
        )
        .expect("weak manifest");
        let weak_error = load_plugin_packages(&weak).expect_err("weak");
        assert!(weak_error.contains("kernel"), "{weak_error}");

        let internal = root.join("internal");
        std::fs::create_dir_all(internal.join("providers/internal")).expect("internal");
        std::fs::write(
            internal.join("providers/internal/Cargo.toml"),
            "[package]\nname = \"internal\"\n[dependencies]\nhofund-plugin-api = { workspace = true, features = [\"kernel-internal\"] }\n",
        )
        .expect("internal manifest");
        let internal_error = load_plugin_packages(&internal).expect_err("internal");
        assert!(internal_error.contains("kernel"), "{internal_error}");

        let tokio = root.join("tokio");
        std::fs::create_dir_all(tokio.join("providers/tk")).expect("tokio");
        std::fs::write(
            tokio.join("providers/tk/Cargo.toml"),
            "[package]\nname = \"tk-plugin\"\n[dependencies]\ntk = { package = \"tokio\" }\n",
        )
        .expect("tokio manifest");
        let tokio_error = load_plugin_packages(&tokio).expect_err("tokio rename");
        assert!(tokio_error.contains("renames tokio"), "{tokio_error}");

        let registry = root.join("registry");
        std::fs::create_dir_all(registry.join("registry/evil")).expect("registry");
        std::fs::write(
            registry.join("registry/evil/Cargo.toml"),
            "[package]\nname = \"evil\"\n",
        )
        .expect("registry manifest");
        let registry_error = load_plugin_packages(&registry).expect_err("registry");
        assert!(
            registry_error.contains("plugins/registry must not contain a package"),
            "{registry_error}"
        );

        let composition = root.join("composition");
        std::fs::create_dir_all(composition.join("registry")).expect("composition");
        std::fs::write(
            composition.join("registry/Cargo.toml"),
            "[package]\nname = \"hofund-plugin-registry\"\n",
        )
        .expect("composition manifest");
        std::fs::create_dir_all(composition.join("providers/ok")).expect("ok plugin");
        std::fs::write(
            composition.join("providers/ok/Cargo.toml"),
            "[package]\nname = \"ok\"\n[dependencies]\nhofund-plugin-api = { workspace = true }\n[lints]\nworkspace = true\n",
        )
        .expect("ok manifest");
        let names = load_plugin_packages(&composition).expect("registry root is not a plugin");
        assert_eq!(names.len(), 1);

        assert!(
            workspace_root().join("plugins").is_absolute(),
            "check-plugins resolves plugins from the workspace root"
        );
        let _ = std::fs::remove_dir_all(&root);
    }

    #[test]
    fn cargo_tree_sees_the_kernel_feature_per_package() {
        assert!(
            resolved_kernel_feature("hofund-plugin-host").expect("host tree"),
            "the host enables the kernel feature"
        );
        assert!(
            !resolved_kernel_feature("hofund-provider").expect("provider tree"),
            "another workspace member must not inherit that feature"
        );
        assert!(
            !resolved_kernel_feature("hofund-plugin-fixture").expect("fixture tree"),
            "the fixture plugin must compile without the kernel feature"
        );
    }
}
