//! Source Map v3 (ECMA-426): generated-to-original positions and reverse lookup.
//!
//! Maps decode once into segments sorted by generated position and indexed by
//! original position. Without a segment at or before a position on the same
//! line, it has no original position; mappings never carry across lines.
//!
//! Lines and UTF-16 code-unit columns are zero-based. [`Line`] and [`Column`]
//! convert to and from one-based stack-trace positions only through named
//! constructors and accessors.
//!
//! Every malformed input is a [`SourceMapError`]. Input sizes are bounded
//! before decoding, and decoding is linear in the input apart from one sort.

use std::collections::HashMap;
use std::ffi::OsString;
use std::os::unix::ffi::OsStringExt as _;
use std::path::{Component, Path, PathBuf};
use std::sync::Arc;

/// The largest map this decoder reads, in bytes of JSON.
pub const MAX_MAP_BYTES: usize = 256 << 20;

/// The most segments one map may hold, across all its sections.
pub const MAX_SEGMENTS: usize = 1 << 24;

/// The most sections one index map may hold.
pub const MAX_SECTIONS: usize = 1 << 16;

/// A line of a document, counted from zero, as source maps count lines.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct Line(u32);

impl Line {
    #[must_use]
    pub const fn from_zero_based(line: u32) -> Self {
        Self(line)
    }

    /// Converts a one-based line; `None` for zero.
    #[must_use]
    pub const fn from_one_based(line: u32) -> Option<Self> {
        match line.checked_sub(1) {
            Some(line) => Some(Self(line)),
            None => None,
        }
    }

    #[must_use]
    pub const fn zero_based(self) -> u32 {
        self.0
    }

    /// The line counted from one, or `None` past `u32::MAX`.
    #[must_use]
    pub const fn one_based(self) -> Option<u32> {
        self.0.checked_add(1)
    }
}

/// A column within a line, counted from zero in UTF-16 code units.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct Column(u32);

impl Column {
    #[must_use]
    pub const fn from_zero_based(column: u32) -> Self {
        Self(column)
    }

    /// The column a one-based count names, or `None` for zero.
    #[must_use]
    pub const fn from_one_based(column: u32) -> Option<Self> {
        match column.checked_sub(1) {
            Some(column) => Some(Self(column)),
            None => None,
        }
    }

    #[must_use]
    pub const fn zero_based(self) -> u32 {
        self.0
    }

    /// The column counted from one, or `None` past `u32::MAX`.
    #[must_use]
    pub const fn one_based(self) -> Option<u32> {
        self.0.checked_add(1)
    }
}

/// A place in a document. Positions order by line, then column.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct Position {
    pub line: Line,
    pub column: Column,
}

impl Position {
    #[must_use]
    pub const fn new(line: Line, column: Column) -> Self {
        Self { line, column }
    }
}

crate::model::id_type!(SourceId, "An original source in one source map's table.");
crate::model::id_type!(NameId, "A name in one source map's table.");

/// Where a document is: a file, or a URL this module does not read, as an
/// `https:` or `webpack:` one.
#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub enum Location {
    Path(PathBuf),
    Url(Arc<str>),
}

/// One original source a map names.
#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub struct Source {
    /// The map's `sources` entry as written, or `None` where it is null.
    pub name: Option<Arc<str>>,

    /// Where the entry leads, after `sourceRoot`, from the map's location;
    /// `None` for a null entry or one that does not resolve to a location.
    pub location: Option<Location>,

    /// The source's text, when the map carries it in `sourcesContent`.
    pub content: Option<Arc<str>>,

    /// Whether the map's `ignoreList` names the source as third-party code.
    pub ignored: bool,
}

/// The original place a segment maps generated code to.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct OriginalPlace {
    pub source: SourceId,
    pub line: Line,
    pub column: Column,

    /// The original name, as of the identifier the segment starts at.
    pub name: Option<NameId>,
}

/// The segment that covers a generated position.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Covering {
    /// The column, on the position's own line, where the segment starts.
    pub start: Column,

    /// Its original place, or `None` for a segment of generated code that
    /// has none.
    pub original: Option<OriginalPlace>,
}

/// A generated position whose segment maps to an original line.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct GeneratedMatch {
    pub generated: Position,
    pub original_column: Column,
}

/// Which of the segments that map to an original line a breakpoint uses.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ColumnChoice {
    /// The line's least original column, across all generated copies.
    LineStart,

    /// The greatest original column at or before this one on the same line.
    /// None when every segment starts after it.
    Covering(Column),
}

/// Why a source map or a reference to one cannot be read.
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
pub enum SourceMapError {
    #[error("the source map's {what} exceeds the limit of {limit}")]
    TooLarge { what: &'static str, limit: usize },

    #[error("the source map is not valid JSON of the expected shape: {0}")]
    Json(Arc<str>),

    #[error("the source map's version is not 3")]
    Version,

    #[error("the source map has no `{0}` field")]
    MissingField(&'static str),

    #[error("the source map's mappings are malformed at byte {offset}: {reason}")]
    Mappings { offset: usize, reason: &'static str },

    #[error("section {section} of the index map: {error}")]
    Section { section: usize, error: Box<Self> },

    #[error("section {section} of the index map {reason}")]
    Sections {
        section: usize,
        reason: &'static str,
    },

    #[error("the source map URL is malformed: {0}")]
    Url(&'static str),
}

type Result<T> = std::result::Result<T, SourceMapError>;

const fn mappings_error(offset: usize, reason: &'static str) -> SourceMapError {
    SourceMapError::Mappings { offset, reason }
}

/// A decoded source map.
#[derive(Debug, Clone)]
pub struct SourceMap {
    file: Option<Arc<str>>,
    sources: Vec<Source>,
    name_text: String,

    /// Where each name ends in `name_text`; a name starts where the one
    /// before it ends.
    name_ends: Vec<u32>,

    /// Each segment's generated start, sorted, with segments that start at
    /// one position in the order the map lists them.
    generated: Vec<Position>,

    /// Each segment's original place, beside `generated`.
    originals: Vec<Option<OriginalPlace>>,

    /// The segments that cover some position and have an original place,
    /// as indexes into `generated`, sorted by original place and then by
    /// generated position.
    by_original: Vec<u32>,
}

impl SourceMap {
    /// Decodes a map, or an index map of sections, whose own location is
    /// `base`: the map file's, or for a map inline in a script, the
    /// script's. Sources resolve against it.
    pub fn parse(json: &[u8], base: &Location) -> Result<Self> {
        if json.len() > MAX_MAP_BYTES {
            return Err(SourceMapError::TooLarge {
                what: "size in bytes",
                limit: MAX_MAP_BYTES,
            });
        }

        let raw: RawMap<'_> = serde_json::from_slice(json)
            .map_err(|error| SourceMapError::Json(error.to_string().into()))?;

        let mut builder = Builder::default();
        let file = raw.file.as_deref().map(Arc::from);
        if let Some(sections) = &raw.sections {
            check_version(raw.version.as_ref())?;
            builder.sections(sections, base)?;
        } else {
            builder.section(&raw, base, Position::default(), false)?;
        }

        let map = builder.finish(file);
        if cfg!(debug_assertions) {
            map.check_invariants();
        }
        Ok(map)
    }

    /// The generated file's name as the map gives it.
    #[must_use]
    pub fn file(&self) -> Option<&str> {
        self.file.as_deref()
    }

    pub fn sources(&self) -> impl ExactSizeIterator<Item = (SourceId, &Source)> {
        self.sources
            .iter()
            .enumerate()
            .map(|(index, source)| (SourceId::new(index_u32(index)), source))
    }

    #[must_use]
    pub fn source(&self, id: SourceId) -> Option<&Source> {
        self.sources.get(id.index())
    }

    #[must_use]
    pub fn name(&self, id: NameId) -> Option<&str> {
        let end = *self.name_ends.get(id.index())?;
        let start = match id.index().checked_sub(1) {
            Some(before) => *self.name_ends.get(before)?,
            None => 0,
        };
        self.name_text
            .get(usize::try_from(start).ok()?..usize::try_from(end).ok()?)
    }

    #[must_use]
    pub const fn segment_count(&self) -> usize {
        self.generated.len()
    }

    /// The segment covering a generated position: the last one, in the
    /// map's order, of those that start at the greatest position at or
    /// before it on the same line. A segment that another starts at the
    /// same position after covers nothing. `None` when no segment on the
    /// position's line starts at or before it.
    #[must_use]
    pub fn covering(&self, at: Position) -> Option<Covering> {
        let after = self.generated.partition_point(|start| *start <= at);
        let index = after.checked_sub(1)?;
        let start = *self.generated.get(index)?;
        if start.line != at.line {
            return None;
        }

        Some(Covering {
            start: start.column,
            original: *self.originals.get(index)?,
        })
    }

    /// The original place of a generated position: that of the segment
    /// covering it, if one does and it has one.
    #[must_use]
    pub fn original_at(&self, at: Position) -> Option<OriginalPlace> {
        self.covering(at)?.original
    }

    /// Every generated position whose covering segment maps to `line` of
    /// `source`, in order of original column and then generated position.
    /// Each maps back to the same line through [`Self::original_at`].
    pub fn generated_for_line(
        &self,
        source: SourceId,
        line: Line,
    ) -> impl Iterator<Item = GeneratedMatch> + '_ {
        let key = |index: &u32| {
            self.original_of(*index)
                .map(|place| (place.source, place.line))
        };
        let target = Some((source, line));
        let start = self
            .by_original
            .partition_point(|index| key(index) < target);
        let end = self
            .by_original
            .partition_point(|index| key(index) <= target);
        self.by_original
            .get(start..end)
            .unwrap_or_default()
            .iter()
            .filter_map(|&index| {
                let generated = *self.generated.get(usize::try_from(index).ok()?)?;
                Some(GeneratedMatch {
                    generated,
                    original_column: self.original_of(index)?.column,
                })
            })
    }

    /// The generated positions a breakpoint on `line` of `source` uses,
    /// chosen by `column`, in generated order. Empty when no segment maps
    /// to the line: a breakpoint never moves to another line.
    #[must_use]
    pub fn breakpoint_positions(
        &self,
        source: SourceId,
        line: Line,
        column: ColumnChoice,
    ) -> Vec<Position> {
        let matches: Vec<GeneratedMatch> = self.generated_for_line(source, line).collect();
        let chosen = match column {
            ColumnChoice::LineStart => matches.first().map(|first| first.original_column),
            ColumnChoice::Covering(column) => matches
                .iter()
                .map(|candidate| candidate.original_column)
                .filter(|start| *start <= column)
                .max(),
        };

        let Some(chosen) = chosen else {
            return Vec::new();
        };

        let mut positions: Vec<Position> = matches
            .iter()
            .filter(|candidate| candidate.original_column == chosen)
            .map(|candidate| candidate.generated)
            .collect();
        positions.sort_unstable();
        positions
    }

    fn original_of(&self, index: u32) -> Option<OriginalPlace> {
        *self.originals.get(usize::try_from(index).ok()?)?
    }

    /// Checks the tables against each other.
    pub fn check_invariants(&self) {
        debug_assert_eq!(
            self.generated.len(),
            self.originals.len(),
            "a source map's generated and original tables differ in length",
        );
        debug_assert!(
            self.generated.is_sorted(),
            "a source map's segments are out of generated order",
        );
        debug_assert!(
            self.name_ends.is_sorted()
                && self.name_ends.last().is_none_or(
                    |end| usize::try_from(*end).is_ok_and(|end| end == self.name_text.len())
                ),
            "a source map's name table does not partition its text",
        );

        for original in self.originals.iter().flatten() {
            debug_assert!(
                original.source.index() < self.sources.len(),
                "a segment names source {} of {}",
                original.source,
                self.sources.len(),
            );
            debug_assert!(
                original
                    .name
                    .is_none_or(|name| name.index() < self.name_ends.len()),
                "a segment names a name past the table's {}",
                self.name_ends.len(),
            );
        }

        let mut previous = None;
        for &index in &self.by_original {
            let index = index as usize;
            debug_assert!(
                self.originals.get(index).is_some_and(Option::is_some),
                "the original index holds segment {index}, which has no original place",
            );
            debug_assert!(
                self.generated.get(index + 1) != self.generated.get(index),
                "the original index holds segment {index}, which covers nothing",
            );

            let key = self.originals.get(index).copied().flatten().map(|place| {
                (
                    place.source,
                    place.line,
                    place.column,
                    self.generated.get(index).copied(),
                )
            });
            debug_assert!(
                previous < key,
                "the original index is out of order at segment {index}",
            );
            previous = key;
        }
    }
}

fn index_u32(index: usize) -> u32 {
    debug_assert!(
        u32::try_from(index).is_ok(),
        "table row {index} does not fit 32 bits, past MAX_SEGMENTS and MAX_MAP_BYTES",
    );
    u32::try_from(index).unwrap_or(u32::MAX)
}

fn check_version(version: Option<&serde_json::Value>) -> Result<()> {
    match version.and_then(serde_json::Value::as_f64) {
        Some(version) if version.to_bits() == 3.0f64.to_bits() => Ok(()),
        _ => Err(SourceMapError::Version),
    }
}

#[derive(serde::Deserialize)]
struct RawMap<'a> {
    version: Option<serde_json::Value>,
    file: Option<String>,
    #[serde(rename = "sourceRoot")]
    source_root: Option<String>,
    sources: Option<Vec<Option<String>>>,
    #[serde(rename = "sourcesContent")]
    sources_content: Option<Vec<Option<String>>>,
    names: Option<Vec<String>>,
    #[serde(borrow)]
    mappings: Option<std::borrow::Cow<'a, str>>,
    #[serde(rename = "ignoreList")]
    ignore_list: Option<Vec<u32>>,
    #[serde(rename = "x_google_ignoreList")]
    google_ignore_list: Option<Vec<u32>>,
    #[serde(borrow)]
    sections: Option<Vec<RawSection<'a>>>,
}

#[derive(serde::Deserialize)]
struct RawSection<'a> {
    offset: RawOffset,
    #[serde(borrow)]
    map: Option<Box<RawMap<'a>>>,
    url: Option<serde::de::IgnoredAny>,
}

#[derive(serde::Deserialize)]
struct RawOffset {
    line: u32,
    column: u32,
}

/// Decoded segments before they are sorted.
#[derive(Default)]
struct Builder {
    sources: Vec<Source>,
    name_text: String,
    name_ends: Vec<u32>,
    segments: Vec<(Position, Option<OriginalPlace>)>,

    /// Each source's id, by its record, in an index map, whose sections
    /// share a source the spec decodes as one record.
    shared: HashMap<Source, SourceId>,
}

impl Builder {
    fn sections(&mut self, sections: &[RawSection<'_>], base: &Location) -> Result<()> {
        if sections.len() > MAX_SECTIONS {
            return Err(SourceMapError::TooLarge {
                what: "number of sections",
                limit: MAX_SECTIONS,
            });
        }

        let mut previous_offset: Option<Position> = None;
        let mut previous_last: Option<Position> = None;
        for (index, section) in sections.iter().enumerate() {
            let offset = Position::new(Line(section.offset.line), Column(section.offset.column));
            if previous_offset.is_some_and(|previous| offset < previous) {
                return Err(SourceMapError::Sections {
                    section: index,
                    reason: "starts before the section before it",
                });
            }
            if previous_last.is_some_and(|previous| offset < previous) {
                return Err(SourceMapError::Sections {
                    section: index,
                    reason: "starts before the last segment of the section before it",
                });
            }
            if section.url.is_some() {
                return Err(SourceMapError::Sections {
                    section: index,
                    reason: "refers to its map by URL, which ECMA-426 does not allow",
                });
            }
            let Some(map) = &section.map else {
                return Err(SourceMapError::Sections {
                    section: index,
                    reason: "has no map",
                });
            };
            if map.sections.is_some() {
                return Err(SourceMapError::Sections {
                    section: index,
                    reason: "is itself an index map, which ECMA-426 does not allow",
                });
            }

            let first = self.segments.len();
            self.section(map, base, offset, true)
                .map_err(|error| SourceMapError::Section {
                    section: index,
                    error: Box::new(error),
                })?;
            previous_offset = Some(offset);
            if let Some(last) = self
                .segments
                .get(first..)
                .and_then(|added| added.iter().map(|(position, _)| *position).max())
            {
                previous_last = Some(last);
            }
        }

        Ok(())
    }

    /// Decodes one map that is not an index map into the tables, its
    /// generated positions moved by `offset`.
    fn section(
        &mut self,
        raw: &RawMap<'_>,
        base: &Location,
        offset: Position,
        shared: bool,
    ) -> Result<()> {
        check_version(raw.version.as_ref())?;
        let Some(mappings) = &raw.mappings else {
            return Err(SourceMapError::MissingField("mappings"));
        };
        let Some(sources) = &raw.sources else {
            return Err(SourceMapError::MissingField("sources"));
        };

        let ignore_list = raw.ignore_list.as_ref().or(raw.google_ignore_list.as_ref());
        let prefix = source_prefix(raw.source_root.as_deref());
        let mut ids = Vec::with_capacity(sources.len());
        for (index, name) in sources.iter().enumerate() {
            let location = name
                .as_deref()
                .and_then(|name| resolve_source(base, prefix.as_deref(), name));
            let content = raw
                .sources_content
                .as_ref()
                .and_then(|contents| contents.get(index))
                .and_then(|content| content.as_deref().map(Arc::from));
            let ignored = ignore_list.is_some_and(|list| {
                list.iter()
                    .any(|ignored| usize::try_from(*ignored).is_ok_and(|i| i == index))
            });
            let source = Source {
                name: name.as_deref().map(Arc::from),
                location,
                content,
                ignored,
            };
            ids.push(self.add_source(source, shared));
        }

        let names = raw.names.as_deref().unwrap_or_default();
        let first_name = self.name_ends.len();
        for name in names {
            self.name_text.push_str(name);
            let end =
                u32::try_from(self.name_text.len()).map_err(|_| SourceMapError::TooLarge {
                    what: "name text",
                    limit: u32::MAX as usize,
                })?;
            self.name_ends.push(end);
        }

        decode_mappings(
            mappings.as_bytes(),
            &ids,
            first_name,
            names.len(),
            offset,
            &mut self.segments,
        )
    }

    fn add_source(&mut self, source: Source, shared: bool) -> SourceId {
        if shared && let Some(id) = self.shared.get(&source) {
            return *id;
        }

        let id = SourceId::new(index_u32(self.sources.len()));
        if shared {
            self.shared.insert(source.clone(), id);
        }
        self.sources.push(source);
        id
    }

    fn finish(self, file: Option<Arc<str>>) -> SourceMap {
        let mut segments = self.segments;
        if !segments.is_sorted_by_key(|(position, _)| *position) {
            segments.sort_by_key(|(position, _)| *position);
        }

        let (generated, originals): (Vec<_>, Vec<_>) = segments.into_iter().unzip();
        let mut by_original: Vec<u32> = (0..generated.len())
            .filter(|&index| {
                originals.get(index).is_some_and(Option::is_some)
                    && generated.get(index + 1) != generated.get(index)
            })
            .map(index_u32)
            .collect();
        by_original.sort_unstable_by_key(|&index| {
            let index = index as usize;
            let place = originals.get(index).copied().flatten();
            (
                place.map(|place| (place.source, place.line, place.column)),
                generated.get(index).copied(),
            )
        });

        SourceMap {
            file,
            sources: self.sources,
            name_text: self.name_text,
            name_ends: self.name_ends,
            generated,
            originals,
            by_original,
        }
    }
}

/// Decodes a `mappings` string, whose sources are `sources` and whose names
/// are the `name_count` from `first_name`, into `out`, its generated
/// positions moved by `offset`: lines by its line, and the first line's
/// columns by its column.
fn decode_mappings(
    text: &[u8],
    sources: &[SourceId],
    first_name: usize,
    name_count: usize,
    offset: Position,
    out: &mut Vec<(Position, Option<OriginalPlace>)>,
) -> Result<()> {
    let mut at = 0;
    let mut line: u32 = 0;
    let mut column: i64 = 0;
    let mut source: i64 = 0;
    let mut original_line: i64 = 0;
    let mut original_column: i64 = 0;
    let mut name: i64 = 0;

    loop {
        match text.get(at) {
            None => break,
            Some(b';') => {
                at += 1;
                line = line
                    .checked_add(1)
                    .ok_or_else(|| mappings_error(at, "it has more lines than fit 32 bits"))?;
                column = 0;
                continue;
            }
            Some(b',') => return Err(mappings_error(at, "a segment is empty")),
            Some(_) => {}
        }

        let start = at;
        let mut fields = [0i64; 5];
        let mut count = 0;
        loop {
            let value = decode_vlq(text, &mut at)?;
            let Some(field) = fields.get_mut(count) else {
                return Err(mappings_error(start, "a segment has more than 5 fields"));
            };
            *field = value;
            count += 1;
            if matches!(text.get(at), None | Some(b',' | b';')) {
                break;
            }
        }
        if !matches!(count, 1 | 4 | 5) {
            return Err(mappings_error(
                start,
                "a segment has neither 1, 4, nor 5 fields",
            ));
        }

        if out.len() >= MAX_SEGMENTS {
            return Err(SourceMapError::TooLarge {
                what: "number of segments",
                limit: MAX_SEGMENTS,
            });
        }

        let [
            column_delta,
            source_delta,
            line_delta,
            original_column_delta,
            name_delta,
        ] = fields;
        column += column_delta;
        let local_column = u32::try_from(column)
            .map_err(|_| mappings_error(start, "a generated column is out of range"))?;
        let generated = generated_position(offset, line, local_column)
            .ok_or_else(|| mappings_error(start, "a generated position overflows 32 bits"))?;

        let original = if count == 1 {
            None
        } else {
            source += source_delta;
            original_line += line_delta;
            original_column += original_column_delta;
            let source = usize::try_from(source)
                .ok()
                .and_then(|index| sources.get(index))
                .ok_or_else(|| mappings_error(start, "a source index is out of range"))?;
            let line = u32::try_from(original_line)
                .map_err(|_| mappings_error(start, "an original line is out of range"))?;
            let original_column = u32::try_from(original_column)
                .map_err(|_| mappings_error(start, "an original column is out of range"))?;

            let name = if count == 5 {
                name += name_delta;
                let index = usize::try_from(name)
                    .ok()
                    .filter(|index| *index < name_count)
                    .and_then(|index| index.checked_add(first_name))
                    .ok_or_else(|| mappings_error(start, "a name index is out of range"))?;
                Some(NameId::new(index_u32(index)))
            } else {
                None
            };

            Some(OriginalPlace {
                source: *source,
                line: Line(line),
                column: Column(original_column),
                name,
            })
        };
        out.push((generated, original));

        if text.get(at) == Some(&b',') {
            at += 1;
            if matches!(text.get(at), None | Some(b',' | b';')) {
                return Err(mappings_error(at, "a segment is empty"));
            }
        }
    }

    Ok(())
}

fn generated_position(offset: Position, line: u32, column: u32) -> Option<Position> {
    let column = if line == 0 {
        offset.column.0.checked_add(column)?
    } else {
        column
    };
    Some(Position::new(
        Line(offset.line.0.checked_add(line)?),
        Column(column),
    ))
}

/// Decodes one base64 VLQ at `at`, moving `at` past it. Values are limited
/// to 32 bits, sign included, as ECMA-426's `VLQSignedValue` defines.
fn decode_vlq(text: &[u8], at: &mut usize) -> Result<i64> {
    let start = *at;
    let mut value: u64 = 0;
    let mut shift: u32 = 0;
    loop {
        let Some(&byte) = text.get(*at) else {
            return Err(mappings_error(
                start,
                "a number ends without its last digit",
            ));
        };
        let digit = base64_digit(byte).ok_or_else(|| {
            if matches!(byte, b',' | b';') {
                mappings_error(start, "a number ends without its last digit")
            } else {
                mappings_error(*at, "a character is not a base64 digit")
            }
        })?;
        *at += 1;

        let bits = u64::from(digit & 0x1f);
        if bits != 0 {
            if shift >= 32 {
                return Err(mappings_error(start, "a number exceeds 32 bits"));
            }
            value |= bits << shift;
            if value >= 1 << 32 {
                return Err(mappings_error(start, "a number exceeds 32 bits"));
            }
        }
        shift = shift.saturating_add(5);
        if digit & 0x20 == 0 {
            break;
        }
    }

    let magnitude = i64::try_from(value >> 1).unwrap_or(i64::MAX);
    Ok(if value & 1 == 0 {
        magnitude
    } else if magnitude == 0 {
        -(1 << 31)
    } else {
        -magnitude
    })
}

const fn base64_digit(byte: u8) -> Option<u8> {
    match byte {
        b'A'..=b'Z' => Some(byte - b'A'),
        b'a'..=b'z' => Some(byte - b'a' + 26),
        b'0'..=b'9' => Some(byte - b'0' + 52),
        b'+' => Some(62),
        b'/' => Some(63),
        _ => None,
    }
}

/// What `sourceRoot` puts before each relative source: the root and a
/// separating `/`. An empty root puts nothing, as browsers and Mozilla's
/// library read it, where ECMA-426's text would put a lone `/` and make
/// every relative source absolute.
fn source_prefix(root: Option<&str>) -> Option<String> {
    let root = root.filter(|root| !root.is_empty())?;
    Some(if root.ends_with('/') {
        root.to_owned()
    } else {
        format!("{root}/")
    })
}

/// Where a `sources` entry leads from the map's location. A root goes
/// before a relative entry only, as browsers apply it: an entry with its
/// own scheme stands alone.
fn resolve_source(base: &Location, prefix: Option<&str>, name: &str) -> Option<Location> {
    match prefix {
        Some(prefix) if split_scheme(name).is_none() => {
            let joined = match name.strip_prefix('/') {
                Some(rest) if prefix.ends_with('/') => format!("{prefix}{rest}"),
                _ => format!("{prefix}{name}"),
            };
            base.resolve(&joined).ok()
        }
        _ => base.resolve(name).ok(),
    }
}

impl Location {
    /// Where `reference`, a URL or a path relative to this document as a
    /// URL is relative to its base, leads. A path's percent escapes are
    /// decoded, and its `.` and `..` segments resolved by name, as URL
    /// resolution does.
    pub fn resolve(&self, reference: &str) -> Result<Self> {
        if reference.is_empty() {
            return Err(SourceMapError::Url("it is empty"));
        }

        if let Some((scheme, _)) = split_scheme(reference) {
            if scheme.eq_ignore_ascii_case("file") {
                return file_url_path(reference).map(Self::Path);
            }
            return Ok(Self::Url(reference.into()));
        }

        match self {
            Self::Path(base) => {
                let path = strip_query(reference);
                if path.starts_with("//") {
                    return file_url_path(&format!("file:{path}")).map(Self::Path);
                }

                let decoded = PathBuf::from(OsString::from_vec(percent_decode(path)));
                let mut joined = if decoded.is_absolute() {
                    PathBuf::from("/")
                } else {
                    base.parent().map(Path::to_path_buf).unwrap_or_default()
                };
                for component in decoded.components() {
                    match component {
                        Component::ParentDir => {
                            joined.pop();
                        }
                        Component::Normal(part) => joined.push(part),
                        Component::RootDir | Component::CurDir | Component::Prefix(_) => {}
                    }
                }
                Ok(Self::Path(joined))
            }
            Self::Url(base) => join_url(base, reference)
                .map(|url| Self::Url(url.into()))
                .ok_or(SourceMapError::Url(
                    "it is relative to a URL that has no hierarchical path",
                )),
        }
    }
}

/// A URL's scheme and what follows its colon, if it begins with one.
fn split_scheme(text: &str) -> Option<(&str, &str)> {
    let (scheme, rest) = text.split_once(':')?;
    let mut bytes = scheme.bytes();
    let valid = bytes
        .next()
        .is_some_and(|first| first.is_ascii_alphabetic())
        && bytes.all(|byte| byte.is_ascii_alphanumeric() || matches!(byte, b'+' | b'-' | b'.'));
    valid.then_some((scheme, rest))
}

fn strip_query(reference: &str) -> &str {
    reference.split(['?', '#']).next().unwrap_or(reference)
}

/// The local path a `file:` URL names. A host other than `localhost` is
/// another machine's.
fn file_url_path(url: &str) -> Result<PathBuf> {
    let rest = url.get(5..).unwrap_or_default();
    let rest = strip_query(rest);
    let path = match rest.strip_prefix("//") {
        Some(authority_and_path) => {
            let slash = authority_and_path
                .find('/')
                .unwrap_or(authority_and_path.len());
            let (host, path) = authority_and_path.split_at(slash);
            if !host.is_empty() && !host.eq_ignore_ascii_case("localhost") {
                return Err(SourceMapError::Url("a file URL names another host"));
            }
            path
        }
        None => rest,
    };
    if !path.starts_with('/') {
        return Err(SourceMapError::Url("a file URL's path is not absolute"));
    }

    let mut joined = PathBuf::from("/");
    let decoded = PathBuf::from(OsString::from_vec(percent_decode(path)));
    for component in decoded.components() {
        match component {
            Component::ParentDir => {
                joined.pop();
            }
            Component::Normal(part) => joined.push(part),
            Component::RootDir | Component::CurDir | Component::Prefix(_) => {}
        }
    }
    Ok(joined)
}

/// Resolves a relative reference against a URL with an authority, as
/// RFC 3986 does; `None` for a base with no `//` authority.
fn join_url(base: &str, reference: &str) -> Option<String> {
    let (scheme, rest) = split_scheme(base)?;
    let rest = rest.strip_prefix("//")?;
    if reference.starts_with("//") {
        return Some(format!("{scheme}:{reference}"));
    }

    let authority_end = rest.find(['/', '?', '#']).unwrap_or(rest.len());
    let (authority, after) = rest.split_at(authority_end);
    let base_path = strip_query(after);

    let suffix_start = reference.find(['?', '#']).unwrap_or(reference.len());
    let (path, suffix) = reference.split_at(suffix_start);
    let merged = if path.starts_with('/') {
        path.to_owned()
    } else if path.is_empty() {
        base_path.to_owned()
    } else {
        let directory = base_path.rfind('/').map_or("/", |end| &base_path[..=end]);
        let directory = if directory.is_empty() { "/" } else { directory };
        format!("{directory}{path}")
    };
    Some(format!(
        "{scheme}://{authority}{}{suffix}",
        remove_dot_segments(&merged)
    ))
}

/// RFC 3986's `remove_dot_segments` for an absolute path.
fn remove_dot_segments(path: &str) -> String {
    let segments: Vec<&str> = path.split('/').skip(1).collect();
    let mut out: Vec<&str> = Vec::with_capacity(segments.len());
    let last = segments.len().saturating_sub(1);
    for (index, segment) in segments.iter().enumerate() {
        match *segment {
            "." => {
                if index == last {
                    out.push("");
                }
            }
            ".." => {
                out.pop();
                if index == last {
                    out.push("");
                }
            }
            segment => out.push(segment),
        }
    }
    format!("/{}", out.join("/"))
}

fn percent_decode(text: &str) -> Vec<u8> {
    let bytes = text.as_bytes();
    let mut out = Vec::with_capacity(bytes.len());
    let mut at = 0;
    while let Some(&byte) = bytes.get(at) {
        if byte == b'%'
            && let Some(high) = bytes.get(at + 1).and_then(|digit| hex_value(*digit))
            && let Some(low) = bytes.get(at + 2).and_then(|digit| hex_value(*digit))
        {
            out.push((high << 4) | low);
            at += 3;
        } else {
            out.push(byte);
            at += 1;
        }
    }
    out
}

const fn hex_value(digit: u8) -> Option<u8> {
    match digit {
        b'0'..=b'9' => Some(digit - b'0'),
        b'a'..=b'f' => Some(digit - b'a' + 10),
        b'A'..=b'F' => Some(digit - b'A' + 10),
        _ => None,
    }
}

/// Where a script's source map is.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum MapReference {
    /// The map is the JSON of a `data:` URL, whose sources resolve against
    /// the script's location, as browsers and Node resolve them.
    Inline(Vec<u8>),

    /// The map is the document at this location, whose sources resolve
    /// against it.
    Document(Location),
}

impl MapReference {
    /// Where the source map URL a script at `script` gives leads.
    pub fn resolve(script: &Location, url: &str) -> Result<Self> {
        if let Some((scheme, rest)) = split_scheme(url)
            && scheme.eq_ignore_ascii_case("data")
        {
            return decode_data_url(rest).map(Self::Inline);
        }

        script.resolve(url).map(Self::Document)
    }

    /// The location the map's sources resolve against.
    #[must_use]
    pub const fn base<'a>(&'a self, script: &'a Location) -> &'a Location {
        match self {
            Self::Inline(_) => script,
            Self::Document(location) => location,
        }
    }
}

/// Finds a script's source map: the URL its last `sourceMappingURL`
/// comment gives, resolved against `script`, its location. `None` when the
/// script names no map.
pub fn locate(script: &Location, text: &str) -> Result<Option<MapReference>> {
    source_mapping_url(text)
        .map(|url| MapReference::resolve(script, url))
        .transpose()
}

/// The URL in a script's `//# sourceMappingURL=` comment, or the legacy
/// `//@` one, as ECMA-426's `JavaScriptExtractSourceMapURL` extracts it
/// without parsing: from the last line backwards, past lines of nothing
/// but whitespace and comments, stopping at the first line with code. A
/// comment holding a quote or `*/` makes the extraction fail, since it may
/// be inside a string or a block comment.
#[must_use]
pub fn source_mapping_url(script: &str) -> Option<&str> {
    for line in script.rsplit(['\n', '\r', '\u{2028}', '\u{2029}']) {
        let code = line.trim_start_matches(is_js_whitespace);
        if code.is_empty() {
            continue;
        }

        let comment = code.strip_prefix("//")?;
        if comment.contains(['"', '\'', '`']) || comment.contains("*/") {
            return None;
        }
        if let Some(url) = match_source_mapping_url(comment) {
            return Some(url);
        }
    }

    None
}

/// ECMA-426's `MatchSourceMapURL`: the URL in a comment that matches
/// `^[@#]\s*sourceMappingURL=(\S*?)\s*$`.
fn match_source_mapping_url(comment: &str) -> Option<&str> {
    let rest = comment.strip_prefix(['@', '#'])?;
    let rest = rest.trim_start_matches(is_js_regexp_space);
    let url = rest.strip_prefix("sourceMappingURL=")?;
    let url = url.trim_end_matches(is_js_regexp_space);
    (!url.contains(is_js_regexp_space)).then_some(url)
}

/// ECMAScript's `WhiteSpace`.
const fn is_js_whitespace(character: char) -> bool {
    matches!(
        character,
        '\t' | '\u{b}' | '\u{c}' | '\u{feff}' | ' ' | '\u{a0}' | '\u{1680}' | '\u{2000}'
            ..='\u{200a}' | '\u{202f}' | '\u{205f}' | '\u{3000}'
    )
}

/// What `\s` matches in an ECMAScript regular expression: `WhiteSpace` and
/// `LineTerminator`.
const fn is_js_regexp_space(character: char) -> bool {
    is_js_whitespace(character) || matches!(character, '\n' | '\r' | '\u{2028}' | '\u{2029}')
}

/// The JSON of a `data:` URL, given what follows `data:`: a JSON media
/// type, with a UTF-8 charset if any, and its body percent-decoded and,
/// with `;base64`, base64-decoded as the Fetch standard decodes it.
fn decode_data_url(rest: &str) -> Result<Vec<u8>> {
    if rest.len() > MAX_MAP_BYTES.saturating_mul(2) {
        return Err(SourceMapError::TooLarge {
            what: "data URL",
            limit: MAX_MAP_BYTES.saturating_mul(2),
        });
    }

    let (header, body) = rest
        .split_once(',')
        .ok_or(SourceMapError::Url("a data URL has no comma"))?;
    let mut parameters = header.split(';').map(str::trim);
    let media_type = parameters.next().unwrap_or_default();
    if !media_type.eq_ignore_ascii_case("application/json") {
        return Err(SourceMapError::Url(
            "a data URL's media type is not application/json",
        ));
    }

    let mut base64 = false;
    for parameter in parameters {
        if parameter.eq_ignore_ascii_case("base64") {
            base64 = true;
            continue;
        }
        if let Some((key, value)) = parameter.split_once('=')
            && key.trim().eq_ignore_ascii_case("charset")
        {
            let value = value.trim().trim_matches('"');
            if !value.eq_ignore_ascii_case("utf-8") && !value.eq_ignore_ascii_case("utf8") {
                return Err(SourceMapError::Url("a data URL's charset is not UTF-8"));
            }
        }
    }

    let body = percent_decode(body);
    let json = if base64 {
        forgiving_base64(&body).ok_or(SourceMapError::Url("a data URL's base64 is malformed"))?
    } else {
        body
    };
    if json.len() > MAX_MAP_BYTES {
        return Err(SourceMapError::TooLarge {
            what: "size in bytes",
            limit: MAX_MAP_BYTES,
        });
    }
    Ok(json)
}

/// The Infra standard's forgiving-base64 decode.
fn forgiving_base64(input: &[u8]) -> Option<Vec<u8>> {
    let mut digits: Vec<u8> = input
        .iter()
        .copied()
        .filter(|byte| !matches!(byte, b'\t' | b'\n' | b'\x0c' | b'\r' | b' '))
        .collect();
    if digits.len().is_multiple_of(4) {
        for _ in 0..2 {
            if digits.last() == Some(&b'=') {
                digits.pop();
            }
        }
    }
    if digits.len() % 4 == 1 {
        return None;
    }

    let mut out = Vec::with_capacity(digits.len() / 4 * 3 + 2);
    let mut buffer: u32 = 0;
    let mut bits = 0;
    for &byte in &digits {
        buffer = ((buffer << 6) | u32::from(base64_digit(byte)?)) & 0xff_ffff;
        bits += 6;
        if bits >= 8 {
            bits -= 8;
            out.push(u8::try_from((buffer >> bits) & 0xff).ok()?);
        }
    }
    Some(out)
}

#[cfg(feature = "fuzzing")]
pub fn fuzz(data: &[u8]) {
    let script = Location::Path(PathBuf::from("/fuzz/dist/app.js"));
    if let Ok(map) = SourceMap::parse(
        data,
        &Location::Path(PathBuf::from("/fuzz/dist/app.js.map")),
    ) {
        fuzz_queries(&map, data);
    }

    if let Ok(text) = std::str::from_utf8(data)
        && let Ok(Some(reference)) = locate(&script, text)
        && let MapReference::Inline(json) = &reference
        && let Ok(map) = SourceMap::parse(json, reference.base(&script))
    {
        fuzz_queries(&map, data);
    }
}

#[cfg(feature = "fuzzing")]
fn fuzz_queries(map: &SourceMap, data: &[u8]) {
    for pair in data.chunks(2).take(256) {
        let line = Line(u32::from(pair.first().copied().unwrap_or(0) % 16));
        let column = Column(u32::from(pair.get(1).copied().unwrap_or(0)));
        let at = Position::new(line, column);
        if let Some(covering) = map.covering(at) {
            assert!(covering.start <= column);
        }

        for (source, _) in map.sources().take(16) {
            for found in map.generated_for_line(source, line) {
                let back = map.original_at(found.generated);
                assert_eq!(
                    back.map(|place| (place.source, place.line)),
                    Some((source, line))
                );
            }
            let _ = map.breakpoint_positions(source, line, ColumnChoice::Covering(column));
        }
    }
}

#[cfg(test)]
mod tests;

#[cfg(test)]
mod node_oracle;
