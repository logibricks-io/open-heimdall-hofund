use rusqlite::params;

use super::{Ledger, UsageError};

/// Usage totals for one authenticated gateway key.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct CallerTotals {
    /// Number of recorded requests.
    pub calls: u64,
    /// Number of requests whose status is not 2xx.
    pub errors: u64,
    /// Sum of known non-negative input token counts.
    pub input: u64,
    /// Sum of known non-negative output token counts.
    pub output: u64,
}

/// One model's usage totals for an authenticated gateway key.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct CallerModel {
    /// Client-facing model id.
    pub id: String,
    /// This model's totals.
    pub totals: CallerTotals,
}

impl Ledger {
    /// Summarizes only `key_id`'s rows at or after `since`, ranked by call count.
    ///
    /// # Errors
    ///
    /// Returns [`UsageError`] when a query fails.
    pub fn caller_summary(
        &self,
        key_id: &str,
        since: Option<i64>,
    ) -> Result<(CallerTotals, Vec<CallerModel>), UsageError> {
        let since = since.unwrap_or(i64::MIN);
        let connection = self
            .connection
            .lock()
            .map_err(|_| UsageError("usage database lock is poisoned".to_owned()))?;
        let totals = connection
            .query_row(
                "SELECT COUNT(*),
                        COALESCE(SUM(CASE WHEN status NOT LIKE '2%' THEN 1 ELSE 0 END), 0),
                        COALESCE(SUM(MAX(input_tokens, 0)), 0),
                        COALESCE(SUM(MAX(output_tokens, 0)), 0)
                 FROM requests WHERE created_unix >= ?1 AND caller_key_id = ?2",
                params![since, key_id],
                read_totals,
            )
            .map_err(|error| UsageError(error.to_string()))?;
        let mut statement = connection
            .prepare(
                "SELECT model, COUNT(*),
                        COALESCE(SUM(CASE WHEN status NOT LIKE '2%' THEN 1 ELSE 0 END), 0),
                        COALESCE(SUM(MAX(input_tokens, 0)), 0),
                        COALESCE(SUM(MAX(output_tokens, 0)), 0)
                 FROM requests WHERE created_unix >= ?1 AND caller_key_id = ?2
                 GROUP BY model
                 ORDER BY COUNT(*) DESC,
                          SUM(MAX(input_tokens, 0) + MAX(output_tokens, 0)) DESC,
                          model ASC",
            )
            .map_err(|error| UsageError(error.to_string()))?;
        let rows = statement
            .query_map(params![since, key_id], |row| {
                Ok(CallerModel {
                    id: row.get(0)?,
                    totals: read_totals_at(row, 1)?,
                })
            })
            .map_err(|error| UsageError(error.to_string()))?;
        let models = rows
            .collect::<Result<Vec<_>, _>>()
            .map_err(|error| UsageError(error.to_string()))?;
        Ok((totals, models))
    }
}

fn read_totals(row: &rusqlite::Row<'_>) -> rusqlite::Result<CallerTotals> {
    read_totals_at(row, 0)
}

fn read_totals_at(row: &rusqlite::Row<'_>, offset: usize) -> rusqlite::Result<CallerTotals> {
    let count = |index| -> rusqlite::Result<u64> {
        let value: i64 = row.get(index)?;
        Ok(u64::try_from(value).unwrap_or(0))
    };
    Ok(CallerTotals {
        calls: count(offset)?,
        errors: count(offset + 1)?,
        input: count(offset + 2)?,
        output: count(offset + 3)?,
    })
}
