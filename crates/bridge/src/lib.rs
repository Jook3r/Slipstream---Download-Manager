//! How the browser extension reaches the Slipstream app.
//!
//! The browser starts `slipstream-bridge` as a native messaging host and
//! talks to it over stdin/stdout. The bridge passes each message on to the
//! running app over a local TCP connection whose port and secret token the
//! app writes to `ipc.json` in its data folder, starting the app first if
//! needed. This crate holds both ends' shared pieces, plus the code that
//! registers the bridge with installed browsers.

pub mod install;
pub mod native;

use std::fs;
use std::io::{self, BufRead, BufReader, Write};
use std::net::{Ipv4Addr, SocketAddr, TcpStream};
use std::path::{Path, PathBuf};
use std::time::Duration;

use serde::{Deserialize, Serialize};
use serde_json::Value;

/// The app's identifier; also names its data folder.
pub const APP_ID: &str = "io.github.jook3r.slipstream";
/// The name browsers know the bridge by.
pub const HOST_NAME: &str = "io.github.jook3r.slipstream";
/// Chrome/Edge extension ID, fixed by the `key` in the extension's manifest.
pub const CHROME_EXTENSION_ID: &str = "lfnhebbceoknhndfahdlmlimifinjkam";
/// Firefox extension ID, set in the extension's manifest.
pub const FIREFOX_EXTENSION_ID: &str = "slipstream@jook3r.github.io";
pub const VERSION: &str = env!("CARGO_PKG_VERSION");

const ENDPOINT_FILE: &str = "ipc.json";

/// The app's data folder, the same one Tauri uses for `app_data_dir`.
pub fn data_dir() -> Option<PathBuf> {
    dirs::data_dir().map(|d| d.join(APP_ID))
}

/// Where the running app listens, published in its data folder.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Endpoint {
    pub port: u16,
    pub token: String,
    pub pid: u32,
}

impl Endpoint {
    pub fn read(data_dir: &Path) -> Option<Self> {
        serde_json::from_slice(&fs::read(data_dir.join(ENDPOINT_FILE)).ok()?).ok()
    }

    /// Writes the file readable by the current user only.
    pub fn write(&self, data_dir: &Path) -> io::Result<()> {
        fs::create_dir_all(data_dir)?;
        let path = data_dir.join(ENDPOINT_FILE);
        let tmp = data_dir.join(format!("{ENDPOINT_FILE}.tmp"));
        let mut options = fs::OpenOptions::new();
        options.write(true).create(true).truncate(true);
        #[cfg(unix)]
        std::os::unix::fs::OpenOptionsExt::mode(&mut options, 0o600);
        let mut file = options.open(&tmp)?;
        file.write_all(&serde_json::to_vec(self).map_err(io::Error::other)?)?;
        drop(file);
        fs::rename(tmp, path)
    }

    pub fn remove(data_dir: &Path) {
        let _ = fs::remove_file(data_dir.join(ENDPOINT_FILE));
    }
}

/// What travels over the local connection: one JSON line each way.
#[derive(Debug, Serialize, Deserialize)]
pub struct Envelope {
    pub token: String,
    /// A message from the extension, passed through as is.
    pub request: Value,
}

/// Sends one message to the running app and returns its reply.
pub fn send(data_dir: &Path, request: &Value, timeout: Duration) -> io::Result<Value> {
    let endpoint = Endpoint::read(data_dir)
        .ok_or_else(|| io::Error::new(io::ErrorKind::NotFound, "Slipstream is not running"))?;
    let addr = SocketAddr::from((Ipv4Addr::LOCALHOST, endpoint.port));
    let mut stream = TcpStream::connect_timeout(&addr, timeout)?;
    stream.set_read_timeout(Some(timeout))?;
    stream.set_write_timeout(Some(timeout))?;
    let envelope = Envelope {
        token: endpoint.token,
        request: request.clone(),
    };
    let mut line = serde_json::to_vec(&envelope).map_err(io::Error::other)?;
    line.push(b'\n');
    stream.write_all(&line)?;
    let mut reply = String::new();
    BufReader::new(stream).read_line(&mut reply)?;
    serde_json::from_str(&reply).map_err(io::Error::other)
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::net::TcpListener;
    use std::thread;

    #[test]
    fn endpoint_round_trips() {
        let dir = tempfile::tempdir().unwrap();
        assert_eq!(Endpoint::read(dir.path()), None);
        let endpoint = Endpoint {
            port: 4242,
            token: "secret".into(),
            pid: 7,
        };
        endpoint.write(dir.path()).unwrap();
        assert_eq!(Endpoint::read(dir.path()), Some(endpoint));
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            let mode = fs::metadata(dir.path().join(ENDPOINT_FILE))
                .unwrap()
                .permissions()
                .mode();
            assert_eq!(mode & 0o777, 0o600);
        }
        Endpoint::remove(dir.path());
        assert_eq!(Endpoint::read(dir.path()), None);
    }

    #[test]
    fn sends_request_with_token() {
        let dir = tempfile::tempdir().unwrap();
        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        Endpoint {
            port: listener.local_addr().unwrap().port(),
            token: "t0k3n".into(),
            pid: 1,
        }
        .write(dir.path())
        .unwrap();
        let server = thread::spawn(move || {
            let (stream, _) = listener.accept().unwrap();
            let mut line = String::new();
            let mut reader = BufReader::new(stream.try_clone().unwrap());
            reader.read_line(&mut line).unwrap();
            let envelope: Envelope = serde_json::from_str(&line).unwrap();
            let mut stream = stream;
            stream.write_all(b"{\"ok\":true}\n").unwrap();
            envelope
        });

        let reply = send(
            dir.path(),
            &serde_json::json!({"type": "ping"}),
            Duration::from_secs(5),
        )
        .unwrap();

        assert_eq!(reply, serde_json::json!({"ok": true}));
        let envelope = server.join().unwrap();
        assert_eq!(envelope.token, "t0k3n");
        assert_eq!(envelope.request["type"], "ping");
    }

    #[test]
    fn says_when_app_is_not_running() {
        let dir = tempfile::tempdir().unwrap();
        let err = send(dir.path(), &Value::Null, Duration::from_secs(1)).unwrap_err();
        assert_eq!(err.kind(), io::ErrorKind::NotFound);
    }
}
