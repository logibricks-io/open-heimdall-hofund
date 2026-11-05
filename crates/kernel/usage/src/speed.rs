//! Output speed derived from stored timing and flow.

use crate::decode::decode_row;

/// Mean output tokens per second: sum of decode tokens over sum of decode windows.
#[must_use]
pub fn mean_tokens_per_second<'a, I>(rows: I) -> Option<f64>
where
    I: IntoIterator<Item = &'a crate::Row>,
{
    let mut tokens = 0i64;
    let mut window_ms = 0i64;
    for row in rows {
        let (row_tokens, row_window) = decode_row(row);
        if row_tokens <= 0 || row_window <= 0 {
            continue;
        }
        tokens += row_tokens;
        window_ms += row_window;
    }
    if window_ms > 0 {
        Some(tokens as f64 / (window_ms as f64 / 1000.0))
    } else {
        None
    }
}

/// Mean TTFT over rows with a positive streamed measurement.
#[must_use]
pub fn mean_ttft_ms<I>(rows: I) -> Option<f64>
where
    I: IntoIterator<Item = Option<i64>>,
{
    let mut sum = 0.0;
    let mut count = 0.0;
    for ttft in rows {
        let Some(ms) = ttft.filter(|value| *value > 0) else {
            continue;
        };
        sum += ms as f64;
        count += 1.0;
    }
    if count > 0.0 { Some(sum / count) } else { None }
}

#[cfg(test)]
mod tests {
    use super::{mean_tokens_per_second, mean_ttft_ms};
    use crate::Row;

    #[test]
    fn r10_mean_ttft_skips_missing() {
        assert!((mean_ttft_ms([Some(100), None, Some(300)]).expect("mean") - 200.0).abs() < 0.001);
    }

    #[test]
    fn r10_mean_speed_sums_tokens_over_windows() {
        let fast = Row {
            output_tokens: Some(100),
            latency_ms: 2500,
            ttft_ms: Some(500),
            ..Default::default()
        };
        let slow = Row {
            output_tokens: Some(10),
            latency_ms: 1500,
            ttft_ms: Some(500),
            ..Default::default()
        };
        let mean = mean_tokens_per_second([&fast, &slow]).expect("mean");
        assert!((mean - 36.666).abs() < 0.1, "mean={mean}");
    }
}
