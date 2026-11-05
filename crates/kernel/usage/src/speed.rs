//! Output speed derived from stored latency and time to first content.

/// Tokens per second while the model was writing output.
///
/// Uses `output_tokens / ((latency_ms - ttft_ms) / 1000)` when every input is
/// present and the writing window is positive. Returns `None` when:
///
/// - `output_tokens` is missing or zero (nothing to measure),
/// - `ttft_ms` is missing (streaming start was not recorded),
/// - `latency_ms - ttft_ms` is zero or negative (no distinct writing phase).
#[must_use]
pub fn tokens_per_second(
    output_tokens: Option<i64>,
    latency_ms: i64,
    ttft_ms: Option<i64>,
) -> Option<f64> {
    let output = output_tokens.filter(|tokens| *tokens > 0)?;
    let ttft = ttft_ms.filter(|ms| *ms >= 0)?;
    let writing_ms = latency_ms.saturating_sub(ttft);
    if writing_ms <= 0 {
        return None;
    }
    Some(output as f64 / (writing_ms as f64 / 1000.0))
}

/// Weighted mean of [`tokens_per_second`] over rows that yield a speed.
#[must_use]
pub fn mean_tokens_per_second<I>(rows: I) -> Option<f64>
where
    I: IntoIterator<Item = (Option<i64>, i64, Option<i64>)>,
{
    let mut sum = 0.0;
    let mut weight = 0.0;
    for (output, latency_ms, ttft_ms) in rows {
        let Some(speed) = tokens_per_second(output, latency_ms, ttft_ms) else {
            continue;
        };
        let w = output.filter(|tokens| *tokens > 0).unwrap_or(1) as f64;
        sum += speed * w;
        weight += w;
    }
    if weight > 0.0 {
        Some(sum / weight)
    } else {
        None
    }
}

/// Mean TTFT over rows with a positive measurement.
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
    use super::{mean_tokens_per_second, mean_ttft_ms, tokens_per_second};

    #[test]
    fn r10_tokens_per_second_divides_output_by_writing_window() {
        let speed = tokens_per_second(Some(100), 2500, Some(500)).expect("speed");
        assert!((speed - 50.0).abs() < 0.001);
    }

    #[test]
    fn r10_tokens_per_second_missing_ttft_is_none() {
        assert!(tokens_per_second(Some(10), 1000, None).is_none());
    }

    #[test]
    fn r10_tokens_per_second_zero_writing_window_is_none() {
        assert!(tokens_per_second(Some(10), 500, Some(500)).is_none());
        assert!(tokens_per_second(Some(10), 400, Some(500)).is_none());
    }

    #[test]
    fn r10_mean_helpers_skip_empty_inputs() {
        assert!((mean_ttft_ms([Some(100), None, Some(300)]).expect("mean") - 200.0).abs() < 0.001);
        let mean = mean_tokens_per_second([(Some(100), 2500, Some(500)), (None, 1000, Some(0))])
            .expect("mean speed");
        assert!((mean - 50.0).abs() < 0.001);
    }
}
