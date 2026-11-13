//! Moves Codex threads between model providers (D-80).
//!
//! Codex 0.166 and later list and resume a thread only on the provider it
//! was started on, which the first `session_meta` line of each rollout and
//! the thread's row in `state_<n>.sqlite` both record. Connect moves the
//! previous provider's threads onto `hofund`; disconnect moves `hofund`'s
//! threads to the provider Codex is on again (`openai` when that provider's
//! table is gone). A copy of every rewritten rollout is kept first, busy
//! threads are skipped, and a failed move never fails the config change.

use std::collections::{BTreeMap, BTreeSet};
use std::fs::{File, OpenOptions};
use std::io::{BufRead, BufReader, ErrorKind};
use std::path::{Path, PathBuf};
use std::time::{Duration, SystemTime};

use rusqlite::{Connection, OpenFlags};
use serde_json::Value;
use toml_edit::DocumentMut;

use crate::store::write_atomic;

const DEFAULT_PACKED_BUDGET: u64 = 128 * 1024 * 1024;
const ACTIVE_WINDOW: Duration = Duration::from_secs(60);
const PACKED_WINDOW: Duration = Duration::from_secs(30 * 24 * 60 * 60);
/// How many leading lines may hold a rollout's `session_meta`.
const HEAD_LINES: usize = 8;
const MAX_UNPACKED_SIZE: usize = 512 * 1024 * 1024;
const DB_BUSY_TIMEOUT: Duration = Duration::from_millis(250);
const HOFUND: &str = "hofund";
const OPENAI: &str = "openai";
const OFFICIAL_ROUTE: &str = "cc-switch-official";

#[derive(Debug, Default, PartialEq, Eq)]
pub(crate) struct MoveReport {
    pub(crate) moved: usize,
    pub(crate) notes: Vec<String>,
}

impl MoveReport {
    pub(crate) fn log(self) -> Vec<String> {
        if self.moved > 0 {
            tracing::info!(
                moved = self.moved,
                "moved Codex threads to the active provider"
            );
        }
        for note in &self.notes {
            tracing::warn!("{note}");
        }
        self.notes
    }
}

#[derive(Clone, Copy)]
enum MovePolicy {
    Connect { since: SystemTime, budget: u64 },
    All,
}

#[derive(Debug)]
struct CodexConfig {
    provider: String,
    selected_profile: bool,
    profile_providers: BTreeSet<String>,
    providers: BTreeSet<String>,
    sqlite_home: Option<PathBuf>,
}

#[derive(Debug)]
struct Rollout {
    path: PathBuf,
    provider: String,
    packed: bool,
}

#[derive(Debug)]
struct StateRow {
    db: PathBuf,
    id: String,
    provider: String,
    updated_at: Option<i64>,
}

struct Candidate {
    id: String,
    files: Vec<Rollout>,
    rows: Vec<StateRow>,
    updated_at: Option<i64>,
    packed: bool,
}

struct Backup {
    directory: PathBuf,
    items: Vec<BackupItem>,
}

struct BackupItem {
    source: PathBuf,
    bytes: Vec<u8>,
    modified: Option<SystemTime>,
}

struct ThreadLock(Option<File>);

impl Drop for ThreadLock {
    fn drop(&mut self) {
        if let Some(file) = &self.0 {
            let _ = file.unlock();
        }
    }
}

enum LockOutcome {
    Held(ThreadLock),
    Busy,
}

pub(crate) fn connect(config_path: &Path) -> MoveReport {
    connect_at(config_path, SystemTime::now(), DEFAULT_PACKED_BUDGET)
}

pub(crate) fn disconnect(config_path: &Path) -> MoveReport {
    disconnect_at(config_path, SystemTime::now())
}

fn connect_at(config_path: &Path, now: SystemTime, packed_budget: u64) -> MoveReport {
    let mut report = MoveReport::default();
    let Some(home) = config_path.parent() else {
        report.notes.push(format!(
            "Codex threads were not moved: {} has no parent directory",
            config_path.display()
        ));
        return report;
    };
    let before_path = config_path.with_file_name("config.toml.hofund.bak");
    let before_bytes = match std::fs::read(&before_path) {
        Ok(bytes) => bytes,
        Err(error) if error.kind() == ErrorKind::NotFound => {
            let absent = config_path.with_file_name("config.toml.hofund.absent");
            if absent.is_file() {
                Vec::new()
            } else {
                match std::fs::read(config_path) {
                    Ok(bytes) => bytes,
                    Err(error) => {
                        report.notes.push(format!(
                            "Codex threads were not moved: cannot read {}: {error}",
                            config_path.display()
                        ));
                        return report;
                    }
                }
            }
        }
        Err(error) => {
            report.notes.push(format!(
                "Codex threads were not moved: cannot read {}: {error}",
                before_path.display()
            ));
            return report;
        }
    };
    let before = match parse_config(&before_bytes) {
        Ok(config) => config,
        Err(error) => {
            report.notes.push(format!(
                "Codex threads were not moved: connect-time config: {error}"
            ));
            return report;
        }
    };
    if before.selected_profile {
        return report;
    }
    let current = match read_config(config_path) {
        Ok(config) => config,
        Err(error) => {
            report.notes.push(format!(
                "Codex threads were not moved: current config: {error}"
            ));
            return report;
        }
    };
    let database_home = current
        .sqlite_home
        .map(|path| {
            if path.is_absolute() {
                path
            } else {
                home.join(path)
            }
        })
        .unwrap_or_else(|| home.to_path_buf());
    let since = now
        .checked_sub(PACKED_WINDOW)
        .unwrap_or(SystemTime::UNIX_EPOCH);
    if before.provider != HOFUND
        && before.provider != OFFICIAL_ROUTE
        && !before.profile_providers.contains(&before.provider)
    {
        move_provider(
            home,
            &database_home,
            &before.provider,
            HOFUND,
            now,
            MovePolicy::Connect {
                since,
                budget: packed_budget,
            },
            &mut report,
        );
    }
    if before.provider != OFFICIAL_ROUTE && !before.profile_providers.contains(OFFICIAL_ROUTE) {
        move_provider(
            home,
            &database_home,
            OFFICIAL_ROUTE,
            HOFUND,
            now,
            MovePolicy::All,
            &mut report,
        );
    }
    report
}

fn disconnect_at(config_path: &Path, now: SystemTime) -> MoveReport {
    let mut report = MoveReport::default();
    let Some(home) = config_path.parent() else {
        report.notes.push(format!(
            "Codex threads were not moved: {} has no parent directory",
            config_path.display()
        ));
        return report;
    };
    let current = match read_config(config_path) {
        Ok(config) => config,
        Err(error) => {
            report.notes.push(format!(
                "Codex threads were not moved: restored config: {error}"
            ));
            return report;
        }
    };
    if current.provider == HOFUND {
        return report;
    }
    let target =
        if builtin_provider(&current.provider) || current.providers.contains(&current.provider) {
            current.provider.clone()
        } else {
            OPENAI.to_owned()
        };
    let database_home = current
        .sqlite_home
        .map(|path| {
            if path.is_absolute() {
                path
            } else {
                home.join(path)
            }
        })
        .unwrap_or_else(|| home.to_path_buf());
    move_provider(
        home,
        &database_home,
        HOFUND,
        &target,
        now,
        MovePolicy::All,
        &mut report,
    );
    report
}

fn move_provider(
    home: &Path,
    database_home: &Path,
    from: &str,
    to: &str,
    now: SystemTime,
    policy: MovePolicy,
    report: &mut MoveReport,
) {
    if from == to || !valid_provider(from) || !valid_provider(to) {
        return;
    }
    let mut groups = discover_rollouts(home, report);
    let rows = read_state_rows(database_home, report);
    let mut ids = BTreeSet::new();
    ids.extend(groups.keys().cloned());
    ids.extend(
        rows.iter()
            .filter(|row| row.provider == from)
            .map(|row| row.id.clone()),
    );
    let mut candidates = Vec::new();
    for id in ids {
        let files = groups.remove(&id).unwrap_or_default();
        let thread_rows: Vec<StateRow> = rows
            .iter()
            .filter(|row| row.id == id && row.provider == from)
            .map(|row| StateRow {
                db: row.db.clone(),
                id: row.id.clone(),
                provider: row.provider.clone(),
                updated_at: row.updated_at,
            })
            .collect();
        if files.is_empty()
            || !files.iter().any(|file| file.provider == from) && thread_rows.is_empty()
        {
            continue;
        }
        let updated_at = thread_rows.iter().filter_map(|row| row.updated_at).max();
        let files: Vec<Rollout> = files
            .into_iter()
            .filter(|file| match policy {
                MovePolicy::All => true,
                MovePolicy::Connect { since, .. } if file.packed => {
                    let since = since
                        .duration_since(SystemTime::UNIX_EPOCH)
                        .unwrap_or_default()
                        .as_secs();
                    updated_at.is_some_and(|updated| {
                        i64::try_from(since).map_or(true, |since| updated >= since)
                    })
                }
                MovePolicy::Connect { .. } => true,
            })
            .collect();
        if files.is_empty() {
            continue;
        }
        let packed = files.iter().any(|file| file.packed);
        candidates.push(Candidate {
            id,
            files,
            rows: thread_rows,
            updated_at,
            packed,
        });
    }
    candidates.sort_by(|left, right| {
        left.packed
            .cmp(&right.packed)
            .then_with(|| right.updated_at.cmp(&left.updated_at))
            .then_with(|| left.id.cmp(&right.id))
    });
    let mut spent = 0_u64;
    for candidate in candidates {
        if candidate.packed
            && matches!(policy, MovePolicy::Connect { budget, .. } if spent >= budget)
        {
            continue;
        }
        let size = candidate
            .files
            .iter()
            .filter(|file| file.packed)
            .filter_map(|file| std::fs::metadata(&file.path).ok())
            .map(|metadata| metadata.len())
            .sum::<u64>();
        process_candidate(home, from, to, now, candidate, report);
        spent = spent.saturating_add(size);
    }
}

fn process_candidate(
    home: &Path,
    from: &str,
    to: &str,
    now: SystemTime,
    candidate: Candidate,
    report: &mut MoveReport,
) {
    if candidate.files.iter().any(|file| {
        std::fs::metadata(&file.path)
            .and_then(|metadata| metadata.modified())
            .is_ok_and(|modified| {
                now.duration_since(modified)
                    .map_or(true, |elapsed| elapsed < ACTIVE_WINDOW)
            })
    }) {
        report.notes.push(format!(
            "Codex thread {} is busy; it stays on {from} for the next connect, model sync, or disconnect",
            candidate.id
        ));
        return;
    }
    let _lock = match acquire_thread_lock(home, &candidate.id) {
        Ok(LockOutcome::Held(lock)) => lock,
        Ok(LockOutcome::Busy) => {
            report.notes.push(format!(
                "Codex thread {} is busy because Codex holds its writer lock; it stays on {from}",
                candidate.id
            ));
            return;
        }
        Err(error) => {
            report.notes.push(error);
            return;
        }
    };

    let mut backup = None;
    let mut changed_paths = Vec::new();
    for file in &candidate.files {
        if file.provider == to {
            continue;
        }
        if backup.is_none() {
            match Backup::new(home, &candidate.id, now) {
                Ok(created) => backup = Some(created),
                Err(error) => {
                    report.notes.push(error);
                    return;
                }
            }
        }
        let kept = backup.as_mut().expect("backup created");
        if let Err(error) = kept.keep(&file.path) {
            report.notes.push(format!(
                "Codex thread {} was not moved: {error}",
                candidate.id
            ));
            return;
        }
        match rewrite_rollout(&file.path, &candidate.id, from, to) {
            Ok(true) => changed_paths.push(file.path.clone()),
            Ok(false) => {}
            Err(error) => {
                if let Some(backup) = &backup {
                    backup.restore(&changed_paths, report);
                }
                report.notes.push(format!(
                    "Codex thread {} was not moved: {error}",
                    candidate.id
                ));
                return;
            }
        }
    }

    let mut moved = !changed_paths.is_empty();
    for row in &candidate.rows {
        match update_state_row(row, from, to) {
            Ok(true) => moved = true,
            Ok(false) => {}
            Err(error) => report.notes.push(format!(
                "Codex thread {} changed on disk, but its state row did not: {error}",
                candidate.id
            )),
        }
    }
    if moved {
        report.moved += 1;
    }
}

fn acquire_thread_lock(home: &Path, id: &str) -> Result<LockOutcome, String> {
    let locks = home.join("thread-writer-locks");
    if !locks.is_dir() {
        return Ok(LockOutcome::Held(ThreadLock(None)));
    }
    let path = locks.join(format!("{}.lock", id.to_ascii_lowercase()));
    let file = OpenOptions::new()
        .read(true)
        .write(true)
        .create(true)
        .truncate(false)
        .open(&path)
        .map_err(|error| format!("cannot open Codex writer lock {}: {error}", path.display()))?;
    match file.try_lock() {
        Ok(()) => Ok(LockOutcome::Held(ThreadLock(Some(file)))),
        Err(std::fs::TryLockError::WouldBlock) => Ok(LockOutcome::Busy),
        Err(std::fs::TryLockError::Error(error)) => Err(format!(
            "cannot lock Codex writer lock {}: {error}",
            path.display()
        )),
    }
}

impl Backup {
    fn new(home: &Path, id: &str, now: SystemTime) -> Result<Self, String> {
        let root = home.join(".hofund/trash/codex-provider");
        std::fs::create_dir_all(&root)
            .map_err(|error| format!("cannot create Codex backup {}: {error}", root.display()))?;
        let stamp = now
            .duration_since(SystemTime::UNIX_EPOCH)
            .unwrap_or_default()
            .as_nanos();
        let base = format!("{stamp}-{id}");
        for suffix in 0_u32..1000 {
            let name = if suffix == 0 {
                base.clone()
            } else {
                format!("{base}-{suffix}")
            };
            let directory = root.join(name);
            match std::fs::create_dir(&directory) {
                Ok(()) => {
                    return Ok(Self {
                        directory,
                        items: Vec::new(),
                    });
                }
                Err(error) if error.kind() == ErrorKind::AlreadyExists => {}
                Err(error) => {
                    return Err(format!(
                        "cannot create Codex backup {}: {error}",
                        directory.display()
                    ));
                }
            }
        }
        Err(format!(
            "cannot allocate a unique Codex backup under {}",
            root.display()
        ))
    }

    fn keep(&mut self, source: &Path) -> Result<(), String> {
        let bytes = std::fs::read(source)
            .map_err(|error| format!("cannot back up {}: {error}", source.display()))?;
        let modified = std::fs::metadata(source)
            .and_then(|metadata| metadata.modified())
            .ok();
        let name = source
            .file_name()
            .map_or_else(|| "rollout".into(), |name| name.to_owned());
        let destination =
            self.directory
                .join(format!("{}-{}", self.items.len(), name.to_string_lossy()));
        write_atomic(&destination, &bytes).map_err(|error| error.to_string())?;
        self.items.push(BackupItem {
            source: source.to_path_buf(),
            bytes,
            modified,
        });
        Ok(())
    }

    fn restore(&self, changed: &[PathBuf], report: &mut MoveReport) {
        for item in &self.items {
            if !changed.contains(&item.source) {
                continue;
            }
            if let Err(error) = write_atomic(&item.source, &item.bytes) {
                report.notes.push(format!(
                    "cannot restore Codex rollout {} after a failed move: {error}",
                    item.source.display()
                ));
            } else if let Some(modified) = item.modified {
                restore_modified(&item.source, modified, report);
            }
        }
    }
}

fn rewrite_rollout(path: &Path, id: &str, from: &str, to: &str) -> Result<bool, String> {
    let modified = std::fs::metadata(path)
        .and_then(|metadata| metadata.modified())
        .ok();
    let packed = path.extension() == Some(std::ffi::OsStr::new("zst"));
    let bytes = read_rollout_bytes(path)?;
    let mut output = Vec::with_capacity(bytes.len());
    let mut found = false;
    let mut changed = false;
    for (index, line) in bytes.split_inclusive(|byte| *byte == b'\n').enumerate() {
        if !found
            && index < HEAD_LINES
            && let Ok(before) = serde_json::from_slice::<Value>(line)
            && before.get("type").and_then(Value::as_str) == Some("session_meta")
        {
            found = true;
            if before.pointer("/payload/id").and_then(Value::as_str) != Some(id) {
                return Ok(false);
            }
            let provider = before
                .pointer("/payload/model_provider")
                .and_then(Value::as_str)
                .unwrap_or(OPENAI);
            if provider != from && provider != to {
                return Ok(false);
            }
            if provider == to {
                output.extend_from_slice(line);
                continue;
            }
            let mut after = before.clone();
            after["payload"]["model_provider"] = Value::String(to.to_owned());
            let text = std::str::from_utf8(line)
                .map_err(|_| format!("Codex rollout {} is not UTF-8", path.display()))?;
            let edited = crate::jsonc_edit::reconcile(text, &before, &after)
                .ok_or_else(|| format!("cannot edit Codex rollout {}", path.display()))?;
            changed = edited.as_bytes() != line;
            output.extend_from_slice(edited.as_bytes());
            continue;
        }
        output.extend_from_slice(line);
    }
    if !found || !changed {
        return Ok(false);
    }
    let output = if packed {
        zstd::bulk::compress(&output, 3)
            .map_err(|error| format!("cannot pack Codex rollout {}: {error}", path.display()))?
    } else {
        output
    };
    write_atomic(path, &output).map_err(|error| error.to_string())?;
    if let Some(modified) = modified
        && let Ok(file) = OpenOptions::new().read(true).write(true).open(path)
    {
        let _ = file.set_modified(modified);
    }
    Ok(true)
}

fn restore_modified(path: &Path, modified: SystemTime, report: &mut MoveReport) {
    let result = OpenOptions::new()
        .read(true)
        .write(true)
        .open(path)
        .and_then(|file| file.set_modified(modified));
    if let Err(error) = result {
        report.notes.push(format!(
            "cannot restore Codex rollout time {}: {error}",
            path.display()
        ));
    }
}

fn update_state_row(row: &StateRow, from: &str, to: &str) -> Result<bool, String> {
    let connection = open_state_db(&row.db)?;
    let changed = connection
        .execute(
            "UPDATE threads SET model_provider = ? WHERE id = ? AND model_provider = ?",
            rusqlite::params![to, row.id, from],
        )
        .map_err(|error| format!("cannot update Codex state {}: {error}", row.db.display()))?;
    Ok(changed > 0)
}

fn discover_rollouts(home: &Path, report: &mut MoveReport) -> BTreeMap<String, Vec<Rollout>> {
    let mut paths = Vec::new();
    for name in ["sessions", "archived_sessions"] {
        collect_rollout_paths(&home.join(name), &mut paths, report);
    }
    paths.sort();
    let mut groups = BTreeMap::<String, Vec<Rollout>>::new();
    for path in paths {
        match read_rollout(&path) {
            Ok(Some((id, provider))) => {
                if valid_thread_id(&id)
                    && path
                        .file_name()
                        .is_some_and(|name| name.to_string_lossy().contains(&id))
                {
                    groups.entry(id).or_default().push(Rollout {
                        packed: path.extension() == Some(std::ffi::OsStr::new("zst")),
                        path,
                        provider,
                    });
                }
            }
            Ok(None) => {}
            Err(error) => report.notes.push(error),
        }
    }
    groups
}

fn collect_rollout_paths(root: &Path, paths: &mut Vec<PathBuf>, report: &mut MoveReport) {
    let entries = match std::fs::read_dir(root) {
        Ok(entries) => entries,
        Err(error) if error.kind() == ErrorKind::NotFound => return,
        Err(error) => {
            report.notes.push(format!(
                "cannot read Codex sessions {}: {error}",
                root.display()
            ));
            return;
        }
    };
    for entry in entries {
        let entry = match entry {
            Ok(entry) => entry,
            Err(error) => {
                report.notes.push(format!(
                    "cannot read an entry in Codex sessions {}: {error}",
                    root.display()
                ));
                continue;
            }
        };
        let path = entry.path();
        let file_type = match entry.file_type() {
            Ok(file_type) => file_type,
            Err(error) => {
                report.notes.push(format!(
                    "cannot inspect Codex rollout {}: {error}",
                    path.display()
                ));
                continue;
            }
        };
        if file_type.is_dir() {
            collect_rollout_paths(&path, paths, report);
            continue;
        }
        if !file_type.is_file() {
            continue;
        }
        let name = entry.file_name();
        let name = name.to_string_lossy();
        if name.starts_with("rollout-")
            && (name.ends_with(".jsonl") || name.ends_with(".jsonl.zst"))
        {
            paths.push(path);
        }
    }
}

/// Reads the thread id and provider from the first lines of a rollout.
///
/// Only the head is read (streamed through the zstd decoder for a packed
/// rollout), so a connect does not load every rollout into memory.
fn read_rollout(path: &Path) -> Result<Option<(String, String)>, String> {
    let file = File::open(path)
        .map_err(|error| format!("cannot read Codex rollout {}: {error}", path.display()))?;
    let reader: Box<dyn BufRead> = if path.extension() == Some(std::ffi::OsStr::new("zst")) {
        let decoder = zstd::stream::read::Decoder::new(file)
            .map_err(|error| format!("cannot unpack Codex rollout {}: {error}", path.display()))?;
        Box::new(BufReader::new(decoder))
    } else {
        Box::new(BufReader::new(file))
    };
    for line in reader.split(b'\n').take(HEAD_LINES) {
        let line =
            line.map_err(|error| format!("cannot read Codex rollout {}: {error}", path.display()))?;
        let Ok(value) = serde_json::from_slice::<Value>(&line) else {
            continue;
        };
        if value.get("type").and_then(Value::as_str) != Some("session_meta") {
            continue;
        }
        let Some(id) = value
            .pointer("/payload/id")
            .and_then(Value::as_str)
            .filter(|id| !id.is_empty())
        else {
            return Ok(None);
        };
        let provider = match value.pointer("/payload/model_provider") {
            Some(Value::String(provider)) if !provider.is_empty() => provider.clone(),
            None | Some(Value::Null) => OPENAI.to_owned(),
            _ => return Ok(None),
        };
        return Ok(Some((id.to_owned(), provider)));
    }
    Ok(None)
}

fn read_rollout_bytes(path: &Path) -> Result<Vec<u8>, String> {
    let bytes = std::fs::read(path)
        .map_err(|error| format!("cannot read Codex rollout {}: {error}", path.display()))?;
    if path.extension() == Some(std::ffi::OsStr::new("zst")) {
        zstd::bulk::decompress(&bytes, MAX_UNPACKED_SIZE)
            .map_err(|error| format!("cannot unpack Codex rollout {}: {error}", path.display()))
    } else {
        Ok(bytes)
    }
}

fn read_state_rows(database_home: &Path, report: &mut MoveReport) -> Vec<StateRow> {
    let entries = match std::fs::read_dir(database_home) {
        Ok(entries) => entries,
        Err(error) if error.kind() == ErrorKind::NotFound => return Vec::new(),
        Err(error) => {
            report.notes.push(format!(
                "cannot read Codex state directory {}: {error}",
                database_home.display()
            ));
            return Vec::new();
        }
    };
    let mut paths = Vec::new();
    for entry in entries {
        let Ok(entry) = entry else {
            continue;
        };
        let name = entry.file_name().to_string_lossy().into_owned();
        if name.starts_with("state_") && name.ends_with(".sqlite") && entry.path().is_file() {
            paths.push(entry.path());
        }
    }
    paths.sort();
    let mut rows = Vec::new();
    for path in paths {
        match read_state_db(&path) {
            Ok(mut found) => rows.append(&mut found),
            Err(error) => report.notes.push(error),
        }
    }
    rows
}

fn read_state_db(path: &Path) -> Result<Vec<StateRow>, String> {
    let connection = open_state_db(path)?;
    let columns = thread_columns(&connection, path)?;
    if !columns.contains("id") || !columns.contains("model_provider") {
        return Ok(Vec::new());
    }
    let has_updated_at = columns.contains("updated_at");
    let sql = if has_updated_at {
        "SELECT id, model_provider, updated_at FROM threads"
    } else {
        "SELECT id, model_provider, NULL FROM threads"
    };
    let mut statement = connection
        .prepare(sql)
        .map_err(|error| format!("cannot read Codex state {}: {error}", path.display()))?;
    let found = statement
        .query_map([], |row| {
            Ok(StateRow {
                db: path.to_path_buf(),
                id: row.get(0)?,
                provider: row.get(1)?,
                updated_at: row.get(2)?,
            })
        })
        .map_err(|error| format!("cannot read Codex state {}: {error}", path.display()))?;
    let mut rows = Vec::new();
    for row in found {
        match row {
            Ok(row) if valid_thread_id(&row.id) && valid_provider(&row.provider) => rows.push(row),
            Ok(_) => {}
            Err(error) => report_row_error(path, error)?,
        }
    }
    Ok(rows)
}

fn report_row_error(path: &Path, error: rusqlite::Error) -> Result<(), String> {
    Err(format!(
        "cannot read Codex state {}: {error}",
        path.display()
    ))
}

fn open_state_db(path: &Path) -> Result<Connection, String> {
    let connection = Connection::open_with_flags(path, OpenFlags::SQLITE_OPEN_READ_WRITE)
        .map_err(|error| format!("cannot open Codex state {}: {error}", path.display()))?;
    connection
        .busy_timeout(DB_BUSY_TIMEOUT)
        .map_err(|error| format!("cannot configure Codex state {}: {error}", path.display()))?;
    Ok(connection)
}

fn thread_columns(connection: &Connection, path: &Path) -> Result<BTreeSet<String>, String> {
    let mut statement = connection
        .prepare("PRAGMA table_info(threads)")
        .map_err(|error| format!("cannot inspect Codex state {}: {error}", path.display()))?;
    let columns = statement
        .query_map([], |row| row.get::<_, String>(1))
        .map_err(|error| format!("cannot inspect Codex state {}: {error}", path.display()))?;
    let mut names = BTreeSet::new();
    for column in columns {
        names.insert(
            column.map_err(|error| {
                format!("cannot inspect Codex state {}: {error}", path.display())
            })?,
        );
    }
    Ok(names)
}

fn parse_config(bytes: &[u8]) -> Result<CodexConfig, String> {
    if bytes.is_empty() {
        return Ok(CodexConfig {
            provider: OPENAI.to_owned(),
            selected_profile: false,
            profile_providers: BTreeSet::new(),
            providers: BTreeSet::new(),
            sqlite_home: None,
        });
    }
    let text = std::str::from_utf8(bytes).map_err(|_| "config is not UTF-8".to_owned())?;
    let document = text
        .parse::<DocumentMut>()
        .map_err(|error| format!("config is not TOML: {error}"))?;
    let top_provider = document
        .get("model_provider")
        .and_then(toml_edit::Item::as_str)
        .filter(|provider| !provider.is_empty())
        .unwrap_or(OPENAI)
        .to_owned();
    let profiles = document
        .get("profiles")
        .and_then(toml_edit::Item::as_table_like);
    let selected = document
        .get("profile")
        .and_then(toml_edit::Item::as_str)
        .filter(|profile| !profile.is_empty());
    let selected_provider = selected.and_then(|profile| {
        profiles
            .and_then(|profiles| profiles.get(profile))
            .and_then(toml_edit::Item::as_table_like)
            .and_then(|profile| profile.get("model_provider"))
            .and_then(toml_edit::Item::as_str)
            .filter(|provider| !provider.is_empty())
    });
    let mut profile_providers = BTreeSet::new();
    if let Some(profiles) = profiles {
        for (_, profile) in profiles.iter() {
            if let Some(provider) = profile
                .as_table_like()
                .and_then(|profile| profile.get("model_provider"))
                .and_then(toml_edit::Item::as_str)
                .filter(|provider| !provider.is_empty())
            {
                profile_providers.insert(provider.to_owned());
            }
        }
    }
    let providers = document
        .get("model_providers")
        .and_then(toml_edit::Item::as_table_like)
        .map(|providers| {
            providers
                .iter()
                .map(|(provider, _)| provider.to_owned())
                .collect()
        })
        .unwrap_or_default();
    let sqlite_home = document
        .get("sqlite_home")
        .and_then(toml_edit::Item::as_str)
        .filter(|path| !path.is_empty())
        .map(PathBuf::from);
    Ok(CodexConfig {
        provider: selected_provider.unwrap_or(&top_provider).to_owned(),
        selected_profile: selected_provider.is_some(),
        profile_providers,
        providers,
        sqlite_home,
    })
}

fn read_config(path: &Path) -> Result<CodexConfig, String> {
    match std::fs::read(path) {
        Ok(bytes) => parse_config(&bytes),
        Err(error) if error.kind() == ErrorKind::NotFound => parse_config(&[]),
        Err(error) => Err(format!("cannot read {}: {error}", path.display())),
    }
}

fn builtin_provider(provider: &str) -> bool {
    matches!(provider, OPENAI | "ollama" | "lmstudio")
}

fn valid_provider(provider: &str) -> bool {
    !provider.is_empty()
        && provider.len() <= 64
        && provider
            .bytes()
            .all(|byte| byte.is_ascii_alphanumeric() || matches!(byte, b'_' | b'.' | b'-'))
}

fn valid_thread_id(id: &str) -> bool {
    !id.is_empty()
        && id.len() <= 128
        && id
            .bytes()
            .all(|byte| byte.is_ascii_alphanumeric() || matches!(byte, b'_' | b'-'))
}

#[cfg(test)]
mod tests;
