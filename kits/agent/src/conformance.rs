//! Golden-file conformance for compiled agent descriptions.

use std::collections::{BTreeMap, BTreeSet};
use std::ffi::OsString;
use std::fs;
use std::path::{Path, PathBuf};
use std::sync::Arc;

use hofund_agent::{
    AgentAdapter, CREDENTIAL_CHANGE_PREFIX, Compiled, FileChange, GatewayTarget, Plan, Session,
    TargetOs, resolve,
};
use hofund_plugin_api::{AgentDescription, FileKind};
use serde::Deserialize;

use crate::MemoryCredentialStore;

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
    #[serde(default)]
    configured_model: Option<String>,
    only: Option<Vec<String>>,
}

const OPERATING_SYSTEMS: [(&str, TargetOs); 3] = [
    ("linux", TargetOs::Linux),
    ("macos", TargetOs::Macos),
    ("windows", TargetOs::Windows),
];

/// Runs every golden case for `description` against its target operating systems.
///
/// Cases under `all/` run on Linux, macOS, and Windows; cases under an OS
/// directory run only on that OS. A case name cannot occur in both layouts.
/// Per-OS cases must occur in every OS directory unless `only` names their
/// exact directories; `only` is not valid in `all/`. Every OS needs coverage.
/// Identical per-OS trees and shared cases whose resolved paths differ by OS
/// are layout errors. Each case has a `case.toml`, an optional `before/` home
/// tree, and an `after/` tree containing the files changed by connect. Status
/// is checked before apply, after apply, and after forced disconnect. The
/// optional `configured_model` case field is checked after apply; absent means
/// no configured model. This helper panics with the agent and case names when
/// a contract fails.
/// `UPDATE_GOLDEN=1` rewrites `after/`, then moves identical three-OS cases to
/// `all/` and removes their duplicates. `scratch` is a caller-owned directory
/// whose lifetime must cover the entire conformance run.
pub fn conformance(description: &'static AgentDescription, fixtures: &Path, scratch: &Path) {
    let (shared, per_os) = golden_layout(description, fixtures)
        .unwrap_or_else(|error| panic!("{}: {error}", description.id().as_str()));

    for case in &shared {
        verify_shared_paths(description, case, scratch);
        for (_, os) in OPERATING_SYSTEMS {
            run_case(description, os, case, scratch);
        }
    }
    for (_, os, cases) in &per_os {
        for case in cases {
            run_case(description, *os, case, scratch);
        }
    }
    if updating_goldens() {
        dedupe_goldens(fixtures, &per_os);
    }
}

type GoldenLayout = (Vec<PathBuf>, [(&'static str, TargetOs, Vec<PathBuf>); 3]);

fn golden_layout(
    description: &'static AgentDescription,
    fixtures: &Path,
) -> Result<GoldenLayout, String> {
    let shared = golden_cases(&fixtures.join("all"))?;
    let per_os = [
        (
            "linux",
            TargetOs::Linux,
            golden_cases(&fixtures.join("linux"))?,
        ),
        (
            "macos",
            TargetOs::Macos,
            golden_cases(&fixtures.join("macos"))?,
        ),
        (
            "windows",
            TargetOs::Windows,
            golden_cases(&fixtures.join("windows"))?,
        ),
    ];
    validate_layout(description, fixtures, &shared, &per_os)?;
    Ok((shared, per_os))
}

fn golden_cases(root: &Path) -> Result<Vec<PathBuf>, String> {
    let mut cases = Vec::new();
    match fs::read_dir(root) {
        Ok(entries) => {
            for entry in entries {
                let path = entry
                    .map_err(|error| format!("read {}: {error}", root.display()))?
                    .path();
                if !path.join("case.toml").is_file() {
                    return Err(format!(
                        "golden case directory {} must contain case.toml",
                        path.display()
                    ));
                }
                cases.push(path);
            }
        }
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => {}
        Err(error) => return Err(format!("read {}: {error}", root.display())),
    }
    cases.sort();
    Ok(cases)
}

fn validate_layout(
    description: &'static AgentDescription,
    fixtures: &Path,
    shared: &[PathBuf],
    per_os: &[(&str, TargetOs, Vec<PathBuf>); 3],
) -> Result<(), String> {
    match fs::read_dir(fixtures) {
        Ok(entries) => {
            for entry in entries {
                let entry =
                    entry.map_err(|error| format!("read {}: {error}", fixtures.display()))?;
                let name = entry.file_name().to_string_lossy().into_owned();
                if !OPERATING_SYSTEMS
                    .iter()
                    .any(|(directory, _)| *directory == name)
                    && name != "all"
                {
                    return Err(format!(
                        "unknown top-level fixture entry `{name}` at {}",
                        entry.path().display()
                    ));
                }
            }
        }
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => {}
        Err(error) => return Err(format!("read {}: {error}", fixtures.display())),
    }
    for (directory, _, cases) in per_os {
        if shared.is_empty() && cases.is_empty() {
            return Err(format!("{directory} has no golden cases"));
        }
    }
    for case in shared {
        if case_targets(case)?.is_some() {
            return Err(format!(
                "shared case `{}` must not declare only",
                case.display()
            ));
        }
        let name = case_name(case);
        for (directory, _, cases) in per_os {
            if cases.iter().any(|candidate| case_name(candidate) == name) {
                return Err(format!(
                    "{}: case `{name}` occurs in both all/ and {directory}/",
                    description.id().as_str()
                ));
            }
        }
    }

    let mut by_name: BTreeMap<&str, Vec<(&str, &Path)>> = BTreeMap::new();
    for (directory, _, cases) in per_os {
        for case in cases {
            by_name
                .entry(case_name(case))
                .or_default()
                .push((directory, case));
        }
    }
    for (name, cases) in by_name {
        let targets = case_targets(cases[0].1)?;
        for (_, case) in cases.iter().skip(1) {
            if case_targets(case)? != targets {
                return Err(format!("case `{name}` has inconsistent only declarations"));
            }
        }
        let expected = targets.as_ref().map_or_else(
            || {
                OPERATING_SYSTEMS
                    .iter()
                    .map(|(directory, _)| (*directory).to_owned())
                    .collect::<BTreeSet<_>>()
            },
            |targets| targets.clone(),
        );
        let actual = cases
            .iter()
            .map(|(directory, _)| (*directory).to_owned())
            .collect::<BTreeSet<_>>();
        if actual != expected {
            return Err(format!(
                "case `{name}` occurs in {actual:?}, but its layout requires {expected:?}"
            ));
        }
        if targets.is_none() && !updating_goldens() {
            let first = tree(cases[0].1);
            if cases.iter().skip(1).all(|(_, case)| tree(case) == first) {
                return Err(format!(
                    "{}: case `{name}` has byte-identical per-OS trees in {}; the trees must differ or the case must be shared via all/ for all three OS",
                    description.id().as_str(),
                    fixtures.display()
                ));
            }
        }
    }
    Ok(())
}

fn case_targets(case: &Path) -> Result<Option<BTreeSet<String>>, String> {
    let path = case.join("case.toml");
    let text =
        fs::read_to_string(&path).map_err(|error| format!("read {}: {error}", path.display()))?;
    let parsed: Case =
        toml::from_str(&text).map_err(|error| format!("parse {}: {error}", path.display()))?;
    let Some(only) = parsed.only else {
        return Ok(None);
    };
    if only.is_empty() {
        return Err(format!(
            "case `{}` only must list at least one OS",
            case.display()
        ));
    }
    let mut targets = BTreeSet::new();
    for directory in only {
        if !OPERATING_SYSTEMS
            .iter()
            .any(|(candidate, _)| *candidate == directory)
        {
            return Err(format!(
                "case `{}` declares unknown OS `{directory}` in only",
                case.display()
            ));
        }
        if !targets.insert(directory.clone()) {
            return Err(format!(
                "case `{}` declares duplicate OS `{directory}` in only",
                case.display()
            ));
        }
    }
    if targets.len() == OPERATING_SYSTEMS.len() {
        return Err(format!(
            "case `{}` only lists every OS; use all/ instead",
            case.display()
        ));
    }
    Ok(Some(targets))
}

fn case_name(case: &Path) -> &str {
    case.file_name()
        .and_then(|name| name.to_str())
        .expect("UTF-8 golden case name")
}

fn verify_shared_paths(description: &'static AgentDescription, case_dir: &Path, scratch: &Path) {
    let case = read_case(case_dir);
    let home = scratch.join("shared-home");
    fs::create_dir_all(&home).expect("conformance temp dir");
    let session = session_for(&home, &case, None);
    let mut expected = None;
    for (_, os) in OPERATING_SYSTEMS {
        let paths = description
            .files()
            .iter()
            .map(|file| {
                let path = resolve(file, os, &session);
                (
                    file.id(),
                    path.strip_prefix(&home)
                        .unwrap_or_else(|_| {
                            panic!(
                                "{}: resolved path escapes the test home: {}",
                                case_dir.display(),
                                path.display()
                            )
                        })
                        .to_path_buf(),
                )
            })
            .collect::<Vec<_>>();
        match &expected {
            None => expected = Some(paths),
            Some(expected) => assert_eq!(
                *expected,
                paths,
                "{}: all/ case resolves to different paths on different operating systems",
                case_dir.display()
            ),
        }
    }
    let _ = fs::remove_dir_all(&home);
}

fn dedupe_goldens(fixtures: &Path, per_os: &[(&str, TargetOs, Vec<PathBuf>); 3]) {
    let mut by_name: BTreeMap<&str, Vec<&Path>> = BTreeMap::new();
    for (_, _, cases) in per_os {
        for case in cases {
            by_name.entry(case_name(case)).or_default().push(case);
        }
    }
    for (name, cases) in by_name {
        if cases.len() != OPERATING_SYSTEMS.len() {
            continue;
        }
        let first = tree(cases[0]);
        if cases.iter().skip(1).any(|case| tree(case) != first) {
            continue;
        }
        let destination = fixtures.join("all").join(name);
        assert!(
            !destination.exists(),
            "cannot deduplicate `{}`: {} already exists",
            cases[0].display(),
            destination.display()
        );
        fs::create_dir_all(destination.parent().expect("all parent"))
            .expect("create all golden directory");
        fs::rename(cases[0], &destination).expect("move identical golden to all");
        for case in cases.iter().skip(1) {
            fs::remove_dir_all(case).expect("remove duplicate golden");
        }
    }
}

fn updating_goldens() -> bool {
    updating_goldens_value(std::env::var_os("UPDATE_GOLDEN").as_deref())
}

fn updating_goldens_value(value: Option<&std::ffi::OsStr>) -> bool {
    value == Some(std::ffi::OsStr::new("1"))
}

fn run_case(description: &'static AgentDescription, os: TargetOs, case_dir: &Path, scratch: &Path) {
    let label = format!("{} / {}", description.id().as_str(), case_dir.display());
    let case = read_case(case_dir);
    let home = scratch.join(format!("{os:?}-{}-home", case_name(case_dir)));
    fs::create_dir_all(&home).expect("conformance temp dir");
    let before_dir = case_dir.join("before");
    if before_dir.is_dir() {
        copy_tree(&before_dir, &home);
    }
    let before = tree(&home);
    // An agent that reads its key from the OS credential store gets an
    // in-memory one: conformance never touches the real store.
    let store = description
        .credential()
        .map(|_| Arc::new(MemoryCredentialStore::new()));
    let session = session_for(&home, &case, store.as_ref());
    let target = GatewayTarget::new(&case.base_url, &case.token, case.routes.clone());
    let adapter = Compiled::new(description).with_os(os);
    let after_dir = case_dir.join("after");
    if updating_goldens() {
        let plan = adapter.plan(&session, &target).expect("golden plan");
        update_golden(&home, &after_dir, &plan);
    }
    let after = tree(&after_dir);
    let expected = overlay(&before, &after);

    let initial = adapter.status(&session, &target).expect("initial status");
    assert!(!initial.connected(), "{label}: connected before apply");
    let plan = adapter.plan(&session, &target).expect("plan");
    verify_plan(&label, &home, &plan, &before, &after);
    assert_eq!(tree(&home), before, "{label}: plan wrote files");
    if let Some(store) = &store {
        assert!(store.items().is_empty(), "{label}: plan wrote the store");
    }
    let applied = adapter.apply(&session, &target).expect("apply");
    verify_plan(&label, &home, &applied, &before, &after);
    if let Some(store) = &store {
        verify_store(&label, &home, store, &case.token);
        let paths = credential_paths(applied.changes());
        assert_eq!(
            paths,
            credential_paths(plan.changes()),
            "{label}: applied store items differ from the plan"
        );
        assert_eq!(
            paths,
            store
                .items()
                .iter()
                .map(|item| PathBuf::from(format!("{CREDENTIAL_CHANGE_PREFIX}{}", item.url())))
                .collect::<BTreeSet<_>>(),
            "{label}: planned store items"
        );
    }
    let connected = adapter.status(&session, &target).expect("applied status");
    assert!(connected.connected(), "{label}: not connected after apply");
    assert_eq!(
        adapter
            .configured_model(&session)
            .expect("applied configured model")
            .as_deref(),
        case.configured_model.as_deref(),
        "{label}: configured model after apply"
    );
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
        "{label}: second apply was not idempotent: {second:?}"
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
    if let Some(store) = &store {
        verify_store(&label, &home, store, &case.token);
    }

    let force_home = scratch.join(format!("{os:?}-{}-force", case_name(case_dir)));
    fs::create_dir_all(&force_home).expect("conformance temp dir");
    copy_tree(&home, &force_home);
    let force_store = store.as_ref().map(|store| Arc::new(store.fork()));
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
    if let Some(store) = &store {
        verify_store(&label, &home, store, &case.refreshed_token);
        assert_eq!(
            credential_path_set(&updated),
            credential_paths(applied.changes()),
            "{label}: refreshed store items"
        );
    }
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
    // The edit changed a value hofund wrote only where its token was.
    let edited = refreshed_expected
        .values()
        .any(|bytes| String::from_utf8_lossy(bytes).contains(&case.refreshed_token));
    assert!(
        !edited || !drift.warnings().is_empty(),
        "{label}: drift warning missing"
    );
    if let Some(store) = &store {
        verify_store_released(&label, &home, store);
    }
    let drifted = managed_tree(&home);
    let gateway_values = gateway_values(&case);
    for (path, bytes) in &drifted {
        if is_directory(path) {
            continue;
        }
        if description.files().iter().all(|file| {
            resolve(file, os, &session).strip_prefix(&home).ok() != Some(path.as_path())
        }) {
            assert_eq!(
                bytes,
                before.get(path).expect("unmanaged file before apply"),
                "{label}: unmanaged file changed in {path:?}"
            );
            continue;
        }
        let text = std::str::from_utf8(bytes).expect("drift UTF-8");
        assert!(
            text.contains("user_added"),
            "{label}: non-owned edit lost in {path:?}"
        );
        let surviving = surviving_gateway_values(text, &gateway_values);
        assert!(
            surviving.is_empty(),
            "{label}: gateway values survived disconnect in {path:?}: {surviving:?}"
        );
    }
    for (path, before_bytes) in &refreshed_expected {
        let before_text = std::str::from_utf8(before_bytes).expect("refresh UTF-8");
        if before_text.contains(&case.refreshed_token) {
            let text = std::str::from_utf8(&drifted[path]).expect("owned drift UTF-8");
            // A token inside hofund's shared entries goes with them, and the
            // disconnect says so.
            assert!(
                text.contains("user-token") || reports_removed_entries(drift.warnings()),
                "{label}: owned edit lost"
            );
        }
    }

    let force_session = session_for(&force_home, &case, force_store.as_ref());
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
    if let Some(store) = &force_store {
        verify_store_released(&label, &force_home, store);
    }
    let disconnected = adapter
        .status(&force_session, &target)
        .expect("forced disconnect status");
    assert!(
        !disconnected.connected(),
        "{label}: connected after forced disconnect"
    );
    let _ = fs::remove_dir_all(&home);
    let _ = fs::remove_dir_all(&force_home);
}

fn read_case(case_dir: &Path) -> Case {
    let label = case_dir.display();
    toml::from_str(&fs::read_to_string(case_dir.join("case.toml")).expect("read case.toml"))
        .unwrap_or_else(|error| panic!("{label}: parse case.toml: {error}"))
}

fn gateway_values(case: &Case) -> [String; 3] {
    let origin = case.base_url.trim_end_matches('/').to_owned();
    [origin, case.token.clone(), case.refreshed_token.clone()]
}

fn surviving_gateway_values<'a>(text: &str, values: &'a [String]) -> Vec<&'a str> {
    values
        .iter()
        .filter(|value| !value.is_empty() && text.contains(value.as_str()))
        .map(String::as_str)
        .collect()
}

fn session_for(home: &Path, case: &Case, store: Option<&Arc<MemoryCredentialStore>>) -> Session {
    let mut session = Session::new(home);
    if let Some(store) = store {
        session = session.with_credential_store(store.clone());
    }
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
    for change in file_changes(plan) {
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
        after
            .keys()
            .filter(|path| !is_directory(path))
            .cloned()
            .collect(),
        "{label}: after tree does not match plan paths"
    );
}

fn expected_backups(
    home: &Path,
    plan: &Plan,
    before: &BTreeMap<PathBuf, Vec<u8>>,
) -> BTreeMap<PathBuf, Vec<u8>> {
    file_changes(plan)
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
    file_changes(plan)
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
    for change in file_changes(plan) {
        let relative = change.path().strip_prefix(home).expect("golden path");
        write(&after.join(relative), change.after());
    }
}

/// The changes to files; store items are checked against the store.
fn file_changes(plan: &Plan) -> impl Iterator<Item = &FileChange> {
    plan.changes()
        .iter()
        .filter(|change| !is_credential_path(change.path()))
}

fn is_credential_path(path: &Path) -> bool {
    path.to_string_lossy().starts_with(CREDENTIAL_CHANGE_PREFIX)
}

fn credential_paths(changes: &[FileChange]) -> BTreeSet<PathBuf> {
    credential_path_set(
        &changes
            .iter()
            .map(|change| change.path().to_path_buf())
            .collect::<Vec<_>>(),
    )
}

fn credential_path_set(paths: &[PathBuf]) -> BTreeSet<PathBuf> {
    paths
        .iter()
        .filter(|path| is_credential_path(path))
        .cloned()
        .collect()
}

/// The store holds exactly one item, whose secret is `token`, and one record
/// beside the managed file names it without the key.
fn verify_store(label: &str, home: &Path, store: &MemoryCredentialStore, token: &str) {
    let items = store.items();
    assert_eq!(items.len(), 1, "{label}: store items");
    assert_eq!(
        store.secret(items[0].url(), items[0].account()).as_deref(),
        Some(token),
        "{label}: store secret"
    );
    let records = marker_tree(home, ".hofund.credential");
    assert_eq!(records.len(), 1, "{label}: credential records");
    for bytes in records.values() {
        let text = String::from_utf8_lossy(bytes);
        assert!(
            !text.contains(token),
            "{label}: credential record holds the key"
        );
        assert!(
            text.contains(items[0].url()),
            "{label}: credential record names another item"
        );
    }
}

/// Disconnect removed the item and its record.
fn verify_store_released(label: &str, home: &Path, store: &MemoryCredentialStore) {
    assert!(
        store.items().is_empty(),
        "{label}: store item survived disconnect"
    );
    assert!(
        marker_tree(home, ".hofund.credential").is_empty(),
        "{label}: credential record survived disconnect"
    );
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
        let Some(kind) = description
            .files()
            .iter()
            .find(|file| resolve(file, os, session).strip_prefix(home).ok() == Some(&relative))
            .map(|file| file.kind())
        else {
            continue;
        };
        text = match kind {
            FileKind::Json => {
                let mut value: serde_json::Value = serde_json::from_str(&text).expect("edit JSON");
                let added = serde_json::Value::String("keep".to_owned());
                // A root array gets the user's own object appended.
                if let Some(items) = value.as_array_mut() {
                    items.push(serde_json::json!({ "user_added": added }));
                } else {
                    value
                        .as_object_mut()
                        .expect("JSON root")
                        .insert("user_added".to_owned(), added);
                }
                let mut bytes = serde_json::to_vec_pretty(&value).expect("render JSON");
                bytes.push(b'\n');
                String::from_utf8(bytes).expect("JSON UTF-8")
            }
            FileKind::Toml => toml_with_user_key(&text),
            FileKind::Env => format!("{text}user_added=keep\n"),
            FileKind::Yaml => format!("{text}user_added: keep\n"),
            _ => panic!("unsupported fixture file kind"),
        };
        write(&home.join(relative), text.as_bytes());
    }
}

/// `text` with the user's own top-level key, put before the first table so
/// it does not land inside a table hofund owns.
fn toml_with_user_key(text: &str) -> String {
    let line = "user_added = \"keep\"\n";
    let mut at = 0;
    for part in text.split_inclusive('\n') {
        if part.trim_start().starts_with('[') {
            return format!("{}{line}{}", &text[..at], &text[at..]);
        }
        at += part.len();
    }
    let gap = if text.is_empty() || text.ends_with('\n') {
        ""
    } else {
        "\n"
    };
    format!("{text}{gap}{line}")
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
            ![
                ".hofund.bak",
                ".hofund.absent",
                ".hofund.wrote",
                ".hofund.credential",
            ]
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
        .filter(|path| !is_credential_path(path))
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
    let mut entries = BTreeMap::new();
    collect_entries(root, root, &mut entries);
    entries
}

fn collect_entries(root: &Path, directory: &Path, entries: &mut BTreeMap<PathBuf, Vec<u8>>) {
    let Ok(children) = fs::read_dir(directory) else {
        return;
    };
    for child in children {
        let path = child.expect("tree entry").path();
        if path.is_dir() {
            let mut relative = path
                .strip_prefix(root)
                .expect("tree relative")
                .as_os_str()
                .to_os_string();
            relative.push("/");
            entries.insert(PathBuf::from(relative), Vec::new());
            collect_entries(root, &path, entries);
        } else if path.is_file() {
            entries.insert(
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
        let path = destination.join(&relative);
        if is_directory(&relative) {
            fs::create_dir_all(path).expect("copy directory");
        } else {
            write(&path, &bytes);
        }
    }
}

fn is_directory(path: &Path) -> bool {
    path.as_os_str().to_string_lossy().ends_with('/')
}

fn write(path: &Path, bytes: &[u8]) {
    if let Some(parent) = path.parent() {
        fs::create_dir_all(parent).expect("fixture parent");
    }
    fs::write(path, bytes).expect("fixture write");
}

#[cfg(test)]
struct Scratch(tempfile::TempDir);

#[cfg(test)]
impl std::ops::Deref for Scratch {
    type Target = Path;

    fn deref(&self) -> &Self::Target {
        self.0.path()
    }
}

#[cfg(test)]
impl AsRef<Path> for Scratch {
    fn as_ref(&self) -> &Path {
        self.0.path()
    }
}

#[cfg(test)]
fn scratch() -> Scratch {
    Scratch(tempfile::tempdir().expect("conformance temp dir"))
}

/// Whether a disconnect said it removed hofund's edited shared entries.
fn reports_removed_entries(warnings: &[String]) -> bool {
    warnings
        .iter()
        .any(|warning| warning.contains("removed its entries"))
}

#[cfg(test)]
mod tests {
    use hofund_plugin_api::{
        AgentDescription, AgentFile, AgentRoute, AgentSet, FileKind, PluginId, Slot, Visible,
    };

    use super::{
        BTreeMap, Case, TargetOs, fs, gateway_values, golden_layout, reports_removed_entries,
        run_case, scratch, surviving_gateway_values, toml_with_user_key, updating_goldens_value,
        write,
    };

    #[test]
    fn toml_user_key_goes_before_the_first_table() {
        let key = "user_added = \"keep\"\n";
        assert_eq!(
            toml_with_user_key("a = 1\n  [t]\nb = 2\n"),
            format!("a = 1\n{key}  [t]\nb = 2\n")
        );
        assert_eq!(
            toml_with_user_key("[t]\nb = 2\n"),
            format!("{key}[t]\nb = 2\n")
        );
        assert_eq!(toml_with_user_key("a = 1\n"), format!("a = 1\n{key}"));
        assert_eq!(toml_with_user_key("a = 1"), format!("a = 1\n{key}"));
        assert_eq!(toml_with_user_key(""), key);
    }

    static OS_FILES: [AgentFile; 1] = [AgentFile::from_static(
        "env",
        FileKind::Env,
        "~/linux/fixture.env",
        "~/macos/fixture.env",
        "~/windows/fixture.env",
        None,
    )];
    static OS_SETS: [AgentSet; 2] = [
        AgentSet::from_static("env", "FIXTURE_ORIGIN", Slot::Origin),
        AgentSet::from_static("env", "FIXTURE_TOKEN", Slot::Token),
    ];
    static OS_DESCRIPTION: AgentDescription = AgentDescription::from_static(
        PluginId::from_static("os-test-agent"),
        "OS Test Agent",
        "os-test",
        &OS_FILES,
        &OS_SETS,
        None,
        None,
        Visible::All,
        AgentRoute::Gateway,
        &[],
    );

    fn write_layout_case(
        root: &std::path::Path,
        directory: &str,
        name: &str,
        only: Option<&[&str]>,
    ) {
        let only = only.map_or_else(String::new, |directories| {
            let directories = directories
                .iter()
                .map(|directory| format!("\"{directory}\""))
                .collect::<Vec<_>>()
                .join(", ");
            format!("only = [{directories}]\n")
        });
        write(
            &root.join(directory).join(name).join("case.toml"),
            format!(
                "routes = []\nbase_url = \"http://127.0.0.1:3425/\"\ntoken = \"old-token\"\nrefreshed_token = \"refreshed-token\"\n{only}"
            )
            .as_bytes(),
        );
        write(
            &root
                .join(directory)
                .join(name)
                .join("after")
                .join(directory)
                .join("fixture.env"),
            b"FIXTURE_TOKEN=old-token\n",
        );
    }

    fn layout_error(root: &std::path::Path) -> String {
        golden_layout(&OS_DESCRIPTION, root)
            .map(|_| ())
            .expect_err("invalid fixture layout")
    }

    #[test]
    fn fixture_layout_rejects_unknown_top_level_entry() {
        let root = scratch();
        for directory in ["linux", "macOS", "windows"] {
            write_layout_case(&root, directory, "fresh", None);
        }
        let error = layout_error(&root);
        assert!(error.contains("macOS"), "{error}");
        let _ = fs::remove_dir_all(root);
    }

    #[test]
    fn fixture_layout_requires_every_os() {
        let root = scratch();
        for directory in ["linux", "macos"] {
            write_layout_case(&root, directory, "fresh", None);
        }
        let error = layout_error(&root);
        assert!(error.contains("windows has no golden cases"), "{error}");
        let _ = fs::remove_dir_all(&root);

        let root = scratch();
        for directory in ["linux", "windows"] {
            write_layout_case(&root, directory, "fresh", None);
        }
        let error = layout_error(&root);
        assert!(error.contains("macos has no golden cases"), "{error}");
        let _ = fs::remove_dir_all(root);
    }

    #[test]
    fn fixture_layout_requires_complete_per_os_cases() {
        let root = scratch();
        write_layout_case(&root, "all", "shared", None);
        for directory in ["macos", "windows"] {
            write_layout_case(&root, directory, "fresh", None);
        }
        let error = layout_error(&root);
        assert!(error.contains("layout requires"), "{error}");
        assert!(error.contains("linux"), "{error}");
        let _ = fs::remove_dir_all(root);
    }

    #[test]
    fn fixture_layout_requires_case_toml() {
        let root = scratch();
        fs::create_dir_all(root.join("all").join("broken")).expect("case directory");
        let error = layout_error(&root);
        assert!(error.contains("must contain case.toml"), "{error}");
        let _ = fs::remove_dir_all(root);
    }

    #[test]
    fn fixture_layout_enforces_only() {
        let root = scratch();
        write_layout_case(&root, "all", "shared", None);
        for directory in ["linux", "windows"] {
            write_layout_case(&root, directory, "selected", Some(&["linux", "windows"]));
        }
        golden_layout(&OS_DESCRIPTION, &root).expect("only layout");

        write_layout_case(&root, "windows", "selected", Some(&["linux"]));
        let error = layout_error(&root);
        assert!(error.contains("inconsistent only"), "{error}");

        write_layout_case(&root, "all", "shared", Some(&["linux"]));
        let error = layout_error(&root);
        assert!(error.contains("must not declare only"), "{error}");
        let _ = fs::remove_dir_all(&root);

        for (only, expected) in [
            (&[][..], "must list at least one OS"),
            (&["linux", "linux"][..], "duplicate OS `linux`"),
            (&["linux", "macos", "windows"][..], "use all/ instead"),
        ] {
            let root = scratch();
            write_layout_case(&root, "all", "shared", None);
            write_layout_case(&root, "linux", "selected", Some(only));
            let error = layout_error(&root);
            assert!(error.contains(expected), "{error}");
            let _ = fs::remove_dir_all(root);
        }

        let root = scratch();
        write_layout_case(&root, "all", "shared", None);
        write_layout_case(&root, "linux", "selected", Some(&["plan9"]));
        let error = layout_error(&root);
        assert!(error.contains("unknown OS `plan9`"), "{error}");
        let _ = fs::remove_dir_all(root);
    }

    #[test]
    fn real_fixture_trees_pass_layout_validation() {
        let agents = std::path::Path::new(env!("CARGO_MANIFEST_DIR"))
            .join("../../plugins/registry/tests/fixtures/agents");
        for id in [
            "fixture-toml-agent",
            "fixture-json-agent",
            "fixture-yaml-agent",
        ] {
            golden_layout(&OS_DESCRIPTION, &agents.join(id).join("fixtures"))
                .unwrap_or_else(|error| panic!("{id}: {error}"));
        }
    }

    #[test]
    fn conformance_rejects_goldens_for_the_wrong_os() {
        let root = scratch();
        let case = root.join("fresh");
        write(
            &case.join("case.toml"),
            br#"routes = []
base_url = "http://127.0.0.1:3425/"
token = "old-token"
refreshed_token = "refreshed-token"
"#,
        );
        write(
            &case.join("after/linux/fixture.env"),
            b"FIXTURE_ORIGIN=http://127.0.0.1:3425\nFIXTURE_TOKEN=old-token\n",
        );

        run_case(&OS_DESCRIPTION, TargetOs::Linux, &case, &root);
        let wrong_os = std::panic::catch_unwind(|| {
            run_case(&OS_DESCRIPTION, TargetOs::Macos, &case, &root);
        });
        assert!(wrong_os.is_err(), "macOS accepted Linux golden paths");
        let _ = fs::remove_dir_all(root);
    }

    #[test]
    fn golden_updates_require_the_exact_value_one() {
        for (value, expected) in [
            (None, false),
            (Some(""), false),
            (Some("0"), false),
            (Some("true"), false),
            (Some("1"), true),
        ] {
            assert_eq!(
                updating_goldens_value(value.map(std::ffi::OsStr::new)),
                expected
            );
        }
    }

    #[test]
    fn only_the_removed_entries_warning_explains_a_lost_token() {
        let other = vec!["warning: x changed since connect".to_owned()];
        assert!(!reports_removed_entries(&other));
        let removed = vec!["warning: x removed its entries from `k` in f".to_owned()];
        assert!(reports_removed_entries(&removed));
    }

    #[test]
    fn drift_check_detects_origin_without_trailing_slash() {
        let case = Case {
            env: BTreeMap::new(),
            routes: Vec::new(),
            base_url: "http://127.0.0.1:3425/".to_owned(),
            token: "old-token".to_owned(),
            refreshed_token: "refreshed-token".to_owned(),
            configured_model: None,
            only: None,
        };
        let values = gateway_values(&case);
        assert_eq!(
            values,
            ["http://127.0.0.1:3425", "old-token", "refreshed-token"]
        );
        assert_eq!(
            surviving_gateway_values("FIXTURE_ORIGIN=http://127.0.0.1:3425\n", &values),
            vec!["http://127.0.0.1:3425"]
        );
        assert!(
            surviving_gateway_values("user_added=keep\n", &values).is_empty(),
            "origin-free file was flagged"
        );
    }

    #[test]
    fn tree_comparison_detects_extra_and_missing_empty_directories() {
        let expected_root = scratch();
        let actual_root = scratch();
        fs::create_dir_all(actual_root.join("empty")).expect("empty directory");
        let expected = super::tree(&expected_root);
        let actual = super::tree(&actual_root);

        assert_ne!(actual, expected, "extra empty directory passed");
        assert_ne!(expected, actual, "missing empty directory passed");

        let _ = fs::remove_dir_all(expected_root);
        let _ = fs::remove_dir_all(actual_root);
    }
}
