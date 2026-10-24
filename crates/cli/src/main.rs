use std::path::PathBuf;
use std::process::ExitCode;
use std::sync::Arc;
use std::time::{Duration, Instant};

use anyhow::{Context, bail};
use clap::Parser;
use indicatif::{HumanBytes, ProgressBar, ProgressStyle};
use slipstream_engine::{
    ClientOptions, Download, Options, Outcome, Output, ProgressSnapshot, RateLimiter, Request,
    build_client,
};
use tokio_util::sync::CancellationToken;

/// Download files over many connections at once, with pause and resume.
///
/// Press Ctrl+C to pause; run the same command again to resume.
#[derive(Parser)]
#[command(version)]
struct Args {
    /// The link to download.
    url: String,
    /// Save to this exact file.
    #[arg(short, long, conflicts_with = "dir")]
    output: Option<PathBuf>,
    /// Save into this folder under the server's file name [default: current folder].
    #[arg(short, long)]
    dir: Option<PathBuf>,
    /// Maximum parallel connections.
    #[arg(short, long, default_value_t = 8, value_parser = clap::value_parser!(u16).range(1..=32))]
    connections: u16,
    /// Extra request header, e.g. -H "Cookie: session=abc". Repeatable.
    #[arg(short = 'H', long = "header", value_name = "NAME: VALUE")]
    headers: Vec<String>,
    /// Page the link came from (some sites require it).
    #[arg(long)]
    referer: Option<String>,
    /// User-Agent to send (e.g. your browser's).
    #[arg(short = 'A', long)]
    user_agent: Option<String>,
    /// Speed limit in bytes per second, e.g. 500K or 2M.
    #[arg(short, long, value_parser = parse_size)]
    limit: Option<u64>,
    /// Proxy, e.g. http://host:8080 or socks5://host:1080.
    #[arg(long)]
    proxy: Option<String>,
    /// Consecutive failures a connection tolerates before giving up.
    #[arg(long, default_value_t = 8)]
    retries: u32,
    /// Seconds a connection may stall before it is retried.
    #[arg(long, default_value_t = 30)]
    timeout: u64,
}

#[tokio::main]
async fn main() -> ExitCode {
    match run(Args::parse()).await {
        Ok(code) => code,
        Err(e) => {
            eprintln!("error: {e:#}");
            ExitCode::FAILURE
        }
    }
}

async fn run(args: Args) -> anyhow::Result<ExitCode> {
    let mut headers = args
        .headers
        .iter()
        .map(|h| parse_header(h))
        .collect::<anyhow::Result<Vec<_>>>()?;
    if let Some(referer) = args.referer {
        headers.push(("Referer".into(), referer));
    }
    let mut client_opts = ClientOptions {
        proxy: args.proxy,
        read_timeout: Duration::from_secs(args.timeout),
        ..Default::default()
    };
    if let Some(ua) = args.user_agent {
        client_opts.user_agent = ua;
    }
    let client = build_client(&client_opts).context("invalid network settings")?;

    let output = match (args.output, args.dir) {
        (Some(file), _) => Output::File(file),
        (None, Some(dir)) => Output::Dir(dir),
        (None, None) => Output::Dir(PathBuf::from(".")),
    };
    let options = Options {
        connections: args.connections.into(),
        max_retries: args.retries,
        limiters: args
            .limit
            .map(|l| Arc::new(RateLimiter::new(l)))
            .into_iter()
            .collect(),
        ..Default::default()
    };
    let request = Request {
        url: args.url,
        headers,
        output,
    };
    let download = Download::prepare(client, request, options)
        .await
        .context("could not start the download")?;

    let info = download.info().clone();
    println!("Saving to  {}", info.path.display());
    match (info.total, info.resumable) {
        (Some(total), true) => println!(
            "Size       {} (resumable, up to {} connection{})",
            HumanBytes(total),
            args.connections,
            if args.connections == 1 { "" } else { "s" }
        ),
        (Some(total), false) => println!(
            "Size       {} (server does not support resume; 1 connection)",
            HumanBytes(total)
        ),
        (None, _) => println!("Size       unknown (server does not support resume; 1 connection)"),
    }
    if info.restarted {
        println!("The file changed on the server since the last attempt, so it starts over.");
    }
    if info.resumed_bytes > 0 {
        println!(
            "Resuming   {} already downloaded",
            HumanBytes(info.resumed_bytes)
        );
    }

    let cancel = CancellationToken::new();
    tokio::spawn({
        let cancel = cancel.clone();
        async move {
            if tokio::signal::ctrl_c().await.is_ok() {
                cancel.cancel();
            }
        }
    });

    let bar = progress_bar(info.total, info.resumed_bytes);
    let ticker = tokio::spawn({
        let progress = download.progress();
        let bar = bar.clone();
        async move {
            loop {
                let snap = progress.snapshot();
                bar.set_position(snap.downloaded);
                bar.set_message(status_line(&snap));
                tokio::time::sleep(Duration::from_millis(200)).await;
            }
        }
    });

    let started = Instant::now();
    let result = download.run(cancel).await;
    ticker.abort();
    bar.finish_and_clear();

    match result? {
        Outcome::Completed(path) => {
            let secs = started.elapsed().as_secs_f64();
            let size = std::fs::metadata(&path).map(|m| m.len()).unwrap_or(0);
            let fetched = size.saturating_sub(info.resumed_bytes);
            println!(
                "Done       {} in {:.1}s ({}/s)",
                path.display(),
                secs,
                HumanBytes((fetched as f64 / secs.max(0.001)) as u64)
            );
            Ok(ExitCode::SUCCESS)
        }
        Outcome::Paused if info.resumable => {
            println!("Paused. Run the same command again to resume.");
            Ok(ExitCode::from(130))
        }
        Outcome::Paused => {
            println!(
                "Stopped. This server does not support resuming, so the next run starts over."
            );
            Ok(ExitCode::from(130))
        }
    }
}

fn progress_bar(total: Option<u64>, start: u64) -> ProgressBar {
    let (bar, template) = match total {
        Some(total) => (
            ProgressBar::new(total),
            "{spinner:.green} {bar:40.cyan/blue} {bytes}/{total_bytes}  {binary_bytes_per_sec}  ETA {eta}\n  {msg}",
        ),
        None => (
            ProgressBar::no_length(),
            "{spinner:.green} {bytes}  {binary_bytes_per_sec}  {elapsed}\n  {msg}",
        ),
    };
    bar.set_style(
        ProgressStyle::with_template(template)
            .unwrap()
            .progress_chars("█▉▊▋▌▍▎▏ "),
    );
    bar.set_position(start);
    bar.reset_eta();
    bar.enable_steady_tick(Duration::from_millis(120));
    bar
}

/// IDM-style map of the file: which parts are on disk, and how many
/// connections are working on it.
fn status_line(snap: &ProgressSnapshot) -> String {
    let conns = format!(
        "{} connection{}",
        snap.connections,
        if snap.connections == 1 { "" } else { "s" }
    );
    let Some(total) = snap.total.filter(|&t| t > 0 && !snap.segments.is_empty()) else {
        return conns;
    };
    const CELLS: u64 = 50;
    let map: String = (0..CELLS)
        .map(|i| {
            let (lo, hi) = (i * total / CELLS, (i + 1) * total / CELLS);
            let done: u64 = snap
                .segments
                .iter()
                .map(|s| s.written.min(hi).saturating_sub(s.start.max(lo)))
                .sum();
            match done as f64 / (hi - lo).max(1) as f64 {
                f if f >= 0.999 => '█',
                f if f >= 0.5 => '▓',
                f if f > 0.0 => '░',
                _ => '·',
            }
        })
        .collect();
    format!("{map}  {conns}")
}

fn parse_header(h: &str) -> anyhow::Result<(String, String)> {
    match h.split_once(':') {
        Some((name, value)) if !name.trim().is_empty() => {
            Ok((name.trim().to_string(), value.trim().to_string()))
        }
        _ => bail!("header must look like \"Name: value\", got {h:?}"),
    }
}

/// Parses sizes like `750`, `500K`, `1.5M` or `1G` (powers of 1024).
fn parse_size(s: &str) -> Result<u64, String> {
    let s = s.trim();
    let split = s
        .find(|c: char| !(c.is_ascii_digit() || c == '.'))
        .unwrap_or(s.len());
    let (num, unit) = s.split_at(split);
    let num: f64 = num.parse().map_err(|_| format!("not a size: {s:?}"))?;
    let mult = match unit
        .trim()
        .to_ascii_uppercase()
        .trim_end_matches("IB")
        .trim_end_matches('B')
    {
        "" => 1u64,
        "K" => 1 << 10,
        "M" => 1 << 20,
        "G" => 1 << 30,
        _ => return Err(format!("unknown unit in {s:?}; use K, M or G")),
    };
    Ok((num * mult as f64) as u64)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn sizes() {
        assert_eq!(parse_size("750"), Ok(750));
        assert_eq!(parse_size("500K"), Ok(500 * 1024));
        assert_eq!(parse_size("1.5M"), Ok(3 << 19));
        assert_eq!(parse_size("2mb"), Ok(2 << 20));
        assert_eq!(parse_size("1GiB"), Ok(1 << 30));
        assert!(parse_size("fast").is_err());
        assert!(parse_size("5T").is_err());
    }

    #[test]
    fn headers() {
        assert_eq!(
            parse_header("Cookie: a=b; c=d").unwrap(),
            ("Cookie".into(), "a=b; c=d".into())
        );
        assert!(parse_header("no colon").is_err());
    }
}
