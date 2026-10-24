//! Watching the clipboard, as IDM does: copying a link to a file opens the
//! Download File Info window for it.

use std::time::Duration;

use slipstream_manager::Manager;
use tauri::{AppHandle, Manager as _};

use crate::windows::{self, PendingLink};

const EVERY: Duration = Duration::from_millis(700);

pub fn start(app: AppHandle) {
    std::thread::Builder::new()
        .name("clipboard".into())
        .spawn(move || watch(app))
        .expect("could not start the clipboard watcher");
}

fn watch(app: AppHandle) {
    let mut clipboard = match arboard::Clipboard::new() {
        Ok(clipboard) => clipboard,
        Err(e) => return eprintln!("not watching the clipboard: {e}"),
    };
    // What is on the clipboard when Slipstream starts is not news.
    let mut last = clipboard.get_text().ok();
    let mut seen = changes();
    loop {
        std::thread::sleep(EVERY);
        let Some(manager) = app.try_state::<Manager>() else {
            continue;
        };
        // Cheap to ask on Windows; elsewhere the text itself is compared.
        let now = changes();
        if now.is_some() && now == seen {
            continue;
        }
        seen = now;
        let text = clipboard.get_text().ok();
        if text == last {
            continue;
        }
        last = text.clone();
        if !manager.settings().watch_clipboard {
            continue;
        }
        let Some(url) = text.as_deref().and_then(|t| manager.clipboard_link(t)) else {
            continue;
        };
        let link = PendingLink {
            url,
            headers: Vec::new(),
            referrer: None,
            file_name: None,
            file_size: None,
            from_browser: false,
            media: None,
        };
        let _ = windows::open_file_info(&app, link);
    }
}

/// A number that changes whenever the clipboard does, where the system has
/// one.
#[cfg(windows)]
fn changes() -> Option<u32> {
    // SAFETY: takes no arguments and only reads a counter.
    Some(unsafe { windows_sys::Win32::System::DataExchange::GetClipboardSequenceNumber() })
}

#[cfg(not(windows))]
fn changes() -> Option<u32> {
    None
}
