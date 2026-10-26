//! Golden-file conformance for compiled agent descriptions.

use std::collections::{BTreeMap, BTreeSet};
use std::ffi::OsString;
use std::fs;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};

use hofund_agent::{AgentAdapter, Compiled, GatewayTarget, Plan, Session, TargetOs, resolve};
use hofund_plugin_api::{AgentDescription, FileKind};
use serde::Deserialize;

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
struct Case {
    #[serde(default)]
    env: BTreeMap<String, String>,
    #[serde(default)]
    routes: Vec<String>,
    base_url: String,
    token: String,
    refreshed_token: String,
}

/// Runs every golden case for `description` against every target operating system.
///
/// The fixture root contains `linux`, `macos`, and `windows` directories. Each
/// case has a `case.toml`, an optional `before/` home tree, and an `after/`
/// tree containing the files changed by connect. This helper panics with the
/// agent and case names when a contract fails. `UPDATE_GOLDEN=1` rewrites
/// `after/` from the plan before running the remaining assertions.
pub fn conformance(description: &'static AgentDescription, fixtures: &Path) {
    for (directory, os) in [
        ("linux", TargetOs::Linux),
        ("macos", TargetOs::Macos),
        ("windows", TargetOs::Windows),
    ] {
        let root = fixtures.join(directory);
        let mut cases = fs::read_dir(&root)
            .unwrap_or_else(|error| panic!("read {}: {error}", root.display()))
            .map(|entry| entry.expect("read golden case").path())
            .filter(|path| path.join("case.toml").is_file())
            .collect::<Vec<_>>();
        cases.sort();
        assert!(
            !cases.is_empty(),
            "{} has no golden cases for {}",
            description.id().as_str(),
            directory
        );
        for case in cases {
            run_case(description, os, &case);
        }
    }
}

fn run_case(description: &'static AgentDescription, os: TargetOs, case_dir: &Path) {
    let label = format!("{} / {}", description.id().as_str(), case_dir.display());
    let case: Case =
        toml::from_str(&fs::read_to_string(case_dir.join("case.toml")).expect("read case.toml"))
            .unwrap_or_else(|error| panic!("{label}: parse case.toml: {error}"));
    let home = scratch();
    let before_dir = case_dir.join("before");
    if before_dir.is_dir() {
        copy_tree(&before_dir, &home);
    }
    let before = tree(&home);
    let session = session_for(&home, &case);
    let target = GatewayTarget::new(&case.base_url, &case.token, case.routes.clone());
    let adapter = Compiled::new(description).with_os(os);
    let after_dir = case_dir.join("after");
    if std::env::var_os("UPDATE_GOLDEN").is_some() {
        let plan = adapter.plan(&session, &target).expect("golden plan");
        update_golden(&home, &after_dir, &plan);
    }
    let after = tree(&after_dir);
    let expected = overlay(&before, &after);

    let plan = adapter.plan(&session, &target).expect("plan");
    verify_plan(&label, &home, &plan, &before, &after);
    assert_eq!(tree(&home), before, "{label}: plan wrote files");
    let applied = adapter.apply(&session, &target).expect("apply");
    verify_plan(&label, &home, &applied, &before, &after);
    assert_eq!(
        managed_tree(&home),
        expected,
        "{label}: applied files differ from before plus after"
    );
    let backups = expected_backups(&home, &applied, &before);
    let absent = expected_absent(&home, &applied, &before);
    assert_eq!(
        marker_tree(&home, ".hofund.bak"),
        backups,
        "{label}: backups"
    );
    assert_eq!(
        marker_tree(&home, ".hofund.absent"),
        absent,
        "{label}: absent markers"
    );

    let second = adapter.apply(&session, &target).expect("second apply");
    assert!(
        second.is_empty(),
        "{label}: second apply was not idempotent"
    );
    assert_eq!(
        marker_tree(&home, ".hofund.bak"),
        backups,
        "{label}: second backup"
    );
    assert_eq!(
        marker_tree(&home, ".hofund.absent"),
        absent,
        "{label}: second absent"
    );
    assert_eq!(managed_tree(&home), expected, "{label}: second apply files");

    let force_home = scratch();
    copy_tree(&home, &force_home);
    let refreshed = target.clone().with_token(&case.refreshed_token);
    let updated = adapter
        .refresh_token(&session, &refreshed)
        .expect("refresh token");
    let refreshed_expected = replace_token(&expected, &case.token, &case.refreshed_token);
    assert_eq!(
        managed_tree(&home),
        refreshed_expected,
        "{label}: refresh changed non-token bytes"
    );
    assert_eq!(
        relative_paths(&home, &updated),
        changed_paths(&expected, &refreshed_expected),
        "{label}: refreshed paths"
    );
    assert_eq!(
        marker_tree(&home, ".hofund.bak"),
        backups,
        "{label}: refresh backup"
    );
    assert_eq!(
        marker_tree(&home, ".hofund.absent"),
        absent,
        "{label}: refresh absent"
    );

    add_user_edits(description, os, &session, &home, &case.refreshed_token);
    let drift = adapter
        .disconnect(&session, false)
        .expect("drift disconnect");
    assert!(drift.changed(), "{label}: drift disconnect changed nothing");
    assert!(
        !drift.warnings().is_empty(),
        "{label}: drift warning missing"
    );
    let drifted = managed_tree(&home);
    for (path, bytes) in &drifted {
        let text = std::str::from_utf8(bytes).expect("drift UTF-8");
        assert!(
            text.contains("user_added"),
            "{label}: non-owned edit lost in {path:?}"
        );
        assert!(
            !text.contains(&case.base_url),
            "{label}: gateway origin survived disconnect"
        );
    }
    for (path, before_bytes) in &refreshed_expected {
        let before_text = std::str::from_utf8(before_bytes).expect("refresh UTF-8");
        if before_text.contains(&case.refreshed_token) {
            let bytes = &drifted[path];
            let text = std::str::from_utf8(bytes).expect("owned drift UTF-8");
            assert!(text.contains("user-token"), "{label}: owned edit lost");
            assert!(
                !text.contains(&case.refreshed_token),
                "{label}: refreshed token survived"
            );
        }
    }

    let force_session = session_for(&force_home, &case);
    let forced = adapter
        .disconnect(&force_session, true)
        .expect("forced disconnect");
    assert!(
        forced.changed(),
        "{label}: forced disconnect changed nothing"
    );
    assert_eq!(
        tree(&force_home),
        before,
        "{label}: forced restore was not exact"
    );
    let _ = fs::remove_dir_all(&home);
    let _ = fs::remove_dir_all(&force_home);
}

fn session_for(home: &Path, case: &Case) -> Session {
    let mut session = Session::new(home);
    for (name, value) in &case.env {
        let value = value.replace("{tmp}", &home.to_string_lossy());
        session = session.with_env(name, value);
    }
    session
}

fn verify_plan(
    label: &str,
    home: &Path,
    plan: &Plan,
    before: &BTreeMap<PathBuf, Vec<u8>>,
    after: &BTreeMap<PathBuf, Vec<u8>>,
) {
    let mut planned = BTreeSet::new();
    for change in plan.changes() {
        let relative = change.path().strip_prefix(home).expect("plan path in home");
        planned.insert(relative.to_path_buf());
        assert_eq!(
            change.before(),
            before.get(relative).map_or(&[][..], Vec::as_slice),
            "{label}: plan before bytes for {relative:?}"
        );
        assert_eq!(
            change.after(),
            after.get(relative).map_or(&[][..], Vec::as_slice),
            "{label}: plan after bytes for {relative:?}"
        );
    }
    assert_eq!(
        planned,
        after.keys().cloned().collect(),
        "{label}: after tree does not match plan paths"
    );
}

fn expected_backups(
    home: &Path,
    plan: &Plan,
    before: &BTreeMap<PathBuf, Vec<u8>>,
) -> BTreeMap<PathBuf, Vec<u8>> {
    plan.changes()
        .iter()
        .filter(|change| before.contains_key(change.path().strip_prefix(home).expect("home")))
        .map(|change| {
            let relative = change.path().strip_prefix(home).expect("home");
            (sibling(relative, ".hofund.bak"), before[relative].clone())
        })
        .collect()
}

fn expected_absent(
    home: &Path,
    plan: &Plan,
    before: &BTreeMap<PathBuf, Vec<u8>>,
) -> BTreeMap<PathBuf, Vec<u8>> {
    plan.changes()
        .iter()
        .filter(|change| !before.contains_key(change.path().strip_prefix(home).expect("home")))
        .map(|change| {
            let relative = change.path().strip_prefix(home).expect("home");
            (sibling(relative, ".hofund.absent"), Vec::new())
        })
        .collect()
}

fn update_golden(home: &Path, after: &Path, plan: &Plan) {
    if after.exists() {
        fs::remove_dir_all(after).expect("remove old golden");
    }
    for change in plan.changes() {
        let relative = change.path().strip_prefix(home).expect("golden path");
        write(&after.join(relative), change.after());
    }
}

fn add_user_edits(
    description: &'static AgentDescription,
    os: TargetOs,
    session: &Session,
    home: &Path,
    refreshed_token: &str,
) {
    for (relative, bytes) in managed_tree(home) {
        let mut text = String::from_utf8(bytes).expect("edit UTF-8");
        text = text.replace(refreshed_token, "user-token");
        let kind = description
            .files()
            .iter()
            .find(|file| resolve(file, os, session).strip_prefix(home).ok() == Some(&relative))
            .map(|file| file.kind())
            .expect("managed file kind");
        text = match kind {
            FileKind::Json => {
                let mut value: serde_json::Value = serde_json::from_str(&text).expect("edit JSON");
                value.as_object_mut().expect("JSON root").insert(
                    "user_added".to_owned(),
                    serde_json::Value::String("keep".to_owned()),
                );
                let mut bytes = serde_json::to_vec_pretty(&value).expect("render JSON");
                bytes.push(b'\n');
                String::from_utf8(bytes).expect("JSON UTF-8")
            }
            FileKind::Toml => format!("{text}user_added = \"keep\"\n"),
            FileKind::Env => format!("{text}user_added=keep\n"),
            _ => panic!("unsupported fixture file kind"),
        };
        write(&home.join(relative), text.as_bytes());
    }
}

fn replace_token(
    files: &BTreeMap<PathBuf, Vec<u8>>,
    old: &str,
    new: &str,
) -> BTreeMap<PathBuf, Vec<u8>> {
    files
        .iter()
        .map(|(path, bytes)| {
            let text = std::str::from_utf8(bytes).expect("token file UTF-8");
            (path.clone(), text.replace(old, new).into_bytes())
        })
        .collect()
}

fn changed_paths(
    before: &BTreeMap<PathBuf, Vec<u8>>,
    after: &BTreeMap<PathBuf, Vec<u8>>,
) -> BTreeSet<PathBuf> {
    before
        .iter()
        .filter(|(path, bytes)| after.get(*path) != Some(*bytes))
        .map(|(path, _)| path.clone())
        .collect()
}

fn managed_tree(home: &Path) -> BTreeMap<PathBuf, Vec<u8>> {
    tree(home)
        .into_iter()
        .filter(|(path, _)| {
            let name = path.file_name().unwrap_or_default().to_string_lossy();
            ![".hofund.bak", ".hofund.absent", ".hofund.wrote"]
                .iter()
                .any(|suffix| name.ends_with(suffix))
        })
        .collect()
}

fn marker_tree(home: &Path, suffix: &str) -> BTreeMap<PathBuf, Vec<u8>> {
    tree(home)
        .into_iter()
        .filter(|(path, _)| {
            path.file_name()
                .unwrap_or_default()
                .to_string_lossy()
                .ends_with(suffix)
        })
        .collect()
}

fn overlay(
    before: &BTreeMap<PathBuf, Vec<u8>>,
    after: &BTreeMap<PathBuf, Vec<u8>>,
) -> BTreeMap<PathBuf, Vec<u8>> {
    let mut expected = before.clone();
    expected.extend(after.clone());
    expected
}

fn relative_paths(home: &Path, paths: &[PathBuf]) -> BTreeSet<PathBuf> {
    paths
        .iter()
        .map(|path| path.strip_prefix(home).expect("updated path").to_path_buf())
        .collect()
}

fn sibling(path: &Path, suffix: &str) -> PathBuf {
    let mut name = path
        .file_name()
        .map_or_else(|| OsString::from("config"), OsString::from);
    name.push(suffix);
    path.with_file_name(name)
}

fn tree(root: &Path) -> BTreeMap<PathBuf, Vec<u8>> {
    let mut files = BTreeMap::new();
    collect_files(root, root, &mut files);
    files
}

fn collect_files(root: &Path, directory: &Path, files: &mut BTreeMap<PathBuf, Vec<u8>>) {
    let Ok(entries) = fs::read_dir(directory) else {
        return;
    };
    for entry in entries {
        let path = entry.expect("tree entry").path();
        if path.is_dir() {
            collect_files(root, &path, files);
        } else if path.is_file() {
            files.insert(
                path.strip_prefix(root)
                    .expect("tree relative")
                    .to_path_buf(),
                fs::read(path).expect("tree file"),
            );
        }
    }
}

fn copy_tree(source: &Path, destination: &Path) {
    for (relative, bytes) in tree(source) {
        write(&destination.join(relative), &bytes);
    }
}

fn write(path: &Path, bytes: &[u8]) {
    if let Some(parent) = path.parent() {
        fs::create_dir_all(parent).expect("fixture parent");
    }
    fs::write(path, bytes).expect("fixture write");
}

fn scratch() -> PathBuf {
    static NEXT: AtomicU64 = AtomicU64::new(0);
    let id = NEXT.fetch_add(1, Ordering::Relaxed);
    let path = std::env::temp_dir().join(format!(
        "hofund-agent-conformance-{}-{id}",
        std::process::id()
    ));
    fs::create_dir_all(&path).expect("conformance temp dir");
    path
}
