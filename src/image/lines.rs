//! Source files and line tables.
//!
//! Every row a line program emits is kept, with its address in a column of
//! its own and its location and flags in a 12-byte record; the rare parts
//! of a row, such as a discriminator or a line too long for 32 bits, are in
//! a side table by row. Sequences are runs of rows. The ranges of code each
//! line describes refer to the row whose location they take, and are kept
//! in interval order, each with the greatest end of it and every range
//! before it; since each row opens at most one range, in order, a range's
//! row is also its place in the line program. Two indexes are persisted
//! with them: statement rows by file and line, and the rows that mark a
//! prologue's end or an epilogue's start, by address. Each index refers to
//! its records by row, so validation checks it against them in one pass.

use std::path::PathBuf;

use rayon::slice::ParallelSliceMut as _;
use zerocopy::little_endian::{U16, U32, U64};
use zerocopy::{FromBytes, Immutable, IntoBytes, KnownLayout, Unaligned};

use super::{NONE, Record, TableKind};
use crate::{
    AddressRange, ColumnNumber, ImageAddress, LineNumber, LineSequenceId, SourceFileId,
    SourceLocation, StatementFlags, StatementRow,
};

/// A source file, by its resolved path.
#[repr(C)]
#[derive(
    Debug, Clone, Copy, PartialEq, Eq, FromBytes, IntoBytes, KnownLayout, Immutable, Unaligned,
)]
pub struct FileRecord {
    pub path: U32,
}

impl Record for FileRecord {
    const KIND: TableKind = TableKind::Files;
}

/// One line-program row's address.
#[repr(C)]
#[derive(
    Debug, Clone, Copy, PartialEq, Eq, FromBytes, IntoBytes, KnownLayout, Immutable, Unaligned,
)]
pub struct RowAddress {
    pub address: U64,
}

impl Record for RowAddress {
    const KIND: TableKind = TableKind::LineAddresses;
}

/// One line-program row's location and flags. A row without a file has
/// line and column zero; a line or column too wide for its field is
/// [`WIDE_LINE`] or [`WIDE_COLUMN`] here and whole in the row's extra.
#[repr(C)]
#[derive(
    Debug, Clone, Copy, PartialEq, Eq, FromBytes, IntoBytes, KnownLayout, Immutable, Unaligned,
)]
pub struct LineRow {
    /// The file, or [`NONE`].
    pub file: U32,

    /// The line, zero for none.
    pub line: U32,

    /// The column, zero for the line's left edge.
    pub column: U16,
    pub flags: u8,
    pub reserved: u8,
}

impl Record for LineRow {
    const KIND: TableKind = TableKind::LineRows;
}

/// A line too wide for [`LineRow::line`].
pub const WIDE_LINE: u32 = u32::MAX;

/// A column too wide for [`LineRow::column`].
pub const WIDE_COLUMN: u16 = u16::MAX;

/// [`LineRow::flags`].
pub mod row_flags {
    pub const STATEMENT: u8 = 1 << 0;

    pub const PROLOGUE_END: u8 = 1 << 1;

    pub const EPILOGUE_BEGIN: u8 = 1 << 2;

    /// The row ends its sequence and describes no code of its own.
    pub const END_SEQUENCE: u8 = 1 << 3;

    /// The row has an entry in the extras table.
    pub const EXTRA: u8 = 1 << 4;

    pub const ALL: u8 = STATEMENT | PROLOGUE_END | EPILOGUE_BEGIN | END_SEQUENCE | EXTRA;
}

/// What a row has that most rows do not.
#[repr(C)]
#[derive(
    Debug, Clone, Copy, PartialEq, Eq, FromBytes, IntoBytes, KnownLayout, Immutable, Unaligned,
)]
pub struct LineExtra {
    pub row: U32,
    pub operation_index: U64,
    pub discriminator: U64,
    pub isa: U64,

    /// The whole line, when the row's is [`WIDE_LINE`].
    pub line: U64,

    /// The whole column, when the row's is [`WIDE_COLUMN`].
    pub column: U64,
}

impl Record for LineExtra {
    const KIND: TableKind = TableKind::LineExtras;
}

/// One sequence: a run of rows, the last of which ends it.
#[repr(C)]
#[derive(
    Debug, Clone, Copy, PartialEq, Eq, FromBytes, IntoBytes, KnownLayout, Immutable, Unaligned,
)]
pub struct LineSequence {
    pub first: U32,
    pub rows: U32,
}

impl Record for LineSequence {
    const KIND: TableKind = TableKind::LineSequences;
}

/// The code one line describes, with the row whose location it has. The
/// table is in (start, end, row) order.
#[repr(C)]
#[derive(
    Debug, Clone, Copy, PartialEq, Eq, FromBytes, IntoBytes, KnownLayout, Immutable, Unaligned,
)]
pub struct LineRange {
    pub start: U64,
    pub end: U64,

    /// The greatest end of this range and every range before it, which
    /// ends a backward search for the ranges containing an address.
    pub prefix_max_end: U64,
    pub row: U32,

    /// Whether the range begins at a recommended breakpoint location.
    pub statement: u8,
}

impl Record for LineRange {
    const KIND: TableKind = TableKind::LineRanges;
}

/// A statement row with a location, in (file, line, row) order.
#[repr(C)]
#[derive(
    Debug, Clone, Copy, PartialEq, Eq, FromBytes, IntoBytes, KnownLayout, Immutable, Unaligned,
)]
pub struct StatementKey {
    pub file: U32,
    pub line: U64,
    pub row: U32,
}

impl Record for StatementKey {
    const KIND: TableKind = TableKind::StatementIndex;
}

/// A row that marks a prologue's end or an epilogue's start, in (address,
/// row) order.
#[repr(C)]
#[derive(
    Debug, Clone, Copy, PartialEq, Eq, FromBytes, IntoBytes, KnownLayout, Immutable, Unaligned,
)]
pub struct ControlBoundary {
    pub row: U32,
}

impl Record for ControlBoundary {
    const KIND: TableKind = TableKind::ControlBoundaries;
}

const _: () = assert!(size_of::<LineRow>() == 12);
const _: () = assert!(size_of::<RowAddress>() == 8);
const _: () = assert!(size_of::<LineRange>() == 29);

/// One row as a line program describes it, before it is encoded.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
#[expect(
    clippy::struct_excessive_bools,
    reason = "a line-program row's flags, as DWARF names them"
)]
pub struct Row {
    pub address: u64,
    pub file: Option<SourceFileId>,

    /// Zero when the row names no line.
    pub line: u64,

    /// Zero for the line's left edge.
    pub column: u64,
    pub operation_index: u64,
    pub discriminator: u64,
    pub isa: u64,
    pub statement: bool,
    pub prologue_end: bool,
    pub epilogue_begin: bool,
    pub end_sequence: bool,
}

impl Row {
    /// The row's location: none without a file or a line.
    pub fn location(&self) -> Option<SourceLocation> {
        Some(SourceLocation {
            file: self.file?,
            line: LineNumber::new(self.line)?,
            column: ColumnNumber::new(self.column),
        })
    }

    /// Whether the row is one of the public statement rows: it has a
    /// location or marks a prologue's end or an epilogue's start.
    pub fn is_public(&self) -> bool {
        !self.end_sequence
            && (self.location().is_some() || self.prologue_end || self.epilogue_begin)
    }
}

/// Line tables being built: rows by sequence, then ranges. Indexes are
/// built as the tables are sealed.
#[derive(Debug, Default, Clone)]
pub struct LineTables {
    pub addresses: Vec<RowAddress>,
    pub rows: Vec<LineRow>,
    pub extras: Vec<LineExtra>,
    pub sequences: Vec<LineSequence>,
    pub ranges: Vec<LineRange>,
}

/// Why line tables could not be built.
#[derive(Debug, Clone, Copy, PartialEq, Eq, thiserror::Error)]
#[error("the line tables hold more than {} rows or ranges", super::MAX_ROWS)]
pub struct TooManyRows;

fn index(count: usize) -> Result<u32, TooManyRows> {
    u32::try_from(count)
        .ok()
        .filter(|index| *index != NONE)
        .ok_or(TooManyRows)
}

impl LineTables {
    /// Where public rows end prologues, in row order.
    pub fn prologue_ends(&self) -> Vec<ImageAddress> {
        self.rows
            .iter()
            .zip(&self.addresses)
            .filter(|(row, _)| is_public(row) && row.flags & row_flags::PROLOGUE_END != 0)
            .map(|(_, address)| ImageAddress::new(address.address.get()))
            .collect()
    }

    /// Starts a sequence; rows pushed until the next start belong to it.
    pub fn begin_sequence(&mut self) -> Result<(), TooManyRows> {
        let first = index(self.rows.len())?;
        index(self.sequences.len())?;
        self.sequences.push(LineSequence {
            first: first.into(),
            rows: 0.into(),
        });
        Ok(())
    }

    /// Adds a row to the current sequence and returns its index.
    pub fn push_row(&mut self, row: &Row) -> Result<u32, TooManyRows> {
        let at = index(self.rows.len())?;
        let Some(sequence) = self.sequences.last_mut() else {
            debug_assert!(false, "row {at} was pushed before any sequence began");
            self.begin_sequence()?;
            return self.push_row(row);
        };

        sequence.rows = (sequence.rows.get() + 1).into();
        let line = u32::try_from(row.line)
            .ok()
            .filter(|line| *line != WIDE_LINE);
        let column = u16::try_from(row.column)
            .ok()
            .filter(|column| *column != WIDE_COLUMN);
        let extra = row.operation_index != 0
            || row.discriminator != 0
            || row.isa != 0
            || line.is_none()
            || column.is_none();

        let mut flags = 0;
        for (set, flag) in [
            (row.statement, row_flags::STATEMENT),
            (row.prologue_end, row_flags::PROLOGUE_END),
            (row.epilogue_begin, row_flags::EPILOGUE_BEGIN),
            (row.end_sequence, row_flags::END_SEQUENCE),
            (extra, row_flags::EXTRA),
        ] {
            if set {
                flags |= flag;
            }
        }

        let file = row.file.map_or(NONE, SourceFileId::get);

        self.addresses.push(RowAddress {
            address: row.address.into(),
        });
        self.rows.push(LineRow {
            file: file.into(),
            line: line.unwrap_or(WIDE_LINE).into(),
            column: column.unwrap_or(WIDE_COLUMN).into(),
            flags,
            reserved: 0,
        });

        if extra {
            self.extras.push(LineExtra {
                row: at.into(),
                operation_index: row.operation_index.into(),
                discriminator: row.discriminator.into(),
                isa: row.isa.into(),
                line: row.line.into(),
                column: row.column.into(),
            });
        }

        Ok(at)
    }

    /// Adds the range of code a line describes, taking the location of
    /// `row`.
    pub fn push_range(
        &mut self,
        range: AddressRange<ImageAddress>,
        row: u32,
        statement: bool,
    ) -> Result<(), TooManyRows> {
        index(self.ranges.len())?;
        self.ranges.push(LineRange {
            start: range.start.get().into(),
            end: range.end.get().into(),
            prefix_max_end: 0.into(),
            row: row.into(),
            statement: u8::from(statement),
        });
        Ok(())
    }

    /// Appends `other`'s tables, renumbering its rows, and its files by
    /// `file`.
    pub fn append(
        &mut self,
        other: &Self,
        file: impl Fn(SourceFileId) -> SourceFileId,
    ) -> Result<(), TooManyRows> {
        let base = index(self.rows.len())?;
        index(self.rows.len() + other.rows.len())?;
        index(self.ranges.len() + other.ranges.len())?;
        self.addresses.extend_from_slice(&other.addresses);
        self.rows.extend(other.rows.iter().map(|row| {
            LineRow {
                file: if row.file.get() == NONE {
                    NONE
                } else {
                    file(SourceFileId::new(row.file.get())).get()
                }
                .into(),
                ..*row
            }
        }));
        self.extras
            .extend(other.extras.iter().map(|extra| LineExtra {
                row: (extra.row.get() + base).into(),
                ..*extra
            }));
        self.sequences
            .extend(other.sequences.iter().map(|sequence| LineSequence {
                first: (sequence.first.get() + base).into(),
                rows: sequence.rows,
            }));
        self.ranges
            .extend(other.ranges.iter().map(|range| LineRange {
                row: (range.row.get() + base).into(),
                ..*range
            }));
        Ok(())
    }

    /// Ends each line range where a function symbol begins inside it. A
    /// line program describes every function it covers from the function's
    /// first instruction, but its last row before code it does not
    /// describe, such as hand-written assembly placed after a compiled
    /// function, runs on to the next row: that code has no source line.
    pub fn clip_at(&mut self, function_starts: &[ImageAddress]) {
        debug_assert!(
            function_starts.is_sorted_by(|earlier, later| earlier < later),
            "the {} function starts that clip line ranges are not in strict order",
            function_starts.len()
        );
        for range in &mut self.ranges {
            let start = ImageAddress::new(range.start.get());
            let end = ImageAddress::new(range.end.get());
            let after = function_starts.partition_point(|function| *function <= start);
            if let Some(&function) = function_starts.get(after)
                && function < end
            {
                range.end = function.get().into();
            }
        }
    }

    /// Row `index`, decoded.
    pub fn row(&self, index: usize) -> Row {
        decode_row(&self.addresses, &self.rows, &self.extras, index)
    }

    /// Every public statement row, decoded, in order.
    #[cfg(test)]
    pub fn statement_rows(&self) -> Vec<StatementRow> {
        statement_rows(&self.addresses, &self.rows, &self.extras, &self.sequences).collect()
    }

    /// The public statement rows by address, which the loader's analyses
    /// of code read before the tables are sealed.
    pub fn statements_by_address(&self) -> StatementsByAddress<'_> {
        let mut order = self
            .sequences
            .iter()
            .zip(0u32..)
            .flat_map(|(run, sequence)| {
                let first = run.first.get();
                (first..first + run.rows.get())
                    .filter(|index| is_public(&self.rows[*index as usize]))
                    .map(move |index| (index, sequence))
            })
            .collect::<Vec<_>>();

        // Stable, so rows at one address keep line-program order.
        order.sort_by_key(|(index, _)| self.addresses[*index as usize].address.get());
        StatementsByAddress {
            tables: self,
            order,
        }
    }

    /// Every line range, decoded, in order.
    #[cfg(test)]
    pub fn line_entries(&self) -> Vec<crate::model::LineEntry> {
        self.ranges
            .iter()
            .map(|range| {
                line_entry(&self.addresses, &self.rows, &self.extras, range)
                    .expect("every range has a location")
            })
            .collect()
    }
}

/// The public statement rows of [`LineTables`] by address, keeping
/// line-program order among rows at one address, each decoded when asked.
#[derive(Debug)]
pub struct StatementsByAddress<'a> {
    tables: &'a LineTables,

    /// Each public row's index and its sequence's.
    order: Vec<(u32, u32)>,
}

impl StatementsByAddress<'_> {
    fn address(&self, (index, _): (u32, u32)) -> ImageAddress {
        ImageAddress::new(self.tables.addresses[index as usize].address.get())
    }

    fn row(&self, (index, sequence): (u32, u32)) -> StatementRow {
        let tables = self.tables;
        let row = decode_row(
            &tables.addresses,
            &tables.rows,
            &tables.extras,
            index as usize,
        );
        let ordinal = index - tables.sequences[sequence as usize].first.get();
        statement_row(&row, sequence, ordinal)
    }

    /// The line of the row whose code holds `address`: the last row at or
    /// before it, unless that row has no line.
    pub fn line_at(&self, address: ImageAddress) -> Option<crate::LineNumber> {
        let after = self
            .order
            .partition_point(|at| self.address(*at) <= address);
        let row = self.row(*self.order.get(after.checked_sub(1)?)?);
        row.location.map(|location| location.line)
    }

    /// The rows in `range`, by address.
    pub fn within(
        &self,
        range: AddressRange<ImageAddress>,
    ) -> impl DoubleEndedIterator<Item = StatementRow> + '_ {
        let start = self
            .order
            .partition_point(|at| self.address(*at) < range.start);
        let end = self
            .order
            .partition_point(|at| self.address(*at) < range.end);
        self.order[start..end.max(start)]
            .iter()
            .map(|at| self.row(*at))
    }
}

/// Whether a row has a location: a file and a line.
pub(super) const fn is_located(row: &LineRow) -> bool {
    row.file.get() != NONE && row.line.get() != 0
}

/// Whether a row is a public statement row; see [`Row::is_public`].
pub(super) const fn is_public(row: &LineRow) -> bool {
    row.flags & row_flags::END_SEQUENCE == 0
        && (is_located(row)
            || row.flags & (row_flags::PROLOGUE_END | row_flags::EPILOGUE_BEGIN) != 0)
}

/// The line of row `index` of the given tables, as [`decode_row`] finds
/// it, without reading the rest of the row.
pub(super) fn row_line(rows: &[LineRow], extras: &[LineExtra], index: usize) -> u64 {
    let row = &rows[index];
    if row.line.get() != WIDE_LINE || row.flags & row_flags::EXTRA == 0 {
        return u64::from(row.line.get());
    }

    extras
        .binary_search_by_key(&index, |extra| extra.row.get() as usize)
        .map_or_else(|_| u64::from(WIDE_LINE), |found| extras[found].line.get())
}

/// Row `index` of the given tables.
pub(super) fn decode_row(
    addresses: &[RowAddress],
    rows: &[LineRow],
    extras: &[LineExtra],
    index: usize,
) -> Row {
    let row = &rows[index];
    let flags = row.flags;
    let extra = (flags & row_flags::EXTRA != 0)
        .then(|| {
            let found = extras.binary_search_by_key(&index, |extra| extra.row.get() as usize);

            // Validation pairs every flagged row with an extra.
            debug_assert!(found.is_ok(), "row {index} is flagged, but has no extra");
            found.ok().map(|found| &extras[found])
        })
        .flatten();
    let line = match (row.line.get(), extra) {
        (WIDE_LINE, Some(extra)) => extra.line.get(),
        (line, _) => u64::from(line),
    };

    let column = match (row.column.get(), extra) {
        (WIDE_COLUMN, Some(extra)) => extra.column.get(),
        (column, _) => u64::from(column),
    };

    Row {
        address: addresses[index].address.get(),
        file: (row.file.get() != NONE).then(|| SourceFileId::new(row.file.get())),
        line,
        column,
        operation_index: extra.map_or(0, |extra| extra.operation_index.get()),
        discriminator: extra.map_or(0, |extra| extra.discriminator.get()),
        isa: extra.map_or(0, |extra| extra.isa.get()),
        statement: flags & row_flags::STATEMENT != 0,
        prologue_end: flags & row_flags::PROLOGUE_END != 0,
        epilogue_begin: flags & row_flags::EPILOGUE_BEGIN != 0,
        end_sequence: flags & row_flags::END_SEQUENCE != 0,
    }
}

/// The public statement row a row is, with its sequence and its ordinal
/// within it.
pub(super) fn statement_row(row: &Row, sequence: u32, ordinal: u32) -> StatementRow {
    StatementRow {
        address: ImageAddress::new(row.address),
        operation_index: row.operation_index,
        location: row.location(),
        discriminator: row.discriminator,
        flags: StatementFlags::empty()
            .with_statement(row.statement)
            .with_prologue_end(row.prologue_end)
            .with_epilogue_begin(row.epilogue_begin),
        isa: row.isa,
        sequence: LineSequenceId::new(sequence),
        ordinal,
    }
}

/// Every public statement row of the given tables, in order.
pub(super) fn statement_rows<'a>(
    addresses: &'a [RowAddress],
    rows: &'a [LineRow],
    extras: &'a [LineExtra],
    sequences: &'a [LineSequence],
) -> impl Iterator<Item = StatementRow> + 'a {
    sequences
        .iter()
        .zip(0u32..)
        .flat_map(move |(run, sequence)| {
            let first = run.first.get();
            (first..first + run.rows.get()).filter_map(move |index| {
                let row = decode_row(addresses, rows, extras, index as usize);
                row.is_public()
                    .then(|| statement_row(&row, sequence, index - first))
            })
        })
}

/// A line range, decoded.
pub(super) fn line_entry(
    addresses: &[RowAddress],
    rows: &[LineRow],
    extras: &[LineExtra],
    range: &LineRange,
) -> Option<crate::model::LineEntry> {
    let location = decode_row(addresses, rows, extras, range.row.get() as usize).location();
    debug_assert!(
        location.is_some(),
        "validation checked line range row {} has a location",
        range.row.get()
    );
    Some(crate::model::LineEntry {
        range: AddressRange {
            start: ImageAddress::new(range.start.get()),
            end: ImageAddress::new(range.end.get()),
        },
        location: location?,
        statement: range.statement != 0,
    })
}

/// The line ranges in the order they are persisted, and the indexes
/// persisted beside the line tables.
pub(super) struct LineIndexes {
    pub ranges: Vec<LineRange>,
    pub statements: Vec<StatementKey>,
    pub boundaries: Vec<ControlBoundary>,
}

/// Orders `tables`' ranges and builds their indexes.
pub(super) fn build_indexes(tables: &LineTables) -> LineIndexes {
    let mut ranges = tables.ranges.clone();
    ranges.par_sort_unstable_by_key(|range| (range.start.get(), range.end.get(), range.row.get()));
    let mut prefix_max_end = 0;

    for range in &mut ranges {
        prefix_max_end = prefix_max_end.max(range.end.get());
        range.prefix_max_end = prefix_max_end.into();
    }

    let mut statements = Vec::new();
    let mut boundaries = Vec::new();

    for (at, _) in (0u32..).zip(&tables.rows) {
        let row = tables.row(at as usize);
        if !row.is_public() {
            continue;
        }

        if row.statement
            && let Some(location) = row.location()
        {
            statements.push(StatementKey {
                file: location.file.get().into(),
                line: location.line.get().into(),
                row: at.into(),
            });
        }

        if row.prologue_end || row.epilogue_begin {
            boundaries.push(ControlBoundary { row: at.into() });
        }
    }

    statements.par_sort_unstable_by_key(|key| (key.file.get(), key.line.get(), key.row.get()));
    boundaries.par_sort_unstable_by_key(|boundary| {
        (
            tables.addresses[boundary.row.get() as usize].address.get(),
            boundary.row.get(),
        )
    });
    LineIndexes {
        ranges,
        statements,
        boundaries,
    }
}

/// Source paths being interned, in the order first seen.
#[derive(Debug, Default, Clone)]
pub struct Files {
    paths: Vec<PathBuf>,
    ids: foldhash::HashMap<PathBuf, SourceFileId>,
}

impl Files {
    /// The file `path` names, added if it is new.
    pub fn intern(&mut self, path: PathBuf) -> SourceFileId {
        if let Some(&id) = self.ids.get(&path) {
            return id;
        }

        // Past `u32::MAX` files, ids repeat, but the image cannot seal: the
        // paths, each distinct and NUL-terminated, then outgrow the pool's
        // `u32` offsets.
        let id = SourceFileId::new(u32::try_from(self.paths.len()).unwrap_or(u32::MAX));
        self.paths.push(path.clone());
        self.ids.insert(path, id);
        debug_assert_eq!(
            self.ids.len(),
            self.paths.len(),
            "every interned path has one file id"
        );
        id
    }

    /// Forgets every file but the first `len`.
    pub fn truncate(&mut self, len: usize) {
        for path in self.paths.drain(len.min(self.paths.len())..) {
            self.ids.remove(&path);
        }

        debug_assert_eq!(
            self.ids.len(),
            self.paths.len(),
            "every kept path has one file id"
        );
    }

    /// The file a relative path names when exactly one absolute path ends
    /// with it, as a type unit names the file of its declarations;
    /// otherwise the path itself.
    pub fn intern_suffix(&mut self, path: PathBuf) -> SourceFileId {
        if path.is_relative() {
            let mut matches = self
                .paths
                .iter()
                .zip(0u32..)
                .filter(|(candidate, _)| candidate.is_absolute() && candidate.ends_with(&path));
            if let Some((_, index)) = matches.next()
                && matches.next().is_none()
            {
                return SourceFileId::new(index);
            }
        }

        self.intern(path)
    }

    pub fn paths(&self) -> &[PathBuf] {
        &self.paths
    }
}

/// Line tables for statement rows alone, as a synthetic image describes
/// them: each sequence's rows in order, at their ordinals, with rows
/// describing nothing before them, and no ranges.
#[cfg(test)]
pub fn from_statement_rows(statements: &[StatementRow]) -> LineTables {
    let mut tables = LineTables::default();
    let mut current = None;
    let mut next = 0;

    for statement in statements {
        if current != Some(statement.sequence) {
            tables.begin_sequence().expect("few rows");
            current = Some(statement.sequence);
            next = 0;
        }

        while next < statement.ordinal {
            tables
                .push_row(&Row {
                    address: statement.address.get(),
                    ..Row::default()
                })
                .expect("few rows");
            next += 1;
        }

        let location = statement.location.as_ref();
        tables
            .push_row(&Row {
                address: statement.address.get(),
                file: location.map(|location| location.file),
                line: location.map_or(0, |location| location.line.get()),
                column: location
                    .and_then(|location| location.column)
                    .map_or(0, ColumnNumber::get),
                operation_index: statement.operation_index,
                discriminator: statement.discriminator,
                isa: statement.isa,
                statement: statement.flags.is_statement(),
                prologue_end: statement.flags.prologue_end(),
                epilogue_begin: statement.flags.epilogue_begin(),
                end_sequence: false,
            })
            .expect("few rows");
        next += 1;
    }

    tables
}

impl LineTables {
    /// Adds the tables and their indexes to `builder`.
    pub fn add_to<'a>(&'a self, builder: &mut super::Builder<'a>) {
        let indexes = build_indexes(self);
        builder
            .table(&self.addresses)
            .table(&self.rows)
            .table(&self.extras)
            .table(&self.sequences)
            .owned_table(indexes.ranges)
            .owned_table(indexes.statements)
            .owned_table(indexes.boundaries);
    }
}

impl Files {
    /// Adds the files to `builder` and their paths to `paths`, or `None`
    /// when a path contains a NUL or the paths do not fit.
    pub fn add_to(
        &self,
        builder: &mut super::Builder<'_>,
        paths: &mut super::PathsBuilder,
    ) -> Option<()> {
        let records = self
            .paths
            .iter()
            .map(|path| {
                Some(FileRecord {
                    path: paths.intern(path)?.0.into(),
                })
            })
            .collect::<Option<Vec<_>>>()?;

        builder.owned_table(records);
        Some(())
    }
}

/// Lookups in a validated image's line tables. Every index it reads was
/// checked against the rows by validation, so no lookup can miss.
#[derive(Clone, Copy)]
pub struct LineView<'a> {
    addresses: &'a [RowAddress],
    rows: &'a [LineRow],
    extras: &'a [LineExtra],
    sequences: &'a [LineSequence],
    ranges: &'a [LineRange],
    statements: &'a [StatementKey],
    boundaries: &'a [ControlBoundary],
}

impl<'a> LineView<'a> {
    pub fn new(image: &'a super::Image) -> Self {
        Self {
            addresses: image.table(),
            rows: image.table(),
            extras: image.table(),
            sequences: image.table(),
            ranges: image.table(),
            statements: image.table(),
            boundaries: image.table(),
        }
    }

    /// Row `index`, decoded.
    pub fn row(self, index: usize) -> Row {
        decode_row(self.addresses, self.rows, self.extras, index)
    }

    /// Row `index` as a public statement row.
    fn statement_row(self, at: u32) -> StatementRow {
        let sequence = self
            .sequences
            .partition_point(|sequence| sequence.first.get() <= at)
            - 1;
        let ordinal = at - self.sequences[sequence].first.get();
        let sequence = u32::try_from(sequence);
        debug_assert!(
            sequence.is_ok(),
            "validation bounds the sequences below u32"
        );

        statement_row(&self.row(at as usize), sequence.unwrap_or(NONE), ordinal)
    }

    /// Every public statement row, in order.
    pub fn statement_rows(self) -> impl Iterator<Item = StatementRow> + 'a {
        statement_rows(self.addresses, self.rows, self.extras, self.sequences)
    }

    /// The line ranges at `indexes`, in line-program order.
    fn entries_in_order(
        self,
        mut indexes: Vec<usize>,
    ) -> impl Iterator<Item = crate::model::LineEntry> + 'a {
        indexes.sort_unstable_by_key(|index| self.ranges[*index].row.get());
        indexes.into_iter().filter_map(move |index| {
            line_entry(self.addresses, self.rows, self.extras, &self.ranges[index])
        })
    }

    /// Every line range, in line-program order.
    #[cfg(feature = "tools")]
    pub fn line_entries(self) -> impl Iterator<Item = crate::model::LineEntry> + 'a {
        self.entries_in_order((0..self.ranges.len()).collect())
    }

    /// The first line range, in line-program order, that contains
    /// `address`.
    pub fn line_entry_containing(self, address: ImageAddress) -> Option<crate::model::LineEntry> {
        let address = address.get();
        let mut index = self
            .ranges
            .partition_point(|range| range.start.get() <= address);
        let mut first = None::<&LineRange>;
        while index > 0 {
            index -= 1;
            let range = &self.ranges[index];
            if range.prefix_max_end.get() <= address {
                break;
            }

            if address < range.end.get()
                && first.is_none_or(|first| range.row.get() < first.row.get())
            {
                first = Some(range);
            }
        }

        first.and_then(|range| line_entry(self.addresses, self.rows, self.extras, range))
    }

    /// The line ranges that start within any of `within`, in line-program
    /// order.
    pub fn line_entries_starting_in(
        self,
        within: impl IntoIterator<Item = AddressRange<ImageAddress>>,
    ) -> impl Iterator<Item = crate::model::LineEntry> + 'a {
        let mut indexes = Vec::new();
        for range in within {
            let first = self
                .ranges
                .partition_point(|entry| entry.start.get() < range.start.get());
            let end = self
                .ranges
                .partition_point(|entry| entry.start.get() < range.end.get());
            indexes.extend(first..end.max(first));
        }

        indexes.sort_unstable();
        indexes.dedup();
        self.entries_in_order(indexes)
    }

    /// The rows at `address` that mark a prologue's end or an epilogue's
    /// start, in row order.
    pub fn control_boundaries_at(
        self,
        address: ImageAddress,
    ) -> impl Iterator<Item = StatementRow> + 'a {
        let address = address.get();
        let at =
            |boundary: &ControlBoundary| self.addresses[boundary.row.get() as usize].address.get();
        let first = self
            .boundaries
            .partition_point(|boundary| at(boundary) < address);
        self.boundaries[first..]
            .iter()
            .take_while(move |boundary| at(boundary) == address)
            .map(move |boundary| self.statement_row(boundary.row.get()))
    }

    /// The statement keys of `file` with lines in `lines`, in (line, row)
    /// order.
    pub fn statements(
        self,
        file: SourceFileId,
        lines: std::ops::RangeInclusive<u64>,
    ) -> &'a [StatementKey] {
        let key = |line| (file.get(), line);
        let order = |entry: &StatementKey| (entry.file.get(), entry.line.get());
        let start = self
            .statements
            .partition_point(|entry| order(entry) < key(*lines.start()));
        let end = self
            .statements
            .partition_point(|entry| order(entry) <= key(*lines.end()));
        &self.statements[start..end.max(start)]
    }

    /// The address of row `index`.
    pub const fn address(self, index: u32) -> ImageAddress {
        ImageAddress::new(self.addresses[index as usize].address.get())
    }
}
