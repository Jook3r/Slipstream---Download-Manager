//! Putting the computer to sleep or turning it off when the download queue
//! is done, as IDM's scheduler can.

use std::io;
use std::process::Command;

use slipstream_manager::WhenDone;

/// Sleeps or shuts down; anything else does nothing here.
pub fn act(when_done: WhenDone) -> io::Result<()> {
    match when_done {
        WhenDone::Sleep => sleep(),
        WhenDone::Shutdown => shut_down(),
        WhenDone::Nothing | WhenDone::Exit => Ok(()),
    }
}

#[cfg(windows)]
fn sleep() -> io::Result<()> {
    // Not hibernate, and let wake-up timers work.
    // SAFETY: plain call into powrprof.dll with no pointers involved.
    let ok = unsafe { windows_sys::Win32::System::Power::SetSuspendState(false, false, false) };
    if ok {
        Ok(())
    } else {
        Err(io::Error::last_os_error())
    }
}

#[cfg(windows)]
fn shut_down() -> io::Result<()> {
    use std::os::windows::process::CommandExt;
    run(Command::new("shutdown")
        .args(["/s", "/t", "0"])
        .creation_flags(0x0800_0000))
}

#[cfg(target_os = "macos")]
fn sleep() -> io::Result<()> {
    run(Command::new("pmset").arg("sleepnow"))
}

#[cfg(target_os = "macos")]
fn shut_down() -> io::Result<()> {
    // Asks the login window to shut down, as the Apple menu does.
    run(Command::new("osascript").args([
        "-e",
        r#"tell application "loginwindow" to «event aevtrsdn»"#,
    ]))
}

#[cfg(all(unix, not(target_os = "macos")))]
fn sleep() -> io::Result<()> {
    run(Command::new("systemctl").arg("suspend"))
}

#[cfg(all(unix, not(target_os = "macos")))]
fn shut_down() -> io::Result<()> {
    run(Command::new("systemctl").arg("poweroff"))
}

fn run(cmd: &mut Command) -> io::Result<()> {
    let status = cmd.status()?;
    if status.success() {
        Ok(())
    } else {
        Err(io::Error::other(format!("{cmd:?} failed with {status}")))
    }
}
