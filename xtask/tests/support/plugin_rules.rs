//! Syn and cargo-metadata checks for the plugin seam.
//!
//! The scan follows crate aliases, globs, type aliases, angle-bracket calls,
//! and paths inside macros, including a `use` inside a function and a
//! statement-form macro. Plugin manifests are parsed as TOML. A renamed
//! `hofund-plugin-api` is rejected, and a feature forward enables `kernel`
//! under the dependency's package name, including `api/kernel` and `?/kernel`.
//! Macro fragments that splice `std::$m` are denied by Clippy after expansion,
//! not by matching `$` in the source. Runtime paths, lint suppression, and
//! `cfg(clippy)` go through `xtask::boundary`, the same scan `check-plugins` runs.

use std::collections::{BTreeMap, BTreeSet};
use std::fs;
use std::path::{Path, PathBuf};
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
        api.contains("#[cfg(not(feature = \"kernel\"))]\nmod kernel;"),
        "hofund_plugin_api::kernel stays private, so a macro splice cannot name it"
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
        for hit in seam_hits_file(&path, &mut BTreeSet::new()) {
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

#[test]
fn plugin_rules_reject_each_bypass() {
    let local_kernel = seam_hits(
        "fn sneak() { use hofund_plugin_api::kernel::HttpBackend; let _x: HttpBackend; }\n",
    );
    assert!(
        local_kernel.iter().any(|hit| hit.contains("kernel")),
        "function-local kernel import: {local_kernel:?}"
    );
    let self_as = seam_hits(
        "use hofund_plugin_api::{self as api};\nfn sneak(value: api::kernel::HttpBackend) {}\n",
    );
    assert!(
        self_as.iter().any(|hit| hit.contains("kernel")),
        "{{self as api}}: {self_as:?}"
    );
    let alias_of_alias = seam_hits(
        "use hofund_plugin_api as a;\nuse a as b;\nfn sneak(value: b::kernel::HttpBackend) {}\n",
    );
    assert!(
        alias_of_alias.iter().any(|hit| hit.contains("kernel")),
        "alias of an alias: {alias_of_alias:?}"
    );
    let crate_path = seam_hits(
        "use hofund_plugin_api as api;\nfn sneak(value: crate::api::kernel::HttpBackend) {}\n",
    );
    assert!(
        crate_path.iter().any(|hit| hit.contains("kernel")),
        "crate:: re-export: {crate_path:?}"
    );
    let raw = seam_hits("fn sneak(value: hofund_plugin_api::r#kernel::HttpBackend) {}\n");
    assert!(
        raw.iter().any(|hit| hit.contains("kernel")),
        "raw identifier: {raw:?}"
    );
    let statement_macro =
        seam_hits("fn sneak() { m!{ hofund_plugin_api::AuthResult::new(true); } }\n");
    assert!(
        statement_macro
            .iter()
            .any(|hit| hit.contains("AuthResult::new")),
        "statement-form macro: {statement_macro:?}"
    );
    let let_macro =
        seam_hits("fn sneak() { m!(let _x: hofund_plugin_api::kernel::HttpBackend); }\n");
    assert!(
        let_macro.iter().any(|hit| hit.contains("kernel")),
        "let inside a macro: {let_macro:?}"
    );
    let attribute = seam_hits("#[probe(hofund_plugin_api::kernel::HttpBackend)]\nfn sneak() {}\n");
    assert!(
        attribute.iter().any(|hit| hit.contains("kernel")),
        "attribute argument: {attribute:?}"
    );

    let root = std::env::temp_dir().join(format!("hofund-plugin-include-{}", std::process::id()));
    let _ = fs::remove_dir_all(&root);
    fs::create_dir_all(&root).expect("dir");
    fs::write(
        root.join("lib.rs"),
        "include!(\"hidden.inc\");\nfn clean() {}\n",
    )
    .expect("source");
    fs::write(
        root.join("hidden.inc"),
        "fn sneak() { let _ = hofund_plugin_api::AuthResult::new(true); }\n",
    )
    .expect("include");
    let included = seam_hits_file(&root.join("lib.rs"), &mut BTreeSet::new());
    assert!(
        included.iter().any(|hit| hit.contains("AuthResult::new")),
        "include! of a non-rs file: {included:?}"
    );
    let _ = fs::remove_dir_all(&root);

    let local_process =
        runtime_hits("fn sneak() { use std::process::Command; let _ = Command::new(\"true\"); }\n");
    assert!(
        local_process.iter().any(|hit| hit.contains("std::process")),
        "function-local use: {local_process:?}"
    );
    let self_std = runtime_hits(
        "use std::{self as s};\nfn sneak() { let _ = s::process::Command::new(\"true\"); }\n",
    );
    assert!(
        self_std.iter().any(|hit| hit.contains("std::process")),
        "{{self as s}}: {self_std:?}"
    );
    let glob_std = runtime_hits(
        "use std::*;\nfn sneak() { let _ = process::Command::new(\"true\"); let _ = net::TcpStream::connect(\"127.0.0.1:1\"); }\n",
    );
    assert!(
        glob_std.iter().any(|hit| hit.contains("std::process")),
        "std::* process: {glob_std:?}"
    );
    assert!(
        glob_std.iter().any(|hit| hit.contains("std::net")),
        "std::* net: {glob_std:?}"
    );
    let self_tokio = runtime_hits(
        "use tokio::{self as t};\nfn sneak() { let _ = t::net::TcpListener::bind(\"127.0.0.1:1\"); }\n",
    );
    assert!(
        self_tokio.iter().any(|hit| hit.contains("tokio::net")),
        "{{self as t}}: {self_tokio:?}"
    );
    assert!(
        runtime_hits("fn sneak() { let _ = std::net::TcpStream::connect(\"127.0.0.1:1\"); }\n")
            .iter()
            .any(|hit| hit.contains("std::net"))
    );
    assert!(
        runtime_hits("fn sneak() { let _ = libc::fork(); }\n")
            .iter()
            .any(|hit| hit.contains("libc"))
    );
    assert!(
        runtime_hits("fn sneak() { let _ = nix::unistd::fork(); }\n")
            .iter()
            .any(|hit| hit.contains("nix"))
    );
    assert!(
        runtime_hits("fn sneak() { let _ = std::os::unix::process::CommandExt::pre_exec; }\n")
            .iter()
            .any(|hit| hit.contains("std::os::unix::process"))
    );

    let renamed_forward = r#"
[dependencies]
api = { package = "hofund-plugin-api" }
[features]
default = ["fast"]
fast = ["api/kernel"]
"#;
    assert!(
        manifest_enables_kernel(renamed_forward),
        "api/kernel must count as the kernel feature"
    );
    let weak = r#"
[dependencies]
hofund-plugin-api = { optional = true }
[features]
default = ["fast", "dep:hofund-plugin-api"]
fast = ["hofund-plugin-api?/kernel"]
"#;
    assert!(
        manifest_enables_kernel(weak),
        "hofund-plugin-api?/kernel must count"
    );
    let weak_renamed = r#"
[dependencies]
api = { package = "hofund-plugin-api", optional = true }
[features]
fast = ["api?/kernel"]
"#;
    assert!(
        manifest_enables_kernel(weak_renamed),
        "api?/kernel must count"
    );
    let dep_prefix = r#"
[dependencies]
api = { package = "hofund-plugin-api" }
[features]
fast = ["dep:api/kernel"]
"#;
    assert!(
        manifest_enables_kernel(dep_prefix),
        "dep:api/kernel must count"
    );

    let tree = std::env::temp_dir().join(format!("hofund-plugin-bypass-{}", std::process::id()));
    let _ = fs::remove_dir_all(&tree);
    let renamed = tree.join("plugins/providers/renamed");
    fs::create_dir_all(renamed.join("src")).expect("dir");
    fs::write(
        renamed.join("Cargo.toml"),
        "[package]\nname = \"renamed\"\n[dependencies]\napi = { package = \"hofund-plugin-api\" }\ntk = { package = \"tokio\" }\nlibc = \"0.2\"\n[lints]\nworkspace = true\n",
    )
    .expect("manifest");
    fs::write(renamed.join("src/lib.rs"), "fn clean() {}\n").expect("source");
    let unknown = tree.join("plugins/unknownkind/x");
    fs::create_dir_all(&unknown).expect("unknown");
    fs::write(
        unknown.join("Cargo.toml"),
        "[package]\nname = \"x\"\n[lints]\nworkspace = true\n",
    )
    .expect("unknown manifest");
    let broken = tree.join("plugins/providers/broken");
    fs::create_dir_all(&broken).expect("broken");
    fs::write(broken.join("Cargo.toml"), "this is not toml [\n").expect("broken");
    let found = plugin_tree_violations(&tree.join("plugins")).join("\n");
    for needle in [
        "renames hofund-plugin-api",
        "renames tokio",
        "depends on libc",
        "unknown plugin kind unknownkind",
        "not valid toml",
    ] {
        assert!(found.contains(needle), "missing {needle} in:\n{found}");
    }
    let _ = fs::remove_dir_all(&tree);

    let module_path = seam_hits(
        "mod a { pub use hofund_plugin_api as api; }\nfn sneak(value: self::a::api::kernel::HttpBackend) {}\n",
    );
    assert!(
        module_path.iter().any(|hit| hit.contains("kernel")),
        "module path alias: {module_path:?}"
    );
    assert!(
        runtime_hits("fn sneak() { let _ = std::os::unix::net::UnixStream::connect(\"x\"); }\n")
            .iter()
            .any(|hit| hit.contains("std::os::unix::net"))
    );
    assert!(
        runtime_hits("fn sneak() { let _ = std::os::windows::process::CommandExt::raw_arg; }\n")
            .iter()
            .any(|hit| hit.contains("std::os::windows::process"))
    );

    let bypass = std::env::temp_dir().join(format!("hofund-plugin-round2-{}", std::process::id()));
    let _ = fs::remove_dir_all(&bypass);
    let outside = bypass.join("plugins/providers/outside");
    fs::create_dir_all(outside.join("src")).expect("dir");
    fs::write(
        outside.join("Cargo.toml"),
        "[package]\nname = \"outside\"\n[dependencies]\nhofund-plugin-api = { workspace = true }\n[lints]\nworkspace = true\n",
    )
    .expect("outside manifest");
    fs::write(
        outside.join("src/lib.rs"),
        "#[path = \"../hidden/net.rs\"]\nmod net;\nfn load() { include!(concat!(env!(\"CARGO_MANIFEST_DIR\"), \"/hidden/net.rs\")); }\n",
    )
    .expect("source");
    fs::write(
        outside.join("build.rs"),
        "fn main() { let _ = std::process::Command::new(\"true\"); }\n",
    )
    .expect("build");
    let sockets = bypass.join("plugins/providers/sockets");
    fs::create_dir_all(sockets.join("src")).expect("sockets");
    fs::write(
        sockets.join("Cargo.toml"),
        "[package]\nname = \"sockets\"\n[dependencies]\nureq = \"2\"\nsocket2 = \"0.5\"\nmio = \"1\"\nrustix = \"0.38\"\ntokio = { version = \"1\", features = [\"net\"] }\n[lints]\nworkspace = true\n",
    )
    .expect("sockets manifest");
    fs::write(sockets.join("src/lib.rs"), "fn clean() {}\n").expect("sockets source");
    let registry = bypass.join("plugins/registry/evil");
    fs::create_dir_all(&registry).expect("registry");
    fs::write(registry.join("Cargo.toml"), "[package]\nname = \"evil\"\n").expect("registry");
    let found = plugin_tree_violations(&bypass.join("plugins")).join("\n");
    for needle in [
        "#[path]",
        "non-literal include!",
        "std::process",
        "depends on ureq",
        "depends on socket2",
        "depends on mio",
        "depends on rustix",
        "enables tokio feature net",
        "plugins/registry must not contain a package",
    ] {
        assert!(found.contains(needle), "missing {needle} in:\n{found}");
    }
    let _ = fs::remove_dir_all(&bypass);

    let round3 = [
        (
            "#![allow(clippy::disallowed_methods, clippy::disallowed_types)]\nfn sneak() {}\n",
            "disallowed_methods",
        ),
        ("#[allow(clippy::all)]\nfn sneak() {}\n", "clippy::all"),
        (
            "#[expect(clippy::disallowed_methods)]\nfn sneak() {}\n",
            "disallowed_methods",
        ),
        (
            "#[cfg_attr(all(), allow(clippy::disallowed_types))]\nfn sneak() {}\n",
            "disallowed_types",
        ),
        ("#[cfg(not(clippy))]\nfn sneak() {}\n", "cfg(clippy)"),
        (
            "#[cfg_attr(all(), path = \"../hidden.rs\")]\nmod net;\n",
            "cfg_attr path",
        ),
        (
            "extern crate std as s;\nfn sneak() { let _ = s::process::Command::new(\"true\"); }\n",
            "std::process",
        ),
    ];
    for (source, needle) in round3 {
        let hits = runtime_hits(source).join("\n");
        assert!(hits.contains(needle), "{needle} missing in:\n{hits}");
    }
    let fake = std::env::temp_dir().join(format!("hofund-plugin-round3-{}", std::process::id()));
    let _ = fs::remove_dir_all(&fake);
    let pkg = fake.join("plugins/providers/fakeapi");
    fs::create_dir_all(pkg.join("src")).expect("dir");
    fs::write(
        pkg.join("Cargo.toml"),
        "[package]\nname = \"fakeapi\"\n[dependencies]\nhofund-plugin-api = { path = \"/tmp/fakeapi\" }\n[lints]\nworkspace = true\n",
    )
    .expect("manifest");
    fs::write(pkg.join("src/lib.rs"), "fn clean() {}\n").expect("source");
    let found = plugin_tree_violations(&fake.join("plugins")).join("\n");
    assert!(
        found.contains("hofund-plugin-api must be workspace = true"),
        "{found}"
    );
    let _ = fs::remove_dir_all(&fake);
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum DepKind {
    Normal,
    Dev,
    Build,
}

#[derive(Clone, Debug, PartialEq, Eq)]
struct DeclaredDep {
    /// Manifest key. A rename keeps this different from [`Self::name`].
    key: String,
    /// Package name after `package =`.
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
    let (aliases, mut hits) = collect_seam(&file);
    let mut finder = Finder {
        aliases,
        hits: Vec::new(),
    };
    finder.visit_file(&file);
    hits.append(&mut finder.hits);
    hits.sort();
    hits.dedup();
    hits
}

fn seam_hits_file(path: &Path, seen: &mut BTreeSet<PathBuf>) -> Vec<String> {
    if !seen.insert(path.to_path_buf()) {
        return Vec::new();
    }
    let Ok(text) = fs::read_to_string(path) else {
        return vec![format!("unreadable {}", path.display())];
    };
    let mut hits = seam_hits(&text);
    let Some(dir) = path.parent() else {
        return hits;
    };
    for included in include_literals(&text) {
        hits.extend(seam_hits_file(&dir.join(included), seen));
    }
    hits
}

fn runtime_hits(source: &str) -> Vec<String> {
    xtask::boundary::scan_source(source)
}

fn runtime_hits_file(path: &Path, seen: &mut BTreeSet<PathBuf>) -> Vec<String> {
    if !seen.insert(path.to_path_buf()) {
        return Vec::new();
    }
    let Ok(text) = fs::read_to_string(path) else {
        return vec![format!("unreadable {}", path.display())];
    };
    let mut hits = runtime_hits(&text);
    let Some(dir) = path.parent() else {
        return hits;
    };
    for included in include_literals(&text) {
        hits.extend(runtime_hits_file(&dir.join(included), seen));
    }
    hits
}

fn include_literals(source: &str) -> Vec<String> {
    let Ok(file) = syn::parse_file(source) else {
        return Vec::new();
    };
    let mut found = Vec::new();
    IncludeFinder { found: &mut found }.visit_file(&file);
    found
}

struct IncludeFinder<'a> {
    found: &'a mut Vec<String>,
}

impl<'ast> Visit<'ast> for IncludeFinder<'_> {
    fn visit_macro(&mut self, mac: &'ast syn::Macro) {
        if mac.path.is_ident("include")
            && let Ok(literal) = syn::parse2::<syn::LitStr>(mac.tokens.clone())
        {
            self.found.push(literal.value());
        }
        syn::visit::visit_macro(self, mac);
    }
}

fn collect_seam(file: &syn::File) -> (Aliases, Vec<String>) {
    let mut aliases = Aliases::default();
    let mut hits = Vec::new();
    UseCollector {
        aliases: &mut aliases,
        hits: &mut hits,
    }
    .visit_file(file);
    TypeCollector {
        aliases: &mut aliases,
    }
    .visit_file(file);
    (aliases, hits)
}

struct UseCollector<'a> {
    aliases: &'a mut Aliases,
    hits: &'a mut Vec<String>,
}

impl<'ast> Visit<'ast> for UseCollector<'_> {
    fn visit_item_use(&mut self, item: &'ast syn::ItemUse) {
        walk_use(&[], &item.tree, self.aliases, self.hits);
    }

    fn visit_item_extern_crate(&mut self, item: &'ast syn::ItemExternCrate) {
        note_extern_crate(item, self.aliases);
    }
}

struct TypeCollector<'a> {
    aliases: &'a mut Aliases,
}

impl<'ast> Visit<'ast> for TypeCollector<'_> {
    fn visit_item_type(&mut self, item: &'ast syn::ItemType) {
        note_type_alias(item, self.aliases);
    }
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
        for stmt in parsed_macro_stmts(mac.tokens.clone()) {
            self.visit_stmt(&stmt);
        }
    }

    fn visit_attribute(&mut self, attr: &'ast syn::Attribute) {
        if let syn::Meta::List(list) = &attr.meta {
            for stmt in parsed_macro_stmts(list.tokens.clone()) {
                self.visit_stmt(&stmt);
            }
        }
        syn::visit::visit_attribute(self, attr);
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

fn ident_str(ident: &syn::Ident) -> String {
    let text = ident.to_string();
    text.strip_prefix("r#").unwrap_or(&text).to_owned()
}

fn segments(path: &syn::Path) -> Vec<String> {
    path.segments
        .iter()
        .map(|segment| ident_str(&segment.ident))
        .collect()
}

fn canonical_segments(segs: &[String], crates: &BTreeMap<String, String>) -> Vec<String> {
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
            let Some(real) = crates.get(slot.as_str()).cloned() else {
                continue;
            };
            if real == *slot {
                continue;
            }
            *slot = real;
            changed = true;
        }
        if !changed {
            break;
        }
    }
    out
}

fn note_extern_crate(item: &syn::ItemExternCrate, aliases: &mut Aliases) {
    let original = ident_str(&item.ident);
    if original != "hofund_plugin_api" {
        return;
    }
    let local = item
        .rename
        .as_ref()
        .map(|(_, name)| ident_str(name))
        .unwrap_or_else(|| original.clone());
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
            next.push(ident_str(&path.ident));
            walk_use(&next, &path.tree, aliases, hits);
        }
        syn::UseTree::Name(name) => {
            let ident = ident_str(&name.ident);
            note_import(prefix, &ident, &ident, aliases, hits);
        }
        syn::UseTree::Rename(name) => {
            note_import(
                prefix,
                &ident_str(&name.ident),
                &ident_str(&name.rename),
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
    let imported = if name == "self" {
        prefix.to_vec()
    } else {
        let mut full = prefix.to_vec();
        full.push(name.to_owned());
        full
    };
    let full = canonical_segments(&imported, &aliases.crates);
    if full == ["hofund_plugin_api"] {
        aliases
            .crates
            .insert(local.to_owned(), "hofund_plugin_api".to_owned());
    }
    if kernel_under_plugin_api(&full) {
        hits.push(format!("imports kernel as {local}"));
    }
    let item = full.last().map(String::as_str);
    let parent = full
        .len()
        .checked_sub(2)
        .and_then(|index| full.get(index))
        .map(String::as_str);
    if item == Some("from_backends") {
        hits.push(format!("imports from_backends as {local}"));
        aliases
            .functions
            .insert(local.to_owned(), "from_backends".to_owned());
    }
    if parent == Some("SurfaceHost") && item == Some("from_parts") {
        hits.push(format!("imports SurfaceHost::from_parts as {local}"));
        aliases
            .functions
            .insert(local.to_owned(), "SurfaceHost::from_parts".to_owned());
    }
    if parent == Some("AuthResult") && item == Some("new") {
        hits.push(format!("imports AuthResult::new as {local}"));
        aliases
            .functions
            .insert(local.to_owned(), "AuthResult::new".to_owned());
    }
    if matches!(item, Some("SurfaceHost" | "AuthResult" | "HostContext")) {
        aliases
            .types
            .insert(local.to_owned(), item.unwrap_or(name).to_owned());
    }
}

fn kernel_under_plugin_api(segments: &[String]) -> bool {
    segments
        .windows(2)
        .any(|pair| pair[0] == "hofund_plugin_api" && pair[1] == "kernel")
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
        if kind.starts_with('.') {
            continue;
        }
        if kind == "registry" {
            if registry_has_manifest(&kind_path) {
                violations.push("plugins/registry must not contain a package".to_owned());
            }
            continue;
        }
        if !kinds.contains(&kind.as_ref()) {
            violations.push(format!("unknown plugin kind {kind}"));
            continue;
        }
        let Ok(plugins_in_kind) = fs::read_dir(&kind_path) else {
            violations.push(format!("unreadable plugin kind {kind}"));
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
                violations.push(format!("{kind}/{id} manifest is missing"));
                continue;
            }
            let Ok(manifest) = fs::read_to_string(&manifest_path) else {
                violations.push(format!("{kind}/{id} manifest is unreadable"));
                continue;
            };
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
                if dep.name == "hofund-plugin-api" && dep.key != "hofund-plugin-api" {
                    violations.push(format!("{kind}/{id} renames hofund-plugin-api"));
                }
                if dep.name == "tokio" && dep.key != "tokio" {
                    violations.push(format!("{kind}/{id} renames tokio"));
                }
                if dep.key == "std" || dep.name == "std" {
                    violations.push(format!("{kind}/{id} depends on a std shim"));
                }
                if dep.kind == DepKind::Dev {
                    let kit = own_kit(kind.as_ref()).unwrap_or("");
                    if dep.name.starts_with("hofund-kit-") && dep.name != kit {
                        violations.push(format!("{kind}/{id} dev-depends on {}", dep.name));
                    }
                }
                let dev = dep.kind == DepKind::Dev;
                if let Some(hit) = xtask::boundary::dependency_error(
                    &dep.name,
                    &dep.features,
                    dev,
                    own_kit(kind.as_ref()),
                ) {
                    violations.push(format!("{kind}/{id} {hit}"));
                }
            }
            if let Ok(errors) =
                xtask::boundary::manifest_dependency_errors(&manifest, own_kit(kind.as_ref()))
            {
                for error in errors {
                    violations.push(format!("{kind}/{id} {error}"));
                }
            }
            if path.join("src").is_dir() {
                for source in rust_files(&path.join("src")) {
                    let file = source.file_name().unwrap_or_default().to_string_lossy();
                    for hit in runtime_hits_file(&source, &mut BTreeSet::new()) {
                        violations.push(format!("{kind}/{id} uses {hit} in {file}"));
                    }
                }
            }
            let build = path.join("build.rs");
            if build.is_file() {
                for hit in runtime_hits_file(&build, &mut BTreeSet::new()) {
                    violations.push(format!("{kind}/{id} uses {hit} in build.rs"));
                }
                for hit in seam_hits_file(&build, &mut BTreeSet::new()) {
                    violations.push(format!("{kind}/{id} {hit} in build.rs"));
                }
            }
            for hit in xtask::boundary::source_violations(&path) {
                violations.push(format!("{kind}/{id} {hit}"));
            }
        }
    }
    violations
}

fn own_kit(kind: &str) -> Option<&'static str> {
    match kind {
        "providers" => Some("hofund-kit-upstream"),
        "subscriptions" => Some("hofund-kit-subscription"),
        "agents" => Some("hofund-kit-agent"),
        _ => None,
    }
}

fn registry_has_manifest(dir: &Path) -> bool {
    let Ok(entries) = fs::read_dir(dir) else {
        return true;
    };
    for entry in entries.flatten() {
        let path = entry.path();
        if entry.file_name() == "Cargo.toml" {
            return true;
        }
        if path.is_dir() && registry_has_manifest(&path) {
            return true;
        }
    }
    false
}

fn manifest_enables_kernel(manifest: &str) -> bool {
    let deps = manifest_dependencies(manifest);
    let keys = dep_keys(&deps);
    let dependency = deps.iter().any(|dep| {
        dep.name == "hofund-plugin-api" && dep.features.iter().any(|feature| feature == "kernel")
    });
    dependency || feature_forwards_kernel(manifest, &keys)
}

fn dep_keys(deps: &[DeclaredDep]) -> BTreeMap<String, String> {
    deps.iter()
        .map(|dep| (dep.key.clone(), dep.name.clone()))
        .collect()
}

fn feature_forwards_kernel(manifest: &str, keys: &BTreeMap<String, String>) -> bool {
    let Ok(value) = toml::from_str::<toml::Value>(manifest) else {
        return false;
    };
    let Some(features) = value.get("features").and_then(toml::Value::as_table) else {
        return false;
    };
    features
        .keys()
        .any(|name| feature_list_enables_kernel(name, features, keys, &mut BTreeSet::new()))
}

fn feature_list_enables_kernel(
    name: &str,
    features: &toml::map::Map<String, toml::Value>,
    keys: &BTreeMap<String, String>,
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
        if is_kernel_forward(text, keys)
            || (!text.contains('/') && feature_list_enables_kernel(text, features, keys, seen))
        {
            return true;
        }
    }
    false
}

fn is_kernel_forward(text: &str, keys: &BTreeMap<String, String>) -> bool {
    let text = text.strip_prefix("dep:").unwrap_or(text);
    let Some((dep, feature)) = text.split_once('/') else {
        return false;
    };
    let dep = dep.strip_suffix('?').unwrap_or(dep);
    let package = keys.get(dep).map(String::as_str).unwrap_or(dep);
    package == "hofund-plugin-api" && feature == "kernel"
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
                key: key.clone(),
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
                    key: key.clone(),
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
                let key = dep
                    .get("rename")
                    .and_then(|rename| rename.as_str())
                    .unwrap_or(dep_name);
                dependencies.push(DeclaredDep {
                    key: key.to_owned(),
                    name: dep_name.to_owned(),
                    kind,
                    features,
                });
            }
        }
        let keys = dep_keys(&dependencies);
        let direct = dependencies.iter().any(|dep| {
            dep.name == "hofund-plugin-api"
                && dep.features.iter().any(|feature| feature == "kernel")
        });
        let forwarded = package
            .get("features")
            .and_then(|features| features.as_object())
            .is_some_and(|features| json_features_enable_kernel(features, &keys));
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

fn json_features_enable_kernel(
    features: &serde_json::Map<String, serde_json::Value>,
    keys: &BTreeMap<String, String>,
) -> bool {
    fn enables(
        name: &str,
        features: &serde_json::Map<String, serde_json::Value>,
        keys: &BTreeMap<String, String>,
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
            if is_kernel_forward(text, keys)
                || (!text.contains('/') && enables(text, features, keys, seen))
            {
                return true;
            }
        }
        false
    }
    features
        .keys()
        .any(|name| enables(name, features, keys, &mut BTreeSet::new()))
}
