//! IDM-style download categories, decided by file extension.

use serde::{Deserialize, Serialize};

#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum Category {
    Video,
    Music,
    Documents,
    Compressed,
    Programs,
    Images,
    Other,
}

const VIDEO: &[&str] = &[
    "mp4", "m4v", "mkv", "webm", "avi", "mov", "wmv", "flv", "mpg", "mpeg", "3gp", "ts", "m2ts",
    "ogv",
];
const MUSIC: &[&str] = &[
    "mp3", "m4a", "aac", "flac", "wav", "ogg", "oga", "opus", "wma", "aiff", "aif", "mka",
];
const DOCUMENTS: &[&str] = &[
    "pdf", "doc", "docx", "xls", "xlsx", "ppt", "pptx", "odt", "ods", "odp", "rtf", "txt", "csv",
    "epub", "md",
];
const COMPRESSED: &[&str] = &[
    "zip", "rar", "7z", "tar", "gz", "tgz", "bz2", "tbz2", "xz", "txz", "zst", "lz", "lzma", "cab",
];
const PROGRAMS: &[&str] = &[
    "exe", "msi", "msix", "appx", "dmg", "pkg", "deb", "rpm", "appimage", "apk", "jar", "iso",
    "img",
];
const IMAGES: &[&str] = &[
    "jpg", "jpeg", "png", "gif", "webp", "bmp", "svg", "heic", "heif", "tif", "tiff", "ico", "avif",
];

impl Category {
    pub const ALL: [Category; 7] = [
        Category::Video,
        Category::Music,
        Category::Documents,
        Category::Compressed,
        Category::Programs,
        Category::Images,
        Category::Other,
    ];

    pub fn from_file_name(name: &str) -> Self {
        let Some((_, ext)) = name.rsplit_once('.') else {
            return Category::Other;
        };
        let ext = ext.to_ascii_lowercase();
        let table = [
            (Category::Video, VIDEO),
            (Category::Music, MUSIC),
            (Category::Documents, DOCUMENTS),
            (Category::Compressed, COMPRESSED),
            (Category::Programs, PROGRAMS),
            (Category::Images, IMAGES),
        ];
        table
            .into_iter()
            .find(|(_, exts)| exts.contains(&ext.as_str()))
            .map_or(Category::Other, |(category, _)| category)
    }

    /// Sub-folder of the download folder; `Other` goes straight into it.
    pub fn folder(self) -> Option<&'static str> {
        match self {
            Category::Video => Some("Video"),
            Category::Music => Some("Music"),
            Category::Documents => Some("Documents"),
            Category::Compressed => Some("Compressed"),
            Category::Programs => Some("Programs"),
            Category::Images => Some("Images"),
            Category::Other => None,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn categorizes_by_extension() {
        assert_eq!(Category::from_file_name("Movie.MKV"), Category::Video);
        assert_eq!(Category::from_file_name("song.flac"), Category::Music);
        assert_eq!(Category::from_file_name("report.pdf"), Category::Documents);
        assert_eq!(
            Category::from_file_name("linux-6.1.tar.gz"),
            Category::Compressed
        );
        assert_eq!(Category::from_file_name("Setup.exe"), Category::Programs);
        assert_eq!(
            Category::from_file_name("Slipstream.dmg"),
            Category::Programs
        );
        assert_eq!(Category::from_file_name("photo.jpeg"), Category::Images);
        assert_eq!(Category::from_file_name("README"), Category::Other);
        assert_eq!(Category::from_file_name("data.bin"), Category::Other);
    }
}
