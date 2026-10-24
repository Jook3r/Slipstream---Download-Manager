//! Finding out what we are about to download before committing to a plan.

use percent_encoding::percent_decode_str;
use reqwest::header::{self, HeaderMap};
use reqwest::{Client, StatusCode};

use crate::{Error, Result, apply_headers};

#[derive(Debug, Clone)]
pub struct RemoteInfo {
    /// Where the server finally sent us after redirects.
    pub final_url: String,
    /// Total size, if the server told us.
    pub total: Option<u64>,
    /// Whether the server honours `Range` requests.
    pub ranges: bool,
    pub etag: Option<String>,
    pub last_modified: Option<String>,
    pub content_type: Option<String>,
    /// File name suggested by the server or the URL, already sanitized.
    pub file_name: String,
}

impl RemoteInfo {
    /// Validator to send in `If-Range`, so a changed file is never stitched
    /// together from two versions. Weak ETags are not allowed there.
    pub fn if_range(&self) -> Option<&str> {
        match &self.etag {
            Some(etag) if !etag.starts_with("W/") => Some(etag),
            _ => self.last_modified.as_deref(),
        }
    }
}

/// Asks for the first byte only. A `206` tells us ranges work and gives the
/// total size; a `200` means the server ignores ranges. We use GET rather
/// than HEAD because many servers and signed URLs handle HEAD badly.
pub async fn probe(client: &Client, url: &str, headers: &[(String, String)]) -> Result<RemoteInfo> {
    let req = apply_headers(client.get(url), headers).header(header::RANGE, "bytes=0-0");
    let resp = req.send().await?;
    let status = resp.status();
    let final_url = resp.url().to_string();
    let h = resp.headers();

    let (total, ranges) = match status {
        StatusCode::PARTIAL_CONTENT => {
            let total = h
                .get(header::CONTENT_RANGE)
                .and_then(|v| v.to_str().ok())
                .and_then(parse_content_range)
                .and_then(|cr| cr.total);
            (total, total.is_some())
        }
        // An empty file cannot satisfy "bytes=0-0".
        StatusCode::RANGE_NOT_SATISFIABLE => {
            let total = h
                .get(header::CONTENT_RANGE)
                .and_then(|v| v.to_str().ok())
                .and_then(parse_content_range)
                .and_then(|cr| cr.total);
            match total {
                Some(0) => (Some(0), false),
                _ => return Err(Error::Status(status)),
            }
        }
        s if s.is_success() => (resp.content_length(), false),
        s => return Err(Error::Status(s)),
    };

    let header_str = |name| {
        h.get(name)
            .and_then(|v: &header::HeaderValue| v.to_str().ok())
            .map(str::to_owned)
    };
    let file_name = file_name_from(h, &final_url);
    Ok(RemoteInfo {
        final_url,
        total,
        ranges,
        etag: header_str(header::ETAG),
        last_modified: header_str(header::LAST_MODIFIED),
        content_type: header_str(header::CONTENT_TYPE),
        file_name,
    })
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct ContentRange {
    pub start: Option<u64>,
    pub end: Option<u64>,
    pub total: Option<u64>,
}

/// Parses `bytes 0-99/1234`, `bytes 0-99/*` and `bytes */1234`.
pub fn parse_content_range(v: &str) -> Option<ContentRange> {
    let rest = v.trim().strip_prefix("bytes")?.trim_start();
    let (range, total) = rest.split_once('/')?;
    let total = match total.trim() {
        "*" => None,
        t => Some(t.parse().ok()?),
    };
    let (start, end) = match range.trim() {
        "*" => (None, None),
        r => {
            let (s, e) = r.split_once('-')?;
            (Some(s.trim().parse().ok()?), Some(e.trim().parse().ok()?))
        }
    };
    Some(ContentRange { start, end, total })
}

fn file_name_from(h: &HeaderMap, url: &str) -> String {
    let from_header = h
        .get(header::CONTENT_DISPOSITION)
        .and_then(|v| v.to_str().ok())
        .and_then(disposition_file_name);
    sanitize_file_name(
        &from_header
            .or_else(|| url_file_name(url))
            .unwrap_or_default(),
    )
}

/// A best guess at the file name from the link alone, before contacting the
/// server (which may suggest a different one).
pub fn file_name_from_url(url: &str) -> String {
    sanitize_file_name(&url_file_name(url).unwrap_or_default())
}

fn url_file_name(url: &str) -> Option<String> {
    let path = url.split(['?', '#']).next().unwrap_or("");
    let path = path.split_once("://").map_or(path, |(_, rest)| {
        rest.split_once('/').map_or("", |(_, p)| p)
    });
    path.rsplit('/')
        .find(|s| !s.is_empty())
        .map(|s| percent_decode_str(s).decode_utf8_lossy().into_owned())
}

/// Reads the file name from a `Content-Disposition` header, preferring the
/// RFC 5987 `filename*=UTF-8''...` form.
pub fn disposition_file_name(v: &str) -> Option<String> {
    let mut plain = None;
    for part in v.split(';').map(str::trim) {
        let Some((key, value)) = part.split_once('=') else {
            continue;
        };
        match key.trim().to_ascii_lowercase().as_str() {
            "filename*" => {
                let value = value.trim().trim_matches('"');
                if let Some((charset, rest)) = value.split_once('\'') {
                    let encoded = rest.split_once('\'').map_or(rest, |(_, e)| e);
                    let decoded = percent_decode_str(encoded);
                    let name = if charset.eq_ignore_ascii_case("utf-8") {
                        decoded.decode_utf8_lossy().into_owned()
                    } else {
                        decoded.map(char::from).collect()
                    };
                    if !name.is_empty() {
                        return Some(name);
                    }
                }
            }
            "filename" => plain = Some(value.trim().trim_matches('"').replace("\\\"", "\"")),
            _ => {}
        }
    }
    plain.filter(|n| !n.is_empty())
}

/// Makes a name safe on Windows, macOS and Linux alike.
pub fn sanitize_file_name(name: &str) -> String {
    let name = name.rsplit(['/', '\\']).next().unwrap_or("");
    let mut out: String = name
        .chars()
        .map(|c| {
            if c.is_control() || matches!(c, '<' | '>' | ':' | '"' | '|' | '?' | '*') {
                '_'
            } else {
                c
            }
        })
        .collect();
    while out.ends_with(['.', ' ']) {
        out.pop();
    }
    let out = out.trim_start().to_string();

    const RESERVED: [&str; 22] = [
        "CON", "PRN", "AUX", "NUL", "COM1", "COM2", "COM3", "COM4", "COM5", "COM6", "COM7", "COM8",
        "COM9", "LPT1", "LPT2", "LPT3", "LPT4", "LPT5", "LPT6", "LPT7", "LPT8", "LPT9",
    ];
    let stem = out.split('.').next().unwrap_or("");
    let mut out = if RESERVED.iter().any(|r| r.eq_ignore_ascii_case(stem)) {
        format!("_{out}")
    } else {
        out
    };

    // Most file systems cap names at 255 bytes; leave room for ".part" etc.
    if out.len() > 200 {
        let (stem, ext) = match out.rfind('.') {
            Some(dot) if out.len() - dot <= 16 => (&out[..dot], &out[dot..]),
            _ => (out.as_str(), ""),
        };
        let mut cut = 200 - ext.len();
        while !stem.is_char_boundary(cut) {
            cut -= 1;
        }
        out = format!("{}{}", &stem[..cut], ext);
    }
    if out.is_empty() || out == "." || out == ".." {
        "download".to_string()
    } else {
        out
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn content_range_forms() {
        assert_eq!(
            parse_content_range("bytes 0-0/1234"),
            Some(ContentRange {
                start: Some(0),
                end: Some(0),
                total: Some(1234)
            })
        );
        assert_eq!(
            parse_content_range("bytes 5-9/*"),
            Some(ContentRange {
                start: Some(5),
                end: Some(9),
                total: None
            })
        );
        assert_eq!(
            parse_content_range("bytes */0"),
            Some(ContentRange {
                start: None,
                end: None,
                total: Some(0)
            })
        );
        assert_eq!(parse_content_range("items 0-1/2"), None);
        assert_eq!(parse_content_range("bytes x-1/2"), None);
    }

    #[test]
    fn disposition_names() {
        assert_eq!(
            disposition_file_name(r#"attachment; filename="report.pdf""#).as_deref(),
            Some("report.pdf")
        );
        assert_eq!(
            disposition_file_name("attachment; filename=plain.zip").as_deref(),
            Some("plain.zip")
        );
        assert_eq!(
            disposition_file_name(
                r#"attachment; filename="fallback.txt"; filename*=UTF-8''%E2%82%AC%20rates.txt"#
            )
            .as_deref(),
            Some("€ rates.txt")
        );
        assert_eq!(disposition_file_name("inline"), None);
    }

    #[test]
    fn names_from_urls() {
        let h = HeaderMap::new();
        assert_eq!(
            file_name_from(&h, "https://example.com/files/My%20Movie.mp4?token=abc"),
            "My Movie.mp4"
        );
        assert_eq!(file_name_from(&h, "https://example.com/dir/"), "dir");
        assert_eq!(file_name_from(&h, "https://example.com"), "download");
        assert_eq!(file_name_from(&h, "https://example.com/"), "download");
    }

    #[test]
    fn sanitizes_for_every_platform() {
        assert_eq!(
            sanitize_file_name(r#"a<b>c:d"e|f?g*h.txt"#),
            "a_b_c_d_e_f_g_h.txt"
        );
        assert_eq!(sanitize_file_name("../../etc/passwd"), "passwd");
        assert_eq!(sanitize_file_name(r"..\..\boot.ini"), "boot.ini");
        assert_eq!(sanitize_file_name("trailing. . "), "trailing");
        assert_eq!(sanitize_file_name("con.txt"), "_con.txt");
        assert_eq!(sanitize_file_name(".."), "download");
        assert_eq!(sanitize_file_name(""), "download");
        let long = format!("{}.tar.gz", "é".repeat(150));
        let s = sanitize_file_name(&long);
        assert!(s.len() <= 200 && s.ends_with(".gz"), "{s}");
    }
}
