//! U-1a ledger migration and summary totals.

use hofund_usage::{Ledger, Row, Totals, UsageDetails, Window};

#[test]
fn u1a_1_old_database_gains_cache_and_reasoning_columns() {
    let dir = tempfile::tempdir().expect("dir");
    let path = dir.path().join("usage.sqlite");
    rusqlite::Connection::open(&path)
        .expect("old database")
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
                cache_read_tokens INTEGER,
                cache_write_tokens INTEGER
            );
            INSERT INTO requests VALUES (
                1, 10, 'old', 'up', 3, 4, 1, '200', '', 100, 200
            );",
        )
        .expect("seed");
    let ledger = Ledger::open(&path).expect("migrate");
    ledger
        .record_with_details(
            &Row {
                created_unix: 20,
                model: "new".to_owned(),
                upstream: "up".to_owned(),
                input_tokens: Some(1),
                output_tokens: Some(2),
                latency_ms: 1,
                status: "200".to_owned(),
                ..Default::default()
            },
            None,
            UsageDetails {
                cache_read_tokens: Some(5),
                cache_write_tokens: Some(7),
                cache_write_5m_tokens: Some(7),
                cache_write_1h_tokens: Some(0),
                reasoning_tokens: Some(3),
                reasoning_hidden: true,
                ..UsageDetails::default()
            },
            None,
        )
        .expect("insert");
    let (rows, totals) = ledger
        .summary(Window::All.since_now(), 10)
        .expect("summary");
    assert_eq!(rows.len(), 2);
    assert_eq!(rows[1].cache_read_tokens, Some(5));
    assert_eq!(rows[1].cache_write_5m_tokens, Some(7));
    assert_eq!(rows[1].reasoning_tokens, Some(3));
    assert!(rows[1].reasoning_hidden);
    assert_eq!(
        totals,
        Totals {
            requests: 2,
            input_tokens: 4,
            output_tokens: 6,
            cache_read_tokens: 105,
            cache_write_5m_tokens: 7,
            cache_write_1h_tokens: 0,
            reasoning_tokens: 3,
            cost_usd: 0.0,
            unpriced: 2,
        }
    );
}

#[test]
fn u1a_2_summary_totals_aggregate_cache_and_reasoning() {
    let dir = tempfile::tempdir().expect("dir");
    let ledger = Ledger::open(&dir.path().join("usage.sqlite")).expect("open");
    for (at, read, write_5m, write_1h, reasoning) in
        [(1_i64, 10_i64, 3_i64, 0_i64, 1_i64), (2, 0, 0, 5, 2)]
    {
        ledger
            .record_with_details(
                &Row {
                    created_unix: at,
                    model: "m".to_owned(),
                    upstream: "up".to_owned(),
                    input_tokens: Some(1),
                    output_tokens: Some(1),
                    latency_ms: 1,
                    status: "200".to_owned(),
                    ..Default::default()
                },
                None,
                UsageDetails {
                    cache_read_tokens: Some(read),
                    cache_write_tokens: Some(write_5m + write_1h),
                    cache_write_5m_tokens: Some(write_5m),
                    cache_write_1h_tokens: Some(write_1h),
                    reasoning_tokens: Some(reasoning),
                    ..UsageDetails::default()
                },
                None,
            )
            .expect("row");
    }
    let (_, totals) = ledger.summary(Some(0), 10).expect("summary");
    assert_eq!(
        totals,
        Totals {
            requests: 2,
            input_tokens: 2,
            output_tokens: 2,
            cache_read_tokens: 10,
            cache_write_5m_tokens: 3,
            cache_write_1h_tokens: 5,
            reasoning_tokens: 3,
            cost_usd: 0.0,
            unpriced: 2,
        }
    );
}
