//! The app's windows besides the main one, as in IDM: a Download File Info
//! window for each new link, a progress window per download (which turns
//! into the "Download complete" window), and a list for downloading many
//! links at once.

use std::collections::HashMap;
use std::sync::Mutex;
use std::sync::atomic::{AtomicU64, Ordering};

use serde::Serialize;
use slipstream_manager::Id;
use tauri::{AppHandle, Manager as _, WebviewUrl, WebviewWindowBuilder};

/// A link waiting for the user's choices in a Download File Info window.
#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct PendingLink {
    pub url: String,
    /// Cookies, referer and user agent from the browser.
    #[serde(skip)]
    pub headers: Vec<(String, String)>,
    pub referrer: Option<String>,
    /// What the browser said about the file, if it came from one.
    pub file_name: Option<String>,
    pub file_size: Option<u64>,
    pub from_browser: bool,
}

/// One link in a "download all links" list.
#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct BatchLink {
    pub url: String,
    /// The link's text on the page.
    pub text: Option<String>,
    #[serde(skip)]
    pub headers: Vec<(String, String)>,
}

/// Links handed to windows that are still open, by token.
#[derive(Default)]
pub struct Pending {
    links: Mutex<HashMap<String, PendingLink>>,
    batches: Mutex<HashMap<String, Vec<BatchLink>>>,
    next: AtomicU64,
}

impl Pending {
    fn token(&self) -> String {
        (self.next.fetch_add(1, Ordering::Relaxed) + 1).to_string()
    }

    pub fn link(&self, token: &str) -> Option<PendingLink> {
        self.links.lock().unwrap().get(token).cloned()
    }

    pub fn take_link(&self, token: &str) -> Option<PendingLink> {
        self.links.lock().unwrap().remove(token)
    }

    pub fn batch(&self, token: &str) -> Option<Vec<BatchLink>> {
        self.batches.lock().unwrap().get(token).cloned()
    }

    pub fn take_batch(&self, token: &str) -> Option<Vec<BatchLink>> {
        self.batches.lock().unwrap().remove(token)
    }

    /// Forgets whatever belonged to a window that closed.
    pub fn forget(&self, label: &str) {
        if let Some(token) = label.strip_prefix("info-") {
            self.take_link(token);
        } else if let Some(token) = label.strip_prefix("batch-") {
            self.take_batch(token);
        }
    }
}

pub fn show_main(app: &AppHandle) {
    if let Some(window) = app.get_webview_window("main") {
        let _ = window.show();
        let _ = window.unminimize();
        let _ = window.set_focus();
    }
}

pub fn open_file_info(app: &AppHandle, link: PendingLink) -> tauri::Result<()> {
    let pending = app.state::<Pending>();
    let token = pending.token();
    pending.links.lock().unwrap().insert(token.clone(), link);
    open(
        app,
        &format!("info-{token}"),
        &format!("info.html?token={token}"),
        "Download File Info",
        (620.0, 262.0),
        false,
    )
}

pub fn open_batch(app: &AppHandle, links: Vec<BatchLink>) -> tauri::Result<()> {
    let pending = app.state::<Pending>();
    let token = pending.token();
    pending.batches.lock().unwrap().insert(token.clone(), links);
    open(
        app,
        &format!("batch-{token}"),
        &format!("batch.html?token={token}"),
        "Download links",
        (780.0, 520.0),
        true,
    )
}

/// Shows the download's own window, opening it if needed.
pub fn open_progress(app: &AppHandle, id: Id) -> tauri::Result<()> {
    open(
        app,
        &format!("progress-{id}"),
        &format!("progress.html?id={id}"),
        "Slipstream",
        (540.0, 300.0),
        false,
    )
}

fn open(
    app: &AppHandle,
    label: &str,
    url: &str,
    title: &str,
    (width, height): (f64, f64),
    resizable: bool,
) -> tauri::Result<()> {
    if let Some(window) = app.get_webview_window(label) {
        window.show()?;
        window.unminimize()?;
        return window.set_focus();
    }
    WebviewWindowBuilder::new(app, label, WebviewUrl::App(url.into()))
        .title(title)
        .inner_size(width, height)
        .min_inner_size(width.min(480.0), height.min(260.0))
        .resizable(resizable)
        .maximizable(resizable)
        .center()
        .focused(true)
        .build()?;
    Ok(())
}
