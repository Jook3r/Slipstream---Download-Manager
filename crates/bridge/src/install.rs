//! Registering the bridge with the current user's browsers. Chrome-family
//! browsers and Firefox each look for a small JSON manifest naming the
//! bridge program and the extension allowed to start it: in a folder of
//! their own on macOS and Linux, through the registry on Windows.

use std::io;
use std::path::Path;

use serde_json::{Value, json};

use crate::{CHROME_EXTENSION_ID, FIREFOX_EXTENSION_ID, HOST_NAME};

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Family {
    Chromium,
    Firefox,
}

/// A browser the bridge was registered with (or removed from).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Registration {
    pub browser: &'static str,
    pub location: String,
}

pub fn manifest(family: Family, bridge: &Path) -> Value {
    let mut manifest = json!({
        "name": HOST_NAME,
        "description": "Slipstream download manager",
        "path": bridge,
        "type": "stdio",
    });
    match family {
        Family::Chromium => {
            manifest["allowed_origins"] =
                json!([format!("chrome-extension://{CHROME_EXTENSION_ID}/")]);
        }
        Family::Firefox => manifest["allowed_extensions"] = json!([FIREFOX_EXTENSION_ID]),
    }
    manifest
}

/// Registers `bridge` with every supported browser of the current user.
pub fn install(bridge: &Path) -> io::Result<Vec<Registration>> {
    platform::install(bridge)
}

pub fn uninstall() -> io::Result<Vec<Registration>> {
    platform::uninstall()
}

#[cfg(not(windows))]
pub use platform::{install_in, uninstall_in};

#[cfg(not(windows))]
mod platform {
    use std::fs;
    use std::io;
    use std::path::Path;

    use super::{Family, Registration, manifest};
    use crate::HOST_NAME;

    /// Browser, folder that shows it is installed, folder for manifests
    /// (both relative to the home folder).
    #[cfg(target_os = "macos")]
    const BROWSERS: &[(&str, &str, &str, Family)] = &[
        (
            "Google Chrome",
            "Library/Application Support/Google/Chrome",
            "Library/Application Support/Google/Chrome/NativeMessagingHosts",
            Family::Chromium,
        ),
        (
            "Google Chrome Beta",
            "Library/Application Support/Google/Chrome Beta",
            "Library/Application Support/Google/Chrome Beta/NativeMessagingHosts",
            Family::Chromium,
        ),
        (
            "Chromium",
            "Library/Application Support/Chromium",
            "Library/Application Support/Chromium/NativeMessagingHosts",
            Family::Chromium,
        ),
        (
            "Microsoft Edge",
            "Library/Application Support/Microsoft Edge",
            "Library/Application Support/Microsoft Edge/NativeMessagingHosts",
            Family::Chromium,
        ),
        (
            "Brave",
            "Library/Application Support/BraveSoftware/Brave-Browser",
            "Library/Application Support/BraveSoftware/Brave-Browser/NativeMessagingHosts",
            Family::Chromium,
        ),
        (
            "Vivaldi",
            "Library/Application Support/Vivaldi",
            "Library/Application Support/Vivaldi/NativeMessagingHosts",
            Family::Chromium,
        ),
        (
            "Firefox",
            "Library/Application Support/Mozilla",
            "Library/Application Support/Mozilla/NativeMessagingHosts",
            Family::Firefox,
        ),
    ];

    #[cfg(not(target_os = "macos"))]
    const BROWSERS: &[(&str, &str, &str, Family)] = &[
        (
            "Google Chrome",
            ".config/google-chrome",
            ".config/google-chrome/NativeMessagingHosts",
            Family::Chromium,
        ),
        (
            "Google Chrome Beta",
            ".config/google-chrome-beta",
            ".config/google-chrome-beta/NativeMessagingHosts",
            Family::Chromium,
        ),
        (
            "Chromium",
            ".config/chromium",
            ".config/chromium/NativeMessagingHosts",
            Family::Chromium,
        ),
        (
            "Microsoft Edge",
            ".config/microsoft-edge",
            ".config/microsoft-edge/NativeMessagingHosts",
            Family::Chromium,
        ),
        (
            "Brave",
            ".config/BraveSoftware/Brave-Browser",
            ".config/BraveSoftware/Brave-Browser/NativeMessagingHosts",
            Family::Chromium,
        ),
        (
            "Vivaldi",
            ".config/vivaldi",
            ".config/vivaldi/NativeMessagingHosts",
            Family::Chromium,
        ),
        (
            "Firefox",
            ".mozilla",
            ".mozilla/native-messaging-hosts",
            Family::Firefox,
        ),
    ];

    pub fn install(bridge: &Path) -> io::Result<Vec<Registration>> {
        let home = dirs::home_dir().ok_or_else(|| io::Error::other("no home folder"))?;
        install_in(&home, bridge)
    }

    pub fn uninstall() -> io::Result<Vec<Registration>> {
        let home = dirs::home_dir().ok_or_else(|| io::Error::other("no home folder"))?;
        uninstall_in(&home)
    }

    /// Writes manifests for the browsers installed under `home`.
    pub fn install_in(home: &Path, bridge: &Path) -> io::Result<Vec<Registration>> {
        let mut done = Vec::new();
        for &(browser, profile, hosts, family) in BROWSERS {
            if !home.join(profile).is_dir() {
                continue;
            }
            let dir = home.join(hosts);
            fs::create_dir_all(&dir)?;
            let file = dir.join(format!("{HOST_NAME}.json"));
            let body =
                serde_json::to_vec_pretty(&manifest(family, bridge)).map_err(io::Error::other)?;
            if fs::read(&file).ok().as_deref() != Some(body.as_slice()) {
                fs::write(&file, body)?;
            }
            done.push(Registration {
                browser,
                location: file.display().to_string(),
            });
        }
        Ok(done)
    }

    pub fn uninstall_in(home: &Path) -> io::Result<Vec<Registration>> {
        let mut done = Vec::new();
        for &(browser, _, hosts, _) in BROWSERS {
            let file = home.join(hosts).join(format!("{HOST_NAME}.json"));
            if file.exists() {
                fs::remove_file(&file)?;
                done.push(Registration {
                    browser,
                    location: file.display().to_string(),
                });
            }
        }
        Ok(done)
    }
}

#[cfg(windows)]
mod platform {
    use std::fs;
    use std::io;
    use std::path::Path;

    use winreg::RegKey;
    use winreg::enums::HKEY_CURRENT_USER;

    use super::{Family, Registration, manifest};
    use crate::HOST_NAME;

    const BROWSERS: &[(&str, &str, Family)] = &[
        (
            "Google Chrome",
            r"Software\Google\Chrome\NativeMessagingHosts",
            Family::Chromium,
        ),
        (
            "Chromium",
            r"Software\Chromium\NativeMessagingHosts",
            Family::Chromium,
        ),
        (
            "Microsoft Edge",
            r"Software\Microsoft\Edge\NativeMessagingHosts",
            Family::Chromium,
        ),
        (
            "Brave",
            r"Software\BraveSoftware\Brave-Browser\NativeMessagingHosts",
            Family::Chromium,
        ),
        (
            "Vivaldi",
            r"Software\Vivaldi\NativeMessagingHosts",
            Family::Chromium,
        ),
        (
            "Firefox",
            r"Software\Mozilla\NativeMessagingHosts",
            Family::Firefox,
        ),
    ];

    /// The manifests live in the app's data folder; the registry points at them.
    pub fn install(bridge: &Path) -> io::Result<Vec<Registration>> {
        let dir = crate::data_dir()
            .ok_or_else(|| io::Error::other("no data folder"))?
            .join("native-messaging");
        fs::create_dir_all(&dir)?;
        let hkcu = RegKey::predef(HKEY_CURRENT_USER);
        let mut done = Vec::new();
        for &(browser, key, family) in BROWSERS {
            let file = dir.join(match family {
                Family::Chromium => "chromium.json",
                Family::Firefox => "firefox.json",
            });
            let body =
                serde_json::to_vec_pretty(&manifest(family, bridge)).map_err(io::Error::other)?;
            fs::write(&file, body)?;
            let (entry, _) = hkcu.create_subkey(format!(r"{key}\{HOST_NAME}"))?;
            entry.set_value("", &file.display().to_string())?;
            done.push(Registration {
                browser,
                location: format!(r"HKCU\{key}\{HOST_NAME}"),
            });
        }
        Ok(done)
    }

    pub fn uninstall() -> io::Result<Vec<Registration>> {
        let hkcu = RegKey::predef(HKEY_CURRENT_USER);
        let mut done = Vec::new();
        for &(browser, key, _) in BROWSERS {
            let path = format!(r"{key}\{HOST_NAME}");
            if hkcu.delete_subkey_all(&path).is_ok() {
                done.push(Registration {
                    browser,
                    location: format!(r"HKCU\{path}"),
                });
            }
        }
        if let Some(dir) = crate::data_dir() {
            let _ = fs::remove_dir_all(dir.join("native-messaging"));
        }
        Ok(done)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn manifests_name_the_right_extension() {
        let bridge = Path::new("/opt/slipstream/slipstream-bridge");
        let chrome = manifest(Family::Chromium, bridge);
        assert_eq!(chrome["name"], HOST_NAME);
        assert_eq!(chrome["type"], "stdio");
        assert_eq!(chrome["path"], "/opt/slipstream/slipstream-bridge");
        assert_eq!(
            chrome["allowed_origins"][0],
            "chrome-extension://lfnhebbceoknhndfahdlmlimifinjkam/"
        );
        assert!(chrome.get("allowed_extensions").is_none());
        let firefox = manifest(Family::Firefox, bridge);
        assert_eq!(
            firefox["allowed_extensions"][0],
            "slipstream@jook3r.github.io"
        );
        assert!(firefox.get("allowed_origins").is_none());
    }

    #[cfg(target_os = "linux")]
    #[test]
    fn installs_only_for_browsers_that_exist() {
        let home = tempfile::tempdir().unwrap();
        std::fs::create_dir_all(home.path().join(".config/chromium")).unwrap();
        std::fs::create_dir_all(home.path().join(".mozilla")).unwrap();
        let bridge = Path::new("/usr/bin/slipstream-bridge");

        let done = install_in(home.path(), bridge).unwrap();

        let browsers: Vec<_> = done.iter().map(|r| r.browser).collect();
        assert_eq!(browsers, ["Chromium", "Firefox"]);
        let chrome: Value = serde_json::from_slice(
            &std::fs::read(
                home.path()
                    .join(".config/chromium/NativeMessagingHosts/io.github.jook3r.slipstream.json"),
            )
            .unwrap(),
        )
        .unwrap();
        assert_eq!(chrome["path"], "/usr/bin/slipstream-bridge");
        assert!(
            home.path()
                .join(".mozilla/native-messaging-hosts/io.github.jook3r.slipstream.json")
                .exists()
        );
        assert!(!home.path().join(".config/google-chrome").exists());

        let removed = uninstall_in(home.path()).unwrap();
        assert_eq!(removed.len(), 2);
        assert!(uninstall_in(home.path()).unwrap().is_empty());
    }
}
