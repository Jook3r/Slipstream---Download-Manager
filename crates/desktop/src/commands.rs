//! What the app's pages can ask for. Commands that open windows are async:
//! creating a window from a synchronous command can deadlock on Windows.

use std::path::{Path, PathBuf};

use serde::{Deserialize, Serialize};
use slipstream_manager::{
    AddReport, Category, FfmpegStatus, Id, Item, LinkInfo, Manager, NewDownload, Settings,
    StreamChoice, StreamLinkInfo,
};
use tauri::{AppHandle, State};
use tauri_plugin_dialog::DialogExt;
use tauri_plugin_opener::OpenerExt;

use crate::Integration;
use crate::windows::{self, BatchLink, Pending, PendingLink};

pub type CmdResult<T = ()> = Result<T, String>;

fn text(e: impl ToString) -> String {
    e.to_string()
}

// ---------- the list ----------

#[tauri::command]
pub fn list(manager: State<Manager>) -> Vec<Item> {
    manager.items()
}

#[tauri::command]
pub fn get_item(manager: State<Manager>, id: Id) -> Option<Item> {
    manager.item(id)
}

#[tauri::command]
pub fn get_settings(manager: State<Manager>) -> Settings {
    manager.settings()
}

#[tauri::command]
pub fn set_settings(manager: State<Manager>, settings: Settings) -> Settings {
    manager.set_settings(settings)
}

#[tauri::command]
pub fn pause(manager: State<Manager>, ids: Vec<Id>) {
    manager.pause(&ids);
}

/// Resumes downloads; like IDM, a download resumed by hand gets its
/// progress window (but not when resuming many at once).
#[tauri::command]
pub async fn resume(app: AppHandle, manager: State<'_, Manager>, ids: Vec<Id>) -> CmdResult {
    manager.resume(&ids);
    if manager.settings().show_progress && ids.len() <= 3 {
        for id in ids {
            windows::open_progress(&app, id).map_err(text)?;
        }
    }
    Ok(())
}

#[tauri::command]
pub fn pause_all(manager: State<Manager>) {
    manager.pause_all();
}

#[tauri::command]
pub fn resume_all(manager: State<Manager>) {
    manager.resume_all();
}

#[tauri::command]
pub async fn remove(manager: State<'_, Manager>, ids: Vec<Id>, delete_files: bool) -> CmdResult {
    manager.remove(&ids, delete_files).await;
    Ok(())
}

#[tauri::command]
pub fn delete_completed(manager: State<Manager>) -> usize {
    manager.delete_completed()
}

#[tauri::command]
pub fn set_speed_limit(manager: State<Manager>, id: Id, bytes_per_sec: u64) {
    manager.set_speed_limit(id, bytes_per_sec);
}

#[tauri::command]
pub fn set_open_when_done(manager: State<Manager>, id: Id, open: bool) {
    manager.set_open_when_done(id, open);
}

#[tauri::command]
pub async fn open_progress(app: AppHandle, id: Id) -> CmdResult {
    windows::open_progress(&app, id).map_err(text)
}

// ---------- adding links ----------

/// From the main window's Add URL dialog: one link opens a Download File
/// Info window, several open the list of links.
#[tauri::command]
pub async fn add_links(app: AppHandle, urls: Vec<String>) -> CmdResult {
    let urls: Vec<String> = urls
        .into_iter()
        .map(|u| u.trim().to_string())
        .filter(|u| !u.is_empty())
        .collect();
    match urls.as_slice() {
        [] => Err("paste at least one link".into()),
        [url] => windows::open_file_info(
            &app,
            PendingLink {
                url: url.clone(),
                headers: Vec::new(),
                referrer: None,
                file_name: None,
                file_size: None,
                from_browser: false,
                media: None,
            },
        )
        .map_err(text),
        _ => windows::open_batch(
            &app,
            urls.into_iter()
                .map(|url| BatchLink {
                    url,
                    text: None,
                    headers: Vec::new(),
                })
                .collect(),
        )
        .map_err(text),
    }
}

#[tauri::command]
pub fn pending_link(pending: State<Pending>, token: String) -> Option<PendingLink> {
    pending.link(&token)
}

/// A quick guess from the link alone, shown before the server answers.
#[tauri::command]
pub fn guess_link(manager: State<Manager>, url: String) -> LinkInfo {
    manager.guess(&url)
}

/// Asks the server about a pending link, with the browser's cookies.
#[tauri::command]
pub async fn inspect_link(
    manager: State<'_, Manager>,
    pending: State<'_, Pending>,
    token: String,
    url: String,
) -> CmdResult<LinkInfo> {
    let headers = pending.link(&token).map(|l| l.headers).unwrap_or_default();
    manager.inspect(&url, &headers).await
}

/// Lists a video stream's qualities, with the browser's cookies.
#[tauri::command]
pub async fn inspect_stream(
    manager: State<'_, Manager>,
    pending: State<'_, Pending>,
    token: String,
    url: String,
) -> CmdResult<StreamLinkInfo> {
    let headers = pending.link(&token).map(|l| l.headers).unwrap_or_default();
    manager.inspect_stream(&url, &headers).await
}

/// A free place to save `file_name`, in its category's folder.
#[tauri::command]
pub fn suggest_path(manager: State<Manager>, file_name: String) -> PathBuf {
    manager.suggest_path(&file_name)
}

#[derive(Debug, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct LinkChoice {
    url: String,
    path: String,
    category: Category,
    description: String,
    start: bool,
    remember_folder: bool,
    /// For a video stream, the quality chosen.
    stream: Option<StreamChoice>,
}

/// "Start Download" or "Download Later" in a Download File Info window.
#[tauri::command]
pub async fn confirm_link(
    app: AppHandle,
    manager: State<'_, Manager>,
    pending: State<'_, Pending>,
    token: String,
    choice: LinkChoice,
) -> CmdResult<Id> {
    let link = pending
        .link(&token)
        .ok_or("this link is no longer waiting")?;
    let path = PathBuf::from(choice.path.trim());
    if !path.is_absolute() || path.file_name().is_none() {
        return Err("choose where to save the file".into());
    }
    if choice.remember_folder
        && let Some(dir) = path.parent()
    {
        let mut settings = manager.settings();
        settings
            .category_dirs
            .insert(choice.category, dir.to_path_buf());
        manager.set_settings(settings);
    }
    let item = manager.add_download(NewDownload {
        url: choice.url,
        headers: link.headers,
        path: Some(path),
        category: Some(choice.category),
        description: choice.description,
        start: choice.start,
        stream: choice.stream,
    })?;
    pending.take_link(&token);
    if choice.start && manager.settings().show_progress {
        windows::open_progress(&app, item.id).map_err(text)?;
    }
    Ok(item.id)
}

#[tauri::command]
pub fn cancel_link(pending: State<Pending>, token: String) {
    pending.take_link(&token);
}

#[derive(Debug, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct BatchEntry {
    url: String,
    text: Option<String>,
    file_name: String,
    category: Category,
}

#[tauri::command]
pub fn pending_batch(
    manager: State<Manager>,
    pending: State<Pending>,
    token: String,
) -> Option<Vec<BatchEntry>> {
    let links = pending.batch(&token)?;
    Some(
        links
            .into_iter()
            .map(|link| {
                let guess = manager.guess(&link.url);
                BatchEntry {
                    url: link.url,
                    text: link.text,
                    file_name: guess.file_name,
                    category: guess.category,
                }
            })
            .collect(),
    )
}

#[tauri::command]
pub fn confirm_batch(
    manager: State<Manager>,
    pending: State<Pending>,
    token: String,
    urls: Vec<String>,
    start: bool,
) -> CmdResult<AddReport> {
    let links = pending
        .take_batch(&token)
        .ok_or("these links are no longer waiting")?;
    let mut report = AddReport::default();
    for url in urls {
        let headers = links
            .iter()
            .find(|l| l.url == url)
            .map(|l| l.headers.clone())
            .unwrap_or_default();
        let one = manager.add(&[url], &headers, start);
        report.added.extend(one.added);
        report.skipped.extend(one.skipped);
    }
    Ok(report)
}

#[tauri::command]
pub fn cancel_batch(pending: State<Pending>, token: String) {
    pending.take_batch(&token);
}

// ---------- files and folders ----------

/// Opens a finished download with its default app.
#[tauri::command]
pub fn open_file(app: AppHandle, manager: State<Manager>, id: Id) -> CmdResult {
    let path = find(&manager, id)?
        .path
        .ok_or("this download has not started yet")?;
    open_path(&app, &path)
}

/// Shows the download in Explorer, Finder or the file manager.
#[tauri::command]
pub fn show_in_folder(app: AppHandle, manager: State<Manager>, id: Id) -> CmdResult {
    let item = find(&manager, id)?;
    match item.path {
        Some(path) if path.exists() => app.opener().reveal_item_in_dir(&path).map_err(text),
        Some(path) => open_path(&app, path.parent().unwrap_or(&path)),
        None => open_path(&app, &manager.settings().download_dir),
    }
}

#[tauri::command]
pub fn open_download_folder(app: AppHandle, manager: State<Manager>) -> CmdResult {
    let dir = manager.settings().download_dir;
    std::fs::create_dir_all(&dir).map_err(text)?;
    open_path(&app, &dir)
}

/// Asks the user for a folder; `None` if they cancel.
#[tauri::command]
pub async fn pick_folder(app: AppHandle) -> Option<String> {
    app.dialog()
        .file()
        .blocking_pick_folder()
        .map(|p| p.to_string())
}

/// The "…" button next to Save As.
#[tauri::command]
pub async fn choose_save_path(app: AppHandle, path: String) -> Option<String> {
    let path = PathBuf::from(path);
    let mut dialog = app.dialog().file().set_title("Save As");
    if let Some(dir) = path.parent().filter(|d| d.is_dir()) {
        dialog = dialog.set_directory(dir);
    }
    if let Some(name) = path.file_name() {
        dialog = dialog.set_file_name(name.to_string_lossy());
    }
    dialog.blocking_save_file().map(|p| p.to_string())
}

#[tauri::command]
pub fn path_exists(path: String) -> bool {
    Path::new(&path).exists()
}

fn find(manager: &Manager, id: Id) -> CmdResult<Item> {
    manager
        .item(id)
        .ok_or_else(|| "that download is no longer in the list".to_string())
}

fn open_path(app: &AppHandle, path: &Path) -> CmdResult {
    app.opener()
        .open_path(path.to_string_lossy(), None::<&str>)
        .map_err(text)
}

// ---------- ffmpeg ----------

#[tauri::command]
pub fn ffmpeg_status(manager: State<Manager>) -> FfmpegStatus {
    manager.ffmpeg_status()
}

/// Fetches ffmpeg in the background; progress arrives as `ffmpeg` events.
#[tauri::command]
pub fn install_ffmpeg(manager: State<Manager>) -> CmdResult {
    manager.install_ffmpeg()
}

/// Lets the user point at an ffmpeg they already have.
#[tauri::command]
pub async fn choose_ffmpeg(app: AppHandle, manager: State<'_, Manager>) -> CmdResult<FfmpegStatus> {
    let Some(picked) = app
        .dialog()
        .file()
        .set_title("Choose ffmpeg")
        .blocking_pick_file()
    else {
        return Ok(manager.ffmpeg_status());
    };
    let path = picked.into_path().map_err(text)?;
    if !slipstream_manager::ffmpeg::runs(&path).await {
        return Err(format!("{} is not a working ffmpeg", path.display()));
    }
    let mut settings = manager.settings();
    settings.ffmpeg_path = Some(path);
    manager.set_settings(settings);
    Ok(manager.ffmpeg_status())
}

// ---------- the app ----------

#[tauri::command]
pub fn integration_status(integration: State<Integration>) -> Integration {
    integration.inner().clone()
}

#[tauri::command]
pub fn quit_app(app: AppHandle) {
    crate::quit(&app);
}
