//! A token bucket shared by every connection of a download.

use std::sync::Mutex;
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::{Duration, Instant};

#[derive(Debug)]
pub struct RateLimiter {
    bytes_per_sec: AtomicU64,
    bucket: Mutex<Bucket>,
}

#[derive(Debug)]
struct Bucket {
    tokens: f64,
    last: Instant,
}

impl RateLimiter {
    /// `0` means unlimited.
    pub fn new(bytes_per_sec: u64) -> Self {
        Self {
            bytes_per_sec: AtomicU64::new(bytes_per_sec),
            bucket: Mutex::new(Bucket {
                tokens: 0.0,
                last: Instant::now(),
            }),
        }
    }

    /// Changes the limit while downloads are running; `0` removes it.
    pub fn set_limit(&self, bytes_per_sec: u64) {
        self.bytes_per_sec.store(bytes_per_sec, Ordering::Relaxed);
    }

    pub fn limit(&self) -> u64 {
        self.bytes_per_sec.load(Ordering::Relaxed)
    }

    /// Accounts for `n` bytes, sleeping if they arrived too fast. Tokens may go
    /// negative, so a large chunk is paid for by waiting rather than refused.
    pub async fn acquire(&self, n: usize) {
        let rate = self.limit();
        if rate == 0 {
            return;
        }
        let wait = {
            let mut b = self.bucket.lock().unwrap();
            let now = Instant::now();
            // Allow bursts of at most 1/8 s so the speed stays smooth.
            let burst = (rate as f64 / 8.0).max(16.0 * 1024.0);
            b.tokens =
                (b.tokens + now.duration_since(b.last).as_secs_f64() * rate as f64).min(burst);
            b.last = now;
            b.tokens -= n as f64;
            (b.tokens < 0.0).then(|| Duration::from_secs_f64(-b.tokens / rate as f64))
        };
        if let Some(wait) = wait {
            tokio::time::sleep(wait).await;
        }
    }
}
