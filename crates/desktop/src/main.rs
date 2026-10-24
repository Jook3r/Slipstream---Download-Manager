//! The Slipstream desktop app: the main download list, IDM-style windows for
//! each download, a tray icon, and the local connection the browser
//! extension uses. Pages in `ui/` call the commands in `commands.rs` and
//! listen for `slipstream` events.

#![cfg_attr(not(debug_assertions), windows_subsystem = "windows")]

mod commands;
mod ipc;
mod power;
mod tray;
mod windows;

use std::path::PathBuf;
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::Duration;

use serde::Serialize;
use serde_json::json;
use slipstream_bridge::{CHROME_EXTENSION_ID, Endpoint, FIREFOX_EXTENSION_ID};
use slipstream_manager::{Event, Item, Manager, WhenDone};
use tauri::{AppHandle, Emitter, Manager as _, RunEvent, WindowEvent};
use tauri_plugin_notification::NotificationExt;
use tauri_plugin_opener::OpenerExt;

use crate::windows::Pending;

/// Set once downloads are paused and saved, so the app may really exit.
static READY_TO_EXIT: AtomicBool = AtomicBool::new(false);

/// Where the app keeps its list, settings and `ipc.json`.
struct DataDir(PathBuf);

/// Which browsers the bridge was registered with, for Help → Browser
/// integration.
#[derive(Debug, Clone, Default, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct Integration {
    browsers: Vec<String>,
    chrome_extension_id: &'static str,
    firefox_extension_id: &'static str,
    problem: Option<String>,
}

/// Passes manager events to the pages, and acts on the ones the app cares
/// about itself.
fn forward(app: &AppHandle, event: Event) {
    match &event {
        Event::Finished { item } => finished(app, item),
        Event::QueueDone { when_done } => queue_done(app, *when_done),
        Event::Progress { items, total_speed } => tray::update(app, items.len(), *total_speed),
        _ => {}
    }
    let _ = app.emit("slipstream", event);
}

fn finished(app: &AppHandle, item: &Item) {
    let Some(manager) = app.try_state::<Manager>() else {
        return;
    };
    if item.open_when_done
        && let Some(path) = &item.path
    {
        let _ = app.opener().open_path(path.to_string_lossy(), None::<&str>);
    }
    let settings = manager.settings();
    if settings.show_complete {
        let _ = windows::open_progress(app, item.id);
    } else if settings.notify_on_complete {
        let _ = app
            .notification()
            .builder()
            .title("Download complete")
            .body(&item.file_name)
            .show();
    }
}

/// The main download queue finished: do what the scheduler was told to.
fn queue_done(app: &AppHandle, when_done: WhenDone) {
    match when_done {
        WhenDone::Nothing => {
            let notify = app
                .try_state::<Manager>()
                .is_some_and(|m| m.settings().notify_on_complete);
            if notify {
                let _ = app
                    .notification()
                    .builder()
                    .title("Download queue finished")
                    .body("Every download in the queue is done.")
                    .show();
            }
        }
        WhenDone::Exit => quit(app),
        WhenDone::Sleep | WhenDone::Shutdown => {
            let _ = windows::open_countdown(app, when_done);
        }
    }
}

/// Pauses and saves every download, then exits.
pub fn quit(app: &AppHandle) {
    let app = app.clone();
    tauri::async_runtime::spawn(async move {
        if let Some(manager) = app.try_state::<Manager>() {
            let manager = manager.inner().clone();
            manager.shutdown().await;
        }
        if let Some(dir) = app.try_state::<DataDir>() {
            Endpoint::remove(&dir.0);
        }
        READY_TO_EXIT.store(true, Ordering::SeqCst);
        app.exit(0);
    });
}

/// Registers `slipstream-bridge` (next to this program) with the browsers.
fn register_bridge() -> Integration {
    let mut status = Integration {
        chrome_extension_id: CHROME_EXTENSION_ID,
        firefox_extension_id: FIREFOX_EXTENSION_ID,
        ..Integration::default()
    };
    let name = if cfg!(windows) {
        "slipstream-bridge.exe"
    } else {
        "slipstream-bridge"
    };
    let bridge = match std::env::current_exe() {
        Ok(exe) => exe.with_file_name(name),
        Err(e) => {
            status.problem = Some(e.to_string());
            return status;
        }
    };
    if !bridge.exists() {
        status.problem = Some(format!("{name} was not found next to Slipstream"));
        return status;
    }
    match slipstream_bridge::install::install(&bridge) {
        Ok(done) => status.browsers = done.into_iter().map(|r| r.browser.to_string()).collect(),
        Err(e) => status.problem = Some(e.to_string()),
    }
    status
}

fn main() {
    // `--background`: started by the browser bridge; show only the windows
    // for the link, not the main window.
    let background = std::env::args().any(|a| a == "--background");

    // One Slipstream at a time: starting it again brings the first forward.
    if let Some(dir) = slipstream_bridge::data_dir() {
        let request = if background {
            json!({ "type": "ping" })
        } else {
            json!({ "type": "activate" })
        };
        if slipstream_bridge::send(&dir, &request, Duration::from_secs(2))
            .is_ok_and(|reply| reply["ok"] == true)
        {
            return;
        }
    }

    tauri::Builder::default()
        .plugin(tauri_plugin_opener::init())
        .plugin(tauri_plugin_dialog::init())
        .plugin(tauri_plugin_notification::init())
        .manage(Pending::default())
        .setup(move |app| {
            let paths = app.path();
            let data_dir = paths.app_data_dir()?;
            let downloads: PathBuf = paths.download_dir().or_else(|_| paths.home_dir())?;
            let handle = app.handle().clone();
            let manager =
                Manager::open(&data_dir, downloads, move |event| forward(&handle, event))?;
            app.manage(manager.clone());
            tauri::async_runtime::spawn(async move { manager.start() });

            app.manage(register_bridge());
            if let Err(e) = ipc::start(app.handle().clone(), &data_dir) {
                eprintln!("browser integration is off: {e}");
            }
            app.manage(DataDir(data_dir));
            tray::build(app.handle())?;
            if !background {
                windows::show_main(app.handle());
            }
            Ok(())
        })
        .on_window_event(|window, event| match event {
            WindowEvent::CloseRequested { api, .. } if window.label() == "main" => {
                let to_tray = window
                    .app_handle()
                    .try_state::<Manager>()
                    .is_some_and(|m| m.settings().close_to_tray);
                if to_tray {
                    api.prevent_close();
                    let _ = window.hide();
                }
            }
            WindowEvent::Destroyed => {
                window
                    .app_handle()
                    .state::<Pending>()
                    .forget(window.label());
            }
            _ => {}
        })
        .invoke_handler(tauri::generate_handler![
            commands::list,
            commands::get_item,
            commands::get_settings,
            commands::set_settings,
            commands::pause,
            commands::resume,
            commands::pause_all,
            commands::resume_all,
            commands::remove,
            commands::delete_completed,
            commands::set_speed_limit,
            commands::set_open_when_done,
            commands::open_progress,
            commands::add_links,
            commands::pending_link,
            commands::guess_link,
            commands::inspect_link,
            commands::inspect_stream,
            commands::suggest_path,
            commands::get_queue,
            commands::add_to_queue,
            commands::remove_from_queue,
            commands::reorder_queue,
            commands::start_queue,
            commands::stop_queue,
            commands::open_scheduler,
            commands::power_off,
            commands::ffmpeg_status,
            commands::install_ffmpeg,
            commands::choose_ffmpeg,
            commands::confirm_link,
            commands::cancel_link,
            commands::pending_batch,
            commands::confirm_batch,
            commands::cancel_batch,
            commands::open_file,
            commands::show_in_folder,
            commands::open_download_folder,
            commands::pick_folder,
            commands::choose_save_path,
            commands::path_exists,
            commands::integration_status,
            commands::quit_app,
        ])
        .build(tauri::generate_context!())
        .expect("failed to start Slipstream")
        .run(|app, event| match event {
            RunEvent::ExitRequested { api, .. } if !READY_TO_EXIT.load(Ordering::SeqCst) => {
                api.prevent_exit();
                quit(app);
            }
            #[cfg(target_os = "macos")]
            RunEvent::Reopen { .. } => windows::show_main(app),
            _ => {}
        });
}
