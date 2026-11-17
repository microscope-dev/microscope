use std::{
    fs::{self, DirBuilder, File, OpenOptions},
    io::{self, Write as _},
    os::unix::fs::{DirBuilderExt as _, OpenOptionsExt as _, PermissionsExt as _},
    path::{Path, PathBuf},
};

use serde::{Deserialize, Serialize};

use super::wire::KeyPair;

const KEY: &str = "key";
const ENROLLMENT: &str = "agent.json";
const STATUS: &str = "status.json";
const MARKER: &str = "running";

#[derive(Debug)]
pub struct Store {
    dir: PathBuf,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct Enrollment {
    pub agent_id: i64,
    pub server: String,
}

#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct Status {
    pub server: String,
    pub agent_id: Option<i64>,
    pub fingerprint: Option<String>,
    pub state: String,
    pub since: i64,
    pub last_refresh: Option<i64>,
    pub last_error: Option<String>,
    pub pid: u32,
    pub running: bool,
}

#[derive(Debug, PartialEq, Eq)]
pub enum Stored<T> {
    Missing,
    Damaged,
    Found(T),
}

impl Store {
    pub fn open(dir: &Path) -> io::Result<Self> {
        DirBuilder::new().recursive(true).mode(0o700).create(dir)?;
        let mode = fs::metadata(dir)?.permissions().mode();
        if mode & 0o022 != 0 {
            return Err(io::Error::new(
                io::ErrorKind::PermissionDenied,
                format!(
                    "{} is writable by others (mode {:o}); the agent keeps its private key there",
                    dir.display(),
                    mode & 0o777
                ),
            ));
        }
        Ok(Self {
            dir: dir.to_path_buf(),
        })
    }

    #[must_use]
    pub fn dir(&self) -> &Path {
        &self.dir
    }

    fn read(&self, name: &str) -> io::Result<Option<String>> {
        match fs::read_to_string(self.dir.join(name)) {
            Ok(text) => Ok(Some(text)),
            Err(error) if error.kind() == io::ErrorKind::NotFound => Ok(None),
            Err(error) => Err(error),
        }
    }

    fn write(&self, name: &str, bytes: &[u8]) -> io::Result<()> {
        let temporary = self.dir.join(format!(".{name}.new"));
        let mut file = OpenOptions::new()
            .write(true)
            .create(true)
            .truncate(true)
            .mode(0o600)
            .open(&temporary)?;
        file.write_all(bytes)?;
        file.sync_all()?;
        drop(file);

        fs::rename(&temporary, self.dir.join(name))?;
        File::open(&self.dir)?.sync_all()
    }

    fn remove(&self, name: &str) -> io::Result<()> {
        match fs::remove_file(self.dir.join(name)) {
            Err(error) if error.kind() != io::ErrorKind::NotFound => Err(error),
            _ => Ok(()),
        }
    }

    pub fn key(&self) -> io::Result<Stored<KeyPair>> {
        Ok(self.read(KEY)?.map_or(Stored::Missing, |text| {
            KeyPair::decode(&text).map_or(Stored::Damaged, Stored::Found)
        }))
    }

    pub fn save_key(&self, key: &KeyPair) -> io::Result<()> {
        self.write(KEY, key.encode().as_bytes())
    }

    pub fn forget(&self) -> io::Result<()> {
        self.remove(ENROLLMENT)?;
        self.remove(KEY)
    }

    pub fn enrollment(&self) -> io::Result<Stored<Enrollment>> {
        Ok(self.read(ENROLLMENT)?.map_or(Stored::Missing, |text| {
            serde_json::from_str(&text).map_or(Stored::Damaged, Stored::Found)
        }))
    }

    pub fn save_enrollment(&self, enrollment: &Enrollment) -> io::Result<()> {
        self.write(ENROLLMENT, &serde_json::to_vec(enrollment)?)
    }

    pub fn status(&self) -> io::Result<Stored<Status>> {
        Ok(self.read(STATUS)?.map_or(Stored::Missing, |text| {
            serde_json::from_str(&text).map_or(Stored::Damaged, Stored::Found)
        }))
    }

    pub fn save_status(&self, status: &Status) -> io::Result<()> {
        self.write(STATUS, &serde_json::to_vec_pretty(status)?)
    }

    pub fn marker(&self) -> io::Result<Option<String>> {
        Ok(self.read(MARKER)?.map(|text| text.trim().to_owned()))
    }

    pub fn mark_running(&self, boot_id: &str) -> io::Result<()> {
        self.write(MARKER, boot_id.as_bytes())
    }

    pub fn mark_stopped(&self) -> io::Result<()> {
        self.remove(MARKER)
    }
}

#[cfg(test)]
mod tests;
