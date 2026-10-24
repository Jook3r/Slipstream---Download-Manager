//! A multi-connection download engine with dynamic segmentation and resume.
//!
//! ```no_run
//! # async fn demo() -> slipstream_engine::Result<()> {
//! use slipstream_engine::{Download, Options, Output, Request, build_client, ClientOptions};
//! use tokio_util::sync::CancellationToken;
//!
//! let client = build_client(&ClientOptions::default())?;
//! let request = Request { url: "https://example.com/file.zip".into(), headers: vec![], output: Output::Dir(".".into()) };
//! let download = Download::prepare(client, request, Options::default()).await?;
//! let outcome = download.run(CancellationToken::new()).await?;
//! # Ok(()) }
//! ```

mod download;
mod io;
mod limiter;
pub mod probe;
pub mod segments;
mod state;

use std::time::Duration;

use reqwest::{RequestBuilder, StatusCode, header};

pub use download::{
    Download, DownloadInfo, Options, Outcome, Output, Progress, ProgressSnapshot, Request,
};
pub use limiter::RateLimiter;
pub use probe::{RemoteInfo, probe};
pub use reqwest::{Client, Url};

pub type Result<T, E = Error> = std::result::Result<T, E>;

/// The temporary files a download keeps next to `path` until it finishes:
/// the data (`.part`) and the resume state (`.slipstream`).
pub fn partial_files(path: &std::path::Path) -> [std::path::PathBuf; 2] {
    [state::part_path(path), state::state_path(path)]
}

#[derive(Debug, thiserror::Error)]
pub enum Error {
    #[error("network error")]
    Http(reqwest::Error),
    #[error("server responded with {0}")]
    Status(StatusCode),
    #[error(transparent)]
    Io(#[from] std::io::Error),
    #[error("the file changed on the server while downloading; start it again")]
    RemoteChanged,
    #[error("unexpected response: {0}")]
    BadResponse(String),
    #[error(
        "the server sent a web page instead of the file; the link has probably expired, so get a fresh one from the website"
    )]
    WebPage,
    #[error("gave up after repeated errors; run the download again to resume")]
    Incomplete,
    #[error("cancelled")]
    Cancelled,
}

impl From<reqwest::Error> for Error {
    /// Leaves the URL out: download links are long, and the caller knows it.
    fn from(err: reqwest::Error) -> Self {
        Error::Http(err.without_url())
    }
}

impl Error {
    /// Fatal errors stop every connection; the rest are retried with backoff.
    pub(crate) fn is_fatal(&self) -> bool {
        match self {
            Error::Http(e) => e.is_builder(),
            // 403 is often a per-connection limit rather than a real denial,
            // so it is retried; a link that has truly expired gives up after
            // the retry budget and can be resumed later.
            Error::Status(s) => {
                s.is_client_error()
                    && !matches!(
                        *s,
                        StatusCode::FORBIDDEN
                            | StatusCode::REQUEST_TIMEOUT
                            | StatusCode::TOO_MANY_REQUESTS
                    )
            }
            Error::Io(_)
            | Error::RemoteChanged
            | Error::Incomplete
            | Error::Cancelled
            | Error::WebPage => true,
            Error::BadResponse(_) => false,
        }
    }
}

#[derive(Debug, Clone)]
pub struct ClientOptions {
    pub user_agent: String,
    /// e.g. `http://host:8080` or `socks5://host:1080`. Without one, the
    /// system and `HTTPS_PROXY`-style settings are used.
    pub proxy: Option<String>,
    pub connect_timeout: Duration,
    /// How long a connection may stay silent before it is dropped and retried.
    pub read_timeout: Duration,
}

impl Default for ClientOptions {
    fn default() -> Self {
        Self {
            user_agent: concat!("Slipstream/", env!("CARGO_PKG_VERSION")).to_string(),
            proxy: None,
            connect_timeout: Duration::from_secs(15),
            read_timeout: Duration::from_secs(30),
        }
    }
}

/// Builds an HTTP/1.1-only client. HTTP/2 would multiplex every range request
/// over a single TCP connection, which defeats the point of using several.
pub fn build_client(opts: &ClientOptions) -> Result<Client> {
    let mut builder = Client::builder()
        .http1_only()
        .user_agent(&opts.user_agent)
        .connect_timeout(opts.connect_timeout)
        .read_timeout(opts.read_timeout);
    if let Some(proxy) = &opts.proxy {
        builder = builder.proxy(reqwest::Proxy::all(proxy)?);
    }
    Ok(builder.build()?)
}

/// Adds user headers (cookies, referer, user agent from the browser) and asks
/// for the raw bytes: compressed transfers make byte ranges meaningless.
pub(crate) fn apply_headers(
    mut req: RequestBuilder,
    headers: &[(String, String)],
) -> RequestBuilder {
    req = req.header(header::ACCEPT_ENCODING, "identity");
    for (name, value) in headers {
        req = req.header(name.as_str(), value.as_str());
    }
    req
}
