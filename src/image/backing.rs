//! Owned bytes aligned for tables, and bounded snapshot reads of files.
//!
//! Every byte microscope parses is its own copy: a mapping of a file another
//! process can rewrite would change under the borrows that read it, and a
//! cached image is validated byte by byte anyway, so mapping it would save
//! only the copy.

use std::io::Read as _;
use std::path::Path;

use zerocopy::{FromBytes, FromZeros as _, Immutable, IntoBytes, KnownLayout};

use super::format::TABLE_ALIGNMENT;

/// One table-aligned block.
#[repr(C, align(64))]
#[derive(Clone, Copy, FromBytes, IntoBytes, KnownLayout, Immutable)]
struct Block([u8; TABLE_ALIGNMENT]);

const _: () = assert!(align_of::<Block>() == TABLE_ALIGNMENT);

/// Owned, zero-initialized bytes whose start is aligned for tables.
#[derive(Clone)]
pub struct AlignedBytes {
    blocks: Vec<Block>,
    length: usize,
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
    /// `length` zero bytes, or `None` when they cannot be allocated.
    ///
    /// The bytes are asked of the allocator as zeroed, not filled: a large
    /// block comes from pages the kernel has already zeroed, so an image or
    /// file about to be written over is not written twice.
    pub fn zeroed(length: usize) -> Option<Self> {
        let blocks = length.div_ceil(TABLE_ALIGNMENT);
        Some(Self {
            blocks: Block::new_vec_zeroed(blocks).ok()?,
            length,
        })
    }

    /// A copy of `bytes`.
    #[cfg(test)]
    pub fn copy_of(bytes: &[u8]) -> Option<Self> {
        let mut aligned = Self::zeroed(bytes.len())?;
        aligned.as_mut_bytes().copy_from_slice(bytes);
        Some(aligned)
    }

    pub fn as_bytes(&self) -> &[u8] {
        &self.blocks.as_bytes()[..self.length]
    }

    pub fn as_mut_bytes(&mut self) -> &mut [u8] {
        &mut self.blocks.as_mut_bytes()[..self.length]
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

/// Why a file could not be read as one consistent snapshot.
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

/// What identifies one version of a file's contents well enough to see an
/// ordinary rewrite: not proof of equal contents, which only hashing gives.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct FileStamp {
    pub device: u64,
    pub inode: u64,
    pub length: u64,
    pub modified: (i64, i64),
    pub changed: (i64, i64),
}

impl FileStamp {
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

/// The most bytes microscope reads of one input file.
pub const MAX_INPUT: u64 = 16 << 30;

/// Reads an input file, such as a program or its debug file, as one
/// snapshot of at most [`MAX_INPUT`] bytes.
pub fn read_input(path: &Path) -> std::io::Result<(Vec<u8>, FileStamp)> {
    Ok(read_snapshot(path, MAX_INPUT, &mut |_| {}, vector)?)
}

/// `length` zero bytes, or `None` when they cannot be allocated; zeroed
/// as [`AlignedBytes::zeroed`] is.
fn vector(length: usize) -> Option<Vec<u8>> {
    u8::new_vec_zeroed(length).ok()
}

/// How many times a read starts again when the file changed under it.
const READ_ATTEMPTS: u32 = 3;

/// Reads a whole file from one descriptor into a buffer `allocate` makes
/// of the file's length, or `None` when it cannot. The file's metadata is
/// compared before and after, and a read that saw it change, grow, or
/// shrink starts again, at most [`READ_ATTEMPTS`] times.
///
/// That catches ordinary rewrites, such as a build replacing its output;
/// it cannot prove a snapshot against a writer racing on purpose. Tests
/// change the file in `between`, which runs after each attempt's first
/// metadata check and before its read.
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

        // A file that grew has more to read; one that shrank ended early.
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

/// Reads a stream, such as a FIFO, to its end: what it yields is consumed
/// once, so it is its own snapshot.
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

/// Reads until `buffer` is full or the file ends, returning how much it
/// read.
fn read_fully(file: &mut std::fs::File, buffer: &mut [u8]) -> std::io::Result<usize> {
    let mut filled = 0;
    while filled < buffer.len() {
        match file.read(&mut buffer[filled..]) {
            Ok(0) => break,
            Ok(count) => filled += count,
            Err(error) if error.kind() == std::io::ErrorKind::Interrupted => {}
            Err(error) => return Err(error),
        }
    }

    Ok(filled)
}
