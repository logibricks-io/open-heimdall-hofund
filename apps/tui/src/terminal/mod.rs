//! Terminal entry, async loop, and live gateway dependencies.

pub(crate) mod deps;
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
use crate::gateway::{HandoffState, WATCH_INTERVAL, run_handoff_tick};
use crate::pages::read_path;
use crate::quit::{QuitAction, perform_quit};
use crate::runtime::{
    HandoffSnapshot, WorkerMsg, apply_control_followup, apply_worker_msg, open_event_stream,
    schedule_page_probe,
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
    spawn_handoff_worker(
        Arc::clone(&live),
        config_path.clone(),
        listen,
        work_tx.clone(),
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
                open_control_if_needed(&live, &mut control, &app, &work_tx).await?;
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
                            reconnect_after_token(&live, config, &mut control).await?;
                        } else if followup.resync_stream {
                            reopen_control_stream(&live, &mut control, None).await?;
                        }
                        schedule_probe_for_open_page(&app, live.client().as_ref(), &work_tx);
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
                        if app.handle_key(key) == AppAction::Quit {
                            break;
                        }
                        schedule_probe_for_open_page(&app, live.client().as_ref(), &work_tx);
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

    perform_quit(config, &handoff_snapshot, live.client(), live.as_ref()).await
}

fn spawn_handoff_worker(
    live: Arc<deps::LiveHandoff>,
    config: PathBuf,
    listen: std::net::SocketAddr,
    work_tx: mpsc::Sender<WorkerMsg>,
) {
    tokio::spawn(async move {
        let mut watch = tokio::time::interval(WATCH_INTERVAL);
        watch.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Skip);
        let tokens = TokenReader::new(config.clone());
        let mut state = HandoffState::default();
        loop {
            watch.tick().await;
            let outcome = run_handoff_tick(
                live.as_ref(),
                &config,
                &mut || tokens.read(),
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
    });
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
    control: &mut Option<ControlEventStream>,
    app: &App,
    work_tx: &mpsc::Sender<WorkerMsg>,
) -> Result<(), String> {
    if live.client().is_none() {
        *control = None;
        return Ok(());
    }
    if control.is_some() {
        return Ok(());
    }
    reopen_control_stream(live, control, None).await?;
    schedule_probe_for_open_page(app, live.client().as_ref(), work_tx);
    Ok(())
}

async fn reopen_control_stream(
    live: &deps::LiveHandoff,
    control: &mut Option<ControlEventStream>,
    last_event_id: Option<&str>,
) -> Result<(), String> {
    let Some(client) = live.client() else {
        *control = None;
        return Ok(());
    };
    *control = Some(open_event_stream(&client, last_event_id).await?);
    Ok(())
}

async fn reconnect_after_token(
    live: &deps::LiveHandoff,
    config: &Path,
    control: &mut Option<ControlEventStream>,
) -> Result<(), String> {
    *control = None;
    let token = read_token(config);
    let _client = live.rediscover(config, &token).await?;
    reopen_control_stream(live, control, None).await
}

fn schedule_probe_for_open_page(
    app: &App,
    client: Option<&hofund_control_client::Client>,
    work_tx: &mpsc::Sender<WorkerMsg>,
) {
    let page = app.page;
    if read_path(page).is_some() {
        let generation = app.pages.page(page).fetch_gen();
        schedule_page_probe(client, work_tx, page, generation);
    }
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
    use crossterm::terminal::{disable_raw_mode, enable_raw_mode};

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
