//! Incremental, consent-filtered session cache.

use std::collections::{BTreeMap, BTreeSet};
use std::io::{self, Read, Seek, SeekFrom};
use std::path::{Path, PathBuf};
use std::time::UNIX_EPOCH;

use hofund_plugin_api::{
    PluginError, SessionDay, SessionFile, SessionFormat, SessionParse, SessionRecord, SessionStore,
};
use serde::{Deserialize, Serialize};
use serde_json::Value;
use thiserror::Error;

use crate::consent::Consent;
use crate::json::parse_line;
use crate::open::{open_session_file, within_root};
use crate::storage::atomic_write;

/// Maximum bytes read from one session file.
pub const MAX_FILE_BYTES: u64 = 512 * 1024 * 1024;
/// Maximum bytes retained for one JSON line.
pub const MAX_LINE_BYTES: usize = 1024 * 1024;

/// The position and truncation state after a bounded read.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct ReadOutcome {
    /// Byte immediately after the last complete, accepted or skipped line.
    pub offset: u64,
    /// A long line or the file cap prevented a complete read.
    pub cut: bool,
    /// Number of valid JSON values delivered to the parser.
    pub lines: u64,
}

/// Reads complete lines from `offset`, never retaining more than 1 MiB for a
/// line or reading beyond 512 MiB from the start of the file.
///
/// An incomplete final line is left for the next append. Invalid JSON is
/// ignored. Long lines are skipped and mark the outcome cut.
///
/// # Errors
///
/// Returns an I/O error when reading fails. The caller positions seekable
/// inputs at `offset` before calling; compressed inputs start at byte zero.
pub fn read_incremental<R: Read>(
    reader: &mut R,
    offset: u64,
    mut on_value: impl FnMut(&Value),
) -> io::Result<ReadOutcome> {
    let mut outcome = ReadOutcome {
        offset,
        ..ReadOutcome::default()
    };
    let mut position = offset;
    let mut line = Vec::new();
    let mut long = false;
    let mut buffer = vec![0_u8; 64 * 1024].into_boxed_slice();
    loop {
        if position >= MAX_FILE_BYTES {
            outcome.cut = true;
            break;
        }
        let remaining = usize::try_from(MAX_FILE_BYTES - position).unwrap_or(usize::MAX);
        let capacity = buffer.len().min(remaining);
        let read = reader.read(&mut buffer[..capacity])?;
        if read == 0 {
            break;
        }
        for &byte in &buffer[..read] {
            position += 1;
            if byte == b'\n' {
                if long {
                    outcome.cut = true;
                } else if !line.is_empty()
                    && let Ok(value) = parse_line(&line)
                {
                    outcome.lines += 1;
                    on_value(&value);
                }
                outcome.offset = position;
                line.clear();
                long = false;
                continue;
            }
            if !long {
                if line.len() < MAX_LINE_BYTES {
                    line.push(byte);
                } else {
                    long = true;
                    line.clear();
                }
            }
        }
    }
    Ok(outcome)
}

/// A cache load, parse, or save failure.
#[derive(Debug, Error)]
pub enum IndexError {
    /// The index or a session file could not be read or written.
    #[error("session index I/O failed: {0}")]
    Io(#[from] io::Error),
    /// A plugin parser failed.
    #[error("session parser failed: {0}")]
    Plugin(#[from] PluginError),
    /// The in-memory cache could not be serialized.
    #[error("session index could not be serialized")]
    Serialize,
}

type EntryKey = (PathBuf, PathBuf, String);

#[derive(Clone, Debug, Deserialize, PartialEq, Eq, Serialize)]
struct FileStamp {
    path: PathBuf,
    size: u64,
    modified_ns: i64,
}

#[derive(Clone, Debug, Deserialize, Serialize)]
struct CacheEntry {
    root: PathBuf,
    path: PathBuf,
    file_id: String,
    size: u64,
    modified_ns: i64,
    offset: u64,
    state: String,
    #[serde(default)]
    related: Vec<FileStamp>,
    record: SessionRecord,
}

#[derive(Debug, Default, Deserialize, Serialize)]
struct DiskIndex {
    #[serde(default)]
    entries: Vec<CacheEntry>,
}

/// The process-local session index backed by `<config>/state/sessions-index.json`.
#[derive(Debug)]
pub struct SessionIndex {
    path: PathBuf,
    entries: BTreeMap<EntryKey, CacheEntry>,
    dirty: bool,
}

impl SessionIndex {
    /// Loads the cache, dropping rows without consent or with escaped paths.
    /// A missing or malformed cache starts empty.
    #[must_use]
    pub fn load(config_dir: &Path, consent: &Consent) -> Self {
        let path = config_dir.join("state").join("sessions-index.json");
        let disk = std::fs::read(&path)
            .ok()
            .and_then(|bytes| serde_json::from_slice::<DiskIndex>(&bytes).ok())
            .unwrap_or_default();
        let mut entries = BTreeMap::new();
        let mut dropped = false;
        for entry in disk.entries {
            if consent.allows(&entry.record.agent)
                && entry.path == entry.record.path
                && within_root(&entry.root, &entry.path)
                && entry
                    .related
                    .iter()
                    .all(|related| within_root(&entry.root, &related.path))
            {
                entries.insert(
                    (
                        entry.root.clone(),
                        entry.path.clone(),
                        entry.file_id.clone(),
                    ),
                    entry,
                );
            } else {
                dropped = true;
            }
        }
        Self {
            path,
            entries,
            dirty: dropped,
        }
    }

    /// Refreshes all consented stores and atomically saves a changed index.
    ///
    /// # Errors
    ///
    /// Returns [`IndexError`] on a file, parser, or cache write failure.
    pub fn refresh(
        &mut self,
        consent: &Consent,
        stores: &[&dyn SessionStore],
    ) -> Result<(), IndexError> {
        let before_allowed: BTreeSet<String> = self
            .entries
            .values()
            .map(|entry| entry.record.agent.clone())
            .collect();
        for agent in before_allowed {
            if !consent.allows(&agent) {
                self.entries.retain(|_, entry| entry.record.agent != agent);
                self.dirty = true;
            }
        }

        let mut seen = BTreeSet::new();
        for store in stores {
            let agent = store.agent();
            if !consent.allows(agent) {
                self.entries.retain(|_, entry| entry.record.agent != agent);
                continue;
            }
            for root in store.locator().roots() {
                for file in store.files(&root)? {
                    if file.format == SessionFormat::Sqlite {
                        self.refresh_database(*store, &root, &file, &mut seen)?;
                    } else {
                        self.refresh_file(*store, &root, &file, &mut seen)?;
                    }
                }
            }
        }
        let before = self.entries.len();
        self.entries.retain(|key, _| seen.contains(key));
        self.dirty |= before != self.entries.len();
        self.save_if_dirty()
    }

    /// Newest-first main records. Consent is checked again before rows return.
    #[must_use]
    pub fn list(&mut self, consent: &Consent) -> Vec<SessionRecord> {
        self.drop_disallowed(consent);
        let mut records: Vec<SessionRecord> = self
            .entries
            .values()
            .filter(|entry| entry.record.main)
            .map(|entry| entry.record.clone())
            .collect();
        dedup_forks(&mut records);
        sort_newest(&mut records);
        let mut children: Vec<SessionRecord> = self
            .entries
            .values()
            .filter(|entry| !entry.record.main)
            .map(|entry| entry.record.clone())
            .collect();
        children.extend(hide_pure_copies(&mut records));
        records.retain(|record| record.main);
        for child in children {
            let Some(parent_id) = child.parent_id.as_deref() else {
                continue;
            };
            if let Some(parent) = records
                .iter_mut()
                .find(|parent| parent.agent == child.agent && parent.id == parent_id)
            {
                if child.copy {
                    parent.copies.push(child.id.clone());
                } else {
                    for (model, tokens) in &child.models {
                        parent.add_usage(model.clone(), *tokens);
                    }
                    for day in child.days.clone() {
                        parent.add_day(day);
                    }
                    parent.subagents.push(child.id.clone());
                }
                parent.start_ms = match (parent.start_ms, child.start_ms) {
                    (0, child) => child,
                    (parent, 0) => parent,
                    (parent, child) => parent.min(child),
                };
                parent.last_ms = parent.last_ms.max(child.last_ms);
                parent.cut |= child.cut;
            }
        }
        sort_newest(&mut records);
        records
    }

    /// Returns one listed session by its public id, with consent checked again.
    #[must_use]
    pub fn get(&mut self, consent: &Consent, id: &str) -> Option<SessionRecord> {
        self.list(consent).into_iter().find(|row| row.id == id)
    }

    /// Saves the cache when a refresh or consent filter changed it.
    ///
    /// # Errors
    ///
    /// Returns [`IndexError`] when serialization or the atomic write fails.
    pub fn save_if_dirty(&mut self) -> Result<(), IndexError> {
        if !self.dirty {
            return Ok(());
        }
        let disk = DiskIndex {
            entries: self.entries.values().cloned().collect(),
        };
        let mut bytes = serde_json::to_vec(&disk).map_err(|_| IndexError::Serialize)?;
        bytes.push(b'\n');
        if let Some(state) = self.path.parent() {
            std::fs::create_dir_all(state)?;
            #[cfg(unix)]
            {
                use std::os::unix::fs::PermissionsExt;
                std::fs::set_permissions(state, std::fs::Permissions::from_mode(0o700))?;
            }
        }
        atomic_write(&self.path, &bytes)?;
        self.dirty = false;
        Ok(())
    }

    fn drop_disallowed(&mut self, consent: &Consent) {
        let before = self.entries.len();
        self.entries
            .retain(|_, entry| consent.allows(&entry.record.agent));
        self.dirty |= before != self.entries.len();
    }

    fn refresh_database(
        &mut self,
        store: &dyn SessionStore,
        root: &Path,
        file: &SessionFile,
        seen: &mut BTreeSet<EntryKey>,
    ) -> Result<(), IndexError> {
        if !within_root(root, &file.path) {
            return Ok(());
        }
        let stamp = match file_stamp(root, &file.path) {
            Ok(stamp) => stamp,
            Err(error) if error.kind() == io::ErrorKind::NotFound => return Ok(()),
            Err(error) => return Err(error.into()),
        };
        let old: Vec<_> = self
            .entries
            .iter()
            .filter(|(_, entry)| {
                entry.root == root && entry.path == file.path && entry.record.agent == store.agent()
            })
            .map(|(key, entry)| (key.clone(), entry.clone()))
            .collect();
        if !old.is_empty()
            && old.iter().all(|(_, entry)| {
                entry.size == stamp.size && entry.modified_ns == stamp.modified_ns
            })
        {
            seen.extend(old.into_iter().map(|(key, _)| key));
            return Ok(());
        }
        self.entries.retain(|_, entry| {
            entry.root != root || entry.path != file.path || entry.record.agent != store.agent()
        });
        let database = crate::sqlite::open_ro(root, &file.path)?;
        let records = store.read_database(file, &database)?;
        for mut record in records {
            store.agent().clone_into(&mut record.agent);
            record.path.clone_from(&file.path);
            let key = (root.to_path_buf(), file.path.clone(), record.id.clone());
            seen.insert(key.clone());
            self.entries.insert(
                key,
                CacheEntry {
                    root: root.to_path_buf(),
                    path: file.path.clone(),
                    file_id: record.id.clone(),
                    size: stamp.size,
                    modified_ns: stamp.modified_ns,
                    offset: 0,
                    state: String::new(),
                    related: Vec::new(),
                    record,
                },
            );
        }
        self.dirty = true;
        Ok(())
    }

    fn refresh_file(
        &mut self,
        store: &dyn SessionStore,
        root: &Path,
        file: &SessionFile,
        seen: &mut BTreeSet<EntryKey>,
    ) -> Result<(), IndexError> {
        let key = (root.to_path_buf(), file.path.clone(), file.id.clone());
        if !within_root(root, &file.path) {
            self.entries.remove(&key);
            self.dirty = true;
            return Ok(());
        }
        let stamp = match file_stamp(root, &file.path) {
            Ok(stamp) => stamp,
            Err(error) if error.kind() == io::ErrorKind::NotFound => return Ok(()),
            Err(error) => return Err(error.into()),
        };
        let related = file
            .related
            .iter()
            .map(|path| file_stamp(root, path))
            .collect::<Result<Vec<_>, _>>()?;
        seen.insert(key.clone());
        let old = self.entries.get(&key).cloned();
        if let Some(old) = &old
            && old.size == stamp.size
            && old.modified_ns == stamp.modified_ns
            && old.file_id == file.id
            && old.related == related
        {
            return Ok(());
        }
        let append = old.as_ref().is_some_and(|old| {
            old.file_id == file.id
                && file.format == SessionFormat::JsonLines
                && file.related.is_empty()
                && file.compression == hofund_plugin_api::SessionCompression::None
                && stamp.size > old.size
                && stamp.modified_ns >= old.modified_ns
                && old.offset <= stamp.size.min(MAX_FILE_BYTES)
        });
        let (offset, previous) = if append {
            let old = old.as_ref().expect("append has old state");
            (old.offset, Some(old.state.as_str()))
        } else {
            (0, None)
        };
        let (parsed, outcome) = parse_file(store, root, file, offset, previous)?;
        let SessionParse { record, state, .. } = parsed;
        let Some(mut record) = record.or_else(|| {
            if append {
                old.as_ref().map(|old| old.record.clone())
            } else {
                None
            }
        }) else {
            self.entries.remove(&key);
            self.dirty = true;
            return Ok(());
        };
        store.agent().clone_into(&mut record.agent);
        record.path.clone_from(&file.path);
        record.cut |= outcome.cut;
        self.entries.insert(
            key,
            CacheEntry {
                root: root.to_path_buf(),
                path: file.path.clone(),
                file_id: file.id.clone(),
                size: stamp.size,
                modified_ns: stamp.modified_ns,
                offset: outcome.offset,
                state,
                related,
                record,
            },
        );
        self.dirty = true;
        Ok(())
    }
}

fn sort_newest(records: &mut [SessionRecord]) {
    records.sort_by(|left, right| {
        right
            .last_ms
            .cmp(&left.last_ms)
            .then_with(|| left.id.cmp(&right.id))
    });
}

fn dedup_forks(records: &mut [SessionRecord]) {
    let parents = records.to_vec();
    for record in records {
        let Some(parent_id) = record.forked_from.as_deref() else {
            continue;
        };
        let Some(parent) = parents
            .iter()
            .find(|parent| parent.agent == record.agent && parent.id == parent_id)
        else {
            continue;
        };
        let events = record.usage.clone();
        record.models.clear();
        record.days.clear();
        for event in events {
            if parent
                .usage
                .iter()
                .any(|held| held.checkpoint == event.checkpoint)
            {
                continue;
            }
            record.add_usage(event.model.clone(), event.tokens);
            if !event.date.is_empty() {
                record.add_day(SessionDay::new(event.date, event.model, event.tokens));
            }
        }
    }
}

fn hide_pure_copies(records: &mut [SessionRecord]) -> Vec<SessionRecord> {
    let source = records.to_vec();
    let mut links = BTreeMap::new();
    for (index, child) in source.iter().enumerate() {
        if child.family_root.is_empty() || child.family_leaf.is_empty() {
            continue;
        }
        for (candidate, keeper) in source.iter().enumerate() {
            if index == candidate || child.agent != keeper.agent || !holds_family(keeper, child) {
                continue;
            }
            if holds_family(child, keeper) && !preferred_keeper(keeper, child) {
                continue;
            }
            links.insert(index, candidate);
            break;
        }
    }
    let mut copies = Vec::new();
    for index in links.keys() {
        let mut keeper = links[index];
        let mut seen = BTreeSet::new();
        while let Some(next) = links.get(&keeper) {
            if !seen.insert(keeper) {
                break;
            }
            keeper = *next;
        }
        let mut copy = source[*index].clone();
        copy.main = false;
        copy.copy = true;
        copy.parent_id = Some(source[keeper].id.clone());
        records[*index].main = false;
        records[*index].copy = true;
        records[*index].parent_id.clone_from(&copy.parent_id);
        copies.push(copy);
    }
    copies
}

fn holds_family(keeper: &SessionRecord, other: &SessionRecord) -> bool {
    [&other.family_root, &other.family_leaf]
        .into_iter()
        .all(|id| {
            keeper.family_ids.contains(id) || keeper.family_root == *id || keeper.family_leaf == *id
        })
}

fn preferred_keeper(left: &SessionRecord, right: &SessionRecord) -> bool {
    left.last_ms > right.last_ms || (left.last_ms == right.last_ms && left.id > right.id)
}

fn file_stamp(root: &Path, path: &Path) -> io::Result<FileStamp> {
    if !within_root(root, path) {
        return Err(io::Error::new(
            io::ErrorKind::InvalidData,
            "session path escapes its root",
        ));
    }
    let metadata = std::fs::metadata(path)?;
    if !metadata.is_file() {
        return Err(io::Error::new(
            io::ErrorKind::InvalidData,
            "session path is not a regular file",
        ));
    }
    let modified_ns = metadata
        .modified()
        .ok()
        .and_then(|modified| modified.duration_since(UNIX_EPOCH).ok())
        .and_then(|duration| i64::try_from(duration.as_nanos()).ok())
        .unwrap_or(0);
    Ok(FileStamp {
        path: path.to_path_buf(),
        size: metadata.len(),
        modified_ns,
    })
}

fn read_document<R: Read>(
    reader: &mut R,
    mut on_value: impl FnMut(&Value),
) -> io::Result<ReadOutcome> {
    let mut bytes = Vec::new();
    let mut limited = reader.take((MAX_LINE_BYTES + 1) as u64);
    limited.read_to_end(&mut bytes)?;
    if bytes.len() > MAX_LINE_BYTES {
        return Ok(ReadOutcome {
            offset: MAX_LINE_BYTES as u64,
            cut: true,
            lines: 0,
        });
    }
    let mut outcome = ReadOutcome {
        offset: bytes.len() as u64,
        ..ReadOutcome::default()
    };
    if let Ok(value) = parse_line(&bytes) {
        outcome.lines = 1;
        on_value(&value);
    }
    Ok(outcome)
}

fn parse_file(
    store: &dyn SessionStore,
    root: &Path,
    file: &SessionFile,
    offset: u64,
    previous: Option<&str>,
) -> Result<(SessionParse, ReadOutcome), IndexError> {
    let mut parser = store.parser(file, previous)?;
    let mut parser_error = None;
    let mut on_value = |value: &Value| {
        if parser_error.is_none()
            && let Err(error) = parser.push(value)
        {
            parser_error = Some(error);
        }
    };
    let mut outcome = ReadOutcome::default();
    match file.format {
        SessionFormat::JsonLines => {
            let mut opened = open_session_file(root, &file.path)
                .map_err(|error| io::Error::other(error.to_string()))?;
            outcome = match file.compression {
                hofund_plugin_api::SessionCompression::None => {
                    opened.seek(SeekFrom::Start(offset))?;
                    read_incremental(&mut opened, offset, &mut on_value)?
                }
                hofund_plugin_api::SessionCompression::Zstd => {
                    let mut decoder = zstd::stream::read::Decoder::new(opened)?;
                    read_incremental(&mut decoder, 0, &mut on_value)?
                }
                _ => {
                    return Err(io::Error::new(
                        io::ErrorKind::InvalidData,
                        "unsupported session compression",
                    )
                    .into());
                }
            };
        }
        SessionFormat::Json => {
            for path in std::iter::once(&file.path).chain(file.related.iter()) {
                let mut opened = open_session_file(root, path)
                    .map_err(|error| io::Error::other(error.to_string()))?;
                let next = read_document(&mut opened, &mut on_value)?;
                outcome.offset = next.offset;
                outcome.lines += next.lines;
                outcome.cut |= next.cut;
            }
        }
        SessionFormat::Sqlite => {
            return Err(io::Error::new(
                io::ErrorKind::InvalidData,
                "SQLite source reached the JSON parser",
            )
            .into());
        }
        _ => {
            return Err(
                io::Error::new(io::ErrorKind::InvalidData, "unsupported session format").into(),
            );
        }
    }
    if let Some(error) = parser_error {
        return Err(error.into());
    }
    Ok((parser.finish()?, outcome))
}

#[cfg(test)]
mod tests {
    use std::fs::File;
    use std::io::Write;
    use std::sync::Arc;
    use std::sync::atomic::{AtomicUsize, Ordering};

    use hofund_plugin_api::{SessionLocator, SessionParser, SessionTokens, SessionUsageEvent};

    use super::*;

    #[test]
    fn index_stops_at_byte_cap() {
        let temp = tempfile::tempdir().expect("temp dir");
        let path = temp.path().join("large.jsonl");
        let mut file = File::create(&path).expect("create");
        file.write_all(b"{}\n").expect("first line");
        file.set_len(MAX_FILE_BYTES + 1).expect("sparse size");
        drop(file);
        let mut file = File::open(&path).expect("open");
        let mut lines = 0;
        let outcome = read_incremental(&mut file, 0, |_| lines += 1).expect("read");
        assert_eq!(lines, 1);
        assert_eq!(outcome.offset, 3);
        assert!(outcome.cut);
    }

    #[test]
    fn index_skips_long_line() {
        let temp = tempfile::tempdir().expect("temp dir");
        let path = temp.path().join("long.jsonl");
        let mut bytes = b"{}\n".to_vec();
        bytes.extend(std::iter::repeat_n(b' ', MAX_LINE_BYTES + 1));
        bytes.extend(b"\n{\"ok\":true}\n");
        std::fs::write(&path, bytes).expect("fixture");
        let mut file = File::open(&path).expect("open");
        let mut values = Vec::new();
        let outcome =
            read_incremental(&mut file, 0, |value| values.push(value.clone())).expect("read");
        assert_eq!(values.len(), 2);
        assert!(outcome.cut);
        assert_eq!(outcome.offset, std::fs::metadata(path).expect("size").len());
    }

    #[test]
    fn zstd_stream_uses_the_same_json_bounds() {
        let compressed =
            zstd::bulk::compress(b"{}\n{\"packed\":true}\n".as_slice(), 3).expect("compress");
        let mut decoder = zstd::stream::read::Decoder::new(compressed.as_slice()).expect("decoder");
        let mut values = Vec::new();
        let outcome =
            read_incremental(&mut decoder, 0, |value| values.push(value.clone())).expect("read");
        assert_eq!(values.len(), 2);
        assert_eq!(
            outcome.offset,
            u64::try_from(b"{}\n{\"packed\":true}\n".len()).expect("fixture length")
        );
    }

    #[test]
    fn incomplete_line_waits_for_append() {
        let temp = tempfile::tempdir().expect("temp dir");
        let path = temp.path().join("append.jsonl");
        std::fs::write(&path, b"{}\n{\"later\":").expect("fixture");
        let mut file = File::open(&path).expect("open");
        let outcome = read_incremental(&mut file, 0, |_| {}).expect("read");
        assert_eq!(outcome.offset, 3);
        assert!(!outcome.cut);
    }

    #[test]
    fn index_drops_escaped_path() {
        let temp = tempfile::tempdir().expect("temp dir");
        let root = temp.path().join("root");
        std::fs::create_dir(&root).expect("root");
        let outside = temp.path().join("outside.jsonl");
        std::fs::write(&outside, b"{}\n").expect("outside");
        let record = SessionRecord::new("fake", "fake:one", &outside);
        let entry = CacheEntry {
            root: root.clone(),
            path: outside.clone(),
            file_id: "one".to_owned(),
            size: 3,
            modified_ns: 1,
            offset: 3,
            state: String::new(),
            related: Vec::new(),
            record,
        };
        let state = temp.path().join("state");
        std::fs::create_dir(&state).expect("state");
        std::fs::write(
            state.join("sessions-index.json"),
            serde_json::to_vec(&DiskIndex {
                entries: vec![entry],
            })
            .expect("encode"),
        )
        .expect("cache");
        let mut consent = Consent::new(temp.path());
        consent.settle();
        consent.set_reads(Some(true), []).expect("consent");
        let mut index = SessionIndex::load(temp.path(), &consent);
        assert_eq!(index.list(&consent), Vec::<SessionRecord>::new());
        index.save_if_dirty().expect("save");
        let saved = std::fs::read_to_string(state.join("sessions-index.json")).expect("saved");
        assert!(!saved.contains("outside.jsonl"));
    }

    struct FakeLocator {
        root: PathBuf,
    }

    impl SessionLocator for FakeLocator {
        fn roots(&self) -> Vec<PathBuf> {
            vec![self.root.clone()]
        }

        fn present(&self) -> bool {
            self.root.is_dir()
        }
    }

    struct FakeStore {
        locator: FakeLocator,
        parses: Arc<AtomicUsize>,
    }

    impl FakeStore {
        fn new(root: &Path) -> (Self, Arc<AtomicUsize>) {
            let parses = Arc::new(AtomicUsize::new(0));
            (
                Self {
                    locator: FakeLocator {
                        root: root.to_path_buf(),
                    },
                    parses: Arc::clone(&parses),
                },
                parses,
            )
        }
    }

    struct FakeParser {
        record: SessionRecord,
    }

    impl SessionParser for FakeParser {
        fn push(&mut self, value: &Value) -> Result<(), PluginError> {
            if value.get("prompt").and_then(Value::as_str).is_some() {
                self.record.title = "fake prompt".to_owned();
                self.record.last_ms = 10;
                self.record
                    .add_usage("model", SessionTokens::new(1, 0, 0, 0));
            }
            Ok(())
        }

        fn finish(self: Box<Self>) -> Result<SessionParse, PluginError> {
            Ok(SessionParse::new(self.record, "{}"))
        }
    }

    impl SessionStore for FakeStore {
        fn agent(&self) -> &'static str {
            "fake"
        }

        fn locator(&self) -> &dyn SessionLocator {
            &self.locator
        }

        fn files(&self, root: &Path) -> Result<Vec<SessionFile>, PluginError> {
            Ok(vec![SessionFile::new("one", root.join("one.jsonl"))])
        }

        fn parser(
            &self,
            file: &SessionFile,
            _state: Option<&str>,
        ) -> Result<Box<dyn SessionParser>, PluginError> {
            self.parses.fetch_add(1, Ordering::SeqCst);
            Ok(Box::new(FakeParser {
                record: SessionRecord::new("fake", "fake:one", &file.path),
            }))
        }
    }

    #[test]
    fn unchanged_file_reuses_cache_and_consent_drops_rows() {
        let temp = tempfile::tempdir().expect("temp dir");
        let agent_root = temp.path().join("agent");
        std::fs::create_dir(&agent_root).expect("agent");
        std::fs::write(agent_root.join("one.jsonl"), b"{\"prompt\":\"hello\"}\n").expect("session");
        let config = temp.path().join("config");
        let mut consent = Consent::new(&config);
        consent.settle();
        consent.set_reads(Some(true), []).expect("consent");
        let (store, parses) = FakeStore::new(&agent_root);
        let stores: [&dyn SessionStore; 1] = [&store];
        let mut index = SessionIndex::load(&config, &consent);
        index.refresh(&consent, &stores).expect("first refresh");
        assert_eq!(parses.load(Ordering::SeqCst), 1);
        assert_eq!(index.list(&consent)[0].title, "fake prompt");
        index.refresh(&consent, &stores).expect("second refresh");
        assert_eq!(parses.load(Ordering::SeqCst), 1);
        std::fs::write(agent_root.join("one.jsonl"), b"{\"prompt\":\"howdy\"}\n")
            .expect("same-size rewrite");
        let changed = std::fs::OpenOptions::new()
            .write(true)
            .open(agent_root.join("one.jsonl"))
            .expect("open rewrite");
        changed
            .set_times(
                std::fs::FileTimes::new()
                    .set_modified(std::time::SystemTime::now() + std::time::Duration::from_secs(1)),
            )
            .expect("mtime");
        index.refresh(&consent, &stores).expect("rewrite refresh");
        assert_eq!(parses.load(Ordering::SeqCst), 2);
        consent
            .set_reads(None, [("fake".to_owned(), false)])
            .expect("disable");
        assert_eq!(index.list(&consent), Vec::<SessionRecord>::new());
        index.save_if_dirty().expect("drop save");
        assert_eq!(
            SessionIndex::load(&config, &consent).list(&consent),
            Vec::<SessionRecord>::new()
        );
    }

    #[test]
    fn pure_copy_is_not_a_top_level_row() {
        let mut keeper = SessionRecord::new("fake", "fake:keeper", "keeper");
        keeper.last_ms = 20;
        keeper.family_root = "root".to_owned();
        keeper.family_leaf = "leaf".to_owned();
        keeper.family_ids = vec!["root".to_owned(), "leaf".to_owned()];
        let mut copy = SessionRecord::new("fake", "fake:copy", "copy");
        copy.last_ms = 10;
        copy.family_root = "root".to_owned();
        copy.family_leaf = "leaf".to_owned();
        copy.family_ids.clone_from(&keeper.family_ids);
        let mut records = vec![copy, keeper];
        let copies = hide_pure_copies(&mut records);
        assert_eq!(copies.len(), 1);
        assert_eq!(copies[0].parent_id.as_deref(), Some("fake:keeper"));
        assert!(!records[0].main);
        assert!(records[1].main);
    }

    #[test]
    fn fork_counts_only_checkpoints_missing_from_parent() {
        let copied = SessionUsageEvent::new(
            SessionTokens::new(10, 1, 0, 0),
            SessionTokens::new(10, 1, 0, 0),
            "model",
            "2026-11-10",
        );
        let later = SessionUsageEvent::new(
            SessionTokens::new(20, 2, 0, 0),
            SessionTokens::new(10, 1, 0, 0),
            "model",
            "2026-11-11",
        );
        let mut parent = SessionRecord::new("fake", "fake:parent", "parent");
        parent.usage.push(copied.clone());
        let mut child = SessionRecord::new("fake", "fake:child", "child");
        child.forked_from = Some(parent.id.clone());
        child.usage = vec![copied, later];
        for event in &child.usage.clone() {
            child.add_usage(event.model.clone(), event.tokens);
            child.add_day(SessionDay::new(
                event.date.clone(),
                event.model.clone(),
                event.tokens,
            ));
        }
        let mut records = vec![parent, child];
        dedup_forks(&mut records);
        assert_eq!(records[1].tokens(), SessionTokens::new(10, 1, 0, 0));
        assert_eq!(records[1].days.len(), 1);
        assert_eq!(records[1].days[0].date, "2026-11-11");
    }
}
