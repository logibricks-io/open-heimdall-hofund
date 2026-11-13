//! Async UI loop helpers (keyboard, control events, workers).

use hofund_control_client::hofund_control_proto::{
    EventPayload, SessionListQuery, SessionStatsQuery, Topic,
};
use hofund_control_client::{Client, EventStream};
use tokio::sync::mpsc;

use crate::app::App;
use crate::events::apply_control_event;
use crate::gateway::HandoffOutcome;
use crate::pages::{ProbeOutcome, read_path};
use crate::session::{SessionOutcome, run_intent};
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
    /// One control intent finished.
    Session(Box<SessionOutcome>),
    /// Sessions list and stats fetch finished.
    SessionsLoad {
        /// Read generation this fetch was started with.
        generation: u64,
        /// List and stats outcome.
        outcome: Box<SessionsLoadOutcome>,
    },
    /// Library fetch finished.
    LibraryLoad {
        /// Read generation this fetch was started with.
        generation: u64,
        /// Library outcome.
        outcome: LibraryLoadOutcome,
    },
}

/// Result of loading the Library page.
#[derive(Clone, Debug)]
pub enum LibraryLoadOutcome {
    /// Route missing in this build.
    NotInBuild,
    /// The control call succeeded.
    Ok(hofund_control_proto::LibraryResponse),
    /// Any error.
    Err(String),
}

/// Result of loading the Sessions page.
#[derive(Clone, Debug)]
pub enum SessionsLoadOutcome {
    /// Route missing in this build.
    NotInBuild,
    /// Both control calls succeeded.
    Ok {
        /// Session list body.
        list: hofund_control_proto::SessionListResponse,
        /// Stats body.
        stats: Box<hofund_control_proto::SessionStatsResponse>,
    },
    /// Any error.
    Err(String),
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
        WorkerMsg::Session(outcome) => app.apply_session(*outcome),
        WorkerMsg::SessionsLoad {
            generation,
            outcome,
        } => {
            app.apply_read_result(generation, |app| {
                let sessions = app.pages.sessions_mut();
                match *outcome {
                    SessionsLoadOutcome::NotInBuild => {
                        sessions.apply_probe(ProbeOutcome::NotInBuild);
                    }
                    SessionsLoadOutcome::Ok { list, stats } => {
                        sessions.apply_load(list, *stats);
                    }
                    SessionsLoadOutcome::Err(message) => sessions.apply_load_error(message),
                }
            });
        }
        WorkerMsg::LibraryLoad {
            generation,
            outcome,
        } => {
            app.apply_read_result(generation, |app| {
                let library = app.pages.library_mut();
                match outcome {
                    LibraryLoadOutcome::NotInBuild => {
                        library.apply_probe(ProbeOutcome::NotInBuild);
                    }
                    LibraryLoadOutcome::Ok(view) => library.apply_load(view),
                    LibraryLoadOutcome::Err(message) => library.apply_load_error(message),
                }
            });
        }
    }
}

/// Schedules list and stats fetches for the Sessions page when a client exists.
pub fn schedule_sessions_fetch(
    client: Option<&Client>,
    work_tx: &mpsc::Sender<WorkerMsg>,
    list: SessionListQuery,
    stats: SessionStatsQuery,
    generation: u64,
) {
    let Some(client) = client.cloned() else {
        return;
    };
    let work_tx = work_tx.clone();
    tokio::spawn(async move {
        let outcome = match client.sessions(list).await {
            Err(error) if error.http_status().is_some_and(|s| s.as_u16() == 404) => {
                SessionsLoadOutcome::NotInBuild
            }
            Err(error) => SessionsLoadOutcome::Err(error.to_string()),
            Ok(list) => match client.session_stats(stats).await {
                Ok(stats) => SessionsLoadOutcome::Ok {
                    list,
                    stats: Box::new(stats),
                },
                Err(error) if error.http_status().is_some_and(|s| s.as_u16() == 404) => {
                    SessionsLoadOutcome::NotInBuild
                }
                Err(error) => SessionsLoadOutcome::Err(error.to_string()),
            },
        };
        let _ = work_tx
            .send(WorkerMsg::SessionsLoad {
                generation,
                outcome: Box::new(outcome),
            })
            .await;
    });
}

/// Schedules a Library fetch when a client exists.
pub fn schedule_library_fetch(
    client: Option<&Client>,
    work_tx: &mpsc::Sender<WorkerMsg>,
    generation: u64,
) {
    let Some(client) = client.cloned() else {
        return;
    };
    let work_tx = work_tx.clone();
    tokio::spawn(async move {
        let outcome = match client.library().await {
            Ok(view) => LibraryLoadOutcome::Ok(view),
            Err(error) if error.http_status().is_some_and(|s| s.as_u16() == 404) => {
                LibraryLoadOutcome::NotInBuild
            }
            Err(error) => LibraryLoadOutcome::Err(error.to_string()),
        };
        let _ = work_tx
            .send(WorkerMsg::LibraryLoad {
                generation,
                outcome,
            })
            .await;
    });
}

/// Schedules load work for the open page when a client exists.
pub fn schedule_open_page_load(
    app: &App,
    client: Option<&Client>,
    work_tx: &mpsc::Sender<WorkerMsg>,
) {
    let page = app.page;
    let generation = app.pages.page_dyn(page).fetch_gen();
    match page {
        PageId::Sessions => {
            let sessions = app.pages.sessions();
            let list = sessions.list_query();
            let stats = sessions.stats_query();
            schedule_sessions_fetch(client, work_tx, list, stats, generation);
        }
        PageId::Library => schedule_library_fetch(client, work_tx, generation),
        other if read_path(other).is_some() => {
            schedule_page_probe(client, work_tx, other, generation);
        }
        _ => {}
    }
}

/// Spawns queued control calls. Intents stay queued when no client is attached.
pub fn schedule_intents(app: &mut App, client: Option<&Client>, work_tx: &mpsc::Sender<WorkerMsg>) {
    if client.is_none() {
        return;
    }
    let Some(client) = client.cloned() else {
        return;
    };
    for intent in app.take_intents() {
        let client = client.clone();
        let work_tx = work_tx.clone();
        tokio::spawn(async move {
            let outcome = run_intent(&client, intent).await;
            let _ = work_tx.send(WorkerMsg::Session(Box::new(outcome))).await;
        });
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
