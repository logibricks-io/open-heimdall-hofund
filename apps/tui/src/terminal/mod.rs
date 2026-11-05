//! Terminal entry, async loop, and live gateway dependencies.

mod deps;
mod spawn;

use std::io;
use std::path::Path;

use crossterm::event::{Event, EventStream, KeyEventKind};
use crossterm::execute;
use crossterm::terminal::{
    EnterAlternateScreen, LeaveAlternateScreen, disable_raw_mode, enable_raw_mode,
};
use futures_util::StreamExt;
use hofund_config::{effective_listen, load_path};
use hofund_control_client::hofund_control_proto::ShutdownRequest;
use ratatui::Terminal;
use ratatui::backend::CrosstermBackend;

use crate::app::{App, AppAction};
use crate::gateway::{HandoffState, QuitDecision, WATCH_INTERVAL, decide_quit, run_handoff_tick};
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
    result
}

fn run_async(config: &Path, no_color: bool) -> Result<(), String> {
    let runtime = tokio::runtime::Builder::new_multi_thread()
        .enable_all()
        .build()
        .map_err(|error| format!("hofund-tui: runtime failed: {error}"))?;
    runtime.block_on(async_main(config, no_color))
}

async fn async_main(config: &Path, no_color: bool) -> Result<(), String> {
    let theme = Theme::new(no_color);
    let mut app = App::new(PageId::Agents);
    app.gateway = GatewayPresence::Unknown;

    let mut terminal = setup_terminal()?;
    let size = terminal.size().map_err(|e| e.to_string())?;
    app.resize(size.width, size.height);

    let live = deps::LiveHandoff::new(config);
    let listen = resolve_listen(config)?;
    let mut handoff = HandoffState::default();
    if let crate::gateway::HandoffOutcome::Update(presence) =
        run_handoff_tick(&live, config, &read_token(config), listen, &mut handoff).await
    {
        app.gateway = presence;
    }

    let mut events = EventStream::new();
    let mut watch = tokio::time::interval(WATCH_INTERVAL);
    watch.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Skip);

    loop {
        tokio::select! {
            maybe = events.next() => {
                let Some(event) = maybe else { break };
                match event {
                    Ok(Event::Key(key)) if key.kind == KeyEventKind::Press => {
                        if app.handle_key(key) == AppAction::Quit {
                            break;
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
            _ = watch.tick() => {
                if let crate::gateway::HandoffOutcome::Update(presence) =
                    run_handoff_tick(&live, config, &read_token(config), listen, &mut handoff).await
                {
                    app.gateway = presence;
                    draw(&mut terminal, &app, &theme)?;
                }
            }
        }
    }

    let decision = decide_quit(handoff.spawned, false);
    match decision {
        QuitDecision::LeaveRunning => {
            eprintln!("hofund-tui: agents still connected; leaving the daemon running.");
        }
        QuitDecision::ShutdownSpawned => {
            if let Some(client) = live.client() {
                let _ = client.shutdown(ShutdownRequest::default()).await;
            }
        }
        QuitDecision::ExitAttached => {}
    }
    Ok(())
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
    let _ = terminal
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
