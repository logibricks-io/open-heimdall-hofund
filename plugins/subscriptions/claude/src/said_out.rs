//! Limit lines and session refusals kept until the reset they name.
//!
//! A failed `/usage` read used to leave the sign-in where it was. A line that
//! says the allowance is spent, or a session `rate_limit` refusal since this
//! account was the one in use, is kept here until that reset. The sign-in
//! switch reads it as an account-wide held window when it applies to the
//! model the agent is on. A whole good `/usage` read clears it.

use std::collections::BTreeMap;
use std::fs;
use std::io::Write;
use std::path::{Path, PathBuf};
use std::sync::{Mutex, OnceLock};
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use hofund_plugin_api::{AccountId, HostContext, PluginError, PluginErrorKind, QuotaWindow};
use regex::Regex;
use serde_json::{Map, Value};

use crate::usage;
use crate::usage_reset::reset_time;

const HOUR: Duration = Duration::from_secs(60 * 60);
const DAY: Duration = Duration::from_secs(24 * 60 * 60);
const FIVE_HOURS: Duration = Duration::from_secs(5 * 60 * 60);
const WEEK: Duration = Duration::from_secs(7 * 24 * 60 * 60);
const SETTLE: Duration = Duration::from_secs(60);
const SCAN_TAIL: u64 = 1 << 20;

/// How long a quota refusal rests when the vendor named no reset.
///
/// The router uses the same default when a quota error carries no reset.
pub(crate) const QUOTA_GUESS: Duration = Duration::from_secs(15 * 60);

const FILE_NAME: &str = "allowance-limits.json";
const HELD_NAME: &str = "limit";

fn limit_said() -> &'static Regex {
    static RE: OnceLock<Regex> = OnceLock::new();
    RE.get_or_init(|| {
        Regex::new(
            r"(?i)(?:hit|reached) your (?:[\w.-]+ ){0,3}limit|\b(?:session|weekly|usage) limit (?:reached|hit)",
        )
        .expect("limit line")
    })
}

fn family_said() -> &'static Regex {
    static RE: OnceLock<Regex> = OnceLock::new();
    RE.get_or_init(|| {
        Regex::new(r"(?i)(?:hit|reached) your (opus|sonnet|haiku|fable|mythos) limit")
            .expect("family limit")
    })
}

fn model_family_re() -> &'static Regex {
    static RE: OnceLock<Regex> = OnceLock::new();
    RE.get_or_init(|| Regex::new(r"(?i)(opus|sonnet|haiku|fable|mythos)").expect("model family"))
}

fn resets_said() -> &'static Regex {
    static RE: OnceLock<Regex> = OnceLock::new();
    RE.get_or_init(|| Regex::new(r"(?i)\bresets\s+(.+?)\s*$").expect("reset words"))
}

fn not_your_limit() -> &'static Regex {
    static RE: OnceLock<Regex> = OnceLock::new();
    RE.get_or_init(|| Regex::new(r"(?i)\(not your usage limit\)").expect("not your limit"))
}

/// The model family a model id or alias names, when it has one.
#[must_use]
pub(crate) fn model_family(model: &str) -> Option<String> {
    model_family_re()
        .captures(model)
        .and_then(|caps| caps.get(1))
        .map(|m| m.as_str().to_ascii_lowercase())
}

/// Family a refusal is for. Empty means the whole account.
#[must_use]
pub(crate) fn limit_family(kind: &str, said: &str) -> Option<String> {
    let from_kind = match kind {
        "seven_day_opus" => Some("opus"),
        "seven_day_sonnet" => Some("sonnet"),
        "seven_day_overage_included" => Some("fable"),
        _ => None,
    };
    if let Some(family) = from_kind {
        return Some(family.to_owned());
    }
    family_said()
        .captures(said)
        .and_then(|caps| caps.get(1))
        .map(|name| name.as_str().to_ascii_lowercase())
}

fn scrub(text: &str) -> String {
    not_your_limit().replace_all(text, "").into_owned()
}

/// Whether `text` is the wording that the allowance is spent.
#[must_use]
pub(crate) fn is_limit_line(text: &str) -> bool {
    limit_said().is_match(&scrub(text))
}

/// When the words say the limit ends. An unreadable reset lasts one hour.
#[must_use]
pub(crate) fn said_until(text: &str, now: SystemTime) -> SystemTime {
    said_reset(text, now).unwrap_or(now + HOUR)
}

fn said_reset(text: &str, now: SystemTime) -> Option<SystemTime> {
    let caps = resets_said().captures(text)?;
    let words = caps.get(1)?.as_str().trim().trim_end_matches('.').trim();
    if words.is_empty() {
        return None;
    }
    let lower = text.to_ascii_lowercase();
    let span = if lower.contains("session") || lower.contains("5-hour") {
        FIVE_HOURS
    } else {
        WEEK
    };
    let at = reset_time(Some(words), now, span)?;
    at.gt(&now).then_some(at)
}

/// Until when a session refusal says the account is out.
#[must_use]
pub(crate) fn refusal_until(resets_at: u64, kind: &str, said: &str, at: SystemTime) -> SystemTime {
    if resets_at > 0 {
        let stamp = UNIX_EPOCH + Duration::from_secs(resets_at);
        if stamp > at {
            return stamp;
        }
    }
    if let Some(stamp) = said_reset(said, at) {
        return stamp;
    }
    match kind {
        "five_hour" => at + FIVE_HOURS,
        "" => at + HOUR,
        _ => at + DAY,
    }
}

#[derive(Clone, Debug, Default)]
struct Part {
    until: Option<SystemTime>,
    said: String,
}

#[derive(Clone, Debug, Default)]
struct Entry {
    whole: Part,
    models: BTreeMap<String, Part>,
}

fn key_of(account: &AccountId) -> String {
    account.as_str().to_ascii_lowercase()
}

fn limits_path(host: &HostContext) -> Option<PathBuf> {
    host.dirs().state_dir().ok().map(|dir| dir.join(FILE_NAME))
}

fn encode_time(at: SystemTime) -> String {
    hofund_plugin_sdk::time::format_rfc3339(at)
}

fn decode_time(text: &str) -> Option<SystemTime> {
    hofund_plugin_sdk::time::parse_rfc3339(text)
}

fn part_from(value: &Value) -> Part {
    let until = value
        .get("until")
        .and_then(Value::as_str)
        .and_then(decode_time);
    let said = value
        .get("said")
        .and_then(Value::as_str)
        .unwrap_or("")
        .to_owned();
    Part { until, said }
}

fn load(path: &Path) -> Result<BTreeMap<String, Entry>, ()> {
    if !path.is_file() {
        return Ok(BTreeMap::new());
    }
    let text = fs::read_to_string(path).map_err(|_| ())?;
    let raw: Map<String, Value> = serde_json::from_str(&text).map_err(|_| ())?;
    let mut out = BTreeMap::new();
    for (key, value) in raw {
        let mut entry = Entry {
            whole: part_from(&value),
            models: BTreeMap::new(),
        };
        if let Some(models) = value.get("models").and_then(Value::as_object) {
            for (family, part) in models {
                entry.models.insert(family.clone(), part_from(part));
            }
        }
        out.insert(key, entry);
    }
    Ok(out)
}

fn part_json(part: &Part) -> Value {
    let mut map = Map::new();
    if let Some(until) = part.until {
        map.insert("until".to_owned(), Value::String(encode_time(until)));
    }
    if !part.said.is_empty() {
        map.insert("said".to_owned(), Value::String(part.said.clone()));
    }
    Value::Object(map)
}

fn store(path: &Path, map: &BTreeMap<String, Entry>) {
    if let Some(parent) = path.parent() {
        let _ = fs::create_dir_all(parent);
    }
    let mut raw = Map::new();
    for (key, entry) in map {
        let mut object = part_json(&entry.whole);
        if !entry.models.is_empty() {
            let mut models = Map::new();
            for (family, part) in &entry.models {
                models.insert(family.clone(), part_json(part));
            }
            if let Value::Object(fields) = &mut object {
                fields.insert("models".to_owned(), Value::Object(models));
            }
        }
        raw.insert(key.clone(), object);
    }
    let Ok(bytes) = serde_json::to_vec(&raw) else {
        return;
    };
    let tmp = path.with_extension("json.tmp");
    {
        let mut options = fs::OpenOptions::new();
        options.write(true).create(true).truncate(true);
        #[cfg(unix)]
        {
            use std::os::unix::fs::OpenOptionsExt;
            options.mode(0o600);
        }
        let Ok(mut file) = options.open(&tmp) else {
            return;
        };
        if file.write_all(&bytes).is_err() {
            let _ = fs::remove_file(&tmp);
            return;
        }
    }
    if fs::rename(&tmp, path).is_err() {
        let _ = fs::remove_file(&tmp);
    }
}

fn prune(map: &mut BTreeMap<String, Entry>, now: SystemTime) {
    map.retain(|_, entry| {
        entry
            .models
            .retain(|_, part| part.until.is_some_and(|at| at > now));
        if entry.whole.until.is_some_and(|at| at <= now) {
            entry.whole = Part::default();
        }
        entry.whole.until.is_some() || !entry.models.is_empty()
    });
}

static FILE_MU: Mutex<()> = Mutex::new(());

fn clip(text: &str) -> String {
    let trimmed = text.trim();
    let chars: Vec<char> = trimmed.chars().collect();
    if chars.len() > 200 {
        chars[..200].iter().collect()
    } else {
        trimmed.to_owned()
    }
}

fn standing(part: &Part, now: SystemTime) -> bool {
    part.until.is_some_and(|at| at > now)
}

/// Keeps `said` for `account` until `until`.
///
/// `named` is false when `until` is a guess. A guess does not replace a
/// reset that lasts at least as long.
fn keep(
    path: &Path,
    account: &AccountId,
    family: Option<&str>,
    until: SystemTime,
    said: &str,
    now: SystemTime,
    named: bool,
) {
    if !until.gt(&now) {
        return;
    }
    let _guard = FILE_MU
        .lock()
        .unwrap_or_else(|poisoned| poisoned.into_inner());
    let mut map = load(path).unwrap_or_default();
    prune(&mut map, now);
    let key = key_of(account);
    if !named {
        let longer = map.get(&key).is_some_and(|entry| {
            let part = match family {
                Some(family) => entry.models.get(family),
                None => Some(&entry.whole),
            };
            part.is_some_and(|part| part.until.is_some_and(|at| at >= until))
        });
        if longer {
            return;
        }
    }
    let entry = map.entry(key).or_default();
    let part = Part {
        until: Some(until),
        said: clip(said),
    };
    if let Some(family) = family {
        entry.models.insert(family.to_owned(), part);
    } else {
        let models = std::mem::take(&mut entry.models);
        entry.whole = part;
        entry.models = models;
    }
    store(path, &map);
}

/// Records a limit line. Other text is left as it was.
pub(crate) fn note_text(host: &HostContext, account: &AccountId, text: &str, now: SystemTime) {
    let Some(path) = limits_path(host) else {
        return;
    };
    let said = scrub(text);
    if !limit_said().is_match(&said) {
        return;
    }
    let family = limit_family("", &said);
    keep(
        &path,
        account,
        family.as_deref(),
        said_until(&said, now),
        &said,
        now,
        true,
    );
}

/// Drops a kept limit after a whole good read. A family stays while its own
/// window is still full, or while the read did not mention that family.
pub(crate) fn clear_good_read(
    host: &HostContext,
    account: &AccountId,
    windows: &[QuotaWindow],
    now: SystemTime,
) {
    let Some(path) = limits_path(host) else {
        return;
    };
    let _guard = FILE_MU
        .lock()
        .unwrap_or_else(|poisoned| poisoned.into_inner());
    let Ok(mut map) = load(&path) else {
        return;
    };
    let key = key_of(account);
    let Some(entry) = map.get(&key).cloned() else {
        return;
    };
    let mut left = BTreeMap::new();
    for (family, part) in entry.models {
        if !standing(&part, now) {
            continue;
        }
        let mut read = false;
        let mut full = false;
        for window in windows {
            let scoped = window
                .scope_model()
                .and_then(model_family)
                .or_else(|| model_family(window.name()));
            if scoped.as_deref() != Some(family.as_str()) {
                continue;
            }
            read = true;
            let reset_passed = window.resets_at().is_some_and(|at| at <= now);
            if window.used() >= 100 && !reset_passed {
                full = true;
            }
        }
        if !read || full {
            left.insert(family, part);
        }
    }
    if left.is_empty() {
        map.remove(&key);
    } else {
        map.insert(
            key,
            Entry {
                whole: Part::default(),
                models: left,
            },
        );
    }
    store(&path, &map);
}

fn families_of(home: &Path, now: SystemTime) -> Option<Vec<String>> {
    let recent = {
        let watch = WATCH
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner());
        let mut names: Vec<String> = watch
            .families
            .iter()
            .filter(|(_, at)| recent_at(**at, now))
            .map(|(name, _)| name.clone())
            .collect();
        names.sort();
        names
    };
    if !recent.is_empty() {
        return Some(recent);
    }
    settings_family(home).map(|family| vec![family])
}

fn recent_at(at: SystemTime, now: SystemTime) -> bool {
    match now.duration_since(at) {
        Ok(age) => age < HOUR,
        Err(_) => true,
    }
}

fn settings_family(home: &Path) -> Option<String> {
    let text = fs::read_to_string(home.join("settings.json")).ok()?;
    let value: Value = serde_json::from_str(&text).ok()?;
    model_family(value.get("model").and_then(Value::as_str).unwrap_or(""))
}

fn agent_requests_are_routed(home: &Path) -> bool {
    let Ok(text) = fs::read_to_string(home.join("settings.json")) else {
        return false;
    };
    let Ok(value) = serde_json::from_str::<Value>(&text) else {
        return false;
    };
    value
        .pointer("/env/ANTHROPIC_BASE_URL")
        .and_then(Value::as_str)
        .is_some_and(|url| !url.trim().is_empty())
}

fn applicable_until(
    entry: &Entry,
    families: Option<&[String]>,
    now: SystemTime,
) -> Option<SystemTime> {
    let mut until = None;
    if standing(&entry.whole, now) {
        until = entry.whole.until;
    }
    let consider = |part: &Part, until: &mut Option<SystemTime>| {
        if standing(part, now)
            && let Some(at) = part.until
        {
            *until = Some(until.map_or(at, |have| have.max(at)));
        }
    };
    match families {
        None => {
            for part in entry.models.values() {
                consider(part, &mut until);
            }
        }
        Some(names) => {
            for name in names {
                if let Some(part) = entry.models.get(name) {
                    consider(part, &mut until);
                }
            }
        }
    }
    until
}

fn held_window(until: SystemTime) -> Vec<QuotaWindow> {
    let window = QuotaWindow::new(HELD_NAME, 100, Some(100), Some(until))
        .expect("held window")
        .with_held(true);
    vec![window]
}

fn account_wide(window: &QuotaWindow) -> bool {
    window.scope_model().is_none()
        && window.models().is_empty()
        && window.excluded_models().is_empty()
}

/// Adds an account-wide held window when a kept limit applies.
#[must_use]
pub(crate) fn overlay(
    host: &HostContext,
    account: &AccountId,
    mut windows: Vec<QuotaWindow>,
    now: SystemTime,
) -> Vec<QuotaWindow> {
    let Some(until) = kept_until(host, account, now) else {
        return windows;
    };
    if windows
        .iter()
        .any(|window| window.held() && account_wide(window))
    {
        return windows;
    }
    windows.extend(held_window(until));
    windows
}

fn kept_until(host: &HostContext, account: &AccountId, now: SystemTime) -> Option<SystemTime> {
    let path = limits_path(host)?;
    let map = load(&path).ok()?;
    let entry = map.get(&key_of(account))?;
    let families = agent_home().and_then(|home| families_of(&home, now));
    applicable_until(entry, families.as_deref(), now)
}

fn agent_home() -> Option<PathBuf> {
    crate::cli::agent_home()
}

/// Windows that replace a failed read when the account counts as spent.
#[must_use]
pub(crate) fn spent_on_failure(
    host: &HostContext,
    account: &AccountId,
    error: &PluginError,
    now: SystemTime,
) -> Option<Vec<QuotaWindow>> {
    note_text(host, account, error.message(), now);
    if matches!(error.kind(), PluginErrorKind::Auth) && kept_until(host, account, now).is_none() {
        return None;
    }
    if unread_counts(account, error.message(), now) || kept_until(host, account, now).is_some() {
        let windows = overlay(host, account, Vec::new(), now);
        if windows.iter().any(|window| window.held()) {
            return Some(windows);
        }
        return memory_held(account, error.message(), now);
    }
    None
}

fn memory_held(account: &AccountId, message: &str, now: SystemTime) -> Option<Vec<QuotaWindow>> {
    if !unread_counts(account, message, now) {
        return None;
    }
    let watch = WATCH
        .lock()
        .unwrap_or_else(|poisoned| poisoned.into_inner());
    let until = watch.refusal.as_ref().map(|refusal| refusal.until)?;
    until.gt(&now).then(|| held_window(until))
}

fn unread_counts(account: &AccountId, message: &str, now: SystemTime) -> bool {
    if !crate::usage_cadence::is_active_account(account) {
        return false;
    }
    let said = scrub(message);
    let families = agent_home().and_then(|home| families_of(&home, now));
    if is_limit_line(&said) {
        return family_stops(limit_family("", &said).as_deref(), families.as_deref());
    }
    if usage::sign_in_refused(&said) {
        return false;
    }
    let watch = WATCH
        .lock()
        .unwrap_or_else(|poisoned| poisoned.into_inner());
    let Some(since) = watch.on_since else {
        return false;
    };
    let Some(refusal) = watch.refusal.as_ref() else {
        return false;
    };
    if refusal.at <= since {
        return false;
    }
    family_stops(refusal.family.as_deref(), families.as_deref())
}

fn family_stops(family: Option<&str>, families: Option<&[String]>) -> bool {
    match family {
        None => true,
        Some(family) => families.is_none_or(|names| names.iter().any(|name| name == family)),
    }
}

#[derive(Clone, Debug)]
struct Refusal {
    at: SystemTime,
    until: SystemTime,
    said: String,
    family: Option<String>,
    kept: bool,
}

#[derive(Default)]
struct Watch {
    started: bool,
    off: BTreeMap<PathBuf, u64>,
    on_user: String,
    on_since: Option<SystemTime>,
    families: BTreeMap<String, SystemTime>,
    refusal: Option<Refusal>,
}

static WATCH: Mutex<Watch> = Mutex::new(Watch {
    started: false,
    off: BTreeMap::new(),
    on_user: String::new(),
    on_since: None,
    families: BTreeMap::new(),
    refusal: None,
});

/// Looks at the agent home's session logs when `account` is the one in use.
pub(crate) fn observe_agent(host: &HostContext, account: &AccountId, now: SystemTime) {
    if !crate::usage_cadence::is_active_account(account) {
        return;
    }
    let Some(home) = agent_home() else {
        return;
    };
    let mut watch = WATCH
        .lock()
        .unwrap_or_else(|poisoned| poisoned.into_inner());
    if watch.on_user != account.as_str() || watch.on_since.is_none() {
        watch.on_user = account.as_str().to_owned();
        watch.on_since = Some(now);
    }
    scan_projects(&mut watch, &home.join("projects"), now);
    let since = watch.on_since.unwrap_or(now);
    let routed = agent_requests_are_routed(&home);
    let Some(refusal) = watch.refusal.clone() else {
        return;
    };
    if refusal.kept || routed || refusal.at <= since + SETTLE || refusal.until <= now {
        return;
    }
    drop(watch);
    if same_reset_kept_for_other(host, account, refusal.family.as_deref(), refusal.until) {
        return;
    }
    if let Some(path) = limits_path(host) {
        keep(
            &path,
            account,
            refusal.family.as_deref(),
            refusal.until,
            if refusal.said.is_empty() {
                "the session refused the account for its limit"
            } else {
                &refusal.said
            },
            now,
            true,
        );
    }
    let mut watch = WATCH
        .lock()
        .unwrap_or_else(|poisoned| poisoned.into_inner());
    if let Some(current) = watch.refusal.as_mut()
        && current.at == refusal.at
    {
        current.kept = true;
    }
}

fn same_reset_kept_for_other(
    host: &HostContext,
    account: &AccountId,
    family: Option<&str>,
    until: SystemTime,
) -> bool {
    let Some(path) = limits_path(host) else {
        return false;
    };
    let Ok(map) = load(&path) else {
        return false;
    };
    let mine = key_of(account);
    map.iter().any(|(key, entry)| {
        if key == &mine {
            return false;
        }
        match family {
            None => entry.whole.until == Some(until),
            Some(family) => entry.models.get(family).and_then(|part| part.until) == Some(until),
        }
    })
}

fn scan_projects(watch: &mut Watch, projects: &Path, now: SystemTime) {
    let mut off = BTreeMap::new();
    let Ok(dirs) = fs::read_dir(projects) else {
        watch.off.clear();
        watch.started = true;
        return;
    };
    for dir in dirs.flatten() {
        let root = dir.path();
        let is_dir = dir.file_type().ok().is_some_and(|kind| kind.is_dir())
            || fs::metadata(&root).ok().is_some_and(|meta| meta.is_dir());
        if !is_dir {
            continue;
        }
        let Ok(files) = fs::read_dir(&root) else {
            continue;
        };
        for file in files.flatten() {
            let path = file.path();
            if path.extension().is_none_or(|ext| ext != "jsonl") {
                continue;
            }
            let Ok(meta) = file.metadata() else {
                continue;
            };
            let size = meta.len();
            let was = watch.off.get(&path).copied();
            if !watch.started {
                off.insert(path, size);
                continue;
            }
            let start = match was {
                Some(was) if was == size => {
                    off.insert(path, size);
                    continue;
                }
                Some(was) if was < size => was,
                _ => 0,
            };
            let end = scan_session(watch, &path, start, size, now);
            off.insert(path, end);
        }
    }
    watch.off = off;
    watch.started = true;
}

fn scan_session(watch: &mut Watch, path: &Path, from: u64, size: u64, now: SystemTime) -> u64 {
    let from = if size < from { 0 } else { from };
    let start = from.max(size.saturating_sub(SCAN_TAIL));
    let Ok(bytes) = fs::read(path) else {
        return from;
    };
    let end = usize::try_from(size)
        .unwrap_or(bytes.len())
        .min(bytes.len());
    let mut at = usize::try_from(start).unwrap_or(0).min(end);
    if start > from {
        if let Some(rel) = bytes[at..end].iter().position(|byte| *byte == b'\n') {
            at += rel + 1;
        } else {
            return size;
        }
    }
    while at < end {
        let Some(rel) = bytes[at..end].iter().position(|byte| *byte == b'\n') else {
            return u64::try_from(at).unwrap_or(from);
        };
        let next = at + rel + 1;
        if let Ok(line) = std::str::from_utf8(&bytes[at..next]) {
            note_session_line(watch, line.trim(), now);
        }
        at = next;
    }
    u64::try_from(at).unwrap_or(size)
}

fn note_session_line(watch: &mut Watch, line: &str, now: SystemTime) {
    if line.is_empty() {
        return;
    }
    let Ok(value) = serde_json::from_str::<Value>(line) else {
        return;
    };
    if value.get("type").and_then(Value::as_str) == Some("assistant")
        && let Some(model) = value.pointer("/message/model").and_then(Value::as_str)
        && let Some(family) = model_family(model)
    {
        let at = value
            .get("timestamp")
            .and_then(Value::as_str)
            .and_then(hofund_plugin_sdk::time::parse_rfc3339)
            .unwrap_or(now);
        let slot = watch.families.entry(family).or_insert(at);
        if at > *slot {
            *slot = at;
        }
    }
    if value.get("error").and_then(Value::as_str) != Some("rate_limit")
        || value.get("isApiErrorMessage").and_then(Value::as_bool) != Some(true)
    {
        return;
    }
    let at = value
        .get("timestamp")
        .and_then(Value::as_str)
        .and_then(hofund_plugin_sdk::time::parse_rfc3339)
        .unwrap_or(now);
    let said = refusal_text(&value);
    let quota = refusal_quota(&value);
    let until = refusal_until(quota.resets_at, &quota.kind, &said, at);
    let family = limit_family(&quota.kind, &said);
    let replace = watch.refusal.as_ref().is_none_or(|have| at > have.at);
    if replace {
        watch.refusal = Some(Refusal {
            at,
            until,
            said,
            family,
            kept: false,
        });
    }
}

fn refusal_text(value: &Value) -> String {
    value
        .pointer("/message/content")
        .and_then(Value::as_array)
        .and_then(|parts| {
            parts.iter().find_map(|part| {
                (part.get("type").and_then(Value::as_str) == Some("text"))
                    .then(|| part.get("text").and_then(Value::as_str))
                    .flatten()
            })
        })
        .unwrap_or("")
        .to_owned()
}

struct QuotaBits {
    resets_at: u64,
    kind: String,
}

fn refusal_quota(value: &Value) -> QuotaBits {
    let primary = value.get("quotaLimits");
    let nested = value.pointer("/apiErrorParams/rate_limit_info");
    let chosen = match primary {
        Some(primary)
            if primary.get("resetsAt").and_then(Value::as_u64).unwrap_or(0) > 0
                || primary
                    .get("rateLimitType")
                    .and_then(Value::as_str)
                    .is_some_and(|kind| !kind.is_empty()) =>
        {
            primary
        }
        _ => nested.unwrap_or(&Value::Null),
    };
    QuotaBits {
        resets_at: chosen.get("resetsAt").and_then(Value::as_u64).unwrap_or(0),
        kind: chosen
            .get("rateLimitType")
            .and_then(Value::as_str)
            .unwrap_or("")
            .to_owned(),
    }
}

/// A quota error from the bridge, kept for the sign-in switch.
pub(crate) fn note_bridge(
    host: &HostContext,
    account: &AccountId,
    model: &str,
    error: &PluginError,
    now: SystemTime,
) {
    if error.kind() != PluginErrorKind::Quota {
        return;
    }
    let Some(path) = limits_path(host) else {
        return;
    };
    let named = error.reset_after().is_some();
    let until = now + error.reset_after().unwrap_or(QUOTA_GUESS);
    let family = if error.scopes_model() {
        limit_family("", error.message()).or_else(|| model_family(model))
    } else {
        None
    };
    if error.scopes_model() && family.is_none() {
        return;
    }
    keep(
        &path,
        account,
        family.as_deref(),
        until,
        error.message(),
        now,
        named,
    );
}

/// A request the kept limit already refuses, before another process starts.
#[must_use]
pub(crate) fn block_request(
    host: &HostContext,
    account: &AccountId,
    model: &str,
) -> Option<PluginError> {
    let path = limits_path(host)?;
    let map = load(&path).ok()?;
    let entry = map.get(&key_of(account))?;
    let now = host.clock().now();
    if let Some(until) = entry.whole.until.filter(|at| *at > now) {
        return Some(spent_error(until, now, false));
    }
    let family = model_family(model)?;
    let until = entry.models.get(&family)?.until.filter(|at| *at > now)?;
    Some(spent_error(until, now, true))
}

fn spent_error(until: SystemTime, now: SystemTime, model_scoped: bool) -> PluginError {
    let after = until.duration_since(now).unwrap_or(Duration::from_secs(1));
    let mut error = PluginError::quota_status(429, "the allowance is spent until it resets")
        .with_reset_after(after);
    if model_scoped {
        error = error.with_model_scope();
    }
    error
}

/// Last rejected allowance event in one turn, when it names a future reset.
#[must_use]
pub(crate) fn rejected_reset(
    info: &crate::bridge::decode::RateLimitInfo,
    now: SystemTime,
) -> Option<SystemTime> {
    if info.status.as_deref() != Some("rejected") {
        return None;
    }
    let secs = info.resets_at.filter(|secs| *secs > 0)?;
    let at = UNIX_EPOCH + Duration::from_secs(secs);
    at.gt(&now).then_some(at)
}

#[cfg(test)]
pub(crate) fn test_lock() -> std::sync::MutexGuard<'static, ()> {
    static LOCK: Mutex<()> = Mutex::new(());
    LOCK.lock().unwrap_or_else(|poisoned| poisoned.into_inner())
}

#[cfg(test)]
pub(crate) fn reset_for_test() {
    *WATCH
        .lock()
        .unwrap_or_else(|poisoned| poisoned.into_inner()) = Watch::default();
}

#[cfg(test)]
mod tests {
    use std::io::Write;
    use std::path::PathBuf;
    use std::time::{Duration, SystemTime, UNIX_EPOCH};

    use hofund_plugin_api::{AccountId, PluginError, QuotaWindow};

    use super::*;

    fn at(secs: u64) -> SystemTime {
        UNIX_EPOCH + Duration::from_secs(secs)
    }

    fn account(name: &'static str) -> AccountId {
        AccountId::from_static(name)
    }

    struct HomeGuard;
    impl Drop for HomeGuard {
        fn drop(&mut self) {
            crate::cli::test_configured_dir(None);
            crate::cli::test_default_home(None);
        }
    }

    struct Guards {
        _cadence: std::sync::MutexGuard<'static, ()>,
        _watch: std::sync::MutexGuard<'static, ()>,
        _home: HomeGuard,
    }

    fn guards() -> Guards {
        let cadence = crate::usage_cadence::test_hooks::lock();
        let watch = test_lock();
        reset_for_test();
        crate::cli::test_configured_dir(None);
        crate::cli::test_default_home(None);
        Guards {
            _cadence: cadence,
            _watch: watch,
            _home: HomeGuard,
        }
    }

    #[test]
    fn d254_limit_lines_count_and_a_signed_out_line_does_not() {
        let _guards = guards();
        assert!(is_limit_line(
            "You've hit your weekly limit · resets Nov 24 at 5am (UTC)"
        ));
        assert!(is_limit_line(
            "You've hit your session limit · resets 3:30am (UTC)"
        ));
        assert!(is_limit_line("You've hit your Fable limit for this week"));
        assert!(is_limit_line("You've hit your limit · resets 5pm (UTC)"));
        assert!(!is_limit_line("HTTP 401 Unauthorized"));
        assert!(!is_limit_line("signal: killed"));
        assert!(!is_limit_line(
            "Fable limit reached · continuing on usage credits"
        ));
        assert!(!is_limit_line(
            "Claude is temporarily limiting requests (not your usage limit)"
        ));
    }

    #[test]
    fn d254_said_until_uses_the_named_reset_or_one_hour() {
        let _guards = guards();
        let now = at(1_700_000_000);
        let weekly = said_until(
            "You've hit your weekly limit · resets Nov 24, 2027 at 5am (UTC)",
            now,
        );
        assert!(weekly > now);
        assert_eq!(
            weekly,
            hofund_plugin_sdk::time::parse_rfc3339("2027-11-24T05:00:00Z").expect("reset")
        );
        let unknown = said_until("You've hit your limit", now);
        assert_eq!(unknown, now + HOUR);
    }

    #[test]
    fn d254_refusal_until_prefers_resets_at_then_words_then_the_window() {
        let _guards = guards();
        let at = hofund_plugin_sdk::time::parse_rfc3339("2026-11-10T07:19:36Z").expect("at");
        let noon = hofund_plugin_sdk::time::parse_rfc3339("2026-11-10T12:00:00Z").expect("noon");
        assert_eq!(
            refusal_until(
                1_794_463_200,
                "seven_day",
                "You've hit your weekly limit · resets Nov 12 at 2pm (UTC)",
                at
            ),
            UNIX_EPOCH + Duration::from_secs(1_794_463_200)
        );
        assert_eq!(
            refusal_until(
                0,
                "",
                "You've hit your 5-hour limit · resets 12pm (UTC)",
                at
            ),
            noon
        );
        assert_eq!(
            refusal_until(
                at.duration_since(UNIX_EPOCH).expect("secs").as_secs() - 60,
                "five_hour",
                "You've hit your 5-hour limit · resets 12pm (UTC)",
                at
            ),
            noon
        );
        assert_eq!(
            refusal_until(0, "five_hour", "You've hit your 5-hour limit", at),
            at + FIVE_HOURS
        );
        assert_eq!(
            refusal_until(0, "seven_day", "You've hit your weekly limit", at),
            at + DAY
        );
        assert_eq!(
            refusal_until(0, "", "API Error: Rate limited", at),
            at + HOUR
        );
    }

    #[test]
    fn d254_a_heard_limit_is_kept_until_reset_and_a_good_read_clears_it() {
        let _guards = guards();
        let dir = tempfile::tempdir().expect("dir");
        let host = hofund_kit_subscription::TestHost::open_for(
            hofund_plugin_api::PluginKind::Subscription,
            hofund_plugin_api::PluginId::from_static("claude"),
            dir.path(),
        );
        let ctx = host.context();
        let id = account("acct-week");
        let now = at(1_700_000_000);
        note_text(
            ctx,
            &id,
            "You've hit your weekly limit · resets Nov 24, 2027 at 5am (UTC)",
            now,
        );
        let held = overlay(ctx, &id, Vec::new(), now);
        assert!(
            held.iter()
                .any(|window| window.held() && window.used() == 100)
        );
        let open = QuotaWindow::new("5 hours", 10, Some(100), Some(now + FIVE_HOURS)).expect("w");
        clear_good_read(ctx, &id, std::slice::from_ref(&open), now);
        assert!(
            overlay(ctx, &id, vec![open], now)
                .iter()
                .all(|window| !window.held())
        );
    }

    #[test]
    fn d254_a_guess_does_not_shorten_a_named_reset() {
        let _guards = guards();
        let dir = tempfile::tempdir().expect("dir");
        let host = hofund_kit_subscription::TestHost::open_for(
            hofund_plugin_api::PluginKind::Subscription,
            hofund_plugin_api::PluginId::from_static("claude"),
            dir.path(),
        );
        let ctx = host.context();
        let id = account("acct-keep");
        let now = at(1_800_000_000);
        let named = now + WEEK;
        let path = limits_path(ctx).expect("path");
        keep(&path, &id, None, named, "weekly", now, true);
        keep(&path, &id, None, now + QUOTA_GUESS, "guess", now, false);
        let until = kept_until(ctx, &id, now).expect("until");
        assert_eq!(until, named);
    }

    #[test]
    fn d254_a_family_limit_holds_only_that_family() {
        let _guards = guards();
        let dir = tempfile::tempdir().expect("dir");
        let host = hofund_kit_subscription::TestHost::open_for(
            hofund_plugin_api::PluginKind::Subscription,
            hofund_plugin_api::PluginId::from_static("claude"),
            dir.path(),
        );
        let ctx = host.context();
        let id = account("acct-fable");
        let now = at(1_800_000_000);
        note_text(
            ctx,
            &id,
            "You've hit your Fable limit · resets Nov 24, 2027 at 5am (UTC)",
            now,
        );
        let home = dir.path().join("agent");
        fs::create_dir_all(&home).expect("home");
        fs::write(home.join("settings.json"), r#"{"model":"opus"}"#).expect("settings");
        crate::cli::test_configured_dir(Some(home.clone()));
        assert!(kept_until(ctx, &id, now).is_none());
        fs::write(home.join("settings.json"), r#"{"model":"fable"}"#).expect("settings");
        assert!(kept_until(ctx, &id, now).is_some());
        assert!(block_request(ctx, &id, "claude-opus-4").is_none());
        let fable = block_request(ctx, &id, "claude-fable-5").expect("fable");
        assert!(fable.scopes_model());
    }

    #[test]
    fn d254_an_unreadable_file_holds_nobody_out() {
        let _guards = guards();
        let dir = tempfile::tempdir().expect("dir");
        let host = hofund_kit_subscription::TestHost::open_for(
            hofund_plugin_api::PluginKind::Subscription,
            hofund_plugin_api::PluginId::from_static("claude"),
            dir.path(),
        );
        let ctx = host.context();
        let path = limits_path(ctx).expect("path");
        fs::create_dir_all(path.parent().expect("parent")).expect("dir");
        fs::write(&path, b"{").expect("garbage");
        let id = account("acct-bad");
        assert!(kept_until(ctx, &id, at(1_800_000_000)).is_none());
    }

    #[test]
    fn d254_bridge_quota_rest_is_kept_and_a_rate_limit_is_not() {
        let _guards = guards();
        let dir = tempfile::tempdir().expect("dir");
        let host = hofund_kit_subscription::TestHost::open_for(
            hofund_plugin_api::PluginKind::Subscription,
            hofund_plugin_api::PluginId::from_static("claude"),
            dir.path(),
        );
        let ctx = host.context();
        let id = account("acct-bridge");
        let now = at(1_800_000_000);
        let quota =
            PluginError::quota_status(429, "You've hit your weekly limit").with_reset_after(WEEK);
        note_bridge(ctx, &id, "claude-opus-4", &quota, now);
        assert_eq!(kept_until(ctx, &id, now), Some(now + WEEK));
        let rate = PluginError::status_error(429, "Rate limited. Please try again later.");
        let other = account("acct-rate");
        note_bridge(ctx, &other, "claude-opus-4", &rate, now);
        assert!(kept_until(ctx, &other, now).is_none());
    }

    fn active_home(root: &std::path::Path) -> (PathBuf, AccountId) {
        let home = root.join("agent");
        fs::create_dir_all(home.join("projects").join("work")).expect("projects");
        let uuid = "4f6c1a52-1111-4222-8333-944455556666";
        fs::write(
            home.join(".claude.json"),
            format!(
                r#"{{"oauthAccount":{{"accountUuid":"{uuid}","emailAddress":"ada@example.test"}}}}"#
            ),
        )
        .expect("profile");
        fs::write(home.join("settings.json"), r#"{"model":"opus"}"#).expect("settings");
        let id = hofund_plugin_sdk::oauth::account_id_from(&["claude", uuid]);
        (home, id)
    }

    fn refusal_line(at: SystemTime, resets_at: u64, said: &str) -> String {
        let stamp = hofund_plugin_sdk::time::format_rfc3339(at);
        format!(
            r#"{{"type":"assistant","timestamp":"{stamp}","message":{{"model":"claude-opus-4","content":[{{"type":"text","text":"{said}"}}]}},"error":"rate_limit","isApiErrorMessage":true,"quotaLimits":{{"status":"rejected","resetsAt":{resets_at},"rateLimitType":"seven_day"}}}}"#
        )
    }

    #[test]
    fn d254_a_session_refusal_is_kept_only_after_it_settles() {
        let _guards = guards();
        let dir = tempfile::tempdir().expect("dir");
        let host = hofund_kit_subscription::TestHost::open_for(
            hofund_plugin_api::PluginKind::Subscription,
            hofund_plugin_api::PluginId::from_static("claude"),
            dir.path(),
        );
        let ctx = host.context();
        let (home, id) = active_home(dir.path());
        crate::cli::test_configured_dir(Some(home.clone()));
        let session = home.join("projects").join("work").join("session.jsonl");
        let now = at(1_800_000_000);
        fs::write(&session, "{}\n").expect("seed");
        observe_agent(ctx, &id, now);
        let early = refusal_line(
            now + Duration::from_secs(30),
            1_900_000_000,
            "You've hit your weekly limit",
        );
        let mut file = fs::OpenOptions::new()
            .append(true)
            .open(&session)
            .expect("append");
        writeln!(file, "{early}").expect("append");
        drop(file);
        observe_agent(ctx, &id, now);
        assert!(
            kept_until(ctx, &id, now).is_none(),
            "a refusal inside the settle minute is not kept"
        );
        let settled = refusal_line(
            now + Duration::from_secs(120),
            1_900_000_000,
            "You've hit your weekly limit",
        );
        let mut file = fs::OpenOptions::new()
            .append(true)
            .open(&session)
            .expect("append");
        writeln!(file, "{settled}").expect("append");
        observe_agent(ctx, &id, now);
        assert_eq!(
            kept_until(ctx, &id, now),
            Some(UNIX_EPOCH + Duration::from_secs(1_900_000_000))
        );
        let killed = PluginError::transport("Claude Code: signal: killed");
        let spent = spent_on_failure(ctx, &id, &killed, now).expect("spent");
        assert!(spent.iter().any(|window| window.held()));
    }

    #[test]
    fn d254_default_home_keeps_a_settled_refusal_and_spends_a_failed_read() {
        let _guards = guards();
        assert!(
            crate::cli::configured_dir().is_none(),
            "the configured-dir override stays unset"
        );
        let dir = tempfile::tempdir().expect("dir");
        let host = hofund_kit_subscription::TestHost::open_for(
            hofund_plugin_api::PluginKind::Subscription,
            hofund_plugin_api::PluginId::from_static("claude"),
            dir.path(),
        );
        let ctx = host.context();
        let (home, id) = active_home(dir.path());
        crate::cli::test_default_home(Some(home.clone()));
        assert!(crate::usage_cadence::is_active_account(&id));
        let session = home.join("projects").join("work").join("session.jsonl");
        let now = at(1_800_000_000);
        fs::write(&session, "{}\n").expect("seed");
        observe_agent(ctx, &id, now);
        let settled = refusal_line(
            now + Duration::from_secs(120),
            1_900_000_000,
            "You've hit your weekly limit",
        );
        let mut file = fs::OpenOptions::new()
            .append(true)
            .open(&session)
            .expect("append");
        writeln!(file, "{settled}").expect("append");
        drop(file);
        observe_agent(ctx, &id, now);
        assert_eq!(
            kept_until(ctx, &id, now),
            Some(UNIX_EPOCH + Duration::from_secs(1_900_000_000))
        );
        let killed = PluginError::transport("Claude Code: signal: killed");
        let spent = spent_on_failure(ctx, &id, &killed, now).expect("spent");
        assert!(spent.iter().any(|window| window.held()));
    }

    #[test]
    fn d254_a_routed_session_is_not_kept_but_still_spends_a_failed_read() {
        let _guards = guards();
        let dir = tempfile::tempdir().expect("dir");
        let host = hofund_kit_subscription::TestHost::open_for(
            hofund_plugin_api::PluginKind::Subscription,
            hofund_plugin_api::PluginId::from_static("claude"),
            dir.path(),
        );
        let ctx = host.context();
        let (home, id) = active_home(dir.path());
        fs::write(
            home.join("settings.json"),
            r#"{"model":"opus","env":{"ANTHROPIC_BASE_URL":"http://127.0.0.1:9"}}"#,
        )
        .expect("routed");
        crate::cli::test_configured_dir(Some(home.clone()));
        let session = home.join("projects").join("work").join("session.jsonl");
        let now = at(1_800_000_000);
        fs::write(&session, "{}\n").expect("seed");
        observe_agent(ctx, &id, now);
        let settled = refusal_line(
            now + Duration::from_secs(120),
            1_900_000_000,
            "You've hit your weekly limit",
        );
        let mut file = fs::OpenOptions::new()
            .append(true)
            .open(&session)
            .expect("append");
        writeln!(file, "{settled}").expect("append");
        observe_agent(ctx, &id, now);
        assert!(kept_until(ctx, &id, now).is_none());
        let killed = PluginError::transport("Claude Code: signal: killed");
        let spent = spent_on_failure(ctx, &id, &killed, now).expect("spent");
        assert!(spent.iter().any(|window| window.held()));
    }

    #[test]
    fn d254_another_accounts_reset_is_not_kept_again() {
        let _guards = guards();
        let dir = tempfile::tempdir().expect("dir");
        let host = hofund_kit_subscription::TestHost::open_for(
            hofund_plugin_api::PluginKind::Subscription,
            hofund_plugin_api::PluginId::from_static("claude"),
            dir.path(),
        );
        let ctx = host.context();
        let (home, id) = active_home(dir.path());
        crate::cli::test_configured_dir(Some(home.clone()));
        let now = at(1_800_000_000);
        let until = UNIX_EPOCH + Duration::from_secs(1_900_000_000);
        let path = limits_path(ctx).expect("path");
        keep(
            &path,
            &account("other-account"),
            None,
            until,
            "weekly",
            now,
            true,
        );
        let session = home.join("projects").join("work").join("session.jsonl");
        fs::write(&session, "{}\n").expect("seed");
        observe_agent(ctx, &id, now);
        let settled = refusal_line(now + Duration::from_secs(120), 1_900_000_000, "weekly");
        let mut file = fs::OpenOptions::new()
            .append(true)
            .open(&session)
            .expect("append");
        writeln!(file, "{settled}").expect("append");
        observe_agent(ctx, &id, now);
        assert!(kept_until(ctx, &id, now).is_none());
    }

    #[test]
    fn d254_a_full_family_window_stays_after_a_good_read() {
        let _guards = guards();
        let dir = tempfile::tempdir().expect("dir");
        let host = hofund_kit_subscription::TestHost::open_for(
            hofund_plugin_api::PluginKind::Subscription,
            hofund_plugin_api::PluginId::from_static("claude"),
            dir.path(),
        );
        let ctx = host.context();
        let id = account("acct-full");
        let now = at(1_800_000_000);
        note_text(
            ctx,
            &id,
            "You've hit your Fable limit · resets Nov 24, 2027 at 5am (UTC)",
            now,
        );
        let full = QuotaWindow::new("7 days · Fable", 100, Some(100), Some(now + WEEK))
            .expect("window")
            .with_scope_model("fable");
        let open =
            QuotaWindow::new("5 hours", 10, Some(100), Some(now + FIVE_HOURS)).expect("open");
        clear_good_read(ctx, &id, &[open, full], now);
        let home = dir.path().join("agent-fable");
        fs::create_dir_all(&home).expect("home");
        fs::write(home.join("settings.json"), r#"{"model":"fable"}"#).expect("settings");
        crate::cli::test_configured_dir(Some(home));
        assert!(kept_until(ctx, &id, now).is_some());
    }
}
