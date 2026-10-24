//! ffmpeg joins a video stream's picture and sound into one file. Slipstream
//! uses one the user already has, or fetches a build for them into its data
//! folder.

use std::fs::{self, File};
use std::io;
use std::path::{Path, PathBuf};
use std::time::Duration;

use slipstream_engine::{Client, Download, Options, Outcome, Output, Request};
use tokio_util::sync::CancellationToken;

pub const EXE: &str = if cfg!(windows) {
    "ffmpeg.exe"
} else {
    "ffmpeg"
};

/// A ready-made ffmpeg for this computer, as a zip holding the program.
pub fn download_url() -> Option<&'static str> {
    if cfg!(all(windows, target_arch = "x86_64")) {
        Some("https://www.gyan.dev/ffmpeg/builds/ffmpeg-release-essentials.zip")
    } else if cfg!(all(windows, target_arch = "aarch64")) {
        Some(
            "https://github.com/BtbN/FFmpeg-Builds/releases/download/latest/ffmpeg-master-latest-winarm64-gpl.zip",
        )
    } else if cfg!(all(target_os = "macos", target_arch = "aarch64")) {
        Some("https://ffmpeg.martin-riedl.de/redirect/latest/macos/arm64/release/ffmpeg.zip")
    } else if cfg!(all(target_os = "macos", target_arch = "x86_64")) {
        Some("https://ffmpeg.martin-riedl.de/redirect/latest/macos/amd64/release/ffmpeg.zip")
    } else if cfg!(all(target_os = "linux", target_arch = "x86_64")) {
        Some("https://ffmpeg.martin-riedl.de/redirect/latest/linux/amd64/release/ffmpeg.zip")
    } else if cfg!(all(target_os = "linux", target_arch = "aarch64")) {
        Some("https://ffmpeg.martin-riedl.de/redirect/latest/linux/arm64/release/ffmpeg.zip")
    } else {
        None
    }
}

/// Where a fetched ffmpeg lives.
pub fn installed_path(data_dir: &Path) -> PathBuf {
    data_dir.join("ffmpeg").join(EXE)
}

/// Downloads the zip at `url`, takes ffmpeg out of it and checks that it
/// runs. `progress` hears (downloaded, total) as it goes.
pub async fn install(
    client: Client,
    url: &str,
    data_dir: &Path,
    progress: impl Fn(u64, Option<u64>) + Send + 'static,
) -> Result<PathBuf, String> {
    let dir = data_dir.join("ffmpeg");
    fs::create_dir_all(&dir).map_err(|e| e.to_string())?;
    let zip = dir.join("ffmpeg-download.zip");
    let request = Request {
        url: url.to_string(),
        headers: Vec::new(),
        output: Output::File(zip.clone()),
    };
    let download = Download::prepare(client, request, Options::default())
        .await
        .map_err(|e| format!("could not download ffmpeg: {e}"))?;
    let watch = download.progress();
    let ticker = tokio::spawn(async move {
        loop {
            let snap = watch.snapshot();
            progress(snap.downloaded, snap.total);
            tokio::time::sleep(Duration::from_millis(250)).await;
        }
    });
    let outcome = download.run(CancellationToken::new()).await;
    ticker.abort();
    match outcome {
        Ok(Outcome::Completed(_)) => {}
        Ok(Outcome::Paused) => return Err("the download of ffmpeg stopped".into()),
        Err(e) => return Err(format!("could not download ffmpeg: {e}")),
    }

    let target = installed_path(data_dir);
    let extracted = tokio::task::spawn_blocking({
        let (zip, target) = (zip.clone(), target.clone());
        move || extract(&zip, &target)
    })
    .await
    .map_err(|e| e.to_string())?;
    let _ = fs::remove_file(&zip);
    extracted.map_err(|e| format!("could not unpack ffmpeg: {e}"))?;
    if !runs(&target).await {
        let _ = fs::remove_file(&target);
        return Err("the downloaded ffmpeg does not run on this computer".into());
    }
    Ok(target)
}

/// Copies the ffmpeg program out of the zip, wherever it sits inside.
fn extract(zip: &Path, target: &Path) -> io::Result<()> {
    let mut archive = zip::ZipArchive::new(File::open(zip)?).map_err(io::Error::other)?;
    for i in 0..archive.len() {
        let mut entry = archive.by_index(i).map_err(io::Error::other)?;
        let is_ffmpeg = entry
            .enclosed_name()
            .and_then(|p| p.file_name().map(|n| n == EXE))
            .unwrap_or(false);
        if entry.is_dir() || !is_ffmpeg {
            continue;
        }
        let tmp = target.with_extension("part");
        let mut out = File::create(&tmp)?;
        io::copy(&mut entry, &mut out)?;
        out.sync_all()?;
        drop(out);
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            fs::set_permissions(&tmp, fs::Permissions::from_mode(0o755))?;
        }
        return fs::rename(tmp, target);
    }
    Err(io::Error::new(
        io::ErrorKind::NotFound,
        format!("{EXE} is not in the download"),
    ))
}

/// Whether `ffmpeg -version` works.
pub async fn runs(ffmpeg: &Path) -> bool {
    let mut cmd = tokio::process::Command::new(ffmpeg);
    cmd.arg("-version")
        .stdin(std::process::Stdio::null())
        .stdout(std::process::Stdio::null())
        .stderr(std::process::Stdio::null())
        .kill_on_drop(true);
    #[cfg(windows)]
    cmd.creation_flags(0x0800_0000); // CREATE_NO_WINDOW
    matches!(
        tokio::time::timeout(Duration::from_secs(20), cmd.status()).await,
        Ok(Ok(status)) if status.success()
    )
}
