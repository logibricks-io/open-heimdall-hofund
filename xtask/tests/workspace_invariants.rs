//! Locks the workspace dependency graph and two source rules:
//! gateway and router code must not name vendors, and the magpie name is not
//! used as branding outside the acknowledgement files.

use std::collections::BTreeSet;
use std::fs;
use std::path::{Path, PathBuf};

use syn::visit::Visit;

#[path = "support/plugin_rules.rs"]
mod plugin_rules;

/// Workspace-relative paths of every Cargo package, in the same order as the
/// root `[workspace] members` list is asserted below.
const MEMBERS: &[&str] = &[
    "apps/desktop/src-tauri",
    "crates/agent",
    "crates/cli",
    "crates/codec",
    "crates/config",
    "crates/control-proto",
    "crates/daemon",
    "crates/gateway",
    "crates/ir",
    "crates/plugin-api",
    "crates/plugin-host",
    "crates/plugin-sdk",
    "plugins/inbound/echo",
    "plugins/providers/fixture",
    "plugins/providers/relay",
    "plugins/registry",
    "crates/provider",
    "kits/agent",
    "kits/subscription",
    "kits/upstream",
    "crates/quirks",
    "crates/router",
    "crates/secrets",
    "crates/usage",
    "xtask",
];

/// Direct normal and build workspace dependencies each package may declare.
/// Update `docs/architecture.md` in the same change.
fn expected_deps(package: &str) -> Option<&'static [&'static str]> {
    Some(match package {
        "hofund-agent" => &["hofund-config", "hofund-plugin-api"],
        "hofund-cli" => &["hofund-agent", "hofund-daemon"],
        "hofund-codec" => &["hofund-ir"],
        "hofund-config" => &[],
        "hofund-control-proto" => &[],
        "hofund-desktop" => &["hofund-agent", "hofund-daemon"],
        "hofund-daemon" => &[
            "hofund-codec",
            "hofund-config",
            "hofund-control-proto",
            "hofund-gateway",
            "hofund-plugin-api",
            "hofund-plugin-host",
            "hofund-plugin-registry",
            "hofund-plugin-sdk",
            "hofund-provider",
            "hofund-quirks",
            "hofund-router",
            "hofund-secrets",
            "hofund-usage",
        ],
        "hofund-gateway" => &[
            "hofund-codec",
            "hofund-ir",
            "hofund-plugin-api",
            "hofund-plugin-host",
            "hofund-router",
            "hofund-usage",
        ],
        "hofund-ir" => &[],
        "hofund-kit-agent" => &["hofund-agent", "hofund-plugin-api", "hofund-plugin-host"],
        "hofund-kit-subscription" => &["hofund-plugin-host"],
        "hofund-kit-upstream" => &["hofund-plugin-api", "hofund-plugin-host"],
        "hofund-plugin-api" => &["hofund-ir"],
        "hofund-plugin-host" => &["hofund-plugin-api"],
        "hofund-plugin-echo" => &["hofund-plugin-api", "hofund-plugin-sdk"],
        "hofund-plugin-fixture" => &["hofund-plugin-api"],
        "hofund-plugin-registry" => &[
            "hofund-plugin-api",
            "hofund-plugin-echo",
            "hofund-plugin-relay",
            "hofund-plugin-sdk",
            "hofund-provider",
            "hofund-quirks",
        ],
        "hofund-plugin-relay" => &["hofund-plugin-api", "hofund-plugin-sdk"],
        "hofund-plugin-sdk" => &["hofund-plugin-api", "hofund-quirks"],
        "hofund-provider" => &[
            "hofund-codec",
            "hofund-ir",
            "hofund-plugin-api",
            "hofund-plugin-sdk",
            "hofund-quirks",
        ],
        "hofund-quirks" => &["hofund-ir"],
        "hofund-router" => &["hofund-ir", "hofund-plugin-api"],
        "hofund-secrets" => &[],
        "hofund-usage" => &[],
        "xtask" => &[],
        _ => return None,
    })
}

/// Direct dev workspace dependencies each package may declare.
fn expected_dev_deps(package: &str) -> Option<&'static [&'static str]> {
    Some(match package {
        "hofund-agent" => &["hofund-agent"],
        "hofund-cli" => &[],
        "hofund-codec" => &[],
        "hofund-config" => &[],
        "hofund-control-proto" => &[],
        "hofund-desktop" => &[],
        "hofund-daemon" => &["hofund-agent", "hofund-daemon", "hofund-plugin-registry"],
        "hofund-gateway" => &[
            "hofund-codec",
            "hofund-ir",
            "hofund-plugin-api",
            "hofund-router",
            "hofund-usage",
        ],
        "hofund-ir" => &[],
        "hofund-kit-agent" => &[],
        "hofund-kit-subscription" => &[],
        "hofund-kit-upstream" => &[],
        "hofund-plugin-api" => &[],
        "hofund-plugin-host" => &["hofund-plugin-sdk"],
        "hofund-plugin-echo" => &[],
        "hofund-plugin-fixture" => &[],
        "hofund-plugin-registry" => &[
            "hofund-agent",
            "hofund-kit-agent",
            "hofund-kit-upstream",
            "hofund-plugin-registry",
        ],
        "hofund-plugin-relay" => &["hofund-kit-upstream"],
        "hofund-plugin-sdk" => &[],
        "hofund-provider" => &["hofund-ir", "hofund-quirks"],
        "hofund-quirks" => &[],
        "hofund-router" => &["hofund-ir", "hofund-plugin-api"],
        "hofund-secrets" => &[],
        "hofund-usage" => &[],
        "xtask" => &[],
        _ => return None,
    })
}

/// Lowercase vendor and product names that must not appear in gateway or router
/// sources. Protocol crates and provider adapters may name them.
const VENDOR_NAMES: &[&str] = &[
    "anthropic",
    "azure",
    "bedrock",
    "claude",
    "codex",
    "copilot",
    "deepseek",
    "fireworks",
    "gemini",
    "google",
    "grok",
    "groq",
    "kimi",
    "minimax",
    "mistral",
    "moonshot",
    "ollama",
    "openai",
    "openrouter",
    "qwen",
    "siliconflow",
    "together",
    "vertex",
    "xai",
    "zhipu",
];

/// Repo-relative paths that may mention the upstream project this work was
/// inspired by. Everywhere else that name is branding and is rejected.
const INSPIRATION_ALLOWLIST: &[&str] = &[
    ".cursor/rules/hofund.mdc",
    "AGENTS.md",
    "CONTRIBUTING.md",
    "README.md",
    "README.zh-CN.md",
    "THIRD_PARTY_NOTICES.md",
    "docs/architecture.md",
    "docs/coverage/agents.md",
    "docs/coverage/providers.md",
    "docs/coverage/subscriptions.md",
    "docs/feature-parity.md",
    "docs/magpie-comparison.md",
    "docs/plugins/control-plane.md",
    "docs/plugins/models.md",
    "docs/rfc/0001-microkernel-plugins.md",
    "docs/roadmap.md",
    "xtask/tests/workspace_invariants.rs",
];

#[test]
fn dependency_graph_matches_architecture() {
    let root = workspace_root();
    let manifest = read_to_string(&root.join("Cargo.toml"));
    let members = workspace_members(&manifest);
    let expected_members: BTreeSet<String> =
        MEMBERS.iter().map(|path| (*path).to_string()).collect();
    assert_eq!(
        members, expected_members,
        "workspace members drifted from the architecture test"
    );

    assert!(
        manifest.contains("edition = \"2024\""),
        "workspace edition must stay 2024"
    );
    assert!(
        manifest.contains("resolver = \"3\""),
        "workspace resolver must stay 3"
    );

    let toolchain = read_to_string(&root.join("rust-toolchain.toml"));
    assert!(
        toolchain.contains("channel = \"1.99.1\""),
        "pinned toolchain drifted"
    );
    assert!(
        manifest.contains("version = \"0.1.0\""),
        "workspace package version drifted"
    );
    for line in manifest.lines() {
        let trimmed = strip_comment(line).trim();
        if trimmed.starts_with("hofund-") {
            assert!(
                trimmed.contains("version = \"0.1.0\""),
                "workspace dependency must repeat version 0.1.0: {trimmed}"
            );
        }
    }

    assert!(
        workspace_legacy_codex_feature_is_allowed(&manifest),
        "workspace dependencies must not enable legacy-agent-consts"
    );
    let mut seen = BTreeSet::new();
    for member in MEMBERS {
        let package_manifest = read_to_string(&root.join(member).join("Cargo.toml"));
        let name = package_name(&package_manifest);
        assert!(
            inherits_workspace_edition(&package_manifest),
            "{name} must inherit edition 2024 from the workspace"
        );
        let deps = workspace_deps(&package_manifest, DependencySection::Normal, &manifest);
        let Some(expected) = expected_deps(&name) else {
            panic!("no allowed dependency list for package {name}");
        };
        let expected_set: BTreeSet<String> =
            expected.iter().map(|dep| (*dep).to_string()).collect();
        assert_eq!(
            deps, expected_set,
            "package {name} normal/build dependencies drifted; update docs/architecture.md and this test together"
        );
        let dev_deps = workspace_deps(&package_manifest, DependencySection::Dev, &manifest);
        let Some(expected_dev) = expected_dev_deps(&name) else {
            panic!("no allowed dev dependency list for package {name}");
        };
        let expected_dev_set: BTreeSet<String> =
            expected_dev.iter().map(|dep| (*dep).to_string()).collect();
        assert_eq!(
            dev_deps, expected_dev_set,
            "package {name} dev dependencies drifted; review the dev allow-map"
        );
        assert!(
            legacy_codex_feature_is_allowed(&name, &package_manifest, &manifest),
            "package {name} enables legacy-agent-consts outside dev-dependencies"
        );
        assert!(seen.insert(name), "duplicate package name");
    }

    for package in [
        "hofund-agent",
        "hofund-cli",
        "hofund-codec",
        "hofund-config",
        "hofund-control-proto",
        "hofund-daemon",
        "hofund-desktop",
        "hofund-gateway",
        "hofund-ir",
        "hofund-kit-agent",
        "hofund-kit-subscription",
        "hofund-kit-upstream",
        "hofund-plugin-api",
        "hofund-plugin-echo",
        "hofund-plugin-fixture",
        "hofund-plugin-host",
        "hofund-plugin-registry",
        "hofund-plugin-relay",
        "hofund-plugin-sdk",
        "hofund-provider",
        "hofund-quirks",
        "hofund-router",
        "hofund-secrets",
        "hofund-usage",
        "xtask",
    ] {
        assert!(seen.contains(package), "missing package {package}");
    }
}

#[test]
fn only_platform_win_may_allow_unsafe_code() {
    let root = workspace_root();
    let manifest = read_to_string(&root.join("Cargo.toml"));
    assert!(
        compact(&manifest).contains("unsafe_code=\"forbid\""),
        "the workspace must forbid unsafe code"
    );
    let mut violations = Vec::new();
    for member in MEMBERS {
        if platform_win_exempt(member) {
            continue;
        }
        let member_root = root.join(member);
        let package_manifest = read_to_string(&member_root.join("Cargo.toml"));
        if manifest_opts_out_of_safe_code(&package_manifest) {
            violations.push(format!("{member}/Cargo.toml"));
        }
        for path in rust_files(&member_root) {
            let text = read_to_string(&path);
            if source_opts_out_of_safe_code(&text) {
                violations.push(relative(&root, &path));
            }
        }
    }
    assert!(
        violations.is_empty(),
        "only crates/platform-win may relax the unsafe-code forbid rule:\n{}",
        violations.join("\n")
    );
}

#[test]
fn unsafe_code_scanner_flags_opt_outs_and_exempts_platform_win() {
    for source in [
        concat!("#![allow", "(unsafe_code)]"),
        concat!("#[allow", "(unsafe_code)]"),
        concat!("#![cfg_attr(windows, allow", "(unsafe_code))]"),
        concat!("#[expect", "(unsafe_code)]"),
        concat!("#[warn", "(unsafe_code)]"),
        concat!("#[deny", "(unsafe_code)]"),
        concat!("#![allow", "(unused, unsafe_code)]"),
        concat!("#[allow", "(clippy::pedantic,unsafe_code,dead_code)]"),
        concat!("#[expect", "(rust::unsafe_code, reason = \"ffi\")]"),
        concat!(
            "#![cfg_attr(all(windows, feature = \"x\"), allow",
            "(dead_code, unsafe_code))]"
        ),
    ] {
        assert!(source_opts_out_of_safe_code(source), "{source}");
        assert!(!unsafe_source_violation("crates/platform-win", source));
        assert!(unsafe_source_violation("crates/control-proto", source));
    }
    let manifest = "[lints.rust]\nunsafe_code = \"allow\"\n";
    assert!(manifest_opts_out_of_safe_code(manifest));
    assert!(!unsafe_manifest_violation("crates/platform-win", manifest));
    assert!(!source_opts_out_of_safe_code(
        "fn safe() { let unsafe_code = 1; }"
    ));
    for source in [
        "#![allow(unused, unsafe_code_like)]",
        "#[disallow(unsafe_code)]",
        "#[allow(unused)] fn f(unsafe_code: u8) {}",
    ] {
        assert!(!source_opts_out_of_safe_code(source), "{source}");
    }
}

fn platform_win_exempt(member: &str) -> bool {
    member == "crates/platform-win"
}

fn unsafe_source_violation(member: &str, source: &str) -> bool {
    !platform_win_exempt(member) && source_opts_out_of_safe_code(source)
}

fn unsafe_manifest_violation(member: &str, manifest: &str) -> bool {
    !platform_win_exempt(member) && manifest_opts_out_of_safe_code(manifest)
}

// The manifest check is the real guard: workspace `forbid` already rejects a
// source-level allowance. This scan is conservative belt-and-braces and may
// report false positives, such as `deny` or attribute text in comments.
fn source_opts_out_of_safe_code(source: &str) -> bool {
    let source = compact(source);
    ["allow", "expect", "warn", "deny"]
        .into_iter()
        .any(|level| {
            lint_lists(&source, level)
                .into_iter()
                .any(names_unsafe_code)
        })
}

/// Returns the contents of every `level(...)` group whose level name is not
/// part of a longer identifier, up to the matching paren.
fn lint_lists<'a>(source: &'a str, level: &str) -> Vec<&'a str> {
    let pattern = format!("{level}(");
    let mut lists = Vec::new();
    for (start, _) in source.match_indices(&pattern) {
        let before = source[..start].chars().next_back();
        if before.is_some_and(|ch| ch.is_alphanumeric() || ch == '_') {
            continue;
        }
        let body = &source[start + pattern.len()..];
        let mut depth = 0_usize;
        let mut end = body.len();
        for (index, ch) in body.char_indices() {
            match ch {
                '(' => depth += 1,
                ')' if depth == 0 => {
                    end = index;
                    break;
                }
                ')' => depth -= 1,
                _ => {}
            }
        }
        lists.push(&body[..end]);
    }
    lists
}

fn names_unsafe_code(list: &str) -> bool {
    list.split(',')
        .any(|lint| lint == "unsafe_code" || lint.ends_with("::unsafe_code"))
}

fn manifest_opts_out_of_safe_code(manifest: &str) -> bool {
    let manifest = compact(manifest);
    !manifest.contains("[lints]workspace=true")
        || manifest.contains("[lints.rust]")
        || manifest.contains("unsafe_code")
}

fn compact(text: &str) -> String {
    text.split_whitespace().collect()
}

#[test]
fn gateway_and_router_do_not_name_vendors() {
    let root = workspace_root();
    let mut violations = Vec::new();
    for crate_dir in ["crates/gateway", "crates/router"] {
        for path in rust_files(&root.join(crate_dir)) {
            let text = read_to_string(&path);
            for (index, line) in text.lines().enumerate() {
                let hits = vendor_hits(line);
                if hits.is_empty() {
                    continue;
                }
                violations.push(format!(
                    "{}:{}: {}",
                    relative(&root, &path),
                    index + 1,
                    hits.join(", ")
                ));
            }
        }
    }
    assert!(
        violations.is_empty(),
        "vendor names belong in quirks data and provider adapters, not in gateway or router:\n{}",
        violations.join("\n")
    );
}

#[test]
fn router_does_not_name_plugin_host_types() {
    let root = workspace_root();
    let mut violations = Vec::new();
    for path in rust_files(&root.join("crates/router")) {
        let text = read_to_string(&path);
        for word in ["UpstreamProvider", "HostContext"] {
            if contains_word(&text, word) {
                violations.push(format!("{}: {word}", relative(&root, &path)));
            }
        }
    }
    let reexports = ["Completion", "MemberId", "PluginError", "Request"];
    let imports = [
        "BodyStream",
        "Completion",
        "CountOutcome",
        "MemberId",
        "PluginError",
        "PluginErrorKind",
        "Request",
    ];
    for path in rust_files(&root.join("crates/router/src")) {
        let text = read_to_string(&path);
        for (is_pub, name) in plugin_api_use_names(&text) {
            let allowed = if is_pub { &reexports[..] } else { &imports[..] };
            if !allowed.contains(&name.as_str()) {
                let kind = if is_pub { "re-exports" } else { "imports" };
                violations.push(format!(
                    "{} {kind} `{name}` from hofund-plugin-api",
                    relative(&root, &path)
                ));
            }
        }
    }
    assert!(
        violations.is_empty(),
        "the router re-exports only PluginError, Request, Completion, and MemberId, and its other plugin-api imports are PluginErrorKind, CountOutcome, and BodyStream:\n{}",
        violations.join("\n")
    );
}

#[test]
fn a_crate_alias_reexport_is_outside_the_allow_list() {
    let reexports = ["Completion", "MemberId", "PluginError", "Request"];
    for sample in [
        "pub use hofund_plugin_api as api;\n",
        "pub use hofund_plugin_api::*;\n",
        "pub use hofund_plugin_api::{self as api};\n",
    ] {
        let names = plugin_api_use_names(sample);
        assert!(
            names
                .iter()
                .any(|(is_pub, name)| { *is_pub && !reexports.contains(&name.as_str()) }),
            "{sample:?} -> {names:?}"
        );
    }
}

fn plugin_api_use_names(text: &str) -> Vec<(bool, String)> {
    let Ok(file) = syn::parse_file(text) else {
        return vec![(false, "does not parse".to_owned())];
    };
    let mut found = Vec::new();
    UseScan { found: &mut found }.visit_file(&file);
    found
}

struct UseScan<'a> {
    found: &'a mut Vec<(bool, String)>,
}

impl<'ast> Visit<'ast> for UseScan<'_> {
    fn visit_item_use(&mut self, item: &'ast syn::ItemUse) {
        let is_pub = matches!(item.vis, syn::Visibility::Public(_));
        collect_plugin_api_use(&item.tree, &[], is_pub, self.found);
    }
}

fn collect_plugin_api_use(
    tree: &syn::UseTree,
    prefix: &[String],
    is_pub: bool,
    found: &mut Vec<(bool, String)>,
) {
    match tree {
        syn::UseTree::Path(path) => {
            let mut next = prefix.to_vec();
            next.push(path.ident.to_string());
            collect_plugin_api_use(&path.tree, &next, is_pub, found);
        }
        syn::UseTree::Name(name) => note_plugin_api_name(prefix, &name.ident, is_pub, found),
        syn::UseTree::Rename(name) => {
            if prefix.is_empty() && name.ident == "hofund_plugin_api" {
                found.push((is_pub, name.rename.to_string()));
            } else {
                note_plugin_api_name(prefix, &name.ident, is_pub, found);
            }
        }
        syn::UseTree::Glob(_) => {
            if prefix.first().map(String::as_str) == Some("hofund_plugin_api") {
                found.push((is_pub, "*".to_owned()));
            }
        }
        syn::UseTree::Group(group) => {
            for item in &group.items {
                collect_plugin_api_use(item, prefix, is_pub, found);
            }
        }
    }
}

fn note_plugin_api_name(
    prefix: &[String],
    name: &syn::Ident,
    is_pub: bool,
    found: &mut Vec<(bool, String)>,
) {
    if prefix.first().map(String::as_str) == Some("hofund_plugin_api") {
        found.push((is_pub, name.to_string()));
    }
}

fn contains_word(text: &str, word: &str) -> bool {
    let bytes = text.as_bytes();
    let needle = word.as_bytes();
    let mut start = 0;
    while let Some(offset) = text[start..].find(word) {
        let index = start + offset;
        let before_ok = index == 0 || !is_ident_byte(bytes[index - 1]);
        let after = index + needle.len();
        let after_ok = after == bytes.len() || !is_ident_byte(bytes[after]);
        if before_ok && after_ok {
            return true;
        }
        start = index + word.len();
    }
    false
}

#[test]
fn inspiration_name_is_not_used_as_branding() {
    let root = workspace_root();
    let allow: BTreeSet<&str> = INSPIRATION_ALLOWLIST.iter().copied().collect();
    let mut violations = Vec::new();
    for path in text_files(&root) {
        let rel = relative(&root, &path);
        if allow.contains(rel.as_str()) {
            continue;
        }
        let Ok(text) = fs::read_to_string(&path) else {
            continue;
        };
        if text.to_ascii_lowercase().contains("magpie") {
            violations.push(rel);
        }
    }
    assert!(
        violations.is_empty(),
        "the upstream project's name may appear only in the acknowledgement files:\n{}",
        violations.join("\n")
    );
}

fn workspace_root() -> PathBuf {
    PathBuf::from(env!("CARGO_MANIFEST_DIR"))
        .parent()
        .expect("xtask lives one directory below the workspace root")
        .to_path_buf()
}

fn read_to_string(path: &Path) -> String {
    fs::read_to_string(path).unwrap_or_else(|error| panic!("read {}: {error}", path.display()))
}

fn workspace_members(manifest: &str) -> BTreeSet<String> {
    let mut members = BTreeSet::new();
    let mut in_members = false;
    for line in manifest.lines() {
        let trimmed = strip_comment(line).trim();
        if !in_members {
            if trimmed.starts_with("members") && trimmed.contains('[') {
                in_members = true;
            }
            continue;
        }
        push_quoted(trimmed, &mut members);
        if trimmed.contains(']') {
            break;
        }
    }
    members
}

fn package_name(manifest: &str) -> String {
    for line in manifest.lines() {
        let trimmed = strip_comment(line).trim();
        if let Some(rest) = trimmed.strip_prefix("name = ") {
            return rest.trim().trim_matches('"').to_string();
        }
    }
    panic!("manifest has no package name");
}

#[derive(Clone, Copy, PartialEq, Eq)]
enum DependencySection {
    Normal,
    Dev,
}

fn workspace_deps(
    manifest: &str,
    wanted: DependencySection,
    workspace_manifest: &str,
) -> BTreeSet<String> {
    let manifest = parse_manifest(manifest);
    let workspace_manifest = parse_manifest(workspace_manifest);
    let mut deps = BTreeSet::new();
    visit_dependencies(&manifest, wanted, |key, dependency| {
        let package = resolved_dependency_package_name(key, dependency, &workspace_manifest);
        let is_local = dependency
            .get("workspace")
            .and_then(toml::Value::as_bool)
            .unwrap_or(false)
            || dependency.get("path").is_some();
        if is_workspace_crate(package) && is_local {
            deps.insert(package.to_string());
        }
    });
    deps
}

fn parse_manifest(manifest: &str) -> toml::Value {
    toml::from_str(manifest).expect("manifest must be valid TOML")
}

fn visit_dependencies<F>(manifest: &toml::Value, wanted: DependencySection, mut visit: F)
where
    F: FnMut(&str, &toml::Value),
{
    let sections: &[&str] = match wanted {
        DependencySection::Normal => &["dependencies", "build-dependencies"],
        DependencySection::Dev => &["dev-dependencies"],
    };
    for section in sections {
        visit_dependency_section(manifest, section, &mut visit);
    }
    if let Some(targets) = manifest.get("target").and_then(toml::Value::as_table) {
        for target in targets.values() {
            for section in sections {
                visit_dependency_section(target, section, &mut visit);
            }
        }
    }
}

fn visit_dependency_section<F>(manifest: &toml::Value, section: &str, visit: &mut F)
where
    F: FnMut(&str, &toml::Value),
{
    if let Some(dependencies) = manifest.get(section).and_then(toml::Value::as_table) {
        for (key, dependency) in dependencies {
            visit(key, dependency);
        }
    }
}

fn dependency_package_name<'a>(key: &'a str, dependency: &'a toml::Value) -> &'a str {
    dependency
        .get("package")
        .and_then(toml::Value::as_str)
        .unwrap_or(key)
}

fn resolved_dependency_package_name<'a>(
    key: &'a str,
    dependency: &'a toml::Value,
    workspace_manifest: &'a toml::Value,
) -> &'a str {
    if dependency
        .get("workspace")
        .and_then(toml::Value::as_bool)
        .unwrap_or(false)
        && let Some(inherited) = workspace_dependency(workspace_manifest, key)
    {
        return dependency_package_name(key, inherited);
    }
    dependency_package_name(key, dependency)
}

fn is_workspace_crate(name: &str) -> bool {
    name.starts_with("hofund-") || name == "xtask"
}

fn legacy_codex_feature_is_allowed(
    package: &str,
    manifest: &str,
    workspace_manifest: &str,
) -> bool {
    if package == "hofund-agent" {
        return true;
    }
    let manifest = parse_manifest(manifest);
    let workspace_manifest = parse_manifest(workspace_manifest);
    let mut agent_aliases = BTreeSet::new();
    let mut allowed = true;
    visit_dependencies(&manifest, DependencySection::Normal, |key, dependency| {
        if resolved_dependency_package_name(key, dependency, &workspace_manifest) != "hofund-agent"
        {
            return;
        }
        agent_aliases.insert(key.to_string());
        if dependency_enables_legacy_codex(dependency) {
            allowed = false;
        }
        if dependency
            .get("workspace")
            .and_then(toml::Value::as_bool)
            .unwrap_or(false)
            && workspace_dependency(&workspace_manifest, key)
                .is_some_and(dependency_enables_legacy_codex)
        {
            allowed = false;
        }
    });
    if !allowed {
        return false;
    }

    let Some(features) = manifest.get("features").and_then(toml::Value::as_table) else {
        return true;
    };
    for declarations in features.values().filter_map(toml::Value::as_array) {
        for declaration in declarations.iter().filter_map(toml::Value::as_str) {
            let Some((dependency, feature)) = declaration.split_once('/') else {
                continue;
            };
            let dependency = dependency.strip_suffix('?').unwrap_or(dependency);
            if feature == "legacy-agent-consts" && agent_aliases.contains(dependency) {
                return false;
            }
        }
    }
    true
}

fn dependency_enables_legacy_codex(dependency: &toml::Value) -> bool {
    dependency
        .get("features")
        .and_then(toml::Value::as_array)
        .is_some_and(|features| {
            features
                .iter()
                .any(|feature| feature.as_str() == Some("legacy-agent-consts"))
        })
}

fn workspace_dependency<'a>(manifest: &'a toml::Value, key: &str) -> Option<&'a toml::Value> {
    manifest.get("workspace")?.get("dependencies")?.get(key)
}

fn workspace_legacy_codex_feature_is_allowed(manifest: &str) -> bool {
    let manifest = parse_manifest(manifest);
    let Some(dependencies) = manifest
        .get("workspace")
        .and_then(|workspace| workspace.get("dependencies"))
        .and_then(toml::Value::as_table)
    else {
        return true;
    };
    dependencies.iter().all(|(key, dependency)| {
        dependency_package_name(key, dependency) != "hofund-agent"
            || !dependency_enables_legacy_codex(dependency)
    })
}

#[test]
fn legacy_codex_feature_is_rejected_in_normal_dependencies() {
    let manifest = r#"
[dependencies]
hofund-agent = { workspace = true, features = ["legacy-agent-consts"] }
"#;
    assert!(!legacy_codex_feature_is_allowed(
        "hofund-plugin-registry",
        manifest,
        ""
    ));
}

#[test]
fn legacy_codex_feature_is_allowed_in_dev_dependencies() {
    let manifest = r#"
[dev-dependencies]
hofund-agent = { workspace = true, features = ["legacy-agent-consts"] }
"#;
    assert!(legacy_codex_feature_is_allowed(
        "hofund-plugin-registry",
        manifest,
        ""
    ));
}

#[test]
fn dependency_table_form_is_detected() {
    let manifest = r#"
[dependencies.hofund-agent]
workspace = true
features = ["legacy-agent-consts"]
"#;
    assert!(workspace_deps(manifest, DependencySection::Normal, "").contains("hofund-agent"));
    assert!(!legacy_codex_feature_is_allowed(
        "hofund-plugin-registry",
        manifest,
        ""
    ));
}

#[test]
fn multiline_inline_features_are_detected() {
    let manifest = r#"
[dependencies]
hofund-agent = { workspace = true, features = [
    "legacy-agent-consts",
] }
"#;
    assert!(!legacy_codex_feature_is_allowed(
        "hofund-plugin-registry",
        manifest,
        ""
    ));
}

#[test]
fn multiline_table_features_are_detected() {
    let manifest = r#"
[dependencies.hofund-agent]
workspace = true
features = [
    "legacy-agent-consts",
]
"#;
    assert!(!legacy_codex_feature_is_allowed(
        "hofund-plugin-registry",
        manifest,
        ""
    ));
}

#[test]
fn target_dependency_table_is_detected() {
    let manifest = r#"
[target.'cfg(unix)'.dependencies.hofund-agent]
workspace = true
features = ["legacy-agent-consts"]
"#;
    assert!(workspace_deps(manifest, DependencySection::Normal, "").contains("hofund-agent"));
    assert!(!legacy_codex_feature_is_allowed(
        "hofund-plugin-registry",
        manifest,
        ""
    ));
}

#[test]
fn renamed_dependency_is_detected() {
    let manifest = r#"
[dependencies]
agent = { package = "hofund-agent", workspace = true, features = ["legacy-agent-consts"] }
"#;
    assert!(workspace_deps(manifest, DependencySection::Normal, "").contains("hofund-agent"));
    assert!(!legacy_codex_feature_is_allowed(
        "hofund-plugin-registry",
        manifest,
        ""
    ));
}

#[test]
fn workspace_renamed_dependency_is_detected() {
    let workspace_manifest = r#"
[workspace.dependencies]
agent = { package = "hofund-agent", path = "crates/agent" }
"#;
    let manifest = r#"
[dependencies]
agent = { workspace = true }
"#;
    assert!(
        workspace_deps(manifest, DependencySection::Normal, workspace_manifest)
            .contains("hofund-agent")
    );
    assert!(legacy_codex_feature_is_allowed(
        "hofund-plugin-registry",
        manifest,
        workspace_manifest
    ));
}

#[test]
fn workspace_renamed_feature_forwarding_is_detected() {
    let workspace_manifest = r#"
[workspace.dependencies]
agent = { package = "hofund-agent", path = "crates/agent" }
"#;
    let manifest = r#"
[dependencies]
agent = { workspace = true }
[features]
default = ["agent/legacy-agent-consts"]
"#;
    assert!(!legacy_codex_feature_is_allowed(
        "hofund-plugin-registry",
        manifest,
        workspace_manifest
    ));
}

#[test]
fn workspace_renamed_member_features_are_detected() {
    let workspace_manifest = r#"
[workspace.dependencies]
agent = { package = "hofund-agent", path = "crates/agent" }
"#;
    let manifest = r#"
[dependencies]
agent = { workspace = true, features = ["legacy-agent-consts"] }
"#;
    assert!(!legacy_codex_feature_is_allowed(
        "hofund-plugin-registry",
        manifest,
        workspace_manifest
    ));
}

#[test]
fn dotted_dependency_keys_are_detected() {
    let manifest = r#"
[dependencies]
hofund-agent.workspace = true
hofund-agent.features = ["legacy-agent-consts"]
"#;
    assert!(workspace_deps(manifest, DependencySection::Normal, "").contains("hofund-agent"));
    assert!(!legacy_codex_feature_is_allowed(
        "hofund-plugin-registry",
        manifest,
        ""
    ));
}

#[test]
fn feature_forwarding_is_detected() {
    let manifest = r#"
[dependencies]
hofund-agent = { workspace = true }
[features]
default = ["hofund-agent/legacy-agent-consts"]
"#;
    assert!(!legacy_codex_feature_is_allowed(
        "hofund-plugin-registry",
        manifest,
        ""
    ));
}

#[test]
fn optional_feature_forwarding_is_detected() {
    let manifest = r#"
[dependencies]
hofund-agent = { workspace = true, optional = true }
[features]
default = ["hofund-agent?/legacy-agent-consts"]
"#;
    assert!(!legacy_codex_feature_is_allowed(
        "hofund-plugin-registry",
        manifest,
        ""
    ));
}

#[test]
fn renamed_feature_forwarding_is_detected() {
    let manifest = r#"
[dependencies]
agent = { package = "hofund-agent", workspace = true }
[features]
default = ["agent/legacy-agent-consts"]
"#;
    assert!(!legacy_codex_feature_is_allowed(
        "hofund-plugin-registry",
        manifest,
        ""
    ));
}

#[test]
fn inherited_workspace_features_are_detected() {
    let workspace_manifest = r#"
[workspace.dependencies]
hofund-agent = { path = "crates/agent", features = ["legacy-agent-consts"] }
"#;
    let manifest = r#"
[dependencies]
hofund-agent.workspace = true
"#;
    assert!(!workspace_legacy_codex_feature_is_allowed(
        workspace_manifest
    ));
    assert!(!legacy_codex_feature_is_allowed(
        "hofund-plugin-registry",
        manifest,
        workspace_manifest
    ));
}

#[test]
fn dependency_sections_stay_separate() {
    let manifest = r#"
[build-dependencies]
hofund-ir = { workspace = true }
[target.'cfg(unix)'.dependencies]
hofund-codec = { workspace = true }
[target.'cfg(unix)'.build-dependencies]
hofund-config = { workspace = true }
[target.'cfg(unix)'.dev-dependencies]
hofund-router = { workspace = true }
"#;
    let normal = workspace_deps(manifest, DependencySection::Normal, "");
    assert_eq!(
        normal,
        ["hofund-codec", "hofund-config", "hofund-ir"]
            .into_iter()
            .map(str::to_string)
            .collect()
    );
    assert_eq!(
        workspace_deps(manifest, DependencySection::Dev, ""),
        ["hofund-router".to_string()].into_iter().collect()
    );
}

fn inherits_workspace_edition(manifest: &str) -> bool {
    manifest.lines().any(|line| {
        let trimmed = strip_comment(line).trim();
        trimmed == "edition.workspace = true"
    })
}

fn strip_comment(line: &str) -> &str {
    match line.split_once('#') {
        Some((code, _)) => code,
        None => line,
    }
}

fn push_quoted(line: &str, out: &mut BTreeSet<String>) {
    for quoted in line.split('"').skip(1).step_by(2) {
        out.insert(quoted.to_string());
    }
}

fn vendor_hits(line: &str) -> Vec<&'static str> {
    let lower = line.to_ascii_lowercase();
    let bytes = lower.as_bytes();
    let mut hits = Vec::new();
    for word in VENDOR_NAMES {
        let needle = word.as_bytes();
        let mut start = 0;
        while let Some(offset) = lower[start..].find(word) {
            let index = start + offset;
            let before_ok = index == 0 || !is_ident_byte(bytes[index - 1]);
            let after = index + needle.len();
            let after_ok = after == bytes.len() || !is_ident_byte(bytes[after]);
            if before_ok && after_ok {
                hits.push(*word);
                break;
            }
            start = index + needle.len();
        }
    }
    hits
}

fn is_ident_byte(byte: u8) -> bool {
    byte.is_ascii_alphanumeric() || byte == b'_'
}

fn rust_files(dir: &Path) -> Vec<PathBuf> {
    let mut files = Vec::new();
    collect_files(dir, &mut files, true);
    files.sort();
    files
}

fn text_files(dir: &Path) -> Vec<PathBuf> {
    let mut files = Vec::new();
    collect_files(dir, &mut files, false);
    files.sort();
    files
}

fn collect_files(dir: &Path, files: &mut Vec<PathBuf>, rust_only: bool) {
    let entries =
        fs::read_dir(dir).unwrap_or_else(|error| panic!("read dir {}: {error}", dir.display()));
    for entry in entries {
        let entry = entry.unwrap_or_else(|error| panic!("dir entry: {error}"));
        let path = entry.path();
        let name = entry.file_name();
        let name = name.to_string_lossy();
        if name == ".git" || name == "target" {
            continue;
        }
        if path.is_dir() {
            collect_files(&path, files, rust_only);
            continue;
        }
        if rust_only {
            if path.extension().is_some_and(|ext| ext == "rs") {
                files.push(path);
            }
        } else {
            files.push(path);
        }
    }
}

fn relative(root: &Path, path: &Path) -> String {
    path.strip_prefix(root)
        .unwrap_or_else(|_| panic!("{} is outside {}", path.display(), root.display()))
        .components()
        .map(|component| component.as_os_str().to_string_lossy().into_owned())
        .collect::<Vec<_>>()
        .join("/")
}
