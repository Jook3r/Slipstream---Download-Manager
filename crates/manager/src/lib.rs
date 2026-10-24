//! The download manager behind the desktop app: a list of downloads, a queue
//! that runs a few at a time, IDM-style categories, settings and
//! persistence. It knows nothing about windows; the app subscribes to
//! [`Event`]s through an [`EventSink`].

mod category;
pub mod ffmpeg;
mod store;

use std::collections::{BTreeMap, HashMap, VecDeque};
use std::fs;
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex, MutexGuard};
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

use serde::{Deserialize, Serialize};
use slipstream_engine::probe::file_name_from_url;
use slipstream_engine::segments::SegmentView;
use slipstream_engine::{
    Client, ClientOptions, Download, Options, Outcome, Output, Progress, RateLimiter, Request,
    StreamDownload, StreamKind, StreamProgress, StreamRequest, Url, Variant, VariantKey,
    build_client, find_ffmpeg, inspect_stream, partial_files, probe, stream_kind, stream_work_dir,
};
use tokio::task::JoinHandle;
use tokio_util::sync::CancellationToken;

pub use category::Category;
use store::Store;

pub type Id = u64;

const LIST_FILE: &str = "downloads.json";
const SETTINGS_FILE: &str = "settings.json";
/// How often live progress is sent to the UI.
const TICK: Duration = Duration::from_millis(200);
/// Transfer rates are measured over this much recent time.
const SPEED_WINDOW: Duration = Duration::from_secs(1);
/// An HTML answer smaller than this, for a link to some other kind of file,
/// is a web page served instead of the file.
const MAX_STRAY_PAGE: u64 = 4 << 20;

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum Status {
    Queued,
    Downloading,
    Paused,
    Completed,
    Failed,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct Item {
    pub id: Id,
    pub url: String,
    /// Sent with every request (cookies, referer from the browser).
    #[serde(default)]
    pub headers: Vec<(String, String)>,
    pub file_name: String,
    /// Chosen when the download first starts; reused to resume.
    pub path: Option<PathBuf>,
    pub category: Category,
    pub total: Option<u64>,
    pub downloaded: u64,
    pub resumable: Option<bool>,
    pub status: Status,
    pub error: Option<String>,
    /// Milliseconds since the Unix epoch.
    pub added_at: u64,
    pub completed_at: Option<u64>,
    /// When the latest attempt started.
    #[serde(default)]
    pub last_try_at: Option<u64>,
    #[serde(default)]
    pub description: String,
    /// This download's own speed limit in bytes per second; 0 means none.
    #[serde(default)]
    pub speed_limit: u64,
    /// Open the file as soon as it finishes.
    #[serde(default)]
    pub open_when_done: bool,
    /// Set for a video stream (HLS or DASH): the quality to download.
    #[serde(default)]
    pub stream: Option<StreamChoice>,
}

/// The quality chosen for a video stream.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", default)]
pub struct StreamChoice {
    /// Empty for "the best on offer".
    pub variant: VariantKey,
    /// How the quality was described when chosen, e.g. "1280×720".
    pub quality: String,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", default)]
pub struct Settings {
    pub download_dir: PathBuf,
    /// Sort downloads into Video, Music, Documents, ... sub-folders.
    pub use_categories: bool,
    /// Connections per download.
    pub connections: usize,
    /// Downloads running at the same time; the rest wait in the queue.
    pub max_active: usize,
    /// Combined speed limit in bytes per second; 0 means none.
    pub speed_limit: u64,
    pub resume_on_startup: bool,
    pub close_to_tray: bool,
    pub notify_on_complete: bool,
    /// Folders picked with "Remember this path for this category".
    pub category_dirs: BTreeMap<Category, PathBuf>,
    /// Show the Download File Info window for links caught from a browser.
    pub show_file_info: bool,
    /// Show a progress window for downloads started by hand.
    pub show_progress: bool,
    /// Show the "Download complete" window when a download finishes.
    pub show_complete: bool,
    /// ffmpeg chosen by the user; otherwise one is looked for.
    pub ffmpeg_path: Option<PathBuf>,
}

impl Default for Settings {
    fn default() -> Self {
        Self {
            download_dir: PathBuf::new(),
            use_categories: true,
            connections: 8,
            max_active: 3,
            speed_limit: 0,
            resume_on_startup: true,
            close_to_tray: true,
            notify_on_complete: true,
            category_dirs: BTreeMap::new(),
            show_file_info: true,
            show_progress: true,
            show_complete: true,
            ffmpeg_path: None,
        }
    }
}

impl Settings {
    pub fn folder_for(&self, category: Category) -> PathBuf {
        if let Some(dir) = self.category_dirs.get(&category) {
            return dir.clone();
        }
        match category.folder() {
            Some(sub) if self.use_categories => self.download_dir.join(sub),
            _ => self.download_dir.clone(),
        }
    }

    fn sanitized(mut self, default_dir: &Path) -> Self {
        self.connections = self.connections.clamp(1, 32);
        self.max_active = self.max_active.clamp(1, 10);
        if self.download_dir.as_os_str().is_empty() {
            self.download_dir = default_dir.to_path_buf();
        }
        self
    }
}

/// Live numbers for a running download.
#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct Live {
    pub id: Id,
    pub downloaded: u64,
    pub total: Option<u64>,
    /// Bytes per second, smoothed.
    pub speed: f64,
    pub connections: usize,
    /// Each piece of the file and whether a connection is on it, for the
    /// segment map and the connections table.
    pub segments: Vec<SegmentView>,
    /// For a video stream: its pieces, and when they are being joined.
    pub stream: Option<StreamProgress>,
}

/// What a link points to, learned by asking the server before adding it.
#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct LinkInfo {
    pub url: String,
    pub file_name: String,
    pub total: Option<u64>,
    pub resumable: bool,
    pub content_type: Option<String>,
    /// The server sent a web page instead of the file (an expired link).
    pub web_page: bool,
    /// The link is a video stream's manifest, to be opened as a stream.
    pub stream: Option<StreamKind>,
    pub category: Category,
    /// A free file name in the category's folder.
    pub suggested_path: PathBuf,
}

/// One download with explicit choices, as made in the File Info window.
#[derive(Debug, Clone, Default, Deserialize)]
#[serde(rename_all = "camelCase", default)]
pub struct NewDownload {
    pub url: String,
    pub headers: Vec<(String, String)>,
    /// Where to save it; `None` decides when the download starts.
    pub path: Option<PathBuf>,
    pub category: Option<Category>,
    pub description: String,
    pub start: bool,
    /// For a video stream, the quality chosen.
    pub stream: Option<StreamChoice>,
}

/// What a video stream offers, for choosing a quality.
#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct StreamLinkInfo {
    pub kind: StreamKind,
    /// Best first.
    pub variants: Vec<Variant>,
    /// Seconds.
    pub duration: Option<f64>,
    /// Where videos are saved.
    pub folder: PathBuf,
    /// Whether ffmpeg is at hand to join picture and sound.
    pub ffmpeg: bool,
}

/// Where ffmpeg stands, for the windows that need it.
#[derive(Debug, Clone, Default, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct FfmpegStatus {
    pub path: Option<PathBuf>,
    /// Whether Slipstream can fetch a copy on this computer.
    pub can_install: bool,
    pub installing: bool,
    pub downloaded: u64,
    pub total: Option<u64>,
    /// Why the last attempt to fetch it failed.
    pub error: Option<String>,
}

#[derive(Debug, Clone, Serialize)]
#[serde(
    tag = "type",
    rename_all = "camelCase",
    rename_all_fields = "camelCase"
)]
pub enum Event {
    /// An item was added or changed status.
    Updated {
        item: Item,
    },
    Removed {
        id: Id,
    },
    /// A download completed (sent after its `Updated`), for notifications.
    Finished {
        item: Item,
    },
    /// Sent five times a second while anything is downloading.
    Progress {
        items: Vec<Live>,
        total_speed: f64,
    },
    /// ffmpeg was found, or is being fetched.
    Ffmpeg {
        status: FfmpegStatus,
    },
}

pub trait EventSink: Send + Sync + 'static {
    fn emit(&self, event: Event);
}

impl<F: Fn(Event) + Send + Sync + 'static> EventSink for F {
    fn emit(&self, event: Event) {
        self(event)
    }
}

#[derive(Debug, Default, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct AddReport {
    pub added: Vec<Item>,
    pub skipped: Vec<Skipped>,
}

#[derive(Debug, Serialize)]
pub struct Skipped {
    pub url: String,
    pub reason: String,
}

#[derive(Default, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", default)]
struct SavedList {
    next_id: Id,
    items: Vec<Item>,
}

/// Cheap to clone; all clones share the same list.
#[derive(Clone)]
pub struct Manager(Arc<Inner>);

struct Inner {
    client: Client,
    limiter: Arc<RateLimiter>,
    store: Store,
    sink: Box<dyn EventSink>,
    state: Mutex<State>,
}

struct State {
    settings: Settings,
    default_dir: PathBuf,
    items: Vec<Item>,
    next_id: Id,
    running: HashMap<Id, Running>,
    shutting_down: bool,
    /// Fetching ffmpeg: bytes so far and size.
    ffmpeg_install: Option<(u64, Option<u64>)>,
    ffmpeg_error: Option<String>,
}

impl State {
    fn item_mut(&mut self, id: Id) -> Option<&mut Item> {
        self.items.iter_mut().find(|i| i.id == id)
    }

    /// Why a link cannot be added, if it cannot.
    fn refuse(&self, url: &str) -> Option<&'static str> {
        if !Url::parse(url).is_ok_and(|u| matches!(u.scheme(), "http" | "https")) {
            Some("not an http or https link")
        } else if self
            .items
            .iter()
            .any(|i| i.url == url && i.status != Status::Completed)
        {
            Some("already in the list")
        } else {
            None
        }
    }

    fn new_item(&mut self, url: &str, headers: &[(String, String)], start: bool) -> Item {
        let file_name = file_name_from_url(url);
        let item = Item {
            id: self.next_id,
            url: url.to_string(),
            headers: headers.to_vec(),
            category: Category::from_file_name(&file_name),
            file_name,
            path: None,
            total: None,
            downloaded: 0,
            resumable: None,
            status: if start {
                Status::Queued
            } else {
                Status::Paused
            },
            error: None,
            added_at: now_ms(),
            completed_at: None,
            last_try_at: None,
            description: String::new(),
            speed_limit: 0,
            open_when_done: false,
            stream: stream_kind(url, None).map(|_| StreamChoice::default()),
        };
        self.next_id += 1;
        item
    }

    /// `base`, or `base (1)`, `base (2)`... whichever is not on disk and
    /// not already the destination of another download.
    fn free_path(&self, base: &Path) -> PathBuf {
        (0..)
            .map(|n| numbered(base, n))
            .find(|p| {
                !p.exists()
                    && !partial_files(p)[0].exists()
                    && !stream_work_dir(p).exists()
                    && !self.items.iter().any(|i| i.path.as_deref() == Some(p))
            })
            .expect("some numbered name is free")
    }
}

struct Running {
    cancel: CancellationToken,
    handle: Option<JoinHandle<()>>,
    /// Set once the download has been prepared.
    progress: Option<Progress>,
    speed: SpeedMeter,
    removing: bool,
    /// This download's own speed limit, changeable while it runs.
    limiter: Arc<RateLimiter>,
}

impl Manager {
    /// Loads the list and settings from `data_dir`. Call [`Manager::start`]
    /// from inside a Tokio runtime to begin downloading.
    pub fn open(
        data_dir: impl Into<PathBuf>,
        default_download_dir: impl Into<PathBuf>,
        sink: impl EventSink,
    ) -> std::io::Result<Self> {
        let store = Store::new(data_dir.into());
        let default_dir = default_download_dir.into();
        let settings = store
            .load::<Settings>(SETTINGS_FILE)
            .sanitized(&default_dir);
        let mut list: SavedList = store.load(LIST_FILE);
        for item in &mut list.items {
            if matches!(item.status, Status::Downloading | Status::Queued) {
                item.status = if settings.resume_on_startup {
                    Status::Queued
                } else {
                    Status::Paused
                };
            }
        }
        let next_id = list
            .items
            .iter()
            .map(|i| i.id + 1)
            .chain([list.next_id, 1])
            .max()
            .unwrap_or(1);

        let client = build_client(&ClientOptions::default()).map_err(std::io::Error::other)?;
        Ok(Self(Arc::new(Inner {
            client,
            limiter: Arc::new(RateLimiter::new(settings.speed_limit)),
            store,
            sink: Box::new(sink),
            state: Mutex::new(State {
                settings,
                default_dir,
                items: list.items,
                next_id,
                running: HashMap::new(),
                shutting_down: false,
                ffmpeg_install: None,
                ffmpeg_error: None,
            }),
        })))
    }

    /// Starts queued downloads and the progress ticker.
    pub fn start(&self) {
        let weak = Arc::downgrade(&self.0);
        tokio::spawn(async move {
            let mut was_active = false;
            loop {
                tokio::time::sleep(TICK).await;
                let Some(inner) = weak.upgrade() else { break };
                if !Manager(inner).tick(&mut was_active) {
                    break;
                }
            }
        });
        self.pump();
    }

    pub fn items(&self) -> Vec<Item> {
        self.lock().items.clone()
    }

    pub fn settings(&self) -> Settings {
        self.lock().settings.clone()
    }

    /// Adds links to the list. With `start`, they join the queue; otherwise
    /// they wait paused ("download later").
    pub fn add(&self, urls: &[String], headers: &[(String, String)], start: bool) -> AddReport {
        let mut report = AddReport::default();
        {
            let mut st = self.lock();
            for url in urls.iter().map(|u| u.trim()).filter(|u| !u.is_empty()) {
                if let Some(reason) = st.refuse(url) {
                    report.skipped.push(Skipped {
                        url: url.to_string(),
                        reason: reason.to_string(),
                    });
                    continue;
                }
                let item = st.new_item(url, headers, start);
                st.items.push(item.clone());
                report.added.push(item);
            }
            if !report.added.is_empty() {
                self.save_list(&st);
            }
        }
        for item in &report.added {
            self.0.sink.emit(Event::Updated { item: item.clone() });
        }
        self.pump();
        report
    }

    pub fn item(&self, id: Id) -> Option<Item> {
        self.lock().items.iter().find(|i| i.id == id).cloned()
    }

    /// Asks the server about a link without adding it: name, size, whether
    /// it can resume, and a free place to save it.
    pub async fn inspect(
        &self,
        url: &str,
        headers: &[(String, String)],
    ) -> Result<LinkInfo, String> {
        let remote = probe(&self.0.client, url, headers)
            .await
            .map_err(|e| describe(&e))?;
        let category = Category::from_file_name(&remote.file_name);
        let st = self.lock();
        let suggested_path =
            st.free_path(&st.settings.folder_for(category).join(&remote.file_name));
        Ok(LinkInfo {
            url: url.to_string(),
            file_name: remote.file_name.clone(),
            total: remote.total,
            resumable: remote.ranges && remote.total.is_some(),
            content_type: remote.content_type.clone(),
            web_page: page_instead_of_file(&remote, &remote.file_name),
            stream: stream_kind(url, remote.content_type.as_deref()),
            category,
            suggested_path,
        })
    }

    /// Lists the qualities a video stream offers.
    pub async fn inspect_stream(
        &self,
        url: &str,
        headers: &[(String, String)],
    ) -> Result<StreamLinkInfo, String> {
        let info = inspect_stream(&self.0.client, url, headers)
            .await
            .map_err(|e| describe(&e))?;
        let folder = self.lock().settings.folder_for(Category::Video);
        Ok(StreamLinkInfo {
            kind: info.kind,
            variants: info.variants,
            duration: info.duration,
            folder,
            ffmpeg: self.ffmpeg().is_some(),
        })
    }

    /// A free place for `file_name` in its category's folder.
    pub fn suggest_path(&self, file_name: &str) -> PathBuf {
        let st = self.lock();
        let category = Category::from_file_name(file_name);
        st.free_path(&st.settings.folder_for(category).join(file_name))
    }

    /// The ffmpeg to use: the one chosen in Options, one fetched earlier,
    /// or one installed on the computer.
    pub fn ffmpeg(&self) -> Option<PathBuf> {
        find_ffmpeg(&self.ffmpeg_places())
    }

    /// Where ffmpeg is looked for first. A download checks again when it
    /// needs it, so an ffmpeg fetched while it runs is used.
    fn ffmpeg_places(&self) -> Vec<PathBuf> {
        let chosen = self.lock().settings.ffmpeg_path.clone();
        let fetched = ffmpeg::installed_path(self.0.store.dir());
        chosen.into_iter().chain([fetched]).collect()
    }

    pub fn ffmpeg_status(&self) -> FfmpegStatus {
        let path = self.ffmpeg();
        let st = self.lock();
        FfmpegStatus {
            path,
            can_install: ffmpeg::download_url().is_some(),
            installing: st.ffmpeg_install.is_some(),
            downloaded: st.ffmpeg_install.map_or(0, |p| p.0),
            total: st.ffmpeg_install.and_then(|p| p.1),
            error: st.ffmpeg_error.clone(),
        }
    }

    /// Fetches a build of ffmpeg for this computer in the background,
    /// reporting progress with [`Event::Ffmpeg`].
    pub fn install_ffmpeg(&self) -> Result<(), String> {
        let url =
            ffmpeg::download_url().ok_or("there is no ready-made ffmpeg for this computer")?;
        self.install_ffmpeg_from(url)
    }

    /// [`Manager::install_ffmpeg`] from a given zip.
    pub fn install_ffmpeg_from(&self, url: &str) -> Result<(), String> {
        {
            let mut st = self.lock();
            if st.ffmpeg_install.is_some() {
                return Ok(());
            }
            st.ffmpeg_install = Some((0, None));
            st.ffmpeg_error = None;
        }
        self.emit_ffmpeg();
        let manager = self.clone();
        let url = url.to_string();
        tokio::spawn(async move {
            let weak = Arc::downgrade(&manager.0);
            let report = move |downloaded, total| {
                if let Some(inner) = weak.upgrade() {
                    let manager = Manager(inner);
                    manager.lock().ffmpeg_install = Some((downloaded, total));
                    manager.emit_ffmpeg();
                }
            };
            let result = ffmpeg::install(
                manager.0.client.clone(),
                &url,
                manager.0.store.dir(),
                report,
            )
            .await;
            {
                let mut st = manager.lock();
                st.ffmpeg_install = None;
                st.ffmpeg_error = result.err();
            }
            manager.emit_ffmpeg();
        });
        Ok(())
    }

    fn emit_ffmpeg(&self) {
        self.0.sink.emit(Event::Ffmpeg {
            status: self.ffmpeg_status(),
        });
    }

    /// A first guess from the link alone, shown while [`Manager::inspect`]
    /// waits for the server.
    pub fn guess(&self, url: &str) -> LinkInfo {
        let file_name = file_name_from_url(url);
        let category = Category::from_file_name(&file_name);
        let st = self.lock();
        LinkInfo {
            url: url.to_string(),
            suggested_path: st.free_path(&st.settings.folder_for(category).join(&file_name)),
            file_name,
            total: None,
            resumable: false,
            content_type: None,
            web_page: false,
            stream: stream_kind(url, None),
            category,
        }
    }

    /// Adds one download with the choices made in the File Info window.
    pub fn add_download(&self, new: NewDownload) -> Result<Item, String> {
        let url = new.url.trim();
        let item = {
            let mut st = self.lock();
            if let Some(reason) = st.refuse(url) {
                return Err(reason.to_string());
            }
            let mut item = st.new_item(url, &new.headers, new.start);
            if let Some(path) = new.path {
                item.file_name = file_name_of(&path);
                item.category = Category::from_file_name(&item.file_name);
                item.path = Some(path);
            }
            if let Some(category) = new.category {
                item.category = category;
            }
            item.description = new.description.trim().to_string();
            if new.stream.is_some() {
                item.stream = new.stream;
            }
            st.items.push(item.clone());
            self.save_list(&st);
            item
        };
        self.0.sink.emit(Event::Updated { item: item.clone() });
        self.pump();
        Ok(item)
    }

    /// Sets one download's own speed limit (0 for none), even while it runs.
    pub fn set_speed_limit(&self, id: Id, bytes_per_sec: u64) {
        self.update_item(id, |item| item.speed_limit = bytes_per_sec);
        if let Some(running) = self.lock().running.get(&id) {
            running.limiter.set_limit(bytes_per_sec);
        }
    }

    pub fn set_open_when_done(&self, id: Id, open: bool) {
        self.update_item(id, |item| item.open_when_done = open);
    }

    pub fn set_description(&self, id: Id, description: &str) {
        self.update_item(id, |item| item.description = description.trim().to_string());
    }

    /// Takes finished downloads off the list; their files stay on disk.
    pub fn delete_completed(&self) -> usize {
        let removed: Vec<Id> = {
            let mut st = self.lock();
            let ids = st
                .items
                .iter()
                .filter(|i| i.status == Status::Completed)
                .map(|i| i.id)
                .collect();
            st.items.retain(|i| i.status != Status::Completed);
            self.save_list(&st);
            ids
        };
        let count = removed.len();
        self.emit_all(
            removed
                .into_iter()
                .map(|id| Event::Removed { id })
                .collect(),
        );
        count
    }

    fn update_item(&self, id: Id, change: impl FnOnce(&mut Item)) {
        let event = {
            let mut st = self.lock();
            let event = st.item_mut(id).map(|item| {
                change(item);
                Event::Updated { item: item.clone() }
            });
            self.save_list(&st);
            event
        };
        self.emit_all(event.into_iter().collect());
    }

    pub fn pause(&self, ids: &[Id]) {
        let mut events = Vec::new();
        {
            let mut st = self.lock();
            for &id in ids {
                if let Some(running) = st.running.get(&id) {
                    // The task marks the item paused once it has saved.
                    running.cancel.cancel();
                } else if let Some(item) = st.item_mut(id).filter(|i| i.status == Status::Queued) {
                    item.status = Status::Paused;
                    events.push(Event::Updated { item: item.clone() });
                }
            }
            self.save_list(&st);
        }
        self.emit_all(events);
    }

    /// Puts paused or failed downloads back in the queue.
    pub fn resume(&self, ids: &[Id]) {
        let mut events = Vec::new();
        {
            let mut st = self.lock();
            for &id in ids {
                if let Some(item) = st
                    .item_mut(id)
                    .filter(|i| matches!(i.status, Status::Paused | Status::Failed))
                {
                    item.status = Status::Queued;
                    item.error = None;
                    events.push(Event::Updated { item: item.clone() });
                }
            }
            self.save_list(&st);
        }
        self.emit_all(events);
        self.pump();
    }

    pub fn pause_all(&self) {
        let ids: Vec<Id> = self.lock().items.iter().map(|i| i.id).collect();
        self.pause(&ids);
    }

    pub fn resume_all(&self) {
        let ids: Vec<Id> = self.lock().items.iter().map(|i| i.id).collect();
        self.resume(&ids);
    }

    /// Removes downloads from the list, stopping them first. With
    /// `delete_files`, also deletes the downloaded file or partial data.
    pub async fn remove(&self, ids: &[Id], delete_files: bool) {
        let mut handles = Vec::new();
        {
            let mut st = self.lock();
            for id in ids {
                if let Some(running) = st.running.get_mut(id) {
                    running.removing = true;
                    running.cancel.cancel();
                    handles.extend(running.handle.take());
                }
            }
        }
        for handle in handles {
            let _ = handle.await;
        }

        let mut events = Vec::new();
        {
            let mut st = self.lock();
            for &id in ids {
                let Some(pos) = st.items.iter().position(|i| i.id == id) else {
                    continue;
                };
                let item = st.items.remove(pos);
                if delete_files {
                    delete_item_files(&item);
                }
                events.push(Event::Removed { id });
            }
            self.save_list(&st);
        }
        self.emit_all(events);
    }

    /// Applies new settings and returns them as stored (values clamped).
    pub fn set_settings(&self, settings: Settings) -> Settings {
        let settings = {
            let mut st = self.lock();
            let settings = settings.sanitized(&st.default_dir);
            st.settings = settings.clone();
            if let Err(e) = self.0.store.save(SETTINGS_FILE, &settings) {
                eprintln!("could not save settings: {e}");
            }
            settings
        };
        self.0.limiter.set_limit(settings.speed_limit);
        self.pump();
        settings
    }

    /// Pauses everything and saves, so running downloads continue on the
    /// next start (if `resume_on_startup` is set).
    pub async fn shutdown(&self) {
        let handles: Vec<_> = {
            let mut st = self.lock();
            st.shutting_down = true;
            st.running
                .values_mut()
                .filter_map(|running| {
                    running.cancel.cancel();
                    running.handle.take()
                })
                .collect()
        };
        for handle in handles {
            let _ = handle.await;
        }
        let st = self.lock();
        self.save_list(&st);
    }

    fn lock(&self) -> MutexGuard<'_, State> {
        self.0.state.lock().unwrap()
    }

    fn emit_all(&self, events: Vec<Event>) {
        for event in events {
            self.0.sink.emit(event);
        }
    }

    fn save_list(&self, st: &State) {
        let list = SavedList {
            next_id: st.next_id,
            items: st.items.clone(),
        };
        if let Err(e) = self.0.store.save(LIST_FILE, &list) {
            eprintln!("could not save the download list: {e}");
        }
    }

    /// Starts queued downloads, oldest first, while there is room.
    fn pump(&self) {
        let mut events = Vec::new();
        {
            let mut st = self.lock();
            if st.shutting_down {
                return;
            }
            let mut started = false;
            while st.running.len() < st.settings.max_active {
                let Some(item) = st.items.iter_mut().find(|i| i.status == Status::Queued) else {
                    break;
                };
                item.status = Status::Downloading;
                item.error = None;
                item.last_try_at = Some(now_ms());
                let id = item.id;
                let limiter = Arc::new(RateLimiter::new(item.speed_limit));
                events.push(Event::Updated { item: item.clone() });

                let cancel = CancellationToken::new();
                let handle = tokio::spawn(self.clone().run_item(id, cancel.clone()));
                st.running.insert(
                    id,
                    Running {
                        cancel,
                        handle: Some(handle),
                        progress: None,
                        speed: SpeedMeter::default(),
                        removing: false,
                        limiter,
                    },
                );
                started = true;
            }
            if started {
                self.save_list(&st);
            }
        }
        self.emit_all(events);
    }

    async fn run_item(self, id: Id, cancel: CancellationToken) {
        let result = self.download(id, &cancel).await;
        self.finish(id, result);
    }

    async fn download(
        &self,
        id: Id,
        cancel: &CancellationToken,
    ) -> slipstream_engine::Result<Outcome> {
        let (item, settings, own_limiter) = {
            let st = self.lock();
            match (st.items.iter().find(|i| i.id == id), st.running.get(&id)) {
                (Some(item), Some(running)) => {
                    (item.clone(), st.settings.clone(), running.limiter.clone())
                }
                _ => return Ok(Outcome::Paused),
            }
        };
        let options = Options {
            connections: settings.connections,
            limiters: vec![self.0.limiter.clone(), own_limiter],
            ..Options::default()
        };
        let client = self.0.client.clone();
        if let Some(choice) = &item.stream {
            let path = match &item.path {
                Some(path) => path.clone(),
                None => {
                    let st = self.lock();
                    let stem = file_name_from_url(&item.url)
                        .rsplit_once('.')
                        .map_or_else(|| "video".to_string(), |(stem, _)| stem.to_string());
                    let folder = st.settings.folder_for(Category::Video);
                    st.free_path(&folder.join(format!("{stem}.mp4")))
                }
            };
            let request = StreamRequest {
                url: item.url.clone(),
                headers: item.headers.clone(),
                variant: choice.variant.clone(),
                path,
                ffmpeg: self
                    .ffmpeg_places()
                    .into_iter()
                    .chain(self.ffmpeg())
                    .collect(),
            };
            let download = tokio::select! {
                _ = cancel.cancelled() => return Ok(Outcome::Paused),
                download = StreamDownload::prepare(client, request, options) => download?,
            };
            let info = download.info().clone();
            self.started(
                id,
                download.progress(),
                &info.path,
                info.total,
                true,
                info.resumed_bytes,
            );
            return download.run(cancel.clone()).await;
        }
        let request = |output| Request {
            url: item.url.clone(),
            headers: item.headers.clone(),
            output,
        };
        let prepare = async {
            match &item.path {
                Some(path) => {
                    let download =
                        Download::prepare(client, request(Output::File(path.clone())), options)
                            .await?;
                    // Checked before anything touches the file, so a link
                    // that expired never overwrites what was downloaded.
                    if page_instead_of_file(&download.info().remote, &file_name_of(path)) {
                        return Err(slipstream_engine::Error::WebPage);
                    }
                    Ok(download)
                }
                None => {
                    // Probe first: the server's file name decides the folder.
                    let remote = probe(&client, &item.url, &item.headers).await?;
                    if page_instead_of_file(&remote, &remote.file_name) {
                        return Err(slipstream_engine::Error::WebPage);
                    }
                    let folder = settings.folder_for(Category::from_file_name(&remote.file_name));
                    Download::prepare_probed(client, request(Output::Dir(folder)), options, remote)
                        .await
                }
            }
        };
        let download = tokio::select! {
            _ = cancel.cancelled() => return Ok(Outcome::Paused),
            download = prepare => download?,
        };

        let info = download.info();
        self.started(
            id,
            download.progress(),
            &info.path,
            info.total,
            info.resumable,
            info.resumed_bytes,
        );
        download.run(cancel.clone()).await
    }

    /// Records what a download learned while preparing, before it runs.
    fn started(
        &self,
        id: Id,
        progress: Progress,
        path: &Path,
        total: Option<u64>,
        resumable: bool,
        resumed: u64,
    ) {
        let event = {
            let mut st = self.lock();
            if let Some(running) = st.running.get_mut(&id) {
                running.progress = Some(progress);
                running.speed.restart(resumed);
            }
            let event = st.item_mut(id).map(|item| {
                item.file_name = file_name_of(path);
                item.category = Category::from_file_name(&item.file_name);
                item.path = Some(path.to_path_buf());
                item.total = total;
                item.resumable = Some(resumable);
                item.downloaded = resumed;
                Event::Updated { item: item.clone() }
            });
            self.save_list(&st);
            event
        };
        self.emit_all(event.into_iter().collect());
    }

    fn finish(&self, id: Id, result: slipstream_engine::Result<Outcome>) {
        let mut events = Vec::new();
        {
            let mut st = self.lock();
            let running = st.running.remove(&id);
            let shutting_down = st.shutting_down;
            if running.as_ref().is_some_and(|r| r.removing) {
                return;
            }
            let live = running
                .as_ref()
                .and_then(|r| r.progress.as_ref())
                .map(|p| p.snapshot());
            if let Some(item) = st.item_mut(id) {
                if let Some(live) = live {
                    item.downloaded = live.downloaded;
                    item.total = live.total.or(item.total);
                }
                let mut finished = false;
                match result {
                    Ok(Outcome::Completed(path)) => {
                        item.total = fs::metadata(&path).map(|m| m.len()).ok().or(item.total);
                        item.downloaded = item.total.unwrap_or(item.downloaded);
                        item.file_name = file_name_of(&path);
                        item.path = Some(path);
                        item.status = Status::Completed;
                        item.error = None;
                        item.completed_at = Some(now_ms());
                        finished = true;
                    }
                    // Paused by the user, or by shutdown (then it resumes
                    // on the next start).
                    Ok(Outcome::Paused) if shutting_down => item.status = Status::Queued,
                    Ok(Outcome::Paused) => item.status = Status::Paused,
                    Err(e) => {
                        item.status = Status::Failed;
                        item.error = Some(describe(&e));
                    }
                }
                events.push(Event::Updated { item: item.clone() });
                if finished {
                    events.push(Event::Finished { item: item.clone() });
                }
            }
            self.save_list(&st);
        }
        self.emit_all(events);
        self.pump();
    }

    /// Samples running downloads and sends a progress event. Returns false
    /// once the manager is shutting down.
    fn tick(&self, was_active: &mut bool) -> bool {
        let event = {
            let mut st = self.lock();
            if st.shutting_down {
                return false;
            }
            let now = Instant::now();
            let State { running, items, .. } = &mut *st;
            let mut lives = Vec::new();
            let mut total_speed = 0.0;
            for (&id, run) in running.iter_mut() {
                let Some(progress) = &run.progress else {
                    continue;
                };
                let snap = progress.snapshot();
                let speed = run.speed.sample(now, snap.downloaded);
                total_speed += speed;
                if let Some(item) = items.iter_mut().find(|i| i.id == id) {
                    item.downloaded = snap.downloaded;
                    item.total = snap.total.or(item.total);
                }
                lives.push(Live {
                    id,
                    downloaded: snap.downloaded,
                    total: snap.total,
                    speed,
                    connections: snap.connections,
                    segments: snap.segments,
                    stream: snap.stream,
                });
            }
            // After the last download stops, send one empty update so the
            // UI clears its speed display.
            let active = !lives.is_empty();
            let send = active || *was_active;
            *was_active = active;
            send.then_some(Event::Progress {
                items: lives,
                total_speed,
            })
        };
        if let Some(event) = event {
            self.0.sink.emit(event);
        }
        true
    }
}

/// Bytes per second over the last second: quick to follow a change in
/// speed, without jumping around on every update.
#[derive(Default)]
struct SpeedMeter {
    samples: VecDeque<(Instant, u64)>,
}

impl SpeedMeter {
    fn restart(&mut self, bytes: u64) {
        self.samples.clear();
        self.samples.push_back((Instant::now(), bytes));
    }

    fn sample(&mut self, now: Instant, bytes: u64) -> f64 {
        self.samples.push_back((now, bytes));
        while self.samples.len() > 2 && now.duration_since(self.samples[1].0) >= SPEED_WINDOW {
            self.samples.pop_front();
        }
        let (since, from) = self.samples[0];
        let seconds = now.duration_since(since).as_secs_f64();
        if seconds < 0.05 {
            0.0
        } else {
            bytes.saturating_sub(from) as f64 / seconds
        }
    }
}

/// A small HTML page where some other kind of file was expected: the link
/// expired and the site sent its download page instead.
fn page_instead_of_file(remote: &slipstream_engine::RemoteInfo, file_name: &str) -> bool {
    let named_as_page = file_name.rsplit_once('.').is_some_and(|(_, ext)| {
        matches!(
            ext.to_ascii_lowercase().as_str(),
            "html" | "htm" | "xhtml" | "shtml" | "mht" | "mhtml"
        )
    });
    remote.is_web_page() && !named_as_page && remote.total.is_none_or(|t| t < MAX_STRAY_PAGE)
}

fn delete_item_files(item: &Item) {
    let Some(path) = &item.path else { return };
    if item.status == Status::Completed {
        let _ = fs::remove_file(path);
    }
    for partial in partial_files(path) {
        let _ = fs::remove_file(partial);
    }
    let _ = fs::remove_dir_all(stream_work_dir(path));
}

fn numbered(base: &Path, n: u32) -> PathBuf {
    if n == 0 {
        return base.to_path_buf();
    }
    let name = file_name_of(base);
    let (stem, ext) = match name.rfind('.') {
        Some(dot) if dot > 0 => (&name[..dot], &name[dot..]),
        _ => (name.as_str(), ""),
    };
    base.with_file_name(format!("{stem} ({n}){ext}"))
}

fn file_name_of(path: &Path) -> String {
    path.file_name()
        .map(|n| n.to_string_lossy().into_owned())
        .unwrap_or_default()
}

/// The error and its causes, e.g. "network error: ... : connection refused".
fn describe(err: &dyn std::error::Error) -> String {
    let mut text = err.to_string();
    let mut source = err.source();
    while let Some(cause) = source {
        text.push_str(": ");
        text.push_str(&cause.to_string());
        source = cause.source();
    }
    text
}

fn now_ms() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map_or(0, |d| d.as_millis() as u64)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn speed_follows_changes_within_a_second() {
        let start = Instant::now();
        let mut meter = SpeedMeter::default();
        let mut bytes = 0u64;
        let mut speed = 0.0;
        // 2 s at 1 MB/s, then 4 MB/s, sampled every 200 ms.
        for tick in 1..=20u64 {
            bytes += if tick <= 10 { 200_000 } else { 800_000 };
            speed = meter.sample(start + Duration::from_millis(200 * tick), bytes);
            if tick == 10 {
                assert!((speed - 1_000_000.0).abs() < 1.0, "{speed}");
            }
        }
        assert!((speed - 4_000_000.0).abs() < 1.0, "{speed}");
    }
}
