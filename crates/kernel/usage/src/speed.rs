//! Output speed derived from stored timing and flow.

use crate::decode::row_tokens_per_second;

/// Weighted mean output tokens per second over rows that yield a speed.
#[must_use]
pub fn mean_tokens_per_second<'a, I>(rows: I) -> Option<f64>
where
    I: IntoIterator<Item = &'a crate::Row>,
{
    let mut sum = 0.0;
    let mut weight = 0.0;
    for row in rows {
        let Some(speed) = row_tokens_per_second(row) else {
            continue;
        };
        let w = row.output_tokens.filter(|tokens| *tokens > 0).unwrap_or(1) as f64;
        sum += speed * w;
        weight += w;
    }
    if weight > 0.0 {
        Some(sum / weight)
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
    fn r10_mean_speed_uses_decode_of() {
        let steady = Row {
            output_tokens: Some(100),
            latency_ms: 2500,
            ttft_ms: Some(500),
            ..Default::default()
        };
        let mean = mean_tokens_per_second([&steady]).expect("mean speed");
        assert!((mean - 50.0).abs() < 0.001);
    }
}
