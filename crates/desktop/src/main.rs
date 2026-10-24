//! The Slipstream desktop app: a window over the download manager, a tray
//! icon, and notifications. The page in `ui/` talks to it through the
//! commands below and listens for `slipstream` events.

#![cfg_attr(not(debug_assertions), windows_subsystem = "windows")]

use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, Ordering};

use slipstream_manager::{AddReport, Event, Id, Item, Manager, Settings};
use tauri::menu::{Menu, MenuItem, PredefinedMenuItem};
use tauri::tray::{MouseButton, MouseButtonState, TrayIconBuilder, TrayIconEvent};
use tauri::{AppHandle, Emitter, Manager as _, RunEvent, State, WindowEvent};
use tauri_plugin_dialog::DialogExt;
use tauri_plugin_notification::NotificationExt;
use tauri_plugin_opener::OpenerExt;

const TRAY_ID: &str = "main";

/// Set once downloads are paused and saved, so the app may really exit.
static READY_TO_EXIT: AtomicBool = AtomicBool::new(false);

type CmdResult<T = ()> = Result<T, String>;

#[tauri::command]
fn list(manager: State<Manager>) -> Vec<Item> {
    manager.items()
}

#[tauri::command]
fn get_settings(manager: State<Manager>) -> Settings {
    manager.settings()
}

#[tauri::command]
fn set_settings(manager: State<Manager>, settings: Settings) -> Settings {
    manager.set_settings(settings)
}

#[tauri::command]
fn add(manager: State<Manager>, urls: Vec<String>, start: bool) -> AddReport {
    manager.add(&urls, &[], start)
}

#[tauri::command]
fn pause(manager: State<Manager>, ids: Vec<Id>) {
    manager.pause(&ids);
}

#[tauri::command]
fn resume(manager: State<Manager>, ids: Vec<Id>) {
    manager.resume(&ids);
}

#[tauri::command]
fn pause_all(manager: State<Manager>) {
    manager.pause_all();
}

#[tauri::command]
fn resume_all(manager: State<Manager>) {
    manager.resume_all();
}

#[tauri::command]
async fn remove(manager: State<'_, Manager>, ids: Vec<Id>, delete_files: bool) -> CmdResult {
    manager.remove(&ids, delete_files).await;
    Ok(())
}

/// Opens a finished download with its default app.
#[tauri::command]
fn open_file(app: AppHandle, manager: State<Manager>, id: Id) -> CmdResult {
    let path = find(&manager, id)?
        .path
        .ok_or("this download has not started yet")?;
    open_path(&app, &path)
}

/// Shows the download in Explorer, Finder or the file manager.
#[tauri::command]
fn show_in_folder(app: AppHandle, manager: State<Manager>, id: Id) -> CmdResult {
    let item = find(&manager, id)?;
    match item.path {
        Some(path) if path.exists() => app
            .opener()
            .reveal_item_in_dir(&path)
            .map_err(|e| e.to_string()),
        Some(path) => open_path(&app, path.parent().unwrap_or(&path)),
        None => open_path(&app, &manager.settings().download_dir),
    }
}

#[tauri::command]
fn open_download_folder(app: AppHandle, manager: State<Manager>) -> CmdResult {
    let dir = manager.settings().download_dir;
    std::fs::create_dir_all(&dir).map_err(|e| e.to_string())?;
    open_path(&app, &dir)
}

/// Asks the user for a folder; `None` if they cancel.
#[tauri::command]
async fn pick_folder(app: AppHandle) -> Option<String> {
    app.dialog()
        .file()
        .blocking_pick_folder()
        .map(|p| p.to_string())
}

fn find(manager: &Manager, id: Id) -> CmdResult<Item> {
    manager
        .items()
        .into_iter()
        .find(|i| i.id == id)
        .ok_or_else(|| "that download is no longer in the list".to_string())
}

fn open_path(app: &AppHandle, path: &Path) -> CmdResult {
    app.opener()
        .open_path(path.to_string_lossy(), None::<&str>)
        .map_err(|e| e.to_string())
}

/// Passes manager events to the page, and acts on the ones the app cares
/// about itself.
fn forward(app: &AppHandle, event: Event) {
    match &event {
        Event::Finished { item } => notify_finished(app, item),
        Event::Progress { items, total_speed } => update_tray(app, items.len(), *total_speed),
        _ => {}
    }
    let _ = app.emit("slipstream", event);
}

fn notify_finished(app: &AppHandle, item: &Item) {
    let enabled = app
        .try_state::<Manager>()
        .is_some_and(|m| m.settings().notify_on_complete);
    if enabled {
        let _ = app
            .notification()
            .builder()
            .title("Download complete")
            .body(&item.file_name)
            .show();
    }
}

fn update_tray(app: &AppHandle, active: usize, speed: f64) {
    let Some(tray) = app.tray_by_id(TRAY_ID) else {
        return;
    };
    let tip = match active {
        0 => "Slipstream".to_string(),
        n => format!("Slipstream: {n} downloading at {}/s", human_bytes(speed)),
    };
    let _ = tray.set_tooltip(Some(tip));
}

fn human_bytes(bytes: f64) -> String {
    const UNITS: [&str; 5] = ["B", "KB", "MB", "GB", "TB"];
    let mut value = bytes;
    let mut unit = 0;
    while value >= 1024.0 && unit < UNITS.len() - 1 {
        value /= 1024.0;
        unit += 1;
    }
    if unit == 0 {
        format!("{value:.0} {}", UNITS[unit])
    } else {
        format!("{value:.1} {}", UNITS[unit])
    }
}

fn show_main(app: &AppHandle) {
    if let Some(window) = app.get_webview_window("main") {
        let _ = window.show();
        let _ = window.unminimize();
        let _ = window.set_focus();
    }
}

/// Pauses and saves every download, then exits.
fn quit(app: &AppHandle) {
    let app = app.clone();
    tauri::async_runtime::spawn(async move {
        if let Some(manager) = app.try_state::<Manager>() {
            let manager = manager.inner().clone();
            manager.shutdown().await;
        }
        READY_TO_EXIT.store(true, Ordering::SeqCst);
        app.exit(0);
    });
}

fn build_tray(app: &AppHandle) -> tauri::Result<()> {
    let show = MenuItem::with_id(app, "show", "Show Slipstream", true, None::<&str>)?;
    let pause = MenuItem::with_id(app, "pause_all", "Pause all", true, None::<&str>)?;
    let resume = MenuItem::with_id(app, "resume_all", "Resume all", true, None::<&str>)?;
    let quit_item = MenuItem::with_id(app, "quit", "Quit Slipstream", true, None::<&str>)?;
    let menu = Menu::with_items(
        app,
        &[
            &show,
            &PredefinedMenuItem::separator(app)?,
            &pause,
            &resume,
            &PredefinedMenuItem::separator(app)?,
            &quit_item,
        ],
    )?;
    let mut tray = TrayIconBuilder::with_id(TRAY_ID)
        .tooltip("Slipstream")
        .menu(&menu)
        .show_menu_on_left_click(false)
        .on_menu_event(|app, event| match event.id().as_ref() {
            "show" => show_main(app),
            "pause_all" => app.state::<Manager>().pause_all(),
            "resume_all" => app.state::<Manager>().resume_all(),
            "quit" => quit(app),
            _ => {}
        })
        .on_tray_icon_event(|tray, event| {
            if let TrayIconEvent::Click {
                button: MouseButton::Left,
                button_state: MouseButtonState::Up,
                ..
            } = event
            {
                show_main(tray.app_handle());
            }
        });
    if let Some(icon) = app.default_window_icon() {
        tray = tray.icon(icon.clone());
    }
    tray.build(app)?;
    Ok(())
}

fn main() {
    tauri::Builder::default()
        .plugin(tauri_plugin_opener::init())
        .plugin(tauri_plugin_dialog::init())
        .plugin(tauri_plugin_notification::init())
        .setup(|app| {
            let paths = app.path();
            let data_dir = paths.app_data_dir()?;
            let downloads: PathBuf = paths.download_dir().or_else(|_| paths.home_dir())?;
            let handle = app.handle().clone();
            let manager = Manager::open(data_dir, downloads, move |event| forward(&handle, event))?;
            app.manage(manager.clone());
            tauri::async_runtime::spawn(async move { manager.start() });
            build_tray(app.handle())?;
            Ok(())
        })
        .on_window_event(|window, event| {
            if let WindowEvent::CloseRequested { api, .. } = event {
                let to_tray = window
                    .app_handle()
                    .try_state::<Manager>()
                    .is_some_and(|m| m.settings().close_to_tray);
                if to_tray {
                    api.prevent_close();
                    let _ = window.hide();
                }
            }
        })
        .invoke_handler(tauri::generate_handler![
            list,
            get_settings,
            set_settings,
            add,
            pause,
            resume,
            pause_all,
            resume_all,
            remove,
            open_file,
            show_in_folder,
            open_download_folder,
            pick_folder,
        ])
        .build(tauri::generate_context!())
        .expect("failed to start Slipstream")
        .run(|app, event| match event {
            RunEvent::ExitRequested { api, .. } if !READY_TO_EXIT.load(Ordering::SeqCst) => {
                api.prevent_exit();
                quit(app);
            }
            #[cfg(target_os = "macos")]
            RunEvent::Reopen { .. } => show_main(app),
            _ => {}
        });
}
