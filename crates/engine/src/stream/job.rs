//! Downloading a stream: its pieces over several connections, each saved as
//! it arrives and then appended to its track in order. A pause or a crash
//! loses nothing but the pieces in flight, and those continue where they
//! stopped.

use std::collections::{HashMap, HashSet, VecDeque};
use std::fs::{self, File, OpenOptions};
use std::io;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::{Arc, Mutex};

use aes::cipher::block_padding::Pkcs7;
use aes::cipher::{BlockModeDecrypt, KeyIvInit};
use futures_util::StreamExt;
use reqwest::header::{CONTENT_RANGE, RANGE};
use reqwest::{Client, StatusCode, Url};
use serde::{Deserialize, Serialize};
use tokio::io::{AsyncWriteExt, BufWriter};
use tokio::task::JoinSet;
use tokio_util::sync::CancellationToken;

use super::{PieceSpec, TrackSpec, VariantKey, mux, resolve};
use crate::download::{Options, Outcome, Progress, ProgressSnapshot, Watch, backoff, throttle};
use crate::limiter::RateLimiter;
use crate::probe::{parse_content_range, probe};
use crate::{Error, Result, apply_headers};

/// A video that is one whole file is fetched in pieces of this size, so
/// several connections can share it.
const CHUNK: u64 = 8 << 20;
const STATE_FILE: &str = "state.json";
const WRITE_BUFFER: usize = 256 << 10;

#[derive(Debug, Clone)]
pub struct StreamRequest {
    /// The manifest: an `.m3u8` playlist or an `.mpd`.
    pub url: String,
    pub headers: Vec<(String, String)>,
    pub variant: VariantKey,
    /// The finished video. Its extension decides the format ffmpeg writes.
    pub path: PathBuf,
    /// Where ffmpeg may be, which joins picture and sound and turns MPEG-TS
    /// into MP4. The first that exists when the download reaches that
    /// point is used, so one installed meanwhile counts. Without ffmpeg, a
    /// stream with a single track is kept as it came (a `.ts`, say).
    pub ffmpeg: Vec<PathBuf>,
}

#[derive(Debug, Clone)]
pub struct StreamDownloadInfo {
    pub path: PathBuf,
    pub pieces: u64,
    /// Bytes kept from an earlier run.
    pub resumed_bytes: u64,
    /// Known only when the manifest lists every piece's size.
    pub total: Option<u64>,
}

/// Live numbers of a stream download.
#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct StreamProgress {
    pub pieces: u64,
    pub done: u64,
    /// Finished pieces as `[first, end)` runs, for the segment map.
    pub runs: Vec<[u64; 2]>,
    /// The piece each busy connection is on.
    pub workers: Vec<PieceWork>,
    /// Joining the tracks into the finished video.
    pub joining: bool,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct PieceWork {
    pub piece: u64,
    pub received: u64,
}

/// The folder a stream download keeps its pieces in until it finishes.
pub fn stream_work_dir(path: &Path) -> PathBuf {
    let mut name = path.file_name().unwrap_or_default().to_os_string();
    name.push(".parts");
    path.with_file_name(name)
}

pub struct StreamDownload {
    client: Client,
    request: StreamRequest,
    opts: Options,
    tracks: Arc<Vec<TrackSpec>>,
    work: PathBuf,
    signature: String,
    committed: Vec<Committed>,
    info: StreamDownloadInfo,
    shared: Arc<Shared>,
}

impl StreamDownload {
    /// Reads the manifest, finds the chosen quality and picks up any
    /// earlier progress.
    pub async fn prepare(client: Client, request: StreamRequest, opts: Options) -> Result<Self> {
        let mut tracks = resolve(&client, &request.url, &request.headers, &request.variant).await?;
        for track in &mut tracks {
            if track.single_file {
                split(&client, &request.headers, track).await?;
            }
        }
        let work = stream_work_dir(&request.path);
        let signature = signature(&request.variant, &tracks);
        let counts: Vec<usize> = tracks.iter().map(|t| t.pieces.len()).collect();
        let restored = tokio::task::spawn_blocking({
            let (work, signature) = (work.clone(), signature.clone());
            move || restore(&work, &signature, &counts)
        })
        .await
        .map_err(io::Error::other)??;

        let pieces = restored.done.len() as u64;
        let exact_total = tracks
            .iter()
            .flat_map(|t| &t.pieces)
            .map(|p| p.range.map(|r| r.1))
            .sum::<Option<u64>>();
        let done_count = restored.done.iter().filter(|d| **d).count() as u64;
        let mut first = 0;
        let per_track: Vec<TrackDone> = tracks
            .iter()
            .zip(&restored.track_bytes)
            .map(|(track, &bytes)| {
                let done = &restored.done[first..first + track.pieces.len()];
                first += track.pieces.len();
                TrackDone {
                    bytes,
                    seconds: track
                        .pieces
                        .iter()
                        .zip(done)
                        .filter(|(_, d)| **d)
                        .map(|(p, _)| p.seconds)
                        .sum(),
                    total_seconds: track.pieces.iter().map(|p| p.seconds).sum(),
                }
            })
            .collect();
        // Before any piece arrives: the quality's bit rate times its length.
        let length = per_track
            .iter()
            .map(|t| t.total_seconds)
            .fold(0.0, f64::max);
        let first_guess = request
            .variant
            .bandwidth
            .filter(|_| length > 0.0)
            .map(|b| (b as f64 * length / 8.0) as u64);
        let info = StreamDownloadInfo {
            path: request.path.clone(),
            pieces,
            resumed_bytes: restored.bytes,
            total: exact_total,
        };
        let shared = Arc::new(Shared {
            pieces,
            exact_total,
            first_guess,
            per_track: Mutex::new(per_track),
            done: Mutex::new(restored.done),
            done_count: AtomicU64::new(done_count),
            done_bytes: AtomicU64::new(restored.bytes),
            workers: Mutex::new(vec![None; opts.connections.clamp(1, 32)]),
            joining: AtomicBool::new(false),
        });
        Ok(Self {
            client,
            request,
            opts,
            tracks: Arc::new(tracks),
            work,
            signature,
            committed: restored.committed,
            info,
            shared,
        })
    }

    pub fn info(&self) -> &StreamDownloadInfo {
        &self.info
    }

    pub fn progress(&self) -> Progress {
        Progress(self.shared.clone())
    }

    /// Downloads until finished, failed, or `cancel` fires (which pauses).
    pub async fn run(self, cancel: CancellationToken) -> Result<Outcome> {
        if let Some(dir) = self
            .request
            .path
            .parent()
            .filter(|d| !d.as_os_str().is_empty())
        {
            fs::create_dir_all(dir)?;
        }
        let mut offsets = Vec::new();
        let mut pending = Vec::new();
        {
            let done = self.shared.done.lock().unwrap();
            let mut first = 0u64;
            for (t, track) in self.tracks.iter().enumerate() {
                offsets.push(first);
                let mut start = 0.0;
                for (i, piece) in track.pieces.iter().enumerate() {
                    if !done[(first + i as u64) as usize] {
                        pending.push((start, t, i));
                    }
                    start += piece.seconds;
                }
                first += track.pieces.len() as u64;
            }
        }
        // Picture and sound side by side, in playing order.
        pending.sort_by(|a, b| a.0.total_cmp(&b.0).then(a.1.cmp(&b.1)).then(a.2.cmp(&b.2)));
        let pending: VecDeque<(usize, usize)> =
            pending.into_iter().map(|(_, t, i)| (t, i)).collect();

        if !pending.is_empty() {
            let files = (0..self.tracks.len())
                .map(|t| {
                    OpenOptions::new()
                        .append(true)
                        .open(track_path(&self.work, t))
                })
                .collect::<io::Result<Vec<_>>>()?;
            let connections = self.opts.connections.clamp(1, 32).min(pending.len());
            let ctx = Arc::new(Ctx {
                client: self.client.clone(),
                headers: self.request.headers.clone(),
                tracks: self.tracks.clone(),
                offsets,
                work: self.work.clone(),
                shared: self.shared.clone(),
                limiters: self.opts.limiters.clone(),
                max_retries: self.opts.max_retries,
                stop: cancel.child_token(),
                fatal: Mutex::new(None),
                queue: Mutex::new(pending),
                keys: tokio::sync::Mutex::new(HashMap::new()),
                writer: Mutex::new(Writer {
                    files,
                    committed: self.committed.clone(),
                    signature: self.signature.clone(),
                }),
            });
            let mut workers = JoinSet::new();
            for slot in 0..connections {
                workers.spawn(worker(ctx.clone(), slot));
            }
            while workers.join_next().await.is_some() {}
            if let Some(err) = ctx.fatal.lock().unwrap().take() {
                return Err(err);
            }
            if cancel.is_cancelled() {
                return Ok(Outcome::Paused);
            }
            if self.shared.done_count.load(Ordering::Relaxed) < self.shared.pieces {
                return Err(Error::Incomplete);
            }
            // The track files must be closed before they are moved (Windows).
            drop(ctx);
        }

        self.shared.joining.store(true, Ordering::Relaxed);
        let inputs: Vec<(PathBuf, super::Container)> = self
            .tracks
            .iter()
            .enumerate()
            .map(|(t, track)| (track_path(&self.work, t), track.container))
            .collect();
        let ffmpeg = self.request.ffmpeg.iter().find(|p| p.is_file());
        let result = mux::finish(
            &inputs,
            &self.work,
            &self.request.path,
            ffmpeg.map(PathBuf::as_path),
            &cancel,
        )
        .await;
        self.shared.joining.store(false, Ordering::Relaxed);
        match result {
            Ok(path) => {
                let _ = fs::remove_dir_all(&self.work);
                Ok(Outcome::Completed(path))
            }
            Err(Error::Cancelled) => Ok(Outcome::Paused),
            Err(e) => Err(e),
        }
    }
}

/// Fetches a whole-file track in ranges, if the server allows it.
async fn split(client: &Client, headers: &[(String, String)], track: &mut TrackSpec) -> Result<()> {
    let url = track.pieces[0].url.clone();
    let remote = probe(client, url.as_str(), headers).await?;
    if let (true, Some(total)) = (remote.ranges, remote.total.filter(|t| *t > 0)) {
        track.pieces = (0..total.div_ceil(CHUNK))
            .map(|k| {
                let offset = k * CHUNK;
                PieceSpec {
                    url: url.clone(),
                    range: Some((offset, CHUNK.min(total - offset))),
                    key: None,
                    seconds: 0.0,
                }
            })
            .collect();
    }
    Ok(())
}

/// Identifies what the pieces on disk belong to.
fn signature(key: &VariantKey, tracks: &[TrackSpec]) -> String {
    let mut out = format!("{}|{}", key.main, key.audio.as_deref().unwrap_or(""));
    for track in tracks {
        let last = track.pieces.last().map_or("", |p| p.url.path());
        out.push_str(&format!("|{}:{last}", track.pieces.len()));
    }
    out
}

fn track_path(work: &Path, track: usize) -> PathBuf {
    work.join(format!("track{track}"))
}

fn piece_path(work: &Path, track: usize, index: usize, ext: &str) -> PathBuf {
    work.join(format!("{track}-{index:06}.{ext}"))
}

/// How much of each track has been appended to its file.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
struct Committed {
    /// The next piece to append.
    next: usize,
    bytes: u64,
}

#[derive(Debug, Serialize, Deserialize)]
struct Saved {
    signature: String,
    tracks: Vec<Committed>,
}

fn save(work: &Path, saved: &Saved) -> io::Result<()> {
    let tmp = work.join(format!("{STATE_FILE}.tmp"));
    fs::write(&tmp, serde_json::to_vec(saved).map_err(io::Error::other)?)?;
    fs::rename(tmp, work.join(STATE_FILE))
}

struct Restored {
    committed: Vec<Committed>,
    /// Per piece, all tracks in a row.
    done: Vec<bool>,
    bytes: u64,
    track_bytes: Vec<u64>,
}

/// Picks up an earlier run's pieces, or starts afresh if they belong to
/// something else.
fn restore(work: &Path, signature: &str, counts: &[usize]) -> io::Result<Restored> {
    let saved: Option<Saved> = fs::read(work.join(STATE_FILE))
        .ok()
        .and_then(|bytes| serde_json::from_slice(&bytes).ok());
    let mut committed = match saved {
        Some(saved)
            if saved.signature == signature
                && saved.tracks.len() == counts.len()
                && saved.tracks.iter().zip(counts).all(|(c, n)| c.next <= *n) =>
        {
            saved.tracks
        }
        _ => {
            if work.exists() {
                fs::remove_dir_all(work)?;
            }
            vec![Committed::default(); counts.len()]
        }
    };
    fs::create_dir_all(work)?;

    let mut track_bytes = vec![0; counts.len()];
    for (t, c) in committed.iter_mut().enumerate() {
        let file = OpenOptions::new()
            .write(true)
            .create(true)
            .truncate(false)
            .open(track_path(work, t))?;
        let len = file.metadata()?.len();
        if len < c.bytes {
            // Lost in a power cut: this track starts over.
            *c = Committed::default();
        }
        // Anything past the last commit was a piece being appended when we
        // stopped; it is still on disk and gets appended again.
        file.set_len(c.bytes)?;
        track_bytes[t] = c.bytes;
    }

    let mut on_disk = HashSet::new();
    for entry in fs::read_dir(work)? {
        let name = entry?.file_name();
        let Some(stem) = name.to_str().and_then(|n| n.strip_suffix(".seg")) else {
            continue;
        };
        let Some((t, i)) = stem
            .split_once('-')
            .and_then(|(t, i)| Some((t.parse::<usize>().ok()?, i.parse::<usize>().ok()?)))
        else {
            continue;
        };
        if t < counts.len() && i < counts[t] {
            on_disk.insert((t, i));
        }
    }
    let mut done = Vec::new();
    for (t, (&count, c)) in counts.iter().zip(&committed).enumerate() {
        for i in 0..count {
            let seg = on_disk
                .contains(&(t, i))
                .then(|| piece_path(work, t, i, "seg"));
            if i < c.next {
                // Appended already; the copy was left by an interruption.
                if let Some(seg) = seg {
                    let _ = fs::remove_file(seg);
                }
                done.push(true);
            } else if let Some(meta) = seg.and_then(|p| fs::metadata(p).ok()) {
                track_bytes[t] += meta.len();
                done.push(true);
            } else {
                done.push(false);
            }
        }
    }
    save(
        work,
        &Saved {
            signature: signature.to_string(),
            tracks: committed.clone(),
        },
    )?;
    Ok(Restored {
        committed,
        done,
        bytes: track_bytes.iter().sum(),
        track_bytes,
    })
}

#[derive(Debug)]
struct Shared {
    pieces: u64,
    exact_total: Option<u64>,
    first_guess: Option<u64>,
    per_track: Mutex<Vec<TrackDone>>,
    done: Mutex<Vec<bool>>,
    done_count: AtomicU64,
    done_bytes: AtomicU64,
    workers: Mutex<Vec<Option<PieceWork>>>,
    joining: AtomicBool,
}

/// What has arrived of one track, for estimating its size.
#[derive(Debug, Clone, Copy)]
struct TrackDone {
    bytes: u64,
    seconds: f64,
    total_seconds: f64,
}

impl Shared {
    /// Each track's bytes per second of video so far, times its length.
    /// Pieces differ in length, so this beats an average per piece.
    fn estimate(&self) -> Option<u64> {
        self.per_track
            .lock()
            .unwrap()
            .iter()
            .map(|t| (t.seconds > 0.0).then(|| t.bytes as f64 / t.seconds * t.total_seconds))
            .sum::<Option<f64>>()
            .map(|total| total as u64)
            .or(self.first_guess)
    }
}

impl Watch for Shared {
    fn snapshot(&self) -> ProgressSnapshot {
        let workers: Vec<PieceWork> = self
            .workers
            .lock()
            .unwrap()
            .iter()
            .flatten()
            .copied()
            .collect();
        let done = self.done_count.load(Ordering::Relaxed);
        let done_bytes = self.done_bytes.load(Ordering::Relaxed);
        let downloaded = done_bytes + workers.iter().map(|w| w.received).sum::<u64>();
        // Until every size is known, estimate: from the video's length, or
        // failing that, assuming the pieces to come are like those so far.
        let total = self.exact_total.or_else(|| {
            self.estimate()
                .or_else(|| {
                    (done >= 3.min(self.pieces) && done > 0)
                        .then(|| ((done_bytes as f64 / done as f64) * self.pieces as f64) as u64)
                })
                .map(|t| t.max(downloaded))
        });
        ProgressSnapshot {
            downloaded,
            total,
            connections: workers.len(),
            segments: Vec::new(),
            stream: Some(StreamProgress {
                pieces: self.pieces,
                done,
                runs: runs(&self.done.lock().unwrap()),
                workers,
                joining: self.joining.load(Ordering::Relaxed),
            }),
        }
    }
}

fn runs(done: &[bool]) -> Vec<[u64; 2]> {
    let mut out = Vec::new();
    let mut start = None;
    for (i, &d) in done.iter().enumerate() {
        match (d, start) {
            (true, None) => start = Some(i as u64),
            (false, Some(s)) => {
                out.push([s, i as u64]);
                start = None;
            }
            _ => {}
        }
    }
    if let Some(s) = start {
        out.push([s, done.len() as u64]);
    }
    out
}

/// Appends finished pieces to their track files, in order.
struct Writer {
    files: Vec<File>,
    committed: Vec<Committed>,
    signature: String,
}

/// Everything the connections of one stream download share.
struct Ctx {
    client: Client,
    headers: Vec<(String, String)>,
    tracks: Arc<Vec<TrackSpec>>,
    /// Where each track's pieces start in the shared numbering.
    offsets: Vec<u64>,
    work: PathBuf,
    shared: Arc<Shared>,
    limiters: Vec<Arc<RateLimiter>>,
    max_retries: u32,
    /// Fires on pause or on a fatal error.
    stop: CancellationToken,
    fatal: Mutex<Option<Error>>,
    queue: Mutex<VecDeque<(usize, usize)>>,
    keys: tokio::sync::Mutex<HashMap<Url, [u8; 16]>>,
    writer: Mutex<Writer>,
}

impl Ctx {
    fn fail(&self, err: Error) {
        self.fatal.lock().unwrap().get_or_insert(err);
        self.stop.cancel();
    }

    fn set_work(&self, slot: usize, work: Option<PieceWork>) {
        self.shared.workers.lock().unwrap()[slot] = work;
    }

    fn set_received(&self, slot: usize, received: u64) {
        if let Some(work) = &mut self.shared.workers.lock().unwrap()[slot] {
            work.received = received;
        }
    }

    fn mark_done(&self, track: usize, index: usize, bytes: u64) {
        let piece = self.offsets[track] + index as u64;
        self.shared.done.lock().unwrap()[piece as usize] = true;
        let mut per_track = self.shared.per_track.lock().unwrap();
        per_track[track].bytes += bytes;
        per_track[track].seconds += self.tracks[track].pieces[index].seconds;
        drop(per_track);
        self.shared.done_count.fetch_add(1, Ordering::Relaxed);
        self.shared.done_bytes.fetch_add(bytes, Ordering::Relaxed);
    }

    /// The AES key at `url`, fetched once.
    async fn key(&self, url: &Url) -> Result<[u8; 16]> {
        let mut keys = self.keys.lock().await;
        if let Some(key) = keys.get(url) {
            return Ok(*key);
        }
        let req = apply_headers(self.client.get(url.clone()), &self.headers);
        let resp = tokio::select! {
            _ = self.stop.cancelled() => return Err(Error::Cancelled),
            r = req.send() => r?,
        };
        if !resp.status().is_success() {
            return Err(Error::Status(resp.status()));
        }
        let bytes = resp.bytes().await?;
        let key: [u8; 16] = bytes.as_ref().try_into().map_err(|_| {
            Error::BadResponse("the video's decryption key was refused; open the page again".into())
        })?;
        keys.insert(url.clone(), key);
        Ok(key)
    }

    /// Appends every finished piece that is next in line.
    fn commit(&self, track: usize) -> io::Result<()> {
        let mut writer = self.writer.lock().unwrap();
        let Writer {
            files,
            committed,
            signature,
        } = &mut *writer;
        loop {
            let next = committed[track].next;
            let seg = piece_path(&self.work, track, next, "seg");
            let mut piece = match File::open(&seg) {
                Ok(file) => file,
                Err(e) if e.kind() == io::ErrorKind::NotFound => return Ok(()),
                Err(e) => return Err(e),
            };
            let n = io::copy(&mut piece, &mut files[track])?;
            drop(piece);
            committed[track] = Committed {
                next: next + 1,
                bytes: committed[track].bytes + n,
            };
            save(
                &self.work,
                &Saved {
                    signature: signature.clone(),
                    tracks: committed.clone(),
                },
            )?;
            fs::remove_file(&seg)?;
        }
    }
}

async fn worker(ctx: Arc<Ctx>, slot: usize) {
    loop {
        if ctx.stop.is_cancelled() {
            return;
        }
        let Some((track, index)) = ctx.queue.lock().unwrap().pop_front() else {
            return;
        };
        let piece = ctx.offsets[track] + index as u64;
        let mut failures = 0;
        let result = loop {
            ctx.set_work(slot, Some(PieceWork { piece, received: 0 }));
            match fetch_piece(&ctx, slot, track, index).await {
                Ok(size) => break Ok(size),
                Err(_) if ctx.stop.is_cancelled() => break Err(Error::Cancelled),
                Err(e) if e.is_fatal() => break Err(e),
                Err(e) => {
                    failures += 1;
                    if failures > ctx.max_retries {
                        break Err(e);
                    }
                    tokio::select! {
                        _ = ctx.stop.cancelled() => break Err(Error::Cancelled),
                        _ = tokio::time::sleep(backoff(failures)) => {}
                    }
                }
            }
        };
        ctx.set_work(slot, None);
        let size = match result {
            Ok(size) => size,
            Err(Error::Cancelled) => return,
            Err(e) => return ctx.fail(e),
        };
        ctx.mark_done(track, index, size);
        let commit = tokio::task::spawn_blocking({
            let ctx = ctx.clone();
            move || ctx.commit(track)
        });
        match commit.await {
            Ok(Ok(())) => {}
            Ok(Err(e)) => return ctx.fail(e.into()),
            Err(e) => return ctx.fail(io::Error::other(e).into()),
        }
    }
}

/// Downloads one piece into its own file, continuing a partial one.
/// Returns its size.
async fn fetch_piece(ctx: &Ctx, slot: usize, track: usize, index: usize) -> Result<u64> {
    let piece = &ctx.tracks[track].pieces[index];
    let partial = piece_path(&ctx.work, track, index, "part");
    let seg = piece_path(&ctx.work, track, index, "seg");
    // Encrypted pieces are decrypted whole, so they always start over.
    let mut have = match piece.key {
        Some(_) => 0,
        None => tokio::fs::metadata(&partial).await.map_or(0, |m| m.len()),
    };
    if piece.range.is_some_and(|(_, len)| have >= len) {
        have = 0;
    }
    let from = piece.range.map_or(0, |(offset, _)| offset) + have;
    let mut req = apply_headers(ctx.client.get(piece.url.clone()), &ctx.headers);
    match piece.range {
        Some((offset, len)) => {
            req = req.header(RANGE, format!("bytes={from}-{}", offset + len - 1))
        }
        None if have > 0 => req = req.header(RANGE, format!("bytes={have}-")),
        None => {}
    }
    let resp = tokio::select! {
        _ = ctx.stop.cancelled() => return Err(Error::Cancelled),
        r = req.send() => r?,
    };
    let resumed = match resp.status() {
        StatusCode::PARTIAL_CONTENT => {
            let start = resp
                .headers()
                .get(CONTENT_RANGE)
                .and_then(|v| v.to_str().ok())
                .and_then(parse_content_range)
                .and_then(|r| r.start);
            if start.is_some_and(|s| s != from) {
                return Err(Error::BadResponse(format!(
                    "wrong Content-Range for bytes {from}-"
                )));
            }
            have > 0
        }
        // The whole piece: start it over.
        StatusCode::OK if piece.range.is_none() => false,
        StatusCode::OK => return Err(Error::BadResponse("server ignored the Range header".into())),
        StatusCode::RANGE_NOT_SATISFIABLE if have > 0 => {
            let _ = tokio::fs::remove_file(&partial).await;
            return Err(Error::BadResponse("could not continue a piece".into()));
        }
        s => return Err(Error::Status(s)),
    };
    if !resumed {
        have = 0;
    }
    let expected = resp.content_length();
    let file = tokio::fs::OpenOptions::new()
        .create(true)
        .write(true)
        .append(resumed)
        .truncate(!resumed)
        .open(&partial)
        .await?;
    let mut out = BufWriter::with_capacity(WRITE_BUFFER, file);
    let mut stream = resp.bytes_stream();
    let mut received = 0u64;
    loop {
        let next = tokio::select! {
            _ = ctx.stop.cancelled() => None,
            n = stream.next() => Some(n),
        };
        match next {
            None => {
                out.flush().await?;
                return Err(Error::Cancelled);
            }
            Some(None) => break,
            Some(Some(Err(e))) => {
                out.flush().await?;
                return Err(e.into());
            }
            Some(Some(Ok(chunk))) => {
                throttle(&ctx.limiters, chunk.len()).await;
                out.write_all(&chunk).await?;
                received += chunk.len() as u64;
                ctx.set_received(slot, have + received);
            }
        }
    }
    out.flush().await?;
    drop(out);
    if expected.is_some_and(|n| n != received) {
        return Err(Error::BadResponse("connection closed early".into()));
    }
    let size = have + received;
    if let Some((_, len)) = piece.range
        && size != len
    {
        return Err(Error::BadResponse(format!(
            "expected {len} bytes, got {size}"
        )));
    }

    let Some(key) = &piece.key else {
        tokio::fs::rename(&partial, &seg).await?;
        return Ok(size);
    };
    let secret = ctx.key(&key.url).await?;
    let iv = key.iv;
    let decrypted = piece_path(&ctx.work, track, index, "dec");
    tokio::task::spawn_blocking(move || -> Result<u64> {
        let mut data = fs::read(&partial)?;
        let plain = cbc::Decryptor::<aes::Aes128>::new(&secret.into(), &iv.into())
            .decrypt_padded::<Pkcs7>(&mut data)
            .map_err(|_| {
                Error::BadResponse("a piece of the video could not be decrypted".into())
            })?;
        fs::write(&decrypted, plain)?;
        let len = plain.len() as u64;
        // Renamed into place only once complete, so a half-written piece is
        // never taken for a finished one.
        fs::rename(&decrypted, &seg)?;
        let _ = fs::remove_file(&partial);
        Ok(len)
    })
    .await
    .map_err(io::Error::other)?
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn groups_finished_pieces_into_runs() {
        assert_eq!(
            runs(&[true, true, false, true, false, false, true]),
            [[0, 2], [3, 4], [6, 7]]
        );
        assert!(runs(&[false, false]).is_empty());
    }
}
