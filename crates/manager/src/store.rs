//! Small JSON files in the app's data folder.

use std::fs::{self, File};
use std::io::{self, Write};
use std::path::PathBuf;

use serde::Serialize;
use serde::de::DeserializeOwned;

pub struct Store {
    dir: PathBuf,
}

impl Store {
    pub fn new(dir: PathBuf) -> Self {
        Self { dir }
    }

    /// Reads `name`, or returns the default if it is missing. A corrupt file
    /// is set aside as `name.bad` rather than stopping the app from starting.
    pub fn load<T: DeserializeOwned + Default>(&self, name: &str) -> T {
        let path = self.dir.join(name);
        let Ok(bytes) = fs::read(&path) else {
            return T::default();
        };
        serde_json::from_slice(&bytes).unwrap_or_else(|_| {
            let _ = fs::rename(&path, self.dir.join(format!("{name}.bad")));
            T::default()
        })
    }

    /// Writes through a temporary file so a crash never leaves half a file.
    pub fn save<T: Serialize>(&self, name: &str, value: &T) -> io::Result<()> {
        fs::create_dir_all(&self.dir)?;
        let tmp = self.dir.join(format!("{name}.tmp"));
        let mut file = File::create(&tmp)?;
        file.write_all(&serde_json::to_vec_pretty(value).map_err(io::Error::other)?)?;
        file.sync_all()?;
        drop(file);
        fs::rename(&tmp, self.dir.join(name))
    }
}
