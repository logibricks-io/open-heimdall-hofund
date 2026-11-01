//! Parsing `daily-warmup-at-times` in subscription options.

use hofund_plugin_api::{ConfigError, RawTable, RawValue};

/// Option key: local times of day (`06:00`) when daily warm-up may run.
pub const TIMES_KEY: &str = "daily-warmup-at-times";

/// Reads and validates warm-up times from a subscription options table.
///
/// Absent or empty means daily warm-up is off. Times are sorted and
/// deduplicated in local-clock order.
///
/// # Errors
///
/// Returns [`ConfigError`] when the key is present but not a string array
/// of valid clock times.
pub fn read_times(raw: &RawTable) -> Result<Vec<String>, ConfigError> {
    let Some(value) = raw.get(TIMES_KEY) else {
        return Ok(Vec::new());
    };
    let items = match value {
        RawValue::Array(items) => items,
        RawValue::String(one) => {
            let trimmed = one.trim();
            if trimmed.is_empty() {
                return Ok(Vec::new());
            }
            return parse_list(trimmed);
        }
        _ => {
            return Err(ConfigError::new("must be a string or array of times").with_path(TIMES_KEY));
        }
    };
    let mut out = Vec::new();
    for (index, item) in items.iter().enumerate() {
        let RawValue::String(text) = item else {
            return Err(ConfigError::new("each time must be a string")
                .with_path(format!("{TIMES_KEY}[{index}]")));
        };
        let normalized = normalize_time(text).map_err(|message| {
            ConfigError::new(message).with_path(format!("{TIMES_KEY}[{index}]"))
        })?;
        if !out.contains(&normalized) {
            out.push(normalized);
        }
    }
    out.sort();
    Ok(out)
}

fn parse_list(text: &str) -> Result<Vec<String>, ConfigError> {
    let mut out = Vec::new();
    for (index, part) in text.split(',').enumerate() {
        let normalized = normalize_time(part).map_err(|message| {
            ConfigError::new(message).with_path(format!("{TIMES_KEY}[{index}]"))
        })?;
        if !out.contains(&normalized) {
            out.push(normalized);
        }
    }
    out.sort();
    Ok(out)
}

/// Validates one local time of day like `06:00` or `9:05`.
///
/// # Errors
///
/// Returns a static English message when the text is not a valid clock time.
pub fn normalize_time(text: &str) -> Result<String, &'static str> {
    let text = text.trim();
    if text.is_empty() {
        return Err("a time of day must look like 06:00");
    }
    let (hour, minute) = match text.split_once(':') {
        Some((h, m)) => (h.trim(), m.trim()),
        None => return Err("a time of day must look like 06:00"),
    };
    if minute.len() > 2 || minute.chars().any(|ch| !ch.is_ascii_digit()) {
        return Err("a time of day must look like 06:00");
    }
    if hour.is_empty() || hour.len() > 2 || hour.chars().any(|ch| !ch.is_ascii_digit()) {
        return Err("a time of day must look like 06:00");
    }
    let hour: u32 = hour
        .parse()
        .map_err(|_| "a time of day must look like 06:00")?;
    let minute: u32 = minute
        .parse()
        .map_err(|_| "a time of day must look like 06:00")?;
    if hour > 23 || minute > 59 {
        return Err("a time of day must look like 06:00");
    }
    Ok(format!("{hour:02}:{minute:02}"))
}

#[cfg(test)]
mod tests {
    use super::{TIMES_KEY, normalize_time, read_times};
    use hofund_plugin_api::{RawTable, RawValue};

    #[test]
    fn a3a_12_config_rejects_bad_time() {
        assert!(normalize_time("25:00").is_err());
        assert!(normalize_time("06:60").is_err());
        assert_eq!(normalize_time(" 9:5 ").expect("ok"), "09:05");
        let mut raw = RawTable::new();
        raw.insert(
            TIMES_KEY.to_owned(),
            RawValue::Array(vec![RawValue::String("25:00".to_owned())]),
        );
        assert!(read_times(&raw).is_err());
    }

    #[test]
    fn a3a_12_config_accepts_sorted_times() {
        let mut raw = RawTable::new();
        raw.insert(
            TIMES_KEY.to_owned(),
            RawValue::Array(vec![
                RawValue::String("19:10".to_owned()),
                RawValue::String("9:00".to_owned()),
                RawValue::String("09:00".to_owned()),
            ]),
        );
        let times = read_times(&raw).expect("valid");
        assert_eq!(times, ["09:00", "19:10"]);
    }
}
