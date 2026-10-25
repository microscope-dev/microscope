//! The outbox: the directory of recordings waiting to be uploaded.
//!
//! Each recording waits as `KEY.usprof`, where KEY is its upload key in hex,
//! with its name for people in `KEY.name`. Once the service has it, the agent
//! writes the recording's URL to `KEY.url` and deletes the recording. A
//! recording the service refuses for good is kept as `KEY.usprof.failed`,
//! with the reason in `KEY.error`. Files are written under a dotted partial
//! name and renamed, so the agent never reads one half written.

use std::collections::BTreeSet;
use std::io::Write as _;
use std::os::unix::fs::{DirBuilderExt as _, OpenOptionsExt as _};
use std::path::{Path, PathBuf};
use std::time::{Duration, SystemTime};

use anyhow::{Context as _, Result};

use crate::cli::config;

/// How long a recording's URL stays in the outbox, for `record --upload` to
/// read.
const KEEP_URLS: Duration = Duration::from_hours(24);

/// An upload key: 16 random bytes, the same for every attempt at one file.
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct Key(pub [u8; 16]);

impl Key {
    pub fn random() -> Result<Self> {
        let mut key = [0; 16];
        getrandom::fill(&mut key)
            .map_err(|error| anyhow::anyhow!("choosing an upload key: {error}"))?;
        Ok(Self(key))
    }

    fn parse(text: &str) -> Option<Self> {
        if text.len() != 32 {
            return None;
        }
        let mut key = [0; 16];
        let (pairs, _) = text.as_bytes().as_chunks::<2>();
        for (byte, pair) in key.iter_mut().zip(pairs) {
            let pair = std::str::from_utf8(pair).ok()?;
            *byte = u8::from_str_radix(pair, 16).ok()?;
        }
        Some(Self(key))
    }

    pub fn from_slice(bytes: &[u8]) -> Option<Self> {
        bytes.try_into().ok().map(Self)
    }
}

impl std::fmt::Display for Key {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        self.0.iter().try_for_each(|byte| write!(f, "{byte:02x}"))
    }
}

#[derive(Clone, Debug)]
pub struct Outbox {
    dir: PathBuf,
}

impl Outbox {
    /// The outbox at `dir`, created, private to the user, if it is missing.
    pub fn open(dir: &Path) -> Result<Self> {
        std::fs::DirBuilder::new()
            .recursive(true)
            .mode(0o700)
            .create(dir)
            .with_context(|| format!("creating {}", dir.display()))?;
        Ok(Self {
            dir: dir.to_owned(),
        })
    }

    pub fn recording(&self, key: Key) -> PathBuf {
        self.dir.join(format!("{key}.usprof"))
    }

    /// Key replacement must not redirect queued or in-progress recordings.
    pub fn bind_destination(&self, endpoint: &str, project: u64) -> Result<()> {
        let destination = format!("{endpoint}\n{project}\n");
        match std::fs::read_to_string(self.dir.join("destination")) {
            Ok(previous) => anyhow::ensure!(
                previous == destination,
                "the outbox belongs to a different service or project; use a separate XDG_STATE_HOME for another destination"
            ),
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => {
                let partial = self
                    .dir
                    .join(format!(".destination-{}.partial", Key::random()?));
                let result = (|| -> std::io::Result<()> {
                    let mut file = std::fs::OpenOptions::new()
                        .write(true)
                        .create_new(true)
                        .mode(0o600)
                        .open(&partial)?;
                    file.write_all(destination.as_bytes())?;
                    file.sync_all()?;
                    std::fs::rename(&partial, self.dir.join("destination"))
                })();
                if result.is_err() {
                    let _ = std::fs::remove_file(&partial);
                }
                result.context("writing the outbox destination")?;
            }
            Err(error) => return Err(error).context("reading the outbox destination"),
        }
        // Also retry a directory sync that failed after a previous rename.
        std::fs::File::open(&self.dir)
            .and_then(|directory| directory.sync_all())
            .context("persisting the outbox destination")?;
        Ok(())
    }

    /// Where a recording is written before [`Outbox::commit`] queues it.
    pub fn partial(&self, key: Key) -> PathBuf {
        self.dir.join(format!(".{key}.usprof.partial"))
    }

    /// Queues the recording written to [`Outbox::partial`].
    pub fn commit(&self, key: Key, name: &str) -> Result<()> {
        self.write(&format!("{key}.name"), name)?;
        let recording = self.recording(key);
        std::fs::rename(self.partial(key), &recording)
            .with_context(|| format!("writing {}", recording.display()))
    }

    /// Queues a copy of the recording at `path`.
    pub fn add(&self, path: &Path, name: &str) -> Result<Key> {
        let key = Key::random()?;
        let partial = self.partial(key);
        // A hard link costs nothing, but crosses no file system.
        if std::fs::hard_link(path, &partial).is_err() {
            std::fs::copy(path, &partial)
                .with_context(|| format!("copying {} to {}", path.display(), partial.display()))?;
        }
        self.commit(key, name)?;
        Ok(key)
    }

    /// The recordings waiting, oldest first.
    pub fn pending(&self) -> Result<Vec<Key>> {
        let mut pending = Vec::new();
        let entries = std::fs::read_dir(&self.dir)
            .with_context(|| format!("reading {}", self.dir.display()))?;
        for entry in entries {
            let entry = entry.with_context(|| format!("reading {}", self.dir.display()))?;
            let name = entry.file_name();
            let Some(key) = name
                .to_str()
                .and_then(|name| name.strip_suffix(".usprof"))
                .and_then(Key::parse)
            else {
                continue;
            };
            let modified = entry
                .metadata()
                .and_then(|metadata| metadata.modified())
                .unwrap_or(SystemTime::UNIX_EPOCH);
            pending.push((modified, key));
        }
        pending.sort();
        Ok(pending.into_iter().map(|(_, key)| key).collect())
    }

    /// The recording's name for people, or its key's if it has none.
    pub fn name(&self, key: Key) -> String {
        std::fs::read_to_string(self.dir.join(format!("{key}.name")))
            .map(|name| name.trim().to_owned())
            .ok()
            .filter(|name| !name.is_empty())
            .unwrap_or_else(|| key.to_string())
    }

    /// Records that the service stored the recording at `url`, and deletes
    /// the local copy.
    pub fn finish(&self, key: Key, url: &str) -> Result<()> {
        self.write(&format!("{key}.url"), url)?;
        self.remove(key)
    }

    /// Keeps a recording the service refused for good aside, with why.
    pub fn fail(&self, key: Key, why: &str) -> Result<()> {
        self.write(&format!("{key}.error"), why)?;
        let failed = self.dir.join(format!("{key}.usprof.failed"));
        std::fs::rename(self.recording(key), &failed)
            .with_context(|| format!("writing {}", failed.display()))?;
        let _ = std::fs::remove_file(self.dir.join(format!("{key}.name")));
        Ok(())
    }

    fn remove(&self, key: Key) -> Result<()> {
        let recording = self.recording(key);
        match std::fs::remove_file(&recording) {
            Ok(()) => {}
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => {}
            Err(error) => {
                return Err(error).with_context(|| format!("deleting {}", recording.display()));
            }
        }
        let _ = std::fs::remove_file(self.dir.join(format!("{key}.name")));
        Ok(())
    }

    /// The URL of a recording the service stored, once it has.
    pub fn url(&self, key: Key) -> Option<String> {
        read_line(&self.dir.join(format!("{key}.url")))
    }

    /// Why the service refused a recording, if it did.
    pub fn error(&self, key: Key) -> Option<String> {
        read_line(&self.dir.join(format!("{key}.error")))
    }

    /// Deletes the URLs of recordings stored more than a day ago.
    pub fn prune(&self) {
        let Ok(entries) = std::fs::read_dir(&self.dir) else {
            return;
        };
        let now = SystemTime::now();
        for entry in entries.flatten() {
            let is_url = Path::new(&entry.file_name())
                .extension()
                .is_some_and(|extension| extension == "url");
            let old = entry
                .metadata()
                .and_then(|metadata| metadata.modified())
                .is_ok_and(|modified| now.duration_since(modified).unwrap_or_default() > KEEP_URLS);
            if is_url && old {
                let _ = std::fs::remove_file(entry.path());
            }
        }
    }

    fn write(&self, name: &str, text: &str) -> Result<()> {
        config::write_atomically(&self.dir.join(name), text).map_err(anyhow::Error::msg)
    }
}

fn read_line(path: &Path) -> Option<String> {
    std::fs::read_to_string(path)
        .ok()
        .map(|text| text.trim().to_owned())
}

/// A directory of `record --continuous` segments, each queued once. The
/// segments stay where they are; the outbox gets copies. Which segments
/// were queued is kept in a file of the agent's own, so a restart queues
/// none twice.
#[derive(Debug)]
pub struct Watched {
    dir: PathBuf,
    queued: PathBuf,
    name: String,
}

impl Watched {
    /// `dir`, remembered in `state`.
    pub fn new(dir: &Path, state: &Path) -> Self {
        let canonical = dir.canonicalize().unwrap_or_else(|_| dir.to_owned());
        let digest = twox_hash::XxHash3_64::oneshot(canonical.as_os_str().as_encoded_bytes());
        let name = canonical.file_name().map_or_else(
            || "segments".to_owned(),
            |name| name.to_string_lossy().into_owned(),
        );
        Self {
            dir: dir.to_owned(),
            queued: state.join(format!("{digest:016x}")),
            name,
        }
    }

    pub fn dir(&self) -> &Path {
        &self.dir
    }

    /// Queues the segments written since the last scan, returning how many.
    pub fn scan(&self, outbox: &Outbox) -> Result<usize> {
        let mut segments = BTreeSet::new();
        let entries = match std::fs::read_dir(&self.dir) {
            Ok(entries) => entries,
            // The recording may not have started yet.
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(0),
            Err(error) => {
                return Err(error).with_context(|| format!("reading {}", self.dir.display()));
            }
        };
        for entry in entries {
            let entry = entry.with_context(|| format!("reading {}", self.dir.display()))?;
            let Some(name) = entry.file_name().to_str().map(str::to_owned) else {
                continue;
            };
            // A segment being written has a dotted partial name.
            if name.ends_with(".usprof") && !name.starts_with('.') {
                segments.insert(name);
            }
        }

        let queued: BTreeSet<String> = std::fs::read_to_string(&self.queued)
            .unwrap_or_default()
            .lines()
            .map(str::to_owned)
            .collect();

        let mut added = 0;
        for segment in segments.difference(&queued) {
            outbox.add(&self.dir.join(segment), &self.name)?;
            added += 1;
        }

        // Segments the recording deleted are forgotten, so the file stays
        // as small as the directory.
        if added > 0 || queued.len() != segments.len() {
            let mut text = String::new();
            for segment in &segments {
                text.push_str(segment);
                text.push('\n');
            }
            if let Some(parent) = self.queued.parent() {
                std::fs::create_dir_all(parent)
                    .with_context(|| format!("creating {}", parent.display()))?;
            }
            config::write_atomically(&self.queued, &text).map_err(anyhow::Error::msg)?;
        }
        Ok(added)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    struct Scratch(PathBuf);

    impl Scratch {
        fn new(name: &str) -> Self {
            let path = std::env::temp_dir()
                .join(format!("microscope-outbox-{name}-{}", std::process::id()));
            let _ = std::fs::remove_dir_all(&path);
            std::fs::create_dir_all(&path).unwrap();
            Self(path)
        }
    }

    impl Drop for Scratch {
        fn drop(&mut self) {
            let _ = std::fs::remove_dir_all(&self.0);
        }
    }

    #[test]
    fn keys_print_and_parse_as_hex() {
        let key = Key([0xab; 16]);
        assert_eq!(key.to_string(), "ab".repeat(16));
        assert_eq!(Key::parse(&key.to_string()), Some(key));
        assert_eq!(Key::parse("ab"), None);
        assert_eq!(Key::parse(&"zz".repeat(16)), None);
    }

    #[test]
    fn recordings_wait_until_finished() {
        let scratch = Scratch::new("finish");
        let outbox = Outbox::open(&scratch.0.join("outbox")).unwrap();
        let source = scratch.0.join("profile.usprof");
        std::fs::write(&source, b"capture").unwrap();

        let key = outbox.add(&source, "server").unwrap();
        assert_eq!(outbox.pending().unwrap(), [key]);
        assert_eq!(outbox.name(key), "server");
        assert_eq!(std::fs::read(outbox.recording(key)).unwrap(), b"capture");
        assert!(source.exists());

        outbox
            .finish(key, "https://example.com/recordings/1")
            .unwrap();
        assert!(outbox.pending().unwrap().is_empty());
        assert_eq!(
            outbox.url(key).as_deref(),
            Some("https://example.com/recordings/1")
        );
        assert!(!outbox.recording(key).exists());
    }

    #[test]
    fn refused_recordings_are_kept_aside() {
        let scratch = Scratch::new("fail");
        let outbox = Outbox::open(&scratch.0).unwrap();
        let source = scratch.0.join("profile");
        std::fs::write(&source, b"capture").unwrap();
        let key = outbox.add(&source, "server").unwrap();

        outbox.fail(key, "over quota").unwrap();
        assert!(outbox.pending().unwrap().is_empty());
        assert_eq!(outbox.error(key).as_deref(), Some("over quota"));
        assert!(scratch.0.join(format!("{key}.usprof.failed")).exists());
    }

    #[test]
    fn partial_and_other_files_are_not_pending() {
        let scratch = Scratch::new("partial");
        let outbox = Outbox::open(&scratch.0).unwrap();
        let key = Key([1; 16]);
        std::fs::write(outbox.partial(key), b"half").unwrap();
        std::fs::write(scratch.0.join("notes.usprof"), b"x").unwrap();
        assert!(outbox.pending().unwrap().is_empty());

        outbox.commit(key, "").unwrap();
        assert_eq!(outbox.pending().unwrap(), [key]);
        assert_eq!(outbox.name(key), key.to_string());
    }

    #[test]
    fn watched_segments_are_queued_once() {
        let scratch = Scratch::new("watch");
        let outbox = Outbox::open(&scratch.0.join("outbox")).unwrap();
        let segments = scratch.0.join("server-segments");
        std::fs::create_dir_all(&segments).unwrap();
        std::fs::write(segments.join("0000000000000001.usprof"), b"one").unwrap();
        std::fs::write(segments.join(".0000000000000002.usprof.partial"), b"half").unwrap();

        let watched = Watched::new(&segments, &scratch.0.join("watched"));
        assert_eq!(watched.scan(&outbox).unwrap(), 1);
        assert_eq!(watched.scan(&outbox).unwrap(), 0);

        // So is each one after a restart.
        std::fs::write(segments.join("0000000000000002.usprof"), b"two").unwrap();
        let watched = Watched::new(&segments, &scratch.0.join("watched"));
        assert_eq!(watched.scan(&outbox).unwrap(), 1);

        let pending = outbox.pending().unwrap();
        assert_eq!(pending.len(), 2);
        assert!(
            pending
                .iter()
                .all(|&key| outbox.name(key) == "server-segments")
        );
        assert!(segments.join("0000000000000001.usprof").exists());
    }

    #[test]
    fn missing_watched_directories_hold_nothing_yet() {
        let scratch = Scratch::new("watch-missing");
        let outbox = Outbox::open(&scratch.0.join("outbox")).unwrap();
        let watched = Watched::new(&scratch.0.join("later"), &scratch.0.join("watched"));
        assert_eq!(watched.scan(&outbox).unwrap(), 0);
    }
}
