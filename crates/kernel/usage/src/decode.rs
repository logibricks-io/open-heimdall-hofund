//! Output speed from stored timing, matching the reference `DecodeOf` rules.

pub const MIN_DECODE_MS: i64 = 100;
pub const MAX_DECODE_SPEED: i64 = 10_000;
pub const HELD_SHARE: i64 = 4;
pub const THINK_PACE: i64 = 20;
pub const HELD_WINDOW_MS: i64 = 1000;

/// Writing window in milliseconds, or zero when speed cannot be told.
#[must_use]
pub fn decode_window(out: i64, latency_ms: i64, start_ms: i64) -> i64 {
    let window = latency_ms - start_ms;
    if out <= 0 || start_ms <= 0 || window < MIN_DECODE_MS || out * 1000 > MAX_DECODE_SPEED * window
    {
        return 0;
    }
    window
}

/// Token count and window for speed, or `(0, 0)` when untold.
#[must_use]
pub fn decode_of(
    output: i64,
    reasoning: i64,
    latency_ms: i64,
    ttft_ms: i64,
    first_text_ms: i64,
    flow_ms: i64,
    first_reasoning_ms: i64,
) -> (i64, i64) {
    let mut out = output;
    let mut start = ttft_ms;
    if reasoning > 0 {
        if first_text_ms > 0 {
            out = output.saturating_sub(reasoning);
            start = first_text_ms;
        } else if first_reasoning_ms >= ttft_ms && flow_ms > 0 && reasoning <= output {
            start = first_reasoning_ms;
        } else {
            return (0, 0);
        }
    }
    if ttft_ms <= 0 {
        return (0, 0);
    }
    let window = decode_window(out, latency_ms, start);
    if window == 0 {
        return (0, 0);
    }
    if flow_ms > 0 && flow_ms * HELD_SHARE < window {
        return (0, 0);
    }
    if window < HELD_WINDOW_MS && start > HELD_SHARE * window {
        return (0, 0);
    }
    if reasoning > 0
        && first_text_ms > 0
        && out * (first_text_ms - ttft_ms) > THINK_PACE * reasoning * window
    {
        return (0, 0);
    }
    (out, window)
}

/// One `(elapsed_ms, cumulative_content_bytes)` sample.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
struct FlowMark {
    at_ms: i64,
    sum: i64,
}

const FLOW_MOST: usize = 1 << 16;

/// Tracks content byte arrivals for [`FlowTracker::flow_ms`].
#[derive(Clone, Debug, Default)]
pub struct FlowTracker {
    marks: Vec<FlowMark>,
    sum: i64,
    off: bool,
}

impl FlowTracker {
    /// Records `n` content bytes at `elapsed_ms` from stream start.
    pub fn mark(&mut self, elapsed_ms: i64, n: i64) {
        if self.off || n <= 0 {
            return;
        }
        self.sum += n;
        if let Some(last) = self.marks.last()
            && last.at_ms == elapsed_ms
        {
            self.marks.last_mut().expect("last").sum = self.sum;
            return;
        }
        if self.marks.len() >= FLOW_MOST {
            self.off = true;
            self.marks.clear();
            return;
        }
        self.marks.push(FlowMark {
            at_ms: elapsed_ms,
            sum: self.sum,
        });
    }

    /// Middle 80% content span scaled to the whole reply, from `from_ms`.
    #[must_use]
    pub fn flow_ms(&self, from_ms: i64) -> i64 {
        if from_ms <= 0 || self.marks.is_empty() || self.off {
            return 0;
        }
        let mut base = 0;
        for mark in &self.marks {
            if mark.at_ms >= from_ms {
                break;
            }
            base = mark.sum;
        }
        let total = self.sum - base;
        if total <= 0 {
            return 0;
        }
        let at = |share: i64| -> i64 {
            for mark in &self.marks {
                if (mark.sum - base) * 100 >= total * share && mark.at_ms >= from_ms {
                    return mark.at_ms;
                }
            }
            self.marks.last().map(|mark| mark.at_ms).unwrap_or(0)
        };
        let span = at(90) - at(10);
        (span * 10 / 8).max(1)
    }

    /// `flow_ms` from first text when reasoning was counted, else from first content.
    #[must_use]
    pub fn flow_for(&self, reasoning: i64, first_text_ms: i64, first_content_ms: i64) -> i64 {
        if reasoning > 0 && first_text_ms > 0 {
            self.flow_ms(first_text_ms)
        } else {
            self.flow_ms(first_content_ms)
        }
    }
}

/// Token count and decode window for one row, for aggregate speed.
#[must_use]
pub fn decode_row(row: &super::Row) -> (i64, i64) {
    if row.reasoning_hidden && row.first_text_ms.unwrap_or(0) <= 0 {
        return (0, 0);
    }
    let output = row.output_tokens.unwrap_or(0);
    let reasoning = row.reasoning_tokens.unwrap_or(0);
    let ttft = row.ttft_ms.unwrap_or(0);
    decode_of(
        output,
        reasoning,
        row.latency_ms,
        ttft,
        row.first_text_ms.unwrap_or(0),
        row.flow_ms.unwrap_or(0),
        row.first_reasoning_ms.unwrap_or(0),
    )
}

/// Tokens per second from one ledger row, or `None` when speed is untold.
#[must_use]
pub fn row_tokens_per_second(row: &super::Row) -> Option<f64> {
    row.output_tokens.filter(|tokens| *tokens > 0)?;
    row.ttft_ms.filter(|ms| *ms > 0)?;
    let (tokens, window) = decode_row(row);
    if tokens <= 0 || window <= 0 {
        return None;
    }
    Some(tokens as f64 / (window as f64 / 1000.0))
}

#[cfg(test)]
mod tests {
    use super::{FlowTracker, decode_of, decode_window, row_tokens_per_second};
    use crate::Row;

    #[test]
    fn r10_decode_window_rejects_burst_tool_replies() {
        assert_eq!(decode_window(8264, 1, 0), 0);
        assert_eq!(decode_window(100, 2500, 500), 2000);
        assert_eq!(decode_window(100, 500, 500), 0);
    }

    #[test]
    fn r10_decode_of_burst_flow_tells_no_speed() {
        assert_eq!(decode_of(528, 0, 10_500, 500, 0, 236, 0), (0, 0));
        assert_eq!(decode_of(392, 0, 8455, 8200, 8200, 230, 0), (0, 0));
    }

    #[test]
    fn r10_flow_ms_middle_eighty_percent_scaled() {
        let mut flow = FlowTracker::default();
        for (ms, bytes) in [(100, 10), (200, 30), (300, 50), (400, 90), (500, 100)] {
            flow.mark(ms, bytes);
        }
        assert_eq!(flow.flow_ms(100), 375);
    }

    #[test]
    fn r10_flow_ms_zero_for_single_mark() {
        let mut flow = FlowTracker::default();
        flow.mark(10, 100);
        assert_eq!(flow.flow_ms(10), 1);
    }

    #[test]
    fn r10_row_speed_missing_ttft_is_none() {
        let row = Row {
            output_tokens: Some(10),
            latency_ms: 1000,
            ..Default::default()
        };
        assert!(row_tokens_per_second(&row).is_none());
    }

    #[test]
    fn r10_row_speed_steady_stream_without_flow_uses_window() {
        let row = Row {
            output_tokens: Some(100),
            latency_ms: 2500,
            ttft_ms: Some(500),
            ..Default::default()
        };
        let speed = row_tokens_per_second(&row).expect("speed");
        assert!((speed - 50.0).abs() < 0.001);
    }

    #[test]
    fn r10_decode_of_missing_flow_on_old_rows_still_allows_steady_speed() {
        assert_eq!(decode_of(100, 0, 2500, 500, 0, 0, 0), (100, 2000));
    }

    #[test]
    fn r10_row_speed_hidden_reasoning_without_first_text_is_none() {
        let row = Row {
            output_tokens: Some(50),
            reasoning_tokens: Some(20),
            reasoning_hidden: true,
            latency_ms: 5000,
            ttft_ms: Some(500),
            ..Default::default()
        };
        assert!(row_tokens_per_second(&row).is_none());
    }

    #[test]
    fn r10_decode_row_hidden_reasoning_zero_reasoning_without_first_text() {
        let row = Row {
            output_tokens: Some(50),
            reasoning_tokens: Some(0),
            reasoning_hidden: true,
            latency_ms: 5000,
            ttft_ms: Some(500),
            ..Default::default()
        };
        assert_eq!(super::decode_row(&row), (0, 0));
        assert!(row_tokens_per_second(&row).is_none());
    }
}
