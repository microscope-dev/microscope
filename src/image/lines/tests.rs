use std::path::PathBuf;

use proptest::prelude::*;

use super::{Files, LineTables, LineView, Row};
use crate::image::sample::{TARGET, range};
use crate::image::{Builder, Image, Limits, PathsBuilder, TableKind};
use crate::test_strategies::u64_edges;
use crate::{ImageAddress, LineSequenceId, SourceFileId, StatementFlags, StatementRow};

/// The files every generated row may name.
const FILES: u32 = 3;

/// Line and column numbers at the edges of their fields.
fn number() -> impl Strategy<Value = u64> {
    let wide = u64::from(u32::MAX);
    let narrow = u64::from(u16::MAX);
    prop_oneof![
        2 => prop::sample::select(vec![0, 1, narrow - 1, narrow, narrow + 1, wide - 1, wide, wide + 1]),
        2 => 0_u64..64,
        1 => u64_edges(),
    ]
}

/// A row with any address and fields; the builder decides its sequence.
fn row() -> impl Strategy<Value = Row> {
    (
        (
            proptest::option::of((0..FILES).prop_map(SourceFileId::new)),
            number(),
            number(),
        ),
        prop_oneof![3 => Just((0, 0, 0)), 1 => (u64_edges(), u64_edges(), u64_edges())],
        any::<[bool; 3]>(),
    )
        .prop_map(
            |((file, line, column), (operation_index, discriminator, isa), flags)| Row {
                address: 0,
                file,
                line,
                column,
                operation_index,
                discriminator,
                isa,
                statement: flags[0],
                prologue_end: flags[1],
                epilogue_begin: flags[2],
                end_sequence: false,
            },
        )
}

/// Sequences of rows in address order, each ending with a row that ends
/// it, at the bottom of the address space or near its top.
fn sequences() -> impl Strategy<Value = Vec<Vec<Row>>> {
    let sequence = (
        prop::sample::select(vec![0x1000_u64, 0x1010, 0x1040, u64::MAX - 0x100]),
        proptest::collection::vec(
            (row(), prop::sample::select(vec![0_u64, 0, 1, 2, 4, 16])),
            1..6,
        ),
        1_u64..8,
    )
        .prop_map(|(start, rows, last)| {
            let mut address = start;
            let mut sequence = Vec::new();
            for (row, step) in rows {
                sequence.push(Row { address, ..row });
                address += step;
            }
            sequence.push(Row {
                address: address + last,
                end_sequence: true,
                ..Row::default()
            });
            sequence
        });
    proptest::collection::vec(sequence, 0..4)
}

/// A line range as the model keeps it: start, end, row, and statement.
type ModelRange = (u64, u64, u32, bool);

/// Pushes `sequences` as the loader does, opening a range at each located
/// row up to the next row's address, and returns the rows and ranges it
/// pushed.
fn build(tables: &mut LineTables, sequences: &[Vec<Row>]) -> (Vec<Row>, Vec<ModelRange>) {
    let mut rows = Vec::new();
    let mut ranges = Vec::new();
    for sequence in sequences {
        tables.begin_sequence().unwrap();
        for (at, row) in sequence.iter().enumerate() {
            let index = tables.push_row(row).unwrap();
            assert_eq!(index as usize, tables.rows.len() - 1);
            rows.push(*row);
            if let Some(next) = sequence.get(at + 1)
                && row.location().is_some()
                && row.address < next.address
            {
                tables
                    .push_range(range(row.address, next.address), index, row.statement)
                    .unwrap();
                ranges.push((row.address, next.address, index, row.statement));
            }
        }
    }
    (rows, ranges)
}

fn files() -> Files {
    let mut files = Files::default();
    for file in 0..FILES {
        files.intern(PathBuf::from(format!("/src/{file}.c")));
    }
    files
}

fn seal(tables: &LineTables) -> Image {
    let mut builder = Builder::new(TARGET);
    tables.add_to(&mut builder);
    let mut paths = PathsBuilder::default();
    files().add_to(&mut builder, &mut paths).unwrap();
    builder.bytes(TableKind::Paths, paths.into_bytes());
    builder.seal(Limits::default()).unwrap()
}

/// Each row's sequence and its ordinal in it.
fn places(sequences: &[Vec<Row>]) -> Vec<(u32, u32)> {
    sequences
        .iter()
        .zip(0..)
        .flat_map(|(sequence, number)| (0..).take(sequence.len()).map(move |at| (number, at)))
        .collect()
}

fn statement_row(row: &Row, (sequence, ordinal): (u32, u32)) -> StatementRow {
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

/// Addresses at and around every row.
fn probes(rows: &[Row]) -> Vec<u64> {
    let mut probes = rows
        .iter()
        .flat_map(|row| {
            [
                row.address.saturating_sub(1),
                row.address,
                row.address.saturating_add(1),
            ]
        })
        .chain([0, 0x1000, 0x1030, u64::MAX])
        .collect::<Vec<_>>();
    probes.sort_unstable();
    probes.dedup();
    probes
}

proptest! {
    /// A row reads back as it was pushed, whatever its fields hold.
    #[test]
    fn a_row_reads_back_as_it_was_pushed(rows in proptest::collection::vec(row(), 1..8)) {
        let mut tables = LineTables::default();
        tables.begin_sequence().unwrap();
        for (row, address) in rows.iter().zip(0..) {
            let row = Row { address, ..*row };
            let index = tables.push_row(&row).unwrap();
            prop_assert_eq!(tables.row(index as usize), row);
        }
    }

    /// Whatever the builder makes seals, and the sealed tables' lookups
    /// agree with scans of the rows and ranges that made them.
    #[test]
    fn built_tables_seal_and_their_lookups_agree_with_scans(sequences in sequences()) {
        let mut tables = LineTables::default();
        let (rows, ranges) = build(&mut tables, &sequences);
        let image = seal(&tables);
        let view = LineView::new(&image);
        let places = places(&sequences);
        let public = |at: usize| rows[at].is_public();

        for (at, row) in rows.iter().enumerate() {
            prop_assert_eq!(view.row(at), *row);
        }

        for address in probes(&rows) {
            let first = ranges
                .iter()
                .filter(|(start, end, _, _)| (*start..*end).contains(&address))
                .min_by_key(|(_, _, row, _)| *row);
            let expected = first.map(|(start, end, row, statement)| crate::model::LineEntry {
                range: range(*start, *end),
                location: rows[*row as usize].location().unwrap(),
                statement: *statement,
            });
            prop_assert_eq!(
                view.line_entry_containing(ImageAddress::new(address)),
                expected,
                "at {:#x}",
                address
            );

            let boundaries = (0..rows.len())
                .filter(|at| {
                    public(*at)
                        && rows[*at].address == address
                        && (rows[*at].prologue_end || rows[*at].epilogue_begin)
                })
                .map(|at| statement_row(&rows[at], places[at]))
                .collect::<Vec<_>>();
            prop_assert_eq!(
                view.control_boundaries_at(ImageAddress::new(address)).collect::<Vec<_>>(),
                boundaries
            );
        }

        for file in (0..FILES).map(SourceFileId::new) {
            for lines in [0..=u64::MAX, 1..=1, 2..=40, u64::from(u32::MAX)..=u64::MAX] {
                let mut expected = (0..rows.len())
                    .filter(|at| public(*at) && rows[*at].statement)
                    .filter_map(|at| Some((rows[at].location()?, at)))
                    .filter(|(location, _)| {
                        location.file == file && lines.contains(&location.line.get())
                    })
                    .map(|(location, at)| (location.line.get(), u32::try_from(at).unwrap()))
                    .collect::<Vec<_>>();
                expected.sort_unstable();
                let found = view
                    .statements(file, lines.clone())
                    .iter()
                    .map(|key| (key.line.get(), key.row.get()))
                    .collect::<Vec<_>>();
                prop_assert_eq!(found, expected, "{:?} lines {:?}", file, lines);
            }
        }
    }

    /// Before sealing, `within` lists the public rows of a range by
    /// address, keeping row order at one address, and `line_at` names the
    /// line of the last of them at or before an address whose sequence
    /// covers it, or none.
    #[test]
    fn rows_by_address_agree_with_scans(sequences in sequences()) {
        let mut tables = LineTables::default();
        let (rows, _) = build(&mut tables, &sequences);
        let by_address = tables.statements_by_address();
        let places = places(&sequences);
        let probes = probes(&rows);

        // Each sequence covers from its first row to the row ending it.
        let covers = |at: usize, address: u64| {
            let sequence = &sequences[places[at].0 as usize];
            sequence[0].address <= address
                && sequence.last().is_none_or(|end| address < end.address)
        };

        for (start, end) in probes
            .iter()
            .step_by(3)
            .flat_map(|start| probes.iter().step_by(4).map(move |end| (*start, *end)))
        {
            let mut expected = (0..rows.len())
                .filter(|at| rows[*at].is_public() && (start..end).contains(&rows[*at].address))
                .collect::<Vec<_>>();
            expected.sort_by_key(|at| rows[*at].address);
            let expected = expected
                .into_iter()
                .map(|at| statement_row(&rows[at], places[at]))
                .collect::<Vec<_>>();
            prop_assert_eq!(
                by_address.within(range(start, end)).collect::<Vec<_>>(),
                expected,
                "within {:#x}..{:#x}",
                start,
                end
            );
        }

        for address in probes {
            let mut candidates = (0..rows.len())
                .filter(|at| rows[*at].is_public() && rows[*at].address <= address)
                .collect::<Vec<_>>();
            candidates.sort_by_key(|at| rows[*at].address);
            let line = candidates
                .into_iter()
                .rev()
                .find(|at| covers(*at, address))
                .and_then(|at| rows[at].location())
                .map(|location| location.line);
            prop_assert_eq!(by_address.line_at(ImageAddress::new(address)), line, "at {:#x}", address);
        }
    }

    /// Appending tables is building them after the first.
    #[test]
    fn appended_tables_are_those_built_in_turn(first in sequences(), second in sequences()) {
        let renumber = |file: SourceFileId| SourceFileId::new(file.get() + FILES);
        let mut separate = LineTables::default();
        build(&mut separate, &first);
        let mut other = LineTables::default();
        build(&mut other, &second);
        separate.append(&other, renumber).unwrap();

        let moved = second
            .iter()
            .map(|sequence| {
                sequence
                    .iter()
                    .map(|row| Row { file: row.file.map(renumber), ..*row })
                    .collect()
            })
            .collect::<Vec<_>>();
        let mut together = LineTables::default();
        build(&mut together, &[first, moved].concat());

        prop_assert_eq!(separate.addresses, together.addresses);
        prop_assert_eq!(separate.rows, together.rows);
        prop_assert_eq!(separate.extras, together.extras);
        prop_assert_eq!(separate.sequences, together.sequences);
        prop_assert_eq!(separate.ranges, together.ranges);
    }

    /// Clipping ends each range at the first function that starts inside
    /// it, and the tables still seal.
    #[test]
    fn clipped_ranges_hold_no_other_function_start(
        sequences in sequences(),
        starts in proptest::collection::btree_set(
            prop_oneof![0x1000_u64..0x1080, (u64::MAX - 0x100)..u64::MAX],
            0..6,
        ),
    ) {
        let mut tables = LineTables::default();
        let (_, ranges) = build(&mut tables, &sequences);
        let starts = starts.into_iter().map(ImageAddress::new).collect::<Vec<_>>();
        tables.clip_at(&starts);

        for (clipped, (start, end, row, _)) in tables.ranges.iter().zip(&ranges) {
            let inside = starts
                .iter()
                .map(|start| start.get())
                .find(|function| start < function && function < end);
            prop_assert_eq!(clipped.start.get(), *start);
            prop_assert_eq!(clipped.end.get(), inside.unwrap_or(*end));
            prop_assert_eq!(clipped.row.get(), *row);
            let holds = |function: &ImageAddress| {
                clipped.start.get() < function.get() && function.get() < clipped.end.get()
            };
            prop_assert!(!starts.iter().any(holds), "{:?}", clipped);
        }

        seal(&tables);
    }
}

/// An address in a gap between sequences, or past the last, is code no
/// sequence describes: it has no line, not the line of the code before.
#[test]
fn an_address_between_sequences_has_no_line() {
    let row = |address, line| Row {
        address,
        file: Some(SourceFileId::new(0)),
        line,
        statement: true,
        ..Row::default()
    };
    let end = |address| Row {
        address,
        end_sequence: true,
        ..Row::default()
    };
    let mut tables = LineTables::default();
    build(
        &mut tables,
        &[
            vec![row(0x1000, 3), end(0x1010)],
            vec![row(0x1020, 7), end(0x1030)],
        ],
    );
    let rows = tables.statements_by_address();
    let line = |address| {
        rows.line_at(ImageAddress::new(address))
            .map(crate::LineNumber::get)
    };

    assert_eq!(line(0x100f), Some(3));
    assert_eq!(line(0x1010), None);
    assert_eq!(line(0x101f), None);
    assert_eq!(line(0x1020), Some(7));
    assert_eq!(line(0x1030), None);

    // Shrunk by the property above: one row, and the end one byte on.
    let mut tables = LineTables::default();
    build(&mut tables, &[vec![row(0x1000, 1), end(0x1001)]]);
    let rows = tables.statements_by_address();
    assert_eq!(rows.line_at(ImageAddress::new(0x1001)), None);
}

/// Ranges built for two sequences that overlap, as folded code's do: an
/// address the later one does not cover takes the earlier one's line.
#[test]
fn an_address_takes_the_line_of_the_sequence_covering_it() {
    let row = |address, line| Row {
        address,
        file: Some(SourceFileId::new(0)),
        line,
        statement: true,
        ..Row::default()
    };
    let end = |address| Row {
        address,
        end_sequence: true,
        ..Row::default()
    };
    let mut tables = LineTables::default();
    build(
        &mut tables,
        &[
            vec![row(0x1000, 3), end(0x1040)],
            vec![row(0x1010, 7), end(0x1020)],
        ],
    );
    let rows = tables.statements_by_address();
    let line = |address| {
        rows.line_at(ImageAddress::new(address))
            .map(crate::LineNumber::get)
    };

    assert_eq!(line(0x1018), Some(7));
    assert_eq!(line(0x1030), Some(3));
}
