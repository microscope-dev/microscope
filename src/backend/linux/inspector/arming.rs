//! What each isolate is armed with: the runtime's managed locations, as V8
//! breakpoints set by URL, and which exceptions pause it. A location is
//! ready once any isolate placed it, since a script loads only in the
//! isolates that run it.

use std::collections::BTreeMap;
use std::sync::Arc;

use super::super::Controller;
use super::super::cdp::{Connection, RequestId, Session};
use super::super::native::{InspectionOps, LinuxTraceOps};
use super::{Inspector, Isolate, Site, SiteState, protocol, send};
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
                .map(|site| &site.state)
        };
        if states().any(|state| matches!(state, SiteState::Resolved)) {
            return Ok(());
        }
        if states().any(|state| matches!(state, SiteState::Requested(_))) {
            return Err("waiting for Node's inspector to set the breakpoint".into());
        }
        if states().any(|state| matches!(state, SiteState::Unresolved)) {
            return Err("no script by that name has loaded yet".into());
        }
        if let Some(SiteState::Refused(reason)) = states().next() {
            return Err(Arc::clone(reason));
        }
        if self.target.is_none() {
            Err("Node's inspector has not started yet".into())
        } else {
            Err("Node's inspector is not connected".into())
        }
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
            .as_mut()
            .ok_or_else(|| Arc::<str>::from("the process is gone"))?;
        let main = super::super::debug_thread_id(inferior.tgid);
        let inspector = inferior.inspectors.records.entry(runtime).or_default();
        inspector.step = match (arming.step, inspector.step.take()) {
            (None, _) => None,
            (Some(step), Some(stepping)) if stepping.number == step.number => Some(stepping),
            (Some(step), _) => Some(super::steps::stepping(&inspector.isolates, step, main)?),
        };
        inspector.desired = arming.breakpoints.to_vec();
        inspector.pausing = inspector_protocol::pause_state(arming.exceptions);
        self.sync_inspector(runtime);
        Ok(())
    }

    /// Brings what each isolate of `runtime` holds to what the runtime is
    /// armed with, connecting first when it can.
    pub(super) fn sync_inspector(&mut self, runtime: RuntimeId) {
        let Some(inspector) = self
            .inferior
            .as_mut()
            .and_then(|inferior| inferior.inspectors.records.get_mut(&runtime))
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
            sync_isolate(
                &link.connection,
                session,
                isolate,
                &desired,
                inspector.pausing,
            );
        }
    }

    /// Notes what V8 answered to setting a managed location in `runtime`.
    #[cfg_attr(
        not(debug_assertions),
        expect(unused_variables, reason = "only recorded")
    )]
    pub(super) fn breakpoint_set(
        &mut self,
        runtime: RuntimeId,
        id: RequestId,
        result: std::result::Result<serde_json::Value, super::super::cdp::CdpError>,
    ) {
        let Some(inspector) = self
            .inferior
            .as_mut()
            .and_then(|inferior| inferior.inspectors.records.get_mut(&runtime))
        else {
            return;
        };
        let Some((number, site)) = inspector
            .isolates
            .values_mut()
            .flat_map(|isolate| isolate.sites.iter_mut())
            .find(|(_, site)| matches!(site.state, SiteState::Requested(request) if request == id))
        else {
            record!("inspector reply {id:?}: {result:?}");
            return;
        };
        match result {
            Ok(set) => {
                site.id = set["breakpointId"].as_str().map(Arc::from);
                let placed = set["locations"]
                    .as_array()
                    .is_some_and(|locations| !locations.is_empty());
                site.state = if placed {
                    SiteState::Resolved
                } else {
                    SiteState::Unresolved
                };
            }
            Err(error) => site.state = SiteState::Refused(error.to_string().into()),
        }
        record!("location {number} is {:?}", site.state);
        self.publish_breakpoints_changed();
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

/// Brings the isolate `session` reaches to `desired` and `pausing`: sets
/// the locations it lacks, removes those no longer wanted, and tells it
/// which exceptions pause it when that changed.
fn sync_isolate(
    connection: &Connection,
    session: &Session,
    isolate: &mut Isolate,
    desired: &BTreeMap<u64, &ManagedArm>,
    pausing: &'static str,
) {
    if isolate.told != Some(pausing) && !pausing.is_empty() {
        let _ = send(
            connection,
            session,
            inspector_protocol::PAUSE_ON_EXCEPTIONS,
            serde_json::json!({ "state": pausing }),
        );
        isolate.told = Some(pausing);
    }
    let gone = isolate
        .sites
        .keys()
        .filter(|number| !desired.contains_key(number))
        .copied()
        .collect::<Vec<_>>();
    for number in gone {
        if let Some(Site { id: Some(id), .. }) = isolate.sites.remove(&number) {
            let _ = send(
                connection,
                session,
                inspector_protocol::REMOVE_BREAKPOINT,
                serde_json::json!({ "breakpointId": id }),
            );
        }
    }

    for (&number, arm) in desired {
        if isolate.sites.contains_key(&number) {
            continue;
        }
        let state = match &arm.place {
            ManagedPlace::Line { path, line } => match send(
                connection,
                session,
                inspector_protocol::SET_BREAKPOINT,
                serde_json::json!({
                    "urlRegex": protocol::url_pattern(path),
                    "lineNumber": line.saturating_sub(1),
                }),
            ) {
                Ok(request) => SiteState::Requested(request),
                Err(error) => SiteState::Refused(error.to_string().into()),
            },
            ManagedPlace::Function { .. } => {
                SiteState::Refused("microscope cannot yet break at a JavaScript function".into())
            }
        };
        isolate.sites.insert(number, Site { id: None, state });
    }
}
