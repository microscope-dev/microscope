//! A module's static metadata as one buffer of typed tables.
//!
//! An image is a header, a directory, and a fixed set of tables, each a
//! slice of one little-endian record type that nothing in the buffer
//! points into: records refer to each other by index. The bytes in memory
//! are the bytes a cache writes to disk, and an image read back is
//! validated completely before any view reads it, so that after
//! validation every lookup is an in-bounds index.
//!
//! The container and its tables are neutral: lines, files, and the other
//! debugger concepts. What a provider needs that no other part of the
//! debugger reads stays in the provider.

pub mod backing;
pub mod calls;
pub mod container;
pub mod declarations;
pub mod facts;
mod format;
pub mod functions;
pub mod index;
pub mod lines;
pub mod locations;
pub mod packages;
pub mod python;
pub mod resumes;
mod schema;
mod strings;
pub mod symbols;
pub mod type_facts;
pub mod types;
pub mod unwind;
mod validate;
pub mod variables;

#[cfg(any(test, feature = "fuzzing"))]
pub mod sample;
#[cfg(test)]
mod tests;

use std::marker::PhantomData;
use std::ops::Range;

use zerocopy::little_endian::U32;
use zerocopy::{FromBytes, Immutable, IntoBytes, KnownLayout, Unaligned};

pub use backing::AlignedBytes;
pub use format::TableKind;
pub use strings::{
    PathId, Paths, PathsBuilder, StrId, Strings, StringsBuilder, valid_pool, valid_reference,
};

pub use validate::{ImageError, Limits};

use container::{Format, TableBytes, Tables};

/// The index that stands for no row in any table's reference column.
pub const NONE: u32 = u32::MAX;

/// The ids of a table of `rows` rows, in order. Validation bounds every
/// table below [`NONE`] rows, as ids are `u32`s.
fn ids(rows: usize) -> Range<u32> {
    let end = u32::try_from(rows).unwrap_or(NONE);
    debug_assert!(end < NONE, "a table of {rows} rows passed validation");
    0..end
}

/// The `count` rows of one table from row `first`, which a record owns.
/// `T` is what the rows are, so a span reads only its own table.
#[repr(C)]
#[derive(FromBytes, IntoBytes, KnownLayout, Immutable, Unaligned)]
pub struct Span<T> {
    first: U32,
    count: U32,
    rows: PhantomData<T>,
}

impl<T> Span<T> {
    pub const EMPTY: Self = Self::new(0, 0);

    #[must_use]
    pub const fn new(first: u32, count: u32) -> Self {
        Self {
            first: U32::new(first),
            count: U32::new(count),
            rows: PhantomData,
        }
    }

    #[must_use]
    pub const fn first(self) -> u32 {
        self.first.get()
    }

    #[must_use]
    pub const fn count(self) -> u32 {
        self.count.get()
    }

    /// The rows' indexes.
    #[must_use]
    pub const fn range(self) -> Range<usize> {
        let first = self.first.get() as usize;
        first..first + self.count.get() as usize
    }

    /// Whether the rows lie within a table of `length` rows.
    #[must_use]
    pub fn within(self, length: usize) -> bool {
        (self.first.get() as usize)
            .checked_add(self.count.get() as usize)
            .is_some_and(|end| end <= length)
    }

    /// The same rows, as `U`s: for a span whose table its record's kind
    /// says.
    #[must_use]
    pub const fn cast<U>(self) -> Span<U> {
        Span::new(self.first.get(), self.count.get())
    }

    /// The span's rows of `table`, which validation checked hold them.
    #[must_use]
    pub fn rows(self, table: &[T]) -> &[T] {
        &table[self.range()]
    }
}

impl<T> Clone for Span<T> {
    fn clone(&self) -> Self {
        *self
    }
}

impl<T> Copy for Span<T> {}

impl<T> PartialEq for Span<T> {
    fn eq(&self, other: &Self) -> bool {
        (self.first, self.count) == (other.first, other.count)
    }
}

impl<T> Eq for Span<T> {}

impl<T> std::hash::Hash for Span<T> {
    fn hash<H: std::hash::Hasher>(&self, state: &mut H) {
        (self.first, self.count).hash(state);
    }
}

impl<T> std::fmt::Debug for Span<T> {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(formatter, "Span({}+{})", self.first.get(), self.count.get())
    }
}

/// The most rows one table may hold: every index but [`NONE`].
pub const MAX_ROWS: u64 = NONE as u64;

/// One record type: the table it fills.
pub trait Record: FromBytes + IntoBytes + KnownLayout + Immutable + Unaligned {
    const KIND: TableKind;
}

/// A record type several tables hold, such as an index's entries.
pub trait SharedRecord: FromBytes + IntoBytes + KnownLayout + Immutable + Unaligned {
    /// The record's name in the schema.
    const NAME: &'static str;
}

/// Where one table is in an image.
#[derive(Debug, Clone, Copy)]
pub struct Placed {
    pub(crate) offset: usize,
    pub(crate) length: usize,
}

/// What an image's bytes mean beyond the inputs they were built from: the
/// format, the normalization revision, and every record's layout.
pub fn revisions() -> [u64; 3] {
    [
        format::FORMAT_VERSION.into(),
        format::NORMALIZATION_REVISION.into(),
        schema::layout_fingerprint(),
    ]
}

/// Validated, immutable tables over owned bytes.
#[derive(Debug)]
pub struct Image {
    bytes: AlignedBytes,
    tables: [Option<Placed>; TableKind::COUNT],
    target: crate::TargetDescription,
}

impl Image {
    /// Validates `bytes` as an image and takes them, or reports the first
    /// problem found.
    pub fn from_bytes(bytes: AlignedBytes, limits: Limits) -> Result<Self, ImageError> {
        let tables = validate::structure(&bytes, limits)?;
        let target = container::target(&bytes)?;
        let image = Self {
            bytes,
            tables,
            target,
        };

        validate::contents(&image)?;
        Ok(image)
    }

    /// The target the image describes.
    pub const fn target(&self) -> crate::TargetDescription {
        self.target
    }

    /// The whole image, as a cache writes it.
    pub fn as_bytes(&self) -> &[u8] {
        self.bytes.as_bytes()
    }

    /// The rows of `T`'s table: empty when the image has none.
    pub fn table<T: Record>(&self) -> &[T] {
        let Some(placed) = self.tables[T::KIND.index()] else {
            return &[];
        };

        self.rows(placed)
    }

    /// The rows of the table of `kind`, which holds `T`s: empty when the
    /// image has none.
    pub fn shared<T: SharedRecord>(&self, kind: TableKind) -> &[T] {
        assert_eq!(
            schema::record(kind),
            T::NAME,
            "{kind:?} holds another record"
        );
        let Some(placed) = self.tables[kind.index()] else {
            return &[];
        };

        self.rows(placed)
    }

    fn rows<T: FromBytes + Immutable + KnownLayout>(&self, placed: Placed) -> &[T] {
        let rows = self
            .bytes
            .get(placed.offset..placed.offset + placed.length)
            .and_then(|bytes| <[T]>::ref_from_bytes(bytes).ok());
        debug_assert!(
            rows.is_some(),
            "validation checked every table's length and stride"
        );
        rows.unwrap_or_default()
    }

    /// The string pool.
    pub fn strings(&self) -> Strings<'_> {
        Strings(self.bytes(TableKind::Strings))
    }

    /// The bytes of a byte table, such as the string pool.
    pub fn bytes(&self, kind: TableKind) -> &[u8] {
        self.tables[kind.index()].map_or(&[], |placed| {
            &self.bytes[placed.offset..placed.offset + placed.length]
        })
    }
}

/// Assembles tables into an image.
///
/// A table is either borrowed from rows that outlive the builder or moved
/// into it, never copied: sealing copies each table once, into its place
/// in the image. Copying rows as they were added cost a second copy, and
/// a second allocation, of the whole image while it was sealed.
pub struct Builder<'a> {
    target: crate::TargetDescription,
    tables: Tables<'a, DebugImage>,
}

/// Debug information as an image: the format of [`Image`].
#[derive(Debug)]
pub struct DebugImage;

impl Format for DebugImage {
    type Kind = TableKind;
    const MAGIC: [u8; 8] = format::MAGIC;
    const REVISION: u32 = format::NORMALIZATION_REVISION;
    const KINDS: usize = TableKind::COUNT;

    fn fingerprint() -> u64 {
        schema::layout_fingerprint()
    }

    fn kind(number: u32) -> Option<TableKind> {
        TableKind::from_number(number)
    }

    fn number(kind: TableKind) -> u32 {
        kind as u32
    }

    fn index(kind: TableKind) -> usize {
        kind.index()
    }

    fn stride(kind: TableKind) -> usize {
        schema::stride(kind)
    }
}

impl<'a> Builder<'a> {
    pub const fn new(target: crate::TargetDescription) -> Self {
        Self {
            target,
            tables: Tables::new(),
        }
    }

    /// Adds `T`'s table, borrowing its rows. Empty tables are left out.
    pub fn table<T: Record>(&mut self, rows: &'a [T]) -> &mut Self {
        if !rows.is_empty() {
            self.add(
                T::KIND,
                size_of::<T>(),
                rows.len(),
                TableBytes::Borrowed(rows.as_bytes()),
            );
        }

        self
    }

    /// Adds `T`'s table, taking its rows. Empty tables are left out.
    pub fn owned_table<T: Record + Send + 'a>(&mut self, rows: Vec<T>) -> &mut Self {
        if !rows.is_empty() {
            let count = rows.len();
            self.add(
                T::KIND,
                size_of::<T>(),
                count,
                TableBytes::Owned(Box::new(rows)),
            );
        }

        self
    }

    /// Adds the table of `kind`, which holds `T`s, borrowing its rows.
    /// Empty tables are left out.
    pub fn shared<T: SharedRecord>(&mut self, kind: TableKind, rows: &'a [T]) -> &mut Self {
        assert_eq!(
            schema::record(kind),
            T::NAME,
            "{kind:?} holds another record"
        );
        if !rows.is_empty() {
            self.add(
                kind,
                size_of::<T>(),
                rows.len(),
                TableBytes::Borrowed(rows.as_bytes()),
            );
        }

        self
    }

    /// Adds the table of `kind`, which holds `T`s, taking its rows. Empty
    /// tables are left out.
    pub fn owned_shared<T: SharedRecord + Send + 'a>(
        &mut self,
        kind: TableKind,
        rows: Vec<T>,
    ) -> &mut Self {
        assert_eq!(
            schema::record(kind),
            T::NAME,
            "{kind:?} holds another record"
        );
        if !rows.is_empty() {
            let count = rows.len();
            self.add(
                kind,
                size_of::<T>(),
                count,
                TableBytes::Owned(Box::new(rows)),
            );
        }

        self
    }

    /// Adds a byte table, such as the string pool.
    pub fn bytes(&mut self, kind: TableKind, bytes: Vec<u8>) -> &mut Self {
        if !bytes.is_empty() {
            let length = bytes.len();
            self.add(kind, 1, length, TableBytes::Owned(Box::new(bytes)));
        }

        self
    }

    /// Adds every table `other` holds, which another thread may have built:
    /// tables are laid out by kind, whatever order they are added in.
    pub fn absorb(&mut self, other: Self) -> &mut Self {
        self.tables.absorb(other.tables);
        self
    }

    fn add(&mut self, kind: TableKind, stride: usize, count: usize, bytes: TableBytes<'a>) {
        self.tables.add(kind, stride, count, bytes);
    }

    /// Lays out the tables, seals them with the trailer's checksum, and
    /// validates the result as a reader would.
    pub fn seal(self, limits: Limits) -> Result<Image, ImageError> {
        Image::from_bytes(self.tables.seal(self.target)?, limits)
    }
}
