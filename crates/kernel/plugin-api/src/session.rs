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

/// One parsed session file. Non-main records merge into their parent.
#[derive(Clone, Debug, Deserialize, PartialEq, Serialize)]
#[non_exhaustive]
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
    /// Parent checkpoint source for a fork, when one is named.
    #[serde(default)]
    pub forked_from: Option<String>,
    /// Usage observations used for checkpoint dedup.
    #[serde(default)]
    pub usage: Vec<SessionUsageEvent>,
    /// Whether input bounds or the agent format omitted some content.
    pub cut: bool,
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
            forked_from: None,
            usage: Vec::new(),
            cut: false,
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
}

impl SessionFile {
    /// A plain session file.
    #[must_use]
    pub fn new(id: impl Into<String>, path: impl Into<PathBuf>) -> Self {
        Self {
            id: id.into(),
            path: path.into(),
            compression: SessionCompression::None,
        }
    }

    /// Marks the file's compression.
    #[must_use]
    pub const fn with_compression(mut self, compression: SessionCompression) -> Self {
        self.compression = compression;
        self
    }
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
}
