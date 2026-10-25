//! Menu-bar application for the local gateway.
//!
//! The window and the tray render a `control::Snapshot`. Start, stop,
//! connect, disconnect, usage, and open-config call `hofund-daemon` and
//! `hofund-agent`. The agent rows come from the catalog, in catalog order.

mod control;
mod open;

use control::{Control, MenuEntry, Snapshot};
use hofund_agent::Session;
use hofund_daemon::user_config_path;
use tauri::Emitter;
use tauri::Manager;
use tauri::menu::{CheckMenuItem, Menu, MenuItem, PredefinedMenuItem};
use tauri::tray::TrayIconBuilder;
use tokio::sync::Mutex;

struct AppState {
    control: Mutex<Control>,
}

/// Starts the menu-bar process and runs until Quit.
///
/// # Errors
///
/// Returns an error when the tray or the window cannot be created.
pub fn run() -> Result<(), String> {
    let state = AppState {
        control: Mutex::new(Control::open(user_config_path())),
    };
    tauri::Builder::default()
        .manage(state)
        .invoke_handler(tauri::generate_handler![
            snapshot,
            start_gateway,
            stop_gateway,
            set_agent,
            usage_summary,
            open_config,
        ])
        .setup(|app| {
            let snapshot = {
                let state = app.state::<AppState>();
                let mut control = state.control.blocking_lock();
                control.snapshot(&Session::from_process_env())
            };
            install_tray(app.handle(), &snapshot)?;
            if let Some(window) = app.get_webview_window("usage") {
                let handle = app.handle().clone();
                window.on_window_event(move |event| {
                    if let tauri::WindowEvent::CloseRequested { api, .. } = event {
                        api.prevent_close();
                        if let Some(window) = handle.get_webview_window("usage") {
                            let _ = window.hide();
                        }
                    }
                });
            }
            let handle = app.handle().clone();
            app.on_menu_event(move |_app, event| {
                let id = event.id().as_ref().to_owned();
                let handle = handle.clone();
                tauri::async_runtime::spawn(async move {
                    dispatch(&handle, &id).await;
                });
            });
            Ok(())
        })
        .run(tauri::generate_context!())
        .map_err(|error| error.to_string())
}

fn install_tray(app: &tauri::AppHandle, snapshot: &Snapshot) -> tauri::Result<()> {
    let menu = build_menu(app, snapshot)?;
    let builder = TrayIconBuilder::with_id("hofund")
        .icon(tray_icon()?)
        .menu(&menu)
        .show_menu_on_left_click(true);
    // macOS treats a template image as a mask and tints it for the menu bar.
    #[cfg(target_os = "macos")]
    let builder = builder.icon_as_template(true);
    builder.build(app)?;
    Ok(())
}

/// Menu-bar image. macOS uses the black template set. Windows and Linux use the colour set.
fn tray_icon() -> tauri::Result<tauri::image::Image<'static>> {
    #[cfg(target_os = "macos")]
    {
        // 44 px source. `tray-icon` draws it at 18 pt, so the retina pixels stay sharp.
        tauri::image::Image::from_bytes(include_bytes!("../icons/tray/tray-template-22@2x.png"))
    }
    #[cfg(not(target_os = "macos"))]
    {
        tauri::image::Image::from_bytes(include_bytes!("../icons/tray/tray-color-32.png"))
    }
}

fn build_menu(app: &tauri::AppHandle, snapshot: &Snapshot) -> tauri::Result<Menu<tauri::Wry>> {
    let menu = Menu::new(app)?;
    for entry in control::menu_entries(snapshot) {
        append_entry(app, &menu, &entry)?;
    }
    Ok(menu)
}

fn append_entry(
    app: &tauri::AppHandle,
    menu: &Menu<tauri::Wry>,
    entry: &MenuEntry,
) -> tauri::Result<()> {
    if entry.id.starts_with("separator") {
        let item = PredefinedMenuItem::separator(app)?;
        menu.append(&item)?;
        return Ok(());
    }
    if let Some(checked) = entry.checked {
        let item = CheckMenuItem::with_id(
            app,
            &entry.id,
            &entry.label,
            entry.enabled,
            checked,
            None::<&str>,
        )?;
        menu.append(&item)?;
        return Ok(());
    }
    let item = MenuItem::with_id(app, &entry.id, &entry.label, entry.enabled, None::<&str>)?;
    menu.append(&item)?;
    Ok(())
}

async fn dispatch(app: &tauri::AppHandle, id: &str) {
    let state = app.state::<AppState>();
    let mut control = state.control.lock().await;
    let session = Session::from_process_env();
    match id {
        "gateway-start" => {
            let _ = control.start().await;
        }
        "gateway-stop" => {
            let _ = control.stop().await;
        }
        "usage" => {
            if let Some(window) = app.get_webview_window("usage") {
                let _ = window.show();
                let _ = window.set_focus();
            }
        }
        "open-config" => {
            let _ = control.open_config();
        }
        "quit" => {
            let _ = control.stop().await;
            app.exit(0);
            return;
        }
        other => {
            if let Some(agent) = other.strip_prefix("agent:") {
                let connected = control
                    .snapshot(&session)
                    .agents
                    .iter()
                    .find(|row| row.id == agent)
                    .is_some_and(|row| row.connected);
                let _ = control.set_agent(&session, agent, !connected);
            }
        }
    }
    let snapshot = control.snapshot(&session);
    drop(control);
    let _ = publish(app, &snapshot);
}

fn publish(app: &tauri::AppHandle, snapshot: &Snapshot) -> Result<(), String> {
    if let Some(tray) = app.tray_by_id("hofund") {
        let menu = build_menu(app, snapshot).map_err(|error| error.to_string())?;
        tray.set_menu(Some(menu))
            .map_err(|error| error.to_string())?;
    }
    app.emit("snapshot", snapshot)
        .map_err(|error| error.to_string())
}

#[tauri::command]
async fn snapshot(
    app: tauri::AppHandle,
    state: tauri::State<'_, AppState>,
) -> Result<Snapshot, String> {
    let mut control = state.control.lock().await;
    let snapshot = control.snapshot(&Session::from_process_env());
    drop(control);
    publish(&app, &snapshot)?;
    Ok(snapshot)
}

#[tauri::command]
async fn start_gateway(
    app: tauri::AppHandle,
    state: tauri::State<'_, AppState>,
) -> Result<Snapshot, String> {
    let mut control = state.control.lock().await;
    let _ = control.start().await;
    let snapshot = control.snapshot(&Session::from_process_env());
    drop(control);
    publish(&app, &snapshot)?;
    Ok(snapshot)
}

#[tauri::command]
async fn stop_gateway(
    app: tauri::AppHandle,
    state: tauri::State<'_, AppState>,
) -> Result<Snapshot, String> {
    let mut control = state.control.lock().await;
    let _ = control.stop().await;
    let snapshot = control.snapshot(&Session::from_process_env());
    drop(control);
    publish(&app, &snapshot)?;
    Ok(snapshot)
}

#[tauri::command]
async fn set_agent(
    app: tauri::AppHandle,
    state: tauri::State<'_, AppState>,
    id: String,
    connect: bool,
) -> Result<Snapshot, String> {
    let mut control = state.control.lock().await;
    let session = Session::from_process_env();
    let _ = control.set_agent(&session, &id, connect);
    let snapshot = control.snapshot(&session);
    drop(control);
    publish(&app, &snapshot)?;
    Ok(snapshot)
}

#[tauri::command]
async fn usage_summary(state: tauri::State<'_, AppState>) -> Result<String, String> {
    let mut control = state.control.lock().await;
    control.usage_summary()
}

#[tauri::command]
async fn open_config(state: tauri::State<'_, AppState>) -> Result<(), String> {
    let mut control = state.control.lock().await;
    control.open_config()
}
