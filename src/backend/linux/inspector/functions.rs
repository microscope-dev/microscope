//! Resolves V8 function-name breakpoints by querying `Profiler.getBestEffortCoverage` and
//! mapping UTF-16 code-unit offsets back to `(line, column)` positions.

use std::collections::{BTreeMap, BTreeSet};
use std::sync::Arc;

use super::super::Controller;
use super::super::cdp::{CdpError, Connection, RequestId, Session};
use super::super::native::LinuxTraceOps;
use super::{Isolate, Placement, SiteState, send};
use crate::debug_info::js_source_map::{Column, Line, Position};
use crate::runtime_model::{ManagedArm, inspector_protocol};
use crate::{ManagedPlace, Result, RuntimeId};

#[derive(Debug, Default)]
pub(super) struct Search {
    requests: BTreeMap<RequestId, Step>,

    again: bool,
}

#[derive(Debug)]
enum Step {
    Enable,
    Coverage,
    Source(Vec<Found>),
    Within(Found),
}

#[derive(Debug, Clone)]
struct Found {
    script: Arc<str>,
    name: Arc<str>,
    start: u32,
    end: u32,
}

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

    pub(super) fn offset(&self, line: u64, column: u64) -> Option<u32> {
        let start = *self.lines.get(usize::try_from(line).ok()?)?;
        let offset = start.checked_add(u32::try_from(column).ok()?)?;
        (usize::try_from(offset).ok()? <= self.units).then_some(offset)
    }
}

fn names(desired: &[ManagedArm]) -> BTreeSet<Arc<str>> {
    desired
        .iter()
        .filter_map(|arm| match &arm.place {
            ManagedPlace::Function { name } => Some(Arc::clone(name)),
            ManagedPlace::Line { .. } => None,
        })
        .collect()
}

pub(super) fn wanted(desired: &[ManagedArm]) -> bool {
    desired
        .iter()
        .any(|arm| matches!(arm.place, ManagedPlace::Function { .. }))
}

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
    pub(super) fn search_functions(&mut self, runtime: RuntimeId, session: &Session) -> bool {
        let Some(inspector) = self
            .session
            .inferior_mut()
            .and_then(|inferior| inferior.inspectors.records.get_mut(&runtime))
        else {
            return false;
        };
        let Some((link, isolate)) = inspector.status.connected_isolate(session) else {
            return false;
        };
        if !wanted(&inspector.desired) {
            return false;
        }
        search(&link.connection, session, isolate);
        isolate.search.is_some()
    }

    pub(super) fn searches(&self, runtime: RuntimeId, id: RequestId) -> bool {
        self.session
            .inferior()
            .and_then(|inferior| inferior.inspectors.records.get(&runtime))
            .is_some_and(|inspector| {
                inspector.status.isolates().values().any(|isolate| {
                    isolate
                        .search
                        .as_ref()
                        .is_some_and(|search| search.requests.contains_key(&id))
                })
            })
    }

    pub(super) fn search_answered(
        &mut self,
        runtime: RuntimeId,
        id: RequestId,
        result: std::result::Result<serde_json::Value, CdpError>,
    ) -> Result<()> {
        let Some(inspector) = self
            .session
            .inferior_mut()
            .and_then(|inferior| inferior.inspectors.records.get_mut(&runtime))
        else {
            return Ok(());
        };
        let Some((link, isolates)) = inspector.status.connected_mut() else {
            return Ok(());
        };
        let names = names(&inspector.desired);
        let Some((session, isolate)) = isolates.iter_mut().find(|(_, isolate)| {
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
                if let Some(text) = source
                    .get("scriptSource")
                    .and_then(serde_json::Value::as_str)
                {
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
            let offset = |name: &str| {
                function
                    .get("ranges")
                    .and_then(|ranges| ranges.get(0))
                    .and_then(|range| range.get(name))
                    .and_then(serde_json::Value::as_u64)
                    .and_then(|offset| u32::try_from(offset).ok())
            };
            let (Some(start), Some(end)) = (offset("startOffset"), offset("endOffset")) else {
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
    let Some(&first) = places.first() else {
        return Err(refused());
    };
    if places
        .iter()
        .any(|&offset| offset <= function.start || offset > function.end)
    {
        return Err(refused());
    }
    text.position(first).ok_or_else(refused)
}

impl Isolate {
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
