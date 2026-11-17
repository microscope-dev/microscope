use std::fmt::Debug;

use rayon::iter::{IndexedParallelIterator as _, IntoParallelIterator as _, ParallelIterator as _};
use zerocopy::{FromBytes as _, Immutable, IntoBytes};

use super::backing::AlignedBytes;
use super::format::{self, DirectoryEntry, Header, Trailer};
use super::validate::{ImageError, Limits};
use super::{MAX_ROWS, Placed};
use crate::{Architecture, ByteOrder, PointerWidth, TargetDescription};

pub trait Format {
    type Kind: Copy + Ord + Debug;

    const MAGIC: [u8; 8];

    const REVISION: u32;

    const KINDS: usize;

    fn fingerprint() -> u64;

    fn kind(number: u32) -> Option<Self::Kind>;

    fn number(kind: Self::Kind) -> u32;

    fn index(kind: Self::Kind) -> usize;

    fn stride(kind: Self::Kind) -> usize;
}

fn malformed(text: impl Into<String>) -> ImageError {
    ImageError::Malformed(text.into())
}

pub fn structure<F: Format, const N: usize>(
    bytes: &[u8],
    limits: Limits,
) -> Result<[Option<Placed>; N], ImageError> {
    checked_structure::<F, N>(bytes, limits, true)
}

pub fn sealed_structure<F: Format, const N: usize>(
    bytes: &[u8],
    limits: Limits,
) -> Result<[Option<Placed>; N], ImageError> {
    checked_structure::<F, N>(bytes, limits, false)
}

fn checked_structure<F: Format, const N: usize>(
    bytes: &[u8],
    limits: Limits,
    checksum: bool,
) -> Result<[Option<Placed>; N], ImageError> {
    const { assert!(N > 0) };
    assert_eq!(N, F::KINDS, "a directory has one slot per kind");
    let header = check_header::<F>(bytes, limits)?;
    let trailer_start = bytes
        .len()
        .checked_sub(size_of::<Trailer>())
        .filter(|start| *start >= size_of::<Header>())
        .ok_or(ImageError::Truncated)?;
    let (body, trailer) = bytes
        .split_at_checked(trailer_start)
        .ok_or(ImageError::Truncated)?;
    let trailer = Trailer::ref_from_bytes(trailer).map_err(|_| ImageError::Truncated)?;
    if checksum && trailer.checksum.get() != twox_hash::XxHash3_64::oneshot(body) {
        return Err(ImageError::Checksum);
    }

    check_directory::<F, N>(bytes, header, trailer_start)
}

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

    let entries = bytes
        .get(size_of::<Header>()..directory_end)
        .and_then(|directory| <[DirectoryEntry]>::ref_from_bytes(directory).ok())
        .ok_or(ImageError::Truncated)?;
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

        let padding = bytes
            .get(previous_end..offset)
            .ok_or(ImageError::Truncated)?;
        if padding.iter().any(|byte| *byte != 0) {
            return Err(malformed("padding between tables is not zero"));
        }

        previous_end = end;
        let slot = tables.get_mut(F::index(kind));
        debug_assert!(slot.is_some(), "{kind:?}'s index is below {N}");
        *slot.ok_or_else(|| malformed(format!("{kind:?} has no slot")))? = Some(Placed {
            offset,
            length: end - offset,
        });
    }

    let padding = bytes
        .get(previous_end..trailer_start)
        .ok_or(ImageError::Truncated)?;
    if padding.iter().any(|byte| *byte != 0) {
        return Err(malformed("padding before the trailer is not zero"));
    }

    Ok(tables)
}

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

pub trait AsBytes {
    fn bytes(&self) -> &[u8];
}

impl<T: IntoBytes + Immutable> AsBytes for Vec<T> {
    fn bytes(&self) -> &[u8] {
        self.as_slice().as_bytes()
    }
}

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

    pub fn absorb(&mut self, other: Self) {
        for (kind, stride, count, bytes) in other.tables {
            self.add(kind, stride, count, bytes);
        }
    }

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
        copy_at(buffer, 0, header.as_bytes());
        copy_at(buffer, directory_start, entries.as_bytes());

        let mut rest = &mut *buffer;
        let mut consumed = 0;
        let mut slots = Vec::with_capacity(self.tables.len());
        for ((_, _, _, table), start) in self.tables.iter().zip(&offsets) {
            let length = table.bytes().len();
            let (_, after) = std::mem::take(&mut rest)
                .split_at_mut_checked(start - consumed)
                .ok_or(ImageError::TooLarge)?;
            let (slot, after) = after
                .split_at_mut_checked(length)
                .ok_or(ImageError::TooLarge)?;
            slots.push(slot);
            rest = after;
            consumed = start + length;
        }

        self.tables
            .into_iter()
            .map(|(_, _, _, table)| table)
            .collect::<Vec<_>>()
            .into_par_iter()
            .zip(slots)
            .for_each(|(table, slot)| slot.copy_from_slice(table.bytes()));
        let buffer = bytes.as_mut_bytes();

        let (body, trailer) = buffer
            .split_at_mut_checked(offset)
            .ok_or(ImageError::TooLarge)?;
        let checksum = twox_hash::XxHash3_64::oneshot(body);
        trailer.copy_from_slice(
            Trailer {
                checksum: checksum.into(),
            }
            .as_bytes(),
        );

        Ok(bytes)
    }
}

#[cfg(any(test, feature = "fuzzing"))]
#[allow(clippy::indexing_slicing, reason = "callers pass a whole image")]
pub fn reseal(bytes: &mut [u8]) {
    let end = bytes.len() - size_of::<Trailer>();
    let checksum = twox_hash::XxHash3_64::oneshot(&bytes[..end]);
    bytes[end..].copy_from_slice(
        Trailer {
            checksum: checksum.into(),
        }
        .as_bytes(),
    );
}

#[expect(
    clippy::indexing_slicing,
    reason = "`Tables::seal` sizes the buffer from the offsets and lengths it copies at"
)]
fn copy_at(buffer: &mut [u8], start: usize, source: &[u8]) {
    debug_assert!(
        start
            .checked_add(source.len())
            .is_some_and(|end| end <= buffer.len()),
        "{} bytes at {start} overrun a {}-byte image",
        source.len(),
        buffer.len()
    );
    buffer[start..start + source.len()].copy_from_slice(source);
}

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

pub fn target(bytes: &[u8]) -> Result<TargetDescription, ImageError> {
    let (header, _) = Header::ref_from_prefix(bytes).map_err(|_| ImageError::Truncated)?;
    target_of(header).ok_or_else(|| malformed("the header names no target"))
}
