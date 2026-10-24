//! The local connection `slipstream-bridge` uses to hand over links from the
//! browser. It listens on 127.0.0.1 and accepts only callers that present
//! the random token written to `ipc.json`, which only this user can read.

use std::io;
use std::net::{Ipv4Addr, TcpListener as StdListener};
use std::path::Path;
use std::time::Duration;

use serde::Deserialize;
use serde_json::{Value, json};
use slipstream_bridge::{Endpoint, Envelope, VERSION};
use slipstream_manager::{Manager, NewDownload};
use tauri::{AppHandle, Manager as _};
use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader};
use tokio::net::{TcpListener, TcpStream};

use crate::windows::{self, BatchLink, MediaHint, PendingLink};

/// A link the extension caught.
#[derive(Debug, Deserialize)]
#[serde(rename_all = "camelCase")]
struct Download {
    url: String,
    referrer: Option<String>,
    file_name: Option<String>,
    file_size: Option<u64>,
    cookies: Option<String>,
    user_agent: Option<String>,
}

/// A video the extension saw a page play: a stream manifest or a media
/// file.
#[derive(Debug, Deserialize)]
#[serde(rename_all = "camelCase")]
struct Media {
    url: String,
    /// "hls", "dash" or "file".
    kind: String,
    /// The page's title, to name the file after.
    title: Option<String>,
    /// The page playing it.
    referrer: Option<String>,
    file_name: Option<String>,
    file_size: Option<u64>,
    cookies: Option<String>,
    user_agent: Option<String>,
}

/// "Download all links" from a page.
#[derive(Debug, Deserialize)]
#[serde(rename_all = "camelCase")]
struct DownloadAll {
    links: Vec<PageLink>,
    referrer: Option<String>,
    user_agent: Option<String>,
}

#[derive(Debug, Deserialize)]
struct PageLink {
    url: String,
    text: Option<String>,
    cookies: Option<String>,
}

/// Starts listening and publishes the endpoint in `data_dir`.
pub fn start(app: AppHandle, data_dir: &Path) -> io::Result<()> {
    let listener = StdListener::bind((Ipv4Addr::LOCALHOST, 0))?;
    listener.set_nonblocking(true)?;
    let token = random_token()?;
    Endpoint {
        port: listener.local_addr()?.port(),
        token: token.clone(),
        pid: std::process::id(),
    }
    .write(data_dir)?;
    tauri::async_runtime::spawn(async move {
        let listener = match TcpListener::from_std(listener) {
            Ok(listener) => listener,
            Err(e) => return eprintln!("browser integration is off: {e}"),
        };
        while let Ok((stream, _)) = listener.accept().await {
            let (app, token) = (app.clone(), token.clone());
            tauri::async_runtime::spawn(async move {
                let _ = serve(&app, &token, stream).await;
            });
        }
    });
    Ok(())
}

async fn serve(app: &AppHandle, token: &str, stream: TcpStream) -> io::Result<()> {
    let (read, mut write) = stream.into_split();
    let mut line = String::new();
    tokio::time::timeout(
        Duration::from_secs(10),
        BufReader::new(read).read_line(&mut line),
    )
    .await
    .map_err(|_| io::Error::from(io::ErrorKind::TimedOut))??;
    let reply = match serde_json::from_str::<Envelope>(&line) {
        Ok(envelope) if envelope.token == token => handle(app, envelope.request),
        Ok(_) => failure("wrong token"),
        Err(e) => failure(&format!("bad request: {e}")),
    };
    let mut reply = serde_json::to_vec(&reply)?;
    reply.push(b'\n');
    write.write_all(&reply).await
}

fn handle(app: &AppHandle, request: Value) -> Value {
    let result = match request["type"].as_str() {
        Some("ping") => Ok(()),
        Some("activate") => {
            windows::show_main(app);
            Ok(())
        }
        Some("download") => serde_json::from_value(request)
            .map_err(|e| e.to_string())
            .and_then(|d| download(app, d)),
        Some("downloadAll") => serde_json::from_value(request)
            .map_err(|e| e.to_string())
            .and_then(|d| download_all(app, d)),
        Some("media") => serde_json::from_value(request)
            .map_err(|e| e.to_string())
            .and_then(|m| media(app, m)),
        _ => Err("unknown request".to_string()),
    };
    match result {
        Ok(()) => json!({ "ok": true, "appRunning": true, "version": VERSION }),
        Err(e) => failure(&e),
    }
}

fn failure(error: &str) -> Value {
    json!({ "ok": false, "error": error, "version": VERSION })
}

fn download(app: &AppHandle, d: Download) -> Result<(), String> {
    let headers = browser_headers(
        d.cookies.as_deref(),
        d.referrer.as_deref(),
        d.user_agent.as_deref(),
    );
    let manager = app.state::<Manager>();
    let settings = manager.settings();
    if settings.show_file_info {
        let link = PendingLink {
            url: d.url,
            headers,
            referrer: d.referrer,
            file_name: d.file_name,
            file_size: d.file_size,
            from_browser: true,
            media: None,
        };
        return windows::open_file_info(app, link).map_err(|e| e.to_string());
    }
    let item = manager.add_download(NewDownload {
        url: d.url,
        headers,
        start: true,
        ..NewDownload::default()
    })?;
    if settings.show_progress {
        windows::open_progress(app, item.id).map_err(|e| e.to_string())?;
    }
    Ok(())
}

/// A video always gets the Download File Info window, where its quality and
/// name are chosen.
fn media(app: &AppHandle, m: Media) -> Result<(), String> {
    let mut headers = browser_headers(
        m.cookies.as_deref(),
        m.referrer.as_deref(),
        m.user_agent.as_deref(),
    );
    // Video servers often check where a request comes from, as browsers
    // send it for a player's requests to another site.
    if let Some(origin) = m.referrer.as_deref().and_then(origin_of)
        && origin_of(&m.url).as_deref() != Some(origin.as_str())
    {
        headers.push(("Origin".into(), origin));
    }
    let link = PendingLink {
        url: m.url,
        headers,
        referrer: m.referrer,
        file_name: m.file_name,
        file_size: m.file_size,
        from_browser: true,
        media: Some(MediaHint {
            kind: m.kind,
            title: m.title,
        }),
    };
    windows::open_file_info(app, link).map_err(|e| e.to_string())
}

/// `https://host:port` of a link.
fn origin_of(url: &str) -> Option<String> {
    let (scheme, rest) = url.split_once("://")?;
    let host = rest.split(['/', '?', '#']).next()?;
    (!host.is_empty() && matches!(scheme, "http" | "https")).then(|| format!("{scheme}://{host}"))
}

fn download_all(app: &AppHandle, d: DownloadAll) -> Result<(), String> {
    let links: Vec<BatchLink> = d
        .links
        .into_iter()
        .map(|link| BatchLink {
            headers: browser_headers(
                link.cookies.as_deref(),
                d.referrer.as_deref(),
                d.user_agent.as_deref(),
            ),
            url: link.url,
            text: link.text,
        })
        .collect();
    if links.is_empty() {
        return Err("no links".into());
    }
    windows::open_batch(app, links).map_err(|e| e.to_string())
}

/// The headers that make the server treat us like the browser did.
fn browser_headers(
    cookies: Option<&str>,
    referrer: Option<&str>,
    user_agent: Option<&str>,
) -> Vec<(String, String)> {
    [
        ("Cookie", cookies),
        ("Referer", referrer),
        ("User-Agent", user_agent),
    ]
    .into_iter()
    .filter_map(|(name, value)| {
        let value = value?.trim();
        (!value.is_empty() && !value.contains(['\r', '\n']))
            .then(|| (name.to_string(), value.to_string()))
    })
    .collect()
}

fn random_token() -> io::Result<String> {
    let mut bytes = [0u8; 32];
    getrandom::fill(&mut bytes).map_err(io::Error::other)?;
    Ok(bytes.iter().map(|b| format!("{b:02x}")).collect())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn builds_headers_from_browser_details() {
        let headers = browser_headers(Some("a=1; b=2"), Some("https://site/page"), Some(" "));
        assert_eq!(
            headers,
            [
                ("Cookie".to_string(), "a=1; b=2".to_string()),
                ("Referer".to_string(), "https://site/page".to_string()),
            ]
        );
        assert!(browser_headers(Some("x\r\nInjected: 1"), None, None).is_empty());
    }

    #[test]
    fn finds_origins() {
        assert_eq!(
            origin_of("https://www.site.example/watch?v=1").as_deref(),
            Some("https://www.site.example")
        );
        assert_eq!(
            origin_of("http://cdn.example:8080/a.m3u8").as_deref(),
            Some("http://cdn.example:8080")
        );
        assert_eq!(origin_of("file:///etc/passwd"), None);
    }

    #[test]
    fn tokens_are_long_and_different() {
        let a = random_token().unwrap();
        assert_eq!(a.len(), 64);
        assert_ne!(a, random_token().unwrap());
    }
}
