use std::collections::HashMap;
use std::ffi::OsStr;
use std::fmt;
use std::io::{Read as _, Write as _};
use std::os::unix::fs::{DirBuilderExt as _, MetadataExt as _, OpenOptionsExt as _};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex, OnceLock, PoisonError, Weak};

use crate::image::backing::FileStamp;
use crate::image::{AlignedBytes, Image, ImageError, Limits};

pub const DIRECTORY_VARIABLE: &str = "MICROSCOPE_CACHE_DIR";

pub const DEFAULT_CAPACITY: u64 = crate::image::backing::MAX_INPUT;

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Setting {
    Off,

    Directory(PathBuf),
}

#[derive(Debug, thiserror::Error)]
pub enum CacheError {
    #[error("cannot use the image cache in {path}: {error}")]
    Open {
        path: PathBuf,

        #[source]
        error: std::io::Error,
    },

    #[error("the image cache is already {running}, so it cannot be {requested}")]
    AlreadyConfigured { running: String, requested: String },
}

static CACHE: OnceLock<Option<ImageCache>> = OnceLock::new();

pub fn resolve(
    setting: Option<Setting>,
    variable: Option<&OsStr>,
    xdg: Option<&OsStr>,
    home: Option<&OsStr>,
    under_test: bool,
) -> Option<PathBuf> {
    match setting {
        Some(Setting::Off) => return None,
        Some(Setting::Directory(path)) => return Some(path),
        None => {}
    }

    if let Some(variable) = variable {
        return (!variable.is_empty()).then(|| PathBuf::from(variable));
    }

    if under_test {
        return None;
    }

    let base = xdg
        .filter(|xdg| Path::new(xdg).is_absolute())
        .map(PathBuf::from)
        .or_else(|| home.map(|home| Path::new(home).join(".cache")))?;
    Some(base.join("microscope").join("images"))
}

fn resolve_here(setting: Option<Setting>) -> Option<PathBuf> {
    resolve(
        setting,
        std::env::var_os(DIRECTORY_VARIABLE).as_deref(),
        std::env::var_os("XDG_CACHE_HOME").as_deref(),
        std::env::var_os("HOME").as_deref(),
        std::env::var_os("NEXTEST_RUN_ID").is_some(),
    )
}

fn describe(cache: Option<&ImageCache>) -> String {
    cache.map_or_else(
        || "off".to_owned(),
        |cache| format!("in {}", cache.directory.display()),
    )
}

pub fn configure(setting: Option<Setting>) -> Result<(), CacheError> {
    let directory = resolve_here(setting);
    let mut failure = None;
    let chosen = CACHE.get_or_init(|| {
        let directory = directory.clone()?;
        ImageCache::open(&directory, DEFAULT_CAPACITY)
            .map_err(|error| failure = Some(error))
            .ok()
    });
    if let Some(error) = failure {
        return Err(error);
    }

    if chosen.as_ref().map(|cache| &cache.directory) != directory.as_ref() {
        return Err(CacheError::AlreadyConfigured {
            running: describe(chosen.as_ref()),
            requested: directory
                .map_or_else(|| "off".to_owned(), |path| format!("in {}", path.display())),
        });
    }

    Ok(())
}

pub(crate) fn report(problem: fmt::Arguments<'_>) {
    crate::count!("cache_problems", 1);
    #[cfg(debug_assertions)]
    crate::flight_recorder::record(problem);
    #[cfg(not(debug_assertions))]
    let _ = problem;
}

static UNWRITTEN: std::sync::Mutex<Vec<(PathBuf, String)>> = std::sync::Mutex::new(Vec::new());

pub(crate) fn unwritten(path: &Path, error: &WriteError) {
    report(format_args!(
        "{} for {}: {error}",
        error.key,
        path.display()
    ));
    let why = format!("in {}: {}", error.directory.display(), error.error);
    if let Ok(mut unwritten) = UNWRITTEN.lock() {
        unwritten.push((path.to_owned(), why));
    }
}

pub fn take_unwritten() -> Vec<(PathBuf, String)> {
    UNWRITTEN
        .lock()
        .map(|mut unwritten| std::mem::take(&mut *unwritten))
        .unwrap_or_default()
}

pub(crate) fn current() -> Option<&'static ImageCache> {
    CACHE
        .get_or_init(|| {
            let directory = resolve_here(None)?;
            ImageCache::open(&directory, DEFAULT_CAPACITY)
                .inspect_err(|error| report(format_args!("{error}")))
                .ok()
        })
        .as_ref()
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub struct Key(u128);

impl fmt::Display for Key {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(formatter, "{:032x}", self.0)
    }
}

impl Key {
    #[must_use]
    pub fn of(inputs: &[&[u8]]) -> Self {
        let mut digest = twox_hash::XxHash3_128::new();
        digest.write(b"microscope image\0");
        digest.write(env!("MICROSCOPE_SOURCES_DIGEST").as_bytes());

        for revision in crate::image::revisions() {
            digest.write(&revision.to_le_bytes());
        }

        digest.write(&(inputs.len() as u64).to_le_bytes());

        for input in inputs {
            digest.write(&(input.len() as u64).to_le_bytes());
            digest.write(input);
        }

        Self(digest.finish_128())
    }
}

#[derive(Debug)]
pub enum Lookup {
    Hit { image: Arc<Image>, stamp: FileStamp },

    Miss,

    Corrupt(ImageError),
}

#[derive(Debug, thiserror::Error)]
#[error("cannot write {key} to the image cache in {directory}: {error}")]
pub struct WriteError {
    key: Key,
    directory: PathBuf,

    #[source]
    error: std::io::Error,
}

static WRITES: AtomicU64 = AtomicU64::new(0);

#[derive(Debug)]
pub struct ImageCache {
    directory: PathBuf,
    capacity: u64,
    live: Mutex<HashMap<Key, (FileStamp, Weak<Image>)>>,
}

const SUFFIX: &str = ".image";
const TEMPORARY: &str = ".tmp";

impl ImageCache {
    pub fn open(directory: &Path, capacity: u64) -> Result<Self, CacheError> {
        let open = |error| CacheError::Open {
            path: directory.to_owned(),
            error,
        };

        std::fs::DirBuilder::new()
            .recursive(true)
            .mode(0o700)
            .create(directory)
            .map_err(open)?;
        let cache = Self {
            directory: directory.to_owned(),
            capacity,
            live: Mutex::default(),
        };

        cache.evict(capacity, true).map_err(open)?;

        Ok(cache)
    }

    #[must_use]
    pub fn directory(&self) -> &Path {
        &self.directory
    }

    fn path(&self, key: Key) -> PathBuf {
        self.directory.join(format!("{key}{SUFFIX}"))
    }

    #[must_use]
    pub fn get(&self, key: Key) -> Lookup {
        let path = self.path(key);
        let held = self
            .live
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .get(&key)
            .and_then(|(stamp, image)| image.upgrade().map(|image| (*stamp, image)));
        if let Some((stamp, image)) = held
            && std::fs::metadata(&path).is_ok_and(|metadata| FileStamp::of(&metadata) == stamp)
        {
            crate::count!("cache_live_hits", 1);
            return Lookup::Hit { image, stamp };
        }

        let _ = nix::sys::stat::utimensat(
            nix::fcntl::AT_FDCWD,
            &path,
            &nix::sys::time::TimeSpec::UTIME_NOW,
            &nix::sys::time::TimeSpec::UTIME_NOW,
            nix::sys::stat::UtimensatFlags::FollowSymlink,
        );
        let (bytes, stamp) = match read_entry(&path) {
            Ok(read) => read,
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Lookup::Miss,
            Err(error) => {
                report(format_args!("cannot read {}: {error}", path.display()));
                return Lookup::Miss;
            }
        };

        match Image::from_bytes(bytes, Limits::default()) {
            Ok(image) => {
                let image = Arc::new(image);
                let mut live = self.live.lock().unwrap_or_else(PoisonError::into_inner);
                live.retain(|_, (_, image)| image.strong_count() > 0);
                live.insert(key, (stamp, Arc::downgrade(&image)));
                drop(live);
                Lookup::Hit { image, stamp }
            }
            Err(error) => {
                self.discard(key, &stamp);
                Lookup::Corrupt(error)
            }
        }
    }

    pub fn discard(&self, key: Key, stamp: &FileStamp) {
        let path = self.path(key);
        let same = std::fs::symlink_metadata(&path)
            .is_ok_and(|metadata| metadata.dev() == stamp.device && metadata.ino() == stamp.inode);
        if same {
            record!("image cache: removing unusable {}", path.display());
            let _ = std::fs::remove_file(&path);
        }
    }

    pub fn put(&self, key: Key, image: &Image) -> Result<(), WriteError> {
        let failed = |error| WriteError {
            key,
            directory: self.directory.clone(),
            error,
        };

        let length = image.as_bytes().len() as u64;
        self.evict(self.capacity.saturating_sub(length), false)
            .map_err(failed)?;

        let temporary = self.directory.join(format!(
            ".{key}.{}.{}{TEMPORARY}",
            std::process::id(),
            WRITES.fetch_add(1, Ordering::Relaxed)
        ));
        let written = std::fs::OpenOptions::new()
            .write(true)
            .create_new(true)
            .mode(0o600)
            .open(&temporary)
            .and_then(|mut file| file.write_all(image.as_bytes()))
            .and_then(|()| std::fs::rename(&temporary, self.path(key)));
        if let Err(error) = written {
            let _ = std::fs::remove_file(&temporary);
            return Err(failed(error));
        }

        Ok(())
    }

    fn evict(&self, capacity: u64, abandoned: bool) -> std::io::Result<()> {
        let mut entries = Vec::new();

        for entry in std::fs::read_dir(&self.directory)? {
            let entry = entry?;
            let name = entry.file_name();
            let name = name.to_string_lossy();
            let evictable = name.ends_with(SUFFIX) || (abandoned && name.ends_with(TEMPORARY));
            if !evictable {
                continue;
            }

            let Ok(metadata) = entry.metadata() else {
                continue;
            };

            if metadata.is_file() {
                entries.push((
                    (metadata.mtime(), metadata.mtime_nsec()),
                    entry.path(),
                    metadata.len(),
                ));
            }
        }

        let mut held = entries.iter().map(|(_, _, length)| length).sum::<u64>();
        entries.sort();

        for (_, path, length) in entries {
            if held <= capacity {
                break;
            }

            match std::fs::remove_file(&path) {
                Ok(()) => record!("image cache: evicted {}", path.display()),
                Err(error) if error.kind() == std::io::ErrorKind::NotFound => {}
                Err(error) => return Err(error),
            }

            held -= length;
        }

        Ok(())
    }
}

fn read_entry(path: &Path) -> std::io::Result<(AlignedBytes, FileStamp)> {
    let mut file = std::fs::File::open(path)?;
    let metadata = file.metadata()?;
    let unusable = |why| std::io::Error::new(std::io::ErrorKind::InvalidData, why);

    if !metadata.is_file() {
        return Err(unusable("not a file"));
    }

    let length = usize::try_from(metadata.len())
        .ok()
        .filter(|length| *length as u64 <= Limits::default().bytes)
        .ok_or_else(|| unusable("too large"))?;
    let mut bytes = AlignedBytes::zeroed(length).ok_or_else(|| unusable("too large"))?;
    file.read_exact(bytes.as_mut_bytes())?;

    Ok((bytes, FileStamp::of(&metadata)))
}

#[cfg(test)]
mod tests;
