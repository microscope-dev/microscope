//! Finding the files of images a core dump records.
//!
//! A dump names each image by its path on the machine that wrote it. That
//! path is looked up under one root: the host's own by default, or a sysroot
//! holding the other machine's files, inside which every path resolves as if
//! it were `/`, absolute symbolic links and `..` included. Module directories
//! are then searched by the recorded file name and finally by build-id.
//!
//! Every file found here is only a candidate. Identity is proven separately,
//! and a file found by searching is used only once proven, so the search may
//! be generous without ever choosing a wrong file.

use std::cell::OnceCell;
use std::collections::BTreeMap;
use std::fmt::Write as _;
use std::fs::{self, File, OpenOptions};
use std::io;
use std::os::fd::{AsRawFd as _, OwnedFd};
use std::os::unix::fs::{MetadataExt as _, OpenOptionsExt as _};
use std::path::{Path, PathBuf};

use nix::errno::Errno;
use nix::fcntl::{OFlag, OpenHow, ResolveFlag, openat2};
use nix::libc;
use object::ReadCache;

use super::core_dump::elf_build_id;
use crate::{Error, Result};

/// `openat2` fails with `EAGAIN` when a concurrent rename could have let
/// resolution escape the root; the lookup is simply retried.
const RESOLVE_ATTEMPTS: usize = 16;

/// A regular file read for one candidate.
pub(super) struct ModuleFile {
    /// The file's canonical path on this machine.
    pub(super) path: PathBuf,
    pub(super) data: Vec<u8>,
    pub(super) inode: u64,

    /// Whether a module directory supplied the file rather than the
    /// recorded path. Such a file is a guess until proven.
    pub(super) searched: bool,
}

/// Where the files of recorded images are looked for.
pub(super) struct ModuleLocator {
    sysroot: Option<Sysroot>,
    directories: Vec<ModuleDirectory>,
}

struct Sysroot {
    path: PathBuf,
    directory: OwnedFd,
}

struct ModuleDirectory {
    path: PathBuf,

    /// Every ELF file's build-id, read the first time a build-id is sought.
    build_ids: OnceCell<BTreeMap<Vec<u8>, Vec<PathBuf>>>,
}

impl ModuleLocator {
    /// Checks that the sysroot and every module directory are directories,
    /// so a mistyped path fails instead of leaving modules missing.
    pub(super) fn new(sysroot: Option<&Path>, directories: &[PathBuf]) -> Result<Self> {
        let unusable = |path: &Path, error| Error::CoreModuleSearch {
            path: path.to_owned(),
            error,
        };

        let sysroot = sysroot
            .map(|path| {
                OpenOptions::new()
                    .read(true)
                    .custom_flags(libc::O_PATH | libc::O_DIRECTORY)
                    .open(path)
                    .map(|directory| Sysroot {
                        path: path.to_owned(),
                        directory: directory.into(),
                    })
                    .map_err(|error| unusable(path, error))
            })
            .transpose()?;

        let directories = directories
            .iter()
            .map(|path| match fs::metadata(path) {
                Ok(metadata) if metadata.is_dir() => Ok(ModuleDirectory {
                    path: path.clone(),
                    build_ids: OnceCell::new(),
                }),
                Ok(_) => Err(unusable(
                    path,
                    io::Error::from(io::ErrorKind::NotADirectory),
                )),
                Err(error) => Err(unusable(path, error)),
            })
            .collect::<Result<_>>()?;

        Ok(Self {
            sysroot,
            directories,
        })
    }

    /// Files that may be the image recorded at `recorded`, in priority order:
    /// the recorded path under the root, then each module directory's file of
    /// the same name. Places that hold no file are skipped.
    pub(super) fn named<'a>(
        &'a self,
        recorded: &'a Path,
    ) -> impl Iterator<Item = Result<ModuleFile>> + 'a {
        let rooted = std::iter::once_with(move || {
            self.sysroot
                .as_ref()
                .map_or_else(|| open(recorded, false), |sysroot| sysroot.open(recorded))
        });
        let named = recorded.file_name().into_iter().flat_map(move |name| {
            self.directories
                .iter()
                .map(move |directory| open(&directory.path.join(name), true))
        });
        rooted.chain(named).filter_map(Result::transpose)
    }

    /// Files in the module directories whose build-id is `build_id`, in
    /// directory order and then by name.
    pub(super) fn with_build_id<'a>(
        &'a self,
        build_id: &'a [u8],
    ) -> impl Iterator<Item = Result<ModuleFile>> + 'a {
        self.directories
            .iter()
            .flat_map(move |directory| {
                directory
                    .build_ids()
                    .get(build_id)
                    .into_iter()
                    .flatten()
                    .map(|path| open(path, true))
            })
            .filter_map(Result::transpose)
    }

    /// Describes where a recorded path was looked for, as the predicate of
    /// "`<path>` ...".
    pub(super) fn absence(&self) -> String {
        let mut text = self.sysroot.as_ref().map_or_else(
            || "no longer exists".to_owned(),
            |sysroot| {
                format!(
                    "does not exist under the sysroot {}",
                    sysroot.path.display()
                )
            },
        );
        if !self.directories.is_empty() {
            text.push_str(", and no module path holds a file matching it");
        }

        text
    }
}

impl Sysroot {
    fn open(&self, recorded: &Path) -> Result<Option<ModuleFile>> {
        let how = OpenHow::new()
            .flags(OFlag::O_PATH | OFlag::O_CLOEXEC)
            .resolve(ResolveFlag::RESOLVE_IN_ROOT | ResolveFlag::RESOLVE_NO_MAGICLINKS);
        let mut attempts = 0;
        let opened = loop {
            attempts += 1;
            match openat2(&self.directory, recorded, how) {
                Err(Errno::EAGAIN) if attempts < RESOLVE_ATTEMPTS => {}
                result => break result.map(File::from).map_err(io::Error::from),
            }
        };

        let display = self
            .path
            .join(recorded.strip_prefix("/").unwrap_or(recorded));
        read_opened(&display, opened, false)
    }
}

impl ModuleDirectory {
    fn build_ids(&self) -> &BTreeMap<Vec<u8>, Vec<PathBuf>> {
        self.build_ids.get_or_init(|| {
            // A directory that cannot be listed now was listable when the
            // locator was created; its files are then simply not candidates.
            let mut paths = fs::read_dir(&self.path)
                .into_iter()
                .flatten()
                .filter_map(|entry| Some(entry.ok()?.path()))
                .collect::<Vec<_>>();
            paths.sort();
            let mut build_ids = BTreeMap::<_, Vec<_>>::new();
            for path in paths {
                if let Some(build_id) = file_build_id(&path) {
                    build_ids.entry(build_id).or_default().push(path);
                }
            }

            build_ids
        })
    }
}

/// Reads only as much of a file as its build-id note requires. Files that
/// cannot be opened or are not ELF images have none.
fn file_build_id(path: &Path) -> Option<Vec<u8>> {
    let file = open_path(path).ok()?;
    if !file.metadata().ok()?.is_file() {
        return None;
    }

    let cache = ReadCache::new(File::open(fd_path(&file)).ok()?);
    elf_build_id(&cache).map(<[u8]>::to_vec)
}

/// Opens a file the user named explicitly, for which absence is an error.
pub(super) fn open_explicit(path: &Path) -> Result<ModuleFile> {
    open(path, false)?.ok_or_else(|| Error::CoreModuleRead {
        path: path.to_owned(),
        error: io::Error::from(io::ErrorKind::NotFound),
    })
}

fn open(path: &Path, searched: bool) -> Result<Option<ModuleFile>> {
    read_opened(path, open_path(path), searched)
}

/// Opens a path without reading from it: devices and FIFOs, which paths in a
/// dump can name, are never opened for reading.
fn open_path(path: &Path) -> io::Result<File> {
    OpenOptions::new()
        .read(true)
        .custom_flags(libc::O_PATH)
        .open(path)
}

fn fd_path(file: &File) -> PathBuf {
    PathBuf::from(format!("/proc/self/fd/{}", file.as_raw_fd()))
}

/// Reads the regular file behind an `O_PATH` descriptor. A path that names
/// nothing yields `None`, as does one that searching found naming something
/// other than a regular file; anything else that prevents reading it is an
/// error.
fn read_opened(
    display: &Path,
    opened: io::Result<File>,
    searched: bool,
) -> Result<Option<ModuleFile>> {
    let unreadable = |error| Error::CoreModuleRead {
        path: display.to_owned(),
        error,
    };

    let file = match opened {
        Ok(file) => file,
        Err(error)
            if matches!(
                error.kind(),
                io::ErrorKind::NotFound | io::ErrorKind::NotADirectory
            ) =>
        {
            return Ok(None);
        }
        Err(error) => return Err(unreadable(error)),
    };

    let metadata = file.metadata().map_err(unreadable)?;
    if !metadata.is_file() && searched {
        return Ok(None);
    }

    if !metadata.is_file() {
        return Err(unreadable(io::Error::new(
            io::ErrorKind::InvalidInput,
            "not a regular file",
        )));
    }

    // Reopening through the descriptor reads exactly the file that was
    // resolved, however its path has changed since.
    let link = fd_path(&file);
    let path = fs::read_link(&link).map_err(unreadable)?;
    let (data, _) = crate::image::backing::read_input(&link).map_err(unreadable)?;

    Ok(Some(ModuleFile {
        path,
        data,
        inode: metadata.ino(),
        searched,
    }))
}

/// Lowercase hexadecimal, as build-ids are conventionally written.
pub(super) fn hex(bytes: &[u8]) -> String {
    bytes.iter().fold(String::new(), |mut text, byte| {
        // Writing to a String cannot fail.
        let _ = write!(text, "{byte:02x}");
        text
    })
}
