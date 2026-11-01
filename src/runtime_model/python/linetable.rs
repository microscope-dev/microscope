//! A code object's location table, `co_linetable`, as CPython 3.11 and
//! later encode it (`InternalDocs/code_objects.md`): one entry for each
//! run of code units, each giving the run's line as a change from the line
//! before it, which begins at the code object's first line.
//!
//! An entry's first byte has its high bit set, its kind in the next four
//! bits, and the run's length less one, in code units, in the low three.
//! The kind says what follows:
//!
//! - 0 to 9, the short form: the same line, and a byte of columns;
//! - 10 to 12, the one-line form: the line plus 0, 1, or 2, and a
//!   byte each for the start and end columns;
//! - 13: a line change as a signed varint, and no columns;
//! - 14, the long form: a line change as a signed varint, then the end
//!   line's change, the start column, and the end column, as varints;
//! - 15: no location.
//!
//! A varint is 6-bit groups, least significant first, each but the last
//! with bit 6 set. A signed one keeps its sign in its lowest bit.

/// The bytes of one code unit.
pub(super) const CODE_UNIT: u64 = 2;

/// Why a location table cannot be read.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(super) struct Malformed;

/// A run of code units and its line.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(super) struct Run {
    /// The run's bytes in the bytecode.
    pub(super) start: u64,
    pub(super) end: u64,

    /// Its line; `None` for code at no line, as a function's preamble may be.
    pub(super) line: Option<i64>,
}

/// Each run of a table, in order, beginning at line `first_line`.
pub(super) const fn runs(table: &[u8], first_line: i64) -> Runs<'_> {
    Runs {
        table,
        at: 0,
        offset: 0,
        line: first_line,
    }
}

pub(super) struct Runs<'a> {
    table: &'a [u8],
    at: usize,
    offset: u64,
    line: i64,
}

impl Runs<'_> {
    fn byte(&mut self) -> Result<u8, Malformed> {
        let byte = *self.table.get(self.at).ok_or(Malformed)?;
        self.at += 1;
        Ok(byte)
    }

    fn varint(&mut self) -> Result<u64, Malformed> {
        let mut byte = self.byte()?;
        let mut value = u64::from(byte & 63);
        let mut shift = 0;
        while byte & 64 != 0 {
            byte = self.byte()?;
            shift += 6;
            // A group whose bits reach past the 64th names no value.
            let bits = u64::from(byte & 63);
            if shift > 60 || (bits << shift) >> shift != bits {
                return Err(Malformed);
            }

            value |= bits << shift;
        }

        Ok(value)
    }

    fn signed(&mut self) -> Result<i64, Malformed> {
        let value = self.varint()?;
        let magnitude = i64::try_from(value >> 1).map_err(|_| Malformed)?;
        Ok(if value & 1 == 0 {
            magnitude
        } else {
            -magnitude
        })
    }

    fn next_run(&mut self) -> Result<Run, Malformed> {
        let first = self.byte()?;
        if first & 0x80 == 0 {
            return Err(Malformed);
        }

        let kind = (first >> 3) & 15;
        let length = u64::from(first & 7) + 1;
        let change = match kind {
            0..=9 => {
                self.byte()?;
                Some(0)
            }
            10..=12 => {
                self.byte()?;
                self.byte()?;
                Some(i64::from(kind - 10))
            }
            13 => Some(self.signed()?),
            14 => {
                let change = self.signed()?;
                self.varint()?;
                self.varint()?;
                self.varint()?;
                Some(change)
            }
            _ => None,
        };

        let line = match change {
            Some(change) => {
                self.line = self.line.checked_add(change).ok_or(Malformed)?;
                Some(self.line)
            }
            None => None,
        };

        let start = self.offset;
        self.offset = start.checked_add(length * CODE_UNIT).ok_or(Malformed)?;
        Ok(Run {
            start,
            end: self.offset,
            line,
        })
    }
}

impl Iterator for Runs<'_> {
    type Item = Result<Run, Malformed>;

    fn next(&mut self) -> Option<Self::Item> {
        if self.at >= self.table.len() {
            return None;
        }

        let run = self.next_run();
        if run.is_err() {
            // Nothing after a malformed entry can be trusted.
            self.at = self.table.len();
        }

        Some(run)
    }
}

/// The line of the code unit at byte `offset` of the bytecode, which
/// begins at `first_line`: `Ok(None)` for a unit at no line, and an error
/// for one no run covers or a table that cannot be read.
pub(super) fn line_at(
    table: &[u8],
    first_line: i64,
    offset: u64,
) -> Result<Option<i64>, Malformed> {
    for run in runs(table, first_line) {
        let run = run?;
        if run.start <= offset && offset < run.end {
            return Ok(run.line);
        }
    }

    Err(Malformed)
}

#[cfg(test)]
mod tests {
    use proptest::prelude::*;

    use super::*;

    /// One entry as a model: its kind, its length in code units, its line
    /// change, which only kinds 13 and 14 write, and its columns.
    #[derive(Debug, Clone, Copy)]
    struct Entry {
        kind: u8,
        units: u8,
        change: i64,
        extra: [u32; 3],
    }

    fn write_varint(mut value: u64, out: &mut Vec<u8>) {
        while value >= 64 {
            out.push(64 | u8::try_from(value & 63).unwrap());
            value >>= 6;
        }
        out.push(u8::try_from(value).unwrap());
    }

    fn write_signed(value: i64, out: &mut Vec<u8>) {
        let magnitude = value.unsigned_abs() << 1;
        write_varint(magnitude | u64::from(value < 0), out);
    }

    /// The table `entries` write, and the runs CPython reads from it.
    fn written(entries: &[Entry], first_line: i64) -> (Vec<u8>, Vec<Run>) {
        let (mut table, mut runs) = (Vec::new(), Vec::new());
        let (mut line, mut start) = (first_line, 0);
        for entry in entries {
            table.push(0x80 | (entry.kind << 3) | (entry.units - 1));
            let change = match entry.kind {
                0..=9 => {
                    table.push(entry.extra[0].to_le_bytes()[0]);
                    Some(0)
                }
                10..=12 => {
                    table.extend([
                        entry.extra[0].to_le_bytes()[0],
                        entry.extra[1].to_le_bytes()[0],
                    ]);
                    Some(i64::from(entry.kind - 10))
                }
                13 => {
                    write_signed(entry.change, &mut table);
                    Some(entry.change)
                }
                14 => {
                    write_signed(entry.change, &mut table);
                    for extra in entry.extra {
                        write_varint(extra.into(), &mut table);
                    }
                    Some(entry.change)
                }
                _ => None,
            };

            let end = start + u64::from(entry.units) * CODE_UNIT;
            runs.push(Run {
                start,
                end,
                line: change.map(|change| {
                    line += change;
                    line
                }),
            });
            start = end;
        }

        (table, runs)
    }

    proptest! {
        /// A table of entries of every kind reads back as their runs, and
        /// the line of each byte is the line of the run that holds it.
        #[test]
        fn written_runs_read_back_and_place_every_unit(
            entries in proptest::collection::vec(
                (0..16_u8, 1..=8_u8, -(1_i64 << 20)..1 << 20, any::<[u32; 3]>()).prop_map(
                    |(kind, units, change, extra)| Entry { kind, units, change, extra },
                ),
                0..24,
            ),
            first_line in -(1_i64 << 31)..1 << 31,
        ) {
            let (table, expected) = written(&entries, first_line);
            let read: Result<Vec<Run>, Malformed> = runs(&table, first_line).collect();
            prop_assert_eq!(read, Ok(expected.clone()));

            let end = expected.last().map_or(0, |run| run.end);
            for offset in 0..end + 4 {
                let scanned = expected
                    .iter()
                    .find(|run| run.start <= offset && offset < run.end)
                    .map(|run| run.line)
                    .ok_or(Malformed);
                prop_assert_eq!(line_at(&table, first_line, offset), scanned, "{}", offset);
            }
        }
    }

    /// A table of each form, from line 10: a short entry, a one-line entry
    /// two lines on, a long entry four lines back, an entry with no columns
    /// three lines on, and one with no location.
    const TABLE: &[u8] = &[
        0x80 | (3 << 3) | 1,
        0x12,
        0x80 | (12 << 3),
        4,
        9,
        0x80 | (14 << 3) | 2,
        9, // -4
        0,
        5,
        7,
        0x80 | (13 << 3),
        6, // +3
        0x80 | (15 << 3) | 7,
    ];

    #[test]
    fn each_form_gives_its_runs_line() {
        let runs: Vec<Run> = runs(TABLE, 10).collect::<Result<_, _>>().expect("a table");
        let lines: Vec<(u64, u64, Option<i64>)> = runs
            .iter()
            .map(|run| (run.start, run.end, run.line))
            .collect();
        assert_eq!(
            lines,
            [
                (0, 4, Some(10)),
                (4, 6, Some(12)),
                (6, 12, Some(8)),
                (12, 14, Some(11)),
                (14, 30, None),
            ]
        );
        assert_eq!(line_at(TABLE, 10, 7), Ok(Some(8)));
        assert_eq!(line_at(TABLE, 10, 16), Ok(None));
        assert_eq!(line_at(TABLE, 10, 30), Err(Malformed), "past the code");
    }

    /// Prints, for each code object of some of the standard library, its
    /// table, first line, length in bytes, and `co_lines()`'s runs.
    const ORACLE: &str = r#"
import json, sys
def walk(code):
    yield code
    for constant in code.co_consts:
        if hasattr(constant, "co_lines"):
            yield from walk(constant)
for name in ["json.decoder", "threading", "dataclasses", "asyncio.base_events", "argparse",
             "inspect", "typing", "email.message", "pathlib", "tarfile", "unittest.case"]:
    module = __import__(name, fromlist=["_"])
    with open(module.__file__) as source:
        top = compile(source.read(), module.__file__, "exec")
    for code in walk(top):
        print(json.dumps([code.co_linetable.hex(), code.co_firstlineno,
                          len(code.co_code), list(code.co_lines())]))
"#;

    /// A code object's table in hex, first line, length, and runs.
    type Oracle = (String, i64, u64, Vec<(u64, u64, Option<i64>)>);

    /// Every code unit of thousands of code objects has the line CPython
    /// gives it, on every interpreter in the matrix.
    #[test]
    fn each_interpreter_in_the_matrix_agrees_on_every_line() {
        let root = std::env::var_os("MICROSCOPE_CPYTHON")
            .expect("MICROSCOPE_CPYTHON names the interpreters; run tests in the dev shell");
        let mut names: Vec<_> = std::fs::read_dir(&root)
            .expect("list the interpreters")
            .map(|entry| entry.expect("an interpreter").path())
            .collect();
        names.sort();
        for interpreter in names {
            let output = std::process::Command::new(interpreter.join("python"))
                .args(["-I", "-c", ORACLE])
                .output()
                .expect("run the interpreter");
            assert!(
                output.status.success(),
                "{}",
                String::from_utf8_lossy(&output.stderr)
            );
            let mut checked = 0;
            for line in String::from_utf8_lossy(&output.stdout).lines() {
                let (table, first, length, runs): Oracle =
                    serde_json::from_str(line).expect("the oracle's line");
                let table: Vec<u8> = (0..table.len())
                    .step_by(2)
                    .map(|at| u8::from_str_radix(&table[at..at + 2], 16).expect("hex"))
                    .collect();
                for offset in (0..length).step_by(2) {
                    let expected = runs
                        .iter()
                        .find(|(start, end, _)| *start <= offset && offset < *end)
                        .map(|(_, _, line)| *line);
                    assert_eq!(
                        line_at(&table, first, offset).ok(),
                        expected,
                        "{}: offset {offset} of a code object at line {first}",
                        interpreter.display()
                    );
                }

                checked += 1;
            }

            assert!(
                checked > 1000,
                "{} gave {checked} code objects",
                interpreter.display()
            );
        }
    }

    #[test]
    fn a_table_cut_short_or_out_of_step_is_malformed() {
        assert_eq!(line_at(&TABLE[..4], 10, 5), Err(Malformed));
        assert_eq!(line_at(&[0x05], 10, 0), Err(Malformed));
        let endless = [
            0x80 | (13 << 3),
            0x40,
            0x40,
            0x40,
            0x40,
            0x40,
            0x40,
            0x40,
            0x40,
            0x40,
            0x40,
            0x40,
        ];
        assert_eq!(line_at(&endless, 10, 0), Err(Malformed));

        // Eleven groups, the last of whose bits past the 64th would be lost.
        let mut wide = vec![0x80 | (13 << 3)];
        wide.extend([0x40; 10]);
        wide.push(0x10);
        assert_eq!(line_at(&wide, 10, 0), Err(Malformed));
    }
}
