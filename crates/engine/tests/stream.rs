//! Video streams against a local server: HLS and DASH, choosing a quality,
//! AES-128, pausing and resuming, and joining tracks with ffmpeg.

use std::path::{Path, PathBuf};
use std::process::Command;
use std::sync::atomic::Ordering;
use std::time::Duration;

use aes::cipher::block_padding::Pkcs7;
use aes::cipher::{BlockModeEncrypt, KeyIvInit};
use slipstream_engine::{
    ClientOptions, Error, Options, Outcome, StreamDownload, StreamKind, StreamRequest, VariantKey,
    build_client, find_ffmpeg, inspect_stream, stream_work_dir,
};
use slipstream_test_server::{MB, Server, data, start};
use tokio_util::sync::CancellationToken;

fn request(
    url: String,
    variant: VariantKey,
    path: PathBuf,
    ffmpeg: Option<PathBuf>,
) -> StreamRequest {
    StreamRequest {
        url,
        headers: Vec::new(),
        variant,
        path,
        ffmpeg: ffmpeg.into_iter().collect(),
    }
}

async fn prepare(req: StreamRequest) -> StreamDownload {
    let client = build_client(&ClientOptions::default()).unwrap();
    StreamDownload::prepare(client, req, Options::default())
        .await
        .unwrap()
}

async fn download(req: StreamRequest) -> slipstream_engine::Result<Outcome> {
    let client = build_client(&ClientOptions::default()).unwrap();
    StreamDownload::prepare(client, req, Options::default())
        .await?
        .run(CancellationToken::new())
        .await
}

/// Serves `count` pieces of assorted sizes as `dir/seg<n>.ts`.
fn add_segments(server: &Server, dir: &str, count: usize, seed: u64) -> Vec<Vec<u8>> {
    (0..count)
        .map(|i| {
            let seg = data(20_000 + (i * 7919) % 30_000, seed + i as u64);
            server.add_file(&format!("{dir}/seg{i}.ts"), seg.clone(), Some("video/mp2t"));
            seg
        })
        .collect()
}

fn playlist(head: &str, entries: &[String]) -> String {
    let mut text = format!("#EXTM3U\n#EXT-X-TARGETDURATION:2\n#EXT-X-PLAYLIST-TYPE:VOD\n{head}");
    for entry in entries {
        text.push_str(&format!("#EXTINF:2.0,\n{entry}\n"));
    }
    text + "#EXT-X-ENDLIST\n"
}

fn names(count: usize) -> Vec<String> {
    (0..count).map(|i| format!("seg{i}.ts")).collect()
}

const M3U8: Option<&str> = Some("application/vnd.apple.mpegurl");

#[tokio::test]
async fn downloads_the_pieces_of_an_hls_stream_in_order() {
    let (server, _) = start(Vec::new()).await;
    let segments = add_segments(&server, "v", 40, 1);
    server.add_file("v/index.m3u8", playlist("", &names(40)).into_bytes(), M3U8);
    server.per_conn_rate.store(4 * MB as u64, Ordering::SeqCst);
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("video.ts");

    let info = inspect_stream(
        &build_client(&ClientOptions::default()).unwrap(),
        &server.url("v/index.m3u8"),
        &[],
    )
    .await
    .unwrap();
    assert_eq!(info.kind, StreamKind::Hls);
    assert_eq!(info.variants.len(), 1);
    assert_eq!(info.duration, Some(80.0));

    let outcome = download(request(
        server.url("v/index.m3u8"),
        VariantKey::default(),
        path.clone(),
        None,
    ))
    .await
    .unwrap();
    assert_eq!(outcome, Outcome::Completed(path.clone()));
    assert_eq!(std::fs::read(&path).unwrap(), segments.concat());
    assert!(
        !stream_work_dir(&path).exists(),
        "the pieces are cleaned up"
    );
    assert!(
        server.max_in_flight.load(Ordering::SeqCst) > 1,
        "used several connections"
    );
}

#[tokio::test]
async fn picks_the_chosen_quality_from_a_master_playlist() {
    let (server, _) = start(Vec::new()).await;
    let low = add_segments(&server, "low", 6, 10);
    add_segments(&server, "high", 6, 20);
    for dir in ["low", "high"] {
        server.add_file(
            &format!("{dir}/index.m3u8"),
            playlist("", &names(6)).into_bytes(),
            M3U8,
        );
    }
    let master = "#EXTM3U
#EXT-X-STREAM-INF:BANDWIDTH=500000,RESOLUTION=640x360
low/index.m3u8
#EXT-X-STREAM-INF:BANDWIDTH=1500000,RESOLUTION=1280x720
high/index.m3u8
";
    server.add_file("master.m3u8", master.as_bytes().to_vec(), M3U8);
    let client = build_client(&ClientOptions::default()).unwrap();
    let info = inspect_stream(&client, &server.url("master.m3u8"), &[])
        .await
        .unwrap();
    let heights: Vec<_> = info.variants.iter().map(|v| v.height).collect();
    assert_eq!(heights, [Some(720), Some(360)], "best first");
    assert_eq!(info.variants[0].estimated_size, Some(1_500_000 * 12 / 8));

    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("low.ts");
    let key = info.variants[1].key.clone();
    download(request(server.url("master.m3u8"), key, path.clone(), None))
        .await
        .unwrap();
    assert_eq!(std::fs::read(&path).unwrap(), low.concat());
    assert_eq!(server.hits("high/seg0.ts"), 0);

    // With no quality chosen, the best one.
    let path = dir.path().join("best.ts");
    download(request(
        server.url("master.m3u8"),
        VariantKey::default(),
        path.clone(),
        None,
    ))
    .await
    .unwrap();
    assert_eq!(server.hits("high/seg0.ts"), 1);
}

fn encrypt(data: &[u8], key: [u8; 16], iv: [u8; 16]) -> Vec<u8> {
    let mut buf = data.to_vec();
    buf.resize(data.len() + 16 - data.len() % 16, 0);
    let len = cbc::Encryptor::<aes::Aes128>::new(&key.into(), &iv.into())
        .encrypt_padded::<Pkcs7>(&mut buf, data.len())
        .unwrap()
        .len();
    buf.truncate(len);
    buf
}

#[tokio::test]
async fn decrypts_aes_128_segments() {
    let (server, _) = start(Vec::new()).await;
    let (key1, key2) = ([7u8; 16], [9u8; 16]);
    server.add_file("keys/1.key", key1.to_vec(), None);
    server.add_file("keys/2.key", key2.to_vec(), None);
    let explicit_iv = 0x0102_0304_0506_0708_090a_0b0c_0d0e_0f10u128.to_be_bytes();
    let plain: Vec<Vec<u8>> = (0..8)
        .map(|i| data(10_000 + i * 333, 50 + i as u64))
        .collect();
    let mut text = "#EXTM3U\n#EXT-X-TARGETDURATION:2\n#EXT-X-MEDIA-SEQUENCE:5\n".to_string();
    for (i, seg) in plain.iter().enumerate() {
        // The first half uses the sequence number as IV, the rest an
        // explicit IV and another key.
        let encrypted = if i < 4 {
            if i == 0 {
                text.push_str("#EXT-X-KEY:METHOD=AES-128,URI=\"keys/1.key\"\n");
            }
            encrypt(seg, key1, (5 + i as u128).to_be_bytes())
        } else {
            if i == 4 {
                text.push_str("#EXT-X-KEY:METHOD=AES-128,URI=\"keys/2.key\",IV=0x0102030405060708090a0b0c0d0e0f10\n");
            }
            encrypt(seg, key2, explicit_iv)
        };
        server.add_file(&format!("enc/seg{i}.ts"), encrypted, None);
        text.push_str(&format!("#EXTINF:2,\nenc/seg{i}.ts\n"));
    }
    text.push_str("#EXT-X-ENDLIST\n");
    server.add_file("enc.m3u8", text.into_bytes(), M3U8);

    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("secret.ts");
    download(request(
        server.url("enc.m3u8"),
        VariantKey::default(),
        path.clone(),
        None,
    ))
    .await
    .unwrap();
    assert_eq!(std::fs::read(&path).unwrap(), plain.concat());
    assert_eq!(server.hits("keys/1.key"), 1, "each key is fetched once");
}

#[tokio::test]
async fn resumes_after_a_pause() {
    let (server, _) = start(Vec::new()).await;
    let segments = add_segments(&server, "r", 30, 3);
    server.add_file("r/index.m3u8", playlist("", &names(30)).into_bytes(), M3U8);
    server.per_conn_rate.store(400_000, Ordering::SeqCst);
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("paused.ts");
    let req = request(
        server.url("r/index.m3u8"),
        VariantKey::default(),
        path.clone(),
        None,
    );

    let first = prepare(req.clone()).await;
    let progress = first.progress();
    let cancel = CancellationToken::new();
    let run = tokio::spawn(first.run(cancel.clone()));
    loop {
        tokio::time::sleep(Duration::from_millis(20)).await;
        if progress.snapshot().stream.unwrap().done >= 6 {
            break;
        }
    }
    cancel.cancel();
    assert_eq!(run.await.unwrap().unwrap(), Outcome::Paused);
    let fetched_first = server.hits("r/seg0.ts");

    server.per_conn_rate.store(0, Ordering::SeqCst);
    let second = prepare(req).await;
    let total: u64 = segments.iter().map(|s| s.len() as u64).sum();
    let resumed = second.info().resumed_bytes;
    assert!(
        resumed > 0 && resumed < total,
        "resumed {resumed} of {total}"
    );
    assert_eq!(
        second.run(CancellationToken::new()).await.unwrap(),
        Outcome::Completed(path.clone())
    );
    assert_eq!(std::fs::read(&path).unwrap(), segments.concat());
    assert_eq!(
        server.hits("r/seg0.ts"),
        fetched_first,
        "finished pieces are not fetched again"
    );
}

#[tokio::test]
async fn refuses_live_and_protected_streams() {
    let (server, _) = start(Vec::new()).await;
    server.add_file(
        "live.m3u8",
        b"#EXTM3U\n#EXTINF:2,\na.ts\n#EXTINF:2,\nb.ts\n".to_vec(),
        M3U8,
    );
    server.add_file(
        "drm.m3u8",
        b"#EXTM3U\n#EXT-X-KEY:METHOD=SAMPLE-AES,URI=\"skd://k\",KEYFORMAT=\"com.apple.streamingkeydelivery\"\n#EXTINF:2,\na.ts\n#EXT-X-ENDLIST\n".to_vec(),
        M3U8,
    );
    server.add_file(
        "page.m3u8",
        b"<html>expired</html>".to_vec(),
        Some("text/html"),
    );
    let client = build_client(&ClientOptions::default()).unwrap();
    let (live, drm, page) = (
        server.url("live.m3u8"),
        server.url("drm.m3u8"),
        server.url("page.m3u8"),
    );
    assert!(matches!(
        inspect_stream(&client, &live, &[]).await,
        Err(Error::Live)
    ));
    assert!(matches!(
        inspect_stream(&client, &drm, &[]).await,
        Err(Error::Protected)
    ));
    assert!(matches!(
        inspect_stream(&client, &page, &[]).await,
        Err(Error::Playlist(_))
    ));
}

#[tokio::test]
async fn fetches_a_whole_file_dash_track_in_ranges() {
    let (server, _) = start(Vec::new()).await;
    let movie = data(20 * MB, 77);
    server.add_file("d/movie.webm", movie.clone(), Some("video/webm"));
    let mpd = r#"<?xml version="1.0"?>
<MPD xmlns="urn:mpeg:dash:schema:mpd:2011" type="static" mediaPresentationDuration="PT60S">
  <Period><AdaptationSet mimeType="video/webm">
    <Representation id="whole" bandwidth="2000000" width="1920" height="1080">
      <BaseURL>movie.webm</BaseURL><SegmentBase indexRange="0-100"/>
    </Representation>
  </AdaptationSet></Period></MPD>"#;
    server.add_file(
        "d/manifest.mpd",
        mpd.as_bytes().to_vec(),
        Some("application/dash+xml"),
    );
    let client = build_client(&ClientOptions::default()).unwrap();
    let info = inspect_stream(&client, &server.url("d/manifest.mpd"), &[])
        .await
        .unwrap();
    assert_eq!(info.kind, StreamKind::Dash);
    assert_eq!(info.variants[0].extension(), "webm");

    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("movie.webm");
    let download_ = prepare(request(
        server.url("d/manifest.mpd"),
        info.variants[0].key.clone(),
        path.clone(),
        None,
    ))
    .await;
    assert_eq!(download_.info().pieces, 3, "20 MB in 8 MB ranges");
    assert_eq!(download_.info().total, Some(20 * MB as u64));
    download_.run(CancellationToken::new()).await.unwrap();
    assert!(std::fs::read(&path).unwrap() == movie);
    assert!(server.range_requests.load(Ordering::SeqCst) >= 3);
}

#[tokio::test]
async fn needs_ffmpeg_to_join_separate_audio() {
    let (server, _) = start(Vec::new()).await;
    for (kind, seed) in [("video", 100), ("audio", 200)] {
        server.add_file(&format!("s/{kind}-init.mp4"), data(500, seed), None);
        for n in 1..=5 {
            server.add_file(&format!("s/{kind}-{n}.m4s"), data(30_000, seed + n), None);
        }
    }
    let mpd = r#"<MPD type="static" mediaPresentationDuration="PT10S"><Period>
  <AdaptationSet mimeType="video/mp4"><Representation id="video" bandwidth="1000000" height="720">
    <SegmentTemplate initialization="$RepresentationID$-init.mp4" media="$RepresentationID$-$Number$.m4s" duration="2" timescale="1"/>
  </Representation></AdaptationSet>
  <AdaptationSet mimeType="audio/mp4"><Representation id="audio" bandwidth="128000">
    <SegmentTemplate initialization="$RepresentationID$-init.mp4" media="$RepresentationID$-$Number$.m4s" duration="2" timescale="1"/>
  </Representation></AdaptationSet>
</Period></MPD>"#;
    server.add_file("s/manifest.mpd", mpd.as_bytes().to_vec(), None);
    let client = build_client(&ClientOptions::default()).unwrap();
    let info = inspect_stream(&client, &server.url("s/manifest.mpd"), &[])
        .await
        .unwrap();
    let variant = &info.variants[0];
    assert!(variant.separate_audio && variant.needs_ffmpeg());
    assert_eq!(variant.key.audio.as_deref(), Some("audio"));

    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("joined.mp4");
    let req = request(
        server.url("s/manifest.mpd"),
        variant.key.clone(),
        path.clone(),
        None,
    );
    assert!(matches!(
        download(req.clone()).await,
        Err(Error::FfmpegMissing)
    ));
    assert!(
        stream_work_dir(&path).is_dir(),
        "the pieces are kept for later"
    );

    // Everything is on disk; trying again goes straight to joining.
    let hits = server.requests.load(Ordering::SeqCst);
    let again = prepare(req).await;
    assert_eq!(again.info().resumed_bytes, 2 * (500 + 5 * 30_000));
    assert!(matches!(
        again.run(CancellationToken::new()).await,
        Err(Error::FfmpegMissing)
    ));
    assert_eq!(
        server.requests.load(Ordering::SeqCst) - hits,
        1,
        "only the manifest was fetched"
    );
}

/// Real media made by ffmpeg, so joining can be checked with ffprobe.
#[tokio::test]
async fn joins_real_video_and_audio_with_ffmpeg() {
    let (Some(ffmpeg), Some(ffprobe)) = (find_ffmpeg(&[]), which("ffprobe")) else {
        eprintln!("skipped: ffmpeg is not installed");
        return;
    };
    let media = tempfile::tempdir().unwrap();
    let m = media.path();
    run(
        &ffmpeg,
        m,
        "-f lavfi -i testsrc=duration=4:size=160x120:rate=10 -f lavfi -i sine=frequency=440:duration=4 -c:v mpeg4 -c:a aac -shortest src.mp4",
    );
    std::fs::create_dir_all(m.join("hls")).unwrap();
    std::fs::create_dir_all(m.join("dash")).unwrap();
    // Like real HLS, the video repeats its headers in every segment.
    run(
        &ffmpeg,
        m,
        "-i src.mp4 -c copy -bsf:v dump_extra -f hls -hls_time 1 -hls_playlist_type vod -hls_segment_filename hls/s%02d.ts hls/index.m3u8",
    );
    run(
        &ffmpeg,
        m,
        "-i src.mp4 -c copy -map 0:v -map 0:a -f dash -seg_duration 1 -use_template 1 -use_timeline 1 -adaptation_sets id=0,streams=v@id=1,streams=a dash/manifest.mpd",
    );

    let (server, _) = start(Vec::new()).await;
    for sub in ["hls", "dash"] {
        for entry in std::fs::read_dir(m.join(sub)).unwrap() {
            let entry = entry.unwrap();
            let name = entry.file_name().into_string().unwrap();
            server.add_file(
                &format!("{sub}/{name}"),
                std::fs::read(entry.path()).unwrap(),
                None,
            );
        }
    }
    let client = build_client(&ClientOptions::default()).unwrap();
    let out = tempfile::tempdir().unwrap();

    // HLS in MPEG-TS becomes an MP4.
    let hls = inspect_stream(&client, &server.url("hls/index.m3u8"), &[])
        .await
        .unwrap();
    assert!(hls.variants[0].needs_ffmpeg());
    let path = out.path().join("hls.mp4");
    let done = download(request(
        server.url("hls/index.m3u8"),
        hls.variants[0].key.clone(),
        path.clone(),
        Some(ffmpeg.clone()),
    ))
    .await
    .unwrap();
    assert_eq!(done, Outcome::Completed(path.clone()));
    assert_eq!(streams(&ffprobe, &path), ["video", "audio"]);

    // Without ffmpeg it stays a .ts.
    let kept = download(request(
        server.url("hls/index.m3u8"),
        hls.variants[0].key.clone(),
        out.path().join("raw.mp4"),
        None,
    ))
    .await
    .unwrap();
    assert_eq!(kept, Outcome::Completed(out.path().join("raw.ts")));

    // DASH with separate picture and sound is joined into one file.
    let dash = inspect_stream(&client, &server.url("dash/manifest.mpd"), &[])
        .await
        .unwrap();
    assert!(dash.variants[0].separate_audio);
    let path = out.path().join("dash.mp4");
    download(request(
        server.url("dash/manifest.mpd"),
        dash.variants[0].key.clone(),
        path.clone(),
        Some(ffmpeg),
    ))
    .await
    .unwrap();
    assert_eq!(streams(&ffprobe, &path), ["video", "audio"]);
    let seconds = duration(&ffprobe, &path);
    assert!((3.5..4.6).contains(&seconds), "{seconds}");
}

fn which(name: &str) -> Option<PathBuf> {
    let exe = if cfg!(windows) {
        format!("{name}.exe")
    } else {
        name.to_string()
    };
    std::env::split_paths(&std::env::var_os("PATH")?)
        .map(|dir| dir.join(&exe))
        .find(|p| p.is_file())
}

/// Runs ffmpeg in `dir`; `@` stands for a space inside one argument.
fn run(ffmpeg: &Path, dir: &Path, args: &str) {
    let status = Command::new(ffmpeg)
        .args(["-hide_banner", "-loglevel", "error", "-y"])
        .args(args.split(' ').map(|a| a.replace('@', " ")))
        .current_dir(dir)
        .status()
        .unwrap();
    assert!(status.success(), "ffmpeg {args}");
}

fn streams(ffprobe: &Path, file: &Path) -> Vec<String> {
    let out = Command::new(ffprobe)
        .args([
            "-v",
            "error",
            "-show_entries",
            "stream=codec_type",
            "-of",
            "csv=p=0",
        ])
        .arg(file)
        .output()
        .unwrap();
    String::from_utf8_lossy(&out.stdout)
        .lines()
        .map(|l| l.trim().to_string())
        .collect()
}

fn duration(ffprobe: &Path, file: &Path) -> f64 {
    let out = Command::new(ffprobe)
        .args([
            "-v",
            "error",
            "-show_entries",
            "format=duration",
            "-of",
            "csv=p=0",
        ])
        .arg(file)
        .output()
        .unwrap();
    String::from_utf8_lossy(&out.stdout).trim().parse().unwrap()
}

#[tokio::test]
async fn estimates_the_size_from_the_length() {
    // 40 seconds at a steady 10 kB per second, in pieces of 2 and 6 seconds,
    // where an average per piece would be badly off.
    let (server, _) = start(Vec::new()).await;
    let mut text = "#EXTM3U\n#EXT-X-TARGETDURATION:6\n".to_string();
    for i in 0..10 {
        let seconds = if i % 2 == 0 { 2 } else { 6 };
        server.add_file(&format!("e/{i}.ts"), data(seconds * 10_000, i as u64), None);
        text.push_str(&format!("#EXTINF:{seconds}.0,\n{i}.ts\n"));
    }
    server.add_file(
        "e/index.m3u8",
        (text + "#EXT-X-ENDLIST\n").into_bytes(),
        M3U8,
    );
    server.add_file(
        "e/master.m3u8",
        b"#EXTM3U\n#EXT-X-STREAM-INF:BANDWIDTH=88000\nindex.m3u8\n".to_vec(),
        M3U8,
    );
    server.per_conn_rate.store(60_000, Ordering::SeqCst);
    let client = build_client(&ClientOptions::default()).unwrap();
    let info = inspect_stream(&client, &server.url("e/master.m3u8"), &[])
        .await
        .unwrap();
    let dir = tempfile::tempdir().unwrap();
    let req = request(
        server.url("e/master.m3u8"),
        info.variants[0].key.clone(),
        dir.path().join("e.ts"),
        None,
    );
    let options = Options {
        connections: 1,
        ..Options::default()
    };
    let job = StreamDownload::prepare(client, req, options).await.unwrap();
    let progress = job.progress();
    assert_eq!(
        progress.snapshot().total,
        Some(88_000 * 40 / 8),
        "first, from the bit rate"
    );
    let run = tokio::spawn(job.run(CancellationToken::new()));
    loop {
        tokio::time::sleep(Duration::from_millis(10)).await;
        let snap = progress.snapshot();
        if snap.stream.unwrap().done >= 1 {
            assert_eq!(snap.total, Some(400_000), "then from what arrived");
            break;
        }
    }
    run.await.unwrap().unwrap();
}
