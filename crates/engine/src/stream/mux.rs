//! Turning a stream's downloaded tracks into the finished file: ffmpeg
//! copies picture and sound into one container without re-encoding. With
//! no ffmpeg, a stream that came as a single track is kept as it is.

use std::path::{Path, PathBuf};
use std::process::Stdio;

use tokio_util::sync::CancellationToken;

use super::Container;
use crate::download::numbered;
use crate::{Error, Result};

const EXE: &str = if cfg!(windows) {
    "ffmpeg.exe"
} else {
    "ffmpeg"
};

/// Looks for ffmpeg in `first` (a path or a folder), next to this program,
/// on the `PATH`, and where Homebrew and MacPorts put it (apps opened from
/// the Finder do not get the shell's `PATH`).
pub fn find_ffmpeg(first: &[PathBuf]) -> Option<PathBuf> {
    let beside_us = std::env::current_exe()
        .ok()
        .and_then(|exe| exe.parent().map(Path::to_path_buf));
    let on_path = std::env::var_os("PATH")
        .map(|p| std::env::split_paths(&p).collect::<Vec<_>>())
        .unwrap_or_default();
    let usual: &[&str] = if cfg!(target_os = "macos") {
        &["/opt/homebrew/bin", "/usr/local/bin", "/opt/local/bin"]
    } else if cfg!(windows) {
        &[]
    } else {
        &["/usr/bin", "/usr/local/bin"]
    };
    first
        .iter()
        .cloned()
        .chain(beside_us)
        .chain(on_path)
        .chain(usual.iter().map(PathBuf::from))
        .map(|p| if p.is_dir() { p.join(EXE) } else { p })
        .find(|p| p.is_file())
}

/// Makes the finished file from the tracks and returns where it is.
pub(crate) async fn finish(
    tracks: &[(PathBuf, Container)],
    work: &Path,
    path: &Path,
    ffmpeg: Option<&Path>,
    cancel: &CancellationToken,
) -> Result<PathBuf> {
    match (ffmpeg, tracks) {
        (Some(ffmpeg), [(track, container)]) => {
            match join(ffmpeg, tracks, work, path, cancel).await {
                Ok(()) => Ok(path.to_path_buf()),
                // The video is fine as it came; ffmpeg only tidies it up.
                Err(Error::Ffmpeg(_)) => keep(track, *container, path),
                Err(e) => Err(e),
            }
        }
        (Some(ffmpeg), _) => {
            join(ffmpeg, tracks, work, path, cancel).await?;
            Ok(path.to_path_buf())
        }
        (None, [(track, container)]) => keep(track, *container, path),
        (None, _) => Err(Error::FfmpegMissing),
    }
}

/// Keeps a single track as it came, under an extension that fits it.
fn keep(track: &Path, container: Container, path: &Path) -> Result<PathBuf> {
    let ext = path
        .extension()
        .and_then(|e| e.to_str())
        .unwrap_or("")
        .to_ascii_lowercase();
    let out = if fits(container, &ext) {
        path.to_path_buf()
    } else {
        free(&path.with_extension(container.extension()))
    };
    std::fs::rename(track, &out)?;
    Ok(out)
}

fn fits(container: Container, ext: &str) -> bool {
    match container {
        Container::Mp4 => matches!(ext, "mp4" | "m4v" | "m4a" | "mov"),
        Container::Ts => matches!(ext, "ts" | "m2ts" | "mts"),
        Container::Webm => matches!(ext, "webm" | "mkv"),
        Container::Aac => ext == "aac",
        Container::Mp3 => ext == "mp3",
    }
}

/// `path`, or `path (1)`... if something is already there.
fn free(path: &Path) -> PathBuf {
    (0..)
        .map(|n| numbered(path, n))
        .find(|p| !p.exists())
        .expect("some numbered name is free")
}

async fn join(
    ffmpeg: &Path,
    tracks: &[(PathBuf, Container)],
    work: &Path,
    path: &Path,
    cancel: &CancellationToken,
) -> Result<()> {
    let ext = path
        .extension()
        .and_then(|e| e.to_str())
        .unwrap_or("mp4")
        .to_ascii_lowercase();
    // ffmpeg picks the format from the extension; the file is moved into
    // place only once it is complete.
    let temp = work.join(format!("joined.{ext}"));
    let mut cmd = tokio::process::Command::new(ffmpeg);
    cmd.args(["-hide_banner", "-nostdin", "-loglevel", "error", "-y"]);
    for (track, _) in tracks {
        cmd.arg("-i").arg(track);
    }
    // Picture from the first track; sound from the separate audio track if
    // there is one. Subtitles and data streams in MPEG-TS are left out, as
    // MP4 cannot hold most of them.
    let audio = if tracks.len() > 1 { "1:a?" } else { "0:a?" };
    cmd.args(["-map", "0:v?", "-map", audio, "-c", "copy"]);
    if matches!(ext.as_str(), "mp4" | "m4v" | "m4a" | "mov") {
        cmd.args(["-movflags", "+faststart"]);
    }
    cmd.arg(&temp)
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::piped())
        .kill_on_drop(true);
    #[cfg(windows)]
    cmd.creation_flags(0x0800_0000); // CREATE_NO_WINDOW: no console flashing up.

    let output = tokio::select! {
        _ = cancel.cancelled() => return Err(Error::Cancelled),
        out = cmd.output() => out.map_err(|e| Error::Ffmpeg(format!("could not run ffmpeg: {e}")))?,
    };
    if !output.status.success() {
        let stderr = String::from_utf8_lossy(&output.stderr);
        let last: Vec<&str> = stderr.lines().filter(|l| !l.trim().is_empty()).collect();
        let tail = last[last.len().saturating_sub(3)..].join("; ");
        return Err(Error::Ffmpeg(if tail.is_empty() {
            format!("ffmpeg stopped with {}", output.status)
        } else {
            tail
        }));
    }
    std::fs::rename(&temp, path)?;
    Ok(())
}
