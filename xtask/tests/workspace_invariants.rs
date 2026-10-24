//! Locks the workspace dependency graph and two source rules:
//! gateway and router code must not name vendors, and the magpie name is not
//! used as branding outside the acknowledgement files.

use std::collections::BTreeSet;
use std::fs;
use std::path::{Path, PathBuf};

/// Workspace-relative paths of every Cargo package, in the same order as the
/// root `[workspace] members` list is asserted below.
const MEMBERS: &[&str] = &[
    "crates/agent",
    "crates/cli",
    "crates/codec",
    "crates/config",
    "crates/daemon",
    "crates/gateway",
    "crates/ir",
    "crates/provider",
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
        "hofund-agent" => &["hofund-config"],
        "hofund-cli" => &["hofund-agent", "hofund-daemon"],
        "hofund-codec" => &["hofund-ir"],
        "hofund-config" => &[],
        "hofund-daemon" => &["hofund-config", "hofund-gateway", "hofund-secrets"],
        "hofund-gateway" => &["hofund-router"],
        "hofund-ir" => &[],
        "hofund-provider" => &["hofund-codec", "hofund-quirks"],
        "hofund-quirks" => &[],
        "hofund-router" => &["hofund-provider"],
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
        manifest.contains("version = \"0.0.1\""),
        "workspace package version drifted"
    );
    for line in manifest.lines() {
        let trimmed = strip_comment(line).trim();
        if trimmed.starts_with("hofund-") {
            assert!(
                trimmed.contains("version = \"0.0.1\""),
                "workspace dependency must repeat version 0.0.1: {trimmed}"
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
        "hofund-gateway",
        "hofund-ir",
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
