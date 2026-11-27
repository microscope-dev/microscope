use std::io::Read as _;
use std::path::Path;

use zerocopy::{FromBytes, FromZeros as _, Immutable, IntoBytes, KnownLayout};

use super::format::TABLE_ALIGNMENT;

#[repr(C, align(64))]
#[derive(Clone, Copy, FromBytes, IntoBytes, KnownLayout, Immutable)]
struct Block([u8; TABLE_ALIGNMENT]);

const _: () = assert!(align_of::<Block>() == TABLE_ALIGNMENT);

/// 64-byte-aligned byte buffer backed by either heap blocks or a read-only
/// `mmap`.
///
/// Alignment guarantees zero-copy `zerocopy` table reads; calling
/// `as_mut_bytes` on a mapped buffer copies it to the heap first.
pub struct AlignedBytes {
    storage: Storage,
    length: usize,
}

enum Storage {
    Heap(Vec<Block>),
    Mapped(Mapping),
}

struct Mapping {
    start: std::ptr::NonNull<std::ffi::c_void>,
    length: std::num::NonZeroUsize,
}

// SAFETY: the mapping is read-only and owned by one `Mapping`, which unmaps it
// once, when dropped, so sharing or sending it is sharing a `&[u8]`.
#[allow(unsafe_code, reason = "a read-only mapping is shared like a slice")]
unsafe impl Send for Mapping {}

// SAFETY: as for `Send`; nothing writes through the mapping.
#[allow(unsafe_code, reason = "a read-only mapping is shared like a slice")]
unsafe impl Sync for Mapping {}

impl Drop for Mapping {
    #[allow(unsafe_code, reason = "unmapping memory is unsafe")]
    fn drop(&mut self) {
        // SAFETY: `start` and `length` are the mapping `AlignedBytes::map`
        // made, and every slice of it borrows `self`, so none outlives this.
        let _ = unsafe { nix::sys::mman::munmap(self.start, self.length.get()) };
    }
}

impl Clone for AlignedBytes {
    fn clone(&self) -> Self {
        match &self.storage {
            Storage::Heap(blocks) => Self {
                storage: Storage::Heap(blocks.clone()),
                length: self.length,
            },
            Storage::Mapped(_) => self.to_heap(),
        }
    }
}

impl std::fmt::Debug for AlignedBytes {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter
            .debug_struct("AlignedBytes")
            .field("length", &self.length)
            .finish_non_exhaustive()
    }
}

impl AlignedBytes {
    #[must_use]
    pub fn zeroed(length: usize) -> Option<Self> {
        let blocks = length.div_ceil(TABLE_ALIGNMENT);
        Some(Self {
            storage: Storage::Heap(Block::new_vec_zeroed(blocks).ok()?),
            length,
        })
    }

    #[cfg(any(test, feature = "testing"))]
    #[must_use]
    pub fn copy_of(bytes: &[u8]) -> Option<Self> {
        let mut aligned = Self::zeroed(bytes.len())?;
        aligned.as_mut_bytes().copy_from_slice(bytes);
        Some(aligned)
    }

    /// Maps `file` read-only after verifying it is a regular file owned by the
    /// current effective UID and not writable by group or others (`0o022`),
    /// preventing another user from mutating or truncating memory behind `&[u8]`.
    #[allow(unsafe_code, reason = "mapping a file is unsafe")]
    pub fn map(file: &std::fs::File, length: usize) -> std::io::Result<Self> {
        use std::os::unix::fs::MetadataExt as _;

        use nix::sys::mman::{MapFlags, ProtFlags};

        let metadata = file.metadata()?;
        let others_can_write = metadata.mode() & 0o022 != 0;
        if !metadata.is_file()
            || metadata.uid() != nix::unistd::geteuid().as_raw()
            || others_can_write
        {
            return Err(std::io::Error::new(
                std::io::ErrorKind::PermissionDenied,
                "only a regular file that no other user can write is mapped",
            ));
        }
        if metadata.len() < length as u64 {
            return Err(std::io::ErrorKind::UnexpectedEof.into());
        }

        let Some(nonzero) = std::num::NonZeroUsize::new(length) else {
            return Self::zeroed(0).ok_or_else(|| std::io::ErrorKind::OutOfMemory.into());
        };

        // SAFETY: a private, read-only mapping of a file aliases no Rust
        // memory. The file holds at least `length` bytes, and only this
        // process's user or root can change it, either of whom can already
        // change this process's memory. Microscope itself only replaces an
        // entry by renaming a new file over it, which leaves this one intact.
        let start = unsafe {
            nix::sys::mman::mmap(
                None,
                nonzero,
                ProtFlags::PROT_READ,
                MapFlags::MAP_PRIVATE,
                file,
                0,
            )
        }
        .map_err(std::io::Error::from)?;
        assert_eq!(start.as_ptr().cast::<u8>().align_offset(TABLE_ALIGNMENT), 0);

        Ok(Self {
            storage: Storage::Mapped(Mapping {
                start,
                length: nonzero,
            }),
            length,
        })
    }

    #[allow(unsafe_code, reason = "advising the kernel about memory is unsafe")]
    pub fn release(&self) {
        let Storage::Mapped(mapping) = &self.storage else {
            return;
        };

        // SAFETY: the mapping is private and read-only, so it holds no
        // changed page to lose; the kernel reads dropped pages back from the
        // file the next time they are touched.
        let _ = unsafe {
            nix::sys::mman::madvise(
                mapping.start,
                mapping.length.get(),
                nix::sys::mman::MmapAdvise::MADV_DONTNEED,
            )
        };
    }

    #[must_use]
    pub const fn is_mapped(&self) -> bool {
        matches!(self.storage, Storage::Mapped(_))
    }

    fn to_heap(&self) -> Self {
        let mut blocks = Block::new_vec_zeroed(self.length.div_ceil(TABLE_ALIGNMENT))
            .unwrap_or_else(|_| std::alloc::handle_alloc_error(std::alloc::Layout::new::<Block>()));
        blocks.as_mut_bytes()[..self.length].copy_from_slice(self.as_bytes());
        Self {
            storage: Storage::Heap(blocks),
            length: self.length,
        }
    }

    #[expect(
        clippy::indexing_slicing,
        reason = "heap storage holds blocks for at least `length` bytes"
    )]
    #[allow(unsafe_code, reason = "a mapping is read through a raw pointer")]
    #[must_use]
    pub fn as_bytes(&self) -> &[u8] {
        match &self.storage {
            Storage::Heap(blocks) => &blocks.as_bytes()[..self.length],
            // SAFETY: the mapping covers `length` readable bytes, lives as long
            // as `self`, and `map` mapped only a file nothing else changes.
            Storage::Mapped(mapping) => unsafe {
                std::slice::from_raw_parts(mapping.start.as_ptr().cast::<u8>(), self.length)
            },
        }
    }

    #[expect(
        clippy::indexing_slicing,
        reason = "heap storage holds blocks for at least `length` bytes"
    )]
    pub fn as_mut_bytes(&mut self) -> &mut [u8] {
        if self.is_mapped() {
            *self = self.to_heap();
        }
        let Storage::Heap(blocks) = &mut self.storage else {
            unreachable!("a mapping was just copied to the heap");
        };
        &mut blocks.as_mut_bytes()[..self.length]
    }
}

impl AsMut<[u8]> for AlignedBytes {
    fn as_mut(&mut self) -> &mut [u8] {
        self.as_mut_bytes()
    }
}

impl std::ops::Deref for AlignedBytes {
    type Target = [u8];

    fn deref(&self) -> &[u8] {
        self.as_bytes()
    }
}

#[derive(Debug, thiserror::Error)]
pub enum ReadError {
    #[error(transparent)]
    Io(#[from] std::io::Error),

    #[error("{path} is a directory")]
    NotAFile { path: String },

    #[error("{path} is {length} bytes, more than the {limit} bytes microscope reads")]
    TooLarge {
        path: String,
        length: u64,
        limit: u64,
    },

    #[error("{path} changed while it was read, {attempts} times")]
    Changed { path: String, attempts: u32 },

    #[error("cannot allocate {length} bytes to read {path}")]
    Allocation { path: String, length: u64 },
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct FileStamp {
    pub device: u64,
    pub inode: u64,
    pub length: u64,
    pub modified: (i64, i64),
    pub changed: (i64, i64),
}

impl FileStamp {
    #[must_use]
    pub fn of(metadata: &std::fs::Metadata) -> Self {
        use std::os::unix::fs::MetadataExt as _;
        Self {
            device: metadata.dev(),
            inode: metadata.ino(),
            length: metadata.len(),
            modified: (metadata.mtime(), metadata.mtime_nsec()),
            changed: (metadata.ctime(), metadata.ctime_nsec()),
        }
    }
}

impl From<ReadError> for std::io::Error {
    fn from(error: ReadError) -> Self {
        match error {
            ReadError::Io(error) => error,
            ReadError::NotAFile { .. } => Self::new(std::io::ErrorKind::InvalidInput, error),
            ReadError::TooLarge { .. } => Self::new(std::io::ErrorKind::FileTooLarge, error),
            ReadError::Changed { .. } => Self::other(error),
            ReadError::Allocation { .. } => Self::new(std::io::ErrorKind::OutOfMemory, error),
        }
    }
}

pub const MAX_INPUT: u64 = 16 << 30;

pub fn read_input(path: &Path) -> std::io::Result<(Vec<u8>, FileStamp)> {
    Ok(read_snapshot(path, MAX_INPUT, &mut |_| {}, vector)?)
}

/// Opens `path` with `O_PATH` before checking `is_file` and reopening via
/// `/proc/self/fd`, avoiding both blocking on FIFOs/devices and `stat`-then-`open`
/// TOCTOU races.
pub fn read_named(path: &Path) -> std::io::Result<Vec<u8>> {
    use std::os::fd::AsRawFd as _;
    use std::os::unix::fs::OpenOptionsExt as _;

    let file = std::fs::OpenOptions::new()
        .read(true)
        .custom_flags(nix::fcntl::OFlag::O_PATH.bits())
        .open(path)?;
    if !file.metadata()?.is_file() {
        return Err(std::io::Error::new(
            std::io::ErrorKind::InvalidInput,
            format!("{} is not a regular file", path.display()),
        ));
    }

    let opened = format!("/proc/self/fd/{}", file.as_raw_fd());
    Ok(read_input(Path::new(&opened))?.0)
}

fn vector(length: usize) -> Option<Vec<u8>> {
    u8::new_vec_zeroed(length).ok()
}

const READ_ATTEMPTS: u32 = 3;

/// Reads a lock-free consistent snapshot of `path` by comparing `FileStamp`
/// before and after the read (plus a 1-byte EOF probe), retrying up to
/// `READ_ATTEMPTS` times if the file changes concurrently.
pub fn read_snapshot<B: AsMut<[u8]>>(
    path: &Path,
    limit: u64,
    between: &mut dyn FnMut(u32),
    allocate: impl Fn(usize) -> Option<B>,
) -> Result<(B, FileStamp), ReadError> {
    let name = || path.display().to_string();
    for attempt in 0..READ_ATTEMPTS {
        let mut file = std::fs::File::open(path)?;
        let before = file.metadata()?;
        if before.is_dir() {
            return Err(ReadError::NotAFile { path: name() });
        }

        if !before.is_file() {
            return read_stream(file, &before, limit, &allocate).map_err(|error| match error {
                StreamError::Io(error) => ReadError::Io(error),
                StreamError::TooLarge(length) => ReadError::TooLarge {
                    path: name(),
                    length,
                    limit,
                },
                StreamError::Allocation(length) => ReadError::Allocation {
                    path: name(),
                    length,
                },
            });
        }

        let stamp = FileStamp::of(&before);
        if stamp.length > limit {
            return Err(ReadError::TooLarge {
                path: name(),
                length: stamp.length,
                limit,
            });
        }

        between(attempt);
        let length = usize::try_from(stamp.length).map_err(|_| ReadError::TooLarge {
            path: name(),
            length: stamp.length,
            limit,
        })?;
        let mut bytes = allocate(length).ok_or_else(|| ReadError::Allocation {
            path: name(),
            length: stamp.length,
        })?;
        let read = read_fully(&mut file, bytes.as_mut())?;

        let mut probe = [0_u8; 1];
        let grew = file.read(&mut probe)? != 0;
        let after = FileStamp::of(&file.metadata()?);
        if read == length && !grew && after == stamp {
            return Ok((bytes, stamp));
        }
    }

    Err(ReadError::Changed {
        path: name(),
        attempts: READ_ATTEMPTS,
    })
}

enum StreamError {
    Io(std::io::Error),
    TooLarge(u64),
    Allocation(u64),
}

fn read_stream<B: AsMut<[u8]>>(
    file: std::fs::File,
    metadata: &std::fs::Metadata,
    limit: u64,
    allocate: impl Fn(usize) -> Option<B>,
) -> Result<(B, FileStamp), StreamError> {
    let mut data = Vec::new();
    file.take(limit.saturating_add(1))
        .read_to_end(&mut data)
        .map_err(StreamError::Io)?;
    let length = data.len() as u64;
    if length > limit {
        return Err(StreamError::TooLarge(length));
    }

    let mut bytes = allocate(data.len()).ok_or(StreamError::Allocation(length))?;
    bytes.as_mut().copy_from_slice(&data);

    Ok((
        bytes,
        FileStamp {
            length,
            ..FileStamp::of(metadata)
        },
    ))
}

fn read_fully(file: &mut std::fs::File, buffer: &mut [u8]) -> std::io::Result<usize> {
    let mut filled = 0;
    while let Some(rest) = buffer.get_mut(filled..).filter(|rest| !rest.is_empty()) {
        match file.read(rest) {
            Ok(0) => break,
            Ok(count) => filled += count,
            Err(error) if error.kind() == std::io::ErrorKind::Interrupted => {}
            Err(error) => return Err(error),
        }
    }

    Ok(filled)
}
