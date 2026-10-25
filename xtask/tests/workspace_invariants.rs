//! Locks the workspace dependency graph and two source rules:
//! gateway and router code must not name vendors, and the magpie name is not
//! used as branding outside the acknowledgement files.

use std::collections::BTreeSet;
use std::fs;
use std::path::{Path, PathBuf};

/// Workspace-relative paths of every Cargo package, in the same order as the
/// root `[workspace] members` list is asserted below.
const MEMBERS: &[&str] = &[
    "apps/desktop/src-tauri",
    "crates/agent",
    "crates/cli",
    "crates/codec",
    "crates/config",
    "crates/daemon",
    "crates/gateway",
    "crates/ir",
    "crates/plugin-api",
    "crates/plugin-host",
    "crates/plugin-sdk",
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

/// Direct workspace dependencies each package is allowed to declare.
/// Update `docs/architecture.md` in the same change.
fn expected_deps(package: &str) -> Option<&'static [&'static str]> {
    Some(match package {
        "hofund-agent" => &["hofund-config", "hofund-plugin-api"],
        "hofund-cli" => &["hofund-agent", "hofund-daemon"],
        "hofund-codec" => &["hofund-ir"],
        "hofund-config" => &[],
        "hofund-desktop" => &["hofund-agent", "hofund-daemon"],
        "hofund-daemon" => &[
            "hofund-codec",
            "hofund-config",
            "hofund-gateway",
            "hofund-provider",
            "hofund-quirks",
            "hofund-router",
            "hofund-secrets",
            "hofund-usage",
        ],
        "hofund-gateway" => &["hofund-codec", "hofund-ir", "hofund-router", "hofund-usage"],
        "hofund-ir" => &[],
        "hofund-kit-agent" => &["hofund-plugin-host"],
        "hofund-kit-subscription" => &["hofund-plugin-host"],
        "hofund-kit-upstream" => &["hofund-plugin-host"],
        "hofund-plugin-api" => &["hofund-ir"],
        "hofund-plugin-host" => &["hofund-plugin-api"],
        "hofund-plugin-sdk" => &["hofund-plugin-api", "hofund-quirks"],
        "hofund-provider" => &[
            "hofund-codec",
            "hofund-ir",
            "hofund-plugin-api",
            "hofund-quirks",
        ],
        "hofund-quirks" => &["hofund-ir"],
        "hofund-router" => &["hofund-ir", "hofund-provider"],
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

    let mut seen = BTreeSet::new();
    for member in MEMBERS {
        let package_manifest = read_to_string(&root.join(member).join("Cargo.toml"));
        let name = package_name(&package_manifest);
        assert!(
            inherits_workspace_edition(&package_manifest),
            "{name} must inherit edition 2024 from the workspace"
        );
        let deps = workspace_deps(&package_manifest);
        let Some(expected) = expected_deps(&name) else {
            panic!("no allowed dependency list for package {name}");
        };
        let expected_set: BTreeSet<String> =
            expected.iter().map(|dep| (*dep).to_string()).collect();
        assert_eq!(
            deps, expected_set,
            "package {name} dependencies drifted; update docs/architecture.md and this test together"
        );
        assert!(seen.insert(name), "duplicate package name");
    }

    for package in [
        "hofund-agent",
        "hofund-cli",
        "hofund-codec",
        "hofund-config",
        "hofund-daemon",
        "hofund-desktop",
        "hofund-gateway",
        "hofund-ir",
        "hofund-kit-agent",
        "hofund-kit-subscription",
        "hofund-kit-upstream",
        "hofund-plugin-api",
        "hofund-plugin-host",
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
fn plugin_seam_matches_the_contract() {
    let root = workspace_root();
    let api = read_to_string(&root.join("crates/plugin-api/src/lib.rs"));
    let sdk = read_to_string(&root.join("crates/plugin-sdk/src/lib.rs"));
    for (name, source) in [("hofund-plugin-api", &api), ("hofund-plugin-sdk", &sdk)] {
        assert!(
            source.contains("#! [deny(missing_docs") || source.contains("#![deny(missing_docs"),
            "{name} must deny missing_docs"
        );
        assert!(
            source.contains("unreachable_pub"),
            "{name} must deny unreachable_pub"
        );
    }
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
    for member in MEMBERS {
        let manifest = read_to_string(&root.join(member).join("Cargo.toml"));
        let name = package_name(&manifest);
        if workspace_deps(&manifest).contains("hofund-plugin-host") {
            assert!(
                host_users.contains(&name.as_str()),
                "{name} must not depend on hofund-plugin-host"
            );
        }
        if name.starts_with("hofund-kit-") {
            assert!(
                !manifest.contains("publish = true"),
                "{name} is test-only and must not set publish = true"
            );
            let source = read_to_string(&root.join(member).join("src/lib.rs"));
            assert!(
                source.contains("pub use hofund_plugin_host::test_host"),
                "{name} must expose test_host"
            );
        }
    }

    let mut violations = Vec::new();
    for path in rust_files(&root) {
        let rel = relative(&root, &path);
        if rel.starts_with("target/") {
            continue;
        }
        let text = read_to_string(&path);
        let in_host_stack = rel.starts_with("crates/plugin-api/")
            || rel.starts_with("crates/plugin-host/")
            || rel.starts_with("kits/")
            || rel.starts_with("xtask/");
        if !in_host_stack && text.contains("from_backends(") {
            violations.push(format!("{rel} calls from_backends"));
        }
        if !in_host_stack && text.contains("hofund_plugin_api::kernel") {
            violations.push(format!("{rel} names the kernel backend module"));
        }
    }
    for path in rust_files(&root.join("crates/router")) {
        let text = read_to_string(&path);
        for word in ["UpstreamProvider", "HostContext", "Subscription"] {
            if contains_word(&text, word) {
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
    let root = std::env::temp_dir().join(format!("hofund-plugin-rules-{}", std::process::id()));
    let _ = fs::remove_dir_all(&root);
    let plugin = root.join("plugins/providers/openai-compat");
    fs::create_dir_all(plugin.join("src")).expect("dir");
    fs::write(
        plugin.join("Cargo.toml"),
        r#"
[package]
name = "hofund-plugin-openai-compat"
[dependencies]
hofund-plugin-api = { workspace = true }
hofund-plugin-host = { workspace = true }
hofund-ir = { workspace = true }
reqwest = "0.12"
[dev-dependencies]
hofund-kit-agent = { workspace = true }
[lints]
workspace = false
"#,
    )
    .expect("manifest");
    fs::write(
        plugin.join("src/lib.rs"),
        "use std::process::Command;\nuse reqwest::Client;\n",
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
        "hofund-kit-agent",
        "std::process",
        "lints",
        "reserved",
    ] {
        assert!(text.contains(needle), "missing {needle} in:\n{text}");
    }
    let _ = fs::remove_dir_all(&root);
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
            if !lints_follow_workspace(&manifest) {
                violations.push(format!("{kind}/{id} must set [lints] workspace = true"));
            }
            for dep in manifest_deps(&manifest, false) {
                if dep == "hofund-plugin-host"
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
                {
                    violations.push(format!("{kind}/{id} depends on {dep}"));
                }
                if ["reqwest", "hyper", "axum", "tracing"].contains(&dep.as_str()) {
                    violations.push(format!("{kind}/{id} depends on {dep}"));
                }
            }
            let own_kit = match kind.as_ref() {
                "providers" => "hofund-kit-upstream",
                "subscriptions" => "hofund-kit-subscription",
                "agents" => "hofund-kit-agent",
                _ => "",
            };
            for dep in manifest_deps(&manifest, true) {
                if dep.starts_with("hofund-kit-") && dep != own_kit {
                    violations.push(format!("{kind}/{id} dev-depends on {dep}"));
                }
            }
            if path.join("src").is_dir() {
                for source in rust_files(&path.join("src")) {
                    let text = read_to_string(&source);
                    for needle in [
                        "std::process",
                        "tokio::process",
                        "tokio::net",
                        "reqwest",
                        "hyper",
                        "axum",
                    ] {
                        if text.contains(needle) {
                            violations.push(format!(
                                "{kind}/{id} uses {needle} in {}",
                                source.file_name().unwrap_or_default().to_string_lossy()
                            ));
                        }
                    }
                }
            }
        }
    }
    violations
}

fn lints_follow_workspace(manifest: &str) -> bool {
    let mut in_lints = false;
    for line in manifest.lines() {
        let trimmed = strip_comment(line).trim();
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

fn manifest_deps(manifest: &str, dev: bool) -> Vec<String> {
    let mut deps = Vec::new();
    let mut section = "";
    for line in manifest.lines() {
        let trimmed = strip_comment(line).trim();
        if let Some(name) = trimmed.strip_prefix('[') {
            section = name.trim_end_matches(']');
            continue;
        }
        let wanted = if dev {
            section == "dev-dependencies"
        } else {
            section == "dependencies"
        };
        if !wanted {
            continue;
        }
        if let Some((key, _)) = trimmed.split_once('=') {
            let key = key.trim();
            if !key.is_empty() && !key.starts_with('[') {
                deps.push(key.to_string());
            }
        }
    }
    deps
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

fn workspace_deps(manifest: &str) -> BTreeSet<String> {
    let mut deps = BTreeSet::new();
    for line in manifest.lines() {
        let trimmed = strip_comment(line).trim();
        if trimmed.is_empty() || trimmed.starts_with('[') {
            continue;
        }
        let Some((key, value)) = trimmed.split_once('=') else {
            continue;
        };
        let key = key.trim();
        if (key.starts_with("hofund-") || key == "xtask") && value.contains("workspace") {
            deps.insert(key.to_string());
        }
    }
    deps
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
