# Slipstream

A multi-connection download manager for Windows, macOS and Linux, modelled on
Internet Download Manager's engine. This is phase 1: the download engine and a
command-line tool. A desktop app and browser extension come next.

## Usage

```sh
slipstream https://example.com/big-file.zip                 # save in the current folder
slipstream -d ~/Downloads -c 16 https://example.com/x.iso   # folder, 16 connections
slipstream -o movie.mp4 --limit 2M https://example.com/v    # exact path, 2 MiB/s cap
slipstream -H "Cookie: session=abc" --referer https://site/page https://site/file
```

Press **Ctrl+C** to pause. Run the same command again to resume where it left
off. Run `slipstream --help` for every option (proxy, retries, timeouts, user agent).

## How it works

**Dynamic segmentation.** The file is not cut into fixed pieces up front.
When a connection becomes free it takes over the back half of whichever piece
has the most bytes left, so connections that finish early help slow ones
instead of sitting idle. When a connection reaches the end of its piece and
the neighbouring piece's connection has not received anything yet, it takes
that piece over on the same HTTP response, which saves a round trip. This
follows IDM's published description of its engine.

**Resume.** Each connection writes straight into `name.part` at its own
offset. Every 5 seconds, and on pause, the data is flushed to disk and the
piece positions are saved to `name.slipstream`. After a crash or power cut, the
download continues from the last save. If the server's file has changed
(different size, ETag or Last-Modified), the download starts over instead of
mixing two versions. Every request also carries `If-Range`, so a change in the
middle of a download is caught too.

**Falls back gracefully.** Servers that don't support byte ranges, or don't
report a size, get a single connection. Dropped connections are retried with
exponential backoff. Servers that limit connections end up with as many as
they allow.

**HTTP/1.1 on purpose.** HTTP/2 would multiplex every range request over one
TCP connection, which defeats the point of using several.

## Layout

| Path | What |
|---|---|
| `crates/engine` | The library: reusable by the future desktop app |
| `crates/engine/src/segments.rs` | Dynamic segmentation (pure logic, unit tested) |
| `crates/engine/src/download.rs` | Connections, resume, retries, file handling |
| `crates/engine/src/probe.rs` | Size/range detection, file names from headers or URLs |
| `crates/engine/tests/download.rs` | End-to-end tests against a local misbehaving server |
| `crates/cli` | The `slipstream` command |

## Building

Install Rust from <https://rustup.rs>, then:

```sh
cargo build --release      # binary in target/release/slipstream (slipstream.exe on Windows)
cargo test --workspace
```

The GitHub Actions workflow tests and builds on Windows, macOS and Linux and
uploads the binaries as artifacts.

## Roadmap

1. ~~Engine: dynamic segmentation, resume, retries, speed limit~~ (this)
2. Desktop app (Tauri): download list, queue, categories, tray icon
3. Browser extension (Chrome/Edge/Firefox, Manifest V3) + native messaging
4. Video stream detection (HLS/DASH), ffmpeg muxing, scheduler, clipboard watch
5. Installers (MSI, DMG), auto-update
