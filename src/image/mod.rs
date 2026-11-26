pub mod calls;
pub mod declarations;
pub mod facts;
pub mod functions;
pub mod index;
pub mod lines;
pub mod locations;
pub mod packages;
pub mod python;
pub mod resumes;
mod schema;
pub mod symbols;
pub mod type_facts;
pub mod types;
pub mod unwind;
pub mod v8;
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
pub use microscope_data::image::{MAX_ROWS, NONE, Placed, backing, container, format, strings};
pub use strings::{PathId, Paths, PathsBuilder, Strings, StringsBuilder};

pub use microscope_data::image::{ImageError, Limits};

use container::{Format, TableBytes, Tables};

fn ids(rows: usize) -> Range<u32> {
    let end = u32::try_from(rows).unwrap_or(NONE);
    debug_assert!(end < NONE, "a table of {rows} rows passed validation");
    0..end
}

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

    #[must_use]
    pub const fn range(self) -> Range<usize> {
        let first = self.first.get() as usize;
        first..first + self.count.get() as usize
    }

    #[must_use]
    pub fn within(self, length: usize) -> bool {
        (self.first.get() as usize)
            .checked_add(self.count.get() as usize)
            .is_some_and(|end| end <= length)
    }

    #[must_use]
    pub const fn cast<U>(self) -> Span<U> {
        Span::new(self.first.get(), self.count.get())
    }

    #[must_use]
    #[expect(
        clippy::indexing_slicing,
        reason = "views call this only on spans of validated records, and each family's \
                  validation checks its spans with `within` against the table they name"
    )]
    pub fn rows(self, table: &[T]) -> &[T] {
        debug_assert!(
            self.within(table.len()),
            "{self:?} lies outside a table of {} rows",
            table.len()
        );
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

pub trait Record: FromBytes + IntoBytes + KnownLayout + Immutable + Unaligned {
    const KIND: TableKind;
}

pub trait SharedRecord: FromBytes + IntoBytes + KnownLayout + Immutable + Unaligned {
    const NAME: &'static str;
}

pub fn revisions() -> [u64; 3] {
    [
        format::FORMAT_VERSION.into(),
        format::NORMALIZATION_REVISION.into(),
        schema::layout_fingerprint(),
    ]
}

#[derive(Debug)]
pub struct Image {
    bytes: AlignedBytes,
    tables: [Option<Placed>; TableKind::COUNT],
    target: crate::TargetDescription,
}

impl Image {
    pub fn from_bytes(bytes: AlignedBytes, limits: Limits) -> Result<Self, ImageError> {
        Self::checked(bytes, limits, true)
    }

    fn from_sealed(bytes: AlignedBytes, limits: Limits) -> Result<Self, ImageError> {
        Self::checked(bytes, limits, false)
    }

    fn checked(bytes: AlignedBytes, limits: Limits, checksum: bool) -> Result<Self, ImageError> {
        let tables = if checksum {
            validate::structure(&bytes, limits)?
        } else {
            validate::sealed_structure(&bytes, limits)?
        };
        let target = container::target(&bytes)?;
        let image = Self {
            bytes,
            tables,
            target,
        };

        validate::contents(&image)?;
        image.bytes.release();
        Ok(image)
    }

    pub const fn target(&self) -> crate::TargetDescription {
        self.target
    }

    pub fn as_bytes(&self) -> &[u8] {
        self.bytes.as_bytes()
    }

    pub fn table<T: Record>(&self) -> &[T] {
        let Some(placed) = self.placed(T::KIND) else {
            return &[];
        };

        self.rows(placed)
    }

    pub fn shared<T: SharedRecord>(&self, kind: TableKind) -> &[T] {
        assert_eq!(
            schema::record(kind),
            T::NAME,
            "{kind:?} holds another record"
        );
        let Some(placed) = self.placed(kind) else {
            return &[];
        };

        self.rows(placed)
    }

    #[expect(
        clippy::indexing_slicing,
        reason = "`TableKind::index` is below `TableKind::COUNT` for every kind"
    )]
    const fn placed(&self, kind: TableKind) -> Option<Placed> {
        self.tables[kind.index()]
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

    pub fn strings(&self) -> Strings<'_> {
        Strings(self.bytes(TableKind::Strings))
    }

    pub fn bytes(&self, kind: TableKind) -> &[u8] {
        let Some(placed) = self.placed(kind) else {
            return &[];
        };

        let bytes = self.bytes.get(placed.offset..placed.offset + placed.length);
        debug_assert!(
            bytes.is_some(),
            "validation checked every table lies within the image"
        );

        bytes.unwrap_or_default()
    }
}

pub struct Builder<'a> {
    target: crate::TargetDescription,
    tables: Tables<'a, DebugImage>,
}

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

    pub fn bytes(&mut self, kind: TableKind, bytes: Vec<u8>) -> &mut Self {
        if !bytes.is_empty() {
            let length = bytes.len();
            self.add(kind, 1, length, TableBytes::Owned(Box::new(bytes)));
        }

        self
    }

    pub fn absorb(&mut self, other: Self) -> &mut Self {
        self.tables.absorb(other.tables);
        self
    }

    fn add(&mut self, kind: TableKind, stride: usize, count: usize, bytes: TableBytes<'a>) {
        self.tables.add(kind, stride, count, bytes);
    }

    pub fn seal(self, limits: Limits) -> Result<Image, ImageError> {
        Image::from_sealed(self.tables.seal(self.target)?, limits)
    }
}
