//! Video streams: HLS (`.m3u8`) and DASH (`.mpd`). A stream is a list of
//! small pieces rather than one file. [`inspect_stream`] lists the qualities
//! a stream offers; [`StreamDownload`] fetches one over several connections,
//! puts the pieces back together and, with ffmpeg, joins picture and sound
//! into one video.

mod dash;
mod hls;
mod job;
mod mux;

use std::time::Duration;

use reqwest::{Client, Url};
use serde::{Deserialize, Serialize};

pub use job::{
    PieceWork, StreamDownload, StreamDownloadInfo, StreamProgress, StreamRequest, stream_work_dir,
};
pub use mux::find_ffmpeg;

use crate::{Error, Result, apply_headers};

/// A manifest bigger than this is not a manifest.
const MAX_MANIFEST: usize = 64 << 20;
const MANIFEST_RETRIES: u64 = 3;

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum StreamKind {
    Hls,
    Dash,
}

/// How a stream's pieces are packaged.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum Container {
    /// MPEG transport stream, the classic HLS format.
    Ts,
    /// Fragmented MP4.
    Mp4,
    Webm,
    /// Bare AAC or MP3 audio.
    Aac,
    Mp3,
}

impl Container {
    fn from_extension(ext: &str) -> Option<Self> {
        match ext.to_ascii_lowercase().as_str() {
            "ts" | "tsv" | "tsa" | "m2ts" | "mts" => Some(Self::Ts),
            "mp4" | "m4s" | "m4v" | "m4a" | "cmfv" | "cmfa" | "mp4a" | "mp4v" => Some(Self::Mp4),
            "webm" => Some(Self::Webm),
            "aac" | "adts" => Some(Self::Aac),
            "mp3" => Some(Self::Mp3),
            _ => None,
        }
    }

    /// The extension a file holding just this stream should have.
    pub fn extension(self) -> &'static str {
        match self {
            Self::Ts => "ts",
            Self::Mp4 => "mp4",
            Self::Webm => "webm",
            Self::Aac => "aac",
            Self::Mp3 => "mp3",
        }
    }
}

/// Identifies the chosen quality, so it can be found again on resume.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", default)]
pub struct VariantKey {
    /// HLS: the quality's playlist as written in the master playlist (empty
    /// when the link is that playlist itself). DASH: the representation id.
    pub main: String,
    /// The separate audio to join with it: HLS playlist or DASH id.
    pub audio: Option<String>,
    /// Used to recognise the quality if the stream's links have changed.
    pub bandwidth: Option<u64>,
    pub height: Option<u32>,
}

/// One quality a stream offers.
#[derive(Debug, Clone, PartialEq, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct Variant {
    pub key: VariantKey,
    pub width: Option<u32>,
    pub height: Option<u32>,
    /// Bits per second, picture and sound together.
    pub bandwidth: Option<u64>,
    pub codecs: Option<String>,
    pub audio_only: bool,
    /// Picture and sound come separately and need ffmpeg to be joined.
    pub separate_audio: bool,
    pub container: Container,
    pub estimated_size: Option<u64>,
}

impl Variant {
    /// The natural extension for the finished file, with ffmpeg at hand.
    pub fn extension(&self) -> &'static str {
        match self.container {
            Container::Webm => "webm",
            _ if self.audio_only => "m4a",
            _ => "mp4",
        }
    }

    /// Whether ffmpeg is needed to end up with a normal video file.
    pub fn needs_ffmpeg(&self) -> bool {
        self.separate_audio || matches!(self.container, Container::Ts)
    }
}

#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct StreamInfo {
    pub kind: StreamKind,
    /// Best first.
    pub variants: Vec<Variant>,
    /// Seconds.
    pub duration: Option<f64>,
}

/// One piece of a stream: a URL, maybe a byte range of it, maybe a key.
#[derive(Debug, Clone, PartialEq)]
pub(crate) struct PieceSpec {
    pub url: Url,
    /// Offset and length.
    pub range: Option<(u64, u64)>,
    pub key: Option<KeySpec>,
    /// How many seconds of video it holds; 0 for setup data or if unknown.
    pub seconds: f64,
}

/// HLS AES-128: the segment is encrypted with the key at `url`.
#[derive(Debug, Clone, PartialEq)]
pub(crate) struct KeySpec {
    pub url: Url,
    pub iv: [u8; 16],
}

/// The pieces of one track (picture, or picture and sound; or the separate
/// sound), in order.
#[derive(Debug, Clone)]
pub(crate) struct TrackSpec {
    pub pieces: Vec<PieceSpec>,
    pub container: Container,
    /// One whole file, which can be fetched in ranges.
    pub single_file: bool,
}

/// Guesses from a link or its Content-Type whether it is a stream manifest.
pub fn stream_kind(url: &str, content_type: Option<&str>) -> Option<StreamKind> {
    let content_type = content_type
        .unwrap_or("")
        .split(';')
        .next()
        .unwrap_or("")
        .trim()
        .to_ascii_lowercase();
    match content_type.as_str() {
        "application/vnd.apple.mpegurl"
        | "application/x-mpegurl"
        | "audio/mpegurl"
        | "audio/x-mpegurl" => return Some(StreamKind::Hls),
        "application/dash+xml" => return Some(StreamKind::Dash),
        _ => {}
    }
    let path = Url::parse(url).ok()?.path().to_ascii_lowercase();
    if path.ends_with(".m3u8") || path.ends_with(".m3u") {
        Some(StreamKind::Hls)
    } else if path.ends_with(".mpd") {
        Some(StreamKind::Dash)
    } else {
        None
    }
}

/// Reads a stream's manifest and lists the qualities it offers.
pub async fn inspect_stream(
    client: &Client,
    url: &str,
    headers: &[(String, String)],
) -> Result<StreamInfo> {
    let (text, base) = get_text(client, url, headers).await?;
    match detect(&text) {
        Some(StreamKind::Hls) => inspect_hls(client, &text, &base, headers).await,
        Some(StreamKind::Dash) => inspect_dash(&text, &base),
        None => Err(Error::Playlist("this link is not a video stream".into())),
    }
}

async fn inspect_hls(
    client: &Client,
    text: &str,
    base: &Url,
    headers: &[(String, String)],
) -> Result<StreamInfo> {
    let master = match hls::parse(text, base)? {
        hls::Playlist::Media(media) => {
            check_media(&media)?;
            let ranged = media.pieces.iter().all(|p| p.range.is_some());
            let variant = Variant {
                key: VariantKey::default(),
                width: None,
                height: None,
                bandwidth: None,
                codecs: None,
                audio_only: false,
                separate_audio: false,
                container: media.container,
                estimated_size: ranged.then(|| {
                    media
                        .pieces
                        .iter()
                        .filter_map(|p| p.range)
                        .map(|r| r.1)
                        .sum()
                }),
            };
            return Ok(StreamInfo {
                kind: StreamKind::Hls,
                variants: vec![variant],
                duration: Some(media.duration),
            });
        }
        hls::Playlist::Master(master) => master,
    };
    if master.protected {
        return Err(Error::Protected);
    }
    // The qualities' playlists share length, format and protection; one is
    // enough to learn them.
    let sample = fetch_media(client, &master.variants[0].url, headers).await?;
    let mut variants: Vec<Variant> = master
        .variants
        .iter()
        .map(|v| {
            let bandwidth = v.average_bandwidth.or(v.bandwidth);
            let audio_only =
                v.height.is_none() && v.codecs.as_deref().is_some_and(only_audio_codecs);
            Variant {
                key: VariantKey {
                    main: v.uri.clone(),
                    audio: v.audio.as_ref().map(|a| a.uri.clone()),
                    bandwidth: v.bandwidth,
                    height: v.height,
                },
                width: v.width,
                height: v.height,
                bandwidth,
                codecs: v.codecs.clone(),
                audio_only,
                separate_audio: v.audio.is_some(),
                container: sample.container,
                estimated_size: bandwidth.map(|b| (b as f64 * sample.duration / 8.0) as u64),
            }
        })
        .collect();
    sort_best_first(&mut variants);
    Ok(StreamInfo {
        kind: StreamKind::Hls,
        variants,
        duration: Some(sample.duration),
    })
}

fn inspect_dash(text: &str, base: &Url) -> Result<StreamInfo> {
    let mpd = dash::parse(text, base)?;
    check_mpd(&mpd)?;
    let clear: Vec<&dash::Representation> = mpd.reps.iter().filter(|r| !r.protected).collect();
    if clear.is_empty() {
        return Err(Error::Protected);
    }
    let audio = best_audio(&clear);
    let size = |bandwidth: Option<u64>| {
        bandwidth
            .zip(mpd.duration)
            .map(|(b, d)| (b as f64 * d / 8.0) as u64)
    };
    let mut variants: Vec<Variant> = clear
        .iter()
        .filter(|r| r.kind == dash::Kind::Video)
        .map(|v| {
            let bandwidth = match (v.bandwidth, audio.and_then(|a| a.bandwidth)) {
                (Some(v), Some(a)) => Some(v + a),
                (v, _) => v,
            };
            Variant {
                key: VariantKey {
                    main: v.id.clone(),
                    audio: audio.map(|a| a.id.clone()),
                    bandwidth: v.bandwidth,
                    height: v.height,
                },
                width: v.width,
                height: v.height,
                bandwidth,
                codecs: v.codecs.clone(),
                audio_only: false,
                separate_audio: audio.is_some(),
                container: match (v.container, audio.map(|a| a.container)) {
                    (Container::Webm, Some(Container::Webm) | None) => Container::Webm,
                    (Container::Webm, _) => Container::Mp4,
                    (c, _) => c,
                },
                estimated_size: size(bandwidth),
            }
        })
        .collect();
    if variants.is_empty() {
        // Sound only: each audio quality is a choice.
        variants = clear
            .iter()
            .filter(|r| r.kind == dash::Kind::Audio)
            .map(|a| Variant {
                key: VariantKey {
                    main: a.id.clone(),
                    audio: None,
                    bandwidth: a.bandwidth,
                    height: None,
                },
                width: None,
                height: None,
                bandwidth: a.bandwidth,
                codecs: a.codecs.clone(),
                audio_only: true,
                separate_audio: false,
                container: a.container,
                estimated_size: size(a.bandwidth),
            })
            .collect();
    }
    sort_best_first(&mut variants);
    Ok(StreamInfo {
        kind: StreamKind::Dash,
        variants,
        duration: mpd.duration,
    })
}

/// The tracks to download for a chosen quality.
pub(crate) async fn resolve(
    client: &Client,
    url: &str,
    headers: &[(String, String)],
    key: &VariantKey,
) -> Result<Vec<TrackSpec>> {
    let (text, base) = get_text(client, url, headers).await?;
    match detect(&text) {
        Some(StreamKind::Hls) => resolve_hls(client, &text, &base, headers, key).await,
        Some(StreamKind::Dash) => resolve_dash(&text, &base, key),
        None => Err(Error::Playlist("this link is not a video stream".into())),
    }
}

async fn resolve_hls(
    client: &Client,
    text: &str,
    base: &Url,
    headers: &[(String, String)],
    key: &VariantKey,
) -> Result<Vec<TrackSpec>> {
    let master = match hls::parse(text, base)? {
        hls::Playlist::Media(media) => {
            check_media(&media)?;
            return Ok(vec![track_of(media)]);
        }
        hls::Playlist::Master(master) => master,
    };
    if master.protected {
        return Err(Error::Protected);
    }
    let variant = if key.main.is_empty() {
        // No quality chosen (a link added without asking): the best.
        master
            .variants
            .iter()
            .max_by_key(|v| (v.height, v.bandwidth))
            .ok_or_else(gone)?
    } else {
        master
            .variants
            .iter()
            .find(|v| v.uri == key.main)
            .or_else(|| {
                master
                    .variants
                    .iter()
                    .find(|v| v.bandwidth == key.bandwidth && v.height == key.height)
            })
            .ok_or_else(gone)?
    };
    let mut tracks = vec![track_of(fetch_media(client, &variant.url, headers).await?)];
    if key.main.is_empty() {
        if let Some(audio) = &variant.audio {
            tracks.push(track_of(fetch_media(client, &audio.url, headers).await?));
        }
        return Ok(tracks);
    }
    let audio = key.audio.as_ref().and_then(|uri| {
        master
            .variants
            .iter()
            .filter_map(|v| v.audio.as_ref())
            .find(|a| &a.uri == uri)
            .or(variant.audio.as_ref())
    });
    if let Some(audio) = audio {
        tracks.push(track_of(fetch_media(client, &audio.url, headers).await?));
    }
    Ok(tracks)
}

fn resolve_dash(text: &str, base: &Url, key: &VariantKey) -> Result<Vec<TrackSpec>> {
    let mpd = dash::parse(text, base)?;
    check_mpd(&mpd)?;
    let find = |id: &str| mpd.reps.iter().find(|r| r.id == id);
    let clear: Vec<&dash::Representation> = mpd.reps.iter().filter(|r| !r.protected).collect();
    let mut reps = Vec::new();
    if key.main.is_empty() {
        // No quality chosen (a link added without asking): the best.
        let video = clear
            .iter()
            .filter(|r| r.kind == dash::Kind::Video)
            .max_by_key(|r| (r.height, r.bandwidth))
            .copied();
        reps.extend(video);
        reps.extend(best_audio(&clear));
        if reps.is_empty() {
            return Err(if mpd.reps.is_empty() {
                gone()
            } else {
                Error::Protected
            });
        }
    } else {
        let main = find(&key.main)
            .or_else(|| {
                mpd.reps
                    .iter()
                    .find(|r| r.bandwidth == key.bandwidth && r.height == key.height)
            })
            .ok_or_else(gone)?;
        reps.push(main);
        if let Some(audio) = &key.audio {
            reps.push(
                find(audio)
                    .or_else(|| best_audio(&clear))
                    .ok_or_else(gone)?,
            );
        }
    }
    if reps.iter().any(|r| r.protected) {
        return Err(Error::Protected);
    }
    Ok(reps
        .into_iter()
        .map(|r| TrackSpec {
            pieces: r.pieces.clone(),
            container: r.container,
            single_file: r.single_file,
        })
        .collect())
}

fn gone() -> Error {
    Error::Playlist("the quality you chose is no longer offered; add the video again".into())
}

fn track_of(media: hls::Media) -> TrackSpec {
    TrackSpec {
        pieces: media.pieces,
        container: media.container,
        single_file: false,
    }
}

async fn fetch_media(
    client: &Client,
    url: &Url,
    headers: &[(String, String)],
) -> Result<hls::Media> {
    let (text, base) = get_text(client, url.as_str(), headers).await?;
    match hls::parse(&text, &base)? {
        hls::Playlist::Media(media) => {
            check_media(&media)?;
            Ok(media)
        }
        hls::Playlist::Master(_) => Err(Error::Playlist("a playlist inside a playlist".into())),
    }
}

fn check_media(media: &hls::Media) -> Result<()> {
    if media.protected {
        Err(Error::Protected)
    } else if !media.ended {
        Err(Error::Live)
    } else {
        Ok(())
    }
}

fn check_mpd(mpd: &dash::Mpd) -> Result<()> {
    if mpd.dynamic {
        Err(Error::Live)
    } else if mpd.periods > 1 {
        Err(Error::Playlist(
            "this video is made of several parts (periods), which is not supported yet".into(),
        ))
    } else {
        Ok(())
    }
}

/// The main audio track, in its best quality.
fn best_audio<'a>(reps: &[&'a dash::Representation]) -> Option<&'a dash::Representation> {
    let audio = || reps.iter().filter(|r| r.kind == dash::Kind::Audio);
    let main = audio().any(|r| r.main);
    audio()
        .filter(|r| !main || r.main)
        .max_by_key(|r| r.bandwidth.unwrap_or(0))
        .copied()
}

fn only_audio_codecs(codecs: &str) -> bool {
    codecs.split(',').map(str::trim).all(|c| {
        let c = c.to_ascii_lowercase();
        ["mp4a", "ac-3", "ec-3", "opus", "flac", "mp3"]
            .iter()
            .any(|a| c.starts_with(a))
    })
}

fn sort_best_first(variants: &mut [Variant]) {
    variants.sort_by_key(|v| std::cmp::Reverse((v.height, v.bandwidth)));
}

fn detect(text: &str) -> Option<StreamKind> {
    let start = text.trim_start_matches('\u{feff}').trim_start();
    if start.starts_with("#EXTM3U") {
        Some(StreamKind::Hls)
    } else if start.starts_with('<') && text.contains("<MPD") {
        Some(StreamKind::Dash)
    } else {
        None
    }
}

/// Downloads a manifest, retrying a busy server a few times. Returns its
/// text and its final URL (after redirects), which relative links are
/// resolved against.
async fn get_text(
    client: &Client,
    url: &str,
    headers: &[(String, String)],
) -> Result<(String, Url)> {
    let mut attempt = 0;
    loop {
        match get_text_once(client, url, headers).await {
            Err(e) if !e.is_fatal() && attempt < MANIFEST_RETRIES => {
                attempt += 1;
                tokio::time::sleep(Duration::from_millis(700 * attempt)).await;
            }
            result => return result,
        }
    }
}

async fn get_text_once(
    client: &Client,
    url: &str,
    headers: &[(String, String)],
) -> Result<(String, Url)> {
    let resp = apply_headers(client.get(url), headers).send().await?;
    if !resp.status().is_success() {
        return Err(Error::Status(resp.status()));
    }
    if resp
        .content_length()
        .is_some_and(|n| n > MAX_MANIFEST as u64)
    {
        return Err(Error::Playlist("this link is not a video stream".into()));
    }
    let base = resp.url().clone();
    let bytes = resp.bytes().await?;
    if bytes.len() > MAX_MANIFEST {
        return Err(Error::Playlist("this link is not a video stream".into()));
    }
    Ok((String::from_utf8_lossy(&bytes).into_owned(), base))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn recognises_manifest_links() {
        assert_eq!(
            stream_kind("https://a.example/x/master.m3u8?t=1", None),
            Some(StreamKind::Hls)
        );
        assert_eq!(
            stream_kind("https://a.example/manifest.mpd", None),
            Some(StreamKind::Dash)
        );
        assert_eq!(
            stream_kind(
                "https://a.example/play?id=3",
                Some("application/vnd.apple.mpegURL; charset=utf-8")
            ),
            Some(StreamKind::Hls)
        );
        assert_eq!(
            stream_kind("https://a.example/video.mp4", Some("video/mp4")),
            None
        );
    }

    #[test]
    fn knows_which_codecs_are_sound() {
        assert!(only_audio_codecs("mp4a.40.2"));
        assert!(!only_audio_codecs("avc1.64001f,mp4a.40.2"));
    }
}
