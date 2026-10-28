//! Reads registry constants without duplicating them in maintenance checks.

use std::collections::BTreeSet;
use std::path::Path;

struct ArrayFinder<'a>(Option<&'a syn::ExprArray>);

impl<'ast> syn::visit::Visit<'ast> for ArrayFinder<'ast> {
    fn visit_expr_array(&mut self, array: &'ast syn::ExprArray) {
        if self.0.is_none() {
            self.0 = Some(array);
        }
    }
}

/// Reads the builtin factory ids declared by the registry's shared build checks.
///
/// # Errors
///
/// Returns an error when the source cannot be read or the function and its
/// string-literal array cannot be found and parsed.
pub fn builtin_factory_ids(workspace_root: &Path) -> Result<Vec<String>, String> {
    let path = workspace_root.join("plugins/registry/src/check.rs");
    let source = std::fs::read_to_string(&path)
        .map_err(|error| format!("read {}: {error}", path.display()))?;
    let file: syn::File =
        syn::parse_str(&source).map_err(|error| format!("parse {}: {error}", path.display()))?;
    let function = file
        .items
        .iter()
        .find_map(|item| match item {
            syn::Item::Fn(function) if function.sig.ident == "builtin_factory_ids" => {
                Some(function)
            }
            _ => None,
        })
        .ok_or_else(|| format!("{} has no builtin_factory_ids function", path.display()))?;
    let mut finder = ArrayFinder(None);
    syn::visit::Visit::visit_block(&mut finder, &function.block);
    let array = finder
        .0
        .ok_or_else(|| format!("{} has no builtin factory array", path.display()))?;
    let mut ids = Vec::new();
    for element in &array.elems {
        let syn::Expr::Lit(syn::ExprLit {
            lit: syn::Lit::Str(id),
            ..
        }) = element
        else {
            return Err(format!(
                "{} builtin factory array must contain only string literals",
                path.display()
            ));
        };
        ids.push(id.value());
    }
    if ids.is_empty() {
        return Err(format!("{} builtin factory array is empty", path.display()));
    }
    Ok(ids)
}

/// Checks per-kind plugin ids and globally unique Cargo package and feature names.
///
/// # Errors
///
/// Returns a descriptive error for malformed rows, duplicate ids within one
/// kind, reserved ids, duplicate build identifiers, or unsorted rows.
pub fn check_plugin_list(text: &str) -> Result<(), String> {
    let value: toml::Value = toml::from_str(text).map_err(|error| error.to_string())?;
    let plugins = value
        .get("plugin")
        .and_then(toml::Value::as_array)
        .ok_or_else(|| "plugins.toml has no [[plugin]] tables".to_owned())?;
    let mut kind_ids = BTreeSet::new();
    let mut packages = BTreeSet::new();
    let mut features = BTreeSet::new();
    let mut ordered = Vec::new();
    for plugin in plugins {
        let field = |name: &str| {
            plugin
                .get(name)
                .and_then(toml::Value::as_str)
                .ok_or_else(|| format!("plugin row is missing {name}"))
        };
        let id = field("id")?;
        let kind = field("kind")?;
        let package = field("package")?;
        let feature = field("feature")?;
        if !matches!(kind, "provider" | "subscription" | "agent" | "inbound") {
            return Err(format!("unknown plugin kind {kind}"));
        }
        if matches!(id, "api" | "sdk" | "host" | "registry") {
            return Err(format!("{id} is a reserved plugin id"));
        }
        if !kind_ids.insert((kind, id)) {
            return Err(format!("duplicate plugin id {id} in kind {kind}"));
        }
        if !packages.insert(package) {
            return Err(format!("duplicate plugin package {package}"));
        }
        if !features.insert(feature) {
            return Err(format!("duplicate plugin feature {feature}"));
        }
        ordered.push((id, kind));
    }
    if ordered.windows(2).any(|pair| pair[0] >= pair[1]) {
        return Err("plugins.toml is not sorted by id and kind".to_owned());
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::{builtin_factory_ids, check_plugin_list};

    fn row(id: &str, kind: &str, package: &str, feature: &str) -> String {
        format!(
            "[[plugin]]\nid = \"{id}\"\nkind = \"{kind}\"\npath = \"unused\"\npackage = \"{package}\"\nfeature = \"{feature}\"\n\n"
        )
    }

    #[test]
    fn plugin_list_allows_one_id_in_different_kinds() {
        let text = [
            row(
                "shared",
                "agent",
                "hofund-plugin-agent-shared",
                "agent-shared",
            ),
            row(
                "shared",
                "subscription",
                "hofund-plugin-subscription-shared",
                "subscription-shared",
            ),
        ]
        .concat();
        check_plugin_list(&text).expect("cross-kind ids");
    }

    #[test]
    fn plugin_list_rejects_same_kind_duplicates_and_reserved_ids() {
        for kind in ["agent", "subscription"] {
            let text = [
                row("shared", kind, "first-package", "first-feature"),
                row("shared", kind, "second-package", "second-feature"),
            ]
            .concat();
            let error = check_plugin_list(&text).expect_err("duplicate");
            assert!(error.contains(&format!("duplicate plugin id shared in kind {kind}")));
        }
        for kind in ["provider", "subscription", "agent", "inbound"] {
            for id in ["api", "sdk", "host", "registry"] {
                let error = check_plugin_list(&row(id, kind, "package", "feature"))
                    .expect_err("reserved id");
                assert!(error.contains("reserved plugin id"), "{error}");
            }
        }
    }

    #[test]
    fn plugin_list_rejects_global_package_and_feature_collisions() {
        let cross_kind = |package, feature| {
            [
                row("shared", "agent", package, feature),
                row("shared", "provider", package, "other-feature"),
            ]
            .concat()
        };
        assert!(
            check_plugin_list(&cross_kind("same-package", "first-feature"))
                .expect_err("package")
                .contains("duplicate plugin package")
        );
        let text = [
            row("shared", "agent", "first-package", "same-feature"),
            row("shared", "provider", "second-package", "same-feature"),
        ]
        .concat();
        assert!(
            check_plugin_list(&text)
                .expect_err("feature")
                .contains("duplicate plugin feature")
        );
    }

    fn root_with(source: &str, name: &str) -> std::path::PathBuf {
        let root = std::env::temp_dir().join(format!(
            "hofund-xtask-registry-{name}-{}",
            std::process::id()
        ));
        let _ = std::fs::remove_dir_all(&root);
        std::fs::create_dir_all(root.join("plugins/registry/src")).expect("dirs");
        std::fs::write(root.join("plugins/registry/src/check.rs"), source).expect("source");
        root
    }

    #[test]
    fn reads_the_ids_from_the_single_source() {
        let root = root_with(
            "pub fn builtin_factory_ids() -> &'static [&'static str] {\n    &[\"one\", \"two-x\"]\n}\n",
            "ok",
        );
        assert_eq!(
            builtin_factory_ids(&root).expect("ids"),
            ["one".to_owned(), "two-x".to_owned()]
        );
        let _ = std::fs::remove_dir_all(&root);
    }

    #[test]
    fn rejects_a_missing_function_or_non_literal_ids() {
        for (name, source) in [
            ("missing", "pub fn other() {}\n"),
            (
                "ident",
                "pub fn builtin_factory_ids() -> &'static [&'static str] {\n    &[ONE]\n}\n",
            ),
            (
                "empty",
                "pub fn builtin_factory_ids() -> &'static [&'static str] {\n    &[]\n}\n",
            ),
        ] {
            let root = root_with(source, name);
            assert!(builtin_factory_ids(&root).is_err(), "{name}");
            let _ = std::fs::remove_dir_all(&root);
        }
    }
}
