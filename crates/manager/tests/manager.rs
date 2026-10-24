//! End-to-end tests of the download manager against a local server.

use std::collections::HashSet;
use std::path::Path;
use std::sync::atomic::Ordering;
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use slipstream_manager::{Category, Event, Id, Item, Manager, Settings, Status};
use slipstream_test_server::{MB, data, start};
use tempfile::TempDir;

type Events = Arc<Mutex<Vec<Event>>>;

struct Harness {
    manager: Manager,
    events: Events,
    data_dir: TempDir,
    downloads: TempDir,
}

impl Harness {
    fn new() -> Self {
        let data_dir = tempfile::tempdir().unwrap();
        let downloads = tempfile::tempdir().unwrap();
        let (manager, events) = open(data_dir.path(), downloads.path());
        Self {
            manager,
            events,
            data_dir,
            downloads,
        }
    }

    /// Simulates quitting and relaunching the app.
    fn reopen(&mut self) {
        let (manager, events) = open(self.data_dir.path(), self.downloads.path());
        self.manager = manager;
        self.events = events;
    }

    fn configure(&self, change: impl FnOnce(&mut Settings)) {
        let mut settings = self.manager.settings();
        change(&mut settings);
        self.manager.set_settings(settings);
    }

    fn add(&self, url: &str) -> Id {
        let report = self.manager.add(&[url.to_string()], &[], true);
        assert!(report.skipped.is_empty(), "{:?}", report.skipped);
        report.added[0].id
    }

    async fn wait(&self, id: Id, what: impl Fn(&Item) -> bool) -> Item {
        let deadline = Instant::now() + Duration::from_secs(20);
        loop {
            if let Some(item) = self.manager.items().into_iter().find(|i| i.id == id)
                && what(&item)
            {
                return item;
            }
            assert!(
                Instant::now() < deadline,
                "timed out: {:#?}",
                self.manager.items()
            );
            tokio::time::sleep(Duration::from_millis(20)).await;
        }
    }

    async fn completed(&self, id: Id) -> Item {
        self.wait(id, |i| {
            assert_ne!(i.status, Status::Failed, "{:?}", i.error);
            i.status == Status::Completed
        })
        .await
    }
}

fn open(data_dir: &Path, downloads: &Path) -> (Manager, Events) {
    let events = Events::default();
    let sink = {
        let events = events.clone();
        move |event| events.lock().unwrap().push(event)
    };
    (Manager::open(data_dir, downloads, sink).unwrap(), events)
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn downloads_into_category_folder() {
    let content = data(3 * MB, 1);
    let (server, _) = start(content.clone()).await;
    let h = Harness::new();
    h.manager.start();

    let report = h.manager.add(&[server.url("files/movie.mp4")], &[], true);
    assert_eq!(report.added[0].category, Category::Video);
    assert_eq!(report.added[0].file_name, "movie.mp4");
    let item = h.completed(report.added[0].id).await;

    let expected = h.downloads.path().join("Video").join("movie.mp4");
    assert_eq!(item.path.as_deref(), Some(expected.as_path()));
    assert!(std::fs::read(&expected).unwrap() == content);
    assert_eq!(item.downloaded, content.len() as u64);
    let events = h.events.lock().unwrap();
    assert!(
        events
            .iter()
            .any(|e| matches!(e, Event::Finished { item } if item.id == report.added[0].id))
    );
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn queue_runs_one_download_at_a_time() {
    let (server, _) = start(data(MB, 2)).await;
    server.per_conn_rate.store(4 * MB as u64, Ordering::SeqCst);
    let h = Harness::new();
    h.configure(|s| {
        s.max_active = 1;
        s.connections = 2;
    });
    h.manager.start();

    let ids: Vec<Id> = ["a.bin", "b.bin", "c.bin"]
        .iter()
        .map(|name| h.add(&server.url(name)))
        .collect();
    let mut finished = Vec::new();
    for &id in &ids {
        finished.push(h.completed(id).await.completed_at.unwrap());
    }

    let mut active = HashSet::new();
    let mut most = 0;
    for event in h.events.lock().unwrap().iter() {
        if let Event::Updated { item } = event {
            if item.status == Status::Downloading {
                active.insert(item.id);
            } else {
                active.remove(&item.id);
            }
            most = most.max(active.len());
        }
    }
    assert_eq!(most, 1, "more than one download ran at once");
    assert!(finished.is_sorted(), "queue did not run in order");
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn pauses_and_resumes() {
    let content = data(8 * MB, 3);
    let (server, url) = start(content.clone()).await;
    server.per_conn_rate.store(MB as u64, Ordering::SeqCst);
    let h = Harness::new();
    h.configure(|s| s.connections = 4);
    h.manager.start();

    let id = h.add(&url);
    h.wait(id, |i| i.downloaded >= MB as u64).await;
    h.manager.pause(&[id]);
    let paused = h.wait(id, |i| i.status == Status::Paused).await;
    let path = paused.path.unwrap();
    assert!(slipstream_engine::partial_files(&path)[0].exists());
    assert!(paused.downloaded >= MB as u64);

    server.per_conn_rate.store(0, Ordering::SeqCst);
    h.manager.resume(&[id]);
    let item = h.completed(id).await;
    assert_eq!(item.path.as_deref(), Some(path.as_path()));
    assert!(std::fs::read(&path).unwrap() == content);
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn continues_after_app_restart() {
    let content = data(8 * MB, 4);
    let (server, url) = start(content.clone()).await;
    server.per_conn_rate.store(MB as u64, Ordering::SeqCst);
    let mut h = Harness::new();
    h.configure(|s| s.connections = 4);
    h.manager.start();

    let id = h.add(&url);
    h.wait(id, |i| i.downloaded >= MB as u64).await;
    h.manager.shutdown().await;
    let before = h.manager.items()[0].clone();
    assert_eq!(before.status, Status::Queued, "should resume next launch");

    server.per_conn_rate.store(0, Ordering::SeqCst);
    let sent_before = server.bytes_sent.load(Ordering::SeqCst);
    h.reopen();
    assert_eq!(h.manager.items()[0].status, Status::Queued);
    h.manager.start();
    let item = h.completed(id).await;

    assert_eq!(item.path, before.path);
    assert!(std::fs::read(item.path.unwrap()).unwrap() == content);
    let refetched = server.bytes_sent.load(Ordering::SeqCst) - sent_before;
    assert!(
        refetched < content.len() as u64,
        "restart downloaded everything again"
    );
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn removes_with_files() {
    let (server, url) = start(data(8 * MB, 5)).await;
    server.per_conn_rate.store(MB as u64, Ordering::SeqCst);
    let h = Harness::new();
    h.manager.start();

    let id = h.add(&url);
    let item = h.wait(id, |i| i.path.is_some() && i.downloaded > 0).await;
    h.manager.remove(&[id], true).await;

    assert!(h.manager.items().is_empty());
    let path = item.path.unwrap();
    for file in slipstream_engine::partial_files(&path) {
        assert!(!file.exists(), "{} left behind", file.display());
    }
    assert!(!path.exists());
    assert!(
        h.events
            .lock()
            .unwrap()
            .iter()
            .any(|e| matches!(e, Event::Removed { id: removed } if *removed == id))
    );

    // A finished download's file goes too.
    server.per_conn_rate.store(0, Ordering::SeqCst);
    let id = h.add(&server.url("done.zip"));
    let path = h.completed(id).await.path.unwrap();
    h.manager.remove(&[id], true).await;
    assert!(!path.exists());
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn rejects_duplicates_and_bad_links() {
    let (_server, url) = start(data(10, 6)).await;
    let h = Harness::new();

    let report = h.manager.add(
        &[
            url.clone(),
            url.clone(),
            "ftp://example.com/file".into(),
            "not a link".into(),
            "   ".into(),
        ],
        &[],
        false,
    );
    assert_eq!(report.added.len(), 1);
    assert_eq!(report.added[0].status, Status::Paused, "download later");
    let reasons: Vec<&str> = report.skipped.iter().map(|s| s.reason.as_str()).collect();
    assert_eq!(
        reasons,
        [
            "already in the list",
            "not an http or https link",
            "not an http or https link"
        ]
    );
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn settings_and_list_survive_restart() {
    let (_server, url) = start(data(10, 7)).await;
    let mut h = Harness::new();
    h.configure(|s| {
        s.max_active = 2;
        s.connections = 100;
        s.speed_limit = MB as u64;
        s.use_categories = false;
    });
    h.manager.add(&[url], &[], false);
    let settings = h.manager.settings();
    assert_eq!(settings.connections, 32, "connections are capped");
    assert_eq!(settings.download_dir, h.downloads.path());

    h.reopen();
    assert_eq!(h.manager.settings(), settings);
    let items = h.manager.items();
    assert_eq!(items.len(), 1);
    assert_eq!(items[0].status, Status::Paused);
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn reports_failures() {
    let (server, _) = start(data(10, 8)).await;
    let h = Harness::new();
    h.manager.start();

    let id = h.add(&server.url("missing"));
    let item = h.wait(id, |i| i.status == Status::Failed).await;

    let error = item.error.unwrap();
    assert!(error.contains("404"), "{error}");
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn same_name_downloads_get_separate_files() {
    let content = data(2 * MB, 9);
    let (server, _) = start(content.clone()).await;
    let h = Harness::new();
    h.manager.start();

    let a = h.add(&server.url("a/report.bin"));
    let b = h.add(&server.url("b/report.bin"));
    let a = h.completed(a).await.path.unwrap();
    let b = h.completed(b).await.path.unwrap();

    assert_ne!(a, b);
    assert!(std::fs::read(&a).unwrap() == content);
    assert!(std::fs::read(&b).unwrap() == content);
}
