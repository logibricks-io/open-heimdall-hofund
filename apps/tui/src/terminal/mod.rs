//! Terminal entry, async loop, and live gateway dependencies.

pub(crate) mod deps;
pub(crate) mod resume;
pub(crate) mod spawn;

use std::io;
use std::path::{Path, PathBuf};
use std::sync::Arc;

use crossterm::event::{Event, EventStream, KeyEventKind};
use crossterm::execute;
use crossterm::terminal::{
    EnterAlternateScreen, LeaveAlternateScreen, disable_raw_mode, enable_raw_mode,
};
use futures_util::StreamExt;
use hofund_config::{effective_listen, load_path};
use hofund_control_client::EventStream as ControlEventStream;
use ratatui::Terminal;
use ratatui::backend::CrosstermBackend;
use tokio::sync::mpsc;

use crate::app::{App, AppAction};
use crate::gateway::{HandoffDeps, HandoffState, WATCH_INTERVAL, run_handoff_tick};
use crate::quit::{QuitAction, perform_quit};
use crate::runtime::{
    HandoffSnapshot, WorkerMsg, apply_control_followup, apply_worker_msg, open_event_stream,
    schedule_open_page_load,
};
use crate::state::{GatewayPresence, PageId};
use crate::theme::Theme;
use crate::ui;

/// Runs the UI until the user quits.
///
/// # Errors
///
/// Returns a message when raw mode or the terminal backend fails.
pub fn run(config: &Path, no_color: bool) -> Result<(), String> {
    if let Err(error) = enable_raw_mode() {
        eprintln!("hofund-tui: cannot enable raw mode: {error}");
        std::process::exit(1);
    }
    let mut stdout = io::stdout();
    if let Err(error) = execute!(stdout, EnterAlternateScreen, crossterm::cursor::Hide) {
        let _ = disable_raw_mode();
        return Err(format!("hofund-tui: terminal setup failed: {error}"));
    }
    let result = run_async(config, no_color);
    let _ = disable_raw_mode();
    let _ = execute!(stdout, LeaveAlternateScreen, crossterm::cursor::Show);
    match result {
        Ok(QuitAction::LeaveRunningNotice(message)) => {
            eprintln!("{message}");
            Ok(())
        }
        Ok(QuitAction::None) => Ok(()),
        Err(error) => Err(error),
    }
}

fn run_async(config: &Path, no_color: bool) -> Result<QuitAction, String> {
    let runtime = tokio::runtime::Builder::new_multi_thread()
        .enable_all()
        .build()
        .map_err(|error| format!("hofund-tui: runtime failed: {error}"))?;
    runtime.block_on(async_main(config, no_color))
}

async fn async_main(config: &Path, no_color: bool) -> Result<QuitAction, String> {
    let theme = Theme::new(no_color);
    let mut app = App::new(PageId::Agents);
    app.gateway = GatewayPresence::Unknown;

    let mut terminal = setup_terminal()?;
    let size = terminal.size().map_err(|e| e.to_string())?;
    app.resize(size.width, size.height);
    draw(&mut terminal, &app, &theme)?;

    let config_path = config.to_path_buf();
    let listen = resolve_listen(config)?;
    let live = Arc::new(deps::LiveHandoff::new(config));
    let mut handoff_snapshot = HandoffSnapshot::default();
    let (work_tx, mut work_rx) = mpsc::channel(32);
    let (handoff_stop_tx, handoff_stop_rx) = tokio::sync::watch::channel(false);
    let handoff_task = spawn_handoff_worker(
        Arc::clone(&live),
        config_path.clone(),
        listen,
        work_tx.clone(),
        handoff_stop_rx,
    );

    let mut keys = EventStream::new();
    let mut control: Option<ControlEventStream> = None;

    loop {
        tokio::select! {
            biased;
            msg = work_rx.recv() => {
                let Some(msg) = msg else { break };
                if let WorkerMsg::Handoff { snapshot, .. } | WorkerMsg::HandoffProgress { snapshot } = &msg {
                    handoff_snapshot = snapshot.clone();
                }
                apply_worker_msg(&mut app, msg);
                open_control_if_needed(&live, &mut app, &mut control, &work_tx).await;
                draw(&mut terminal, &app, &theme)?;
            }
            event = async {
                match control.as_mut() {
                    Some(stream) => stream.next().await,
                    None => std::future::pending().await,
                }
            }, if control.is_some() => {
                match event {
                    None => control = None,
                    Some(Ok(envelope)) => {
                        let followup = apply_control_followup(&mut app, &envelope.payload);
                        if followup.reread_token {
                            reconnect_after_token(&live, config, &mut app, &mut control).await;
                        } else if followup.resync_stream {
                            try_reopen_control_stream(&live, &mut app, &mut control, None).await;
                        }
                        schedule_open_page_load(&app, live.client().as_ref(), &work_tx);
                        draw(&mut terminal, &app, &theme)?;
                    }
                    Some(Err(error)) => {
                        control = None;
                        app.set_flash(crate::state::Flash::new(error.to_string(), false));
                        draw(&mut terminal, &app, &theme)?;
                    }
                }
            }
            maybe = keys.next() => {
                let Some(event) = maybe else { break };
                match event {
                    Ok(Event::Key(key)) if key.kind == KeyEventKind::Press => {
                        match app.handle_key(key) {
                            AppAction::Quit => break,
                            AppAction::Reload => {
                                schedule_open_page_load(&app, live.client().as_ref(), &work_tx);
                            }
                            AppAction::Resume(resume) => match run_resume(
                                &mut terminal,
                                &app,
                                &theme,
                                &resume,
                            ) {
                                Ok(()) => app.set_flash(crate::state::Flash::new(
                                    "back from the session",
                                    true,
                                )),
                                Err(message) => {
                                    app.set_flash(crate::state::Flash::new(message, false))
                                }
                            },
                            AppAction::Flash { text, ok } => {
                                app.set_flash(crate::state::Flash::new(text, ok));
                            }
                            AppAction::None => {}
                        }
                        draw(&mut terminal, &app, &theme)?;
                    }
                    Ok(Event::Resize(_, _)) => {
                        let size = terminal.size().map_err(|e| e.to_string())?;
                        app.resize(size.width, size.height);
                        draw(&mut terminal, &app, &theme)?;
                    }
                    Ok(_) => {}
                    Err(error) => return Err(format!("hofund-tui: input error: {error}")),
                }
            }
        }
    }

    let _ = shutdown_handoff_worker(&handoff_stop_tx, handoff_task).await;

    perform_quit(config, &handoff_snapshot, live.client(), live.as_ref()).await
}

async fn shutdown_handoff_worker(
    handoff_stop_tx: &tokio::sync::watch::Sender<bool>,
    handoff_task: tokio::task::JoinHandle<()>,
) {
    let _ = handoff_stop_tx.send(true);
    handoff_task.abort();
    let _ = handoff_task.await;
}

fn spawn_handoff_worker(
    live: Arc<deps::LiveHandoff>,
    config: PathBuf,
    listen: std::net::SocketAddr,
    work_tx: mpsc::Sender<WorkerMsg>,
    handoff_stop: tokio::sync::watch::Receiver<bool>,
) -> tokio::task::JoinHandle<()> {
    tokio::spawn(async move {
        let tokens = TokenReader::new(config.clone());
        run_handoff_worker_loop(live, config, listen, work_tx, handoff_stop, move || {
            tokens.read()
        })
        .await;
    })
}

async fn run_handoff_worker_loop<D, R>(
    deps: Arc<D>,
    config: PathBuf,
    listen: std::net::SocketAddr,
    work_tx: mpsc::Sender<WorkerMsg>,
    mut handoff_stop: tokio::sync::watch::Receiver<bool>,
    mut read_token: R,
) where
    D: HandoffDeps + Send + Sync,
    R: FnMut() -> String + Send,
{
    let mut watch = tokio::time::interval(WATCH_INTERVAL);
    watch.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Skip);
    let mut state = HandoffState::default();
    loop {
        if *handoff_stop.borrow() {
            break;
        }
        tokio::select! {
            biased;
            changed = handoff_stop.changed() => {
                if changed.is_ok() && *handoff_stop.borrow() {
                    break;
                }
            }
            _ = watch.tick() => {
                let outcome = run_handoff_tick(
                    deps.as_ref(),
                    &config,
                    &mut read_token,
                    listen,
                    &mut state,
                    &mut |state| {
                        let snapshot = HandoffSnapshot {
                            spawned: state.spawned,
                            child_pid: state.child_pid,
                        };
                        let _ = work_tx.try_send(WorkerMsg::HandoffProgress { snapshot });
                    },
                )
                .await;
                let snapshot = HandoffSnapshot {
                    spawned: state.spawned,
                    child_pid: state.child_pid,
                };
                if work_tx
                    .send(WorkerMsg::Handoff { outcome, snapshot })
                    .await
                    .is_err()
                {
                    break;
                }
            }
        }
    }
}

struct TokenReader {
    config: PathBuf,
}

impl TokenReader {
    fn new(config: PathBuf) -> Self {
        Self { config }
    }

    fn read(&self) -> String {
        read_token(&self.config)
    }
}

async fn open_control_if_needed(
    live: &deps::LiveHandoff,
    app: &mut App,
    control: &mut Option<ControlEventStream>,
    work_tx: &mpsc::Sender<WorkerMsg>,
) {
    if live.client().is_none() {
        *control = None;
        return;
    }
    if control.is_some() {
        return;
    }
    try_reopen_control_stream(live, app, control, None).await;
    schedule_open_page_load(app, live.client().as_ref(), work_tx);
}

/// Records a failed control stream open without exiting the UI loop.
pub(crate) fn apply_event_stream_open_failure<T>(
    app: &mut App,
    control: &mut Option<T>,
    message: String,
) {
    *control = None;
    app.set_flash(crate::state::Flash::new(message, false));
}

async fn try_reopen_control_stream(
    live: &deps::LiveHandoff,
    app: &mut App,
    control: &mut Option<ControlEventStream>,
    last_event_id: Option<&str>,
) {
    let Some(client) = live.client() else {
        *control = None;
        return;
    };
    match open_event_stream(&client, last_event_id).await {
        Ok(stream) => *control = Some(stream),
        Err(message) => apply_event_stream_open_failure(app, control, message),
    }
}

async fn reconnect_after_token(
    live: &deps::LiveHandoff,
    config: &Path,
    app: &mut App,
    control: &mut Option<ControlEventStream>,
) {
    *control = None;
    let token = read_token(config);
    match live.rediscover(config, &token).await {
        Ok(_client) => try_reopen_control_stream(live, app, control, None).await,
        Err(message) => apply_event_stream_open_failure(app, control, message),
    }
}

fn run_resume(
    terminal: &mut Terminal<CrosstermBackend<io::Stdout>>,
    app: &App,
    theme: &Theme,
    resume: &hofund_control_proto::SessionResume,
) -> Result<(), String> {
    let _ = disable_raw_mode();
    let _ = execute!(io::stdout(), LeaveAlternateScreen, crossterm::cursor::Show);
    let result = resume::run_foreground(resume);
    let _ = enable_raw_mode();
    let _ = execute!(io::stdout(), EnterAlternateScreen, crossterm::cursor::Hide);
    draw(terminal, app, theme)?;
    result
}

fn setup_terminal() -> Result<Terminal<CrosstermBackend<io::Stdout>>, String> {
    let backend = CrosstermBackend::new(io::stdout());
    Terminal::new(backend).map_err(|error| format!("hofund-tui: backend failed: {error}"))
}

fn draw(
    terminal: &mut Terminal<CrosstermBackend<io::Stdout>>,
    app: &App,
    theme: &Theme,
) -> Result<(), String> {
    terminal
        .draw(|frame| ui::draw(app, frame.area(), frame.buffer_mut(), theme))
        .map_err(|error| error.to_string())?;
    Ok(())
}

fn resolve_listen(config: &Path) -> Result<std::net::SocketAddr, String> {
    let loaded = load_path(config).map_err(|error| error.to_string())?;
    let (addr, _) = effective_listen(&loaded, std::env::var("HOFUND_ADDR").ok().as_deref())
        .map_err(|error| error.to_string())?;
    Ok(addr)
}

fn read_token(config: &Path) -> String {
    load_path(config)
        .ok()
        .and_then(|loaded| {
            let base = config.parent().unwrap_or_else(|| Path::new("."));
            hofund_secrets::TokenFile::new(base.join(&loaded.token_file))
                .read()
                .ok()
        })
        .unwrap_or_default()
}

#[cfg(test)]
mod tests {
    use std::sync::Arc;

    use crossterm::terminal::{disable_raw_mode, enable_raw_mode};

    use super::apply_event_stream_open_failure;
    use super::open_control_if_needed;
    use super::run_handoff_worker_loop;
    use super::shutdown_handoff_worker;
    use crate::app::App;
    use crate::gateway::{HandoffDeps, HandoffOutcome};
    use crate::runtime::{HandoffSnapshot, WorkerMsg, apply_worker_msg};
    use crate::state::PageId;
    use crate::terminal::deps::LiveHandoff;
    use hofund_control_client::hofund_control_proto::StatusResponse;
    use hofund_control_client::{Client, ClientError};
    use std::net::{IpAddr, Ipv4Addr, SocketAddr};
    use std::path::{Path, PathBuf};
    use std::sync::Mutex;

    struct HandoffWorkerFake {
        spawn_count: Mutex<u32>,
    }

    impl HandoffWorkerFake {
        fn new() -> Self {
            Self {
                spawn_count: Mutex::new(0),
            }
        }
    }

    impl HandoffDeps for HandoffWorkerFake {
        fn held_locks(&self, _config: &Path) -> Result<Vec<PathBuf>, String> {
            Ok(Vec::new())
        }

        async fn discover_attached(
            &self,
            _config: &Path,
            _token: &str,
        ) -> Result<Client, ClientError> {
            Err(ClientError::ConnectionUnavailable("no daemon".to_owned()))
        }

        async fn probe_model_port(&self, _listen: SocketAddr) -> Result<(), ClientError> {
            Err(ClientError::ConnectionUnavailable("quiet".to_owned()))
        }

        fn spawn_daemon(&self, _config: &Path) -> Result<u32, String> {
            *self.spawn_count.lock().expect("spawn") += 1;
            Ok(999)
        }

        async fn fetch_status(&self, _client: &Client) -> Result<StatusResponse, ClientError> {
            Err(ClientError::ConnectionUnavailable("unused".to_owned()))
        }

        fn process_alive(&self, _pid: u32) -> bool {
            true
        }
    }

    fn listen() -> SocketAddr {
        SocketAddr::new(IpAddr::V4(Ipv4Addr::LOCALHOST), 9)
    }

    #[test]
    fn event_stream_reopen_failure_keeps_control_none_and_sets_flash() {
        let mut app = App::for_test(PageId::Agents);
        let mut control: Option<u32> = Some(1);
        apply_event_stream_open_failure(
            &mut app,
            &mut control,
            "control event stream failed".into(),
        );
        assert!(control.is_none());
        assert_eq!(
            app.flash.as_ref().map(|flash| flash.text.as_str()),
            Some("control event stream failed")
        );
        assert!(app.flash.as_ref().is_some_and(|flash| !flash.ok));
    }

    #[tokio::test]
    async fn handoff_message_then_event_stream_open_failure_keeps_ui_open() {
        let config = std::path::Path::new("/tmp/hofund-tui-test-config.toml");
        let live = LiveHandoff::new(config);
        live.set_client_for_test(hofund_control_client::Client::tcp(
            "127.0.0.1:1".parse().expect("addr"),
            "test-token",
        ));
        let mut app = App::for_test(PageId::Agents);
        apply_worker_msg(
            &mut app,
            WorkerMsg::Handoff {
                outcome: HandoffOutcome::Idle,
                snapshot: HandoffSnapshot {
                    spawned: true,
                    child_pid: Some(42),
                },
            },
        );
        let mut control = None;
        let (work_tx, _work_rx) = tokio::sync::mpsc::channel(4);
        open_control_if_needed(&live, &mut app, &mut control, &work_tx).await;
        assert!(control.is_none());
        assert!(
            app.flash.as_ref().is_some_and(|flash| !flash.ok),
            "expected error flash after failed stream open"
        );
        assert!(
            app.flash
                .as_ref()
                .is_some_and(|flash| !flash.text.is_empty()),
            "failure should surface as status flash text"
        );
    }

    #[tokio::test]
    async fn handoff_worker_exits_without_spawning_when_stopped_up_front() {
        tokio::time::pause();
        let deps = Arc::new(HandoffWorkerFake::new());
        let (work_tx, mut work_rx) = tokio::sync::mpsc::channel(4);
        let (_stop_tx, stop_rx) = tokio::sync::watch::channel(true);
        let worker = tokio::spawn(run_handoff_worker_loop(
            deps.clone(),
            PathBuf::from("/tmp/x.toml"),
            listen(),
            work_tx,
            stop_rx,
            || "t".to_owned(),
        ));
        tokio::time::timeout(std::time::Duration::from_secs(60), worker)
            .await
            .expect("worker must exit when stop is already set")
            .expect("worker join");
        while work_rx.try_recv().is_ok() {}
        assert_eq!(*deps.spawn_count.lock().expect("spawn"), 0);
    }

    #[tokio::test]
    async fn shutdown_handoff_worker_stops_production_watch_loop() {
        tokio::time::pause();
        let deps = Arc::new(HandoffWorkerFake::new());
        let (work_tx, _work_rx) = tokio::sync::mpsc::channel(8);
        let (stop_tx, stop_rx) = tokio::sync::watch::channel(false);
        let worker = tokio::spawn(run_handoff_worker_loop(
            Arc::clone(&deps),
            PathBuf::from("/tmp/x.toml"),
            listen(),
            work_tx,
            stop_rx,
            || "t".to_owned(),
        ));
        tokio::task::yield_now().await;
        let spawns_at_shutdown = *deps.spawn_count.lock().expect("spawn");
        shutdown_handoff_worker(&stop_tx, worker).await;
        tokio::time::advance(
            crate::gateway::SPAWN_WAIT
                + crate::gateway::WATCH_INTERVAL
                + std::time::Duration::from_secs(1),
        )
        .await;
        tokio::task::yield_now().await;
        assert_eq!(*deps.spawn_count.lock().expect("spawn"), spawns_at_shutdown);
    }

    #[test]
    fn raw_mode_reports_not_a_tty_gracefully() {
        match enable_raw_mode() {
            Ok(()) => {
                let _ = disable_raw_mode();
            }
            Err(error) => {
                let msg = error.to_string().to_lowercase();
                assert!(
                    msg.contains("not a terminal") || msg.contains("tty") || msg.contains("device"),
                    "{msg}"
                );
            }
        }
    }
}
