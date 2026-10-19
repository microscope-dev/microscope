//! The sealed container every image kind shares: a header, a directory of
//! tables in kind order, each table 64-byte aligned with zeroed padding
//! between, and an XXH3-64 trailer. A [`Format`] names one kind of image:
//! its magic, its revision, and its tables' kinds and strides. Debug-info
//! images and profile captures are two formats over this one container.

use std::fmt::Debug;

use zerocopy::{FromBytes as _, Immutable, IntoBytes};

use super::backing::AlignedBytes;
use super::format::{self, DirectoryEntry, Header, Trailer};
use super::validate::{ImageError, Limits};
use super::{MAX_ROWS, Placed};
use crate::{Architecture, ByteOrder, PointerWidth, TargetDescription};

/// One kind of image over the container.
pub trait Format {
    /// What a table of this format holds.
    type Kind: Copy + Ord + Debug;

    /// The first bytes of every image of the format.
    const MAGIC: [u8; 8];

    /// What the tables mean, beyond their layout: bump it with any change
    /// to what is stored, so that no older image is read as newer.
    const REVISION: u32;

    /// How many kinds of table the format has.
    const KINDS: usize;

    /// The fingerprint of every record's layout.
    fn fingerprint() -> u64;

    /// The kind a directory number names, if the format has it.
    fn kind(number: u32) -> Option<Self::Kind>;

    /// A kind's number in the directory.
    fn number(kind: Self::Kind) -> u32;

    /// A kind's slot among the format's `KINDS`.
    fn index(kind: Self::Kind) -> usize;

    /// One record's size in a table of `kind`.
    fn stride(kind: Self::Kind) -> usize;
}

fn malformed(text: impl Into<String>) -> ImageError {
    ImageError::Malformed(text.into())
}

/// Checks the header, trailer, and directory of an image of `F`, returning
/// where each table is, by the kind's index; `N` is `F::KINDS`, so that
/// the directory needs no allocation.
pub fn structure<F: Format, const N: usize>(
    bytes: &[u8],
    limits: Limits,
) -> Result<[Option<Placed>; N], ImageError> {
    const { assert!(N > 0) };
    assert_eq!(N, F::KINDS, "a directory has one slot per kind");
    let header = check_header::<F>(bytes, limits)?;
    let trailer_start = bytes
        .len()
        .checked_sub(size_of::<Trailer>())
        .filter(|start| *start >= size_of::<Header>())
        .ok_or(ImageError::Truncated)?;
    let trailer =
        Trailer::ref_from_bytes(&bytes[trailer_start..]).map_err(|_| ImageError::Truncated)?;
    if trailer.checksum.get() != twox_hash::XxHash3_64::oneshot(&bytes[..trailer_start]) {
        return Err(ImageError::Checksum);
    }

    check_directory::<F, N>(bytes, header, trailer_start)
}

/// Checks what the header says of the image as a whole.
fn check_header<F: Format>(bytes: &[u8], limits: Limits) -> Result<&Header, ImageError> {
    let (header, _) = Header::ref_from_prefix(bytes).map_err(|_| ImageError::Truncated)?;
    if header.magic != F::MAGIC {
        return Err(ImageError::NotAnImage);
    }

    if header.format.get() != format::FORMAT_VERSION {
        return Err(ImageError::Format {
            found: header.format.get(),
            expected: format::FORMAT_VERSION,
        });
    }

    if header.normalization.get() != F::REVISION {
        return Err(ImageError::Normalization {
            found: header.normalization.get(),
            expected: F::REVISION,
        });
    }

    if header.layout.get() != F::fingerprint() {
        return Err(ImageError::Layout);
    }

    let length = header.length.get();
    if length > limits.bytes {
        return Err(ImageError::TooLarge);
    }

    if length != bytes.len() as u64 {
        return Err(ImageError::Truncated);
    }

    if target_of(header).is_none() || header.flags != 0 || header.reserved != [0; 24] {
        return Err(malformed("the header has fields it does not define"));
    }

    Ok(header)
}

/// Checks the directory between the header and the trailer, returning
/// where each table is.
fn check_directory<F: Format, const N: usize>(
    bytes: &[u8],
    header: &Header,
    trailer_start: usize,
) -> Result<[Option<Placed>; N], ImageError> {
    let count = header.tables.get() as usize;
    if count > F::KINDS {
        return Err(malformed(
            "the directory lists more tables than there are kinds",
        ));
    }

    let directory_end = size_of::<Header>() + count * size_of::<DirectoryEntry>();
    if directory_end > trailer_start {
        return Err(ImageError::Truncated);
    }

    let entries = <[DirectoryEntry]>::ref_from_bytes(&bytes[size_of::<Header>()..directory_end])
        .map_err(|_| ImageError::Truncated)?;
    let mut tables = [None; N];
    let mut previous_kind = None;
    let mut previous_end = directory_end;
    for entry in entries {
        let kind = F::kind(entry.kind.get())
            .ok_or_else(|| malformed(format!("unknown table kind {}", entry.kind.get())))?;
        if previous_kind.is_some_and(|previous| previous >= kind) {
            return Err(malformed("tables are out of order or repeated"));
        }

        previous_kind = Some(kind);
        let stride = entry.stride.get() as usize;
        if stride != F::stride(kind) {
            return Err(malformed(format!("{kind:?} has the wrong record size")));
        }

        let rows = entry.count.get();
        if rows == 0 || rows > MAX_ROWS {
            return Err(malformed(format!("{kind:?} has {rows} rows")));
        }

        let table_length = (stride as u64)
            .checked_mul(rows)
            .filter(|table_length| *table_length == entry.length.get())
            .ok_or_else(|| malformed(format!("{kind:?}'s length disagrees with its rows")))?;
        let offset = entry.offset.get();
        let end = offset
            .checked_add(table_length)
            .filter(|end| *end <= trailer_start as u64)
            .ok_or(ImageError::Truncated)?;
        let (Ok(offset), Ok(end)) = (usize::try_from(offset), usize::try_from(end)) else {
            return Err(ImageError::TooLarge);
        };

        if offset % format::TABLE_ALIGNMENT != 0 || offset < previous_end {
            return Err(malformed(format!("{kind:?} is misplaced")));
        }

        if bytes[previous_end..offset].iter().any(|byte| *byte != 0) {
            return Err(malformed("padding between tables is not zero"));
        }

        previous_end = end;
        tables[F::index(kind)] = Some(Placed {
            offset,
            length: end - offset,
        });
    }

    if bytes[previous_end..trailer_start]
        .iter()
        .any(|byte| *byte != 0)
    {
        return Err(malformed("padding before the trailer is not zero"));
    }

    Ok(tables)
}

/// The rows of one table, as a builder holds them until it seals.
pub enum TableBytes<'a> {
    Borrowed(&'a [u8]),
    Owned(Box<dyn AsBytes + Send + 'a>),
}

impl TableBytes<'_> {
    fn bytes(&self) -> &[u8] {
        match self {
            Self::Borrowed(bytes) => bytes,
            Self::Owned(rows) => rows.bytes(),
        }
    }
}

/// Owned rows, as bytes.
pub trait AsBytes {
    fn bytes(&self) -> &[u8];
}

impl<T: IntoBytes + Immutable> AsBytes for Vec<T> {
    fn bytes(&self) -> &[u8] {
        self.as_slice().as_bytes()
    }
}

/// Tables waiting to be sealed into an image of `F`.
pub struct Tables<'a, F: Format> {
    tables: Vec<(F::Kind, usize, usize, TableBytes<'a>)>,
}

impl<F: Format> Default for Tables<'_, F> {
    fn default() -> Self {
        Self::new()
    }
}

impl<'a, F: Format> Tables<'a, F> {
    pub const fn new() -> Self {
        Self { tables: Vec::new() }
    }

    /// Adds a table of `count` records of `stride` bytes. Empty tables are
    /// left out.
    pub fn add(&mut self, kind: F::Kind, stride: usize, count: usize, bytes: TableBytes<'a>) {
        if count == 0 {
            return;
        }

        assert!(
            self.tables.iter().all(|(added, ..)| *added != kind),
            "{kind:?} is added once"
        );
        self.tables.push((kind, stride, count, bytes));
    }

    /// Adds every table `other` holds: tables are laid out by kind,
    /// whatever order they are added in.
    pub fn absorb(&mut self, other: Self) {
        for (kind, stride, count, bytes) in other.tables {
            self.add(kind, stride, count, bytes);
        }
    }

    /// Lays out the tables and seals them with the trailer's checksum. The
    /// caller validates the result as a reader would.
    pub fn seal(mut self, target: TargetDescription) -> Result<AlignedBytes, ImageError> {
        self.tables.sort_by_key(|(kind, ..)| *kind);
        let directory_start = size_of::<Header>();
        let directory_length = self.tables.len() * size_of::<DirectoryEntry>();
        let mut offset =
            format::aligned(directory_start + directory_length).ok_or(ImageError::TooLarge)?;
        let mut entries = Vec::with_capacity(self.tables.len());
        let mut offsets = Vec::with_capacity(self.tables.len());
        for (kind, stride, count, bytes) in &self.tables {
            let bytes = bytes.bytes();
            offsets.push(offset);
            entries.push(DirectoryEntry {
                kind: F::number(*kind).into(),
                stride: u32::try_from(*stride)
                    .map_err(|_| ImageError::TooLarge)?
                    .into(),
                count: (*count as u64).into(),
                offset: (offset as u64).into(),
                length: (bytes.len() as u64).into(),
            });
            offset = format::aligned(
                offset
                    .checked_add(bytes.len())
                    .ok_or(ImageError::TooLarge)?,
            )
            .ok_or(ImageError::TooLarge)?;
        }

        let length = offset
            .checked_add(size_of::<Trailer>())
            .ok_or(ImageError::TooLarge)?;
        let header = header::<F>(target, length, entries.len())?;
        let mut bytes = AlignedBytes::zeroed(length).ok_or(ImageError::TooLarge)?;
        let buffer = bytes.as_mut_bytes();
        buffer[..directory_start].copy_from_slice(header.as_bytes());
        buffer[directory_start..directory_start + directory_length]
            .copy_from_slice(entries.as_bytes());

        // Each table is freed once copied, so the tables and the image
        // are not all held at once.
        for ((_, _, _, table), start) in self.tables.into_iter().zip(offsets) {
            let rows = table.bytes();
            buffer[start..start + rows.len()].copy_from_slice(rows);
        }

        let checksum = twox_hash::XxHash3_64::oneshot(&buffer[..offset]);
        buffer[offset..].copy_from_slice(
            Trailer {
                checksum: checksum.into(),
            }
            .as_bytes(),
        );

        Ok(bytes)
    }
}

/// A new image's header.
fn header<F: Format>(
    target: TargetDescription,
    length: usize,
    tables: usize,
) -> Result<Header, ImageError> {
    Ok(Header {
        magic: F::MAGIC,
        format: format::FORMAT_VERSION.into(),
        normalization: F::REVISION.into(),
        layout: F::fingerprint().into(),
        length: (length as u64).into(),
        tables: u32::try_from(tables)
            .map_err(|_| ImageError::TooLarge)?
            .into(),
        architecture: match target.architecture {
            Architecture::X86_64 => 1,
            Architecture::Aarch64 => 2,
        },
        byte_order: match target.byte_order {
            ByteOrder::Little => 1,
            ByteOrder::Big => 2,
        },
        address_size: target.pointer_width.bytes(),
        flags: 0,
        reserved: [0; 24],
    })
}

/// The target a header describes, or `None` for one it cannot.
pub const fn target_of(header: &Header) -> Option<TargetDescription> {
    let architecture = match header.architecture {
        1 => Architecture::X86_64,
        2 => Architecture::Aarch64,
        _ => return None,
    };

    let byte_order = match header.byte_order {
        1 => ByteOrder::Little,
        2 => ByteOrder::Big,
        _ => return None,
    };

    let pointer_width = match header.address_size {
        4 => PointerWidth::Bits32,
        8 => PointerWidth::Bits64,
        _ => return None,
    };

    Some(TargetDescription {
        architecture,
        byte_order,
        pointer_width,
    })
}

/// The target an image's header describes.
pub fn target(bytes: &[u8]) -> Result<TargetDescription, ImageError> {
    let (header, _) = Header::ref_from_prefix(bytes).map_err(|_| ImageError::Truncated)?;
    target_of(header).ok_or_else(|| malformed("the header names no target"))
}
