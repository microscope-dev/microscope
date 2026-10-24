//! Function breakpoints, `js:NAME`, through V8's own lists: a search asks
//! for the best-effort coverage of the isolate, which names every function
//! V8 has compiled, called or not, with its range in its script. Each
//! function named NAME then gets a breakpoint at its own first breakable
//! place, which V8 lists when asked from where the function begins.
//!
//! A search runs when a function breakpoint is armed, at the start of each
//! script, which a sentinel holds while there is one, and each time the
//! program resumes. A function nested in one that has never run is not yet
//! compiled, so V8 lists it only after its parent first runs.

use std::collections::{BTreeMap, BTreeSet};
use std::sync::Arc;

use super::super::Controller;
use super::super::cdp::{CdpError, Connection, RequestId, Session};
use super::super::native::LinuxTraceOps;
use super::{Isolate, Placement, SiteState, send};
use crate::debug_info::js_source_map::{Column, Line, Position};
use crate::runtime_model::{ManagedArm, inspector_protocol};
use crate::{ManagedPlace, Result, RuntimeId};

/// A search of one isolate's functions, under way.
#[derive(Debug, Default)]
pub(super) struct Search {
    requests: BTreeMap<RequestId, Step>,

    /// Whether another search was asked for while this one ran: it may
    /// miss the scripts compiled since it began.
    again: bool,
}

#[derive(Debug)]
enum Step {
    Enable,
    Coverage,
    Source(Vec<Found>),
    Within(Found),
}

/// A function the coverage named, by its script and UTF-16 range.
#[derive(Debug, Clone)]
struct Found {
    script: Arc<str>,
    name: Arc<str>,
    start: u32,
    end: u32,
}

/// Where each line of a script starts, and its length, in UTF-16 code
/// units as V8 counts offsets and columns.
#[derive(Debug)]
pub(super) struct Text {
    units: usize,
    lines: Vec<u32>,
}

impl Text {
    pub(super) fn new(source: &str) -> Self {
        let units = source.encode_utf16().collect::<Vec<_>>();
        let mut lines = vec![0];
        for (index, &unit) in units.iter().enumerate() {
            // V8's line terminators; a CR before an LF ends no line.
            let ends = match unit {
                0x0A | 0x2028 | 0x2029 => true,
                0x0D => units.get(index + 1) != Some(&0x0A),
                _ => false,
            };
            if ends && let Ok(next) = u32::try_from(index + 1) {
                lines.push(next);
            }
        }
        Self {
            units: units.len(),
            lines,
        }
    }

    /// The zero-based line and column of `offset`.
    pub(super) fn position(&self, offset: u32) -> Option<Position> {
        if usize::try_from(offset).ok()? > self.units {
            return None;
        }
        let line = self.lines.partition_point(|&start| start <= offset) - 1;
        let start = *self.lines.get(line)?;
        Some(Position::new(
            Line::from_zero_based(u32::try_from(line).ok()?),
            Column::from_zero_based(offset - start),
        ))
    }

    /// The offset of the zero-based `line` and `column`.
    pub(super) fn offset(&self, line: u64, column: u64) -> Option<u32> {
        let start = *self.lines.get(usize::try_from(line).ok()?)?;
        let offset = start.checked_add(u32::try_from(column).ok()?)?;
        (usize::try_from(offset).ok()? <= self.units).then_some(offset)
    }
}

/// The names of the function breakpoints among `desired`.
fn names(desired: &[ManagedArm]) -> BTreeSet<Arc<str>> {
    desired
        .iter()
        .filter_map(|arm| match &arm.place {
            ManagedPlace::Function { name } => Some(Arc::clone(name)),
            ManagedPlace::Line { .. } => None,
        })
        .collect()
}

/// Whether `desired` holds a function breakpoint.
pub(super) fn wanted(desired: &[ManagedArm]) -> bool {
    desired
        .iter()
        .any(|arm| matches!(arm.place, ManagedPlace::Function { .. }))
}

/// Begins a search of the isolate `session` reaches, or has the one under
/// way run again once it ends.
pub(super) fn search(connection: &Connection, session: &Session, isolate: &mut Isolate) {
    if let Some(search) = isolate.search.as_mut() {
        search.again = true;
        return;
    }
    let mut search = Search::default();
    if !isolate.profiling {
        // Coverage needs the domain; enabling it changes nothing V8 runs.
        if let Ok(request) = send(
            connection,
            session,
            inspector_protocol::ENABLE_PROFILER,
            serde_json::json!({}),
        ) {
            isolate.profiling = true;
            search.requests.insert(request, Step::Enable);
        }
    }
    if let Ok(request) = send(
        connection,
        session,
        inspector_protocol::COVERAGE,
        serde_json::json!({}),
    ) {
        search.requests.insert(request, Step::Coverage);
        isolate.search = Some(search);
    }
}

impl<P: LinuxTraceOps> Controller<P> {
    /// Begins a search of the isolate `session` reaches in `runtime`, when
    /// the runtime has a function breakpoint. Returns whether one runs.
    pub(super) fn search_functions(&mut self, runtime: RuntimeId, session: &Session) -> bool {
        let Some(inspector) = self
            .inferior
            .as_mut()
            .and_then(|inferior| inferior.inspectors.records.get_mut(&runtime))
        else {
            return false;
        };
        let (Some(link), Some(isolate)) = (
            inspector.connection.as_ref(),
            inspector.isolates.get_mut(session),
        ) else {
            return false;
        };
        if !wanted(&inspector.desired) {
            return false;
        }
        search(&link.connection, session, isolate);
        isolate.search.is_some()
    }

    /// Whether `id` is a request of a search in `runtime`.
    pub(super) fn searches(&self, runtime: RuntimeId, id: RequestId) -> bool {
        self.inferior
            .as_ref()
            .and_then(|inferior| inferior.inspectors.records.get(&runtime))
            .is_some_and(|inspector| {
                inspector.isolates.values().any(|isolate| {
                    isolate
                        .search
                        .as_ref()
                        .is_some_and(|search| search.requests.contains_key(&id))
                })
            })
    }

    /// Takes V8's answer to the search request `id` in `runtime` a step
    /// on, and once the search ends, judges the sentinel's pause it held.
    pub(super) fn search_answered(
        &mut self,
        runtime: RuntimeId,
        id: RequestId,
        result: std::result::Result<serde_json::Value, CdpError>,
    ) -> Result<()> {
        let Some(inspector) = self
            .inferior
            .as_mut()
            .and_then(|inferior| inferior.inspectors.records.get_mut(&runtime))
        else {
            return Ok(());
        };
        let Some(link) = inspector.connection.as_ref() else {
            return Ok(());
        };
        let names = names(&inspector.desired);
        let Some((session, isolate)) = inspector.isolates.iter_mut().find(|(_, isolate)| {
            isolate
                .search
                .as_ref()
                .is_some_and(|search| search.requests.contains_key(&id))
        }) else {
            return Ok(());
        };
        let step = isolate
            .search
            .as_mut()
            .and_then(|search| search.requests.remove(&id));
        let numbers = numbers_by_name(&inspector.desired);
        let mut placed = false;
        match (step, result) {
            (Some(Step::Coverage), Ok(coverage)) => {
                found(&link.connection, session, isolate, &names, &coverage);
            }
            (Some(Step::Source(functions)), Ok(source)) => {
                if let Some(text) = source["scriptSource"].as_str() {
                    let text = Arc::new(Text::new(text));
                    for function in functions {
                        isolate
                            .texts
                            .insert(Arc::clone(&function.script), Arc::clone(&text));
                        within(&link.connection, session, isolate, function);
                    }
                }
            }
            (Some(Step::Within(function)), result) => {
                place(
                    &link.connection,
                    session,
                    isolate,
                    &numbers,
                    &function,
                    result,
                );
                placed = true;
            }
            #[cfg_attr(
                not(debug_assertions),
                expect(unused_variables, reason = "only recorded")
            )]
            (Some(step), Err(error)) => record!("the search's {step:?} failed: {error}"),
            (Some(Step::Enable), Ok(_)) | (None, _) => {}
        }
        let ended = isolate
            .search
            .as_ref()
            .is_some_and(|search| search.requests.is_empty());
        let deferred = if ended {
            if isolate.search.take().is_some_and(|search| search.again) {
                search(&link.connection, session, isolate);
            }
            isolate
                .ready_deferred()
                .map(|pause| (session.clone(), pause))
        } else {
            None
        };
        if placed {
            self.publish_breakpoints_changed();
        }
        if let Some((session, pause)) = deferred {
            return self.inspector_paused(runtime, &session, &pause);
        }
        Ok(())
    }
}

/// The numbers of the function breakpoints among `desired`, by name.
fn numbers_by_name(desired: &[ManagedArm]) -> BTreeMap<Arc<str>, Vec<u64>> {
    let mut numbers = BTreeMap::<Arc<str>, Vec<u64>>::new();
    for arm in desired {
        if let ManagedPlace::Function { name } = &arm.place {
            numbers
                .entry(Arc::clone(name))
                .or_default()
                .push(arm.number);
        }
    }
    numbers
}

/// Goes on from the coverage `coverage` of an isolate: each function it
/// names among `names`, in the program's scripts and not yet placed, is
/// looked into in its script's text, which is asked for first if needed.
fn found(
    connection: &Connection,
    session: &Session,
    isolate: &mut Isolate,
    names: &BTreeSet<Arc<str>>,
    coverage: &serde_json::Value,
) {
    let mut by_script = BTreeMap::<Arc<str>, Vec<Found>>::new();
    for script in coverage["result"].as_array().into_iter().flatten() {
        let (Some(id), Some(url)) = (script["scriptId"].as_str(), script["url"].as_str()) else {
            continue;
        };
        // Node's own code is no part of the program's.
        if url.is_empty() || url.starts_with("node:") {
            continue;
        }
        for function in script["functions"].as_array().into_iter().flatten() {
            let Some(name) = function["functionName"]
                .as_str()
                .and_then(|name| names.get(name))
            else {
                continue;
            };
            let range = &function["ranges"][0];
            let (Some(start), Some(end)) = (
                range["startOffset"]
                    .as_u64()
                    .and_then(|start| u32::try_from(start).ok()),
                range["endOffset"]
                    .as_u64()
                    .and_then(|end| u32::try_from(end).ok()),
            ) else {
                continue;
            };
            let script = Arc::<str>::from(id);
            if isolate.placed(&script, start) {
                continue;
            }
            by_script
                .entry(Arc::clone(&script))
                .or_default()
                .push(Found {
                    script,
                    name: Arc::clone(name),
                    start,
                    end,
                });
        }
    }
    for (script, functions) in by_script {
        if isolate.texts.contains_key(&script) {
            for function in functions {
                within(connection, session, isolate, function);
            }
        } else if let Ok(request) = send(
            connection,
            session,
            inspector_protocol::SCRIPT_SOURCE,
            serde_json::json!({ "scriptId": &*script }),
        ) && let Some(search) = isolate.search.as_mut()
        {
            search.requests.insert(request, Step::Source(functions));
        }
    }
}

/// Asks for the breakable places of `function` itself, from where the
/// coverage starts it: V8 lists those of the innermost function there.
fn within(connection: &Connection, session: &Session, isolate: &mut Isolate, function: Found) {
    let Some(start) = isolate
        .texts
        .get(&function.script)
        .and_then(|text| text.position(function.start))
    else {
        return;
    };
    if let Ok(request) = send(
        connection,
        session,
        inspector_protocol::POSSIBLE_BREAKPOINTS,
        serde_json::json!({
            "start": {
                "scriptId": &*function.script,
                "lineNumber": start.line.zero_based(),
                "columnNumber": start.column.zero_based(),
            },
            "restrictToFunction": true,
        }),
    ) && let Some(search) = isolate.search.as_mut()
    {
        search.requests.insert(request, Step::Within(function));
    }
}

/// Sets the function breakpoints named as `function` is at its first
/// breakable place, from `result`, the places V8 listed for it. A list
/// with a place outside the function is of another function, and refused.
fn place(
    connection: &Connection,
    session: &Session,
    isolate: &mut Isolate,
    numbers: &BTreeMap<Arc<str>, Vec<u64>>,
    function: &Found,
    result: std::result::Result<serde_json::Value, CdpError>,
) {
    let first = result
        .map_err(|error| Arc::<str>::from(error.to_string()))
        .and_then(|listed| first_place(isolate.texts.get(&function.script), function, &listed));
    for &number in numbers.get(&function.name).into_iter().flatten() {
        let Some(site) = isolate.sites.get_mut(&number) else {
            continue;
        };
        let (state, mapped) = match &first {
            Ok(at) => {
                let sent = send(
                    connection,
                    session,
                    inspector_protocol::SET_BREAKPOINT_AT,
                    serde_json::json!({ "location": {
                        "scriptId": &*function.script,
                        "lineNumber": at.line.zero_based(),
                        "columnNumber": at.column.zero_based(),
                    }}),
                );
                match sent {
                    Ok(request) => (
                        SiteState::Requested(request),
                        Some((Arc::clone(&function.script), *at)),
                    ),
                    Err(error) => (SiteState::Refused(error.to_string().into()), None),
                }
            }
            Err(reason) => (SiteState::Refused(Arc::clone(reason)), None),
        };
        site.placements.push(Placement {
            id: None,
            state,
            mapped,
            actual: None,
            function: Some((Arc::clone(&function.script), function.start)),
        });
    }
}

/// The first of the places V8 `listed` for `function`, once each is in
/// its range: after where it starts, and at or before where it ends,
/// which a concise arrow function returns at.
fn first_place(
    text: Option<&Arc<Text>>,
    function: &Found,
    listed: &serde_json::Value,
) -> std::result::Result<Position, Arc<str>> {
    let text = text.ok_or("microscope did not read the function's script")?;
    let places = listed["locations"]
        .as_array()
        .map(|places| {
            places
                .iter()
                .map(|place| {
                    text.offset(
                        place["lineNumber"].as_u64().unwrap_or(u64::MAX),
                        place["columnNumber"].as_u64().unwrap_or(u64::MAX),
                    )
                })
                .collect::<Option<Vec<_>>>()
        })
        .unwrap_or_default();
    let refused = || {
        Arc::<str>::from(format!(
            "V8 lists no breakable place of {} inside it",
            function.name
        ))
    };
    let places = places.ok_or_else(refused)?;
    if places.is_empty()
        || places
            .iter()
            .any(|&offset| offset <= function.start || offset > function.end)
    {
        return Err(refused());
    }
    text.position(places[0]).ok_or_else(refused)
}

impl Isolate {
    /// Whether a function the coverage started at `start` in `script` was
    /// placed, or refused, already.
    fn placed(&self, script: &Arc<str>, start: u32) -> bool {
        self.sites.values().any(|site| {
            site.placements.iter().any(|placement| {
                placement
                    .function
                    .as_ref()
                    .is_some_and(|(id, at)| id == script && *at == start)
            })
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn at(text: &Text, offset: u32) -> (u32, u32) {
        let position = text.position(offset).expect("a position");
        (position.line.zero_based(), position.column.zero_based())
    }

    #[test]
    fn lines_end_where_v8_ends_them_and_columns_count_utf16() {
        // "a\r\nb\rc\u{2028}d" then an astral character, two units, then "e".
        let text = Text::new("a\r\nb\rc\u{2028}d\u{1F600}e");
        assert_eq!(at(&text, 0), (0, 0));
        assert_eq!(at(&text, 2), (0, 2), "the LF of a CRLF is on its line");
        assert_eq!(at(&text, 3), (1, 0));
        assert_eq!(at(&text, 5), (2, 0), "a lone CR ends a line");
        assert_eq!(at(&text, 7), (3, 0), "so does U+2028");
        assert_eq!(at(&text, 10), (3, 3), "the astral character is two units");
        assert_eq!(text.offset(3, 3), Some(10));
        assert_eq!(text.offset(9, 0), None);
        assert!(text.position(12).is_none());
    }

    #[test]
    fn a_listing_outside_the_function_is_refused() {
        // `x => (y) => y` lists `(y) => y` from 11 to 19; `f` at 0 to 19.
        let text = Some(Arc::new(Text::new("const f = (x) => (y) => y;")));
        let function = Found {
            script: "1".into(),
            name: "f".into(),
            start: 10,
            end: 25,
        };
        let listed = |columns: &[u64]| {
            serde_json::json!({ "locations": columns
                .iter()
                .map(|column| serde_json::json!({ "lineNumber": 0, "columnNumber": column }))
                .collect::<Vec<_>>() })
        };
        let first = first_place(text.as_ref(), &function, &listed(&[17, 25])).expect("its own");
        assert_eq!(first.column.zero_based(), 17);
        assert!(
            first_place(text.as_ref(), &function, &listed(&[10, 25])).is_err(),
            "at its start is its parent's"
        );
        assert!(
            first_place(text.as_ref(), &function, &listed(&[17, 26])).is_err(),
            "past its end"
        );
        assert!(first_place(text.as_ref(), &function, &listed(&[])).is_err());
    }
}
