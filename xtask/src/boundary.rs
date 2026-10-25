//! Cargo and source boundary for plugin packages.
//!
//! Plugins may depend only on `hofund-plugin-api` and `hofund-plugin-sdk`
//! (plus their own test kit). Clippy, configured by
//! `xtask/plugin-boundary/clippy.toml`, is what sees a macro fragment such as
//! `std::$m::Command` after expansion. This module does not try to match those
//! splices in source text.

use std::collections::BTreeSet;
use std::fs;
use std::path::{Component, Path, PathBuf};
use std::process::Command;

use syn::visit::Visit;

const ALLOWED_DEPS: &[&str] = &["hofund-plugin-api", "hofund-plugin-sdk"];
const TOKIO_BANNED_FEATURES: &[&str] = &["full", "net", "process", "signal"];

/// Why a dependency is outside the plugin allow-list, if it is.
#[must_use]
pub fn dependency_error(
    name: &str,
    features: &[String],
    dev: bool,
    own_kit: Option<&str>,
) -> Option<String> {
    if name == "tokio" {
        let banned: Vec<&str> = TOKIO_BANNED_FEATURES
            .iter()
            .copied()
            .filter(|feature| features.iter().any(|item| item == feature))
            .collect();
        if !banned.is_empty() {
            let text = banned
                .iter()
                .map(|feature| format!("enables tokio feature {feature}"))
                .collect::<Vec<_>>()
                .join("; ");
            return Some(text);
        }
    }
    if ALLOWED_DEPS.contains(&name) {
        return None;
    }
    if dev && own_kit == Some(name) {
        return None;
    }
    Some(format!("depends on {name}"))
}

/// Dependency allow-list failures in one manifest.
///
/// # Errors
///
/// Returns an error when the manifest is not TOML.
pub fn manifest_dependency_errors(
    text: &str,
    own_kit: Option<&str>,
) -> Result<Vec<String>, String> {
    let value: toml::Value =
        toml::from_str(text).map_err(|error| format!("manifest is not valid toml: {error}"))?;
    let mut errors = Vec::new();
    collect_dependency_errors(&value, own_kit, &mut errors);
    errors.sort();
    errors.dedup();
    Ok(errors)
}

fn collect_dependency_errors(value: &toml::Value, own_kit: Option<&str>, errors: &mut Vec<String>) {
    let Some(table) = value.as_table() else {
        return;
    };
    for (section, child) in table {
        let dev = section == "dev-dependencies";
        let is_deps = dev
            || section == "dependencies"
            || section == "build-dependencies"
            || section.ends_with(".dependencies")
            || section.ends_with(".dev-dependencies")
            || section.ends_with(".build-dependencies");
        if is_deps {
            push_dependency_errors(
                child,
                dev || section.contains("dev-dependencies"),
                own_kit,
                errors,
            );
        } else {
            collect_dependency_errors(child, own_kit, errors);
        }
    }
}

fn push_dependency_errors(
    value: &toml::Value,
    dev: bool,
    own_kit: Option<&str>,
    errors: &mut Vec<String>,
) {
    let Some(table) = value.as_table() else {
        return;
    };
    for (key, spec) in table {
        let (name, features) = match spec {
            toml::Value::String(_) => (key.clone(), Vec::new()),
            toml::Value::Table(body) => {
                let name = body
                    .get("package")
                    .and_then(toml::Value::as_str)
                    .unwrap_or(key)
                    .to_owned();
                let features = body
                    .get("features")
                    .and_then(toml::Value::as_array)
                    .map(|items| {
                        items
                            .iter()
                            .filter_map(toml::Value::as_str)
                            .map(str::to_owned)
                            .collect()
                    })
                    .unwrap_or_default();
                (name, features)
            }
            _ => continue,
        };
        if let Some(error) = dependency_error(&name, &features, dev, own_kit) {
            errors.push(error);
        }
    }
}

/// `#[path]` escapes and non-literal includes under a plugin package.
#[must_use]
pub fn source_violations(package_dir: &Path) -> Vec<String> {
    let mut hits = Vec::new();
    let src = package_dir.join("src");
    if src.is_dir() {
        for file in rust_files(&src) {
            hits.extend(file_violations(&file, &src));
        }
    }
    let build = package_dir.join("build.rs");
    if build.is_file() {
        hits.extend(file_violations(&build, package_dir));
    }
    hits.sort();
    hits.dedup();
    hits
}

fn file_violations(file: &Path, root: &Path) -> Vec<String> {
    let Ok(text) = fs::read_to_string(file) else {
        return vec![format!("unreadable {}", file.display())];
    };
    let Ok(parsed) = syn::parse_file(&text) else {
        return vec![format!("{} does not parse", file.display())];
    };
    let mut finder = SourceFinder {
        file,
        root,
        hits: Vec::new(),
    };
    finder.visit_file(&parsed);
    finder.hits
}

struct SourceFinder<'a> {
    file: &'a Path,
    root: &'a Path,
    hits: Vec<String>,
}

impl<'ast> Visit<'ast> for SourceFinder<'_> {
    fn visit_item_mod(&mut self, item: &'ast syn::ItemMod) {
        for attr in &item.attrs {
            if let Some(rel) = path_attribute(attr)
                && (rel.is_empty() || escapes(self.file, &rel, self.root))
            {
                self.hits
                    .push(format!("#[path] = \"{rel}\" leaves the package source"));
            }
        }
        syn::visit::visit_item_mod(self, item);
    }

    fn visit_macro(&mut self, mac: &'ast syn::Macro) {
        if let Some(name) = mac
            .path
            .segments
            .last()
            .map(|segment| segment.ident.to_string())
            && matches!(name.as_str(), "include" | "include_str" | "include_bytes")
            && syn::parse2::<syn::LitStr>(mac.tokens.clone()).is_err()
        {
            self.hits.push(format!("non-literal {name}!"));
        }
        syn::visit::visit_macro(self, mac);
    }

    fn visit_path(&mut self, path: &'ast syn::Path) {
        let segments: Vec<String> = path
            .segments
            .iter()
            .map(|segment| segment.ident.to_string())
            .collect();
        if let Some(hit) = forbidden_path(&segments) {
            self.hits.push(format!("uses {hit}"));
        }
        syn::visit::visit_path(self, path);
    }
}

fn path_attribute(attr: &syn::Attribute) -> Option<String> {
    if !attr.path().is_ident("path") {
        return None;
    }
    if let syn::Meta::NameValue(name) = &attr.meta
        && let syn::Expr::Lit(expr) = &name.value
        && let syn::Lit::Str(lit) = &expr.lit
    {
        return Some(lit.value());
    }
    attr.parse_args::<syn::LitStr>()
        .ok()
        .map(|lit| lit.value())
        .or(Some(String::new()))
}

fn escapes(file: &Path, rel: &str, root: &Path) -> bool {
    let rel_path = Path::new(rel);
    if rel_path.is_absolute() {
        return true;
    }
    let Some(parent) = file.parent() else {
        return true;
    };
    let joined = normalize(&parent.join(rel_path));
    let root = normalize(root);
    !joined.starts_with(&root)
}

fn normalize(path: &Path) -> PathBuf {
    let mut out = PathBuf::new();
    for component in path.components() {
        match component {
            Component::ParentDir => {
                out.pop();
            }
            Component::CurDir => {}
            other => out.push(other.as_os_str()),
        }
    }
    out
}

fn forbidden_path(segments: &[String]) -> Option<&'static str> {
    if segments.len() >= 2 && segments[0] == "std" && segments[1] == "process" {
        return Some("std::process");
    }
    if segments.len() >= 2 && segments[0] == "std" && segments[1] == "net" {
        return Some("std::net");
    }
    if segments.len() >= 4 && segments[0] == "std" && segments[1] == "os" {
        let family = segments[2].as_str();
        let module = segments[3].as_str();
        if matches!(family, "unix" | "windows") && matches!(module, "process" | "net") {
            return Some(match (family, module) {
                ("unix", "process") => "std::os::unix::process",
                ("unix", "net") => "std::os::unix::net",
                ("windows", "process") => "std::os::windows::process",
                _ => "std::os::windows::net",
            });
        }
    }
    if segments.len() >= 2
        && segments[0] == "tokio"
        && matches!(segments[1].as_str(), "process" | "net")
    {
        return if segments[1] == "process" {
            Some("tokio::process")
        } else {
            Some("tokio::net")
        };
    }
    match segments.first().map(String::as_str) {
        Some("libc") => Some("libc"),
        Some("nix") => Some("nix"),
        Some("reqwest") => Some("reqwest"),
        Some("hyper") => Some("hyper"),
        Some("ureq" | "socket2" | "mio" | "rustix" | "async_process" | "duct" | "curl") => {
            Some("a socket or process crate")
        }
        _ => None,
    }
}

fn rust_files(dir: &Path) -> Vec<PathBuf> {
    let mut files = Vec::new();
    let Ok(entries) = fs::read_dir(dir) else {
        return files;
    };
    for entry in entries.flatten() {
        let path = entry.path();
        if path.is_dir() {
            files.extend(rust_files(&path));
        } else if path.extension().and_then(|ext| ext.to_str()) == Some("rs") {
            files.push(path);
        }
    }
    files
}

/// Directory of `xtask/plugin-boundary/clippy.toml`.
#[must_use]
pub fn clippy_conf_dir(workspace: &Path) -> PathBuf {
    workspace.join("xtask/plugin-boundary")
}

/// Package names in `cargo tree -p <package> --target all`.
///
/// # Errors
///
/// Returns an error when Cargo cannot build the tree.
pub fn linked_packages(workspace: &Path, package: &str) -> Result<BTreeSet<String>, String> {
    let output = Command::new("cargo")
        .args(["tree", "-p", package, "--prefix", "none", "--target", "all"])
        .current_dir(workspace)
        .output()
        .map_err(|error| format!("cargo tree -p {package}: {error}"))?;
    if !output.status.success() {
        return Err(format!(
            "cargo tree -p {package} failed: {}",
            String::from_utf8_lossy(&output.stderr)
        ));
    }
    let mut names = BTreeSet::new();
    for line in String::from_utf8_lossy(&output.stdout).lines() {
        let Some(name) = line.split_whitespace().next() else {
            continue;
        };
        if name.chars().all(|character| {
            character.is_ascii_alphanumeric() || character == '-' || character == '_'
        }) {
            names.insert(name.to_owned());
        }
    }
    if names.is_empty() {
        return Err(format!("cargo tree -p {package} listed no packages"));
    }
    Ok(names)
}

/// The plugin's link closure must be the API and SDK closures, plus itself.
///
/// # Errors
///
/// Returns an error when Cargo fails or the plugin links anything else.
pub fn link_allowance(workspace: &Path, package: &str) -> Result<(), String> {
    let mut allowed = linked_packages(workspace, "hofund-plugin-api")?;
    allowed.extend(linked_packages(workspace, "hofund-plugin-sdk")?);
    allowed.insert(package.to_owned());
    let linked = linked_packages(workspace, package)?;
    let extra: Vec<&String> = linked.difference(&allowed).collect();
    if extra.is_empty() {
        Ok(())
    } else {
        Err(format!("{package} links {}", extra_list(&extra)))
    }
}

fn extra_list(names: &[&String]) -> String {
    names
        .iter()
        .map(|name| name.as_str())
        .collect::<Vec<_>>()
        .join(", ")
}

/// True when `plugins/registry` contains a Cargo manifest.
///
/// # Errors
///
/// Returns an error when the directory cannot be read.
pub fn registry_has_package(dir: &Path) -> Result<bool, String> {
    if !dir.exists() {
        return Ok(false);
    }
    contains_manifest(dir)
}

fn contains_manifest(dir: &Path) -> Result<bool, String> {
    let entries = fs::read_dir(dir).map_err(|error| format!("read {}: {error}", dir.display()))?;
    for entry in entries {
        let entry = entry.map_err(|error| format!("read {}: {error}", dir.display()))?;
        let path = entry.path();
        if path.is_dir() {
            if contains_manifest(&path)? {
                return Ok(true);
            }
        } else if entry.file_name() == "Cargo.toml" {
            return Ok(true);
        }
    }
    Ok(false)
}

#[cfg(test)]
mod tests {
    use super::{dependency_error, manifest_dependency_errors, source_violations};

    #[test]
    fn the_allow_list_rejects_sockets_process_helpers_and_tokio_net() {
        let text = r#"
[dependencies]
hofund-plugin-api = { workspace = true }
ureq = "2"
socket2 = "0.5"
mio = "1"
rustix = "0.38"
async-process = "2"
duct = "0.13"
curl = "0.4"
tokio = { version = "1", features = ["rt", "net", "process"] }
[build-dependencies]
cc = "1"
"#;
        let errors = manifest_dependency_errors(text, Some("hofund-kit-upstream"))
            .expect("toml")
            .join("\n");
        for needle in [
            "depends on ureq",
            "depends on socket2",
            "depends on mio",
            "depends on rustix",
            "depends on async-process",
            "depends on duct",
            "depends on curl",
            "enables tokio feature net",
            "enables tokio feature process",
            "depends on cc",
        ] {
            assert!(errors.contains(needle), "{needle} missing in:\n{errors}");
        }
        assert_eq!(
            dependency_error("hofund-plugin-sdk", &[], false, None),
            None
        );
        assert_eq!(
            dependency_error(
                "hofund-kit-upstream",
                &[],
                true,
                Some("hofund-kit-upstream")
            ),
            None
        );
    }

    #[test]
    fn path_attributes_and_computed_includes_are_rejected() {
        let root =
            std::env::temp_dir().join(format!("hofund-plugin-boundary-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&root);
        let src = root.join("src");
        std::fs::create_dir_all(src.join("hidden")).expect("src");
        std::fs::write(
            src.join("lib.rs"),
            "#[path = \"../hidden/net.rs\"]\nmod net;\ninclude!(concat!(env!(\"CARGO_MANIFEST_DIR\"), \"/hidden/net.rs\"));\n",
        )
        .expect("lib");
        std::fs::write(src.join("hidden/place.rs"), "fn inside() {}\n").expect("inside");
        std::fs::write(
            root.join("build.rs"),
            "fn main() { let _ = std::process::Command::new(\"true\"); }\n",
        )
        .expect("build");
        let hits = source_violations(&root).join("\n");
        assert!(hits.contains("#[path]"), "{hits}");
        assert!(hits.contains("non-literal include!"), "{hits}");
        assert!(hits.contains("std::process"), "{hits}");
        let _ = std::fs::remove_dir_all(&root);
    }

    #[test]
    fn a_macro_fragment_is_denied_after_expansion() {
        let root = std::env::temp_dir().join(format!("hofund-plugin-macro-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&root);
        std::fs::create_dir_all(root.join("src")).expect("src");
        std::fs::write(
            root.join("Cargo.toml"),
            "[package]\nname = \"probe\"\nversion = \"0.1.0\"\nedition = \"2021\"\n",
        )
        .expect("manifest");
        std::fs::write(
            root.join("src/lib.rs"),
            "macro_rules! spawn { ($m:ident) => { std::$m::Command::new(\"true\").status() }; }\npub fn sneak() { let _ = spawn!(process); }\n",
        )
        .expect("source");
        let workspace = PathBuf::from(env!("CARGO_MANIFEST_DIR"))
            .parent()
            .expect("workspace")
            .to_path_buf();
        let status = std::process::Command::new("cargo")
            .args([
                "clippy",
                "--offline",
                "--manifest-path",
                root.join("Cargo.toml").to_str().expect("utf-8"),
                "--",
                "-D",
                "warnings",
                "-D",
                "clippy::disallowed_types",
                "-D",
                "clippy::disallowed_methods",
            ])
            .env("CLIPPY_CONF_DIR", super::clippy_conf_dir(&workspace))
            .env("CARGO_TARGET_DIR", root.join("target"))
            .env_remove("CARGO_MANIFEST_DIR")
            .status()
            .expect("clippy");
        assert!(
            !status.success(),
            "std::$m::Command must fail after expansion"
        );
        let _ = std::fs::remove_dir_all(&root);
    }

    use std::path::PathBuf;
}
