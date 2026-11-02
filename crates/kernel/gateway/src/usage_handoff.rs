//! Usage rows for billed tries handed to another member.

use std::sync::{Arc, Mutex};
use std::time::{Instant, SystemTime, UNIX_EPOCH};

use hofund_plugin_api::BilledUsage;
use hofund_router::{Attempt, AttemptError, AttemptHook, MemberAttempt};
use hofund_usage::{Ledger, OUTCOME_HANDED_ON, Row, UsageDetails};

pub(crate) struct UsageHandoffContext {
    pub(crate) usage: Arc<Ledger>,
    pub(crate) model: String,
    pub(crate) caller_key_id: String,
    pub(crate) caller_key_name: String,
    pub(crate) caller_issuer_id: String,
    pub(crate) started: Instant,
}

struct PendingHandoff {
    member: MemberAttempt,
    status: u16,
    billed: BilledUsage,
    latency_ms: i64,
}

pub(crate) struct UsageHandoffHook {
    ctx: UsageHandoffContext,
    inner: Option<Arc<dyn AttemptHook>>,
    pending: Mutex<Option<PendingHandoff>>,
}

impl UsageHandoffHook {
    pub(crate) fn new(ctx: UsageHandoffContext, inner: Option<Arc<dyn AttemptHook>>) -> Self {
        Self {
            ctx,
            inner,
            pending: Mutex::new(None),
        }
    }

    fn flush(&self) {
        let pending = self.pending.lock().expect("handoff lock").take();
        let Some(pending) = pending else {
            return;
        };
        let details = UsageDetails {
            cache_read_tokens: pending.billed.cache_read_tokens,
            cache_write_5m_tokens: pending.billed.cache_write_5m_tokens,
            cache_write_1h_tokens: pending.billed.cache_write_1h_tokens,
            cache_write_tokens: pending
                .billed
                .cache_write_5m_tokens
                .zip(pending.billed.cache_write_1h_tokens)
                .map(|(five, one)| five.saturating_add(one)),
            ..UsageDetails::default()
        };
        let row = Row {
            created_unix: SystemTime::now()
                .duration_since(UNIX_EPOCH)
                .map_or(0, |duration| {
                    i64::try_from(duration.as_secs()).unwrap_or(i64::MAX)
                }),
            model: self.ctx.model.clone(),
            upstream: pending.member.provider_id,
            input_tokens: pending.billed.input_tokens,
            output_tokens: pending.billed.output_tokens,
            latency_ms: pending.latency_ms,
            status: pending.status.to_string(),
            caller_key_id: self.ctx.caller_key_id.clone(),
            caller_key_name: self.ctx.caller_key_name.clone(),
            caller_issuer_id: self.ctx.caller_issuer_id.clone(),
            cache_read_tokens: pending.billed.cache_read_tokens,
            cache_write_5m_tokens: pending.billed.cache_write_5m_tokens,
            cache_write_1h_tokens: pending.billed.cache_write_1h_tokens,
            outcome: OUTCOME_HANDED_ON.to_owned(),
            ..Default::default()
        };
        let _ = self
            .ctx
            .usage
            .record_with_details(&row, None, details, None);
    }
}

impl AttemptHook for UsageHandoffHook {
    fn before(&self, attempt: &Attempt) -> Result<(), AttemptError> {
        self.flush();
        if let Some(inner) = self.inner.as_ref() {
            inner.before(attempt)
        } else {
            Ok(())
        }
    }

    fn prepare(&self, attempt: &Attempt, body: &[u8]) -> Option<Vec<u8>> {
        self.inner
            .as_ref()
            .and_then(|inner| inner.prepare(attempt, body))
    }

    fn refused(&self, attempt: &Attempt, status: u16, body: &[u8]) -> bool {
        self.inner
            .as_ref()
            .is_some_and(|inner| inner.refused(attempt, status, body))
    }

    fn retry(&self, attempt: &Attempt, status: u16, body: &[u8]) -> Option<Vec<u8>> {
        self.inner
            .as_ref()
            .and_then(|inner| inner.retry(attempt, status, body))
    }

    fn answered(&self, attempt: &Attempt) {
        if let Some(inner) = self.inner.as_ref() {
            inner.answered(attempt);
        }
    }

    fn failed_handoff(&self, attempt: &Attempt, status: u16, billed: BilledUsage) {
        let latency_ms = i64::try_from(self.ctx.started.elapsed().as_millis()).unwrap_or(i64::MAX);
        *self.pending.lock().expect("handoff lock") = Some(PendingHandoff {
            member: attempt.member.clone(),
            status,
            billed,
            latency_ms,
        });
    }
}
