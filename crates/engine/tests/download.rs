//! End-to-end tests against a local server that can misbehave on purpose.

use std::sync::atomic::{AtomicBool, AtomicU64, AtomicUsize, Ordering};
use std::sync::{Arc, RwLock};
use std::time::{Duration, Instant};

use axum::Router;
use axum::body::{Body, Bytes};
use axum::extract::State;
use axum::http::{HeaderMap, StatusCode, header};
use axum::response::{IntoResponse, Response};
use futures_util::stream;
use slipstream_engine::{
    ClientOptions, Download, Error, Options, Outcome, Output, RateLimiter, Request, build_client,
};
use tokio_util::sync::CancellationToken;

const MB: usize = 1 << 20;

#[derive(Default)]
struct Server {
    data: RwLock<(Arc<Vec<u8>>, String)>,
    no_ranges: AtomicBool,
    no_length: AtomicBool,
    /// Per-connection speed cap in bytes/s, to make parallelism matter.
    per_conn_rate: AtomicU64,
    /// Break this many data responses after `drop_after` bytes.
    drops_left: AtomicUsize,
    drop_after: AtomicUsize,
    requests: AtomicUsize,
    range_requests: AtomicUsize,
    bytes_sent: AtomicU64,
    in_flight: AtomicUsize,
    max_in_flight: AtomicUsize,
}

impl Server {
    fn set_data(&self, data: Vec<u8>, etag: &str) {
        *self.data.write().unwrap() = (Arc::new(data), format!("\"{etag}\""));
    }
}

struct InFlight(Arc<Server>);
impl Drop for InFlight {
    fn drop(&mut self) {
        self.0.in_flight.fetch_sub(1, Ordering::SeqCst);
    }
}

async fn serve(State(s): State<Arc<Server>>, headers: HeaderMap) -> Response {
    let (data, etag) = s.data.read().unwrap().clone();
    s.requests.fetch_add(1, Ordering::SeqCst);
    let len = data.len();

    let if_range_ok = headers
        .get(header::IF_RANGE)
        .is_none_or(|v| v.to_str().unwrap() == etag);
    let range = headers
        .get(header::RANGE)
        .filter(|_| !s.no_ranges.load(Ordering::SeqCst) && if_range_ok)
        .and_then(|v| {
            v.to_str()
                .ok()?
                .strip_prefix("bytes=")?
                .split_once('-')
                .map(|(a, b)| (a.to_string(), b.to_string()))
        })
        .map(|(a, b)| (a.parse::<usize>().unwrap(), b.parse::<usize>().ok()));

    let mut resp = Response::builder().header(header::ETAG, &etag);
    let (from, to) = match range {
        Some((a, _)) if a >= len => {
            return resp
                .status(StatusCode::RANGE_NOT_SATISFIABLE)
                .header(header::CONTENT_RANGE, format!("bytes */{len}"))
                .body(Body::empty())
                .unwrap();
        }
        Some((a, b)) => {
            let end = b.unwrap_or(len - 1).min(len - 1);
            s.range_requests.fetch_add(1, Ordering::SeqCst);
            resp = resp
                .status(StatusCode::PARTIAL_CONTENT)
                .header(header::CONTENT_RANGE, format!("bytes {a}-{end}/{len}"));
            (a, end + 1)
        }
        None => (0, len),
    };
    if !s.no_length.load(Ordering::SeqCst) {
        resp = resp.header(header::CONTENT_LENGTH, to - from);
    }

    // Only real data requests misbehave, not the one-byte probe.
    let drop_at = (to - from > 1
        && s.drops_left
            .fetch_update(Ordering::SeqCst, Ordering::SeqCst, |n| n.checked_sub(1))
            .is_ok())
    .then(|| from + s.drop_after.load(Ordering::SeqCst));
    let rate = s.per_conn_rate.load(Ordering::SeqCst);
    let current = s.in_flight.fetch_add(1, Ordering::SeqCst) + 1;
    s.max_in_flight.fetch_max(current, Ordering::SeqCst);
    let guard = InFlight(s.clone());

    let body = stream::unfold((from, guard), move |(pos, guard)| {
        let data = data.clone();
        async move {
            if pos >= to {
                return None;
            }
            if drop_at.is_some_and(|d| pos >= d) {
                return Some((Err(std::io::Error::other("simulated drop")), (to, guard)));
            }
            let end = (pos + 16 * 1024).min(to);
            if rate > 0 {
                tokio::time::sleep(Duration::from_secs_f64((end - pos) as f64 / rate as f64)).await;
            }
            guard
                .0
                .bytes_sent
                .fetch_add((end - pos) as u64, Ordering::SeqCst);
            Some((Ok(Bytes::copy_from_slice(&data[pos..end])), (end, guard)))
        }
    });
    resp.body(Body::from_stream(body)).unwrap()
}

async fn start(data: Vec<u8>) -> (Arc<Server>, String) {
    let server = Arc::new(Server::default());
    server.set_data(data, "v1");
    let app = Router::new()
        .route(
            "/missing",
            axum::routing::get(|| async { StatusCode::NOT_FOUND.into_response() }),
        )
        .fallback(serve)
        .with_state(server.clone());
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
    tokio::spawn(async move { axum::serve(listener, app).await.unwrap() });
    (server, format!("http://{addr}/files/test.bin"))
}

fn data(len: usize, seed: u64) -> Vec<u8> {
    let mut x = seed | 1;
    (0..len)
        .map(|_| {
            x ^= x << 13;
            x ^= x >> 7;
            x ^= x << 17;
            x as u8
        })
        .collect()
}

fn opts(connections: usize) -> Options {
    Options {
        connections,
        ..Options::default()
    }
}

async fn prepare(
    url: &str,
    dir: &std::path::Path,
    options: Options,
) -> slipstream_engine::Result<Download> {
    let client = build_client(&ClientOptions::default())?;
    let request = Request {
        url: url.to_string(),
        headers: vec![],
        output: Output::Dir(dir.to_path_buf()),
    };
    Download::prepare(client, request, options).await
}

async fn download(
    url: &str,
    dir: &std::path::Path,
    options: Options,
) -> slipstream_engine::Result<Outcome> {
    prepare(url, dir, options)
        .await?
        .run(CancellationToken::new())
        .await
}

/// Starts a download and pauses it once at least `after` bytes have arrived.
async fn pause_after(url: &str, dir: &std::path::Path, options: Options, after: u64) -> Outcome {
    let dl = prepare(url, dir, options).await.unwrap();
    let progress = dl.progress();
    let cancel = CancellationToken::new();
    let watcher = tokio::spawn({
        let cancel = cancel.clone();
        async move {
            while progress.snapshot().downloaded < after {
                tokio::time::sleep(Duration::from_millis(10)).await;
            }
            cancel.cancel();
        }
    });
    let outcome = dl.run(cancel).await.unwrap();
    watcher.abort();
    outcome
}

fn read(dir: &std::path::Path, name: &str) -> Vec<u8> {
    std::fs::read(dir.join(name)).unwrap()
}

fn leftovers(dir: &std::path::Path) -> Vec<String> {
    std::fs::read_dir(dir)
        .unwrap()
        .map(|e| e.unwrap().file_name().to_string_lossy().into_owned())
        .filter(|n| n.ends_with(".part") || n.ends_with(".slipstream") || n.ends_with(".tmp"))
        .collect()
}

#[tokio::test]
async fn downloads_with_parallel_connections() {
    let content = data(8 * MB, 1);
    let (server, url) = start(content.clone()).await;
    server.per_conn_rate.store(4 * MB as u64, Ordering::SeqCst);
    let dir = tempfile::tempdir().unwrap();

    let outcome = download(&url, dir.path(), opts(8)).await.unwrap();

    assert_eq!(outcome, Outcome::Completed(dir.path().join("test.bin")));
    assert!(read(dir.path(), "test.bin") == content, "content differs");
    assert!(
        server.max_in_flight.load(Ordering::SeqCst) >= 4,
        "connections did not run in parallel"
    );
    assert!(leftovers(dir.path()).is_empty());
}

#[tokio::test]
async fn free_connections_split_remaining_work() {
    let content = data(8 * MB, 2);
    let (server, url) = start(content.clone()).await;
    server.per_conn_rate.store(2 * MB as u64, Ordering::SeqCst);
    let dir = tempfile::tempdir().unwrap();

    download(&url, dir.path(), opts(3)).await.unwrap();

    assert!(read(dir.path(), "test.bin") == content);
    // The first split gives uneven pieces (4, 2 and 2 MB); connections that
    // finish early must take over half of what is left.
    let ranges = server.range_requests.load(Ordering::SeqCst) - 1; // minus the probe
    assert!(
        ranges > 3,
        "expected dynamic re-splitting, saw {ranges} range requests"
    );
}

#[tokio::test]
async fn pauses_and_resumes_without_refetching() {
    let content = data(8 * MB, 3);
    let (server, url) = start(content.clone()).await;
    server.per_conn_rate.store(MB as u64, Ordering::SeqCst);
    let dir = tempfile::tempdir().unwrap();

    assert_eq!(
        pause_after(&url, dir.path(), opts(4), 3 * MB as u64).await,
        Outcome::Paused
    );
    assert_eq!(
        leftovers(dir.path()).len(),
        2,
        "expected .part and .slipstream files"
    );

    server.per_conn_rate.store(0, Ordering::SeqCst);
    let sent_before = server.bytes_sent.load(Ordering::SeqCst);
    let dl = prepare(&url, dir.path(), opts(4)).await.unwrap();
    assert!(
        dl.info().resumed_bytes >= 3 * MB as u64,
        "resumed {}",
        dl.info().resumed_bytes
    );
    assert_eq!(
        dl.info().path,
        dir.path().join("test.bin"),
        "should resume into the same file"
    );
    dl.run(CancellationToken::new()).await.unwrap();

    assert!(read(dir.path(), "test.bin") == content);
    let refetched = server.bytes_sent.load(Ordering::SeqCst) - sent_before;
    assert!(
        refetched <= (5 * MB) as u64 + 1,
        "second run fetched {refetched} bytes"
    );
    assert!(leftovers(dir.path()).is_empty());
}

#[tokio::test]
async fn restarts_when_file_changed_between_runs() {
    let (server, url) = start(data(8 * MB, 4)).await;
    server.per_conn_rate.store(MB as u64, Ordering::SeqCst);
    let dir = tempfile::tempdir().unwrap();
    pause_after(&url, dir.path(), opts(4), 2 * MB as u64).await;

    let new_content = data(8 * MB, 5);
    server.set_data(new_content.clone(), "v2");
    server.per_conn_rate.store(0, Ordering::SeqCst);
    let dl = prepare(&url, dir.path(), opts(4)).await.unwrap();
    assert!(dl.info().restarted);
    assert_eq!(dl.info().resumed_bytes, 0);
    dl.run(CancellationToken::new()).await.unwrap();

    assert!(read(dir.path(), "test.bin") == new_content);
}

#[tokio::test]
async fn stops_when_file_changes_mid_download() {
    let (server, url) = start(data(8 * MB, 6)).await;
    server.per_conn_rate.store(MB as u64, Ordering::SeqCst);
    let dir = tempfile::tempdir().unwrap();
    // Three connections split unevenly, so new range requests (carrying
    // If-Range) are made after the swap; responses already in flight keep
    // serving the old version consistently.
    let dl = prepare(&url, dir.path(), opts(3)).await.unwrap();
    let progress = dl.progress();
    let swapper = tokio::spawn({
        let server = server.clone();
        async move {
            while progress.snapshot().downloaded < MB as u64 {
                tokio::time::sleep(Duration::from_millis(10)).await;
            }
            server.set_data(data(8 * MB, 7), "v2");
        }
    });

    let err = dl.run(CancellationToken::new()).await.unwrap_err();
    swapper.await.unwrap();
    assert!(matches!(err, Error::RemoteChanged), "{err:?}");
}

#[tokio::test]
async fn retries_dropped_connections() {
    let content = data(4 * MB, 8);
    let (server, url) = start(content.clone()).await;
    server.drops_left.store(3, Ordering::SeqCst);
    server.drop_after.store(256 * 1024, Ordering::SeqCst);
    let dir = tempfile::tempdir().unwrap();

    download(&url, dir.path(), opts(4)).await.unwrap();

    assert_eq!(
        server.drops_left.load(Ordering::SeqCst),
        0,
        "drops were not exercised"
    );
    assert!(read(dir.path(), "test.bin") == content);
}

#[tokio::test]
async fn falls_back_to_one_connection_without_range_support() {
    let content = data(3 * MB, 9);
    let (server, url) = start(content.clone()).await;
    server.no_ranges.store(true, Ordering::SeqCst);
    let dir = tempfile::tempdir().unwrap();

    let dl = prepare(&url, dir.path(), opts(8)).await.unwrap();
    assert!(!dl.info().resumable);
    dl.run(CancellationToken::new()).await.unwrap();

    assert!(read(dir.path(), "test.bin") == content);
    assert_eq!(
        server.requests.load(Ordering::SeqCst),
        2,
        "probe plus one download"
    );
}

#[tokio::test]
async fn handles_unknown_length() {
    let content = data(MB + 123, 10);
    let (server, url) = start(content.clone()).await;
    server.no_ranges.store(true, Ordering::SeqCst);
    server.no_length.store(true, Ordering::SeqCst);
    let dir = tempfile::tempdir().unwrap();

    let dl = prepare(&url, dir.path(), opts(8)).await.unwrap();
    assert_eq!(dl.info().total, None);
    dl.run(CancellationToken::new()).await.unwrap();

    assert!(read(dir.path(), "test.bin") == content);
}

#[tokio::test]
async fn respects_speed_limit() {
    let content = data(2 * MB, 11);
    let (_server, url) = start(content.clone()).await;
    let dir = tempfile::tempdir().unwrap();
    let options = Options {
        limiter: Arc::new(RateLimiter::new(MB as u64)),
        ..opts(4)
    };

    let started = Instant::now();
    download(&url, dir.path(), options).await.unwrap();

    let elapsed = started.elapsed();
    assert!(
        elapsed >= Duration::from_millis(1700),
        "2 MB at 1 MB/s took only {elapsed:?}"
    );
    assert!(read(dir.path(), "test.bin") == content);
}

#[tokio::test]
async fn picks_a_free_name_in_the_folder() {
    let (_server, url) = start(data(1000, 12)).await;
    let dir = tempfile::tempdir().unwrap();

    download(&url, dir.path(), opts(2)).await.unwrap();
    let second = download(&url, dir.path(), opts(2)).await.unwrap();

    assert_eq!(second, Outcome::Completed(dir.path().join("test (1).bin")));
}

#[tokio::test]
async fn downloads_empty_file() {
    let (_server, url) = start(Vec::new()).await;
    let dir = tempfile::tempdir().unwrap();

    download(&url, dir.path(), opts(4)).await.unwrap();

    assert!(read(dir.path(), "test.bin").is_empty());
}

#[tokio::test]
async fn reports_missing_file() {
    let (_server, url) = start(data(10, 13)).await;
    let url = url.replace("/files/test.bin", "/missing");
    let dir = tempfile::tempdir().unwrap();

    let err = download(&url, dir.path(), opts(4)).await.unwrap_err();

    assert!(matches!(err, Error::Status(s) if s == 404), "{err:?}");
}
