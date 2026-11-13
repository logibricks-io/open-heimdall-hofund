//! Read-only session discovery and parsing contracts.
//!
//! The kernel opens and bounds files. A plugin locates files and parses JSON
//! values; it never decides whether reading is consented.

use std::collections::BTreeMap;
use std::path::{Path, PathBuf};

use serde::{Deserialize, Serialize};

use crate::PluginError;

/// Token counters attached to one model or one session.
#[derive(Clone, Copy, Debug, Default, Deserialize, PartialEq, Eq, Serialize)]
#[non_exhaustive]
pub struct SessionTokens {
    /// Uncached and cache-miss input tokens.
    pub input: u64,
    /// Output tokens, including reasoning.
    pub output: u64,
    /// Cached input tokens read.
    pub cache_read: u64,
    /// Cached tokens written.
    pub cache_write: u64,
    /// The one-hour subset of `cache_write`.
    #[serde(default)]
    pub cache_write_1h: u64,
}

impl SessionTokens {
    /// A counter set.
    #[must_use]
    pub const fn new(input: u64, output: u64, cache_read: u64, cache_write: u64) -> Self {
        Self {
            input,
            output,
            cache_read,
            cache_write,
            cache_write_1h: 0,
        }
    }

    /// Marks the one-hour subset of cache writes.
    #[must_use]
    pub fn with_cache_write_1h(mut self, tokens: u64) -> Self {
        self.cache_write_1h = tokens.min(self.cache_write);
        self
    }

    /// Adds another counter set, saturating rather than wrapping hostile input.
    pub fn add(&mut self, other: Self) {
        self.input = self.input.saturating_add(other.input);
        self.output = self.output.saturating_add(other.output);
        self.cache_read = self.cache_read.saturating_add(other.cache_read);
        self.cache_write = self.cache_write.saturating_add(other.cache_write);
        self.cache_write_1h = self.cache_write_1h.saturating_add(other.cache_write_1h);
    }

    /// Whether every counter is zero.
    #[must_use]
    pub fn is_zero(self) -> bool {
        self == Self::default()
    }

    /// The sum of all counters.
    #[must_use]
    pub fn total(self) -> u64 {
        self.input
            .saturating_add(self.output)
            .saturating_add(self.cache_read)
            .saturating_add(self.cache_write)
    }
}

/// One local date and model's share of a session.
#[derive(Clone, Debug, Deserialize, PartialEq, Eq, Serialize)]
#[non_exhaustive]
pub struct SessionDay {
    /// Local calendar date in `YYYY-MM-DD` form.
    pub date: String,
    /// Model id.
    pub model: String,
    /// Tokens used on that date and model.
    pub tokens: SessionTokens,
}

impl SessionDay {
    /// A daily model counter.
    #[must_use]
    pub fn new(date: impl Into<String>, model: impl Into<String>, tokens: SessionTokens) -> Self {
        Self {
            date: date.into(),
            model: model.into(),
            tokens,
        }
    }
}

/// One usage observation with the running checkpoint that produced it.
#[derive(Clone, Debug, Deserialize, PartialEq, Eq, Serialize)]
#[non_exhaustive]
pub struct SessionUsageEvent {
    /// Running total reported by the agent, before normalization.
    pub checkpoint: SessionTokens,
    /// Tokens contributed by this observation.
    pub tokens: SessionTokens,
    /// Model id for this contribution.
    pub model: String,
    /// Local calendar date for this contribution.
    pub date: String,
}

impl SessionUsageEvent {
    /// A usage observation.
    #[must_use]
    pub fn new(
        checkpoint: SessionTokens,
        tokens: SessionTokens,
        model: impl Into<String>,
        date: impl Into<String>,
    ) -> Self {
        Self {
            checkpoint,
            tokens,
            model: model.into(),
            date: date.into(),
        }
    }
}

#[allow(clippy::trivially_copy_pass_by_ref)] // serde skip_serializing_if takes &T
fn is_false(value: &bool) -> bool {
    !*value
}

/// One parsed session file. Non-main records merge into their parent.
#[derive(Clone, Debug, Deserialize, PartialEq, Serialize)]
#[non_exhaustive]
#[allow(clippy::struct_excessive_bools)] // independent session flags, not a state machine
pub struct SessionRecord {
    /// Agent plugin id.
    pub agent: String,
    /// Public session id, normally prefixed with the agent id.
    pub id: String,
    /// Lexical path supplied by the plugin.
    pub path: PathBuf,
    /// Working folder recorded by the agent.
    pub cwd: String,
    /// Custom title, agent title, first prompt, or id.
    pub title: String,
    /// First message time in Unix milliseconds.
    pub start_ms: i64,
    /// Last message time in Unix milliseconds.
    pub last_ms: i64,
    /// Tokens by model.
    pub models: BTreeMap<String, SessionTokens>,
    /// Daily tokens by local date and model.
    pub days: Vec<SessionDay>,
    /// USD cost filled by the kernel's pricing pass.
    pub cost: f64,
    /// Number of token-bearing models without a price.
    pub unpriced: u64,
    /// Whether this is a top-level row rather than a subagent or copy.
    pub main: bool,
    /// Parent session id for a non-main record.
    pub parent_id: Option<String>,
    /// Grouped subagent ids on a main record.
    pub subagents: Vec<String>,
    /// Pure-copy ids on a main record.
    pub copies: Vec<String>,
    /// First and last shared message ids, when the format supports copies.
    #[serde(default)]
    pub family_root: String,
    /// Last shared message id.
    #[serde(default)]
    pub family_leaf: String,
    /// All shared message ids known in this file.
    #[serde(default)]
    pub family_ids: Vec<String>,
    /// Whether this record is a pure copy rather than a subagent.
    #[serde(default)]
    pub copy: bool,
    /// Attribution agent for desktop variants or sessions run outside this agent's files.
    #[serde(default, skip_serializing_if = "String::is_empty")]
    pub app: String,
    /// Parent checkpoint source for a fork, when one is named.
    #[serde(default)]
    pub forked_from: Option<String>,
    /// Whether `models` and `days` already exclude copied pre-fork usage.
    ///
    /// When true, the kernel must not rebuild them from checkpoint comparison.
    /// False is omitted so existing serialized goldens and cached records keep
    /// their previous shape.
    #[serde(default, skip_serializing_if = "is_false")]
    pub fork_usage_filtered: bool,
    /// Usage observations used for checkpoint dedup.
    #[serde(default)]
    pub usage: Vec<SessionUsageEvent>,
    /// Whether input bounds or the agent format omitted some content.
    pub cut: bool,
    /// Whether this row is read-only in the session list.
    #[serde(default, skip_serializing_if = "std::ops::Not::not")]
    pub read_only: bool,
    /// Whether this row is projected from the gateway ledger.
    #[serde(default, skip_serializing_if = "std::ops::Not::not")]
    pub gateway: bool,
}

impl SessionRecord {
    /// An empty main record with `id` as its initial title.
    #[must_use]
    pub fn new(agent: impl Into<String>, id: impl Into<String>, path: impl Into<PathBuf>) -> Self {
        let id = id.into();
        Self {
            agent: agent.into(),
            title: id.clone(),
            id,
            path: path.into(),
            cwd: String::new(),
            start_ms: 0,
            last_ms: 0,
            models: BTreeMap::new(),
            days: Vec::new(),
            cost: 0.0,
            unpriced: 0,
            main: true,
            parent_id: None,
            subagents: Vec::new(),
            copies: Vec::new(),
            family_root: String::new(),
            family_leaf: String::new(),
            family_ids: Vec::new(),
            copy: false,
            app: String::new(),
            forked_from: None,
            fork_usage_filtered: false,
            usage: Vec::new(),
            cut: false,
            read_only: false,
            gateway: false,
        }
    }

    /// Marks this record as a child of `parent_id`.
    #[must_use]
    pub fn with_parent(mut self, parent_id: impl Into<String>) -> Self {
        self.main = false;
        self.parent_id = Some(parent_id.into());
        self
    }

    /// Adds model tokens to the session total.
    pub fn add_usage(&mut self, model: impl Into<String>, tokens: SessionTokens) {
        if !tokens.is_zero() {
            self.models.entry(model.into()).or_default().add(tokens);
        }
    }

    /// Adds one daily model counter.
    pub fn add_day(&mut self, day: SessionDay) {
        if let Some(existing) = self
            .days
            .iter_mut()
            .find(|existing| existing.date == day.date && existing.model == day.model)
        {
            existing.tokens.add(day.tokens);
        } else if !day.tokens.is_zero() {
            self.days.push(day);
        }
    }

    /// Total tokens across all models.
    #[must_use]
    pub fn tokens(&self) -> SessionTokens {
        self.models
            .values()
            .copied()
            .fold(SessionTokens::default(), |mut total, tokens| {
                total.add(tokens);
                total
            })
    }

    /// Model ids with nonzero tokens.
    #[must_use]
    pub fn model_ids(&self) -> Vec<String> {
        self.models.keys().cloned().collect()
    }
}

/// One transcript content part.
#[derive(Clone, Debug, Default, Deserialize, PartialEq, Eq, Serialize)]
#[non_exhaustive]
pub struct SessionPart {
    /// Speaker: user, assistant, or tool.
    pub role: String,
    /// Content kind: text, thinking, `tool_use`, `tool_result`, context, or image.
    pub kind: String,
    /// Tool name when the part is a call or result.
    pub name: String,
    /// Text content.
    pub text: String,
}

impl SessionPart {
    /// Constructs one content part.
    #[must_use]
    pub fn new(
        role: impl Into<String>,
        kind: impl Into<String>,
        name: impl Into<String>,
        text: impl Into<String>,
    ) -> Self {
        Self {
            role: role.into(),
            kind: kind.into(),
            name: name.into(),
            text: text.into(),
        }
    }
}

/// A session transcript read from its own file.
#[derive(Clone, Debug, Default, Deserialize, PartialEq, Eq, Serialize)]
#[non_exhaustive]
pub struct SessionTranscript {
    /// Content parts in file order.
    pub parts: Vec<SessionPart>,
    /// Whether input limits omitted later content.
    pub cut: bool,
    /// Optional source label; native agent files leave it empty.
    pub source: String,
}

/// Incremental transcript parser supplied by a session plugin.
pub trait SessionTranscriptParser: Send {
    /// Consumes one depth-checked JSON value.
    ///
    /// # Errors
    ///
    /// Returns [`PluginError`] when the value cannot be interpreted.
    fn push(&mut self, value: &serde_json::Value) -> Result<(), PluginError>;

    /// Finishes the transcript.
    ///
    /// # Errors
    ///
    /// Returns [`PluginError`] when the transcript cannot be completed.
    fn finish(self: Box<Self>) -> Result<SessionTranscript, PluginError>;
}

/// Compression the kernel removes before bounded JSONL parsing.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
#[non_exhaustive]
pub enum SessionCompression {
    /// Plain UTF-8 JSONL.
    #[default]
    None,
    /// A zstd-compressed JSONL stream, reparsed from its start when changed.
    Zstd,
}

/// How the kernel reads a session source.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
#[non_exhaustive]
pub enum SessionFormat {
    /// Incremental JSON lines, optionally compressed.
    #[default]
    JsonLines,
    /// One complete JSON value per listed file.
    Json,
    /// A read-only SQLite database that may contain many sessions.
    Sqlite,
}

/// One file returned by a [`SessionStore`].
#[derive(Clone, Debug, PartialEq, Eq)]
#[non_exhaustive]
pub struct SessionFile {
    /// Session id before parsing supplies richer identity.
    pub id: String,
    /// Absolute lexical path under the root passed to [`SessionStore::files`].
    pub path: PathBuf,
    /// Compression applied to the file bytes.
    pub compression: SessionCompression,
    /// Source format interpreted by the kernel and plugin.
    pub format: SessionFormat,
    /// Additional listed files that form the same session.
    pub related: Vec<PathBuf>,
}

impl SessionFile {
    /// A plain JSONL session file.
    #[must_use]
    pub fn new(id: impl Into<String>, path: impl Into<PathBuf>) -> Self {
        Self {
            id: id.into(),
            path: path.into(),
            compression: SessionCompression::None,
            format: SessionFormat::JsonLines,
            related: Vec::new(),
        }
    }

    /// Marks the file's compression.
    #[must_use]
    pub const fn with_compression(mut self, compression: SessionCompression) -> Self {
        self.compression = compression;
        self
    }

    /// Marks the source format.
    #[must_use]
    pub const fn with_format(mut self, format: SessionFormat) -> Self {
        self.format = format;
        self
    }

    /// Adds related files opened under the same root and bounds.
    #[must_use]
    pub fn with_related(mut self, related: Vec<PathBuf>) -> Self {
        self.related = related;
        self
    }
}

/// One SQLite value returned through the kernel's read-only connection.
#[derive(Clone, Debug, PartialEq)]
#[non_exhaustive]
pub enum SessionValue {
    /// SQL NULL.
    Null,
    /// A signed integer.
    Integer(i64),
    /// A floating-point number.
    Real(f64),
    /// UTF-8 text.
    Text(String),
    /// Binary data, used by schemas that store JSON as blobs.
    Blob(Vec<u8>),
}

/// Rows returned by one bounded SQLite query.
#[derive(Clone, Debug, Default, PartialEq)]
#[non_exhaustive]
pub struct SessionRows {
    /// Row values in selected-column order.
    pub values: Vec<Vec<SessionValue>>,
    /// Whether the database-wide row cap stopped this query.
    pub cut: bool,
}

impl SessionRows {
    /// Rows returned by the bounded kernel connection.
    #[must_use]
    pub const fn new(values: Vec<Vec<SessionValue>>, cut: bool) -> Self {
        Self { values, cut }
    }
}

/// Kernel-owned read-only SQLite access. Plugins supply only fixed schema SQL.
pub trait SessionDatabase {
    /// Runs one fixed query with text parameters, subject to database limits.
    ///
    /// # Errors
    ///
    /// Returns [`PluginError`] when SQLite rejects or cannot read the query.
    fn query(&self, sql: &str, params: &[&str]) -> Result<SessionRows, PluginError>;

    /// A content revision that changes when the database is written.
    ///
    /// # Errors
    ///
    /// Returns [`PluginError`] when the revision cannot be read.
    fn revision(&self) -> Result<i64, PluginError>;
}

/// A completed parse and the opaque state needed by the next incremental read.
#[derive(Clone, Debug, Default, PartialEq)]
#[non_exhaustive]
pub struct SessionParse {
    /// Parsed record, when the file contained a recognizable session.
    pub record: Option<SessionRecord>,
    /// Plugin-owned serialized state, normally JSON.
    pub state: String,
}

impl SessionParse {
    /// A completed parse.
    #[must_use]
    pub fn new(record: SessionRecord, state: impl Into<String>) -> Self {
        Self {
            record: Some(record),
            state: state.into(),
        }
    }
}

/// Incremental parser created by a [`SessionStore`].
pub trait SessionParser: Send {
    /// Consumes one depth-checked JSON value.
    ///
    /// # Errors
    ///
    /// Returns [`PluginError`] when the value cannot be interpreted.
    fn push(&mut self, value: &serde_json::Value) -> Result<(), PluginError>;

    /// Finishes the record and returns state for the next append.
    ///
    /// # Errors
    ///
    /// Returns [`PluginError`] when the partial record cannot be completed.
    fn finish(self: Box<Self>) -> Result<SessionParse, PluginError>;
}

/// Locates agent roots. Presence checks stat directories, never session files.
pub trait SessionLocator: Send + Sync {
    /// Roots the kernel may filter and pass to [`SessionStore::files`].
    fn roots(&self) -> Vec<PathBuf>;

    /// Whether the agent has a session directory.
    fn present(&self) -> bool;
}

/// A plugin-owned session format. Consent remains a kernel concern.
pub trait SessionStore: Send + Sync {
    /// Agent id used in records and consent.
    fn agent(&self) -> &str;

    /// Locator for this store.
    fn locator(&self) -> &dyn SessionLocator;

    /// Lists candidate files under the consent-filtered `root`.
    ///
    /// # Errors
    ///
    /// Returns [`PluginError`] when the directory walk fails.
    fn files(&self, root: &Path) -> Result<Vec<SessionFile>, PluginError>;

    /// Starts a parser, restoring opaque `state` when this is an append.
    ///
    /// # Errors
    ///
    /// Returns [`PluginError`] when the saved state is incompatible.
    fn parser(
        &self,
        file: &SessionFile,
        state: Option<&str>,
    ) -> Result<Box<dyn SessionParser>, PluginError>;

    /// Reads every session in a kernel-opened SQLite database.
    ///
    /// # Errors
    ///
    /// Returns [`PluginError`] when the plugin does not support SQLite or the
    /// database rows cannot be interpreted.
    fn read_database(
        &self,
        _file: &SessionFile,
        _database: &dyn SessionDatabase,
    ) -> Result<Vec<SessionRecord>, PluginError> {
        Err(PluginError::decode("session store does not support SQLite"))
    }

    /// Returns resume argv for a later explicit resume action, without spawning it.
    fn resume_argv(&self, _session_id: &str, _path: &Path) -> Option<Vec<String>> {
        None
    }

    /// Whether this store can read a transcript from a listed file.
    fn transcript_available(&self) -> bool {
        false
    }

    /// Starts a transcript parser for `file`. The kernel opens and bounds it.
    ///
    /// # Errors
    ///
    /// Returns [`PluginError`] when this store does not support transcript reads.
    fn transcript_parser(
        &self,
        file: &SessionFile,
    ) -> Result<Box<dyn SessionTranscriptParser>, PluginError> {
        let _ = file;
        Err(PluginError::unsupported(
            "session transcripts are not supported by this store",
        ))
    }

    /// Extra `(agent, session id)` pairs that suppress a matching gateway row.
    fn gateway_dedup_pairs(&self, _record: &SessionRecord) -> Vec<(String, String)> {
        Vec::new()
    }
}

#[cfg(test)]
mod tests {
    use super::{SessionDay, SessionRecord, SessionTokens};

    #[test]
    fn token_counters_add_and_report_totals() {
        let tokens = SessionTokens::new(2, 3, 4, 10).with_cache_write_1h(6);
        assert_eq!(tokens.cache_write_1h, 6);
        assert!(!tokens.is_zero());
        assert_eq!(tokens.total(), 19);
        assert!(SessionTokens::default().is_zero());
        assert_eq!(SessionTokens::default().total(), 0);

        let mut sum = SessionTokens::new(1, 0, 0, 0);
        sum.add(SessionTokens::new(0, 2, 0, 0));
        assert_eq!(sum, SessionTokens::new(1, 2, 0, 0));
    }

    #[test]
    fn session_record_usage_and_days_ignore_zeros_and_merge() {
        let mut record = SessionRecord::new("fake", "fake:one", "/sessions/one.jsonl");
        record.add_usage("m", SessionTokens::default());
        assert_eq!(record.models.len(), 0);
        record.add_usage("m", SessionTokens::new(2, 0, 0, 0));
        record.add_usage("m", SessionTokens::new(0, 3, 0, 0));
        assert_eq!(record.model_ids(), vec!["m".to_owned()]);
        assert_eq!(record.tokens(), SessionTokens::new(2, 3, 0, 0));

        record.add_day(SessionDay::new("2026-01-01", "m", SessionTokens::default()));
        assert_eq!(record.days.len(), 0);
        record.add_day(SessionDay::new(
            "2026-01-01",
            "m",
            SessionTokens::new(1, 0, 0, 0),
        ));
        record.add_day(SessionDay::new(
            "2026-01-01",
            "m",
            SessionTokens::new(0, 4, 0, 0),
        ));
        record.add_day(SessionDay::new(
            "2026-01-02",
            "m",
            SessionTokens::new(1, 0, 0, 0),
        ));
        record.add_day(SessionDay::new(
            "2026-01-01",
            "other",
            SessionTokens::new(1, 0, 0, 0),
        ));
        assert_eq!(record.days.len(), 3);
        assert_eq!(record.days[0].tokens, SessionTokens::new(1, 4, 0, 0));
    }
}
