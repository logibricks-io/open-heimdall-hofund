//! Usage breakdown by agent, provider key, or gateway key.

use super::{Ledger, Totals, UsageError};

/// Stored when no client agent was identified.
pub const UNKNOWN_CLIENT_AGENT: &str = "unknown";

/// Group id for rows with no authenticated gateway key.
pub const NO_GATEWAY_KEY: &str = "none";

/// Group id for rows with no provider API key fingerprint.
pub const NO_PROVIDER_KEY: &str = "none";

/// What dimension a breakdown query groups by.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum BreakdownDimension {
    /// Client agent id stored on the row.
    Agent,
    /// Five-byte provider key fingerprint.
    ProviderKey,
    /// Authenticated gateway key id.
    GatewayKey,
}

/// One group's totals in a breakdown report.
#[derive(Clone, Debug, PartialEq)]
pub struct BreakdownGroup {
    /// Stable group id for filtering.
    pub id: String,
    /// Human-readable label (masked for provider keys).
    pub label: String,
    /// Aggregated counts for the group.
    pub totals: Totals,
}

/// Normalizes a stored client agent, including legacy empty values.
#[must_use]
pub fn normalize_client_agent(value: &str) -> String {
    if value.trim().is_empty() {
        return UNKNOWN_CLIENT_AGENT.to_owned();
    }
    value.to_owned()
}

/// Display label for a provider key fingerprint without revealing a secret.
#[must_use]
pub fn masked_provider_key_id(id: &str) -> String {
    if id.is_empty() {
        return NO_PROVIDER_KEY.to_owned();
    }
    let chars: Vec<char> = id.chars().collect();
    if chars.len() <= 4 {
        return id.to_owned();
    }
    let suffix: String = chars[chars.len() - 4..].iter().collect();
    format!("…{suffix}")
}

impl Ledger {
    /// Groups rows at or after `since` by `dimension`, ordered by token volume.
    ///
    /// # Errors
    ///
    /// Returns [`UsageError`] when a query fails.
    pub fn breakdown(
        &self,
        since: Option<i64>,
        dimension: BreakdownDimension,
    ) -> Result<Vec<BreakdownGroup>, UsageError> {
        let since = since.unwrap_or(i64::MIN);
        let connection = self
            .connection
            .lock()
            .map_err(|_| UsageError("usage database lock is poisoned".to_owned()))?;
        let unknown = sql_string(UNKNOWN_CLIENT_AGENT);
        let no_provider = sql_string(NO_PROVIDER_KEY);
        let no_gateway = sql_string(NO_GATEWAY_KEY);
        let (group_expr, label_expr) = match dimension {
            BreakdownDimension::Agent => (
                format!("COALESCE(NULLIF(TRIM(client_agent), ''), {unknown})"),
                format!("COALESCE(NULLIF(TRIM(client_agent), ''), {unknown})"),
            ),
            BreakdownDimension::ProviderKey => (
                format!(
                    "CASE WHEN provider_key_id = '' THEN {no_provider} ELSE provider_key_id END"
                ),
                format!(
                    "CASE WHEN provider_key_id = '' THEN {no_provider} ELSE provider_key_id END"
                ),
            ),
            BreakdownDimension::GatewayKey => (
                format!("CASE WHEN caller_key_id = '' THEN {no_gateway} ELSE caller_key_id END"),
                "MAX(caller_key_name)".to_owned(),
            ),
        };
        let sql = format!(
            "SELECT {group_expr}, {label_expr},
                    COUNT(CASE WHEN outcome NOT IN ('handed_on', 'refusal', 'turned_away') OR outcome = '' THEN 1 END),
                    COALESCE(SUM(MAX(input_tokens, 0)), 0),
                    COALESCE(SUM(MAX(output_tokens, 0)), 0),
                    COALESCE(SUM(MAX(cache_read_tokens, 0)), 0),
                    COALESCE(SUM(MAX(cache_write_5m_tokens, 0)), 0),
                    COALESCE(SUM(MAX(cache_write_1h_tokens, 0)), 0),
                    COALESCE(SUM(MAX(reasoning_tokens, 0)), 0),
                    COALESCE(SUM(CASE WHEN priced = 1 THEN cost_usd ELSE 0 END), 0),
                    COALESCE(SUM(CASE WHEN priced = 0 AND (MAX(COALESCE(input_tokens, 0), 0) + MAX(COALESCE(output_tokens, 0), 0)) > 0 THEN 1 ELSE 0 END), 0)
             FROM requests
             WHERE created_unix >= ?1
             GROUP BY {group_expr}
             ORDER BY (COALESCE(SUM(MAX(input_tokens, 0)), 0) + COALESCE(SUM(MAX(output_tokens, 0)), 0)) DESC,
                      COUNT(CASE WHEN outcome NOT IN ('handed_on', 'refusal', 'turned_away') OR outcome = '' THEN 1 END) DESC,
                      {group_expr} ASC"
        );
        let mut statement = connection
            .prepare(&sql)
            .map_err(|error| UsageError(error.to_string()))?;
        let rows = statement
            .query_map([since], |row| {
                let id: String = row.get(0)?;
                let label_hint: String = row.get(1)?;
                let requests: i64 = row.get(2)?;
                let input: i64 = row.get(3)?;
                let output: i64 = row.get(4)?;
                let cache_read: i64 = row.get(5)?;
                let cache_write_5m: i64 = row.get(6)?;
                let cache_write_1h: i64 = row.get(7)?;
                let reasoning: i64 = row.get(8)?;
                let cost: f64 = row.get(9)?;
                let unpriced: i64 = row.get(10)?;
                Ok((
                    id,
                    label_hint,
                    Totals {
                        requests: u64::try_from(requests).unwrap_or(0),
                        input_tokens: u64::try_from(input).unwrap_or(0),
                        output_tokens: u64::try_from(output).unwrap_or(0),
                        cache_read_tokens: u64::try_from(cache_read).unwrap_or(0),
                        cache_write_5m_tokens: u64::try_from(cache_write_5m).unwrap_or(0),
                        cache_write_1h_tokens: u64::try_from(cache_write_1h).unwrap_or(0),
                        reasoning_tokens: u64::try_from(reasoning).unwrap_or(0),
                        cost_usd: cost,
                        unpriced: u64::try_from(unpriced).unwrap_or(0),
                    },
                ))
            })
            .map_err(|error| UsageError(error.to_string()))?;
        let mut groups = Vec::new();
        for row in rows {
            let (id, label_hint, totals) = row.map_err(|error| UsageError(error.to_string()))?;
            let (id, label) = label_for(dimension, &id, &label_hint);
            groups.push(BreakdownGroup { id, label, totals });
        }
        Ok(groups)
    }
}

fn sql_string(value: &str) -> String {
    format!("'{}'", value.replace('\'', "''"))
}

fn label_for(dimension: BreakdownDimension, id: &str, hint: &str) -> (String, String) {
    match dimension {
        BreakdownDimension::Agent => {
            let id = normalize_client_agent(id);
            (id.clone(), id)
        }
        BreakdownDimension::ProviderKey => {
            let id = if id.is_empty() {
                NO_PROVIDER_KEY.to_owned()
            } else {
                id.to_owned()
            };
            let label = masked_provider_key_id(if id == NO_PROVIDER_KEY { "" } else { &id });
            (id, label)
        }
        BreakdownDimension::GatewayKey => {
            let id = if id.is_empty() || id == NO_GATEWAY_KEY {
                NO_GATEWAY_KEY.to_owned()
            } else {
                id.to_owned()
            };
            let label = hint.trim();
            let label = if label.is_empty() {
                id.clone()
            } else {
                label.to_owned()
            };
            (id, label)
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{Row, RowPricing, TokenPrice, UsageDetails};

    fn ledger_with(rows: &[Row]) -> Ledger {
        let dir = tempfile::tempdir().expect("dir");
        let ledger = Ledger::open(&dir.path().join("usage.sqlite")).expect("open");
        for row in rows {
            ledger.record(row).expect("row");
        }
        ledger
    }

    fn base_row() -> Row {
        Row {
            created_unix: 100,
            model: "demo".to_owned(),
            upstream: "primary".to_owned(),
            input_tokens: Some(10),
            output_tokens: Some(5),
            latency_ms: 1,
            status: "200".to_owned(),
            ..Default::default()
        }
    }

    #[test]
    fn u1c_7_old_database_reads_unknown_client_agent() {
        let dir = tempfile::tempdir().expect("dir");
        let path = dir.path().join("legacy.sqlite");
        {
            let connection = rusqlite::Connection::open(&path).expect("open");
            connection
                .execute_batch(
                    "CREATE TABLE requests (
                        id INTEGER PRIMARY KEY,
                        created_unix INTEGER NOT NULL,
                        model TEXT NOT NULL,
                        upstream TEXT NOT NULL,
                        input_tokens INTEGER,
                        output_tokens INTEGER,
                        latency_ms INTEGER NOT NULL,
                        status TEXT NOT NULL,
                        caller_key_id TEXT NOT NULL DEFAULT '',
                        caller_key_name TEXT NOT NULL DEFAULT '',
                        caller_issuer_id TEXT NOT NULL DEFAULT '',
                        provider_key_id TEXT NOT NULL DEFAULT ''
                    );",
                )
                .expect("schema");
            connection
                .execute(
                    "INSERT INTO requests (created_unix, model, upstream, input_tokens, output_tokens, latency_ms, status)
                     VALUES (100, 'demo', 'primary', 1, 2, 1, '200')",
                    [],
                )
                .expect("insert");
        }
        let ledger = Ledger::open(&path).expect("migrate");
        let rows = ledger.rows().expect("rows");
        assert_eq!(rows.len(), 1);
        assert_eq!(rows[0].client_agent, UNKNOWN_CLIENT_AGENT);
        let groups = ledger
            .breakdown(None, BreakdownDimension::Agent)
            .expect("breakdown");
        assert_eq!(groups.len(), 1);
        assert_eq!(groups[0].id, UNKNOWN_CLIENT_AGENT);
        assert_eq!(groups[0].totals.requests, 1);
    }

    #[test]
    fn u1c_8_provider_key_breakdown_masks_ids() {
        let mut first = base_row();
        first.provider_key_id = "aabbccddee".to_owned();
        first.client_agent = "fixture-agent".to_owned();
        let mut second = base_row();
        second.created_unix = 101;
        second.provider_key_id = "0011223344".to_owned();
        second.input_tokens = Some(100);
        let ledger = ledger_with(&[first, second]);
        let groups = ledger
            .breakdown(None, BreakdownDimension::ProviderKey)
            .expect("breakdown");
        assert_eq!(groups.len(), 2);
        assert_eq!(groups[0].id, "0011223344");
        assert_eq!(groups[0].label, "…3344");
        assert_eq!(groups[1].id, "aabbccddee");
        assert_eq!(groups[1].label, "…ddee");
    }

    #[test]
    fn u1c_9_gateway_key_breakdown_uses_key_names() {
        let mut named = base_row();
        named.caller_key_id = "kid-1".to_owned();
        named.caller_key_name = "Ops".to_owned();
        let mut anonymous = base_row();
        anonymous.created_unix = 101;
        anonymous.caller_key_id = String::new();
        let ledger = ledger_with(&[named, anonymous]);
        let groups = ledger
            .breakdown(None, BreakdownDimension::GatewayKey)
            .expect("breakdown");
        assert_eq!(groups.len(), 2);
        assert_eq!(groups[1].id, NO_GATEWAY_KEY);
        assert_eq!(groups[1].label, NO_GATEWAY_KEY);
        assert_eq!(groups[0].id, "kid-1");
        assert_eq!(groups[0].label, "Ops");
    }

    #[test]
    fn u1c_10_breakdown_skips_handed_on_for_request_counts_only() {
        let mut handed = base_row();
        handed.outcome = crate::OUTCOME_HANDED_ON.to_owned();
        let ledger = ledger_with(&[handed]);
        let groups = ledger
            .breakdown(None, BreakdownDimension::Agent)
            .expect("breakdown");
        assert_eq!(groups[0].totals.requests, 0);
    }

    #[test]
    fn u1c_16_breakdown_cost_ignores_unpriced_row_cost() {
        let dir = tempfile::tempdir().expect("dir");
        let ledger = Ledger::open(&dir.path().join("u.sqlite")).expect("open");
        let price = TokenPrice {
            input: 1.0,
            output: 1.0,
            cache_read: 0.0,
            cache_write: 0.0,
            cache_write_1h: 0.0,
        };
        let mut row = base_row();
        row.client_agent = "mixed".to_owned();
        for (priced, cost_usd) in [(true, 1.0), (false, 2.0)] {
            ledger
                .record_with_details(
                    &row,
                    None,
                    UsageDetails::default(),
                    Some(RowPricing {
                        priced,
                        cost_usd,
                        price,
                    }),
                )
                .expect("record");
        }
        let groups = ledger
            .breakdown(None, BreakdownDimension::Agent)
            .expect("groups");
        let mixed = groups
            .iter()
            .find(|group| group.id == "mixed")
            .expect("mixed group");
        assert!((mixed.totals.cost_usd - 1.0).abs() < f64::EPSILON);
        assert_eq!(mixed.totals.unpriced, 1);
    }

    #[test]
    fn u1c_11_unpriced_rows_stay_in_breakdown_totals() {
        let mut priced = base_row();
        priced.client_agent = "priced".to_owned();
        let dir = tempfile::tempdir().expect("dir");
        let ledger = Ledger::open(&dir.path().join("u.sqlite")).expect("open");
        ledger
            .record_with_details(
                &priced,
                None,
                UsageDetails::default(),
                Some(RowPricing {
                    priced: true,
                    cost_usd: 1.0,
                    price: TokenPrice {
                        input: 1.0,
                        output: 1.0,
                        cache_read: 0.0,
                        cache_write: 0.0,
                        cache_write_1h: 0.0,
                    },
                }),
            )
            .expect("priced");
        let mut open = base_row();
        open.client_agent = "open".to_owned();
        ledger.record(&open).expect("open");
        let groups = ledger
            .breakdown(None, BreakdownDimension::Agent)
            .expect("groups");
        assert_eq!(groups.len(), 2);
        let priced_group = groups
            .iter()
            .find(|group| group.id == "priced")
            .expect("priced group");
        assert!((priced_group.totals.cost_usd - 1.0).abs() < f64::EPSILON);
        let open_group = groups
            .iter()
            .find(|group| group.id == "open")
            .expect("open group");
        assert_eq!(open_group.totals.unpriced, 1);
        assert_eq!(open_group.totals.cost_usd, 0.0);
    }
}
