//! Parsing `daily-checkin` in subscription options.

use hofund_plugin_api::{ConfigError, RawTable, RawValue};

/// Option key: when true, the daemon may run daily check-in for this subscription.
pub const ENABLED_KEY: &str = "daily-checkin";

/// Reads whether daily check-in is enabled from a subscription options table.
///
/// Absent or false means off. Only an explicit boolean `true` opts in.
///
/// # Errors
///
/// Returns [`ConfigError`] when the key is present but not a boolean.
pub fn read_enabled(raw: &RawTable) -> Result<bool, ConfigError> {
    let Some(value) = raw.get(ENABLED_KEY) else {
        return Ok(false);
    };
    match value {
        RawValue::Boolean(enabled) => Ok(*enabled),
        _ => Err(ConfigError::new("must be a boolean").with_path(ENABLED_KEY)),
    }
}

#[cfg(test)]
mod tests {
    use super::{ENABLED_KEY, read_enabled};
    use hofund_plugin_api::{RawTable, RawValue};

    #[test]
    fn a3b_1_daily_checkin_defaults_off() {
        assert!(!read_enabled(&RawTable::new()).expect("absent"));
        let mut raw = RawTable::new();
        raw.insert(ENABLED_KEY.to_owned(), RawValue::Boolean(false));
        assert!(!read_enabled(&raw).expect("false"));
    }

    #[test]
    fn a3b_12_daily_checkin_accepts_true() {
        let mut raw = RawTable::new();
        raw.insert(ENABLED_KEY.to_owned(), RawValue::Boolean(true));
        assert!(read_enabled(&raw).expect("true"));
        raw.insert(ENABLED_KEY.to_owned(), RawValue::String("true".to_owned()));
        assert!(read_enabled(&raw).is_err());
    }
}
