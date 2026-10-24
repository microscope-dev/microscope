//! Source maps of the scripts V8 runs: TypeScript compiled by tsx, or a
//! bundle esbuild wrote. A script's map is found when V8 parses it and
//! decoded once something needs it: a breakpoint in one of its sources,
//! or a frame in it. Frames in the script then show their original place,
//! and a breakpoint in an original source is set at each place in the
//! script that maps to its line.
//!
//! V8 runs a `CommonJS` module as soon as it compiles it, before microscope
//! hears of the script, so while there are line breakpoints every
//! isolate holds one more: a sentinel at the start of each script of the
//! program, whose pause lets the mapped breakpoints be set before the
//! script's first statement runs (V8's `beforeScriptWithSourceMapExecution`
//! pauses only for ES modules).

use std::cell::OnceCell;
use std::path::{Path, PathBuf};
use std::sync::Arc;

use crate::RecordedPlace;
use crate::debug_info::js_source_map::{
    Column, ColumnChoice, Line, Location, MAX_MAP_BYTES, MapReference, Position, SourceMap,
};

/// The source map a script names, decoded on first use.
#[derive(Debug)]
pub(super) struct ScriptMap {
    /// The script's own path.
    script: Arc<str>,

    /// The map's URL, as the script's `sourceMappingURL` comment gives it.
    url: Arc<str>,
    decoded: OnceCell<std::result::Result<Arc<SourceMap>, Arc<str>>>,
}

impl ScriptMap {
    pub(super) const fn new(script: Arc<str>, url: Arc<str>) -> Self {
        Self {
            script,
            url,
            decoded: OnceCell::new(),
        }
    }

    /// Whether the map is the one `url` names.
    pub(super) fn is(&self, url: &str) -> bool {
        *self.url == *url
    }

    /// The decoded map, or why it cannot be read.
    pub(super) fn map(&self) -> std::result::Result<&SourceMap, Arc<str>> {
        self.decoded
            .get_or_init(|| decode(&self.script, &self.url))
            .as_deref()
            .map_err(Arc::clone)
    }

    /// Where the map leads the place `generated` in the script, as a
    /// frame shows it: in an original source, or `None` with the reason
    /// when the map has no place for it.
    pub(super) fn original(
        &self,
        generated: &RecordedPlace,
    ) -> std::result::Result<RecordedPlace, Arc<str>> {
        let map = self.map()?;
        let at = Position::new(
            Line::from_one_based(generated.line).ok_or("the frame has no line")?,
            Column::from_one_based(generated.column).unwrap_or(Column::from_zero_based(0)),
        );
        let original = map
            .original_at(at)
            .ok_or("the source map maps this generated code to no source")?;
        let source = map
            .source(original.source)
            .and_then(|source| match source.location.as_ref()? {
                Location::Path(path) => Some(path.to_string_lossy().into_owned()),
                Location::Url(url) => Some(url.to_string()),
            })
            .ok_or("the source map names no source here")?;
        Ok(RecordedPlace {
            path: source.into(),
            line: original
                .line
                .one_based()
                .ok_or("the original line is out of range")?,
            column: original
                .column
                .one_based()
                .ok_or("the original column is out of range")?,
        })
    }

    /// The zero-based positions in the script a breakpoint on one-based
    /// `line` of the source at `path` is set at; none when the map does
    /// not name the source or maps nothing to the line.
    pub(super) fn positions(&self, path: &Path, line: u32) -> Vec<Position> {
        let (Ok(map), Some(line)) = (self.map(), Line::from_one_based(line)) else {
            return Vec::new();
        };
        map.sources()
            .filter(|(_, source)| {
                matches!(&source.location, Some(Location::Path(source)) if names(source, path))
            })
            .flat_map(|(id, _)| map.breakpoint_positions(id, line, ColumnChoice::LineStart))
            .collect()
    }
}

/// Whether the breakpoint path `path` names the source file at `source`:
/// an absolute path the same file, and a relative one the file it ends.
pub(super) fn names(source: &Path, path: &Path) -> bool {
    if path.is_absolute() {
        source == path
    } else {
        source.ends_with(path)
    }
}

/// Decodes the map `url` names for the script at `script`.
fn decode(script: &str, url: &str) -> std::result::Result<Arc<SourceMap>, Arc<str>> {
    let location = Location::Path(PathBuf::from(script));
    let error = |error: &dyn std::fmt::Display| Arc::<str>::from(format!("{error}"));
    let reference = MapReference::resolve(&location, url).map_err(|e| error(&e))?;
    let map = match &reference {
        MapReference::Inline(json) => SourceMap::parse(json, &location),
        MapReference::Document(Location::Path(path)) => {
            let json = read_map(path)?;
            SourceMap::parse(&json, reference.base(&location))
        }
        MapReference::Document(Location::Url(url)) => {
            return Err(format!(
                "microscope reads source maps from files and data: URLs, not {url}"
            )
            .into());
        }
    };
    let map = map.map_err(|e| error(&e))?;
    map.check_invariants();
    record!("decoded the source map of {script}");
    Ok(Arc::new(map))
}

/// The JSON of the map file at `path`, read whole if it is not too large.
fn read_map(path: &Path) -> std::result::Result<Vec<u8>, Arc<str>> {
    let cannot = |error: std::io::Error| {
        Arc::<str>::from(format!(
            "cannot read the source map {}: {error}",
            path.display()
        ))
    };
    let size = std::fs::metadata(path).map_err(cannot)?.len();
    if usize::try_from(size).map_or(true, |size| size > MAX_MAP_BYTES) {
        return Err(format!(
            "the source map {} is larger than microscope reads ({MAX_MAP_BYTES} bytes)",
            path.display()
        )
        .into());
    }
    std::fs::read(path).map_err(cannot)
}

/// The URL pattern of the sentinel breakpoints: every script of the
/// program, which leaves out `node_modules` unless `breakpoints`, the
/// paths of the line breakpoints, name a file there.
pub(super) fn sentinel_pattern<'a>(mut breakpoints: impl Iterator<Item = &'a Path>) -> String {
    let modules = breakpoints.any(|path| {
        path.components()
            .any(|component| component.as_os_str() == "node_modules")
    });
    if modules {
        "^(?:file://)?/".to_owned()
    } else {
        "^(?:file://)?/(?!.*/node_modules/)".to_owned()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A map that leads generated line 0, at columns 0, 4, and 10, to
    /// zero-based lines 2, 2, and 3 of `app.ts`, and leaves column 30 on
    /// unmapped. "AAEA" is (0, 0, +2, 0), "IAAI" (+4, 0, 0, +4), "MACJ"
    /// (+6, 0, +1, -4), and "U" (+20) a segment of one field.
    fn script_map() -> ScriptMap {
        let json = r#"{"version":3,"sources":["app.ts"],"names":[],"mappings":"AAEA,IAAI,MACJ,U"}"#;
        let url = format!("data:application/json;base64,{}", base64(json.as_bytes()));
        ScriptMap::new("/srv/dist/app.js".into(), url.into())
    }

    fn base64(bytes: &[u8]) -> String {
        const DIGITS: &[u8] = b"ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz0123456789+/";
        let mut out = String::new();
        for chunk in bytes.chunks(3) {
            let word = chunk.iter().enumerate().fold(0u32, |word, (index, &byte)| {
                word | u32::from(byte) << (16 - 8 * index)
            });
            for index in 0..=chunk.len() {
                out.push(char::from(DIGITS[(word >> (18 - 6 * index) & 63) as usize]));
            }
        }
        while !out.len().is_multiple_of(4) {
            out.push('=');
        }
        out
    }

    fn place(path: &str, line: u32, column: u32) -> RecordedPlace {
        RecordedPlace {
            path: path.into(),
            line,
            column,
        }
    }

    #[test]
    fn a_frame_shows_the_original_place_its_segment_leads_to() {
        let map = script_map();
        // One-based column 7 is zero-based 6, inside the segment at 4.
        assert_eq!(
            map.original(&place("/srv/dist/app.js", 1, 7)),
            Ok(place("/srv/dist/app.ts", 3, 5))
        );
        assert_eq!(
            map.original(&place("/srv/dist/app.js", 1, 11)),
            Ok(place("/srv/dist/app.ts", 4, 1))
        );
        assert!(
            map.original(&place("/srv/dist/app.js", 1, 31)).is_err(),
            "a segment without a source maps nowhere"
        );
        assert!(
            map.original(&place("/srv/dist/app.js", 2, 1)).is_err(),
            "a line without segments maps nowhere, even after a mapped one"
        );
    }

    #[test]
    fn a_breakpoint_is_set_where_its_lines_first_expression_starts() {
        let map = script_map();
        let at = |line, column| {
            Position::new(Line::from_zero_based(line), Column::from_zero_based(column))
        };
        assert_eq!(map.positions(Path::new("/srv/dist/app.ts"), 3), [at(0, 0)]);
        assert_eq!(map.positions(Path::new("app.ts"), 4), [at(0, 10)]);
        assert_eq!(map.positions(Path::new("/srv/dist/app.ts"), 2), []);
        assert_eq!(map.positions(Path::new("/srv/other/app.ts"), 3), []);
        assert_eq!(
            map.positions(Path::new("p.ts"), 3),
            [],
            "a name is matched whole"
        );
    }

    #[test]
    fn a_map_that_cannot_be_read_says_why() {
        let missing = ScriptMap::new("/nonexistent/app.js".into(), "app.js.map".into());
        let reason = missing.map().expect_err("no such file");
        assert!(reason.contains("/nonexistent/app.js.map"), "{reason}");
        let remote = ScriptMap::new(
            "/srv/app.js".into(),
            "https://example.com/app.js.map".into(),
        );
        assert!(
            remote
                .map()
                .is_err_and(|reason| reason.contains("https://example.com"))
        );
    }

    #[test]
    fn the_sentinel_leaves_out_node_modules_unless_a_breakpoint_is_there() {
        let plain = sentinel_pattern([Path::new("/srv/app.ts")].into_iter());
        assert!(plain.contains("node_modules"));
        let inside = sentinel_pattern([Path::new("/srv/node_modules/lib/x.ts")].into_iter());
        assert!(!inside.contains("node_modules"));
    }
}
