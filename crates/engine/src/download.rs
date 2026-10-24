//! Running one download: probing, resuming, and driving the connections.

use std::fs::{self, File, OpenOptions};
use std::mem;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, AtomicUsize, Ordering};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use futures_util::StreamExt;
use reqwest::header::{CONTENT_RANGE, IF_RANGE, RANGE};
use reqwest::{Client, StatusCode};
use tokio::task::JoinSet;
use tokio_util::sync::CancellationToken;

use crate::io::write_all_at;
use crate::limiter::RateLimiter;
use crate::probe::{RemoteInfo, parse_content_range, probe};
use crate::segments::{Assignment, Receive, SegmentMap, SegmentView, WorkerId};
use crate::state::{State, part_path, state_path};
use crate::{Error, Result, apply_headers};

/// Bytes each connection collects before writing them to disk.
const WRITE_BUFFER: usize = 512 * 1024;
/// How often progress is made durable (IDM: "several times per minute").
const SAVE_EVERY: Duration = Duration::from_secs(5);

#[derive(Debug, Clone)]
pub struct Request {
    pub url: String,
    /// Extra headers such as `Cookie`, `Referer` or `User-Agent`.
    pub headers: Vec<(String, String)>,
    pub output: Output,
}

#[derive(Debug, Clone)]
pub enum Output {
    /// Save into this folder under the server's file name, picking
    /// `name (1).ext` if that is taken.
    Dir(PathBuf),
    /// Save to exactly this path, replacing any existing file.
    File(PathBuf),
}

#[derive(Debug, Clone)]
pub struct Options {
    /// Maximum parallel connections (IDM's default is 8).
    pub connections: usize,
    /// Never split a segment into pieces smaller than this.
    pub min_split_size: u64,
    /// Consecutive failures a connection tolerates before giving up.
    pub max_retries: u32,
    /// Speed limits this download obeys, e.g. one shared by every download
    /// plus one of its own. Empty means unlimited.
    pub limiters: Vec<Arc<RateLimiter>>,
}

impl Default for Options {
    fn default() -> Self {
        Self {
            connections: 8,
            min_split_size: 1 << 20,
            max_retries: 8,
            limiters: Vec::new(),
        }
    }
}

#[derive(Debug, Clone)]
pub struct DownloadInfo {
    /// Where the finished file will be.
    pub path: PathBuf,
    pub total: Option<u64>,
    /// Whether the server allows pausing and multiple connections.
    pub resumable: bool,
    /// Bytes already on disk from an earlier run.
    pub resumed_bytes: u64,
    /// An earlier partial download existed but the file changed on the
    /// server, so it was discarded.
    pub restarted: bool,
    pub remote: RemoteInfo,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Outcome {
    Completed(PathBuf),
    /// Cancelled; run the same request again to continue (if resumable).
    Paused,
}

#[derive(Debug)]
struct Shared {
    total: Option<u64>,
    downloaded: AtomicU64,
    connections: AtomicUsize,
    map: Mutex<SegmentMap>,
}

/// A cheap handle for watching a download from another task.
#[derive(Debug, Clone)]
pub struct Progress(Arc<Shared>);

#[derive(Debug, Clone)]
pub struct ProgressSnapshot {
    pub downloaded: u64,
    pub total: Option<u64>,
    pub connections: usize,
    pub segments: Vec<SegmentView>,
}

impl Progress {
    pub fn snapshot(&self) -> ProgressSnapshot {
        ProgressSnapshot {
            downloaded: self.0.downloaded.load(Ordering::Relaxed),
            total: self.0.total,
            connections: self.0.connections.load(Ordering::Relaxed),
            segments: self.0.map.lock().unwrap().view(),
        }
    }
}

pub struct Download {
    client: Client,
    request: Request,
    opts: Options,
    info: DownloadInfo,
    shared: Arc<Shared>,
}

impl Download {
    /// Probes the server, picks the destination and loads any resume state.
    pub async fn prepare(client: Client, request: Request, opts: Options) -> Result<Self> {
        let remote = probe(&client, &request.url, &request.headers).await?;
        Self::prepare_probed(client, request, opts, remote).await
    }

    /// Like [`Download::prepare`], for callers that already probed the link
    /// (for example to choose a folder from the file name).
    pub async fn prepare_probed(
        client: Client,
        request: Request,
        opts: Options,
        remote: RemoteInfo,
    ) -> Result<Self> {
        let resumable = remote.ranges && remote.total.is_some();

        let (path, saved) = match &request.output {
            Output::File(path) => (path.clone(), State::load(&state_path(path))),
            Output::Dir(dir) => pick_path(&dir.join(&remote.file_name), &request.url, &remote),
        };

        let mut restarted = false;
        let mut map = SegmentMap::new(remote.total.unwrap_or(0), opts.min_split_size);
        if let Some(state) = saved {
            let usable = resumable
                && state.same_version(&remote)
                && SegmentMap::validate_saved(&state.segments, state.total)
                && part_path(&path).is_file();
            if usable {
                map = SegmentMap::from_saved(&state.segments, opts.min_split_size);
            } else {
                restarted = true;
            }
        }

        let resumed_bytes = map.written_bytes();
        let info = DownloadInfo {
            path,
            total: remote.total,
            resumable,
            resumed_bytes,
            restarted,
            remote,
        };
        let shared = Arc::new(Shared {
            total: info.total,
            downloaded: AtomicU64::new(resumed_bytes),
            connections: AtomicUsize::new(0),
            map: Mutex::new(map),
        });
        Ok(Self {
            client,
            request,
            opts,
            info,
            shared,
        })
    }

    pub fn info(&self) -> &DownloadInfo {
        &self.info
    }

    pub fn progress(&self) -> Progress {
        Progress(self.shared.clone())
    }

    /// Downloads until finished, failed, or `cancel` fires (which pauses).
    pub async fn run(self, cancel: CancellationToken) -> Result<Outcome> {
        if let Some(dir) = self
            .info
            .path
            .parent()
            .filter(|d| !d.as_os_str().is_empty())
        {
            fs::create_dir_all(dir)?;
        }
        if self.info.total == Some(0) {
            File::create(&self.info.path)?;
            return Ok(Outcome::Completed(self.info.path));
        }
        if self.info.resumable {
            self.run_segmented(cancel).await
        } else {
            self.run_single(cancel).await
        }
    }

    async fn run_segmented(self, cancel: CancellationToken) -> Result<Outcome> {
        let total = self.info.total.expect("resumable downloads have a size");
        let part = part_path(&self.info.path);
        let state_file = state_path(&self.info.path);
        let file = OpenOptions::new()
            .read(true)
            .write(true)
            .create(true)
            .truncate(false)
            .open(&part)?;
        if self.info.resumed_bytes == 0 {
            file.set_len(0)?;
        }
        if file.metadata()?.len() != total {
            file.set_len(total)?;
        }

        let ctx = Arc::new(Ctx {
            client: self.client,
            url: self.info.remote.final_url.clone(),
            headers: self.request.headers,
            if_range: self.info.remote.if_range().map(str::to_owned),
            total,
            file: Arc::new(file),
            shared: self.shared,
            limiters: self.opts.limiters,
            max_retries: self.opts.max_retries,
            stop: cancel.child_token(),
            fatal: Mutex::new(None),
            state: State::new(&self.request.url, &self.info.remote, total, Vec::new()),
            state_file,
        });
        ctx.save_state().await?;

        let saver = tokio::spawn({
            let ctx = ctx.clone();
            async move {
                loop {
                    tokio::select! {
                        _ = ctx.stop.cancelled() => break,
                        _ = tokio::time::sleep(SAVE_EVERY) => {}
                    }
                    let _ = ctx.save_state().await;
                }
            }
        });
        let mut workers = JoinSet::new();
        for id in 0..self.opts.connections.clamp(1, 32) {
            workers.spawn(worker(ctx.clone(), id));
        }
        while workers.join_next().await.is_some() {}
        saver.abort();
        let _ = saver.await;

        let complete = ctx.shared.map.lock().unwrap().is_complete();
        if complete {
            ctx.file.sync_all()?;
            let state_file = ctx.state_file.clone();
            // Every clone of the file handle must be closed before renaming
            // (Windows refuses to rename open files).
            drop(Arc::into_inner(ctx).map(|ctx| ctx.file));
            fs::rename(&part, &self.info.path)?;
            let _ = fs::remove_file(state_file);
            return Ok(Outcome::Completed(self.info.path));
        }

        ctx.save_state().await?;
        if let Some(err) = ctx.fatal.lock().unwrap().take() {
            return Err(err);
        }
        if cancel.is_cancelled() {
            Ok(Outcome::Paused)
        } else {
            Err(Error::Incomplete)
        }
    }

    /// For servers without range support or a known size: one connection,
    /// and a retry starts over from the beginning.
    async fn run_single(self, cancel: CancellationToken) -> Result<Outcome> {
        let part = part_path(&self.info.path);
        let mut failures = 0;
        loop {
            let file = Arc::new(File::create(&part)?);
            self.shared.downloaded.store(0, Ordering::Relaxed);
            self.shared.connections.store(1, Ordering::Relaxed);
            let result = self.stream_whole(&file, &cancel).await;
            self.shared.connections.store(0, Ordering::Relaxed);
            match result {
                Ok(()) => {
                    file.sync_all()?;
                    drop(file);
                    fs::rename(&part, &self.info.path)?;
                    return Ok(Outcome::Completed(self.info.path));
                }
                Err(_) if cancel.is_cancelled() => return Ok(Outcome::Paused),
                Err(e) if e.is_fatal() => return Err(e),
                Err(e) => {
                    failures += 1;
                    if failures > self.opts.max_retries {
                        return Err(e);
                    }
                    tokio::select! {
                        _ = cancel.cancelled() => return Ok(Outcome::Paused),
                        _ = tokio::time::sleep(backoff(failures)) => {}
                    }
                }
            }
        }
    }

    async fn stream_whole(&self, file: &Arc<File>, cancel: &CancellationToken) -> Result<()> {
        let req = apply_headers(
            self.client.get(&self.info.remote.final_url),
            &self.request.headers,
        );
        let resp = tokio::select! {
            _ = cancel.cancelled() => return Err(Error::Cancelled),
            r = req.send() => r?,
        };
        if !resp.status().is_success() {
            return Err(Error::Status(resp.status()));
        }
        let expected = resp.content_length().or(self.info.total);
        let mut stream = resp.bytes_stream();
        let mut buf = Vec::with_capacity(WRITE_BUFFER);
        let mut offset = 0;
        loop {
            let next = tokio::select! {
                _ = cancel.cancelled() => return Err(Error::Cancelled),
                n = stream.next() => n,
            };
            match next {
                Some(Ok(chunk)) => {
                    throttle(&self.opts.limiters, chunk.len()).await;
                    buf.extend_from_slice(&chunk);
                    self.shared
                        .downloaded
                        .fetch_add(chunk.len() as u64, Ordering::Relaxed);
                    if buf.len() >= WRITE_BUFFER {
                        let len = buf.len() as u64;
                        buf = write_blocking(file, buf, offset).await?;
                        buf.clear();
                        offset += len;
                    }
                }
                Some(Err(e)) => return Err(e.into()),
                None => break,
            }
        }
        let len = buf.len() as u64;
        write_blocking(file, buf, offset).await?;
        offset += len;
        match expected {
            Some(n) if n != offset => Err(Error::BadResponse(format!(
                "expected {n} bytes, got {offset}"
            ))),
            _ => Ok(()),
        }
    }
}

/// Everything the connections of one download share.
struct Ctx {
    client: Client,
    url: String,
    headers: Vec<(String, String)>,
    if_range: Option<String>,
    total: u64,
    file: Arc<File>,
    shared: Arc<Shared>,
    limiters: Vec<Arc<RateLimiter>>,
    max_retries: u32,
    /// Fires on pause or on a fatal error.
    stop: CancellationToken,
    fatal: Mutex<Option<Error>>,
    state: State,
    state_file: PathBuf,
}

impl Ctx {
    fn fail(&self, err: Error) {
        self.fatal.lock().unwrap().get_or_insert(err);
        self.stop.cancel();
    }

    /// Flushes data to disk before recording positions, so the resume file
    /// never claims bytes that a power cut could lose.
    async fn save_state(&self) -> Result<()> {
        let segments = self.shared.map.lock().unwrap().saved();
        let state = State {
            segments,
            ..self.state.clone()
        };
        let file = self.file.clone();
        let path = self.state_file.clone();
        tokio::task::spawn_blocking(move || {
            file.sync_data()?;
            state.save(&path)
        })
        .await
        .map_err(std::io::Error::other)??;
        Ok(())
    }
}

async fn worker(ctx: Arc<Ctx>, id: WorkerId) {
    let mut failures = 0;
    loop {
        if ctx.stop.is_cancelled() {
            return;
        }
        let Some(assignment) = ctx.shared.map.lock().unwrap().claim(id) else {
            return;
        };
        ctx.shared.connections.fetch_add(1, Ordering::Relaxed);
        let result = fetch(&ctx, id, assignment).await;
        ctx.shared.connections.fetch_sub(1, Ordering::Relaxed);
        ctx.shared.map.lock().unwrap().release(id, assignment.id);

        match result {
            Ok(()) => failures = 0,
            Err(_) if ctx.stop.is_cancelled() => return,
            Err(e) if e.is_fatal() => return ctx.fail(e),
            Err(_) => {
                failures += 1;
                if failures > ctx.max_retries {
                    return;
                }
                tokio::select! {
                    _ = ctx.stop.cancelled() => return,
                    _ = tokio::time::sleep(backoff(failures)) => {}
                }
            }
        }
    }
}

async fn throttle(limiters: &[Arc<RateLimiter>], bytes: usize) {
    for limiter in limiters {
        limiter.acquire(bytes).await;
    }
}

fn backoff(failures: u32) -> Duration {
    Duration::from_millis(500 << failures.min(6))
}

/// Downloads one assignment over one HTTP request.
async fn fetch(ctx: &Ctx, worker: WorkerId, a: Assignment) -> Result<()> {
    let mut req = apply_headers(ctx.client.get(&ctx.url), &ctx.headers)
        .header(RANGE, format!("bytes={}-{}", a.from, a.to - 1));
    if let Some(validator) = &ctx.if_range {
        req = req.header(IF_RANGE, validator);
    }
    let resp = tokio::select! {
        _ = ctx.stop.cancelled() => return Err(Error::Cancelled),
        r = req.send() => r?,
    };
    match resp.status() {
        StatusCode::PARTIAL_CONTENT => {}
        // With If-Range, a full 200 response means the file changed.
        StatusCode::OK if ctx.if_range.is_some() => return Err(Error::RemoteChanged),
        StatusCode::OK => return Err(Error::BadResponse("server ignored the Range header".into())),
        StatusCode::RANGE_NOT_SATISFIABLE => return Err(Error::RemoteChanged),
        s => return Err(Error::Status(s)),
    }
    let range = resp
        .headers()
        .get(CONTENT_RANGE)
        .and_then(|v| v.to_str().ok())
        .and_then(parse_content_range);
    let request_end = match range {
        Some(r) if r.total.is_some_and(|t| t != ctx.total) => return Err(Error::RemoteChanged),
        Some(r) if r.start == Some(a.from) => r.end.map_or(a.to, |e| (e + 1).min(a.to)),
        _ => {
            return Err(Error::BadResponse(format!(
                "wrong Content-Range for bytes {}-",
                a.from
            )));
        }
    };

    let mut stream = resp.bytes_stream();
    let mut buf = Vec::with_capacity(WRITE_BUFFER);
    let mut buf_offset = a.from;
    let mut pos = a.from;
    loop {
        let next = tokio::select! {
            _ = ctx.stop.cancelled() => None,
            n = stream.next() => Some(n),
        };
        let chunk = match next {
            None => {
                flush(ctx, a.id, &mut buf, &mut buf_offset).await?;
                return Err(Error::Cancelled);
            }
            Some(Some(Ok(chunk))) => chunk,
            Some(Some(Err(e))) => {
                flush(ctx, a.id, &mut buf, &mut buf_offset).await?;
                return Err(e.into());
            }
            Some(None) => {
                flush(ctx, a.id, &mut buf, &mut buf_offset).await?;
                // Some servers cap how much one request returns. If we got
                // everything promised, the rest is simply claimed again.
                return if pos >= request_end {
                    Ok(())
                } else {
                    Err(Error::BadResponse("connection closed early".into()))
                };
            }
        };

        throttle(&ctx.limiters, chunk.len()).await;
        let action = ctx.shared.map.lock().unwrap().on_receive(
            worker,
            a.id,
            chunk.len() as u64,
            request_end,
        );
        let Receive::Keep { keep, done } = action else {
            // A neighbour absorbed our segment before we received a byte.
            return Ok(());
        };
        buf.extend_from_slice(&chunk[..keep as usize]);
        pos += keep;
        ctx.shared.downloaded.fetch_add(keep, Ordering::Relaxed);
        if done || buf.len() >= WRITE_BUFFER {
            flush(ctx, a.id, &mut buf, &mut buf_offset).await?;
        }
        if done {
            if pos == request_end {
                // Read the end of the response so the connection can be
                // reused for the next request without a new handshake.
                let _ = tokio::time::timeout(Duration::from_secs(1), stream.next()).await;
            }
            return Ok(());
        }
    }
}

async fn flush(ctx: &Ctx, segment: u64, buf: &mut Vec<u8>, offset: &mut u64) -> Result<()> {
    if buf.is_empty() {
        return Ok(());
    }
    let data = write_blocking(&ctx.file, mem::take(buf), *offset).await?;
    *offset += data.len() as u64;
    ctx.shared.map.lock().unwrap().on_written(segment, *offset);
    *buf = data;
    buf.clear();
    Ok(())
}

/// Writes on the blocking pool and hands the buffer back for reuse.
async fn write_blocking(file: &Arc<File>, data: Vec<u8>, offset: u64) -> Result<Vec<u8>> {
    let file = file.clone();
    let data =
        tokio::task::spawn_blocking(move || write_all_at(&file, &data, offset).map(|()| data))
            .await
            .map_err(std::io::Error::other)??;
    Ok(data)
}

/// Picks `name`, `name (1)`, ... in a folder, reusing one whose partial
/// download came from the same link so it resumes. A new name is reserved by
/// creating its `.part` file at once, so two downloads of the same file
/// started together never pick the same name.
fn pick_path(base: &Path, url: &str, remote: &RemoteInfo) -> (PathBuf, Option<State>) {
    for n in 0.. {
        let candidate = numbered(base, n);
        match State::load(&state_path(&candidate)) {
            Some(state) if state.same_source(url, remote) => return (candidate, Some(state)),
            Some(_) => continue,
            None if !candidate.exists() && reserve(&part_path(&candidate)) => {
                return (candidate, None);
            }
            None => continue,
        }
    }
    unreachable!()
}

fn reserve(part: &Path) -> bool {
    if let Some(dir) = part.parent().filter(|d| !d.as_os_str().is_empty()) {
        let _ = fs::create_dir_all(dir);
    }
    OpenOptions::new()
        .write(true)
        .create_new(true)
        .open(part)
        .is_ok()
}

fn numbered(base: &Path, n: u32) -> PathBuf {
    if n == 0 {
        return base.to_path_buf();
    }
    let name = base
        .file_name()
        .and_then(|s| s.to_str())
        .unwrap_or("download");
    let (stem, ext) = match name.rfind('.') {
        Some(dot) if dot > 0 => (&name[..dot], &name[dot..]),
        _ => (name, ""),
    };
    base.with_file_name(format!("{stem} ({n}){ext}"))
}
