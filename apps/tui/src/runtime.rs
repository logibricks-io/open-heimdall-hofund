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
    /// Sessions list and stats fetch finished.
    SessionsLoad {
        generation: u64,
        outcome: Box<SessionsLoadOutcome>,
    },
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

/// Schedules a sessions list and stats fetch.
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
        other if read_path(other).is_some() => {
            schedule_page_probe(client, work_tx, other, generation);
        }
        _ => {}
    }
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
    use tokio::sync::mpsc;

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

    async fn serve_status(status: u16, path_prefix: &str) -> std::net::SocketAddr {
        use tokio::io::{AsyncReadExt, AsyncWriteExt};
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0")
            .await
            .expect("bind");
        let addr = listener.local_addr().expect("addr");
        let prefix = path_prefix.to_owned();
        tokio::spawn(async move {
            loop {
                let Ok((mut stream, _)) = listener.accept().await else {
                    break;
                };
                let mut buf = vec![0_u8; 4096];
                let _ = stream.read(&mut buf).await;
                let req = String::from_utf8_lossy(&buf);
                let hit = req
                    .lines()
                    .next()
                    .is_some_and(|line| line.contains(&prefix));
                let code = if hit { status } else { 404 };
                let body = if code == 200 {
                    "{}".to_owned()
                } else {
                    r#"{"version":1,"error":{"kind":"not_found","message":"missing","request_id":"test","retry_after_ms":null,"details":[]}}"#.to_owned()
                };
                let response = format!(
                    "HTTP/1.1 {code} X\r\ncontent-type: application/json\r\ncontent-length: {}\r\nconnection: close\r\n\r\n{body}",
                    body.len()
                );
                let _ = stream.write_all(response.as_bytes()).await;
            }
        });
        addr
    }

    async fn recv_msg(rx: &mut mpsc::Receiver<super::WorkerMsg>) -> super::WorkerMsg {
        tokio::time::timeout(std::time::Duration::from_secs(2), rx.recv())
            .await
            .expect("worker timed out")
            .expect("channel closed")
    }

    #[tokio::test]
    async fn schedule_sessions_fetch_maps_404_and_other_errors() {
        let (tx, mut rx) = mpsc::channel(4);
        let addr = serve_status(404, "/v1/hofund/sessions").await;
        let client = Client::tcp(addr, "t");
        super::schedule_sessions_fetch(
            Some(&client),
            &tx,
            Default::default(),
            Default::default(),
            3,
        );
        let super::WorkerMsg::SessionsLoad {
            generation,
            outcome,
        } = recv_msg(&mut rx).await
        else {
            panic!("expected sessions load");
        };
        assert_eq!(generation, 3);
        assert!(
            matches!(*outcome, super::SessionsLoadOutcome::NotInBuild),
            "{outcome:?}"
        );

        let addr = serve_status(500, "/v1/hofund/sessions").await;
        let client = Client::tcp(addr, "t");
        super::schedule_sessions_fetch(
            Some(&client),
            &tx,
            Default::default(),
            Default::default(),
            4,
        );
        let super::WorkerMsg::SessionsLoad { outcome, .. } = recv_msg(&mut rx).await else {
            panic!("expected sessions load");
        };
        assert!(matches!(*outcome, super::SessionsLoadOutcome::Err(_)));
    }

    #[tokio::test]
    async fn schedule_open_page_load_uses_sessions_fetch_for_sessions() {
        let (tx, mut rx) = mpsc::channel(4);
        let addr = serve_status(404, "/v1/hofund/sessions").await;
        let client = Client::tcp(addr, "t");
        let app = App::for_test(PageId::Sessions);
        super::schedule_open_page_load(&app, Some(&client), &tx);
        let msg = recv_msg(&mut rx).await;
        assert!(
            matches!(msg, super::WorkerMsg::SessionsLoad { .. }),
            "sessions page must use the list/stats fetch, not the probe path"
        );
    }

    #[tokio::test]
    async fn schedule_open_page_load_probes_library() {
        let (tx, mut rx) = mpsc::channel(4);
        let addr = serve_status(200, "/v1/hofund/library").await;
        let client = Client::tcp(addr, "t");
        let app = App::for_test(PageId::Library);
        super::schedule_open_page_load(&app, Some(&client), &tx);
        let msg = recv_msg(&mut rx).await;
        assert!(
            matches!(
                msg,
                super::WorkerMsg::PageProbe {
                    page: PageId::Library,
                    ..
                }
            ),
            "library must still be probed"
        );
    }

    async fn serve_list_then_stats(list_status: u16, stats_status: u16) -> std::net::SocketAddr {
        use tokio::io::{AsyncReadExt, AsyncWriteExt};
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0")
            .await
            .expect("bind");
        let addr = listener.local_addr().expect("addr");
        tokio::spawn(async move {
            loop {
                let Ok((mut stream, _)) = listener.accept().await else {
                    break;
                };
                let mut buf = vec![0_u8; 4096];
                let _ = stream.read(&mut buf).await;
                let req = String::from_utf8_lossy(&buf);
                let line = req.lines().next().unwrap_or("");
                let stats = line.contains("/v1/hofund/sessions/stats");
                let code = if stats { stats_status } else { list_status };
                let body = if code == 200 && stats {
                    r#"{"version":1,"revision":"","tokens":{"input":0,"output":0,"cache_read":0,"cache_write":0},"cost":0.0,"unpriced":0,"days":[],"models":[],"folders":[]}"#.to_owned()
                } else if code == 200 {
                    r#"{"version":1,"revision":"","consent":{"asked":false,"all":false,"enabled":[]},"sessions":[]}"#.to_owned()
                } else {
                    r#"{"version":1,"error":{"kind":"not_found","message":"missing","request_id":"test","retry_after_ms":null,"details":[]}}"#.to_owned()
                };
                let response = format!(
                    "HTTP/1.1 {code} X\r\ncontent-type: application/json\r\ncontent-length: {}\r\nconnection: close\r\n\r\n{body}",
                    body.len()
                );
                let _ = stream.write_all(response.as_bytes()).await;
            }
        });
        addr
    }

    #[tokio::test]
    async fn schedule_sessions_fetch_maps_a_stats_404_after_a_successful_list() {
        let (tx, mut rx) = mpsc::channel(4);
        let addr = serve_list_then_stats(200, 404).await;
        let client = Client::tcp(addr, "t");
        super::schedule_sessions_fetch(
            Some(&client),
            &tx,
            Default::default(),
            Default::default(),
            7,
        );
        let super::WorkerMsg::SessionsLoad {
            generation,
            outcome,
        } = recv_msg(&mut rx).await
        else {
            panic!("expected sessions load");
        };
        assert_eq!(generation, 7);
        assert!(
            matches!(*outcome, super::SessionsLoadOutcome::NotInBuild),
            "stats 404 must be NotInBuild, not a generic error: {outcome:?}"
        );

        let addr = serve_list_then_stats(200, 500).await;
        let client = Client::tcp(addr, "t");
        super::schedule_sessions_fetch(
            Some(&client),
            &tx,
            Default::default(),
            Default::default(),
            8,
        );
        let super::WorkerMsg::SessionsLoad { outcome, .. } = recv_msg(&mut rx).await else {
            panic!("expected sessions load");
        };
        assert!(
            matches!(*outcome, super::SessionsLoadOutcome::Err(_)),
            "a non-404 stats error must stay an error: {outcome:?}"
        );

        let addr = serve_list_then_stats(200, 200).await;
        let client = Client::tcp(addr, "t");
        super::schedule_sessions_fetch(
            Some(&client),
            &tx,
            Default::default(),
            Default::default(),
            9,
        );
        let super::WorkerMsg::SessionsLoad { outcome, .. } = recv_msg(&mut rx).await else {
            panic!("expected sessions load");
        };
        assert!(
            matches!(*outcome, super::SessionsLoadOutcome::Ok { .. }),
            "list and stats 200 must succeed: {outcome:?}"
        );
    }
}
