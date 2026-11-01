//! USD storage with optional CNY display conversion.

use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};
use std::time::{SystemTime, UNIX_EPOCH};

use serde::{Deserialize, Serialize};

/// Display currency for usage summaries.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub enum DisplayCurrency {
    /// Show stored USD amounts.
    #[default]
    Usd,
    /// Convert USD totals with a fixed or cached rate.
    Cny,
}

impl DisplayCurrency {
    /// Parses `usd` or `cny`, case-insensitive.
    #[must_use]
    pub fn parse(value: &str) -> Option<Self> {
        match value.trim().to_ascii_lowercase().as_str() {
            "" | "usd" => Some(Self::Usd),
            "cny" | "rmb" => Some(Self::Cny),
            _ => None,
        }
    }

    /// Wire label for responses and tables.
    #[must_use]
    pub const fn label(self) -> &'static str {
        match self {
            Self::Usd => "USD",
            Self::Cny => "CNY",
        }
    }
}

/// A USD→CNY snapshot.
#[derive(Clone, Copy, Debug, PartialEq, Serialize, Deserialize)]
pub struct CnyRate {
    /// Yuan per one US dollar.
    pub cny_per_usd: f64,
    /// Unix seconds when the rate was learned; zero for a configured fallback.
    pub learned_unix: i64,
}

impl CnyRate {
    /// Default offline fallback, matching the reference fixed default.
    pub const FALLBACK: f64 = 7.2;

    /// Whether the snapshot is older than twelve hours.
    #[must_use]
    pub fn stale(self, now: SystemTime) -> bool {
        if self.learned_unix == 0 {
            return true;
        }
        let Ok(age) = now.duration_since(UNIX_EPOCH) else {
            return true;
        };
        age.as_secs().saturating_sub(self.learned_unix as u64) >= 12 * 3600
    }
}

/// Fetches a live USD→CNY rate for production refresh; tests inject a stub.
pub trait RateFetcher: Send + Sync {
    /// Returns a fresh rate or an error when the endpoint is unreachable.
    fn fetch(&self) -> Result<CnyRate, String>;
}

/// Fixed and cached USD→CNY conversion for usage display.
pub struct Fx {
    fixed: Option<f64>,
    cache_path: PathBuf,
    mem: Mutex<Option<CnyRate>>,
    fetcher: Option<Arc<dyn RateFetcher>>,
}

impl Fx {
    /// Opens or creates conversion state beside `data_root`.
    #[must_use]
    pub fn open(data_root: &Path, fixed_cny_per_usd: Option<f64>, fetcher: Option<Arc<dyn RateFetcher>>) -> Self {
        Self {
            fixed: fixed_cny_per_usd,
            cache_path: data_root.join(".hofund/cache/fxrate.json"),
            mem: Mutex::new(None),
            fetcher,
        }
    }

    /// Rate for display conversion: configured fixed value wins, then cache,
    /// then an optional live fetch when a fetcher is installed.
    pub fn rate(&self) -> CnyRate {
        if let Some(fixed) = self.fixed.filter(|rate| rate.is_finite() && *rate > 0.0) {
            return CnyRate {
                cny_per_usd: fixed,
                learned_unix: 0,
            };
        }
        if let Ok(guard) = self.mem.lock()
            && let Some(rate) = *guard
            && !rate.stale(SystemTime::now())
        {
            return rate;
        }
        if let Ok(rate) = read_cache(&self.cache_path)
            && !rate.stale(SystemTime::now())
        {
            if let Ok(mut guard) = self.mem.lock() {
                *guard = Some(rate);
            }
            return rate;
        }
        if let Some(fetcher) = &self.fetcher
            && let Ok(rate) = fetcher.fetch()
        {
            let _ = write_cache(&self.cache_path, rate);
            if let Ok(mut guard) = self.mem.lock() {
                *guard = Some(rate);
            }
            return rate;
        }
        if let Ok(rate) = read_cache(&self.cache_path) {
            return rate;
        }
        CnyRate {
            cny_per_usd: CnyRate::FALLBACK,
            learned_unix: 0,
        }
    }

    /// Converts a USD amount for display.
    #[must_use]
    pub fn convert_usd(&self, currency: DisplayCurrency, usd: f64) -> (f64, CnyRate) {
        let rate = self.rate();
        let amount = match currency {
            DisplayCurrency::Usd => usd,
            DisplayCurrency::Cny => usd * rate.cny_per_usd,
        };
        (amount, rate)
    }
}

fn read_cache(path: &Path) -> Result<CnyRate, String> {
    let text = std::fs::read_to_string(path).map_err(|error| error.to_string())?;
    serde_json::from_str(&text).map_err(|error| error.to_string())
}

fn write_cache(path: &Path, rate: CnyRate) -> Result<(), String> {
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent).map_err(|error| error.to_string())?;
    }
    let text = serde_json::to_string(&rate).map_err(|error| error.to_string())?;
    std::fs::write(path, text).map_err(|error| error.to_string())
}

#[cfg(test)]
mod tests {
    use super::*;

    struct StubFetcher(f64);

    impl RateFetcher for StubFetcher {
        fn fetch(&self) -> Result<CnyRate, String> {
            Ok(CnyRate {
                cny_per_usd: self.0,
                learned_unix: 1,
            })
        }
    }

    #[test]
    fn fixed_rate_skips_fetcher() {
        let dir = tempfile::tempdir().expect("dir");
        let fx = Fx::open(dir.path(), Some(6.5), Some(Arc::new(StubFetcher(9.0))));
        assert!((fx.rate().cny_per_usd - 6.5).abs() < f64::EPSILON);
    }

    #[test]
    fn fetcher_populates_cache_without_network_in_other_tests() {
        let dir = tempfile::tempdir().expect("dir");
        let fx = Fx::open(dir.path(), None, Some(Arc::new(StubFetcher(8.0))));
        assert!((fx.rate().cny_per_usd - 8.0).abs() < f64::EPSILON);
        let fx2 = Fx::open(dir.path(), None, None);
        assert!((fx2.rate().cny_per_usd - 8.0).abs() < f64::EPSILON);
    }
}
