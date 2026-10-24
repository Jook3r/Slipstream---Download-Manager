//! The download manager behind the desktop app: a list of downloads, a queue
//! that runs a few at a time, IDM-style categories, settings and
//! persistence. It knows nothing about windows; the app subscribes to
//! [`Event`]s through an [`EventSink`].

mod category;
mod store;

use std::collections::{BTreeMap, HashMap};
use std::fs;
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex, MutexGuard};
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

use serde::{Deserialize, Serialize};
use slipstream_engine::probe::file_name_from_url;
use slipstream_engine::segments::SegmentView;
use slipstream_engine::{
    Client, ClientOptions, Download, Options, Outcome, Output, Progress, RateLimiter, Request, Url,
    build_client, partial_files, probe,
};
use tokio::task::JoinHandle;
use tokio_util::sync::CancellationToken;

pub use category::Category;
use store::Store;

pub type Id = u64;

const LIST_FILE: &str = "downloads.json";
const SETTINGS_FILE: &str = "settings.json";
/// How often live progress is sent to the UI.
const TICK: Duration = Duration::from_millis(500);

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
    /// Sent every half second while anything is downloading.
    Progress {
        items: Vec<Live>,
        total_speed: f64,
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
    last_bytes: u64,
    last_at: Instant,
    speed: f64,
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
            category,
            suggested_path,
        })
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
                        last_bytes: 0,
                        last_at: Instant::now(),
                        speed: 0.0,
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
        let request = |output| Request {
            url: item.url.clone(),
            headers: item.headers.clone(),
            output,
        };
        let prepare = async {
            match &item.path {
                Some(path) => {
                    Download::prepare(client, request(Output::File(path.clone())), options).await
                }
                None => {
                    // Probe first: the server's file name decides the folder.
                    let remote = probe(&client, &item.url, &item.headers).await?;
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

        let event = {
            let mut st = self.lock();
            let info = download.info();
            if let Some(running) = st.running.get_mut(&id) {
                running.progress = Some(download.progress());
                running.last_bytes = info.resumed_bytes;
                running.last_at = Instant::now();
            }
            let event = st.item_mut(id).map(|item| {
                item.file_name = file_name_of(&info.path);
                item.category = Category::from_file_name(&item.file_name);
                item.path = Some(info.path.clone());
                item.total = info.total;
                item.resumable = Some(info.resumable);
                item.downloaded = info.resumed_bytes;
                Event::Updated { item: item.clone() }
            });
            self.save_list(&st);
            event
        };
        self.emit_all(event.into_iter().collect());
        download.run(cancel.clone()).await
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
                let dt = now.duration_since(run.last_at).as_secs_f64();
                if dt > 0.0 {
                    let instant = snap.downloaded.saturating_sub(run.last_bytes) as f64 / dt;
                    run.speed = if run.speed == 0.0 {
                        instant
                    } else {
                        run.speed * 0.6 + instant * 0.4
                    };
                }
                run.last_bytes = snap.downloaded;
                run.last_at = now;
                total_speed += run.speed;
                if let Some(item) = items.iter_mut().find(|i| i.id == id) {
                    item.downloaded = snap.downloaded;
                    item.total = snap.total.or(item.total);
                }
                lives.push(Live {
                    id,
                    downloaded: snap.downloaded,
                    total: snap.total,
                    speed: run.speed,
                    connections: snap.connections,
                    segments: snap.segments,
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

fn delete_item_files(item: &Item) {
    let Some(path) = &item.path else { return };
    if item.status == Status::Completed {
        let _ = fs::remove_file(path);
    }
    for partial in partial_files(path) {
        let _ = fs::remove_file(partial);
    }
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
