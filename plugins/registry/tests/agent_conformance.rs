//! Golden conformance for compiled agent descriptions.

use std::fs;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};

/// The one Gemini CLI case whose drift disconnect the shared kit cannot check.
///
/// Apply moves a string `model` into `model.name`. A non-forced disconnect
/// after a user edit then fails with "`name` is not an object", exactly as the
/// frozen const does (asserted in `gemini_cli_equivalence.rs`). The kit has no
/// per-case way to expect that refusal, so this case runs on its own and only
/// that panic is accepted; every other case runs through the strict kit.
const GEMINI_DRIFT_REFUSAL_CASE: &str = "string-model";

#[test]
fn shipped_agents_match_their_golden_cases() {
    for description in hofund_plugin_registry::agents() {
        let fixtures = Path::new(env!("CARGO_MANIFEST_DIR"))
            .join("../agents")
            .join(description.id().as_str())
            .join("fixtures");
        if description.id().as_str() == "gemini-cli" {
            gemini_cli_conformance(description, &fixtures);
        } else {
            hofund_kit_agent::conformance(description, &fixtures);
        }
    }
}

#[cfg(feature = "test-fixtures")]
#[test]
fn compiled_agents_match_their_golden_cases() {
    for description in hofund_plugin_registry::fixture_agents() {
        let fixtures = Path::new(env!("CARGO_MANIFEST_DIR"))
            .join("tests/fixtures/agents")
            .join(description.id().as_str())
            .join("fixtures");
        hofund_kit_agent::conformance(description, &fixtures);
    }
}

fn gemini_cli_conformance(
    description: &'static hofund_plugin_api::AgentDescription,
    fixtures: &Path,
) {
    let refusal = fixtures.join("all").join(GEMINI_DRIFT_REFUSAL_CASE);
    assert!(
        refusal.join("case.toml").is_file(),
        "gemini-cli: missing {GEMINI_DRIFT_REFUSAL_CASE} golden case"
    );

    let strict = scratch("strict");
    copy_dir(fixtures, &strict);
    fs::remove_dir_all(strict.join("all").join(GEMINI_DRIFT_REFUSAL_CASE))
        .expect("remove refusal case from strict copy");
    hofund_kit_agent::conformance(description, &strict);
    let _ = fs::remove_dir_all(&strict);

    let single = scratch("refusal");
    copy_dir(
        &refusal,
        &single.join("all").join(GEMINI_DRIFT_REFUSAL_CASE),
    );
    let result = std::panic::catch_unwind(|| {
        hofund_kit_agent::conformance(description, &single);
    });
    let _ = fs::remove_dir_all(&single);
    let payload = result
        .expect_err("gemini-cli string-model drift disconnect now succeeds; drop the exception");
    let message = payload
        .downcast_ref::<String>()
        .map(String::as_str)
        .or_else(|| payload.downcast_ref::<&str>().copied())
        .expect("string-model drift panic payload");
    assert!(
        message.starts_with("drift disconnect:") && message.contains("`name` is not an object"),
        "unexpected gemini-cli {GEMINI_DRIFT_REFUSAL_CASE} failure: {message}"
    );
}

fn copy_dir(source: &Path, destination: &Path) {
    fs::create_dir_all(destination).expect("copy directory");
    for entry in fs::read_dir(source).expect("read fixture directory") {
        let entry = entry.expect("fixture entry");
        let target = destination.join(entry.file_name());
        if entry.file_type().expect("fixture type").is_dir() {
            copy_dir(&entry.path(), &target);
        } else {
            fs::copy(entry.path(), target).expect("copy fixture");
        }
    }
}

fn scratch(label: &str) -> PathBuf {
    static NEXT: AtomicU64 = AtomicU64::new(0);
    let path = std::env::temp_dir().join(format!(
        "hofund-agent-conformance-{label}-{}-{}",
        std::process::id(),
        NEXT.fetch_add(1, Ordering::Relaxed)
    ));
    let _ = fs::remove_dir_all(&path);
    path
}
