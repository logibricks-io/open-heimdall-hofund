//! Cargo and source boundary for plugin packages.
//!
//! Plugins may depend only on `hofund-plugin-api` and `hofund-plugin-sdk`
//! (plus their own test kit), and those two crates must be `workspace = true`.
//! Clippy, configured by `xtask/plugin-boundary/clippy.toml`, sees a macro
//! fragment such as `std::$m::Command` after expansion. This module does not
//! try to match `$` splices in source text. The threat model is
//! `docs/plugins/boundary.md`.

use std::collections::{BTreeMap, BTreeSet};
use std::fs;
use std::path::{Component, Path, PathBuf};
use std::process::Command;

use syn::visit::Visit;

const ALLOWED_DEPS: &[&str] = &["hofund-plugin-api", "hofund-plugin-sdk"];
const TOKIO_BANNED_FEATURES: &[&str] = &["full", "net", "process", "signal"];
const WORKSPACE_CRATES: &[&str] = &["hofund-plugin-api", "hofund-plugin-sdk"];
const FOREIGN_DEP_KEYS: &[&str] = &["path", "git", "version", "registry", "branch", "rev", "tag"];

/// Clippy flags for the plugin boundary. `-F` cannot be silenced in source.
pub const CLIPPY_ARGS: &[&str] = &[
    "-D",
    "warnings",
    "-F",
    "clippy::disallowed_types",
    "-F",
    "clippy::disallowed_methods",
];

/// Clippy invocations for one plugin.
///
/// Dev and release each run with `--all-features`. A package that defines
/// features also runs both profiles with `--no-default-features`, so a
/// `cfg(feature)` splice is compiled on and off.
#[must_use]
pub fn clippy_passes(defines_features: bool) -> Vec<(&'static str, &'static str)> {
    let mut passes = vec![("dev", "--all-features"), ("release", "--all-features")];
    if defines_features {
        passes.push(("dev", "--no-default-features"));
        passes.push(("release", "--no-default-features"));
    }
    passes
}

/// Whether `[features]` names at least one feature.
#[must_use]
pub fn manifest_defines_features(text: &str) -> bool {
    let Ok(value) = toml::from_str::<toml::Value>(text) else {
        return false;
    };
    value
        .get("features")
        .and_then(toml::Value::as_table)
        .is_some_and(|features| !features.is_empty())
}

/// Whether the manifest sets `[lints] workspace = true`.
///
/// The workspace forbids `unsafe_code`. A plugin that drops the table can
/// compile an FFI block the boundary lints never see.
#[must_use]
pub fn lints_follow_workspace(manifest: &str) -> bool {
    let mut in_lints = false;
    for line in manifest.lines() {
        let trimmed = strip_manifest_comment(line).trim();
        if trimmed.starts_with('[') {
            in_lints = trimmed == "[lints]";
            continue;
        }
        if in_lints && trimmed == "workspace = true" {
            return true;
        }
    }
    false
}

fn strip_manifest_comment(line: &str) -> &str {
    match line.split_once('#') {
        Some((code, _)) => code,
        None => line,
    }
}

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
        if WORKSPACE_CRATES.contains(&name.as_str())
            && let Some(error) = workspace_dependency_error(&name, spec)
        {
            errors.push(error);
        }
        if let Some(error) = dependency_error(&name, &features, dev, own_kit) {
            errors.push(error);
        }
    }
}

fn workspace_dependency_error(name: &str, spec: &toml::Value) -> Option<String> {
    let message = format!("{name} must be workspace = true");
    match spec {
        toml::Value::Table(body) => {
            let workspace = body.get("workspace").and_then(toml::Value::as_bool) == Some(true);
            let foreign = FOREIGN_DEP_KEYS.iter().any(|key| body.contains_key(*key));
            if workspace && !foreign {
                None
            } else {
                Some(message)
            }
        }
        toml::Value::String(_) => Some(message),
        _ => Some(message),
    }
}

/// `#[path]` escapes, lint silencing, and other source violations in one snippet.
///
/// The seam tests and `check-plugins` both call this.
#[must_use]
pub fn scan_source(text: &str) -> Vec<String> {
    let mut hits = scan_parsed(Path::new("src/lib.rs"), Path::new("src"), text);
    hits.sort();
    hits.dedup();
    hits
}

/// `#[path]` escapes and non-literal includes under a plugin package.
#[must_use]
pub fn source_violations(package_dir: &Path) -> Vec<String> {
    let mut hits = Vec::new();
    let mut seen = BTreeSet::new();
    let src = package_dir.join("src");
    if src.is_dir() {
        for file in rust_files(&src) {
            hits.extend(scan_file(&file, &src, &mut seen));
        }
    }
    let build = package_dir.join("build.rs");
    if build.is_file() {
        hits.extend(scan_file(&build, package_dir, &mut seen));
    }
    hits.sort();
    hits.dedup();
    hits
}

fn scan_file(file: &Path, root: &Path, seen: &mut BTreeSet<PathBuf>) -> Vec<String> {
    if !seen.insert(normalize(file)) {
        return Vec::new();
    }
    let Ok(text) = fs::read_to_string(file) else {
        return vec![format!("unreadable {}", file.display())];
    };
    let mut hits = scan_parsed(file, root, &text);
    if let Some(dir) = file.parent() {
        for included in literal_includes(&text) {
            let next = dir.join(included);
            if next.is_file() {
                hits.extend(scan_file(&next, root, seen));
            }
        }
    }
    hits
}

fn scan_parsed(file: &Path, root: &Path, text: &str) -> Vec<String> {
    let Ok(parsed) = syn::parse_file(text) else {
        return vec![format!("{} does not parse", file.display())];
    };
    let mut aliases = Aliases::default();
    AliasPass {
        aliases: &mut aliases,
    }
    .visit_file(&parsed);
    let mut finder = SourceFinder {
        file,
        root,
        aliases,
        hits: Vec::new(),
    };
    finder.visit_file(&parsed);
    finder.hits
}

fn literal_includes(text: &str) -> Vec<String> {
    let Ok(parsed) = syn::parse_file(text) else {
        return Vec::new();
    };
    let mut found = Vec::new();
    IncludePass { found: &mut found }.visit_file(&parsed);
    found
}

#[derive(Default)]
struct Aliases {
    crates: BTreeMap<String, String>,
    globs: BTreeSet<String>,
}

struct AliasPass<'a> {
    aliases: &'a mut Aliases,
}

impl<'ast> Visit<'ast> for AliasPass<'_> {
    fn visit_item_use(&mut self, item: &'ast syn::ItemUse) {
        note_crate_alias(&[], &item.tree, self.aliases);
    }

    fn visit_item_extern_crate(&mut self, item: &'ast syn::ItemExternCrate) {
        let original = ident(&item.ident);
        if matches!(original.as_str(), "std" | "tokio" | "libc" | "nix") {
            let local = item
                .rename
                .as_ref()
                .map(|(_, name)| ident(name))
                .unwrap_or_else(|| original.clone());
            self.aliases.crates.insert(local, original);
        }
    }
}

struct IncludePass<'a> {
    found: &'a mut Vec<String>,
}

impl<'ast> Visit<'ast> for IncludePass<'_> {
    fn visit_macro(&mut self, mac: &'ast syn::Macro) {
        if mac.path.is_ident("include")
            && let Ok(literal) = syn::parse2::<syn::LitStr>(mac.tokens.clone())
        {
            self.found.push(literal.value());
        }
        syn::visit::visit_macro(self, mac);
    }
}

struct SourceFinder<'a> {
    file: &'a Path,
    root: &'a Path,
    aliases: Aliases,
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

    fn visit_item_use(&mut self, item: &'ast syn::ItemUse) {
        walk_use(&[], &item.tree, &self.aliases, &mut self.hits);
    }

    fn visit_attribute(&mut self, attr: &'ast syn::Attribute) {
        for lint in suppressed_lints(&attr.meta) {
            self.hits.push(format!("suppresses {lint}"));
        }
        if attribute_cfg_word(attr, "clippy") {
            self.hits.push("cfg(clippy)".to_owned());
        }
        if attribute_cfg_word(attr, "debug_assertions") {
            self.hits.push("cfg(debug_assertions)".to_owned());
        }
        if cfg_attr_sets_path(&attr.meta) {
            self.hits.push("cfg_attr path attribute".to_owned());
        }
        if let syn::Meta::List(list) = &attr.meta {
            for stmt in parsed_macro_stmts(list.tokens.clone()) {
                self.visit_stmt(&stmt);
            }
        }
        syn::visit::visit_attribute(self, attr);
    }

    fn visit_macro(&mut self, mac: &'ast syn::Macro) {
        if let Some(name) = mac
            .path
            .segments
            .last()
            .map(|segment| segment.ident.to_string())
        {
            if matches!(name.as_str(), "include" | "include_str" | "include_bytes")
                && syn::parse2::<syn::LitStr>(mac.tokens.clone()).is_err()
            {
                self.hits.push(format!("non-literal {name}!"));
            }
            if name == "cfg"
                && let Ok(meta) = syn::parse2::<syn::Meta>(mac.tokens.clone())
            {
                if mentions_cfg_word(&meta, "clippy") {
                    self.hits.push("cfg(clippy)".to_owned());
                }
                if mentions_cfg_word(&meta, "debug_assertions") {
                    self.hits.push("cfg(debug_assertions)".to_owned());
                }
            }
        }
        // A body that parses as Rust is scanned. A `$` splice does not parse,
        // and Clippy sees it after expansion.
        for stmt in parsed_macro_stmts(mac.tokens.clone()) {
            self.visit_stmt(&stmt);
        }
    }

    fn visit_path(&mut self, path: &'ast syn::Path) {
        let segments = expand(&path_segments(path), &self.aliases);
        if let Some(hit) = forbidden_path(&segments) {
            self.hits.push(format!("uses {hit}"));
        }
        syn::visit::visit_path(self, path);
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

fn parsed_macro_stmts(tokens: proc_macro2::TokenStream) -> Vec<syn::Stmt> {
    if let Some(stmts) = block_stmts(tokens.clone()) {
        return stmts;
    }
    let mut with_semi = tokens.clone();
    with_semi.extend(std::iter::once(proc_macro2::TokenTree::Punct(
        proc_macro2::Punct::new(';', proc_macro2::Spacing::Alone),
    )));
    if let Some(stmts) = block_stmts(with_semi) {
        return stmts;
    }
    if let Ok(parsed) = syn::parse2::<CommaExprs>(tokens.clone()) {
        return parsed
            .0
            .into_iter()
            .map(|expr| syn::Stmt::Expr(expr, None))
            .collect();
    }
    let mut stmts = Vec::new();
    for tree in tokens {
        if let proc_macro2::TokenTree::Group(group) = tree {
            stmts.extend(parsed_macro_stmts(group.stream()));
        }
    }
    stmts
}

fn block_stmts(tokens: proc_macro2::TokenStream) -> Option<Vec<syn::Stmt>> {
    let wrapped = proc_macro2::TokenStream::from(proc_macro2::TokenTree::Group(
        proc_macro2::Group::new(proc_macro2::Delimiter::Brace, tokens),
    ));
    syn::parse2::<syn::Block>(wrapped)
        .ok()
        .map(|block| block.stmts)
}

fn ident(ident: &syn::Ident) -> String {
    let text = ident.to_string();
    text.strip_prefix("r#").unwrap_or(&text).to_owned()
}

fn path_segments(path: &syn::Path) -> Vec<String> {
    path.segments
        .iter()
        .map(|segment| ident(&segment.ident))
        .collect()
}

fn note_crate_alias(prefix: &[String], tree: &syn::UseTree, aliases: &mut Aliases) {
    match tree {
        syn::UseTree::Path(path) => {
            let mut next = prefix.to_vec();
            next.push(ident(&path.ident));
            note_crate_alias(&next, &path.tree, aliases);
        }
        syn::UseTree::Rename(name) => {
            let original = ident(&name.ident);
            let crate_name = if original == "self" {
                prefix.first().cloned()
            } else if prefix.is_empty() {
                Some(original)
            } else {
                None
            };
            if let Some(crate_name) = crate_name.filter(|name| {
                matches!(name.as_str(), "std" | "tokio" | "libc" | "nix") && prefix.len() <= 1
            }) {
                aliases.crates.insert(ident(&name.rename), crate_name);
            }
        }
        syn::UseTree::Glob(_) => {
            let resolved = expand(prefix, aliases);
            if resolved.len() == 1
                && let Some(name) = resolved.first().cloned()
                && matches!(name.as_str(), "std" | "tokio" | "libc" | "nix")
            {
                aliases.globs.insert(name);
            }
        }
        syn::UseTree::Group(group) => {
            for item in &group.items {
                note_crate_alias(prefix, item, aliases);
            }
        }
        syn::UseTree::Name(_) => {}
    }
}

fn walk_use(prefix: &[String], tree: &syn::UseTree, aliases: &Aliases, hits: &mut Vec<String>) {
    match tree {
        syn::UseTree::Path(path) => {
            let mut next = prefix.to_vec();
            next.push(ident(&path.ident));
            walk_use(&next, &path.tree, aliases, hits);
        }
        syn::UseTree::Name(name) => note_use(prefix, &ident(&name.ident), aliases, hits),
        syn::UseTree::Rename(name) => note_use(prefix, &ident(&name.ident), aliases, hits),
        syn::UseTree::Glob(_) => {
            if let Some(hit) = forbidden_path(&expand(prefix, aliases)) {
                hits.push(format!("uses {hit}"));
            }
        }
        syn::UseTree::Group(group) => {
            for item in &group.items {
                walk_use(prefix, item, aliases, hits);
            }
        }
    }
}

fn note_use(prefix: &[String], name: &str, aliases: &Aliases, hits: &mut Vec<String>) {
    let mut full = prefix.to_vec();
    full.push(name.to_owned());
    if let Some(hit) = forbidden_path(&expand(&full, aliases)) {
        hits.push(format!("uses {hit}"));
    }
}

fn expand(segs: &[String], aliases: &Aliases) -> Vec<String> {
    let mut out = segs.to_vec();
    while matches!(
        out.first().map(String::as_str),
        Some("crate" | "self" | "super")
    ) {
        out.remove(0);
    }
    for _ in 0..8 {
        let mut changed = false;
        for slot in &mut out {
            let Some(real) = aliases.crates.get(slot.as_str()) else {
                continue;
            };
            if real == slot {
                continue;
            }
            *slot = real.clone();
            changed = true;
        }
        if !changed {
            break;
        }
    }
    if matches!(
        out.first().map(String::as_str),
        Some("std" | "tokio" | "libc" | "nix")
    ) {
        return out;
    }
    let Some(first) = out.first() else {
        return out;
    };
    if aliases.globs.contains("std") && matches!(first.as_str(), "process" | "net" | "os") {
        let mut prefixed = vec!["std".to_owned()];
        prefixed.extend(out);
        return prefixed;
    }
    if aliases.globs.contains("tokio") && matches!(first.as_str(), "process" | "net") {
        let mut prefixed = vec!["tokio".to_owned()];
        prefixed.extend(out);
        return prefixed;
    }
    out
}

fn nested_metas(list: &syn::MetaList) -> Vec<syn::Meta> {
    list.parse_args_with(syn::punctuated::Punctuated::<syn::Meta, syn::Token![,]>::parse_terminated)
        .map(|metas| metas.into_iter().collect())
        .unwrap_or_default()
}

fn lint_label(meta: &syn::Meta) -> String {
    meta.path()
        .segments
        .iter()
        .map(|segment| ident(&segment.ident))
        .collect::<Vec<_>>()
        .join("::")
}

fn is_boundary_lint(label: &str) -> bool {
    label == "clippy::all"
        || label == "clippy::restriction"
        || label == "clippy::style"
        || label
            .strip_prefix("clippy::")
            .is_some_and(|rest| rest.starts_with("disallowed_"))
}

fn suppressed_lints(meta: &syn::Meta) -> Vec<String> {
    let Some(name) = meta.path().get_ident().map(|name| name.to_string()) else {
        return Vec::new();
    };
    match name.as_str() {
        "allow" | "expect" | "warn" => {
            let syn::Meta::List(list) = meta else {
                return Vec::new();
            };
            nested_metas(list)
                .iter()
                .map(lint_label)
                .filter(|label| is_boundary_lint(label))
                .collect()
        }
        "cfg_attr" => {
            let syn::Meta::List(list) = meta else {
                return Vec::new();
            };
            nested_metas(list)
                .into_iter()
                .skip(1)
                .flat_map(|meta| suppressed_lints(&meta))
                .collect()
        }
        _ => Vec::new(),
    }
}

fn mentions_cfg_word(meta: &syn::Meta, word: &str) -> bool {
    if meta.path().is_ident(word) {
        return true;
    }
    let syn::Meta::List(list) = meta else {
        return false;
    };
    nested_metas(list)
        .iter()
        .any(|meta| mentions_cfg_word(meta, word))
}

fn attribute_cfg_word(attr: &syn::Attribute, word: &str) -> bool {
    let Some(name) = attr.path().get_ident() else {
        return false;
    };
    match name.to_string().as_str() {
        "cfg" => attr
            .parse_args::<syn::Meta>()
            .ok()
            .is_some_and(|meta| mentions_cfg_word(&meta, word)),
        "cfg_attr" => {
            let syn::Meta::List(list) = &attr.meta else {
                return false;
            };
            nested_metas(list)
                .first()
                .is_some_and(|meta| mentions_cfg_word(meta, word))
        }
        _ => false,
    }
}

fn cfg_attr_sets_path(meta: &syn::Meta) -> bool {
    let syn::Meta::List(list) = meta else {
        return false;
    };
    if !list.path.is_ident("cfg_attr") {
        return false;
    }
    nested_metas(list)
        .iter()
        .skip(1)
        .any(|meta| meta.path().is_ident("path") || cfg_attr_sets_path(meta))
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

/// Package ids in the normal and build closure of one workspace package.
///
/// Dev-dependencies are not part of the closure. Ids are Cargo package ids,
/// so a same-named crate from another path is a different package.
///
/// # Errors
///
/// Returns an error when Cargo metadata fails or the package is not in the workspace.
pub fn link_ids(workspace: &Path, package: &str) -> Result<BTreeSet<String>, String> {
    let meta = cargo_metadata(workspace)?;
    let root = workspace_package_id(&meta, package, workspace)?;
    Ok(closure(&meta, &root))
}

/// The plugin's link closure must be the API and SDK closures, plus itself.
///
/// # Errors
///
/// Returns an error when Cargo fails or the plugin links anything else.
pub fn link_allowance(workspace: &Path, package: &str) -> Result<(), String> {
    let meta = cargo_metadata(workspace)?;
    let mut allowed = closure(
        &meta,
        &workspace_package_id(&meta, "hofund-plugin-api", workspace)?,
    );
    allowed.extend(closure(
        &meta,
        &workspace_package_id(&meta, "hofund-plugin-sdk", workspace)?,
    ));
    let root = workspace_package_id(&meta, package, workspace)?;
    allowed.insert(root.clone());
    let linked = closure(&meta, &root);
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

struct Metadata {
    nodes: BTreeMap<String, Vec<(String, bool)>>,
    ids_by_manifest: Vec<(String, String, String)>,
}

fn cargo_metadata(workspace: &Path) -> Result<Metadata, String> {
    let output = Command::new("cargo")
        .args(["metadata", "--format-version", "1"])
        .current_dir(workspace)
        .output()
        .map_err(|error| format!("cargo metadata: {error}"))?;
    if !output.status.success() {
        return Err(format!(
            "cargo metadata failed: {}",
            String::from_utf8_lossy(&output.stderr)
        ));
    }
    let value: serde_json::Value = serde_json::from_slice(&output.stdout)
        .map_err(|error| format!("cargo metadata is not json: {error}"))?;
    let mut nodes = BTreeMap::new();
    if let Some(list) = value
        .pointer("/resolve/nodes")
        .and_then(serde_json::Value::as_array)
    {
        for node in list {
            let Some(id) = node.get("id").and_then(serde_json::Value::as_str) else {
                continue;
            };
            let mut deps = Vec::new();
            if let Some(edges) = node.get("deps").and_then(serde_json::Value::as_array) {
                for edge in edges {
                    let Some(pkg) = edge.get("pkg").and_then(serde_json::Value::as_str) else {
                        continue;
                    };
                    let kinds = edge.get("dep_kinds").and_then(serde_json::Value::as_array);
                    let link = kinds.is_none_or(|kinds| {
                        kinds.is_empty()
                            || kinds.iter().any(|kind| {
                                matches!(
                                    kind.get("kind").and_then(serde_json::Value::as_str),
                                    None | Some("normal") | Some("build")
                                )
                            })
                    });
                    deps.push((pkg.to_owned(), link));
                }
            }
            nodes.insert(id.to_owned(), deps);
        }
    }
    let mut ids_by_manifest = Vec::new();
    if let Some(packages) = value.get("packages").and_then(serde_json::Value::as_array) {
        for package in packages {
            let Some(name) = package.get("name").and_then(serde_json::Value::as_str) else {
                continue;
            };
            let Some(id) = package.get("id").and_then(serde_json::Value::as_str) else {
                continue;
            };
            let Some(manifest) = package
                .get("manifest_path")
                .and_then(serde_json::Value::as_str)
            else {
                continue;
            };
            ids_by_manifest.push((name.to_owned(), id.to_owned(), manifest.to_owned()));
        }
    }
    if nodes.is_empty() {
        return Err("cargo metadata listed no resolve nodes".to_owned());
    }
    Ok(Metadata {
        nodes,
        ids_by_manifest,
    })
}

fn workspace_package_id(meta: &Metadata, name: &str, workspace: &Path) -> Result<String, String> {
    let workspace = normalize(workspace);
    let mut found = Vec::new();
    for (package, id, manifest) in &meta.ids_by_manifest {
        if package == name && normalize(Path::new(manifest)).starts_with(&workspace) {
            found.push(id.clone());
        }
    }
    match found.len() {
        1 => Ok(found.remove(0)),
        0 => Err(format!(
            "{name} is not a package inside {}",
            workspace.display()
        )),
        _ => Err(format!("{name} matches more than one workspace package")),
    }
}

fn closure(meta: &Metadata, root: &str) -> BTreeSet<String> {
    let mut seen = BTreeSet::new();
    let mut stack = vec![root.to_owned()];
    while let Some(id) = stack.pop() {
        if !seen.insert(id.clone()) {
            continue;
        }
        let Some(deps) = meta.nodes.get(&id) else {
            continue;
        };
        for (pkg, link) in deps {
            if *link {
                stack.push(pkg.clone());
            }
        }
    }
    seen
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
            .args(["clippy", "--offline", "--manifest-path"])
            .arg(root.join("Cargo.toml"))
            .arg("--")
            .args(super::CLIPPY_ARGS)
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

    #[test]
    fn round3_bypasses_fail_in_the_shared_scan() {
        let cases = [
            (
                "#![allow(clippy::disallowed_methods, clippy::disallowed_types)]\nfn sneak() {}\n",
                "suppresses clippy::disallowed_methods",
            ),
            (
                "#[allow(clippy::all)]\nfn sneak() {}\n",
                "suppresses clippy::all",
            ),
            (
                "#[expect(clippy::disallowed_methods)]\nfn sneak() {}\n",
                "suppresses clippy::disallowed_methods",
            ),
            (
                "#[warn(clippy::restriction)]\nfn sneak() {}\n",
                "suppresses clippy::restriction",
            ),
            (
                "#[allow(clippy::style)]\nfn sneak() {}\n",
                "suppresses clippy::style",
            ),
            (
                "#[cfg(not(debug_assertions))]\nmacro_rules! spawn { ($m:ident) => { std::$m::Command::new(\"true\").status() }; }\n#[cfg(not(debug_assertions))]\npub fn sneak() { let _ = spawn!(process); }\n#[cfg(debug_assertions)]\npub fn sneak() {}\n",
                "cfg(debug_assertions)",
            ),
            (
                "fn sneak() { if cfg!(not(debug_assertions)) { let _ = std::process::id(); } }\n",
                "cfg(debug_assertions)",
            ),
            (
                "#[cfg_attr(all(), allow(clippy::disallowed_types))]\nfn sneak() {}\n",
                "suppresses clippy::disallowed_types",
            ),
            (
                "#[cfg(not(clippy))]\nfn sneak() { let _ = std::process::Command::new(\"true\"); }\n",
                "cfg(clippy)",
            ),
            ("#[cfg(clippy)]\nfn hidden() {}\n", "cfg(clippy)"),
            (
                "#[cfg_attr(all(), path = \"../../../../tmp/hidden.rs\")]\nmod net;\n",
                "cfg_attr path attribute",
            ),
            (
                "extern crate std as s;\nfn sneak() { let _ = s::process::Command::new(\"true\"); }\n",
                "uses std::process",
            ),
            (
                "fn sneak() { if cfg!(not(clippy)) { let _ = std::process::Command::new(\"true\"); } }\n",
                "cfg(clippy)",
            ),
        ];
        for (source, needle) in cases {
            let hits = super::scan_source(source).join("\n");
            assert!(
                hits.contains(needle),
                "{needle} missing in:\n{hits}\n---\n{source}"
            );
        }
        let manifest =
            "hofund-plugin-api = { path = \"/tmp/fakeapi\" }\nhofund-plugin-sdk = \"0.9\"\n";
        let text = format!("[dependencies]\n{manifest}");
        let errors = manifest_dependency_errors(&text, None)
            .expect("toml")
            .join("\n");
        assert!(
            errors.contains("hofund-plugin-api must be workspace = true"),
            "{errors}"
        );
        assert!(
            errors.contains("hofund-plugin-sdk must be workspace = true"),
            "{errors}"
        );
    }

    #[test]
    fn link_allowance_compares_package_ids_and_skips_dev_deps() {
        let workspace = PathBuf::from(env!("CARGO_MANIFEST_DIR"))
            .parent()
            .expect("workspace")
            .to_path_buf();
        super::link_allowance(&workspace, "hofund-plugin-fixture").expect("fixture closure");
        let ids = super::link_ids(&workspace, "hofund-plugin-fixture").expect("ids");
        assert!(
            ids.iter().any(|id| id.contains("crates/plugin-api#")),
            "the workspace api id is required, got {ids:?}"
        );
        // `quote` and `syn` stay reachable through `thiserror`'s proc-macro.
        // `static_assertions` and `tokio` are dev-dependencies only.
        for banned in ["static_assertions@", "tokio@"] {
            assert!(
                ids.iter().all(|id| !id.contains(banned)),
                "{banned} is a dev-dependency and must stay out of the closure: {ids:?}"
            );
        }
    }

    #[test]
    fn clippy_passes_cover_dev_release_and_features() {
        assert_eq!(
            super::clippy_passes(false),
            [("dev", "--all-features"), ("release", "--all-features")]
        );
        assert_eq!(
            super::clippy_passes(true),
            [
                ("dev", "--all-features"),
                ("release", "--all-features"),
                ("dev", "--no-default-features"),
                ("release", "--no-default-features"),
            ]
        );
        assert!(!super::lints_follow_workspace("[package]\nname = \"x\"\n"));
        assert!(super::lints_follow_workspace("[lints]\nworkspace = true\n"));
        assert!(super::manifest_defines_features("[features]\nextra = []\n"));
        assert!(!super::manifest_defines_features(
            "[package]\nname = \"x\"\n"
        ));
    }

    #[test]
    fn release_and_feature_fixtures_fail_clippy() {
        let workspace = PathBuf::from(env!("CARGO_MANIFEST_DIR"))
            .parent()
            .expect("workspace")
            .to_path_buf();
        let release =
            std::env::temp_dir().join(format!("hofund-plugin-release-{}", std::process::id()));
        let feature =
            std::env::temp_dir().join(format!("hofund-plugin-feature-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&release);
        let _ = std::fs::remove_dir_all(&feature);
        write_fixture(
            &release,
            "[package]\nname = \"probe\"\nversion = \"0.1.0\"\nedition = \"2021\"\n",
            "#[cfg(not(debug_assertions))]\nmacro_rules! spawn { ($m:ident) => { std::$m::Command::new(\"true\").status() }; }\n#[cfg(not(debug_assertions))]\npub fn sneak() { let _ = spawn!(process); }\n#[cfg(debug_assertions)]\npub fn sneak() {}\n",
        );
        assert!(
            !clippy_fixture(&release, &workspace, &["--profile", "release"]),
            "a release-only splice must fail clippy"
        );
        write_fixture(
            &feature,
            "[package]\nname = \"probe\"\nversion = \"0.1.0\"\nedition = \"2021\"\n[features]\nextra = []\n",
            "#[cfg(feature = \"extra\")]\nmacro_rules! spawn { ($m:ident) => { std::$m::Command::new(\"true\").status() }; }\n#[cfg(feature = \"extra\")]\npub fn sneak() { let _ = spawn!(process); }\n#[cfg(not(feature = \"extra\"))]\npub fn sneak() {}\n",
        );
        assert!(
            !clippy_fixture(&feature, &workspace, &["--all-features"]),
            "a feature-gated splice must fail when the feature is on"
        );
        assert!(
            clippy_fixture(&feature, &workspace, &["--no-default-features"]),
            "the same crate without the feature is clean"
        );
        let _ = std::fs::remove_dir_all(&release);
        let _ = std::fs::remove_dir_all(&feature);
    }

    fn write_fixture(root: &std::path::Path, manifest: &str, source: &str) {
        std::fs::create_dir_all(root.join("src")).expect("src");
        std::fs::write(root.join("Cargo.toml"), manifest).expect("manifest");
        std::fs::write(root.join("src/lib.rs"), source).expect("source");
    }

    fn clippy_fixture(root: &std::path::Path, workspace: &std::path::Path, extra: &[&str]) -> bool {
        let mut args = vec!["clippy", "--offline", "--manifest-path"];
        let manifest = root.join("Cargo.toml");
        let manifest = manifest.to_str().expect("utf-8");
        args.push(manifest);
        args.extend(extra);
        args.push("--");
        args.extend(super::CLIPPY_ARGS);
        std::process::Command::new("cargo")
            .args(&args)
            .env("CLIPPY_CONF_DIR", super::clippy_conf_dir(workspace))
            .env("CARGO_TARGET_DIR", root.join("target"))
            .env_remove("CARGO_MANIFEST_DIR")
            .status()
            .expect("clippy")
            .success()
    }

    use std::path::PathBuf;
}
