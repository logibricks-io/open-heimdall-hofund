//! Syn and cargo-metadata checks for the plugin seam.
//!
//! The scan follows crate aliases, globs, type aliases, angle-bracket calls,
//! and paths inside macros. Plugin manifests are parsed as TOML, including a
//! renamed dependency, a `[dependencies.name]` table, a multi-line `features`
//! array, and `[features]` that forwards `hofund-plugin-api/kernel`.

use std::collections::{BTreeMap, BTreeSet};
use std::fs;
use std::path::Path;
use std::process::Command;

use syn::visit::Visit;

use super::{MEMBERS, read_to_string, relative, rust_files, workspace_root};

#[test]
fn plugin_seam_matches_the_contract() {
    let root = workspace_root();
    for (name, relative_path) in [
        ("hofund-plugin-api", "crates/plugin-api/src/lib.rs"),
        ("hofund-plugin-sdk", "crates/plugin-sdk/src/lib.rs"),
        ("hofund-plugin-host", "crates/plugin-host/src/lib.rs"),
    ] {
        let source = read_to_string(&root.join(relative_path));
        assert!(
            denies_lint(&source, "missing_docs"),
            "{name} must deny missing_docs in an inner attribute, not a comment"
        );
        assert!(
            denies_lint(&source, "unreachable_pub"),
            "{name} must deny unreachable_pub in an inner attribute, not a comment"
        );
    }
    let sdk = read_to_string(&root.join("crates/plugin-sdk/src/lib.rs"));
    let api = read_to_string(&root.join("crates/plugin-api/src/lib.rs"));
    assert!(
        sdk.contains("pub use tracing"),
        "the SDK re-exports tracing so a plugin does not depend on it"
    );
    assert!(
        sdk.contains("pub use hofund_quirks::apply as apply_quirks"),
        "the SDK re-exports quirk application"
    );
    assert!(
        !api.contains("hofund_quirks") && !api.contains("tracing"),
        "the API does not depend on quirks or tracing"
    );
    assert!(
        root.join("crates/plugin-api/public-api.txt").is_file(),
        "the public API snapshot is missing"
    );

    let host_users = [
        "hofund-kit-agent",
        "hofund-kit-subscription",
        "hofund-kit-upstream",
    ];
    let declared = cargo_packages();
    for (package, facts) in &declared {
        let uses_host = facts
            .dependencies
            .iter()
            .any(|dep| dep.name == "hofund-plugin-host");
        if uses_host {
            assert!(
                host_users.contains(&package.as_str()),
                "{package} must not depend on hofund-plugin-host"
            );
        }
        if facts.kernel_feature {
            assert!(
                package == "hofund-plugin-host",
                "{package} must not enable the plugin-api kernel feature"
            );
        }
    }
    for member in MEMBERS {
        let manifest = read_to_string(&root.join(member).join("Cargo.toml"));
        let name = super::package_name(&manifest);
        if name.starts_with("hofund-kit-") {
            assert!(
                !manifest.contains("publish = true"),
                "{name} is test-only and must not set publish = true"
            );
            let source = read_to_string(&root.join(member).join("src/lib.rs"));
            assert!(source.contains("test_host"), "{name} must expose test_host");
        }
    }

    let mut violations = Vec::new();
    for path in rust_files(&root) {
        let rel = relative(&root, &path);
        if rel.starts_with("target/") {
            continue;
        }
        let in_host_stack = rel.starts_with("crates/plugin-api/")
            || rel.starts_with("crates/plugin-host/")
            || rel.starts_with("kits/")
            || rel.starts_with("xtask/");
        if in_host_stack {
            continue;
        }
        let text = read_to_string(&path);
        for hit in seam_hits(&text) {
            violations.push(format!("{rel} {hit}"));
        }
    }
    for path in rust_files(&root.join("crates/router")) {
        let text = read_to_string(&path);
        for word in ["UpstreamProvider", "HostContext", "Subscription"] {
            if super::contains_word(&text, word) {
                violations.push(format!("{} names {word}", relative(&root, &path)));
            }
        }
    }
    assert!(
        violations.is_empty(),
        "plugin seam rules:\n{}",
        violations.join("\n")
    );

    let plugins = root.join("plugins");
    if plugins.is_dir() {
        let found = plugin_tree_violations(&plugins);
        assert!(
            found.is_empty(),
            "plugins/ broke the dependency rules:\n{}",
            found.join("\n")
        );
    }
}

#[test]
fn plugin_rules_reject_a_bad_tree() {
    assert!(
        !denies_lint("// unreachable_pub\nfn value() {}\n", "unreachable_pub"),
        "a comment must not satisfy the unreachable_pub check"
    );
    assert!(denies_lint(
        "#![deny(missing_docs, unreachable_pub, rust_2018_idioms)]\nfn value() {}\n",
        "unreachable_pub"
    ));

    let spaced = seam_hits("fn sneak() { hofund_plugin_api::HostContext:: from_backends (); }\n");
    assert!(
        spaced.iter().any(|hit| hit.contains("from_backends")),
        "spaced call: {spaced:?}"
    );
    let alias = seam_hits(
        "use hofund_plugin_api::HostContext::from_backends as make_host;\nfn sneak() { make_host(); }\n",
    );
    assert!(
        alias.iter().any(|hit| hit.contains("from_backends")),
        "renamed import: {alias:?}"
    );
    let grouped = seam_hits("use hofund_plugin_api::{kernel, HostContext};\n");
    assert!(
        grouped.iter().any(|hit| hit.contains("kernel")),
        "grouped kernel import: {grouped:?}"
    );
    let surface = seam_hits(
        "use hofund_plugin_api::SurfaceHost as Door;\nfn sneak() { Door::from_parts(); }\n",
    );
    assert!(
        surface
            .iter()
            .any(|hit| hit.contains("SurfaceHost::from_parts")),
        "renamed SurfaceHost: {surface:?}"
    );
    let auth = seam_hits("fn sneak() { hofund_plugin_api::AuthResult::new(true); }\n");
    assert!(
        auth.iter().any(|hit| hit.contains("AuthResult::new")),
        "AuthResult::new: {auth:?}"
    );
    let comment = seam_hits("// from_backends(\nfn clean() {}\n");
    assert!(
        comment.is_empty(),
        "a comment must not count as a call: {comment:?}"
    );

    let process = runtime_hits(
        "use std::{process, env};\nfn sneak() { let _ = process::Command::new(\"true\"); }\n",
    );
    assert!(
        process.iter().any(|hit| hit.contains("std::process")),
        "grouped std process import: {process:?}"
    );
    assert!(
        runtime_hits("use tokio::process::Command;\n")
            .iter()
            .any(|hit| hit.contains("tokio::process"))
    );
    assert!(
        runtime_hits("use std as s;\nfn sneak() { let _ = s::process::Command::new(\"true\"); }\n")
            .iter()
            .any(|hit| hit.contains("std::process")),
        "a renamed std crate must still be std::process"
    );
    assert!(
        runtime_hits("fn sneak() { let _ = vec![std::process::id()]; }\n")
            .iter()
            .any(|hit| hit.contains("std::process")),
        "a path inside a macro must count"
    );

    assert!(
        seam_hits("use hofund_plugin_api as api;\nfn sneak(value: api::kernel::HttpBackend) {}\n")
            .iter()
            .any(|hit| hit.contains("kernel")),
        "a crate alias must still see kernel"
    );
    assert!(
        seam_hits(
            "extern crate hofund_plugin_api as api;\nfn sneak(value: api::kernel::HttpBackend) {}\n"
        )
        .iter()
        .any(|hit| hit.contains("kernel")),
        "extern crate alias"
    );
    assert!(
        seam_hits("use hofund_plugin_api::*;\nfn sneak(value: kernel::HttpBackend) {}\n")
            .iter()
            .any(|hit| hit.contains("kernel")),
        "a glob must bring kernel into scope"
    );
    assert!(
        seam_hits(
            "fn sneak() { let _ = vec![hofund_plugin_api::HostContext::from_backends()]; }\n"
        )
        .iter()
        .any(|hit| hit.contains("from_backends")),
        "a call inside a macro must count"
    );
    assert!(
        seam_hits("fn sneak() { assert!(hofund_plugin_api::AuthResult::new(true)); }\n")
            .iter()
            .any(|hit| hit.contains("AuthResult::new")),
        "AuthResult::new inside a macro must count"
    );
    assert!(
        seam_hits("type Door = SurfaceHost;\nfn sneak() { let _ = Door::from_parts(); }\n")
            .iter()
            .any(|hit| hit.contains("SurfaceHost::from_parts")),
        "a type alias must count"
    );
    assert!(
        seam_hits("fn sneak() { let _ = <SurfaceHost>::from_parts(); }\n")
            .iter()
            .any(|hit| hit.contains("SurfaceHost::from_parts")),
        "an angle-bracket call must count"
    );
    assert!(
        seam_hits("fn sneak() { let _ = <hofund_plugin_api::AuthResult>::new(true); }\n")
            .iter()
            .any(|hit| hit.contains("AuthResult::new")),
        "an angle-bracket AuthResult::new must count"
    );

    let manifest = r#"
[package]
name = "hofund-plugin-openai-compat"
[dependencies]
hofund-plugin-api = { workspace = true, features = ["kernel"] }
hofund-plugin-host = { workspace = true }
hofund-ir = { workspace = true }
reqwest = "0.12"
[dev-dependencies]
hofund-kit-agent = { workspace = true }
[build-dependencies]
hyper = "1"
[target.'cfg(unix)'.dependencies]
axum = "0.8"
[lints]
workspace = false
"#;
    let deps = manifest_dependencies(manifest);
    for needle in [
        "hofund-plugin-host",
        "hofund-ir",
        "reqwest",
        "hyper",
        "axum",
        "hofund-kit-agent",
    ] {
        assert!(
            deps.iter().any(|dep| dep.name == needle),
            "missing {needle} in {deps:?}"
        );
    }
    assert!(
        deps.iter()
            .any(|dep| dep.name == "hyper" && dep.kind == DepKind::Build),
        "build-dependencies were ignored: {deps:?}"
    );
    assert!(
        deps.iter()
            .any(|dep| dep.name == "axum" && dep.kind == DepKind::Normal),
        "target-specific dependencies were ignored: {deps:?}"
    );
    assert!(
        manifest_enables_kernel(manifest),
        "the kernel feature on a target or normal dependency must be visible"
    );

    let renamed = r#"
[package]
name = "hofund-plugin-renamed"
[dependencies]
api = { package = "hofund-plugin-api", features = [
  "kernel",
] }
host = { package = "hofund-plugin-host" }
[dependencies.reqwest]
version = "0.12"
[features]
default = ["hofund-plugin-api/kernel"]
extra = ["dep:hofund-plugin-api/kernel"]
"#;
    let renamed_deps = manifest_dependencies(renamed);
    assert!(
        renamed_deps
            .iter()
            .any(|dep| dep.name == "hofund-plugin-api"
                && dep.features.iter().any(|feature| feature == "kernel")),
        "renamed dependency and multi-line features: {renamed_deps:?}"
    );
    assert!(
        renamed_deps
            .iter()
            .any(|dep| dep.name == "hofund-plugin-host"),
        "package = must win over the manifest key: {renamed_deps:?}"
    );
    assert!(
        renamed_deps.iter().any(|dep| dep.name == "reqwest"),
        "[dependencies.reqwest] was ignored: {renamed_deps:?}"
    );
    let forwarded = r#"
[package]
name = "hofund-plugin-forward"
[dependencies]
hofund-plugin-api = { workspace = true }
[features]
default = ["extra"]
extra = ["hofund-plugin-api/kernel"]
"#;
    assert!(
        manifest_enables_kernel(forwarded),
        "[features] forwarding hofund-plugin-api/kernel must count"
    );
    assert!(
        manifest_enables_kernel(renamed),
        "a renamed dependency's kernel feature must count"
    );

    let root = std::env::temp_dir().join(format!("hofund-plugin-rules-{}", std::process::id()));
    let _ = fs::remove_dir_all(&root);
    let plugin = root.join("plugins/providers/openai-compat");
    fs::create_dir_all(plugin.join("src")).expect("dir");
    fs::write(plugin.join("Cargo.toml"), manifest).expect("manifest");
    fs::write(
        plugin.join("src/lib.rs"),
        "use std::{process, env};\nuse reqwest::Client;\n",
    )
    .expect("source");
    let reserved = root.join("plugins/agents/api");
    fs::create_dir_all(&reserved).expect("reserved");
    fs::write(
        reserved.join("Cargo.toml"),
        "[package]\nname = \"hofund-plugin-api\"\n[lints]\nworkspace = true\n",
    )
    .expect("reserved manifest");

    let violations = plugin_tree_violations(&root.join("plugins"));
    let text = violations.join("\n");
    for needle in [
        "hofund-plugin-host",
        "hofund-ir",
        "reqwest",
        "hyper",
        "axum",
        "hofund-kit-agent",
        "std::process",
        "kernel",
        "lints",
        "reserved",
    ] {
        assert!(text.contains(needle), "missing {needle} in:\n{text}");
    }
    let _ = fs::remove_dir_all(&root);
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum DepKind {
    Normal,
    Dev,
    Build,
}

#[derive(Clone, Debug, PartialEq, Eq)]
struct DeclaredDep {
    name: String,
    kind: DepKind,
    features: Vec<String>,
}

fn denies_lint(source: &str, lint: &str) -> bool {
    let Ok(file) = syn::parse_file(source) else {
        return false;
    };
    file.attrs.iter().any(|attr| {
        if !matches!(attr.style, syn::AttrStyle::Inner(_)) {
            return false;
        }
        let syn::Meta::List(list) = &attr.meta else {
            return false;
        };
        if !list.path.is_ident("deny") {
            return false;
        }
        let Ok(nested) = list.parse_args_with(
            syn::punctuated::Punctuated::<syn::Meta, syn::Token![,]>::parse_terminated,
        ) else {
            return false;
        };
        nested.iter().any(|meta| meta.path().is_ident(lint))
    })
}

fn seam_hits(source: &str) -> Vec<String> {
    let Ok(file) = syn::parse_file(source) else {
        return vec!["does not parse".to_owned()];
    };
    let mut aliases = Aliases::default();
    let mut hits = Vec::new();
    collect_uses(&file.items, &mut aliases, &mut hits);
    let mut finder = Finder { aliases, hits };
    finder.visit_file(&file);
    finder.hits.sort();
    finder.hits.dedup();
    finder.hits
}

fn runtime_hits(source: &str) -> Vec<String> {
    let Ok(file) = syn::parse_file(source) else {
        return vec!["does not parse".to_owned()];
    };
    let mut aliases = RuntimeAliases::default();
    collect_runtime_aliases(&file.items, &mut aliases);
    let mut hits = Vec::new();
    collect_runtime(&file.items, &aliases, &mut hits);
    let mut finder = RuntimeFinder { aliases, hits };
    finder.visit_file(&file);
    finder.hits.sort();
    finder.hits.dedup();
    finder.hits
}

#[derive(Default)]
struct Aliases {
    functions: BTreeMap<String, String>,
    types: BTreeMap<String, String>,
    crates: BTreeMap<String, String>,
    kernel_glob: bool,
}

struct Finder {
    aliases: Aliases,
    hits: Vec<String>,
}

impl<'ast> Visit<'ast> for Finder {
    fn visit_expr_path(&mut self, expr: &'ast syn::ExprPath) {
        if let Some(qself) = &expr.qself
            && let syn::Type::Path(ty) = qself.ty.as_ref()
        {
            let mut segs = segments(&ty.path);
            segs.extend(segments(&expr.path));
            self.note_path(&segs);
        }
        syn::visit::visit_expr_path(self, expr);
    }

    fn visit_path(&mut self, path: &'ast syn::Path) {
        self.note_path(&segments(path));
        syn::visit::visit_path(self, path);
    }

    fn visit_item_type(&mut self, item: &'ast syn::ItemType) {
        note_type_alias(item, &mut self.aliases);
        syn::visit::visit_item_type(self, item);
    }

    fn visit_macro(&mut self, mac: &'ast syn::Macro) {
        for expr in macro_exprs(mac.tokens.clone()) {
            self.visit_expr(&expr);
        }
    }
}

impl Finder {
    fn note_path(&mut self, segs: &[String]) {
        let mut segs = canonical_segments(segs, &self.aliases.crates);
        if segs.len() >= 2 {
            let index = segs.len() - 2;
            let ty = segs[index].clone();
            if let Some(canonical) = self.aliases.types.get(&ty) {
                segs[index] = canonical.clone();
            }
        }
        if self.aliases.kernel_glob && segs.first().map(String::as_str) == Some("kernel") {
            self.hits
                .push("names kernel from a hofund_plugin_api glob".to_owned());
        }
        if segs.iter().any(|segment| segment == "from_backends") {
            self.hits.push("calls from_backends".to_owned());
        }
        if kernel_under_plugin_api(&segs) {
            self.hits.push("names hofund_plugin_api::kernel".to_owned());
        }
        if segs.len() >= 2 {
            let ty = &segs[segs.len() - 2];
            let method = &segs[segs.len() - 1];
            let canonical = self
                .aliases
                .types
                .get(ty)
                .map_or(ty.as_str(), String::as_str);
            if canonical == "SurfaceHost" && method == "from_parts" {
                self.hits.push("calls SurfaceHost::from_parts".to_owned());
            }
            if canonical == "AuthResult" && method == "new" {
                self.hits.push("calls AuthResult::new".to_owned());
            }
        }
        if segs.len() == 1
            && let Some(reason) = self.aliases.functions.get(&segs[0])
        {
            self.hits.push(format!("calls alias of {reason}"));
        }
    }
}

#[derive(Default)]
struct RuntimeAliases {
    crates: BTreeMap<String, String>,
}

struct RuntimeFinder {
    aliases: RuntimeAliases,
    hits: Vec<String>,
}

impl<'ast> Visit<'ast> for RuntimeFinder {
    fn visit_path(&mut self, path: &'ast syn::Path) {
        let segs = canonical_segments(&segments(path), &self.aliases.crates);
        if let Some(hit) = forbidden_runtime(&segs) {
            self.hits.push(hit.to_owned());
        }
        syn::visit::visit_path(self, path);
    }

    fn visit_macro(&mut self, mac: &'ast syn::Macro) {
        for expr in macro_exprs(mac.tokens.clone()) {
            self.visit_expr(&expr);
        }
    }
}

struct CommaExprs(Vec<syn::Expr>);

impl syn::parse::Parse for CommaExprs {
    fn parse(input: syn::parse::ParseStream<'_>) -> syn::Result<Self> {
        let mut exprs = Vec::new();
        while !input.is_empty() {
            exprs.push(input.parse()?);
            if input.peek(syn::Token![,]) {
                let _ = input.parse::<syn::Token![,]>()?;
            }
        }
        Ok(Self(exprs))
    }
}

fn macro_exprs(tokens: proc_macro2::TokenStream) -> Vec<syn::Expr> {
    if let Ok(parsed) = syn::parse2::<CommaExprs>(tokens.clone()) {
        return parsed.0;
    }
    let mut exprs = Vec::new();
    for tree in tokens {
        if let proc_macro2::TokenTree::Group(group) = tree {
            exprs.extend(macro_exprs(group.stream()));
        }
    }
    exprs
}

fn segments(path: &syn::Path) -> Vec<String> {
    path.segments
        .iter()
        .map(|segment| segment.ident.to_string())
        .collect()
}

fn canonical_segments(segs: &[String], crates: &BTreeMap<String, String>) -> Vec<String> {
    let mut out = segs.to_vec();
    for _ in 0..4 {
        let Some(first) = out.first().cloned() else {
            break;
        };
        let Some(real) = crates.get(&first) else {
            break;
        };
        if real == &first {
            break;
        }
        out[0] = real.clone();
    }
    out
}

fn collect_uses(items: &[syn::Item], aliases: &mut Aliases, hits: &mut Vec<String>) {
    for item in items {
        match item {
            syn::Item::Use(item) => walk_use(&[], &item.tree, aliases, hits),
            syn::Item::ExternCrate(item) => note_extern_crate(item, aliases),
            syn::Item::Mod(module) => {
                if let Some((_, nested)) = &module.content {
                    collect_uses(nested, aliases, hits);
                }
            }
            _ => {}
        }
    }
    for item in items {
        match item {
            syn::Item::Type(item) => note_type_alias(item, aliases),
            syn::Item::Mod(module) => {
                if let Some((_, nested)) = &module.content {
                    note_type_aliases(nested, aliases);
                }
            }
            _ => {}
        }
    }
}

fn note_type_aliases(items: &[syn::Item], aliases: &mut Aliases) {
    for item in items {
        match item {
            syn::Item::Type(item) => note_type_alias(item, aliases),
            syn::Item::Mod(module) => {
                if let Some((_, nested)) = &module.content {
                    note_type_aliases(nested, aliases);
                }
            }
            _ => {}
        }
    }
}

fn note_extern_crate(item: &syn::ItemExternCrate, aliases: &mut Aliases) {
    let original = item.ident.to_string();
    if original != "hofund_plugin_api" {
        return;
    }
    let local = item
        .rename
        .as_ref()
        .map(|(_, name)| name.to_string())
        .unwrap_or(original.clone());
    aliases.crates.insert(local, original);
}

fn note_type_alias(item: &syn::ItemType, aliases: &mut Aliases) {
    let syn::Type::Path(path) = item.ty.as_ref() else {
        return;
    };
    let segs = canonical_segments(&segments(&path.path), &aliases.crates);
    let Some(last) = segs.last() else {
        return;
    };
    let canonical = aliases
        .types
        .get(last)
        .cloned()
        .unwrap_or_else(|| last.clone());
    if matches!(
        canonical.as_str(),
        "SurfaceHost" | "AuthResult" | "HostContext"
    ) {
        aliases.types.insert(item.ident.to_string(), canonical);
    }
}

fn walk_use(prefix: &[String], tree: &syn::UseTree, aliases: &mut Aliases, hits: &mut Vec<String>) {
    match tree {
        syn::UseTree::Path(path) => {
            let mut next = prefix.to_vec();
            next.push(path.ident.to_string());
            walk_use(&next, &path.tree, aliases, hits);
        }
        syn::UseTree::Name(name) => {
            note_import(
                prefix,
                &name.ident.to_string(),
                &name.ident.to_string(),
                aliases,
                hits,
            );
        }
        syn::UseTree::Rename(name) => {
            note_import(
                prefix,
                &name.ident.to_string(),
                &name.rename.to_string(),
                aliases,
                hits,
            );
        }
        syn::UseTree::Glob(_) => {
            let resolved = canonical_segments(prefix, &aliases.crates);
            if resolved == ["hofund_plugin_api"] {
                aliases.kernel_glob = true;
                hits.push("globs hofund_plugin_api".to_owned());
            }
            if kernel_under_plugin_api(&resolved) {
                hits.push("globs hofund_plugin_api::kernel".to_owned());
            }
        }
        syn::UseTree::Group(group) => {
            for item in &group.items {
                walk_use(prefix, item, aliases, hits);
            }
        }
    }
}

fn note_import(
    prefix: &[String],
    name: &str,
    local: &str,
    aliases: &mut Aliases,
    hits: &mut Vec<String>,
) {
    if prefix.is_empty() && name == "hofund_plugin_api" {
        aliases
            .crates
            .insert(local.to_owned(), "hofund_plugin_api".to_owned());
    }
    let mut full = prefix.to_vec();
    full.push(name.to_owned());
    let full = canonical_segments(&full, &aliases.crates);
    if kernel_under_plugin_api(&full) {
        hits.push(format!("imports kernel as {local}"));
    }
    if name == "from_backends" {
        hits.push(format!("imports from_backends as {local}"));
        aliases
            .functions
            .insert(local.to_owned(), "from_backends".to_owned());
    }
    if prefix.last().map(String::as_str) == Some("SurfaceHost") && name == "from_parts" {
        hits.push(format!("imports SurfaceHost::from_parts as {local}"));
        aliases
            .functions
            .insert(local.to_owned(), "SurfaceHost::from_parts".to_owned());
    }
    if prefix.last().map(String::as_str) == Some("AuthResult") && name == "new" {
        hits.push(format!("imports AuthResult::new as {local}"));
        aliases
            .functions
            .insert(local.to_owned(), "AuthResult::new".to_owned());
    }
    if matches!(name, "SurfaceHost" | "AuthResult" | "HostContext") {
        aliases.types.insert(local.to_owned(), name.to_owned());
    }
}

fn collect_runtime_aliases(items: &[syn::Item], aliases: &mut RuntimeAliases) {
    for item in items {
        match item {
            syn::Item::Use(item) => note_runtime_crate(&[], &item.tree, aliases),
            syn::Item::ExternCrate(item) => {
                let original = item.ident.to_string();
                if matches!(original.as_str(), "std" | "tokio") {
                    let local = item
                        .rename
                        .as_ref()
                        .map(|(_, name)| name.to_string())
                        .unwrap_or(original.clone());
                    aliases.crates.insert(local, original);
                }
            }
            syn::Item::Mod(module) => {
                if let Some((_, nested)) = &module.content {
                    collect_runtime_aliases(nested, aliases);
                }
            }
            _ => {}
        }
    }
}

fn note_runtime_crate(prefix: &[String], tree: &syn::UseTree, aliases: &mut RuntimeAliases) {
    match tree {
        syn::UseTree::Path(path) => {
            let mut next = prefix.to_vec();
            next.push(path.ident.to_string());
            note_runtime_crate(&next, &path.tree, aliases);
        }
        syn::UseTree::Rename(name) if prefix.is_empty() => {
            let original = name.ident.to_string();
            if matches!(original.as_str(), "std" | "tokio") {
                aliases.crates.insert(name.rename.to_string(), original);
            }
        }
        syn::UseTree::Group(group) => {
            for item in &group.items {
                note_runtime_crate(prefix, item, aliases);
            }
        }
        syn::UseTree::Name(_) | syn::UseTree::Rename(_) | syn::UseTree::Glob(_) => {}
    }
}

fn collect_runtime(items: &[syn::Item], aliases: &RuntimeAliases, hits: &mut Vec<String>) {
    for item in items {
        match item {
            syn::Item::Use(item) => walk_runtime(&[], &item.tree, aliases, hits),
            syn::Item::Mod(module) => {
                if let Some((_, nested)) = &module.content {
                    collect_runtime(nested, aliases, hits);
                }
            }
            _ => {}
        }
    }
}

fn walk_runtime(
    prefix: &[String],
    tree: &syn::UseTree,
    aliases: &RuntimeAliases,
    hits: &mut Vec<String>,
) {
    match tree {
        syn::UseTree::Path(path) => {
            let mut next = prefix.to_vec();
            next.push(path.ident.to_string());
            walk_runtime(&next, &path.tree, aliases, hits);
        }
        syn::UseTree::Name(name) => {
            let mut full = prefix.to_vec();
            full.push(name.ident.to_string());
            let full = canonical_segments(&full, &aliases.crates);
            if let Some(hit) = forbidden_runtime(&full) {
                hits.push(hit.to_owned());
            }
        }
        syn::UseTree::Rename(name) => {
            let mut full = prefix.to_vec();
            full.push(name.ident.to_string());
            let full = canonical_segments(&full, &aliases.crates);
            if let Some(hit) = forbidden_runtime(&full) {
                hits.push(hit.to_owned());
            }
        }
        syn::UseTree::Glob(_) => {
            let resolved = canonical_segments(prefix, &aliases.crates);
            if let Some(hit) = forbidden_runtime(&resolved) {
                hits.push(hit.to_owned());
            }
        }
        syn::UseTree::Group(group) => {
            for item in &group.items {
                walk_runtime(prefix, item, aliases, hits);
            }
        }
    }
}

fn kernel_under_plugin_api(segments: &[String]) -> bool {
    segments
        .windows(2)
        .any(|pair| pair[0] == "hofund_plugin_api" && pair[1] == "kernel")
}

fn forbidden_runtime(segments: &[String]) -> Option<&'static str> {
    if segments.len() >= 2 && segments[0] == "std" && segments[1] == "process" {
        return Some("std::process");
    }
    if segments.len() >= 2 && segments[0] == "tokio" && segments[1] == "process" {
        return Some("tokio::process");
    }
    if segments.len() >= 2 && segments[0] == "tokio" && segments[1] == "net" {
        return Some("tokio::net");
    }
    match segments.first().map(String::as_str) {
        Some("reqwest") => Some("reqwest"),
        Some("hyper") => Some("hyper"),
        Some("axum") => Some("axum"),
        _ => None,
    }
}

fn plugin_tree_violations(plugins: &Path) -> Vec<String> {
    let mut violations = Vec::new();
    let kinds = ["providers", "subscriptions", "agents", "inbound"];
    let Ok(entries) = fs::read_dir(plugins) else {
        return violations;
    };
    for entry in entries.flatten() {
        let kind_path = entry.path();
        if !kind_path.is_dir() {
            continue;
        }
        let kind = entry.file_name();
        let kind = kind.to_string_lossy();
        if kind == "registry" || !kinds.contains(&kind.as_ref()) {
            continue;
        }
        let Ok(plugins_in_kind) = fs::read_dir(&kind_path) else {
            continue;
        };
        for plugin in plugins_in_kind.flatten() {
            let path = plugin.path();
            let id = plugin.file_name().to_string_lossy().into_owned();
            if ["api", "sdk", "host", "registry"].contains(&id.as_str()) {
                violations.push(format!("{kind}/{id} uses a reserved plugin id"));
            }
            let manifest_path = path.join("Cargo.toml");
            if !manifest_path.is_file() {
                continue;
            }
            let manifest = read_to_string(&manifest_path);
            if toml::from_str::<toml::Value>(&manifest).is_err() {
                violations.push(format!("{kind}/{id} manifest is not valid toml"));
            }
            if !super::lints_follow_workspace(&manifest) {
                violations.push(format!("{kind}/{id} must set [lints] workspace = true"));
            }
            if manifest_enables_kernel(&manifest) {
                violations.push(format!("{kind}/{id} enables the kernel feature"));
            }
            for dep in manifest_dependencies(&manifest) {
                if dep.kind == DepKind::Dev {
                    let own_kit = match kind.as_ref() {
                        "providers" => "hofund-kit-upstream",
                        "subscriptions" => "hofund-kit-subscription",
                        "agents" => "hofund-kit-agent",
                        _ => "",
                    };
                    if dep.name.starts_with("hofund-kit-") && dep.name != own_kit {
                        violations.push(format!("{kind}/{id} dev-depends on {}", dep.name));
                    }
                    continue;
                }
                if forbidden_dep(&dep.name) {
                    violations.push(format!("{kind}/{id} depends on {}", dep.name));
                }
            }
            if path.join("src").is_dir() {
                for source in rust_files(&path.join("src")) {
                    let text = read_to_string(&source);
                    let file = source.file_name().unwrap_or_default().to_string_lossy();
                    for hit in runtime_hits(&text) {
                        violations.push(format!("{kind}/{id} uses {hit} in {file}"));
                    }
                }
            }
        }
    }
    violations
}

fn forbidden_dep(dep: &str) -> bool {
    dep == "hofund-plugin-host"
        || dep == "hofund-ir"
        || dep == "hofund-provider"
        || dep == "hofund-router"
        || dep == "hofund-gateway"
        || dep == "hofund-daemon"
        || dep == "hofund-plugin-registry"
        || dep.starts_with("hofund-kit-")
        || (dep.starts_with("hofund-plugin-")
            && dep != "hofund-plugin-api"
            && dep != "hofund-plugin-sdk")
        || matches!(dep, "reqwest" | "hyper" | "axum" | "tracing")
}

fn manifest_enables_kernel(manifest: &str) -> bool {
    let dependency = manifest_dependencies(manifest).iter().any(|dep| {
        dep.name == "hofund-plugin-api" && dep.features.iter().any(|feature| feature == "kernel")
    });
    dependency || feature_forwards_kernel(manifest)
}

fn feature_forwards_kernel(manifest: &str) -> bool {
    let Ok(value) = toml::from_str::<toml::Value>(manifest) else {
        return false;
    };
    let Some(features) = value.get("features").and_then(toml::Value::as_table) else {
        return false;
    };
    features
        .keys()
        .any(|name| feature_list_enables_kernel(name, features, &mut BTreeSet::new()))
}

fn feature_list_enables_kernel(
    name: &str,
    features: &toml::map::Map<String, toml::Value>,
    seen: &mut BTreeSet<String>,
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
        if is_kernel_forward(text)
            || (!text.contains('/') && feature_list_enables_kernel(text, features, seen))
        {
            return true;
        }
    }
    false
}

fn is_kernel_forward(text: &str) -> bool {
    text == "hofund-plugin-api/kernel" || text == "dep:hofund-plugin-api/kernel"
}

fn manifest_dependencies(manifest: &str) -> Vec<DeclaredDep> {
    let Ok(value) = toml::from_str::<toml::Value>(manifest) else {
        return Vec::new();
    };
    let mut deps = Vec::new();
    collect_dep_tables(&value, &mut deps);
    deps
}

fn collect_dep_tables(value: &toml::Value, deps: &mut Vec<DeclaredDep>) {
    let Some(table) = value.as_table() else {
        return;
    };
    for (key, child) in table {
        if let Some(kind) = dep_kind(key) {
            push_dep_table(child, kind, deps);
        } else {
            collect_dep_tables(child, deps);
        }
    }
}

fn push_dep_table(value: &toml::Value, kind: DepKind, deps: &mut Vec<DeclaredDep>) {
    let Some(table) = value.as_table() else {
        return;
    };
    for (key, spec) in table {
        match spec {
            toml::Value::String(_) => deps.push(DeclaredDep {
                name: key.clone(),
                kind,
                features: Vec::new(),
            }),
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
                deps.push(DeclaredDep {
                    name,
                    kind,
                    features,
                });
            }
            toml::Value::Array(_)
            | toml::Value::Integer(_)
            | toml::Value::Float(_)
            | toml::Value::Boolean(_)
            | toml::Value::Datetime(_) => {}
        }
    }
}

fn dep_kind(section: &str) -> Option<DepKind> {
    if section == "dev-dependencies" {
        Some(DepKind::Dev)
    } else if section == "build-dependencies" {
        Some(DepKind::Build)
    } else if section == "dependencies" {
        Some(DepKind::Normal)
    } else {
        None
    }
}

struct WorkspacePackage {
    dependencies: Vec<DeclaredDep>,
    kernel_feature: bool,
}

fn cargo_packages() -> BTreeMap<String, WorkspacePackage> {
    let output = Command::new("cargo")
        .args(["metadata", "--format-version", "1", "--no-deps"])
        .current_dir(workspace_root())
        .output()
        .expect("cargo metadata");
    assert!(
        output.status.success(),
        "cargo metadata failed: {}",
        String::from_utf8_lossy(&output.stderr)
    );
    let value: serde_json::Value =
        serde_json::from_slice(&output.stdout).expect("cargo metadata json");
    let mut packages = BTreeMap::new();
    let Some(list) = value
        .get("packages")
        .and_then(|packages| packages.as_array())
    else {
        return packages;
    };
    for package in list {
        let Some(name) = package.get("name").and_then(|name| name.as_str()) else {
            continue;
        };
        let mut dependencies = Vec::new();
        if let Some(deps) = package.get("dependencies").and_then(|deps| deps.as_array()) {
            for dep in deps {
                let Some(dep_name) = dep.get("name").and_then(|name| name.as_str()) else {
                    continue;
                };
                let kind = match dep.get("kind").and_then(|kind| kind.as_str()) {
                    Some("dev") => DepKind::Dev,
                    Some("build") => DepKind::Build,
                    _ => DepKind::Normal,
                };
                let features = dep
                    .get("features")
                    .and_then(|features| features.as_array())
                    .map(|features| {
                        features
                            .iter()
                            .filter_map(|feature| feature.as_str().map(str::to_owned))
                            .collect()
                    })
                    .unwrap_or_default();
                dependencies.push(DeclaredDep {
                    name: dep_name.to_owned(),
                    kind,
                    features,
                });
            }
        }
        let direct = dependencies.iter().any(|dep| {
            dep.name == "hofund-plugin-api"
                && dep.features.iter().any(|feature| feature == "kernel")
        });
        let forwarded = package
            .get("features")
            .and_then(|features| features.as_object())
            .is_some_and(json_features_enable_kernel);
        packages.insert(
            name.to_owned(),
            WorkspacePackage {
                dependencies,
                kernel_feature: direct || forwarded,
            },
        );
    }
    packages
}

fn json_features_enable_kernel(features: &serde_json::Map<String, serde_json::Value>) -> bool {
    fn enables(
        name: &str,
        features: &serde_json::Map<String, serde_json::Value>,
        seen: &mut BTreeSet<String>,
    ) -> bool {
        if !seen.insert(name.to_owned()) {
            return false;
        }
        let Some(items) = features.get(name).and_then(|value| value.as_array()) else {
            return false;
        };
        for item in items {
            let Some(text) = item.as_str() else {
                continue;
            };
            if is_kernel_forward(text) || (!text.contains('/') && enables(text, features, seen)) {
                return true;
            }
        }
        false
    }
    features
        .keys()
        .any(|name| enables(name, features, &mut BTreeSet::new()))
}
