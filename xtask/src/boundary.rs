//! Cargo and source boundary for plugin packages.
//!
//! Plugins may depend only on `hofund-plugin-api` and `hofund-plugin-sdk`
//! (plus their own test kit and `tempfile` in dev-dependencies), and those two
//! crates must be `workspace = true`.
//! Clippy, configured by `xtask/plugin-boundary/clippy.toml`, sees a macro
//! fragment such as `std::$m::Command` after expansion. This module does not
//! try to match `$` splices in source text. The threat model is
//! `docs/plugins/boundary.md`.

use std::collections::{BTreeMap, BTreeSet};
use std::fs;
use std::path::{Component, Path, PathBuf};
use std::process::Command;

use syn::ext::IdentExt;
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
/// Each entry is `(profile, feature_flag)`. An empty flag is the default
/// feature set, the one that ships. Dev and release both run. A package that
/// defines `[features]` is also linted with `--all-features` and
/// `--no-default-features`. A package with no features is linted once per
/// profile, because those three sets compile the same crate. Other feature
/// combinations are not compiled.
#[must_use]
pub fn clippy_passes(defines_features: bool) -> Vec<(&'static str, &'static str)> {
    let sets: &[&str] = if defines_features {
        &["", "--all-features", "--no-default-features"]
    } else {
        &[""]
    };
    let mut passes = Vec::with_capacity(sets.len() * 2);
    for profile in ["dev", "release"] {
        for features in sets {
            passes.push((profile, *features));
        }
    }
    passes
}

/// Plugin package names allowed to ship a build script.
///
/// Empty. A build script runs before Clippy and can set `cfg`, so a plugin
/// does not get one until the name is added here after review. That covers a
/// root `build.rs` and any `[package] build` path. `build = false` is the
/// only manifest value that does not name a script. An allow-listed script is
/// still scanned, and it still may not emit `rustc-cfg`.
pub const BUILD_SCRIPT_ALLOW: &[&str] = &[];

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
    if dev && (own_kit == Some(name) || name == "tempfile") {
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
    let (mut hits, _) = scan_parsed(Path::new("src/lib.rs"), Path::new("src"), text);
    hits.sort();
    hits.dedup();
    hits
}

/// Symlinks, `#[path]` escapes, target `path` keys, and includes under a plugin package.
///
/// The file set is every `.rs` file under `src/` that is not a symlink, plus a
/// string-literal `include!` that stays inside `src/`, and in-root `#[path]`
/// targets (any extension). A symlink anywhere in the package is rejected and
/// not followed. That walk is a superset of one build: a `cfg` branch Cargo
/// did not compile is still scanned. Dep-info is a second pass, after Cargo,
/// in [`scan_compiled_sources`].
#[must_use]
pub fn source_violations(package_dir: &Path) -> Vec<String> {
    let mut hits = Vec::new();
    let mut seen = BTreeSet::new();
    let src = package_dir.join("src");
    hits.extend(package_symlinks(package_dir));
    if src.is_dir() {
        for file in rust_files(&src) {
            hits.extend(scan_file(&file, &src, &mut seen));
        }
    }
    if let Ok(text) = fs::read_to_string(package_dir.join("Cargo.toml")) {
        hits.extend(explicit_target_paths(&text));
    }
    let name = manifest_package_name(package_dir);
    let allowed = BUILD_SCRIPT_ALLOW.contains(&name.as_str());
    match package_build_key(package_dir) {
        BuildKey::Absent | BuildKey::Disabled => {}
        BuildKey::Script(rel) => {
            if !allowed {
                hits.push("package.build is not allowed".to_owned());
            }
            if let Some(path) = build_script_path(package_dir, &rel) {
                hits.extend(scan_build_script(&path, package_dir, &mut seen));
            }
        }
        BuildKey::Rejected => hits.push("package.build is not allowed".to_owned()),
    }
    let build = package_dir.join("build.rs");
    if build.is_file() {
        if !allowed {
            hits.push("build.rs is not allowed".to_owned());
        }
        hits.extend(scan_build_script(&build, package_dir, &mut seen));
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
    let (mut hits, extra) = scan_parsed(file, root, &text);
    for next in extra {
        if next.is_file() {
            hits.extend(scan_file(&next, root, seen));
        }
    }
    hits
}

fn scan_build_script(path: &Path, root: &Path, seen: &mut BTreeSet<PathBuf>) -> Vec<String> {
    let mut hits = Vec::new();
    if let Ok(text) = fs::read_to_string(path) {
        hits.extend(build_script_cfg_hits(&text));
    }
    hits.extend(scan_file(path, root, seen));
    hits
}

enum BuildKey {
    Absent,
    Disabled,
    Script(String),
    Rejected,
}

fn package_build_key(dir: &Path) -> BuildKey {
    let Ok(text) = fs::read_to_string(dir.join("Cargo.toml")) else {
        return BuildKey::Absent;
    };
    let Ok(value) = toml::from_str::<toml::Value>(&text) else {
        return BuildKey::Absent;
    };
    let Some(build) = value
        .get("package")
        .and_then(|package| package.get("build"))
    else {
        return BuildKey::Absent;
    };
    match build {
        toml::Value::Boolean(false) => BuildKey::Disabled,
        toml::Value::String(path) => BuildKey::Script(path.clone()),
        _ => BuildKey::Rejected,
    }
}

fn build_script_path(package_dir: &Path, rel: &str) -> Option<PathBuf> {
    let rel_path = Path::new(rel);
    if rel.is_empty() || rel_path.is_absolute() {
        return None;
    }
    let joined = normalize(&package_dir.join(rel_path));
    let root = normalize(package_dir);
    joined.starts_with(&root).then_some(joined)
}

fn scan_parsed(file: &Path, root: &Path, text: &str) -> (Vec<String>, Vec<PathBuf>) {
    let mut hits = lex_cfg_hits(text);
    let Ok(parsed) = syn::parse_file(text) else {
        hits.push(format!("{} does not parse", file.display()));
        return (hits, Vec::new());
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
        extra: Vec::new(),
    };
    finder.visit_file(&parsed);
    hits.extend(finder.hits);
    (hits, finder.extra)
}

const TARGET_KINDS: &[&str] = &["lib", "bin", "test", "example", "bench"];

/// `[lib]`, `[[bin]]`, `[[test]]`, `[[example]]`, and `[[bench]]` must not set `path`.
fn explicit_target_paths(text: &str) -> Vec<String> {
    let Ok(value) = toml::from_str::<toml::Value>(text) else {
        return Vec::new();
    };
    let mut hits = Vec::new();
    for kind in TARGET_KINDS {
        match value.get(*kind) {
            Some(toml::Value::Table(table)) => {
                if table.contains_key("path") {
                    hits.push(format!("{kind} target sets path"));
                }
            }
            Some(toml::Value::Array(items)) => {
                for item in items {
                    let sets_path = item
                        .as_table()
                        .is_some_and(|table| table.contains_key("path"));
                    if sets_path {
                        hits.push(format!("{kind} target sets path"));
                    }
                }
            }
            _ => {}
        }
    }
    hits
}

fn manifest_package_name(dir: &Path) -> String {
    let Ok(text) = fs::read_to_string(dir.join("Cargo.toml")) else {
        return String::new();
    };
    let Ok(value) = toml::from_str::<toml::Value>(&text) else {
        return String::new();
    };
    value
        .get("package")
        .and_then(|package| package.get("name"))
        .and_then(toml::Value::as_str)
        .unwrap_or("")
        .to_owned()
}

/// `cargo:rustc-cfg`, `cargo::rustc-cfg`, and `rustc-check-cfg`.
fn build_script_cfg_hits(text: &str) -> Vec<String> {
    let mut hits = Vec::new();
    if text.contains("rustc-check-cfg") {
        hits.push("build.rs emits rustc-check-cfg".to_owned());
    }
    if text.contains("rustc-cfg") {
        hits.push("build.rs emits rustc-cfg".to_owned());
    }
    hits
}

#[derive(Default)]
struct Aliases {
    crates: BTreeMap<String, String>,
    globs: BTreeSet<String>,
    /// Local name to `include`, `include_str`, or `include_bytes`.
    includes: BTreeMap<String, String>,
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

struct SourceFinder<'a> {
    file: &'a Path,
    root: &'a Path,
    aliases: Aliases,
    hits: Vec<String>,
    /// In-root `#[path]` targets and in-src `include!` files.
    extra: Vec<PathBuf>,
}

impl<'ast> Visit<'ast> for SourceFinder<'_> {
    fn visit_item_mod(&mut self, item: &'ast syn::ItemMod) {
        for attr in &item.attrs {
            let Some(rel) = path_attribute(attr) else {
                continue;
            };
            if rel.is_empty() || escapes(self.file, &rel, self.root) {
                self.hits
                    .push(format!("#[path] = \"{rel}\" leaves the package source"));
                continue;
            }
            let Some(parent) = self.file.parent() else {
                continue;
            };
            self.extra.push(parent.join(rel));
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
            .map(|segment| segment.ident.unraw().to_string())
        {
            let kind = self
                .aliases
                .includes
                .get(&name)
                .cloned()
                .unwrap_or_else(|| name.clone());
            if is_include_macro(&kind) {
                match syn::parse2::<syn::LitStr>(mac.tokens.clone()) {
                    Err(_) => self.hits.push(format!("non-literal {kind}!")),
                    Ok(lit) => match resolved_include(self.file, &lit.value(), self.root) {
                        Some(path) if kind == "include" => self.extra.push(path),
                        Some(_) => {}
                        None => self
                            .hits
                            .push(format!("include path \"{}\" leaves src", lit.value())),
                    },
                }
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
            if is_include_macro(&original) {
                aliases
                    .includes
                    .insert(ident(&name.rename), original.clone());
            }
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
        syn::UseTree::Name(name) => {
            let original = ident(&name.ident);
            if is_include_macro(&original) {
                aliases.includes.insert(original.clone(), original);
            }
        }
    }
}

fn is_include_macro(name: &str) -> bool {
    matches!(name, "include" | "include_str" | "include_bytes")
}

/// A string-literal include path, joined lexically and kept inside `root`.
///
/// Symlinks are not followed. An absolute path, or one that normalizes outside
/// `root`, is not an include the scan will open.
fn resolved_include(file: &Path, rel: &str, root: &Path) -> Option<PathBuf> {
    let rel_path = Path::new(rel);
    if rel.is_empty() || rel_path.is_absolute() {
        return None;
    }
    let parent = file.parent()?;
    let joined = normalize(&parent.join(rel_path));
    under_dir(&joined, root).then_some(joined)
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

fn ident_eq(ident: &proc_macro2::Ident, word: &str) -> bool {
    ident.unraw() == word
}

fn path_eq(path: &syn::Path, word: &str) -> bool {
    path.get_ident().is_some_and(|ident| ident_eq(ident, word))
}

fn mentions_cfg_word(meta: &syn::Meta, word: &str) -> bool {
    if path_eq(meta.path(), word) {
        return true;
    }
    let syn::Meta::List(list) = meta else {
        return false;
    };
    nested_metas(list)
        .iter()
        .any(|meta| mentions_cfg_word(meta, word))
}

/// One scanned file, lexed as tokens. Parsing is not required: a `cfg` inside
/// a macro argument that is not Rust is still visible here.
fn lex_cfg_hits(text: &str) -> Vec<String> {
    let mut hits = Vec::new();
    match text.parse::<proc_macro2::TokenStream>() {
        Ok(tokens) => scan_cfg_tokens(tokens, &mut hits),
        Err(_) => hits.push("source does not lex".to_owned()),
    }
    hits
}

/// `cfg` / `cfg_attr` / `cfg!` anywhere in a token tree. Identifiers are
/// compared with [`proc_macro2::Ident::unraw`], so `r#clippy` is `clippy`.
fn scan_cfg_tokens(tokens: proc_macro2::TokenStream, hits: &mut Vec<String>) {
    let trees: Vec<proc_macro2::TokenTree> = tokens.into_iter().collect();
    let mut index = 0;
    while index < trees.len() {
        match &trees[index] {
            proc_macro2::TokenTree::Group(group) => {
                scan_cfg_tokens(group.stream(), hits);
                index += 1;
            }
            proc_macro2::TokenTree::Ident(ident)
                if ident_eq(ident, "cfg") || ident_eq(ident, "cfg_attr") =>
            {
                let mut next = index + 1;
                if let Some(proc_macro2::TokenTree::Punct(punct)) = trees.get(next)
                    && punct.as_char() == '!'
                {
                    next += 1;
                }
                if let Some(proc_macro2::TokenTree::Group(group)) = trees.get(next)
                    && group.delimiter() == proc_macro2::Delimiter::Parenthesis
                {
                    let inner = group.stream();
                    if tokens_mention(&inner, "clippy") {
                        hits.push("cfg(clippy)".to_owned());
                    }
                    if tokens_mention(&inner, "debug_assertions") {
                        hits.push("cfg(debug_assertions)".to_owned());
                    }
                    scan_cfg_tokens(inner, hits);
                    index = next + 1;
                } else {
                    index += 1;
                }
            }
            _ => index += 1,
        }
    }
}

fn tokens_mention(tokens: &proc_macro2::TokenStream, word: &str) -> bool {
    tokens.clone().into_iter().any(|tree| match tree {
        proc_macro2::TokenTree::Ident(ident) => ident_eq(&ident, word),
        proc_macro2::TokenTree::Group(group) => tokens_mention(&group.stream(), word),
        _ => false,
    })
}

fn attribute_cfg_word(attr: &syn::Attribute, word: &str) -> bool {
    let Some(name) = attr.path().get_ident() else {
        return false;
    };
    if ident_eq(name, "cfg") {
        return attr
            .parse_args::<syn::Meta>()
            .ok()
            .is_some_and(|meta| mentions_cfg_word(&meta, word));
    }
    if ident_eq(name, "cfg_attr") {
        let syn::Meta::List(list) = &attr.meta else {
            return false;
        };
        // The predicate and every attribute it applies. `cfg(not(clippy))`
        // is an attribute, not the predicate, in `cfg_attr(all(), cfg(...))`.
        return nested_metas(list)
            .iter()
            .any(|meta| mentions_cfg_word(meta, word));
    }
    false
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
    !under_dir(&joined, root)
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
        let Ok(meta) = fs::symlink_metadata(&path) else {
            continue;
        };
        if meta.file_type().is_symlink() {
            continue;
        }
        if meta.is_dir() {
            files.extend(rust_files(&path));
        } else if path.extension().and_then(|ext| ext.to_str()) == Some("rs") {
            files.push(path);
        }
    }
    files
}

/// Every symlink in the package, file or directory. The link is not followed.
fn package_symlinks(dir: &Path) -> Vec<String> {
    let mut hits = Vec::new();
    walk_symlinks(dir, &mut hits);
    hits
}

fn walk_symlinks(dir: &Path, hits: &mut Vec<String>) {
    let Ok(entries) = fs::read_dir(dir) else {
        return;
    };
    for entry in entries.flatten() {
        let path = entry.path();
        let Ok(meta) = fs::symlink_metadata(&path) else {
            hits.push(format!("unreadable {}", path.display()));
            continue;
        };
        if meta.file_type().is_symlink() {
            hits.push(format!("symlink {}", path.display()));
            continue;
        }
        if meta.is_dir() {
            walk_symlinks(&path, hits);
        }
    }
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

/// Names of the other workspace packages whose normal and build closure
/// contains `package`: the packages that link it, directly or transitively.
///
/// # Errors
///
/// Returns an error when Cargo metadata fails or the package is not in the workspace.
pub fn linkers_of(workspace: &Path, package: &str) -> Result<BTreeSet<String>, String> {
    let meta = cargo_metadata(workspace)?;
    let target = workspace_package_id(&meta, package, workspace)?;
    let root = normalize(workspace);
    let mut linkers = BTreeSet::new();
    for (name, id, manifest) in &meta.ids_by_manifest {
        if *id != target
            && under_dir(Path::new(manifest), &root)
            && closure(&meta, id).contains(&target)
        {
            linkers.insert(name.clone());
        }
    }
    Ok(linkers)
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
    /// Manifest directory, target crate name, and `src_path`.
    targets: Vec<(PathBuf, String, PathBuf)>,
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
    let mut targets = Vec::new();
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
            let manifest_dir = Path::new(manifest)
                .parent()
                .unwrap_or(Path::new(manifest))
                .to_path_buf();
            let Some(list) = package.get("targets").and_then(serde_json::Value::as_array) else {
                continue;
            };
            for target in list {
                let Some(target_name) = target.get("name").and_then(serde_json::Value::as_str)
                else {
                    continue;
                };
                let Some(src_path) = target.get("src_path").and_then(serde_json::Value::as_str)
                else {
                    continue;
                };
                targets.push((
                    manifest_dir.clone(),
                    target_name.to_owned(),
                    PathBuf::from(src_path),
                ));
            }
        }
    }
    if nodes.is_empty() {
        return Err("cargo metadata listed no resolve nodes".to_owned());
    }
    Ok(Metadata {
        nodes,
        ids_by_manifest,
        targets,
    })
}

/// Every Cargo target root for this package must sit under its `src/` directory.
///
/// An explicit `[lib]` / `[[bin]]` / `[[test]]` / `[[example]]` / `[[bench]]`
/// `path` is already rejected by [`source_violations`]. This is the backstop:
/// `cargo metadata` `targets[].src_path` is what rustc will compile.
///
/// # Errors
///
/// Returns an error when Cargo metadata fails, the package has no targets, or
/// a target root is outside `src/`.
pub fn target_roots_under_src(workspace: &Path, package_dir: &Path) -> Result<(), String> {
    let meta = cargo_metadata(workspace)?;
    let errors = target_root_errors(&meta, package_dir);
    if errors.is_empty() {
        Ok(())
    } else {
        Err(errors.join("; "))
    }
}

fn target_root_errors(meta: &Metadata, package_dir: &Path) -> Vec<String> {
    let src = package_dir.join("src");
    let roots: Vec<&Path> = meta
        .targets
        .iter()
        .filter(|(dir, _, _)| same_root(dir, package_dir))
        .map(|(_, _, src_path)| src_path.as_path())
        .collect();
    if roots.is_empty() {
        return vec![format!(
            "cargo metadata listed no targets for {}",
            package_dir.display()
        )];
    }
    let mut errors = Vec::new();
    for src_path in roots {
        if !under_dir(src_path, &src) {
            errors.push(format!(
                "target root {} is outside {}",
                src_path.display(),
                src.display()
            ));
        }
    }
    errors
}

/// Scan sources rustc listed for this plugin, and reject anything outside `src/`.
///
/// Runs after `cargo check` and the plugin Clippy passes. Dep-info (`.d`) for
/// each of the plugin's own targets is parsed. A listed file inside the
/// package `src/` directory is lexed, whatever its extension. A file that will
/// not lex is rejected. A plugin has no build script, so a listed path in the
/// build directory, under `crates/kernel/plugin-api` or `crates/kernel/plugin-sdk`, or
/// anywhere else outside `src/` is rejected. The workspace `Cargo.toml`,
/// `Cargo.lock`, and `xtask/plugin-boundary/clippy.toml` are Clippy rebuild
/// inputs and are ignored. Paths are classified without following symlinks.
/// The package root and the workspace root are the only paths canonicalized.
///
/// # Errors
///
/// Returns an error when metadata or dep-info cannot be read, no source under
/// `src/` was listed, a listed file will not lex, the scan finds a boundary
/// hit, or a listed file lies outside `src/`.
pub fn scan_compiled_sources(workspace: &Path, package_dir: &Path) -> Result<(), String> {
    scan_dep_info(workspace, package_dir, &cargo_target_dir(workspace))
}

fn cargo_target_dir(workspace: &Path) -> PathBuf {
    std::env::var_os("CARGO_TARGET_DIR")
        .map(PathBuf::from)
        .filter(|path| !path.as_os_str().is_empty())
        .unwrap_or_else(|| workspace.join("target"))
}

fn scan_dep_info(workspace: &Path, package_dir: &Path, target_dir: &Path) -> Result<(), String> {
    let meta = cargo_metadata(workspace)?;
    let names: Vec<String> = meta
        .targets
        .iter()
        .filter(|(dir, _, _)| same_root(dir, package_dir))
        .map(|(_, name, _)| name.clone())
        .collect();
    if names.is_empty() {
        return Err(format!(
            "cargo metadata listed no targets for {}",
            package_dir.display()
        ));
    }
    let mut dep_files = Vec::new();
    for profile in ["debug", "release"] {
        let profile_dir = target_dir.join(profile);
        dep_files.extend(matching_dep_info(&profile_dir, &names));
        dep_files.extend(matching_dep_info(&profile_dir.join("deps"), &names));
    }
    let mut listed = BTreeSet::new();
    for dep in &dep_files {
        let text =
            fs::read_to_string(dep).map_err(|error| format!("read {}: {error}", dep.display()))?;
        for raw in dep_info_prerequisites(&text) {
            listed.insert(resolve_listed(workspace, &raw));
        }
    }
    if listed.is_empty() {
        return Err(format!(
            "dep-info listed no sources for {}",
            package_dir.display()
        ));
    }
    let src = package_dir.join("src");
    let mut hits = Vec::new();
    let mut seen = BTreeSet::new();
    let mut saw_package_source = false;
    for path in listed {
        if !path.exists() {
            // An older dep-info can name a file a later build removed.
            continue;
        }
        match classify_listed(&path, package_dir, workspace) {
            Listed::Inside => {
                saw_package_source = true;
                hits.extend(scan_listed(&path, &src, &mut seen));
            }
            Listed::Ignored => {}
            Listed::Outside => {
                hits.push(format!("dep-info lists {} outside src", path.display()));
            }
        }
    }
    if !saw_package_source {
        hits.push(format!(
            "dep-info listed no sources for {}",
            package_dir.display()
        ));
    }
    if hits.is_empty() {
        Ok(())
    } else {
        hits.sort();
        hits.dedup();
        Err(hits.join("; "))
    }
}

enum Listed {
    Inside,
    Ignored,
    Outside,
}

/// A plugin target's dep-info may name files inside `src/`, or three Clippy
/// rebuild inputs. Every other existing path, including the build directory
/// and `crates/kernel/plugin-api` / `crates/kernel/plugin-sdk`, is outside `src/`.
fn classify_listed(path: &Path, package_dir: &Path, workspace: &Path) -> Listed {
    if is_rebuild_input(path, workspace) {
        return Listed::Ignored;
    }
    if under_dir(path, &package_dir.join("src")) {
        return Listed::Inside;
    }
    Listed::Outside
}

fn scan_listed(file: &Path, root: &Path, seen: &mut BTreeSet<PathBuf>) -> Vec<String> {
    let text = match fs::read_to_string(file) {
        Ok(text) => text,
        Err(_) => return vec!["source does not lex".to_owned()],
    };
    let is_rs = file.extension().and_then(|ext| ext.to_str()) == Some("rs");
    if is_rs || syn::parse_file(&text).is_ok() {
        return scan_file(file, root, seen);
    }
    if !seen.insert(normalize(file)) {
        return Vec::new();
    }
    lex_cfg_hits(&text)
}

fn matching_dep_info(dir: &Path, crate_names: &[String]) -> Vec<PathBuf> {
    let mut found = Vec::new();
    let Ok(entries) = fs::read_dir(dir) else {
        return found;
    };
    for entry in entries.flatten() {
        let path = entry.path();
        if path.extension().and_then(|ext| ext.to_str()) != Some("d") {
            continue;
        }
        let Some(stem) = path.file_stem().and_then(|stem| stem.to_str()) else {
            continue;
        };
        if crate_names.iter().any(|name| dep_stem_matches(stem, name)) {
            found.push(path);
        }
    }
    found
}

fn dep_stem_matches(stem: &str, crate_name: &str) -> bool {
    let bare = format!("{crate_name}-");
    let lib = format!("lib{crate_name}-");
    stem.starts_with(&bare) || stem.starts_with(&lib)
}

/// Makefile prerequisites from a rustc dep-info file.
///
/// The separator is the first colon followed by whitespace or the end of the
/// line. A colon followed by `\` or `/` is a Windows drive, not the separator.
/// `\` escapes a space, `#`, `$`, or another backslash. Full-line `#` comments
/// and blank lines are skipped.
fn dep_info_prerequisites(text: &str) -> Vec<String> {
    let mut found = Vec::new();
    for line in logical_dep_lines(text) {
        let Some((_, deps)) = split_dep_rule(&line) else {
            continue;
        };
        found.extend(split_make_words(deps));
    }
    found
}

fn logical_dep_lines(text: &str) -> Vec<String> {
    let mut lines = Vec::new();
    let mut current = String::new();
    for line in text.split('\n') {
        let line = line.trim_end_matches('\r');
        if current.is_empty() && line.starts_with('#') {
            continue;
        }
        let (body, cont) = split_continuation(line);
        current.push_str(body);
        if cont {
            continue;
        }
        if !current.trim().is_empty() {
            lines.push(std::mem::take(&mut current));
        } else {
            current.clear();
        }
    }
    if !current.trim().is_empty() {
        lines.push(current);
    }
    lines
}

fn split_continuation(line: &str) -> (&str, bool) {
    let bytes = line.as_bytes();
    let mut count = 0;
    while count < bytes.len() && bytes[bytes.len() - 1 - count] == b'\\' {
        count += 1;
    }
    if count % 2 == 1 {
        (&line[..bytes.len() - 1], true)
    } else {
        (line, false)
    }
}

fn split_dep_rule(line: &str) -> Option<(&str, &str)> {
    let mut chars = line.char_indices().peekable();
    while let Some((index, ch)) = chars.next() {
        if ch != ':' {
            continue;
        }
        let next = chars.peek().map(|(_, next)| *next);
        if matches!(next, None | Some(' ') | Some('\t')) {
            let deps = line[index + ch.len_utf8()..].trim_start_matches([' ', '\t']);
            return Some((&line[..index], deps));
        }
    }
    None
}

fn split_make_words(input: &str) -> Vec<String> {
    let mut words = Vec::new();
    let mut current = String::new();
    let mut chars = input.chars().peekable();
    while let Some(ch) = chars.next() {
        if ch == '\\' {
            match chars.next() {
                Some(' ') => current.push(' '),
                Some('#') => current.push('#'),
                Some('$') => current.push('$'),
                Some('\\') => current.push('\\'),
                Some(other) => {
                    current.push('\\');
                    current.push(other);
                }
                None => current.push('\\'),
            }
            continue;
        }
        if ch == ' ' || ch == '\t' {
            if !current.is_empty() {
                words.push(std::mem::take(&mut current));
            }
            continue;
        }
        current.push(ch);
    }
    if !current.is_empty() {
        words.push(current);
    }
    words
}

fn resolve_listed(workspace: &Path, raw: &str) -> PathBuf {
    let path = PathBuf::from(raw);
    if path.is_absolute() {
        lexical_path(&path)
    } else {
        lexical_path(&workspace.join(path))
    }
}

/// Unresolved path: drop a verbatim `\\?\` prefix and collapse `.` / `..`.
///
/// Symlinks are not followed. A Windows short name stays a short name.
fn lexical_path(path: &Path) -> PathBuf {
    let text = path.to_string_lossy();
    let stripped = text
        .strip_prefix(r"\\?\")
        .or_else(|| text.strip_prefix("//?/"))
        .unwrap_or(text.as_ref());
    normalize(Path::new(stripped))
}

/// Canonical spelling of a directory root, used only to compare two roots.
///
/// File paths stay lexical. This is what makes macOS `/var` match
/// `/private/var`, and a Windows 8.3 directory match its long name, without
/// moving a symlink inside the package onto the directory it points at.
fn canonical_root(path: &Path) -> PathBuf {
    let lexical = lexical_path(path);
    if !lexical.is_absolute() {
        return lexical;
    }
    match fs::canonicalize(&lexical) {
        Ok(canonical) => lexical_path(&canonical),
        Err(_) => lexical,
    }
}

fn same_root(a: &Path, b: &Path) -> bool {
    if !a.is_absolute() || !b.is_absolute() {
        return lexical_path(a) == lexical_path(b);
    }
    canonical_root(a) == canonical_root(b)
}

/// `path` is inside `dir` by its unresolved spelling.
///
/// A relative `dir` is only a lexical prefix. Canonicalizing it would use the
/// process cwd. An absolute `dir` that exists is also compared after
/// canonicalizing that root alone.
fn under_dir(path: &Path, dir: &Path) -> bool {
    let path = lexical_path(path);
    let lexical = lexical_path(dir);
    if path.starts_with(&lexical) {
        return true;
    }
    if dir.is_absolute()
        && let Ok(canonical) = fs::canonicalize(dir)
    {
        return path.starts_with(lexical_path(&canonical));
    }
    false
}

/// Clippy records these next to the crate sources. They are not scanned.
const REBUILD_INPUTS: &[&str] = &[
    "Cargo.toml",
    "Cargo.lock",
    "xtask/plugin-boundary/clippy.toml",
];

fn is_rebuild_input(path: &Path, workspace: &Path) -> bool {
    let path = lexical_path(path);
    let mut roots = vec![lexical_path(workspace)];
    if workspace.is_absolute()
        && let Ok(canonical) = fs::canonicalize(workspace)
    {
        let canonical = lexical_path(&canonical);
        if canonical != roots[0] {
            roots.push(canonical);
        }
    }
    roots
        .iter()
        .any(|root| REBUILD_INPUTS.iter().any(|rel| path == root.join(rel)))
}

fn workspace_package_id(meta: &Metadata, name: &str, workspace: &Path) -> Result<String, String> {
    let workspace = normalize(workspace);
    let mut found = Vec::new();
    for (package, id, manifest) in &meta.ids_by_manifest {
        if package == name && under_dir(Path::new(manifest), &workspace) {
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

/// True when `plugins/registry` contains a nested Cargo package.
///
/// The registry crate's own manifest, at the root of this directory, is the
/// composition crate. A package in a subdirectory is still rejected.
///
/// # Errors
///
/// Returns an error when the directory cannot be read.
pub fn registry_has_package(dir: &Path) -> Result<bool, String> {
    if !dir.exists() {
        return Ok(false);
    }
    let entries = fs::read_dir(dir).map_err(|error| format!("read {}: {error}", dir.display()))?;
    for entry in entries {
        let entry = entry.map_err(|error| format!("read {}: {error}", dir.display()))?;
        let path = entry.path();
        if path.is_dir() && contains_manifest(&path)? {
            return Ok(true);
        }
    }
    Ok(false)
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
    fn scratch() -> std::path::PathBuf {
        thread_local! {
            static GUARDS: std::cell::RefCell<Vec<tempfile::TempDir>> = const { std::cell::RefCell::new(Vec::new()) };
        }
        GUARDS.with(|guards| {
            let guard = tempfile::tempdir().expect("temp dir");
            let path = guard.path().to_path_buf();
            guards.borrow_mut().push(guard);
            path
        })
    }
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
        assert_eq!(dependency_error("tempfile", &[], true, None), None);
        assert_eq!(
            dependency_error("tempfile", &[], false, None).as_deref(),
            Some("depends on tempfile")
        );
    }

    #[test]
    fn path_attributes_and_computed_includes_are_rejected() {
        let root = scratch().join(format!("hofund-plugin-boundary-{}", std::process::id()));
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
        let root = scratch().join(format!("hofund-plugin-macro-{}", std::process::id()));
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
            (
                "#[cfg_attr(all(), cfg(not(clippy)))]\nmacro_rules! spawn { ($m:ident) => { std::$m::Command::new(\"true\").status() }; }\n#[cfg_attr(all(), cfg(not(clippy)))]\npub fn sneak() { let _ = spawn!(process); }\n",
                "cfg(clippy)",
            ),
            (
                "#[cfg_attr(all(), cfg_attr(all(), cfg(not(clippy))))]\nfn sneak() {}\n",
                "cfg(clippy)",
            ),
            (
                "macro_rules! spawn { ($m:ident) => { #[cfg(not(clippy))] { std::$m::Command::new(\"true\").status() } }; }\n",
                "cfg(clippy)",
            ),
            (
                "macro_rules! spawn { ($m:ident) => { #[cfg(not(debug_assertions))] { std::$m::Command::new(\"true\").status() } }; }\n",
                "cfg(debug_assertions)",
            ),
            (
                "macro_rules! pass { (@ $($t:tt)*) => { $($t)* }; }\nmacro_rules! spawn { ($m:ident) => { std::$m::Command::new(\"true\").status() }; }\npass! { @ #[cfg(not(clippy))] pub fn sneak() { let _ = spawn!(process); } #[cfg(clippy)] pub fn sneak() {} }\n",
                "cfg(clippy)",
            ),
            (
                "#[cfg(not(r#clippy))]\nmacro_rules! spawn { ($m:ident) => { std::$m::Command::new(\"true\").status() }; }\n#[cfg(not(r#clippy))]\npub fn sneak() { let _ = spawn!(process); }\n#[cfg(r#clippy)]\npub fn sneak() {}\n",
                "cfg(clippy)",
            ),
            (
                "#[cfg(not(r#debug_assertions))]\nmacro_rules! spawn { ($m:ident) => { std::$m::Command::new(\"true\").status() }; }\n#[cfg(not(r#debug_assertions))]\npub fn sneak() { let _ = spawn!(process); }\n#[cfg(r#debug_assertions)]\npub fn sneak() {}\n",
                "cfg(debug_assertions)",
            ),
            ("#[r#cfg(not(r#clippy))]\nfn sneak() {}\n", "cfg(clippy)"),
            (
                "fn sneak() { if cfg!(r#debug_assertions) { let _ = std::process::id(); } }\n",
                "cfg(debug_assertions)",
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
            ids.iter()
                .any(|id| id.contains("crates/kernel/plugin-api#")),
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
        assert_eq!(super::clippy_passes(false), [("dev", ""), ("release", "")]);
        assert_eq!(
            super::clippy_passes(true),
            [
                ("dev", ""),
                ("dev", "--all-features"),
                ("dev", "--no-default-features"),
                ("release", ""),
                ("release", "--all-features"),
                ("release", "--no-default-features"),
            ]
        );
        assert!(super::BUILD_SCRIPT_ALLOW.is_empty());
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
        let release = scratch().join(format!("hofund-plugin-release-{}", std::process::id()));
        let feature = scratch().join(format!("hofund-plugin-feature-{}", std::process::id()));
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

    #[test]
    fn the_default_feature_set_is_linted() {
        let workspace = PathBuf::from(env!("CARGO_MANIFEST_DIR"))
            .parent()
            .expect("workspace")
            .to_path_buf();
        let root = scratch().join(format!("hofund-plugin-default-feat-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&root);
        write_fixture(
            &root,
            "[package]\nname = \"probe\"\nversion = \"0.1.0\"\nedition = \"2021\"\n[features]\ndefault = [\"a\"]\na = []\nb = []\n",
            "#[cfg(all(feature = \"a\", not(feature = \"b\")))]\nmacro_rules! spawn { ($m:ident) => { std::$m::Command::new(\"true\").status() }; }\n#[cfg(all(feature = \"a\", not(feature = \"b\")))]\npub fn sneak() { let _ = spawn!(process); }\n#[cfg(not(all(feature = \"a\", not(feature = \"b\"))))]\npub fn sneak() {}\n",
        );
        assert!(
            !clippy_fixture(&root, &workspace, &[]),
            "the default feature set must compile the splice"
        );
        assert!(
            clippy_fixture(&root, &workspace, &["--all-features"]),
            "every feature turns the splice off"
        );
        assert!(
            clippy_fixture(&root, &workspace, &["--no-default-features"]),
            "no features turns the splice off"
        );
        let _ = std::fs::remove_dir_all(&root);
    }

    #[test]
    fn a_build_script_cannot_set_cfg() {
        let root = scratch().join(format!("hofund-plugin-buildrs-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&root);
        std::fs::create_dir_all(root.join("src")).expect("src");
        std::fs::write(
            root.join("Cargo.toml"),
            "[package]\nname = \"evil\"\nversion = \"0.1.0\"\nedition = \"2021\"\n",
        )
        .expect("manifest");
        std::fs::write(
            root.join("src/lib.rs"),
            "#[cfg(fast_path)]\nfn sneak() {}\n",
        )
        .expect("lib");
        std::fs::write(
            root.join("build.rs"),
            "fn main() {\n    println!(\"cargo::rustc-check-cfg=cfg(fast_path)\");\n    if !std::env::var(\"RUSTC_WORKSPACE_WRAPPER\").unwrap_or_default().contains(\"clippy\") {\n        println!(\"cargo::rustc-cfg=fast_path\");\n        println!(\"cargo:rustc-cfg=fast_path\");\n    }\n}\n",
        )
        .expect("build");
        let hits = source_violations(&root).join("\n");
        assert!(hits.contains("build.rs is not allowed"), "{hits}");
        assert!(hits.contains("build.rs emits rustc-cfg"), "{hits}");
        assert!(hits.contains("build.rs emits rustc-check-cfg"), "{hits}");
        let _ = std::fs::remove_dir_all(&root);
    }

    #[test]
    fn a_renamed_build_script_is_rejected() {
        let root = scratch().join(format!("hofund-plugin-a25-{}", std::process::id()));
        let off = scratch().join(format!("hofund-plugin-a25-off-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&root);
        let _ = std::fs::remove_dir_all(&off);
        std::fs::create_dir_all(root.join("src")).expect("src");
        std::fs::write(
            root.join("Cargo.toml"),
            "[package]\nname = \"evil\"\nversion = \"0.1.0\"\nedition = \"2021\"\nbuild = \"src/gen.rs\"\n",
        )
        .expect("manifest");
        std::fs::write(
            root.join("src/lib.rs"),
            "#[cfg(fast_path)]\nfn sneak() {}\n",
        )
        .expect("lib");
        std::fs::write(
            root.join("src/gen.rs"),
            "fn main() {\n    println!(\"cargo::rustc-check-cfg=cfg(fast_path)\");\n    if !std::env::var(\"RUSTC_WORKSPACE_WRAPPER\").unwrap_or_default().contains(\"clippy\") {\n        println!(\"cargo::rustc-cfg=fast_path\");\n    }\n}\n",
        )
        .expect("gen");
        let hits = source_violations(&root).join("\n");
        assert!(hits.contains("package.build is not allowed"), "{hits}");
        assert!(hits.contains("build.rs emits rustc-cfg"), "{hits}");
        assert!(hits.contains("build.rs emits rustc-check-cfg"), "{hits}");
        std::fs::create_dir_all(off.join("src")).expect("off src");
        std::fs::write(
            off.join("Cargo.toml"),
            "[package]\nname = \"quiet\"\nversion = \"0.1.0\"\nedition = \"2021\"\nbuild = false\n",
        )
        .expect("off manifest");
        std::fs::write(off.join("src/lib.rs"), "fn clean() {}\n").expect("off lib");
        let off_hits = source_violations(&off).join("\n");
        assert!(
            !off_hits.contains("package.build") && !off_hits.contains("build.rs"),
            "{off_hits}"
        );
        let _ = std::fs::remove_dir_all(&root);
        let _ = std::fs::remove_dir_all(&off);
    }

    #[test]
    fn a_non_rs_path_module_is_scanned() {
        let root = scratch().join(format!("hofund-plugin-a34-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&root);
        std::fs::create_dir_all(root.join("src")).expect("src");
        std::fs::write(
            root.join("Cargo.toml"),
            "[package]\nname = \"probe-a34\"\nversion = \"0.1.0\"\nedition = \"2021\"\n",
        )
        .expect("manifest");
        std::fs::write(root.join("src/lib.rs"), "#[path = \"m.txt\"]\nmod m;\n").expect("lib");
        std::fs::write(
            root.join("src/m.txt"),
            "#[cfg(not(clippy))]\npub fn sneak() { let _ = std::process::Command::new(\"true\"); }\n#[cfg(clippy)]\npub fn sneak() {}\n",
        )
        .expect("module");
        let hits = source_violations(&root).join("\n");
        assert!(hits.contains("cfg(clippy)"), "{hits}");
        let target = root.join("target");
        let status = std::process::Command::new("cargo")
            .args(["check", "--offline", "--manifest-path"])
            .arg(root.join("Cargo.toml"))
            .current_dir(&root)
            .env("CARGO_TARGET_DIR", &target)
            .env_remove("CARGO_MANIFEST_DIR")
            .status()
            .expect("cargo check");
        assert!(status.success(), "cargo check of the path module");
        let scanned = super::scan_dep_info(&root, &root, &target).expect_err("m.txt is a source");
        assert!(scanned.contains("cfg(clippy)"), "{scanned}");
        let _ = std::fs::remove_dir_all(&root);
    }

    #[test]
    fn explicit_target_paths_are_rejected() {
        let lib = scratch().join(format!("hofund-plugin-a35-{}", std::process::id()));
        let bin = scratch().join(format!("hofund-plugin-a35-bin-{}", std::process::id()));
        let clean = scratch().join(format!("hofund-plugin-a35-ok-{}", std::process::id()));
        for dir in [&lib, &bin, &clean] {
            let _ = std::fs::remove_dir_all(dir);
            std::fs::create_dir_all(dir.join("src")).expect("src");
            std::fs::write(dir.join("src/lib.rs"), "fn clean() {}\n").expect("lib");
        }
        std::fs::write(
            lib.join("Cargo.toml"),
            "[package]\nname = \"moved\"\nversion = \"0.1.0\"\nedition = \"2021\"\n[lib]\npath = \"lib/lib.rs\"\n",
        )
        .expect("lib manifest");
        let lib_hits = source_violations(&lib).join("\n");
        assert!(lib_hits.contains("lib target sets path"), "{lib_hits}");
        std::fs::write(
            bin.join("Cargo.toml"),
            "[package]\nname = \"moved-bin\"\nversion = \"0.1.0\"\nedition = \"2021\"\n[[bin]]\nname = \"x\"\npath = \"bins/main.rs\"\n",
        )
        .expect("bin manifest");
        let bin_hits = source_violations(&bin).join("\n");
        assert!(bin_hits.contains("bin target sets path"), "{bin_hits}");
        for (kind, body) in [
            ("test", "[[test]]\nname = \"t\"\npath = \"tests/t.rs\"\n"),
            (
                "example",
                "[[example]]\nname = \"e\"\npath = \"examples/e.rs\"\n",
            ),
            (
                "bench",
                "[[bench]]\nname = \"b\"\npath = \"benches/b.rs\"\n",
            ),
        ] {
            let hits = super::explicit_target_paths(body).join("\n");
            assert!(hits.contains(&format!("{kind} target sets path")), "{hits}");
        }
        let explicit_src = super::explicit_target_paths("[lib]\npath = \"src/lib.rs\"\n");
        assert!(
            explicit_src.iter().any(|hit| hit == "lib target sets path"),
            "{explicit_src:?}"
        );
        std::fs::write(
            clean.join("Cargo.toml"),
            "[package]\nname = \"quiet\"\nversion = \"0.1.0\"\nedition = \"2021\"\nbuild = false\n[[bin]]\nname = \"demo\"\n",
        )
        .expect("clean manifest");
        let clean_hits = source_violations(&clean).join("\n");
        assert!(
            !clean_hits.contains("target sets path") && !clean_hits.contains("package.build"),
            "{clean_hits}"
        );
        for dir in [&lib, &bin, &clean] {
            let _ = std::fs::remove_dir_all(dir);
        }
    }

    #[test]
    fn dep_info_rejects_files_outside_the_package() {
        let root = scratch().join(format!("hofund-plugin-depinfo-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&root);
        let package = root.join("plugins/providers/probe");
        std::fs::create_dir_all(package.join("src")).expect("src");
        std::fs::write(
            package.join("Cargo.toml"),
            "[package]\nname = \"probe-dep\"\nversion = \"0.1.0\"\nedition = \"2021\"\n",
        )
        .expect("manifest");
        std::fs::write(package.join("src/lib.rs"), "fn clean() {}\n").expect("lib");
        let allowed = root.join("crates/kernel/plugin-api/sneak.txt");
        std::fs::create_dir_all(allowed.parent().expect("parent")).expect("api");
        std::fs::write(&allowed, "#[cfg(not(clippy))]\nfn sneak() {}\n").expect("allowed");
        let clippy_toml = root.join("xtask/plugin-boundary/clippy.toml");
        std::fs::create_dir_all(clippy_toml.parent().expect("parent")).expect("clippy dir");
        std::fs::write(&clippy_toml, "#[cfg(not(clippy))]\nfn sneak() {}\n").expect("clippy.toml");
        std::fs::write(
            root.join("Cargo.toml"),
            "[workspace]\nresolver = \"2\"\nmembers = [\"plugins/providers/probe\"]\n",
        )
        .expect("workspace");
        let outside = scratch().join(format!("hofund-not-allowed-{}.rs", std::process::id()));
        std::fs::write(&outside, "#[cfg(not(clippy))]\nfn sneak() {}\n").expect("outside");
        let bad = package.join("src/bad.txt");
        std::fs::write(&bad, "\"unterminated").expect("bad");
        let target = root.join("target/debug/deps");
        std::fs::create_dir_all(&target).expect("deps");
        let evil = root.join("target/evil.rs");
        std::fs::write(&evil, "#[cfg(not(clippy))]\nfn sneak() {}\n").expect("evil");
        std::fs::write(root.join("Cargo.lock"), "# lock\n").expect("lock");
        let listed = format!(
            "probe_dep-abc.d: {} {} {} {} {} {} {} {}\n",
            package.join("src/lib.rs").display(),
            allowed.display(),
            clippy_toml.display(),
            outside.display(),
            bad.display(),
            evil.display(),
            root.join("Cargo.toml").display(),
            root.join("Cargo.lock").display(),
        );
        std::fs::write(target.join("probe_dep-abc.d"), listed).expect("dep-info");
        let err =
            super::scan_dep_info(&root, &package, &root.join("target")).expect_err("outside file");
        assert!(err.contains("outside src"), "{err}");
        // The filename is stable. The directory spelling depends on TMPDIR.
        assert!(err.contains("hofund-not-allowed"), "{err}");
        assert!(err.contains("sneak.txt"), "{err}");
        assert!(err.contains("evil.rs"), "{err}");
        assert!(err.contains("source does not lex"), "{err}");
        assert!(!err.contains("cfg(clippy)"), "{err}");
        assert!(!err.contains("does not parse"), "{err}");
        assert!(!err.contains("clippy.toml"), "{err}");
        assert!(
            !err.contains(&root.join("Cargo.toml").display().to_string()),
            "{err}"
        );
        assert!(
            !err.contains(&root.join("Cargo.lock").display().to_string()),
            "{err}"
        );
        let _ = std::fs::remove_dir_all(&root);
        let _ = std::fs::remove_file(&outside);
    }

    #[test]
    fn dep_info_parser_keeps_drives_escapes_and_crate_stems() {
        let text = "C:\\out\\lib.d: C:\\pkg\\src\\lib.rs C:\\pkg\\my\\ src\\m.txt \\\n C:\\pkg\\src\\more.rs\n\nC:\\pkg\\src\\lib.rs:\n# env-dep:CLIPPY_ARGS=-D\n";
        let got = super::dep_info_prerequisites(text);
        assert_eq!(
            got,
            vec![
                "C:\\pkg\\src\\lib.rs".to_owned(),
                "C:\\pkg\\my src\\m.txt".to_owned(),
                "C:\\pkg\\src\\more.rs".to_owned(),
            ]
        );
        let clippy = "\
/tmp/out/foo.d: /tmp/ws/xtask/plugin-boundary/clippy.toml plugins/providers/fixture/src/lib.rs Cargo.toml\n\
\n\
/tmp/out/libfoo.rmeta: /tmp/ws/xtask/plugin-boundary/clippy.toml plugins/providers/fixture/src/lib.rs Cargo.toml\n\
\n\
plugins/providers/fixture/src/lib.rs:\n\
# env-dep:CLIPPY_ARGS=-D__CLIPPY_HACKERY__\n";
        let listed = super::dep_info_prerequisites(clippy);
        assert!(listed.contains(&"plugins/providers/fixture/src/lib.rs".to_owned()));
        assert!(listed.contains(&"Cargo.toml".to_owned()));
        assert!(
            listed
                .iter()
                .all(|path| !path.ends_with(".d") && !path.ends_with(".rmeta"))
        );
        assert!(super::dep_stem_matches(
            "hofund_plugin_fixture-9521719d10da3640",
            "hofund_plugin_fixture"
        ));
        assert!(super::dep_stem_matches(
            "libhofund_plugin_fixture-9521719d10da3640",
            "hofund_plugin_fixture"
        ));
        assert!(!super::dep_stem_matches(
            "hofund_plugin_fixture_extra-9521719d10da3640",
            "hofund_plugin_fixture"
        ));
    }

    #[test]
    fn qualified_includes_must_stay_inside_src() {
        let relative = super::scan_source("::core::include!(\"../../target/evil.rs\");\n");
        assert!(
            relative.iter().any(|hit| hit.contains("leaves src")),
            "{relative:?}"
        );
        let absolute = if cfg!(windows) {
            "C:/abs/evil.rs"
        } else {
            "/tmp/evil.rs"
        };
        let absolute = super::scan_source(&format!("std::include!(\"{absolute}\");\n"));
        assert!(
            absolute.iter().any(|hit| hit.contains("leaves src")),
            "{absolute:?}"
        );
        let api = super::scan_source(
            "::core::include!(\"../../../../crates/kernel/plugin-api/notes/evil.rs\");\n",
        );
        assert!(api.iter().any(|hit| hit.contains("leaves src")), "{api:?}");
        let renamed = super::scan_source("use std::include as sneak;\nsneak!(\"../out.rs\");\n");
        assert!(
            renamed.iter().any(|hit| hit.contains("leaves src")),
            "{renamed:?}"
        );
        let inside = super::scan_source("use std::include as sneak;\nsneak!(\"m.txt\");\n");
        assert!(
            inside
                .iter()
                .all(|hit| !hit.contains("leaves src") && !hit.contains("non-literal")),
            "{inside:?}"
        );
        let bytes = super::scan_source("::core::include_bytes!(\"../out.bin\");\n");
        assert!(
            bytes.iter().any(|hit| hit.contains("leaves src")),
            "{bytes:?}"
        );
        let data = super::scan_source("include_str!(\"data.txt\");\n");
        assert!(
            data.iter().all(|hit| !hit.contains("leaves src")),
            "{data:?}"
        );
        let dynamic = super::scan_source("std::include!(concat!(\"a.rs\"));\n");
        assert!(
            dynamic
                .iter()
                .any(|hit| hit.contains("non-literal include!")),
            "{dynamic:?}"
        );
    }

    #[cfg(unix)]
    #[test]
    fn symlinks_are_rejected_without_rewriting_the_path() {
        let root = scratch().join(format!("hofund-plugin-symlink-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&root);
        let real = root.join("real");
        std::fs::create_dir_all(real.join("src")).expect("real");
        std::fs::write(real.join("src/lib.rs"), "fn clean() {}\n").expect("lib");
        let link = root.join("link");
        std::os::unix::fs::symlink(&real, &link).expect("symlink");
        assert_eq!(super::canonical_root(&link), super::canonical_root(&real));
        let through = link.join("src/lib.rs");
        assert!(super::lexical_path(&through).starts_with(super::lexical_path(&link)));
        assert!(!super::lexical_path(&through).starts_with(super::lexical_path(&real)));
        // Cargo metadata uses the canonical file path. The package root may
        // still be the unresolved symlink, including an outer TMPDIR link.
        let canonical_file =
            std::fs::canonicalize(real.join("src/lib.rs")).expect("canonical file");
        assert!(super::under_dir(&canonical_file, &link.join("src")));
        assert!(super::under_dir(&through, &link.join("src")));
        assert!(!super::under_dir(&root.join("evil.txt"), &link.join("src")));
        assert!(super::under_dir(
            std::path::Path::new("src/lib.rs"),
            std::path::Path::new("src")
        ));
        assert!(!super::under_dir(&through, std::path::Path::new("src")));

        let file_pkg = root.join("file-pkg");
        std::fs::create_dir_all(file_pkg.join("src")).expect("file pkg");
        std::fs::write(
            file_pkg.join("Cargo.toml"),
            "[package]\nname = \"file-pkg\"\nversion = \"0.1.0\"\nedition = \"2021\"\n",
        )
        .expect("manifest");
        std::fs::write(
            file_pkg.join("src/lib.rs"),
            "::core::include!(\"m.txt\");\n",
        )
        .expect("lib");
        let evil = root.join("evil.txt");
        std::fs::write(&evil, "#[cfg(not(clippy))]\nfn sneak() {}\n").expect("evil");
        std::os::unix::fs::symlink(&evil, file_pkg.join("src/m.txt")).expect("file link");
        let file_hits = source_violations(&file_pkg).join("\n");
        assert!(file_hits.contains("symlink"), "{file_hits}");
        assert!(file_hits.contains("m.txt"), "{file_hits}");
        assert!(!file_hits.contains("leaves src"), "{file_hits}");

        let dir_pkg = root.join("dir-pkg");
        std::fs::create_dir_all(dir_pkg.join("src")).expect("dir pkg");
        std::fs::write(
            dir_pkg.join("Cargo.toml"),
            "[package]\nname = \"dir-pkg\"\nversion = \"0.1.0\"\nedition = \"2021\"\n",
        )
        .expect("manifest");
        std::fs::write(
            dir_pkg.join("src/lib.rs"),
            "::core::include!(\"d/m.txt\");\n",
        )
        .expect("lib");
        let evildir = root.join("evildir");
        std::fs::create_dir_all(&evildir).expect("evildir");
        std::fs::write(evildir.join("m.txt"), "fn sneak() {}\n").expect("module");
        std::os::unix::fs::symlink(&evildir, dir_pkg.join("src/d")).expect("dir link");
        let dir_hits = source_violations(&dir_pkg).join("\n");
        assert!(dir_hits.contains("symlink"), "{dir_hits}");
        assert!(
            dir_hits.contains(&dir_pkg.join("src/d").display().to_string()),
            "{dir_hits}"
        );
        assert!(!dir_hits.contains("leaves src"), "{dir_hits}");
        let _ = std::fs::remove_dir_all(&root);
    }

    #[test]
    fn target_roots_must_sit_under_src() {
        let package = std::path::Path::new("/tmp/pkg");
        let src = std::path::PathBuf::from("/tmp/pkg/src/lib.rs");
        assert!(super::under_dir(&src, &package.join("src")));
        let moved = std::path::PathBuf::from("/tmp/pkg/lib/lib.rs");
        assert!(!super::under_dir(&moved, &package.join("src")));
        let tests = std::path::PathBuf::from("/tmp/pkg/tests/t.rs");
        assert!(!super::under_dir(&tests, &package.join("src")));
        let workspace = std::path::PathBuf::from(env!("CARGO_MANIFEST_DIR"))
            .parent()
            .expect("workspace")
            .to_path_buf();
        super::target_roots_under_src(&workspace, &workspace.join("plugins/providers/fixture"))
            .expect("fixture roots stay under src");
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
