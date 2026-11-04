//! Local calendar day keys for daily check-in persistence.

use std::time::{SystemTime, UNIX_EPOCH};

use chrono::{DateTime, FixedOffset, TimeZone};

/// The fixed offset for `now` in the host's local zone.
pub fn local_offset(now: SystemTime) -> FixedOffset {
    let secs = now.duration_since(UNIX_EPOCH).unwrap_or_default().as_secs();
    let local = chrono::Local
        .timestamp_opt(secs as i64, 0)
        .single()
        .unwrap_or_else(|| chrono::Local.timestamp_opt(0, 0).single().expect("epoch"));
    *local.offset()
}

/// Names the local calendar day for persistence (`YYYY-MM-DD`).
pub fn local_day_key(now: SystemTime, zone: FixedOffset) -> String {
    zoned(now, zone).format("%Y-%m-%d").to_string()
}

fn zoned(time: SystemTime, zone: FixedOffset) -> DateTime<FixedOffset> {
    let secs = time
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default()
        .as_secs();
    zone.timestamp_opt(secs as i64, 0)
        .single()
        .unwrap_or_else(|| zone.timestamp_opt(0, 0).single().expect("epoch"))
}

#[cfg(test)]
mod tests {
    use super::local_day_key;
    use chrono::FixedOffset;
    use std::time::{Duration, UNIX_EPOCH};

    #[test]
    fn a3b_2_once_per_local_day_key() {
        let zone = FixedOffset::east_opt(0).expect("utc");
        let noon = UNIX_EPOCH + Duration::from_secs(86_400);
        assert_eq!(local_day_key(noon, zone), "1970-01-02");
        assert_eq!(
            local_day_key(noon - Duration::from_secs(1), zone),
            "1970-01-01"
        );
    }
}
