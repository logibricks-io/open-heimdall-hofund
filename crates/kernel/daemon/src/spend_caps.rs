use std::collections::BTreeMap;
use std::io::Write;
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};
use std::time::SystemTime;

use hofund_config::Provider;
use hofund_usage::{KeySpent, SpendCapPeriod, spend_cap_retry_after, spend_cap_since};
use serde::{Deserialize, Serialize};

use crate::provider_keys::{self, ResolvedKey};

const MAX_USD: f64 = 1_000_000.0;

/// Parsed spend-cap limit from CLI or control input.
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct ParsedSpendCap {
    /// Positive USD ceiling.
    pub usd: f64,
    /// Reporting window.
    pub period: SpendCapPeriod,
}

/// One stored cap entry.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct StoredSpendCap {
    pub provider: String,
    pub key_id: String,
    pub usd: f64,
    pub period: StoredPeriod,
    pub set_at: i64,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum StoredPeriod {
    Day,
    Month,
    Total,
}

impl From<SpendCapPeriod> for StoredPeriod {
    fn from(value: SpendCapPeriod) -> Self {
        match value {
            SpendCapPeriod::Day => Self::Day,
            SpendCapPeriod::Month => Self::Month,
            SpendCapPeriod::Total => Self::Total,
        }
    }
}

impl From<StoredPeriod> for SpendCapPeriod {
    fn from(value: StoredPeriod) -> Self {
        match value {
            StoredPeriod::Day => Self::Day,
            StoredPeriod::Month => Self::Month,
            StoredPeriod::Total => Self::Total,
        }
    }
}

#[derive(Clone, Debug, Default, PartialEq, Serialize, Deserialize)]
struct SpendCapFile {
    #[serde(default)]
    caps: Vec<StoredSpendCap>,
}

pub(crate) struct SpendCapStore {
    path: PathBuf,
    caps: Mutex<Vec<StoredSpendCap>>,
    last_written: Mutex<Option<Vec<StoredSpendCap>>>,
}

impl SpendCapStore {
    pub(crate) fn new(config_path: &Path) -> Self {
        let root = crate::beside(config_path, ".hofund");
        Self {
            path: root.join("spend-caps.json"),
            caps: Mutex::new(Vec::new()),
            last_written: Mutex::new(None),
        }
    }

    pub(crate) fn load(&self) {
        let bytes = match std::fs::read(&self.path) {
            Ok(bytes) => bytes,
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => {
                *self.caps.lock().expect("caps") = Vec::new();
                *self.last_written.lock().expect("last") = Some(Vec::new());
                return;
            }
            Err(error) => {
                tracing::warn!(path = %self.path.display(), %error, "spend caps were not read");
                *self.caps.lock().expect("caps") = Vec::new();
                *self.last_written.lock().expect("last") = Some(Vec::new());
                return;
            }
        };
        match serde_json::from_slice::<SpendCapFile>(&bytes) {
            Ok(file) => {
                *self.caps.lock().expect("caps") = file.caps;
                *self.last_written.lock().expect("last") =
                    Some(self.caps.lock().expect("caps").clone());
            }
            Err(error) => {
                tracing::warn!(path = %self.path.display(), %error, "spend caps file is corrupt");
                *self.caps.lock().expect("caps") = Vec::new();
                *self.last_written.lock().expect("last") = Some(Vec::new());
            }
        }
    }

    pub(crate) fn list(&self) -> Vec<StoredSpendCap> {
        self.caps.lock().expect("caps").clone()
    }

    /// Validates and stores a cap for one key.
    ///
    /// # Errors
    ///
    /// Returns a message when validation fails.
    pub(crate) fn set_key_spend_cap(
        &self,
        provider: &str,
        key_id: &str,
        parsed: ParsedSpendCap,
    ) -> Result<(), String> {
        validate_cap(parsed)?;
        let set_at = SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map_or(0, |duration| i64::try_from(duration.as_secs()).unwrap_or(0));
        let entry = StoredSpendCap {
            provider: provider.to_owned(),
            key_id: key_id.to_owned(),
            usd: parsed.usd,
            period: parsed.period.into(),
            set_at,
        };
        let mut caps = self.caps.lock().expect("caps");
        if let Some(existing) = caps
            .iter_mut()
            .find(|cap| cap.provider == provider && cap.key_id == key_id)
        {
            *existing = entry;
        } else {
            caps.push(entry);
        }
        self.save_locked(&caps)
    }

    pub(crate) fn clear_key(&self, provider: &str, key_id: &str) -> Result<bool, String> {
        let mut caps = self.caps.lock().expect("caps");
        let before = caps.len();
        caps.retain(|cap| cap.provider != provider || cap.key_id != key_id);
        self.save_locked(&caps)?;
        Ok(caps.len() < before)
    }

    fn save_locked(&self, caps: &[StoredSpendCap]) -> Result<(), String> {
        if self
            .last_written
            .lock()
            .expect("last")
            .as_ref()
            .is_some_and(|last| last == caps)
        {
            return Ok(());
        }
        if caps.is_empty() && !self.path.exists() {
            *self.last_written.lock().expect("last") = Some(Vec::new());
            return Ok(());
        }
        let bytes = serde_json::to_vec(&SpendCapFile {
            caps: caps.to_vec(),
        })
        .map_err(|error| error.to_string())?;
        self.write_atomic(&bytes)
            .map_err(|error| error.to_string())?;
        *self.last_written.lock().expect("last") = Some(caps.to_vec());
        Ok(())
    }

    fn write_atomic(&self, bytes: &[u8]) -> std::io::Result<()> {
        let parent = self.path.parent().unwrap_or_else(|| Path::new("."));
        std::fs::create_dir_all(parent)?;
        let temporary = self.path.with_file_name(format!(
            "{}.tmp",
            self.path
                .file_name()
                .and_then(|name| name.to_str())
                .unwrap_or("spend-caps.json")
        ));
        let result = (|| {
            let mut options = std::fs::OpenOptions::new();
            options.write(true).create(true).truncate(true);
            #[cfg(unix)]
            std::os::unix::fs::OpenOptionsExt::mode(&mut options, 0o600);
            let mut file = options.open(&temporary)?;
            file.write_all(bytes)?;
            file.sync_all()?;
            std::fs::rename(&temporary, &self.path)
        })();
        if result.is_err() {
            let _ = std::fs::remove_file(&temporary);
        }
        result
    }
}

/// Parses `amount/period` such as `50/day`, `200/month`, or `1000/total`.
///
/// # Errors
///
/// Returns a message when the text is invalid.
pub fn parse_spend_cap(text: &str) -> Result<ParsedSpendCap, String> {
    let (amount, period) = text
        .split_once('/')
        .ok_or_else(|| "a spend cap looks like 50/day, 200/month, or 1000/total".to_owned())?;
    let usd: f64 = amount
        .trim()
        .parse()
        .map_err(|_| format!("`{amount}` is not a dollar amount"))?;
    validate_amount(usd)?;
    let period = match period.trim().to_ascii_lowercase().as_str() {
        "day" => SpendCapPeriod::Day,
        "month" => SpendCapPeriod::Month,
        "total" => SpendCapPeriod::Total,
        other => return Err(format!("`{other}` is not day, month, or total")),
    };
    Ok(ParsedSpendCap { usd, period })
}

fn validate_cap(parsed: ParsedSpendCap) -> Result<(), String> {
    validate_amount(parsed.usd)
}

fn validate_amount(usd: f64) -> Result<(), String> {
    if !(usd > 0.0 && usd <= MAX_USD) {
        return Err(format!(
            "a spend cap must be greater than 0 and at most {MAX_USD} USD"
        ));
    }
    Ok(())
}

/// Routing gate shared across published snapshots.
pub(crate) struct SpendCapGate {
    store: Arc<SpendCapStore>,
    spent: Arc<KeySpent>,
    routes: Mutex<BTreeMap<String, RouteCap>>,
}

struct RouteCap {
    provider: String,
    key_id: String,
    usd: f64,
    period: SpendCapPeriod,
    set_at: i64,
}

impl SpendCapGate {
    pub(crate) fn new(store: Arc<SpendCapStore>, spent: Arc<KeySpent>) -> Self {
        Self {
            store,
            spent,
            routes: Mutex::new(BTreeMap::new()),
        }
    }

    pub(crate) fn refresh(&self, providers: &[Provider]) {
        let caps = self.store.list();
        let mut routes = BTreeMap::new();
        for provider in providers {
            let Ok(keys) = provider_keys::resolve(provider) else {
                continue;
            };
            let several = keys.len() > 1
                || provider
                    .key_entries()
                    .iter()
                    .filter(|entry| entry.on)
                    .count()
                    > 1;
            for key in keys {
                if !key.entry.on {
                    continue;
                }
                let Some(stored) = caps
                    .iter()
                    .find(|cap| cap.provider == provider.id && cap.key_id == key.id)
                else {
                    continue;
                };
                let route_id = if several {
                    provider_keys::built_id(&provider.id, &key.id)
                } else {
                    provider.id.clone()
                };
                routes.insert(
                    route_id,
                    RouteCap {
                        provider: provider.id.clone(),
                        key_id: key.id.clone(),
                        usd: stored.usd,
                        period: stored.period.into(),
                        set_at: stored.set_at,
                    },
                );
            }
        }
        *self.routes.lock().expect("routes") = routes;
    }

    pub(crate) fn skip_route(&self, route_id: &str) -> bool {
        let routes = self.routes.lock().expect("routes");
        let Some(spec) = routes.get(route_id) else {
            return false;
        };
        self.capped(spec)
    }

    pub(crate) fn retry_after_for_routes(&self, route_ids: &[&str]) -> Option<u64> {
        let routes = self.routes.lock().expect("routes");
        let mut best: Option<u64> = None;
        for route_id in route_ids {
            let Some(spec) = routes.get(*route_id) else {
                continue;
            };
            if !self.capped(spec) {
                continue;
            }
            let retry = spend_cap_retry_after(spec.period);
            best = match (best, retry) {
                (None, None) => None,
                (Some(left), None) => Some(left),
                (None, Some(right)) => Some(right),
                (Some(left), Some(right)) => Some(left.max(right)),
            };
        }
        best
    }

    pub(crate) fn all_routes_capped<'a>(
        &self,
        route_ids: impl IntoIterator<Item = &'a str>,
    ) -> bool {
        let ids: Vec<_> = route_ids.into_iter().collect();
        !ids.is_empty() && ids.iter().all(|route_id| self.skip_route(route_id))
    }

    fn capped(&self, spec: &RouteCap) -> bool {
        let since = spend_cap_since(spec.period, spec.set_at);
        match self.spent.usd(&spec.key_id, since) {
            Ok(spent) => spent >= spec.usd,
            Err(error) => {
                tracing::warn!(
                    provider = %spec.provider,
                    key = %spec.key_id,
                    %error,
                    "spend total was not read"
                );
                false
            }
        }
    }
}

pub(crate) fn ensure_key_exists(provider: &Provider, key_id: &str) -> Result<ResolvedKey, String> {
    let keys = provider_keys::resolve(provider).map_err(|error| error.to_string())?;
    keys.into_iter()
        .find(|key| key.id == key_id)
        .ok_or_else(|| format!("provider `{}` has no key `{key_id}`", provider.id))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn d61_3_parse_spend_cap_accepts_day_month_total() {
        let day = parse_spend_cap("12.5/day").expect("day");
        assert!((day.usd - 12.5).abs() < 1e-9);
        assert_eq!(day.period, SpendCapPeriod::Day);
        assert_eq!(
            parse_spend_cap("1/month").expect("month").period,
            SpendCapPeriod::Month
        );
        assert_eq!(
            parse_spend_cap("999/total").expect("total").period,
            SpendCapPeriod::Total
        );
    }

    #[test]
    fn d61_4_parse_spend_cap_rejects_out_of_range_and_bad_period() {
        assert!(parse_spend_cap("0/day").is_err());
        assert!(parse_spend_cap("1000001/total").is_err());
        assert!(parse_spend_cap("5/week").is_err());
        assert!(parse_spend_cap("nope").is_err());
    }

    #[test]
    fn d61_5_store_writes_atomically_beside_hofund() {
        let root = tempfile::tempdir().expect("dir");
        let config = root.path().join("hofund.toml");
        std::fs::write(&config, "[gateway]\n").expect("config");
        let store = SpendCapStore::new(&config);
        store
            .set_key_spend_cap(
                "primary",
                "0123456789",
                ParsedSpendCap {
                    usd: 25.0,
                    period: SpendCapPeriod::Day,
                },
            )
            .expect("set");
        let path = root.path().join(".hofund/spend-caps.json");
        assert!(path.exists());
        assert!(!root.path().join(".hofund/spend-caps.json.tmp").exists());
        let loaded = SpendCapStore::new(&config);
        loaded.load();
        assert_eq!(loaded.list().len(), 1);
        assert!(loaded.clear_key("primary", "0123456789").expect("off"));
    }
}
