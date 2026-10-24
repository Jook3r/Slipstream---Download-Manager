//! A local HTTP server that can misbehave on purpose, shared by the tests of
//! the engine and the manager. Every path serves the same data, so the URL's
//! last segment decides the file name.

use std::sync::atomic::{AtomicBool, AtomicU64, AtomicUsize, Ordering};
use std::sync::{Arc, RwLock};
use std::time::Duration;

use axum::Router;
use axum::body::{Body, Bytes};
use axum::extract::State;
use axum::http::{HeaderMap, StatusCode, header};
use axum::response::{IntoResponse, Response};
use futures_util::stream;

pub const MB: usize = 1 << 20;

#[derive(Default)]
pub struct Server {
    base: String,
    data: RwLock<(Arc<Vec<u8>>, String)>,
    pub no_ranges: AtomicBool,
    pub no_length: AtomicBool,
    /// Per-connection speed cap in bytes/s, to make parallelism matter.
    pub per_conn_rate: AtomicU64,
    /// Break this many data responses after `drop_after` bytes.
    pub drops_left: AtomicUsize,
    pub drop_after: AtomicUsize,
    pub requests: AtomicUsize,
    pub range_requests: AtomicUsize,
    pub bytes_sent: AtomicU64,
    pub in_flight: AtomicUsize,
    pub max_in_flight: AtomicUsize,
}

impl Server {
    pub fn set_data(&self, data: Vec<u8>, etag: &str) {
        *self.data.write().unwrap() = (Arc::new(data), format!("\"{etag}\""));
    }

    /// A URL on this server, e.g. `url("files/movie.mp4")`.
    pub fn url(&self, path: &str) -> String {
        format!("{}/{}", self.base, path.trim_start_matches('/'))
    }
}

/// Starts a server and returns it with the URL of `files/test.bin`.
/// `/missing` always answers 404.
pub async fn start(data: Vec<u8>) -> (Arc<Server>, String) {
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
    let server = Arc::new(Server {
        base: format!("http://{addr}"),
        ..Server::default()
    });
    server.set_data(data, "v1");
    let app = Router::new()
        .route(
            "/missing",
            axum::routing::get(|| async { StatusCode::NOT_FOUND.into_response() }),
        )
        .fallback(serve)
        .with_state(server.clone());
    tokio::spawn(async move { axum::serve(listener, app).await.unwrap() });
    let url = server.url("files/test.bin");
    (server, url)
}

/// Deterministic pseudo-random bytes.
pub fn data(len: usize, seed: u64) -> Vec<u8> {
    let mut x = seed | 1;
    (0..len)
        .map(|_| {
            x ^= x << 13;
            x ^= x >> 7;
            x ^= x << 17;
            x as u8
        })
        .collect()
}

/// Decrements `counter` unless it is already zero; true if it did.
fn take_one(counter: &AtomicUsize) -> bool {
    let mut n = counter.load(Ordering::SeqCst);
    while n > 0 {
        match counter.compare_exchange(n, n - 1, Ordering::SeqCst, Ordering::SeqCst) {
            Ok(_) => return true,
            Err(current) => n = current,
        }
    }
    false
}

struct InFlight(Arc<Server>);

impl Drop for InFlight {
    fn drop(&mut self) {
        self.0.in_flight.fetch_sub(1, Ordering::SeqCst);
    }
}

async fn serve(State(s): State<Arc<Server>>, headers: HeaderMap) -> Response {
    let (data, etag) = s.data.read().unwrap().clone();
    s.requests.fetch_add(1, Ordering::SeqCst);
    let len = data.len();

    let if_range_ok = headers
        .get(header::IF_RANGE)
        .is_none_or(|v| v.to_str().unwrap() == etag);
    let range = headers
        .get(header::RANGE)
        .filter(|_| !s.no_ranges.load(Ordering::SeqCst) && if_range_ok)
        .and_then(|v| {
            v.to_str()
                .ok()?
                .strip_prefix("bytes=")?
                .split_once('-')
                .map(|(a, b)| (a.to_string(), b.to_string()))
        })
        .map(|(a, b)| (a.parse::<usize>().unwrap(), b.parse::<usize>().ok()));

    let mut resp = Response::builder().header(header::ETAG, &etag);
    let (from, to) = match range {
        Some((a, _)) if a >= len => {
            return resp
                .status(StatusCode::RANGE_NOT_SATISFIABLE)
                .header(header::CONTENT_RANGE, format!("bytes */{len}"))
                .body(Body::empty())
                .unwrap();
        }
        Some((a, b)) => {
            let end = b.unwrap_or(len - 1).min(len - 1);
            s.range_requests.fetch_add(1, Ordering::SeqCst);
            resp = resp
                .status(StatusCode::PARTIAL_CONTENT)
                .header(header::CONTENT_RANGE, format!("bytes {a}-{end}/{len}"));
            (a, end + 1)
        }
        None => (0, len),
    };
    if !s.no_length.load(Ordering::SeqCst) {
        resp = resp.header(header::CONTENT_LENGTH, to - from);
    }

    // Only real data requests misbehave, not the one-byte probe.
    let drop_at = (to - from > 1 && take_one(&s.drops_left))
        .then(|| from + s.drop_after.load(Ordering::SeqCst));
    let rate = s.per_conn_rate.load(Ordering::SeqCst);
    let current = s.in_flight.fetch_add(1, Ordering::SeqCst) + 1;
    s.max_in_flight.fetch_max(current, Ordering::SeqCst);
    let guard = InFlight(s.clone());

    let body = stream::unfold((from, guard), move |(pos, guard)| {
        let data = data.clone();
        async move {
            if pos >= to {
                return None;
            }
            if drop_at.is_some_and(|d| pos >= d) {
                return Some((Err(std::io::Error::other("simulated drop")), (to, guard)));
            }
            let end = (pos + 16 * 1024).min(to);
            if rate > 0 {
                tokio::time::sleep(Duration::from_secs_f64((end - pos) as f64 / rate as f64)).await;
            }
            guard
                .0
                .bytes_sent
                .fetch_add((end - pos) as u64, Ordering::SeqCst);
            Some((Ok(Bytes::copy_from_slice(&data[pos..end])), (end, guard)))
        }
    });
    resp.body(Body::from_stream(body)).unwrap()
}
