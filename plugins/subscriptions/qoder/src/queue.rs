//! Qoder model-queue wait before resending a refused chat.

use std::sync::Arc;
use std::time::{Duration, SystemTime};

use hofund_plugin_api::{HostContext, PluginError};
use hofund_plugin_sdk::Call;

use crate::Endpoints;
use crate::cosy;
use crate::creds::Stored;
use crate::failure::{self, QueueState};
use crate::identity::Devices;

const STATUS_PATH: &str = "/algo/api/v2/service/ask/queue/status";
const BUDGET: Duration = Duration::from_secs(4 * 60);
const SHORTEST: Duration = Duration::from_millis(500);
const LONGEST: Duration = Duration::from_secs(30);
const UNSAID: Duration = Duration::from_secs(30);
const ASK: Duration = Duration::from_secs(30);
pub(crate) const MAX_RECOVERIES: usize = 10;
const FAILURES: u32 = 3;

#[derive(Clone, Debug)]
pub(crate) struct ChatAttempt {
    pub request_set_id: String,
    pub request_id: String,
    pub session_id: String,
    pub model_key: String,
}

pub(crate) async fn maybe_wait(
    host: &HostContext,
    endpoints: &Arc<Endpoints>,
    devices: &Devices,
    stored: &Stored,
    attempt: &ChatAttempt,
    queue: &QueueState,
    wait_allowed: bool,
) -> Result<bool, PluginError> {
    if !wait_allowed {
        return Ok(false);
    }
    let deadline = host.clock().now() + BUDGET;
    wait(host, endpoints, devices, stored, attempt, queue, deadline).await
}

async fn wait(
    host: &HostContext,
    endpoints: &Arc<Endpoints>,
    devices: &Devices,
    stored: &Stored,
    attempt: &ChatAttempt,
    first: &QueueState,
    deadline: SystemTime,
) -> Result<bool, PluginError> {
    let site = endpoints.resolve(&stored.credential)?;
    let api = site.chat.trim_end_matches('/');
    let headers = devices.chat_headers(host, &stored.credential).await;
    let mut state = first.clone();
    let mut asked = false;
    let mut failed = 0u32;
    loop {
        if host.clock().now() >= deadline {
            return Ok(false);
        }
        if asked {
            let pause = pause_length(state.retry_after_seconds, deadline, host.clock().now());
            if !pause.is_zero() {
                hofund_plugin_sdk::sleep(pause).await;
            }
        }
        asked = true;
        if host.clock().now() >= deadline {
            return Ok(false);
        }
        match queue_status(
            host,
            api,
            &stored.credential,
            attempt,
            &state,
            &headers,
            deadline,
        )
        .await
        {
            Ok(next) => {
                failed = 0;
                state = merge_state(&state, next);
                if state.is_queued == Some(false) {
                    let pause =
                        pause_length(state.retry_after_seconds, deadline, host.clock().now());
                    if !pause.is_zero() {
                        hofund_plugin_sdk::sleep(pause).await;
                    }
                    return Ok(host.clock().now() < deadline);
                }
                if state.service_available == Some(false) || state.is_queued == Some(true) {
                    continue;
                }
                return Ok(false);
            }
            Err(StatusOutcome::End) => return Ok(false),
            Err(StatusOutcome::Transient) => {
                failed += 1;
                if failed >= FAILURES {
                    return Ok(false);
                }
                state.retry_after_seconds = None;
            }
        }
    }
}

enum StatusOutcome {
    End,
    Transient,
}

async fn queue_status(
    host: &HostContext,
    api: &str,
    credential: &crate::creds::Credential,
    attempt: &ChatAttempt,
    state: &QueueState,
    base_headers: &[(String, String)],
    deadline: SystemTime,
) -> Result<QueueState, StatusOutcome> {
    let model_key = state
        .model_key
        .as_deref()
        .filter(|text| !text.is_empty())
        .unwrap_or(&attempt.model_key);
    let mut query = format!(
        "{api}{STATUS_PATH}?requestSetId={}&modelKey={}",
        attempt.request_set_id, model_key
    );
    if let Some(kind) = state.queue_type.as_deref().filter(|text| !text.is_empty()) {
        query.push_str(&format!("&queueType={kind}"));
    }
    let left = ASK.min(
        deadline
            .duration_since(host.clock().now())
            .unwrap_or_default(),
    );
    if left.is_zero() {
        return Err(StatusOutcome::End);
    }
    let signature = cosy::sign(&query, credential, "", host.clock().now())
        .map_err(|_| StatusOutcome::Transient)?;
    let mut call = Call::get(&query);
    for (name, value) in base_headers {
        call = call.sensitive_header(name, value);
    }
    call = call
        .sensitive_header("X-Request-ID", &attempt.request_id)
        .sensitive_header("X-Session-ID", &attempt.session_id);
    call = cosy::signed_call_with_accept(call, signature, "");
    let response = match call.read_timeout(left).open_raw(host.http(), 1).await {
        Ok(response) => response,
        Err(_) => return Err(StatusOutcome::Transient),
    };
    let status = response.status().as_u16();
    let mut bytes = Vec::new();
    let mut response = response;
    while let Some(chunk) = response.body_mut().next_chunk().await {
        match chunk {
            Ok(part) => bytes.extend_from_slice(&part),
            Err(_) => return Err(StatusOutcome::Transient),
        }
        if bytes.len() > 1 << 20 {
            break;
        }
    }
    if status == 404 {
        return Err(StatusOutcome::End);
    }
    if !response.status().is_success() {
        return Err(StatusOutcome::Transient);
    }
    let text = String::from_utf8_lossy(&bytes);
    failure::queue_state(&text).ok_or(StatusOutcome::End)
}

fn merge_state(previous: &QueueState, next: QueueState) -> QueueState {
    QueueState {
        is_queued: next.is_queued,
        service_available: next.service_available,
        model_key: next.model_key.or_else(|| previous.model_key.clone()),
        queue_type: next.queue_type.or_else(|| previous.queue_type.clone()),
        queue_count: next.queue_count.or(previous.queue_count),
        retry_after_seconds: next.retry_after_seconds.or(previous.retry_after_seconds),
    }
}

fn pause_length(seconds: Option<f64>, deadline: SystemTime, now: SystemTime) -> Duration {
    queue_pause_seconds(seconds).min(deadline.duration_since(now).unwrap_or_default())
}

fn queue_pause_seconds(seconds: Option<f64>) -> Duration {
    let millis = match seconds.filter(|value| value.is_finite()) {
        Some(value) => (value * 1000.0) as u64,
        None => UNSAID.as_millis() as u64,
    };
    Duration::from_millis(millis.clamp(SHORTEST.as_millis() as u64, LONGEST.as_millis() as u64))
}

#[cfg(test)]
mod tests {
    use super::queue_pause_seconds;
    use std::time::Duration;

    #[test]
    fn r54e_pause_is_clamped_and_defaults_to_thirty() {
        assert_eq!(queue_pause_seconds(Some(0.1)), Duration::from_millis(500));
        assert_eq!(queue_pause_seconds(Some(300.0)), Duration::from_secs(30));
        assert_eq!(queue_pause_seconds(None), Duration::from_secs(30));
    }
}
