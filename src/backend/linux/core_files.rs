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

const RESOLVE_ATTEMPTS: usize = 16;

pub(super) struct ModuleFile {
    pub(super) path: PathBuf,
    pub(super) data: Vec<u8>,
    pub(super) inode: u64,

    pub(super) searched: bool,
}

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

    build_ids: OnceCell<BTreeMap<Vec<u8>, Vec<PathBuf>>>,
}

impl ModuleLocator {
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

fn file_build_id(path: &Path) -> Option<Vec<u8>> {
    let file = open_path(path).ok()?;
    if !file.metadata().ok()?.is_file() {
        return None;
    }

    let cache = ReadCache::new(File::open(fd_path(&file)).ok()?);
    elf_build_id(&cache).map(<[u8]>::to_vec)
}

pub(super) fn open_explicit(path: &Path) -> Result<ModuleFile> {
    open(path, false)?.ok_or_else(|| Error::CoreModuleRead {
        path: path.to_owned(),
        error: io::Error::from(io::ErrorKind::NotFound),
    })
}

fn open(path: &Path, searched: bool) -> Result<Option<ModuleFile>> {
    read_opened(path, open_path(path), searched)
}

fn open_path(path: &Path) -> io::Result<File> {
    OpenOptions::new()
        .read(true)
        .custom_flags(libc::O_PATH)
        .open(path)
}

fn fd_path(file: &File) -> PathBuf {
    PathBuf::from(format!("/proc/self/fd/{}", file.as_raw_fd()))
}

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

pub(super) fn hex(bytes: &[u8]) -> String {
    bytes.iter().fold(String::new(), |mut text, byte| {
        let _ = write!(text, "{byte:02x}");
        text
    })
}
