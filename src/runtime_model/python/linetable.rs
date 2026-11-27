//! Decodes `CPython` 3.11+ `co_linetable` bytecode-offset-to-line mappings (PEP 626).
//!
//! Entries encode 2-byte `CODE_UNIT` runs using 6-bit continuation varints and 4-bit location
//! kinds (0–9 short, 10–12 one-line, 13 no-column, 14 long, 15 no-location).

pub(super) const CODE_UNIT: u64 = 2;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(super) struct Malformed;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(super) struct Run {
    pub(super) start: u64,
    pub(super) end: u64,

    pub(super) line: Option<i64>,
}

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
            self.at = self.table.len();
        }

        Some(run)
    }
}

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

    const TABLE: &[u8] = &[
        0x80 | (3 << 3) | 1,
        0x12,
        0x80 | (12 << 3),
        4,
        9,
        0x80 | (14 << 3) | 2,
        9,
        0,
        5,
        7,
        0x80 | (13 << 3),
        6,
        0x80 | (15 << 3) | 7,
    ];

    const ORACLE: &str = r#"
import json, sys
from importlib.machinery import PathFinder
def walk(code):
    yield code
    for constant in code.co_consts:
        if hasattr(constant, "co_lines"):
            yield from walk(constant)
for name in ["json.decoder", "threading", "dataclasses", "asyncio.base_events", "argparse",
             "inspect", "typing", "email.message", "pathlib", "tarfile", "unittest.case"]:
    path = None
    for part in name.split("."):
        spec = PathFinder.find_spec(part, path)
        path = spec.submodule_search_locations
    with open(spec.origin) as source:
        top = compile(source.read(), spec.origin, "exec")
    for code in walk(top):
        print(json.dumps([code.co_linetable.hex(), code.co_firstlineno,
                          len(code.co_code), list(code.co_lines())]))
"#;

    type Oracle = (String, i64, u64, Vec<(u64, u64, Option<i64>)>);

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
                let decoded = super::runs(&table, first).flat_map(|run| {
                    let run = run.expect("the interpreter's table is valid");
                    (run.start..run.end)
                        .step_by(2)
                        .map(move |offset| (offset, run.line))
                });
                assert_eq!(runs.last().map_or(0, |run| run.1), length);
                let expected = runs.into_iter().flat_map(|(start, end, line)| {
                    (start..end).step_by(2).map(move |offset| (offset, line))
                });
                assert!(
                    decoded.eq(expected),
                    "{}: a code object at line {first}",
                    interpreter.display()
                );

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

        let mut wide = vec![0x80 | (13 << 3)];
        wide.extend([0x40; 10]);
        wide.push(0x10);
        assert_eq!(line_at(&wide, 10, 0), Err(Malformed));
    }
}
