//! What each isolate is armed with: the runtime's managed locations, as V8
//! breakpoints set by URL and at the places source maps lead their lines
//! to, and which exceptions pause it. A location is ready once any
//! isolate placed it, since a script loads only in the isolates that run
//! it.

use std::collections::BTreeMap;
use std::path::Path;
use std::sync::Arc;

use super::super::Controller;
use super::super::cdp::{Connection, RequestId, Session};
use super::super::native::{InspectionOps, LinuxTraceOps};
use super::{
    Inspector, Isolate, Placement, Sentinel, Site, SiteState, functions, maps, protocol, send,
};
use crate::runtime_model::{Arming, ManagedArm, inspector_protocol};
use crate::{ManagedPlace, RuntimeId};

impl Inspector {
    /// Whether the managed location `number` is set, or why it waits.
    pub(super) fn readiness(&self, number: u64) -> std::result::Result<(), Arc<str>> {
        if let Some(problem) = &self.problem {
            return Err(Arc::clone(problem));
        }
        let states = || {
            self.isolates
                .values()
                .filter_map(|isolate| isolate.sites.get(&number))
                .flat_map(|site| &site.placements)
                .map(|placement| &placement.state)
        };
        if states().any(|state| matches!(state, SiteState::Resolved)) {
            return Ok(());
        }
        if states().any(|state| matches!(state, SiteState::Requested(_))) {
            return Err("waiting for Node's inspector to set the breakpoint".into());
        }
        if let Some(SiteState::Refused(reason)) =
            states().find(|state| matches!(state, SiteState::Refused(_)))
        {
            return Err(Arc::clone(reason));
        }
        if self
            .isolates
            .values()
            .any(|isolate| isolate.sites.contains_key(&number))
        {
            let function = self.desired.iter().find_map(|arm| match &arm.place {
                ManagedPlace::Function { name } if arm.number == number => Some(name),
                _ => None,
            });
            return Err(function.map_or_else(
                || self.unloaded(number),
                |name| {
                    format!(
                        "V8 has compiled no function named {name} yet; microscope looks again \
                         as each script loads and each time the program resumes"
                    )
                    .into()
                },
            ));
        }
        if self.target.is_none() {
            Err(self.unstarted.map_or_else(
                || "Node's inspector has not started yet".into(),
                |why| format!("Node's inspector has not started; {why}").into(),
            ))
        } else {
            Err("Node's inspector is not connected".into())
        }
    }
}

impl Inspector {
    /// Why the line location `number` waits for its script, with the
    /// nearest names of the scripts loaded, as a mistyped path's likeliest
    /// meaning.
    fn unloaded(&self, number: u64) -> Arc<str> {
        const UNLOADED: &str = "no script by that name has loaded yet";
        let Some(path) = self.desired.iter().find_map(|arm| match &arm.place {
            ManagedPlace::Line { path, .. } if arm.number == number => Some(path),
            _ => None,
        }) else {
            return UNLOADED.into();
        };
        // A loaded script is named as the location names its own: by as
        // many trailing components, or whole.
        let depth = (!path.starts_with('/')).then(|| path.split('/').count());
        let loaded = (self.isolates.values())
            .flat_map(|isolate| isolate.scripts.values())
            .filter(|url| !url.is_empty() && !url.starts_with("node:"))
            .map(|url| {
                let script = inspector_protocol::script_path(url);
                let parts = script.split('/').collect::<Vec<_>>();
                let start = depth.map_or(0, |depth| parts.len().saturating_sub(depth));
                parts[start..].join("/")
            })
            .collect::<std::collections::BTreeSet<_>>();
        crate::suggest::did_you_mean(path, loaded.iter().map(String::as_str)).map_or_else(
            || UNLOADED.into(),
            |hint| format!("{UNLOADED}; {hint}").into(),
        )
    }
}

impl<P: LinuxTraceOps> Controller<P> {
    /// Arms the runtime `runtime`, whose inspector the debugger drives,
    /// with `arming`. Its locations are set over the connection when there
    /// is one, and each is ready once V8 places it.
    pub(in crate::backend::linux) fn arm_inspector(
        &mut self,
        runtime: RuntimeId,
        arming: &Arming<'_>,
    ) -> std::result::Result<(), Arc<str>> {
        if arming.assignment.is_some() {
            return Err("microscope cannot yet assign in JavaScript".into());
        }
        let inferior = self
            .inferior
            .as_ref()
            .ok_or_else(|| Arc::<str>::from("the process is gone"))?;
        let main = super::super::debug_thread_id(inferior.tgid);
        let received = self
            .runtimes(inferior)
            .into_iter()
            .find(|bound| bound.id == runtime)
            .and_then(|bound| {
                let entry = bound.model.inspector_message_received()?;
                bound.module.virtual_address(entry).ok()
            });
        let inferior = self
            .inferior
            .as_mut()
            .ok_or_else(|| Arc::<str>::from("the process is gone"))?;
        let inspector = inferior.inspectors.records.entry(runtime).or_default();
        inspector.step = match (arming.step, inspector.step.take()) {
            (None, _) => None,
            (Some(step), Some(stepping)) if stepping.number == step.number => Some(stepping),
            (Some(step), _) => Some(super::steps::stepping(&inspector.isolates, step, main)?),
        };
        // A step from a native stop holds the main thread until its pause
        // is handed to it; V8 holds the thread for any other step.
        let number = inspector
            .step
            .as_ref()
            .filter(|step| step.command == inspector_protocol::PAUSE && !step.sent)
            .map(|step| step.number);
        if inspector.gate.as_ref().map(|gate| gate.number) != number {
            inspector.gate = number.and_then(|number| {
                if received.is_none() {
                    record!(
                        "Node names no MessageReceived; a step from a native stop may pause late"
                    );
                }
                Some(super::steps::Gate::new(number, received?))
            });
        }
        inspector.desired = arming.breakpoints.to_vec();
        inspector.pausing = inspector_protocol::pause_state(arming.exceptions);
        self.sync_step_gates(true)
            .map_err(|error| Arc::<str>::from(error.to_string()))?;
        self.sync_inspector(runtime);
        Ok(())
    }

    /// Brings what each isolate of `runtime` holds to what the runtime is
    /// armed with, connecting first when it can.
    pub(super) fn sync_inspector(&mut self, runtime: RuntimeId) {
        let Some(inspectors) = self
            .inferior
            .as_mut()
            .map(|inferior| &mut inferior.inspectors)
        else {
            return;
        };
        let (Some(inspector), maps) = (inspectors.records.get_mut(&runtime), &inspectors.maps)
        else {
            return;
        };

        let Some(link) = inspector.connection.as_ref().filter(|link| link.connected) else {
            if inspector.connection.is_none()
                && inspector.target.is_some()
                && inspector.problem.is_none()
            {
                self.connect_inspector(runtime);
            }
            return;
        };
        let desired = inspector
            .desired
            .iter()
            .map(|arm| (arm.number, arm))
            .collect::<BTreeMap<_, _>>();
        for (session, isolate) in &mut inspector.isolates {
            let target = Target {
                connection: &link.connection,
                session,
                maps,
            };
            sync_isolate(&target, isolate, &desired, inspector.pausing);
        }
    }

    /// Notes that the script `id` at `url`, in the isolate `session`
    /// reaches, names the source map at `map`, and sets the line
    /// breakpoints in its sources there. A breakpoint set by a path that
    /// is the script's own is withdrawn: its lines are the generated
    /// code's, not the source's.
    pub(super) fn script_mapped(
        &mut self,
        runtime: RuntimeId,
        session: &Session,
        id: &str,
        url: &str,
        map: &str,
    ) {
        let Some(inspectors) = self
            .inferior
            .as_mut()
            .map(|inferior| &mut inferior.inspectors)
        else {
            return;
        };
        let path = Arc::<str>::from(inspector_protocol::script_path(url));
        if inspectors
            .maps
            .get(&path)
            .is_none_or(|known| !known.is(map))
        {
            inspectors.maps.insert(
                Arc::clone(&path),
                maps::ScriptMap::new(Arc::clone(&path), map.into()),
            );
        }
        let (Some(inspector), maps) = (inspectors.records.get_mut(&runtime), &inspectors.maps)
        else {
            return;
        };
        let (Some(link), Some(isolate)) = (
            inspector.connection.as_ref(),
            inspector.isolates.get_mut(session),
        ) else {
            return;
        };
        let target = Target {
            connection: &link.connection,
            session,
            maps,
        };
        let script = (Arc::<str>::from(id), path);
        if maps.get(&script.1).is_none_or(|map| map.map().is_err()) {
            return;
        }
        for arm in &inspector.desired {
            let (ManagedPlace::Line { path, line }, Some(site)) =
                (&arm.place, isolate.sites.get_mut(&arm.number))
            else {
                continue;
            };
            if maps::names(Path::new(&*script.1), Path::new(&**path)) {
                withdraw_by_path(&target, site);
            }
            place_mapped(&target, site, &script, Path::new(&**path), *line);
        }
        self.publish_breakpoints_changed();
    }

    /// Notes what V8 answered to setting a breakpoint in `runtime`.
    #[cfg_attr(
        not(debug_assertions),
        expect(unused_variables, reason = "only recorded")
    )]
    pub(super) fn breakpoint_set(
        &mut self,
        runtime: RuntimeId,
        id: RequestId,
        result: std::result::Result<serde_json::Value, super::super::cdp::CdpError>,
    ) -> crate::Result<()> {
        let Some(inspector) = self
            .inferior
            .as_mut()
            .and_then(|inferior| inferior.inspectors.records.get_mut(&runtime))
        else {
            return Ok(());
        };
        let connection = inspector.connection.as_ref().map(|link| &link.connection);
        for (session, isolate) in &mut inspector.isolates {
            if isolate.withdrawn.remove(&id) {
                if let (Some(connection), Ok(set)) = (connection, &result)
                    && let Some(breakpoint) = set["breakpointId"].as_str()
                {
                    remove(connection, session, breakpoint);
                }
                return Ok(());
            }
            if let Some(sentinel) = isolate
                .sentinel
                .as_mut()
                .filter(|sentinel| matches!(sentinel.placement.state, SiteState::Requested(request) if request == id))
            {
                answer(&mut sentinel.placement, result);
                return Ok(());
            }
            for (number, site) in &mut isolate.sites {
                let Some(index) = site.placements.iter().position(|placement| {
                    matches!(placement.state, SiteState::Requested(request) | SiteState::Withdrawn(request) if request == id)
                }) else {
                    continue;
                };
                let placement = &mut site.placements[index];
                if matches!(placement.state, SiteState::Withdrawn(_)) {
                    if let (Some(connection), Ok(set)) = (connection, &result)
                        && let Some(breakpoint) = set["breakpointId"].as_str()
                    {
                        remove(connection, session, breakpoint);
                    }
                    site.placements.remove(index);
                    return Ok(());
                }
                answer(placement, result);
                record!("location {number} is {:?}", placement.state);
                // A sentinel's pause waits for the last of its script's.
                let deferred = isolate
                    .ready_deferred()
                    .map(|pause| (session.clone(), pause));
                self.publish_breakpoints_changed();
                if let Some((session, pause)) = deferred {
                    return self.inspector_paused(runtime, &session, &pause);
                }
                return Ok(());
            }
        }
        record!("inspector reply {id:?}: {result:?}");
        Ok(())
    }
}

impl<P: InspectionOps> Controller<P> {
    /// Whether the managed location `number` of `runtime` is set, or why
    /// it waits, when the runtime's inspector arms it.
    pub(in crate::backend::linux) fn inspector_readiness(
        &self,
        runtime: RuntimeId,
        number: u64,
    ) -> Option<std::result::Result<(), Arc<str>>> {
        let inspector = self.inferior.as_ref()?.inspectors.records.get(&runtime)?;
        Some(inspector.readiness(number))
    }
}

/// Where an isolate's breakpoints are set: its session over a connection,
/// with the source maps of the process's scripts.
struct Target<'a> {
    connection: &'a Connection,
    session: &'a Session,
    maps: &'a BTreeMap<Arc<str>, maps::ScriptMap>,
}

impl Target<'_> {
    fn send(&self, method: &str, params: serde_json::Value) -> Result<RequestId, Arc<str>> {
        send(self.connection, self.session, method, params)
            .map_err(|error| error.to_string().into())
    }
}

/// Brings an isolate to `desired` and `pausing`: sets the locations it
/// lacks, removes those no longer wanted, holds a sentinel while any is
/// a line, and tells it which exceptions pause it when that changed.
fn sync_isolate(
    target: &Target<'_>,
    isolate: &mut Isolate,
    desired: &BTreeMap<u64, &ManagedArm>,
    pausing: &'static str,
) {
    if isolate.told != Some(pausing) && !pausing.is_empty() {
        let _ = target.send(
            inspector_protocol::PAUSE_ON_EXCEPTIONS,
            serde_json::json!({ "state": pausing }),
        );
        isolate.told = Some(pausing);
    }
    sync_sentinel(target, isolate, desired);
    let gone = isolate
        .sites
        .keys()
        .filter(|number| !desired.contains_key(number))
        .copied()
        .collect::<Vec<_>>();
    for number in gone {
        for placement in isolate.sites.remove(&number).unwrap_or_default().placements {
            if let Some(id) = placement.id {
                remove(target.connection, target.session, &id);
            } else if let SiteState::Requested(request) | SiteState::Withdrawn(request) =
                placement.state
            {
                isolate.withdrawn.insert(request);
            }
        }
    }

    let mut searching = false;
    for (&number, arm) in desired {
        if isolate.sites.contains_key(&number) {
            continue;
        }
        let mut site = Site::default();
        match &arm.place {
            ManagedPlace::Line { path, line } => {
                let mapped = isolate
                    .scripts
                    .iter()
                    .filter_map(|(id, url)| {
                        let path = inspector_protocol::script_path(url);
                        let (path, map) = target.maps.get_key_value(&*path)?;
                        map.map().ok()?;
                        Some((Arc::clone(id), Arc::clone(path)))
                    })
                    .collect::<Vec<_>>();
                // A path that is a mapped script's own names the source
                // compiled into it, at lines only the map knows.
                if !mapped
                    .iter()
                    .any(|(_, script)| maps::names(Path::new(&**script), Path::new(&**path)))
                {
                    let state = match target.send(
                        inspector_protocol::SET_BREAKPOINT,
                        serde_json::json!({
                            "urlRegex": protocol::url_pattern(path),
                            "lineNumber": line.saturating_sub(1),
                        }),
                    ) {
                        Ok(request) => SiteState::Requested(request),
                        Err(reason) => SiteState::Refused(reason),
                    };
                    site.placements.push(Placement {
                        id: None,
                        state,
                        mapped: None,
                        actual: None,
                        function: None,
                    });
                }
                for script in &mapped {
                    place_mapped(target, &mut site, script, Path::new(&**path), *line);
                }
            }
            ManagedPlace::Function { .. } => searching = true,
        }
        isolate.sites.insert(number, site);
    }
    // A function breakpoint is placed in each function the search finds.
    if searching {
        functions::search(target.connection, target.session, isolate);
    }
}

/// Holds the isolate's sentinel while `desired` has a line or a function,
/// with the pattern they call for, and removes it once none is. A
/// function may be in any script, `node_modules` too.
fn sync_sentinel(target: &Target<'_>, isolate: &mut Isolate, desired: &BTreeMap<u64, &ManagedArm>) {
    let functions = desired
        .values()
        .any(|arm| matches!(arm.place, ManagedPlace::Function { .. }));
    let mut lines = desired
        .values()
        .filter_map(|arm| match &arm.place {
            ManagedPlace::Line { path, .. } => Some(Path::new(&**path)),
            ManagedPlace::Function { .. } => None,
        })
        .peekable();
    let pattern = if functions {
        Some(maps::EVERY_SCRIPT.to_owned())
    } else {
        lines
            .peek()
            .is_some()
            .then(|| maps::sentinel_pattern(lines))
    };
    if isolate.sentinel.as_ref().map(|sentinel| &sentinel.pattern) == pattern.as_ref() {
        return;
    }
    if let Some(sentinel) = isolate.sentinel.take() {
        if let Some(id) = sentinel.placement.id {
            remove(target.connection, target.session, &id);
        } else if let SiteState::Requested(request) = sentinel.placement.state {
            isolate.withdrawn.insert(request);
        }
    }
    let Some(pattern) = pattern else {
        return;
    };
    let state = match target.send(
        inspector_protocol::SET_BREAKPOINT,
        serde_json::json!({ "urlRegex": pattern, "lineNumber": 0, "columnNumber": 0 }),
    ) {
        Ok(request) => SiteState::Requested(request),
        Err(reason) => SiteState::Refused(reason),
    };
    isolate.sentinel = Some(Sentinel {
        pattern,
        placement: Placement {
            id: None,
            state,
            mapped: None,
            actual: None,
            function: None,
        },
    });
}

/// Sets a breakpoint on `line` of the source at `path` at each place its
/// map leads that line to in `script`, its id and path, that `site` does
/// not already hold.
fn place_mapped(
    target: &Target<'_>,
    site: &mut Site,
    (script, path_of_script): &(Arc<str>, Arc<str>),
    path: &Path,
    line: u32,
) {
    let Some(map) = target.maps.get(path_of_script) else {
        return;
    };
    for position in map.positions(path, line) {
        let at = Some((Arc::clone(script), position));
        if site
            .placements
            .iter()
            .any(|placement| placement.mapped == at)
        {
            continue;
        }
        let state = match target.send(
            inspector_protocol::SET_BREAKPOINT_AT,
            serde_json::json!({ "location": {
                "scriptId": &**script,
                "lineNumber": position.line.zero_based(),
                "columnNumber": position.column.zero_based(),
            }}),
        ) {
            Ok(request) => SiteState::Requested(request),
            Err(reason) => SiteState::Refused(reason),
        };
        site.placements.push(Placement {
            id: None,
            state,
            mapped: at,
            actual: None,
            function: None,
        });
    }
    record!("the source map of {path_of_script}: {:?}", map.map().err());
}

/// Withdraws the breakpoint `site` holds by its path.
fn withdraw_by_path(target: &Target<'_>, site: &mut Site) {
    site.placements.retain_mut(|placement| {
        if placement.mapped.is_some() {
            return true;
        }
        match &placement.state {
            SiteState::Requested(request) => {
                placement.state = SiteState::Withdrawn(*request);
                true
            }
            SiteState::Withdrawn(_) => true,
            _ => {
                if let Some(id) = &placement.id {
                    remove(target.connection, target.session, id);
                }
                false
            }
        }
    });
}

/// Notes V8's answer to setting `placement`.
fn answer(
    placement: &mut Placement,
    result: std::result::Result<serde_json::Value, super::super::cdp::CdpError>,
) {
    match result {
        Ok(set) => {
            placement.id = set["breakpointId"].as_str().map(Arc::from);
            let actual = &set["actualLocation"];
            placement.actual = actual["lineNumber"]
                .as_u64()
                .zip(actual["columnNumber"].as_u64());
            // `setBreakpoint` answers its one location, and
            // `setBreakpointByUrl` all it found.
            let placed = set["locations"]
                .as_array()
                .is_some_and(|locations| !locations.is_empty())
                || set.get("actualLocation").is_some();
            placement.state = if placed {
                SiteState::Resolved
            } else {
                SiteState::Unresolved
            };
        }
        Err(error) => placement.state = SiteState::Refused(error.to_string().into()),
    }
}

impl Isolate {
    /// The sentinel's pause it holds, once nothing it waits for remains:
    /// a search of its functions, or the breakpoints set in its script.
    pub(super) fn ready_deferred(&mut self) -> Option<serde_json::Value> {
        let waits = self.search.is_some()
            || self.deferred.as_ref().is_some_and(|pause| {
                awaits_mapped(self.sites.values().flat_map(|site| &site.placements), pause)
            });
        if waits { None } else { self.deferred.take() }
    }
}

/// Whether a mapped breakpoint among `placements` in the script of the
/// pause `pause` that V8 reported is not yet answered.
pub(super) fn awaits_mapped<'a>(
    mut placements: impl Iterator<Item = &'a Placement>,
    pause: &serde_json::Value,
) -> bool {
    let script = pause["callFrames"][0]["location"]["scriptId"].as_str();
    placements.any(|placement| {
        matches!(placement.state, SiteState::Requested(_))
            && placement
                .mapped
                .as_ref()
                .is_some_and(|(id, _)| Some(&**id) == script)
    })
}

fn remove(connection: &Connection, session: &Session, id: &str) {
    let _ = send(
        connection,
        session,
        inspector_protocol::REMOVE_BREAKPOINT,
        serde_json::json!({ "breakpointId": id }),
    );
}
