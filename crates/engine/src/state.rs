//! The resume file stored next to a partial download.

use std::fs;
use std::io::{self, Write};
use std::path::{Path, PathBuf};

use serde::{Deserialize, Serialize};

use crate::probe::RemoteInfo;
use crate::segments::SavedSegment;

const VERSION: u32 = 1;

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct State {
    pub version: u32,
    pub url: String,
    pub final_url: String,
    pub total: u64,
    pub etag: Option<String>,
    pub last_modified: Option<String>,
    pub segments: Vec<SavedSegment>,
}

impl State {
    pub fn new(url: &str, remote: &RemoteInfo, total: u64, segments: Vec<SavedSegment>) -> Self {
        Self {
            version: VERSION,
            url: url.to_string(),
            final_url: remote.final_url.clone(),
            total,
            etag: remote.etag.clone(),
            last_modified: remote.last_modified.clone(),
            segments,
        }
    }

    pub fn load(path: &Path) -> Option<Self> {
        let state: Self = serde_json::from_slice(&fs::read(path).ok()?).ok()?;
        (state.version == VERSION).then_some(state)
    }

    /// Writes to a temporary file and renames it over the old one, so a
    /// crash mid-write never leaves a corrupt resume file.
    pub fn save(&self, path: &Path) -> io::Result<()> {
        let tmp = with_suffix(path, ".tmp");
        let mut f = fs::File::create(&tmp)?;
        f.write_all(&serde_json::to_vec(self).map_err(io::Error::other)?)?;
        f.sync_all()?;
        drop(f);
        fs::rename(&tmp, path)
    }

    /// Whether this partial download came from the same link.
    pub fn same_source(&self, url: &str, remote: &RemoteInfo) -> bool {
        self.url == url || self.final_url == remote.final_url
    }

    /// Whether the server still has the same version of the file.
    pub fn same_version(&self, remote: &RemoteInfo) -> bool {
        if remote.total != Some(self.total) {
            return false;
        }
        match (
            &self.etag,
            &remote.etag,
            &self.last_modified,
            &remote.last_modified,
        ) {
            (Some(a), Some(b), _, _) => a == b,
            (_, _, Some(a), Some(b)) => a == b,
            _ => true,
        }
    }
}

/// `file.zip` + `.part` -> `file.zip.part`
pub fn with_suffix(path: &Path, suffix: &str) -> PathBuf {
    let mut s = path.as_os_str().to_owned();
    s.push(suffix);
    PathBuf::from(s)
}

pub fn part_path(path: &Path) -> PathBuf {
    with_suffix(path, ".part")
}

pub fn state_path(path: &Path) -> PathBuf {
    with_suffix(path, ".slipstream")
}
