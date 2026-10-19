//! What GNU binutils, not microscope, say about each golden binary.
//!
//! `scripts/golden.sh` writes each program's functions, line table, and
//! epilogue markers to `facts.json` beside its binaries. The semantic
//! oracles judge the debugger's steps against these, so that a mistake in
//! microscope's own reading of the debug information cannot excuse itself.
//!
//! Addresses here are the image's own; add the image's bias for where it
//! loads.

use serde::Deserialize;

/// A variable that is, at some addresses, a register's value on entry
/// to its function.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct EntryValue {
    pub name: String,
    pub addresses: std::ops::Range<u64>,

    /// The register, as the simulated CPU names it.
    pub register: String,
}

/// One variant's facts, as the file holds them.
#[derive(Deserialize)]
pub(super) struct VariantFacts {
    pub name: String,
    pub optimized: bool,
    pub inlined: u64,
    functions: Vec<(String, u64, u64)>,
    lines: Vec<(u64, String, i64, bool)>,
    epilogues: Vec<u64>,
    entry_values: Vec<(String, u64, u64, String)>,
}

/// One program's facts file.
#[derive(Deserialize)]
pub(super) struct ProgramFacts {
    pub variants: Vec<VariantFacts>,
}

/// A function the symbol table defines.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Function {
    pub name: String,
    pub start: u64,
    pub end: u64,
}

/// A source line, by file name and line number.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
pub struct Line {
    /// An index into [`Facts::files`].
    pub file: usize,
    pub line: u64,
}

/// The addresses one line table row describes: from its address up to the
/// next row's, in its sequence.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Range {
    pub start: u64,
    pub end: u64,

    /// The row's line, or `None` for code no source line describes.
    pub line: Option<Line>,

    /// Whether the row is a recommended breakpoint location (`is_stmt`).
    pub statement: bool,
}

/// What binutils say about one compiled variant.
#[derive(Debug, Default)]
pub struct Facts {
    /// Whether the variant was compiled with optimization, which inlines,
    /// splits, and turns calls into jumps.
    pub optimized: bool,

    /// How many inlined calls the debug information describes.
    pub inlined: u64,

    /// File names, without directories.
    pub files: Vec<String>,

    /// Functions by start address.
    pub functions: Vec<Function>,

    /// Rows that describe code, by start address. Rows at one address
    /// before the last describe no instruction.
    pub ranges: Vec<Range>,

    /// Every statement row, uncollapsed, as address and line: where each
    /// line's code begins, though a later row at the address says what
    /// executes there.
    pub statements: Vec<(u64, Line)>,

    /// Where rows mark the beginning of a function's epilogue.
    pub epilogues: std::collections::BTreeSet<u64>,

    /// Where location lists say a variable is exactly what a register held
    /// when its function was entered.
    pub entry_values: Vec<EntryValue>,
}

impl Facts {
    /// Reads one variant's rows as gdb does: rows at one address collapse
    /// into one, a statement if any of them is, at the line of the last
    /// statement among them. A later row that is no statement, such as the
    /// line an inlined call came from, does not say where execution stands.
    pub(super) fn new(variant: VariantFacts) -> Result<Self, String> {
        let mut files = Vec::<String>::new();
        let mut ranges = Vec::<Range>::new();
        let mut statements = Vec::new();

        // The row being collapsed: its address, line, and whether it is a
        // statement.
        let mut open: Option<(u64, Option<Line>, bool)> = None;
        let close =
            |open: &mut Option<(u64, Option<Line>, bool)>, end: u64, ranges: &mut Vec<Range>| {
                if let Some((start, line, statement)) = open.take()
                    && start < end
                {
                    ranges.push(Range {
                        start,
                        end,
                        line,
                        statement,
                    });
                }
            };

        for (address, file, line, statement) in &variant.lines {
            let line = match u64::try_from(*line) {
                // An end of sequence closes the last row.
                Err(_) if *line == -1 => {
                    close(&mut open, *address, &mut ranges);
                    continue;
                }
                Err(_) => return Err(format!("row at {address:#x} has line {line}")),
                Ok(0) => None,
                Ok(line) => {
                    let file = files
                        .iter()
                        .position(|known| known == file)
                        .unwrap_or_else(|| {
                            files.push(file.clone());
                            files.len() - 1
                        });
                    Some(Line { file, line })
                }
            };

            if *statement && let Some(line) = line {
                statements.push((*address, line));
            }

            let same_address = open.is_some_and(|(start, ..)| start == *address);

            // A row of no source ends what came before it at its address.
            if line.is_some()
                && same_address
                && open.is_some_and(|(.., earlier)| earlier)
                && !statement
            {
                continue;
            }

            let statement = line.is_some()
                && (*statement
                    || same_address
                        && open.is_some_and(|(_, earlier_line, earlier)| {
                            earlier && earlier_line.is_some()
                        }));

            close(&mut open, *address, &mut ranges);
            open = Some((*address, line, statement));
        }

        ranges.sort_by_key(|range| range.start);
        let mut functions = variant
            .functions
            .into_iter()
            .map(|(name, start, size)| Function {
                name,
                start,
                end: start + size,
            })
            .collect::<Vec<_>>();
        functions.sort_by_key(|function| function.start);

        if !variant.optimized && variant.inlined > 0 {
            return Err(format!(
                "unoptimized {} describes {} inlined calls",
                variant.name, variant.inlined
            ));
        }

        Ok(Self {
            optimized: variant.optimized,
            inlined: variant.inlined,
            files,
            functions,
            ranges,
            statements,
            epilogues: variant.epilogues.into_iter().collect(),
            entry_values: variant
                .entry_values
                .into_iter()
                .map(|(name, start, end, register)| EntryValue {
                    name,
                    addresses: start..end,
                    register,
                })
                .collect(),
        })
    }

    /// The row describing the instruction at `address`. A row that began
    /// in an earlier function runs on, until the next row, through code
    /// without line information, such as hand-written assembly placed
    /// after it, but it does not describe that code.
    #[must_use]
    pub fn range(&self, address: u64) -> Option<&Range> {
        let after = self.ranges.partition_point(|range| range.start <= address);
        self.ranges[..after].last().filter(|range| {
            address < range.end
                && self
                    .function(address)
                    .is_none_or(|function| function.start <= range.start)
        })
    }

    /// Whether a row begins at `address`.
    #[must_use]
    pub fn starts_row(&self, address: u64) -> bool {
        self.range(address)
            .is_some_and(|range| range.start == address)
    }

    /// The register whose value on entry to its function a variable named
    /// `name` is at `address`, unless variables of that name there say
    /// different things.
    #[must_use]
    pub fn entry_register(&self, name: &str, address: u64) -> Option<&str> {
        let mut registers = self
            .entry_values
            .iter()
            .filter(|value| value.name == name && value.addresses.contains(&address))
            .map(|value| value.register.as_str());
        let register = registers.next()?;
        registers.all(|other| other == register).then_some(register)
    }

    /// The function containing `address`.
    #[must_use]
    pub fn function(&self, address: u64) -> Option<&Function> {
        let after = self
            .functions
            .partition_point(|function| function.start <= address);
        self.functions[..after]
            .iter()
            .rev()
            .find(|function| address < function.end)
    }

    /// The addresses where rows of `line` in `file` begin.
    pub fn line_starts<'a>(&'a self, file: &'a str, line: u64) -> impl Iterator<Item = u64> + 'a {
        self.ranges
            .iter()
            .filter(move |range| {
                range
                    .line
                    .is_some_and(|known| known.line == line && self.files[known.file] == file)
            })
            .map(|range| range.start)
    }

    /// A line's file name.
    #[must_use]
    pub fn file(&self, line: Line) -> &str {
        &self.files[line.file]
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Rows become ranges up to the next row. Rows at one address collapse
    /// into a statement at the last statement's line; an end of sequence
    /// describes nothing; line zero names no source.
    #[test]
    fn rows_describe_code_up_to_the_next_row() {
        let facts = Facts::new(VariantFacts {
            name: "test".into(),
            optimized: true,
            inlined: 0,
            functions: vec![("f".into(), 0x10, 0x20), ("g".into(), 0x42, 0x2)],
            lines: vec![
                (0x10, "a.c".into(), 3, true),
                (0x10, "a.c".into(), 4, true),
                (0x10, "a.c".into(), 5, false),
                (0x14, "a.c".into(), 0, false),
                (0x18, "b.c".into(), 9, false),
                (0x20, "a.c".into(), -1, false),
                (0x40, "a.c".into(), 7, true),
                (0x44, "a.c".into(), -1, false),
            ],
            epilogues: Vec::new(),
            entry_values: Vec::new(),
        })
        .expect("valid facts");
        let line = |address| facts.range(address).and_then(|range| range.line);
        assert_eq!(line(0x12).map(|line| line.line), Some(4));
        assert!(facts.range(0x12).is_some_and(|range| range.statement));
        assert!(!facts.range(0x18).is_some_and(|range| range.statement));
        assert!(facts.starts_row(0x10) && !facts.starts_row(0x12));
        assert_eq!(facts.range(0x14).map(|range| range.line), Some(None));
        assert_eq!(
            line(0x1f).map(|line| (facts.file(line), line.line)),
            Some(("b.c", 9))
        );
        assert_eq!(facts.range(0x20), None);
        assert_eq!(facts.range(0x30), None);
        assert_eq!(line(0x41).map(|line| line.line), Some(7));
        assert_eq!(facts.range(0x44), None);
        assert_eq!(facts.line_starts("a.c", 4).collect::<Vec<_>>(), [0x10]);

        let statements = facts
            .statements
            .iter()
            .map(|&(address, line)| (address, facts.file(line), line.line))
            .collect::<Vec<_>>();
        assert_eq!(
            statements,
            [(0x10, "a.c", 3), (0x10, "a.c", 4), (0x40, "a.c", 7)]
        );
        assert_eq!(facts.function(0x2f).map(|f| f.name.as_str()), Some("f"));
        assert_eq!(facts.function(0x30), None);

        // The row at 0x40 does not describe a function that begins after it.
        assert_eq!(facts.range(0x42).map(|range| range.start), None);
    }
}
