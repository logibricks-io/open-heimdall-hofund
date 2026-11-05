//! Async UI loop helpers (keyboard, control events, workers).

use hofund_control_client::hofund_control_proto::{EventPayload, Topic};
use hofund_control_client::{Client, EventStream};
use tokio::sync::mpsc;

use crate::app::App;
use crate::events::apply_control_event;
use crate::gateway::HandoffOutcome;
use crate::pages::{ProbeOutcome, read_path};
use crate::state::PageId;

/// Work sent back to the UI task from background jobs.
pub enum WorkerMsg {
    /// Handoff tick finished.
    Handoff {
        /// Presence line update.
        outcome: HandoffOutcome,
        /// Quit-related ownership snapshot.
        snapshot: HandoffSnapshot,
    },
    /// Spawn recorded before discovery finishes.
    HandoffProgress {
        /// Quit-related ownership snapshot.
        snapshot: HandoffSnapshot,
    },
    /// Deferred page GET finished.
    PageProbe {
        page: PageId,
        generation: u64,
        outcome: ProbeOutcome,
    },
}

/// Handoff fields needed on quit (updated each tick).
#[derive(Clone, Debug, Default)]
pub struct HandoffSnapshot {
    /// Whether this UI started the daemon.
    pub spawned: bool,
    /// Child pid when spawned.
    pub child_pid: Option<u32>,
}

/// Opens or reopens the control event stream.
pub async fn open_event_stream(
    client: &Client,
    last_event_id: Option<&str>,
) -> Result<EventStream, String> {
    client
        .events(&[Topic::Control, Topic::Requests], last_event_id)
        .await
        .map_err(|error| error.to_string())
}

/// Applies a worker message to `app`.
pub fn apply_worker_msg(app: &mut App, msg: WorkerMsg) {
    match msg {
        WorkerMsg::Handoff { outcome, .. } => apply_handoff_outcome(app, outcome),
        WorkerMsg::HandoffProgress { .. } => {}
        WorkerMsg::PageProbe {
            page,
            generation,
            outcome,
        } => {
            app.apply_read_result(generation, |app| {
                app.pages.apply_probe(page, outcome);
            });
        }
    }
}

fn apply_handoff_outcome(app: &mut App, outcome: HandoffOutcome) {
    if let HandoffOutcome::Update(presence) = outcome {
        app.gateway = presence;
    }
}

/// Applies a control event and returns follow-up actions for the event stream.
pub fn apply_control_followup(app: &mut App, payload: &EventPayload) -> ControlFollowup {
    apply_control_event(app, payload);
    ControlFollowup {
        reread_token: app.take_token_reread(),
        resync_stream: app.take_event_resync(),
    }
}

/// Side effects after one control event.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct ControlFollowup {
    /// Re-read the token file and reconnect the client and stream.
    pub reread_token: bool,
    /// Reopen the event stream without `Last-Event-ID`.
    pub resync_stream: bool,
}

/// Applies a control event and returns whether the bearer token must be reread.
pub fn apply_control_and_take_token_reread(app: &mut App, payload: &EventPayload) -> bool {
    apply_control_followup(app, payload).reread_token
}

/// Applies a control event and returns whether the stream must reopen without a cursor.
pub fn apply_control_and_take_resync(app: &mut App, payload: &EventPayload) -> bool {
    apply_control_followup(app, payload).resync_stream
}

/// Schedules a deferred page probe on the worker channel when a client exists.
pub fn schedule_page_probe(
    client: Option<&Client>,
    work_tx: &mpsc::Sender<WorkerMsg>,
    page: PageId,
    generation: u64,
) {
    let Some(path) = read_path(page) else {
        return;
    };
    let Some(client) = client.cloned() else {
        return;
    };
    let path = path.to_owned();
    let work_tx = work_tx.clone();
    tokio::spawn(async move {
        let outcome =
            ProbeOutcome::from_get_result(client.get_http_status(&path).await.map(|_| ()));
        let _ = work_tx
            .send(WorkerMsg::PageProbe {
                page,
                generation,
                outcome,
            })
            .await;
    });
}

#[cfg(test)]
mod tests {
    use hofund_control_client::hofund_control_proto::{
        EventPayload, ResyncRequiredEvent, TokenRotatedEvent,
    };

    use std::net::SocketAddr;
    use std::path::{Path, PathBuf};

    use super::{apply_control_and_take_resync, apply_control_and_take_token_reread};
    use crate::app::App;
    use crate::gateway::{HandoffDeps, HandoffState, record_attach};
    use crate::state::PageId;
    use hofund_control_client::Client;

    struct AliveDeps;

    impl HandoffDeps for AliveDeps {
        fn held_locks(&self, _config: &Path) -> Result<Vec<PathBuf>, String> {
            Ok(Vec::new())
        }

        async fn discover_attached(
            &self,
            _config: &Path,
            _token: &str,
        ) -> Result<Client, hofund_control_client::ClientError> {
            Err(hofund_control_client::ClientError::ConnectionUnavailable(
                "unused".to_owned(),
            ))
        }

        async fn probe_model_port(
            &self,
            _listen: SocketAddr,
        ) -> Result<(), hofund_control_client::ClientError> {
            Err(hofund_control_client::ClientError::ConnectionUnavailable(
                "unused".to_owned(),
            ))
        }

        fn spawn_daemon(&self, _config: &Path) -> Result<u32, String> {
            Err("unused".to_owned())
        }

        async fn fetch_status(
            &self,
            _client: &Client,
        ) -> Result<
            hofund_control_client::hofund_control_proto::StatusResponse,
            hofund_control_client::ClientError,
        > {
            Err(hofund_control_client::ClientError::ConnectionUnavailable(
                "unused".to_owned(),
            ))
        }

        fn process_alive(&self, _pid: u32) -> bool {
            true
        }
    }

    #[test]
    fn token_rotated_requests_token_reread() {
        let mut app = App::for_test(PageId::Agents);
        assert!(apply_control_and_take_token_reread(
            &mut app,
            &EventPayload::TokenRotated(TokenRotatedEvent::default())
        ));
    }

    #[test]
    fn resync_required_clears_stream_cursor() {
        let mut app = App::for_test(PageId::Agents);
        assert!(apply_control_and_take_resync(
            &mut app,
            &EventPayload::ResyncRequired(ResyncRequiredEvent::default())
        ));
    }

    #[test]
    fn own_spawn_stays_spawned_on_attach() {
        let deps = AliveDeps;
        let mut state = HandoffState {
            spawned: true,
            child_pid: Some(42),
            client: None,
        };
        record_attach(
            &mut state,
            42,
            Client::tcp("127.0.0.1:9".parse().unwrap(), "t"),
            &deps,
        );
        assert!(state.spawned);
        assert_eq!(state.child_pid, Some(42));
    }
}
