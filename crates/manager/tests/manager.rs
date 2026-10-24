//! End-to-end tests of the download manager against a local server.

use std::collections::HashSet;
use std::path::Path;
use std::sync::atomic::Ordering;
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use slipstream_engine::stream_work_dir;
use slipstream_manager::{
    Category, Event, Id, Item, Manager, NewDownload, Settings, Status, StreamChoice,
};
use slipstream_test_server::{MB, Server, data, start};
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

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn inspects_links_before_adding() {
    let (server, _) = start(data(3 * MB, 10)).await;
    let h = Harness::new();
    let video_dir = h.downloads.path().join("Video");
    std::fs::create_dir_all(&video_dir).unwrap();
    std::fs::write(video_dir.join("movie.mp4"), b"already here").unwrap();

    let info = h
        .manager
        .inspect(&server.url("files/movie.mp4"), &[])
        .await
        .unwrap();

    assert_eq!(info.file_name, "movie.mp4");
    assert_eq!(info.total, Some(3 * MB as u64));
    assert!(info.resumable);
    assert_eq!(info.category, Category::Video);
    assert_eq!(info.suggested_path, video_dir.join("movie (1).mp4"));
    assert!(h.manager.items().is_empty(), "inspecting must not add");

    let missing = h.manager.inspect(&server.url("missing"), &[]).await;
    assert!(missing.unwrap_err().contains("404"));
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn adds_with_chosen_path_and_description() {
    let content = data(2 * MB, 11);
    let (server, _) = start(content.clone()).await;
    let h = Harness::new();
    h.manager.start();
    let target = h.downloads.path().join("picked").join("renamed.iso");

    let item = h
        .manager
        .add_download(NewDownload {
            url: server.url("files/original.iso"),
            path: Some(target.clone()),
            description: "  release candidate ".into(),
            start: true,
            ..NewDownload::default()
        })
        .unwrap();
    assert_eq!(item.file_name, "renamed.iso");
    assert_eq!(item.description, "release candidate");
    let done = h.completed(item.id).await;

    assert_eq!(done.path.as_deref(), Some(target.as_path()));
    assert!(std::fs::read(&target).unwrap() == content);
    assert!(done.last_try_at.is_some());
    assert!(
        h.manager
            .add_download(NewDownload {
                url: "nonsense".into(),
                ..NewDownload::default()
            })
            .is_err()
    );
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn remembers_folders_per_category() {
    let (server, _) = start(data(10, 12)).await;
    let h = Harness::new();
    let music = h.downloads.path().join("My Music");
    h.configure(|s| {
        s.category_dirs.insert(Category::Music, music.clone());
    });

    let info = h
        .manager
        .inspect(&server.url("song.mp3"), &[])
        .await
        .unwrap();
    assert_eq!(info.suggested_path, music.join("song.mp3"));
    let guess = h.manager.guess(&server.url("other.pdf"));
    assert_eq!(
        guess.suggested_path,
        h.downloads.path().join("Documents").join("other.pdf")
    );
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn obeys_per_download_speed_limit() {
    let (server, _) = start(data(MB, 13)).await;
    let h = Harness::new();
    h.manager.start();
    let item = h
        .manager
        .add_download(NewDownload {
            url: server.url("limited.bin"),
            start: false,
            ..NewDownload::default()
        })
        .unwrap();
    h.manager.set_speed_limit(item.id, 512 * 1024);
    assert_eq!(h.manager.item(item.id).unwrap().speed_limit, 512 * 1024);

    let started = Instant::now();
    h.manager.resume(&[item.id]);
    h.completed(item.id).await;

    let elapsed = started.elapsed();
    assert!(
        elapsed >= Duration::from_millis(1500),
        "1 MB at 512 KB/s took {elapsed:?}"
    );
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn delete_completed_keeps_unfinished_and_files() {
    let (server, _) = start(data(MB, 14)).await;
    let h = Harness::new();
    h.manager.start();
    let done = h.add(&server.url("done.zip"));
    let path = h.completed(done).await.path.unwrap();
    h.manager.add(&[server.url("later.zip")], &[], false);

    assert_eq!(h.manager.delete_completed(), 1);

    let items = h.manager.items();
    assert_eq!(items.len(), 1);
    assert_eq!(items[0].file_name, "later.zip");
    assert!(path.exists(), "the finished file must stay on disk");
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn reports_which_pieces_are_active() {
    let (server, url) = start(data(8 * MB, 15)).await;
    server.per_conn_rate.store(MB as u64, Ordering::SeqCst);
    let h = Harness::new();
    h.configure(|s| s.connections = 4);
    h.manager.start();
    let id = h.add(&url);
    h.wait(id, |i| i.downloaded >= MB as u64).await;

    let events = h.events.lock().unwrap();
    let live = events
        .iter()
        .rev()
        .find_map(|e| match e {
            Event::Progress { items, .. } => items.iter().find(|l| l.id == id).cloned(),
            _ => None,
        })
        .expect("a progress event");
    assert!(live.segments.len() >= 4);
    assert!(live.segments.iter().filter(|s| s.active).count() >= 2);
    assert_eq!(live.segments[0].start, 0);
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn refuses_a_web_page_served_instead_of_the_file() {
    let (server, _) = start(b"<html>This link has expired</html>".to_vec()).await;
    *server.content_type.write().unwrap() = Some("text/html; charset=utf-8".into());
    let h = Harness::new();
    h.manager.start();

    let info = h
        .manager
        .inspect(&server.url("Tiny11.iso"), &[])
        .await
        .unwrap();
    assert!(info.web_page);
    let id = h.add(&server.url("Tiny11.iso"));
    let item = h.wait(id, |i| i.status == Status::Failed).await;

    assert!(item.error.unwrap().contains("web page"));
    assert!(
        std::fs::read_dir(h.downloads.path()).unwrap().all(|e| e
            .unwrap()
            .file_type()
            .unwrap()
            .is_dir())
    );

    // A link to an actual page still downloads.
    let page = h.add(&server.url("guide.html"));
    h.completed(page).await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn keeps_downloaded_data_when_the_link_expires() {
    let content = data(8 * MB, 16);
    let (server, url) = start(content.clone()).await;
    server.per_conn_rate.store(MB as u64, Ordering::SeqCst);
    let h = Harness::new();
    h.configure(|s| s.connections = 4);
    h.manager.start();
    let id = h.add(&url);
    h.wait(id, |i| i.downloaded >= MB as u64).await;
    h.manager.pause(&[id]);
    let paused = h.wait(id, |i| i.status == Status::Paused).await;

    // The link expires: the site now sends its download page.
    server.set_data(b"<html>expired</html>".to_vec(), "page");
    *server.content_type.write().unwrap() = Some("text/html".into());
    h.manager.resume(&[id]);
    let failed = h.wait(id, |i| i.status == Status::Failed).await;
    assert!(failed.error.unwrap().contains("web page"));
    let part = slipstream_engine::partial_files(paused.path.as_ref().unwrap())[0].clone();
    assert_eq!(
        std::fs::metadata(&part).unwrap().len(),
        content.len() as u64
    );

    // A fresh link: the download continues where it stopped.
    server.set_data(content.clone(), "v1");
    *server.content_type.write().unwrap() = None;
    server.per_conn_rate.store(0, Ordering::SeqCst);
    h.manager.resume(&[id]);
    let done = h.completed(id).await;
    assert!(std::fs::read(done.path.unwrap()).unwrap() == content);
}

/// A short HLS video at `v/index.m3u8`; returns the extension the finished
/// file will have. With ffmpeg installed the manager uses it to make an
/// MP4, so the video must be real; without, made-up pieces do and the
/// video stays a `.ts`.
fn serve_video(server: &Server, ffmpeg: Option<&Path>) -> &'static str {
    let Some(ffmpeg) = ffmpeg else {
        let mut playlist = "#EXTM3U\n#EXT-X-TARGETDURATION:2\n".to_string();
        for i in 0..12 {
            server.add_file(
                &format!("v/{i}.ts"),
                data(20_000 + i * 1000, i as u64),
                None,
            );
            playlist.push_str(&format!("#EXTINF:2,\n{i}.ts\n"));
        }
        server.add_file(
            "v/index.m3u8",
            (playlist + "#EXT-X-ENDLIST\n").into_bytes(),
            None,
        );
        return "ts";
    };
    let dir = tempfile::tempdir().unwrap();
    let run = |args: &str| {
        let ok = std::process::Command::new(ffmpeg)
            .args(["-hide_banner", "-loglevel", "error", "-y"])
            .args(args.split(' '))
            .current_dir(dir.path())
            .status()
            .unwrap()
            .success();
        assert!(ok, "ffmpeg {args}");
    };
    run(
        "-f lavfi -i testsrc=duration=6:size=160x120:rate=10 -f lavfi -i sine=duration=6 -c:v mpeg4 -c:a aac -shortest src.mp4",
    );
    run(
        "-i src.mp4 -c copy -bsf:v dump_extra -f hls -hls_time 1 -hls_playlist_type vod index.m3u8",
    );
    for entry in std::fs::read_dir(dir.path()).unwrap() {
        let entry = entry.unwrap();
        let name = entry.file_name().into_string().unwrap();
        server.add_file(
            &format!("v/{name}"),
            std::fs::read(entry.path()).unwrap(),
            None,
        );
    }
    "mp4"
}

fn best_quality() -> Option<StreamChoice> {
    Some(StreamChoice {
        quality: "best".into(),
        ..StreamChoice::default()
    })
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn downloads_a_video_stream() {
    let h = Harness::new();
    let (server, _) = start(Vec::new()).await;
    let ext = serve_video(&server, h.manager.ffmpeg().as_deref());
    // Slow enough for a few progress updates.
    server.per_conn_rate.store(60_000, Ordering::SeqCst);
    h.manager.start();
    let path = h.downloads.path().join("Video").join("Clip.mp4");
    let item = h
        .manager
        .add_download(NewDownload {
            url: server.url("v/index.m3u8"),
            path: Some(path.clone()),
            start: true,
            stream: best_quality(),
            ..NewDownload::default()
        })
        .unwrap();
    let done = h.completed(item.id).await;
    let saved = done.path.clone().unwrap();
    assert_eq!(saved.extension().unwrap(), ext);
    assert_eq!(
        std::fs::metadata(&saved).unwrap().len(),
        done.total.unwrap()
    );
    assert!(!stream_work_dir(&path).exists());
    let reported_pieces = h.events.lock().unwrap().iter().any(|e| {
        matches!(e, Event::Progress { items, .. }
            if items.iter().any(|l| l.stream.as_ref().is_some_and(|s| s.pieces > 0)))
    });
    assert!(reported_pieces, "progress shows the stream's pieces");
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn manifest_links_become_streams() {
    let h = Harness::new();
    let report = h.manager.add(
        &[
            "https://cdn.example/show/master.m3u8".to_string(),
            "https://cdn.example/show/movie.mp4".to_string(),
        ],
        &[],
        false,
    );
    assert_eq!(report.added[0].stream, Some(StreamChoice::default()));
    assert_eq!(report.added[1].stream, None);
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn removing_a_stream_deletes_its_pieces() {
    let h = Harness::new();
    let (server, _) = start(Vec::new()).await;
    serve_video(&server, None);
    server.per_conn_rate.store(40_000, Ordering::SeqCst);
    h.manager.start();
    let path = h.downloads.path().join("slow.mp4");
    let item = h
        .manager
        .add_download(NewDownload {
            url: server.url("v/index.m3u8"),
            path: Some(path.clone()),
            start: true,
            stream: best_quality(),
            ..NewDownload::default()
        })
        .unwrap();
    h.wait(item.id, |i| i.downloaded > 0).await;
    assert!(stream_work_dir(&path).is_dir());
    h.manager.remove(&[item.id], true).await;
    assert!(!stream_work_dir(&path).exists());
}

#[cfg(unix)]
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn installs_ffmpeg_from_a_zip() {
    use std::io::Write;
    use zip::write::SimpleFileOptions;

    // A stand-in for ffmpeg that answers -version, inside a folder as in
    // the real builds.
    let mut zip = zip::ZipWriter::new(std::io::Cursor::new(Vec::new()));
    zip.start_file("build/README.txt", SimpleFileOptions::default())
        .unwrap();
    zip.write_all(b"hello").unwrap();
    zip.start_file(
        "build/bin/ffmpeg",
        SimpleFileOptions::default().unix_permissions(0o644),
    )
    .unwrap();
    zip.write_all(b"#!/bin/sh\necho ffmpeg version test\n")
        .unwrap();
    let bytes = zip.finish().unwrap().into_inner();

    let h = Harness::new();
    let (server, _) = start(Vec::new()).await;
    server.add_file("tools/ffmpeg.zip", bytes, Some("application/zip"));
    h.manager
        .install_ffmpeg_from(&server.url("tools/ffmpeg.zip"))
        .unwrap();
    let deadline = Instant::now() + Duration::from_secs(20);
    while h.manager.ffmpeg_status().installing {
        assert!(Instant::now() < deadline, "still installing");
        tokio::time::sleep(Duration::from_millis(20)).await;
    }
    let status = h.manager.ffmpeg_status();
    assert_eq!(status.error, None);
    let installed = h.data_dir.path().join("ffmpeg").join("ffmpeg");
    assert_eq!(status.path.as_deref(), Some(installed.as_path()));
    assert!(
        !h.data_dir
            .path()
            .join("ffmpeg/ffmpeg-download.zip")
            .exists()
    );
    assert!(
        h.events
            .lock()
            .unwrap()
            .iter()
            .any(|e| matches!(e, Event::Ffmpeg { .. }))
    );

    // A broken download is reported, not installed.
    server.add_file("tools/broken.zip", b"not a zip".to_vec(), None);
    h.manager
        .install_ffmpeg_from(&server.url("tools/broken.zip"))
        .unwrap();
    while h.manager.ffmpeg_status().installing {
        tokio::time::sleep(Duration::from_millis(20)).await;
    }
    assert!(h.manager.ffmpeg_status().error.unwrap().contains("unpack"));
}
