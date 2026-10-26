//! What a fixture reports about itself, in `TRUTH` lines on its output.
//!
//! Runtime facts, such as thread frames or variable values, follow `TRUTH`
//! as tab-separated fields. Fields escape backslashes, tabs, and newlines
//! as `\\`, `\t`, and `\n`. A `checkpoint` first field starts a checkpoint
//! named by the second field; subsequent lines describe that program state
//! until the next checkpoint.
//!
//! Expected values come from the running program, not an earlier microscope
//! run, so source edits and toolchain changes do not make them stale. Each
//! suite also checks that the comparison rejects the errors it should catch.

use std::path::Path;

/// Every `TRUTH` line a fixture printed, in order, each split into its
/// fields, unescaped.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct Report {
    lines: Vec<Vec<String>>,
}

/// The fields of one line.
pub type Fields = [String];

impl Report {
    /// The `TRUTH` lines of `text`; other lines are the program's own.
    pub fn parse(text: &str) -> Self {
        Self {
            lines: text
                .lines()
                .filter_map(|line| line.strip_prefix("TRUTH\t"))
                .map(|line| line.split('\t').map(unescape).collect())
                .collect(),
        }
    }

    /// The report a fixture wrote to the file at `path`.
    pub fn read(path: &Path) -> Self {
        let text = std::fs::read_to_string(path).unwrap_or_else(|error| {
            panic!("read the fixture's output {}: {error}", path.display())
        });
        Self::parse(&text)
    }

    /// Every line, in order.
    pub fn lines(&self) -> impl Iterator<Item = &Fields> {
        self.lines.iter().map(Vec::as_slice)
    }

    /// The lines of the last checkpoint named `name`, or `None` when the
    /// program reached no such checkpoint.
    pub fn checkpoint(&self, name: &str) -> Option<&[Vec<String>]> {
        let start = self.lines.iter().rposition(|fields| {
            is_checkpoint(fields) && fields.get(1).is_some_and(|n| n == name)
        })?;

        let lines = &self.lines[start + 1..];
        let end = lines
            .iter()
            .position(|fields| is_checkpoint(fields))
            .unwrap_or(lines.len());

        Some(&lines[..end])
    }

    /// The name and lines of the last checkpoint the program reached.
    pub fn latest(&self) -> Option<(&str, &[Vec<String>])> {
        let start = self
            .lines
            .iter()
            .rposition(|fields| is_checkpoint(fields))?;

        let name = self.lines[start].get(1)?;

        Some((name, &self.lines[start + 1..]))
    }
}

fn is_checkpoint(fields: &[String]) -> bool {
    fields.first().is_some_and(|first| first == "checkpoint")
}

/// A field's text, with its escapes undone. A backslash before any other
/// character, or at the end, stands for itself.
pub fn unescape(field: &str) -> String {
    let mut text = String::with_capacity(field.len());
    let mut characters = field.chars();
    while let Some(character) = characters.next() {
        if character != '\\' {
            text.push(character);
            continue;
        }

        match characters.next() {
            Some('t') => text.push('\t'),
            Some('n') => text.push('\n'),
            Some('\\') | None => text.push('\\'),
            Some(other) => {
                text.push('\\');
                text.push(other);
            }
        }
    }

    text
}
