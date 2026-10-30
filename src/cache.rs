//! The image cache: sealed images by their inputs' contents, so that a
//! program microscope has loaded before loads by reading and validating one
//! file.
//!
//! An entry's key digests the exact bytes the image was built from (the
//! file, the separate debug file chosen for it, and the dwz supplementary
//! file its DWARF shares), the image format, and
//! the sources the loader was built from, so an entry is never read by a
//! loader that might have built it differently. Paths, identifiers, and
//! everything else a session binds an image to stay out of the bytes, so
//! a copy of a program at another path is a hit.
//!
//! Entries are written whole to a file of their own and then renamed into
//! place, so a reader never sees part of one, and are removed only by
//! eviction or when found corrupt. Every entry is validated before use: a
//! checksum catches accidental damage, not a deliberately forged entry, so
//! the cache is private to its user.
//!
//! One process has one cache, chosen by the option a program passes to
//! [`configure`], then `MICROSCOPE_CACHE_DIR` (empty for none), and otherwise
//! `$XDG_CACHE_HOME/microscope/images` or `~/.cache/microscope/images`, or none
//! under nextest, whose tests choose their own.

use std::ffi::OsStr;
use std::fmt;
use std::io::{Read as _, Write as _};
use std::os::unix::fs::{DirBuilderExt as _, MetadataExt as _, OpenOptionsExt as _};
use std::path::{Path, PathBuf};
use std::sync::OnceLock;
use std::sync::atomic::{AtomicU64, Ordering};

use crate::image::backing::FileStamp;
use crate::image::{AlignedBytes, Image, ImageError, Limits};

/// The environment variable that names the cache's directory.
pub const DIRECTORY_VARIABLE: &str = "MICROSCOPE_CACHE_DIR";

/// The most bytes a cache keeps.
///
/// Opening one, and writing to it, evicts its oldest entries beyond this.
/// One image may be as large as a load's input, 16 GiB, and Chrome's with
/// its debug information is 7.6 GB, so the cache holds that much.
pub const DEFAULT_CAPACITY: u64 = crate::image::backing::MAX_INPUT;

/// Which cache a process uses.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Setting {
    /// No cache: every load reads its debug information.
    Off,

    /// The cache in this directory.
    Directory(PathBuf),
}

/// Why the cache cannot be used.
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

/// The cache's directory: `setting`, else the variable's value `variable`
/// (empty for none), else none when `under_test`, else the user's cache
/// directory from `xdg` or `home`.
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

/// Chooses the process's cache and opens it.
///
/// [`resolve`] chooses from `setting` and the environment. A cache that
/// cannot be opened leaves loads uncached, and the error says why.
/// Choosing another cache once one is chosen is an error.
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

/// Reports a problem with the cache, which costs loads only time: counted
/// for `--timings` in every build, and described in a development build's
/// flight recording.
pub(crate) fn report(problem: fmt::Arguments<'_>) {
    crate::count!("cache_problems", 1);
    #[cfg(debug_assertions)]
    crate::flight_recorder::record(problem);
    #[cfg(not(debug_assertions))]
    let _ = problem;
}

/// The process's cache, choosing it from the environment when nothing has.
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

/// What names one entry: a digest of every input and of what the loader is.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub struct Key(u128);

impl fmt::Display for Key {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(formatter, "{:032x}", self.0)
    }
}

impl Key {
    /// The key of an image built from `inputs`, in their roles' order: the
    /// file, then the separate debug file chosen for it, if any, then the
    /// supplementary file its DWARF shares, if any. Which role an input has
    /// follows from the bytes before it.
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

/// What looking up a key found.
#[derive(Debug)]
pub enum Lookup {
    /// A valid image, and the file it was read from.
    Hit { image: Box<Image>, stamp: FileStamp },

    /// No entry, or none that could be read.
    Miss,

    /// An entry that failed validation, which was removed.
    Corrupt(ImageError),
}

/// Why an entry could not be written.
#[derive(Debug, thiserror::Error)]
#[error("cannot write {key} to the image cache in {directory}: {error}")]
pub struct WriteError {
    key: Key,
    directory: PathBuf,

    #[source]
    error: std::io::Error,
}

/// Distinguishes the temporary files of one process's writers.
static WRITES: AtomicU64 = AtomicU64::new(0);

/// A directory of images, one file per key.
#[derive(Debug)]
pub struct ImageCache {
    directory: PathBuf,
    capacity: u64,
}

const SUFFIX: &str = ".image";
const TEMPORARY: &str = ".tmp";

impl ImageCache {
    /// Opens the cache in `directory`, creating it for its user alone, and
    /// evicts its oldest entries until it holds at most `capacity` bytes.
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
        };

        cache.evict(capacity, true).map_err(open)?;

        Ok(cache)
    }

    /// The cache's directory.
    #[must_use]
    pub fn directory(&self) -> &Path {
        &self.directory
    }

    fn path(&self, key: Key) -> PathBuf {
        self.directory.join(format!("{key}{SUFFIX}"))
    }

    /// Reads and validates the entry for `key`. One that fails validation
    /// is removed, unless another writer replaced it meanwhile.
    #[must_use]
    pub fn get(&self, key: Key) -> Lookup {
        let path = self.path(key);
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
                // Eviction takes the entries used longest ago first.
                let _ = nix::sys::stat::utimensat(
                    nix::fcntl::AT_FDCWD,
                    &path,
                    &nix::sys::time::TimeSpec::UTIME_NOW,
                    &nix::sys::time::TimeSpec::UTIME_NOW,
                    nix::sys::stat::UtimensatFlags::FollowSymlink,
                );
                Lookup::Hit {
                    image: Box::new(image),
                    stamp,
                }
            }
            Err(error) => {
                self.discard(key, &stamp);
                Lookup::Corrupt(error)
            }
        }
    }

    /// Removes the entry for `key` if it is still the file `stamp`
    /// describes, which a reader found unusable.
    pub fn discard(&self, key: Key, stamp: &FileStamp) {
        let path = self.path(key);
        let same = std::fs::symlink_metadata(&path)
            .is_ok_and(|metadata| metadata.dev() == stamp.device && metadata.ino() == stamp.inode);
        if same {
            record!("image cache: removing unusable {}", path.display());
            let _ = std::fs::remove_file(&path);
        }
    }

    /// Writes `image` as the entry for `key`: to a file of its own, then
    /// renamed into place, replacing any entry another writer made. The
    /// entries used longest ago are evicted first, until it fits.
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

    /// Removes the entries used longest ago, and files writers left when
    /// `abandoned` says the cache is opening, until the cache holds at most
    /// `capacity` bytes. A writer's file is left alone while it writes.
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

            // Another process may have removed it meanwhile.
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

/// Reads an entry. Entries are never written in place, so unlike an
/// input's snapshot this does not watch for changes: replacing an entry
/// changes only the replaced file's link count, and validation catches
/// damage.
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
