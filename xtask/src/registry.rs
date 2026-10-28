//! Reads registry constants without duplicating them in maintenance checks.

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

#[cfg(test)]
mod tests {
    use super::builtin_factory_ids;

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
