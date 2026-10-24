//! `slipstream-bridge`: the native messaging host browsers start for the
//! Slipstream extension. It hands each message to the running app, starting
//! the app first if needed.
//!
//! Run it with `--install` or `--uninstall` to register it with the current
//! user's browsers by hand (the app also does this every time it starts).

use std::io;
use std::path::{Path, PathBuf};
use std::process::{Command, ExitCode, Stdio};
use std::time::{Duration, Instant};

use serde_json::{Value, json};
use slipstream_bridge::{VERSION, data_dir, install, native, send};

const REPLY_TIMEOUT: Duration = Duration::from_secs(10);
/// How long to wait for the app to come up after starting it.
const LAUNCH_TIMEOUT: Duration = Duration::from_secs(15);

fn main() -> ExitCode {
    match std::env::args().nth(1).as_deref() {
        Some("--install") => report(install::install(&bridge_path())),
        Some("--uninstall") => report(install::uninstall()),
        Some("--help" | "-h") => {
            println!(
                "slipstream-bridge {VERSION}\n\nBrowsers start this program to talk to Slipstream.\n\n  --install     register with this user's browsers\n  --uninstall   remove the registration"
            );
            ExitCode::SUCCESS
        }
        // Browsers pass the extension's origin (Chrome) or the manifest
        // path and extension ID (Firefox).
        _ => serve(),
    }
}

fn report(result: io::Result<Vec<install::Registration>>) -> ExitCode {
    match result {
        Ok(done) if done.is_empty() => {
            println!("No supported browsers found.");
            ExitCode::SUCCESS
        }
        Ok(done) => {
            for r in done {
                println!("{:<20} {}", r.browser, r.location);
            }
            ExitCode::SUCCESS
        }
        Err(e) => {
            eprintln!("error: {e}");
            ExitCode::FAILURE
        }
    }
}

fn bridge_path() -> PathBuf {
    std::env::current_exe().unwrap_or_else(|_| PathBuf::from("slipstream-bridge"))
}

/// Answers messages until the browser closes the connection.
fn serve() -> ExitCode {
    let mut input = io::stdin().lock();
    let mut output = io::stdout().lock();
    loop {
        let message = match native::read_message(&mut input) {
            Ok(Some(message)) => message,
            Ok(None) => return ExitCode::SUCCESS,
            Err(e) => {
                eprintln!("slipstream-bridge: {e}");
                return ExitCode::FAILURE;
            }
        };
        let reply = handle(&message);
        if let Err(e) = native::write_message(&mut output, &reply) {
            eprintln!("slipstream-bridge: {e}");
            return ExitCode::FAILURE;
        }
    }
}

fn handle(message: &Value) -> Value {
    let Some(dir) = data_dir() else {
        return failure("could not find Slipstream's data folder");
    };
    // A status check should not start the app.
    let only_checking = message["type"] == "ping";
    match send(&dir, message, REPLY_TIMEOUT) {
        Ok(reply) => reply,
        Err(_) if only_checking => json!({ "ok": true, "appRunning": false, "version": VERSION }),
        Err(_) => match launch_and_send(&dir, message) {
            Ok(reply) => reply,
            Err(e) => failure(&format!("could not reach Slipstream: {e}")),
        },
    }
}

fn launch_and_send(dir: &Path, message: &Value) -> io::Result<Value> {
    launch_app()?;
    let started = Instant::now();
    loop {
        std::thread::sleep(Duration::from_millis(200));
        match send(dir, message, REPLY_TIMEOUT) {
            Ok(reply) => return Ok(reply),
            Err(e) if started.elapsed() > LAUNCH_TIMEOUT => return Err(e),
            Err(_) => {}
        }
    }
}

fn failure(error: &str) -> Value {
    json!({ "ok": false, "error": error, "version": VERSION })
}

/// Starts the app, which sits next to this program, without showing its
/// main window, and detached so the browser closing the bridge does not
/// take the app down too.
fn launch_app() -> io::Result<()> {
    let name = if cfg!(windows) {
        "slipstream-desktop.exe"
    } else {
        "slipstream-desktop"
    };
    let app = bridge_path().with_file_name(name);
    let mut command = Command::new(&app);
    command
        .arg("--background")
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::null());
    detach(&mut command)
        .map(drop)
        .map_err(|e| io::Error::new(e.kind(), format!("could not start {}: {e}", app.display())))
}

#[cfg(unix)]
fn detach(command: &mut Command) -> io::Result<std::process::Child> {
    use std::os::unix::process::CommandExt;
    command.process_group(0).spawn()
}

#[cfg(windows)]
fn detach(command: &mut Command) -> io::Result<std::process::Child> {
    use std::os::windows::process::CommandExt;
    const DETACHED_PROCESS: u32 = 0x0000_0008;
    const CREATE_NEW_PROCESS_GROUP: u32 = 0x0000_0200;
    const CREATE_BREAKAWAY_FROM_JOB: u32 = 0x0100_0000;
    // Browsers run hosts inside a job that ends with them; break away from
    // it when allowed.
    command
        .creation_flags(DETACHED_PROCESS | CREATE_NEW_PROCESS_GROUP | CREATE_BREAKAWAY_FROM_JOB)
        .spawn()
        .or_else(|_| {
            command
                .creation_flags(DETACHED_PROCESS | CREATE_NEW_PROCESS_GROUP)
                .spawn()
        })
}
