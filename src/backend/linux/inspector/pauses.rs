use std::collections::{BTreeMap, BTreeSet};
use std::sync::Arc;

use nix::unistd::Pid;

use super::super::Controller;
use super::super::cdp::{Connection, RequestId, Session};
use super::super::debug_thread_id;
use super::super::native::{InspectionOps, LinuxTraceOps};
use super::steps::{Stepping, pause_for_step};
use super::{Inspector, Isolate, PauseRequest, protocol, send};
use crate::runtime_model::{ReportedLocal, ReportedValue, inspector_protocol};
use crate::{Error, Result, RuntimeId, StopReason, VirtualAddress};

#[derive(Debug)]
pub(super) struct Paused {
    hits: Vec<Arc<str>>,

    placed_here: Vec<u64>,

    pub(super) thread: Option<Pid>,

    judged: bool,

    reason: Option<StopReason>,

    published: bool,

    pub(super) frames: Vec<PausedFrame>,

    pub(super) bound: BTreeMap<VirtualAddress, usize>,

    pub(super) listing: BTreeMap<RequestId, (usize, usize)>,

    exception: Option<Thrown>,

    answers: Answers,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Answers {
    Interrupt,

    Statement,

    Nothing,
}

impl Paused {
    pub(super) fn interrupt_answered(&mut self) {
        if !self.listing.is_empty() && self.answers == Answers::Interrupt {
            self.answers = Answers::Nothing;
        }
    }

    fn ready(&self) -> bool {
        !self.judged && self.listing.is_empty()
    }

    pub(super) fn depth_holding(&self, thread: crate::ThreadId) -> Option<usize> {
        (self.judged && self.thread.map(debug_thread_id) == Some(thread))
            .then_some(self.frames.len())
    }
}

#[derive(Debug)]
struct Thrown {
    uncaught: bool,

    description: Arc<str>,
}

impl Thrown {
    fn of(params: &serde_json::Value) -> Option<Self> {
        if !matches!(
            params["reason"].as_str(),
            Some("exception" | "promiseRejection")
        ) {
            return None;
        }
        let data = &params["data"];
        let description = data["description"]
            .as_str()
            .map(Arc::from)
            .or_else(|| data.get("value").map(|value| value.to_string().into()))
            .unwrap_or_else(|| "an exception V8 did not describe".into());
        Some(Self {
            uncaught: data["uncaught"].as_bool().unwrap_or(false),
            description,
        })
    }
}

#[derive(Debug)]
pub(super) struct PausedFrame {
    pub(super) reported: serde_json::Value,

    path: Option<Arc<str>>,

    line: u64,
    column: u64,

    scopes: Vec<Option<std::result::Result<Vec<ReportedLocal>, Arc<str>>>>,

    pub(super) given: Vec<(usize, Arc<str>, serde_json::Value, ReportedValue)>,
}

impl PausedFrame {
    pub(super) fn assigned(
        &mut self,
        scope: usize,
        name: &str,
        argument: &serde_json::Value,
        value: &ReportedValue,
    ) {
        self.given
            .retain(|(at, given, ..)| (*at, &**given) != (scope, name));
        self.given
            .push((scope, name.into(), argument.clone(), value.clone()));
        let chain = self
            .reported
            .get("scopeChain")
            .and_then(serde_json::Value::as_array)
            .map_or(&[][..], Vec::as_slice);
        let Some(position) = listed_scopes(chain)
            .iter()
            .position(|&index| index == scope)
        else {
            return;
        };
        if let Some(Some(Ok(listed))) = self.scopes.get_mut(position) {
            for local in listed.iter_mut().filter(|local| &*local.name == name) {
                local.value = value.clone();
            }
        }
    }

    fn locals(&self) -> crate::runtime_model::ReportedLocals {
        let chain = self
            .reported
            .get("scopeChain")
            .and_then(serde_json::Value::as_array)
            .map_or(&[][..], Vec::as_slice);
        let mut locals = Vec::new();
        for (scope, index) in self.scopes.iter().zip(listed_scopes(chain)) {
            let block = chain
                .get(index)
                .is_some_and(|listed| listed["type"] == "block");
            match scope {
                Some(Ok(scope)) => locals.extend(
                    scope
                        .iter()
                        .cloned()
                        .map(|local| ReportedLocal { block, ..local }),
                ),
                Some(Err(reason)) => return Err(Arc::clone(reason)),
                None => return Err("V8 did not list the frame's variables before the stop".into()),
            }
        }
        Ok(locals.into())
    }
}

struct ScriptFrame {
    path: Arc<str>,
    line: u64,
    column: u64,
    frame: Option<VirtualAddress>,
}

pub(super) type PauseKey = (RuntimeId, Session);

impl<P: LinuxTraceOps> Controller<P> {
    pub(super) fn inspector_paused(
        &mut self,
        runtime: RuntimeId,
        session: &Session,
        params: &serde_json::Value,
    ) -> Result<()> {
        let mut hits: Vec<Arc<str>> = params["hitBreakpoints"]
            .as_array()
            .into_iter()
            .flatten()
            .filter_map(|hit| hit.as_str().map(Arc::from))
            .collect();
        record!("V8 paused in {session:?}: {} {hits:?}", params["reason"]);
        let Some((at_sentinel, placed_here)) =
            self.sentinel_pause(runtime, session, &mut hits, params)
        else {
            return Ok(());
        };
        if params["reason"] == inspector_protocol::BREAK_ON_START && hits.is_empty() {
            self.let_v8_go(runtime, session);
            return Ok(());
        }
        let asked = self
            .isolate_mut(runtime, session)
            .is_some_and(|isolate| std::mem::take(&mut isolate.pause) == PauseRequest::Sent);
        let statement = !asked
            && !at_sentinel
            && hits.is_empty()
            && placed_here.is_empty()
            && params["reason"] == "other";
        if hits.is_empty()
            && placed_here.is_empty()
            && self.step_goes_on(runtime, session, params, statement)
        {
            return Ok(());
        }
        let interrupted = self
            .session
            .inferior()
            .is_some_and(|inferior| inferior.inspectors.interrupt.is_some());
        let Some(inspector) = self
            .session
            .inferior_mut()
            .and_then(|inferior| inferior.inspectors.records.get_mut(&runtime))
        else {
            return Ok(());
        };
        let Some((link, isolate)) = inspector.status.connected_isolate(session) else {
            return Ok(());
        };
        let mut listing = BTreeMap::new();
        let frames = params["callFrames"]
            .as_array()
            .into_iter()
            .flatten()
            .enumerate()
            .map(|(index, frame)| {
                list_frame(
                    &link.connection,
                    session,
                    &isolate.scripts,
                    frame,
                    index,
                    &mut listing,
                )
            })
            .collect();
        let listed = listing.is_empty();
        isolate.paused = Some(Paused {
            hits,
            placed_here,
            thread: None,
            judged: false,
            reason: None,
            published: false,
            frames,
            bound: BTreeMap::new(),
            listing,
            exception: Thrown::of(params),
            answers: if interrupted {
                Answers::Interrupt
            } else if statement {
                Answers::Statement
            } else {
                Answers::Nothing
            },
        });
        if listed {
            self.judge_inspector_pause()?;
        }
        Ok(())
    }

    fn sentinel_pause(
        &mut self,
        runtime: RuntimeId,
        session: &Session,
        hits: &mut Vec<Arc<str>>,
        params: &serde_json::Value,
    ) -> Option<(bool, Vec<u64>)> {
        let isolate = self.isolate_mut(runtime, session)?;
        let location = params
            .pointer("/callFrames/0/location")
            .unwrap_or(&serde_json::Value::Null);
        let sentinel = isolate
            .sentinel
            .as_ref()
            .and_then(|sentinel| sentinel.placement.id.clone());
        let at_sentinel = sentinel.as_ref().is_some_and(|id| hits.contains(id));
        if !at_sentinel {
            return Some((false, Vec::new()));
        }
        hits.retain(|hit| Some(hit) != sentinel.as_ref());
        if let Some(script) = location["scriptId"].as_str()
            && isolate.searched.insert(script.into())
        {
            self.search_functions(runtime, session);
        }
        let isolate = self.isolate_mut(runtime, session)?;
        let placements = isolate.sites.values().flat_map(|site| &site.placements);
        if isolate.search.is_some() || super::arming::awaits_mapped(placements, params) {
            record!("the sentinel's pause waits for V8 to set its script's breakpoints");
            isolate.deferred = Some(params.clone());
            return None;
        }
        let placed_here = placed_at(isolate, location);
        if hits.is_empty()
            && placed_here.is_empty()
            && isolate.pause != PauseRequest::Sent
            && !self.steps_in(runtime, session)
        {
            self.let_v8_go(runtime, session);
            return None;
        }
        Some((true, placed_here))
    }

    fn let_v8_go(&self, runtime: RuntimeId, session: &Session) {
        if let Some(link) = self
            .session
            .inferior()
            .and_then(|inferior| inferior.inspectors.records.get(&runtime))
            .and_then(|inspector| inspector.status.link())
        {
            let _ = send(
                &link.connection,
                session,
                inspector_protocol::RESUME,
                serde_json::json!({}),
            );
        }
    }

    pub(super) fn scope_listed(
        &mut self,
        runtime: RuntimeId,
        id: RequestId,
        result: std::result::Result<serde_json::Value, super::super::cdp::CdpError>,
    ) -> Result<()> {
        let Some(paused) = self
            .session
            .inferior_mut()
            .and_then(|inferior| inferior.inspectors.records.get_mut(&runtime))
            .and_then(|inspector| {
                let (_, isolates) = inspector.status.connected_mut()?;
                isolates
                    .values_mut()
                    .filter_map(|isolate| isolate.paused.as_mut())
                    .find(|paused| paused.listing.contains_key(&id))
            })
        else {
            return Ok(());
        };
        let Some((frame, scope)) = paused.listing.remove(&id) else {
            return Ok(());
        };
        let listed = result
            .map_err(|error| Arc::from(error.to_string()))
            .and_then(|properties| protocol::scope_locals(&properties));
        if let Some(slot) = paused
            .frames
            .get_mut(frame)
            .and_then(|frame| frame.scopes.get_mut(scope))
        {
            *slot = Some(listed);
        }
        if paused.listing.is_empty() {
            self.judge_inspector_pause()?;
        }
        Ok(())
    }

    fn judge_inspector_pause(&mut self) -> Result<()> {
        let inferior = self.session.inferior().ok_or(Error::NotRunning)?;
        if inferior.run.stop().is_none() && inferior.run.execution().is_some() {
            self.queue_internal_edit(super::super::Edit::InspectorPaused)?;
            self.finish_barrier_if_ready()?;
        }
        Ok(())
    }

    #[cfg_attr(
        not(debug_assertions),
        expect(unused_variables, reason = "only recorded")
    )]
    pub(in crate::backend::linux) fn apply_inspector_pauses(&mut self) {
        let Some(inferior) = self.session.inferior() else {
            return;
        };
        let mut ready = Vec::new();
        for (&runtime, inspector) in &inferior.inspectors.records {
            for (session, isolate) in inspector.status.isolates() {
                if isolate.paused.as_ref().is_some_and(Paused::ready) {
                    ready.push((runtime, session.clone()));
                }
            }
        }
        ready.sort_by_key(|(_, session)| session.path().len());
        if !ready.is_empty()
            && let Err(error) = self.refresh_libraries()
        {
            record!("modules are not refreshed for V8's pauses: {error}");
        }
        for key in ready {
            self.judge_pause(&key);
        }
    }

    #[cfg_attr(
        not(debug_assertions),
        expect(unused_variables, reason = "only recorded")
    )]
    fn judge_pause(&mut self, key: &PauseKey) {
        let thread = self.paused_thread(key);
        let fallback = self.session.inferior().map(|inferior| inferior.tgid);
        let Some(paused) = self.paused_mut(key) else {
            return;
        };
        paused.judged = true;
        paused.thread = thread.as_ref().ok().copied();
        let decided = paused.reason.clone();
        let (pid, reason) = match thread {
            Ok(pid) => {
                if let Err(error) = self.bind_paused_frames(key, pid) {
                    record!("the pause's frames are not bound: {error}");
                }
                (pid, decided.or_else(|| self.pause_reason(key, pid)))
            }
            Err(description) => {
                let Some(pid) = fallback else { return };
                (pid, Some(StopReason::RuntimeEvent { description }))
            }
        };
        let Some(paused) = self.paused_mut(key) else {
            return;
        };
        paused.reason.clone_from(&reason);
        let interrupted = paused.answers == Answers::Interrupt;
        let Some(reason) = reason else {
            record!("V8's pause stops for none of microscope's breakpoints or steps");
            return;
        };
        let Some(inferior) = self.session.inferior_mut() else {
            return;
        };
        if let Some(thread) = inferior.threads.get_mut(&pid)
            && thread.reason.as_ref().is_none_or(|current| {
                super::super::classify::visible_stop_priority(&reason)
                    > super::super::classify::visible_stop_priority(current)
            })
        {
            thread.reason = Some(reason.clone());
        }
        if let Some(barrier) = inferior.run.barrier_mut() {
            barrier.offer(Some(pid), reason);
            if interrupted {
                barrier.pause();
            }
        }
    }

    fn pause_reason(&mut self, key: &PauseKey, pid: Pid) -> Option<StopReason> {
        let (runtime, session) = key;
        let inferior = self.session.inferior()?;
        let isolate = inferior
            .inspectors
            .records
            .get(runtime)?
            .status
            .isolates()
            .get(session)?;
        let paused = isolate.paused.as_ref()?;
        let numbers = paused
            .hits
            .iter()
            .filter_map(|hit| {
                isolate
                    .sites
                    .iter()
                    .find(|(_, site)| {
                        site.placements
                            .iter()
                            .any(|placement| placement.id.as_ref() == Some(hit))
                    })
                    .map(|(&number, _)| number)
            })
            .chain(paused.placed_here.iter().copied())
            .collect::<BTreeSet<_>>();
        let owners = numbers
            .iter()
            .filter_map(|&number| inferior.managed.site(*runtime, number))
            .flat_map(|(_, owners)| owners.iter().copied())
            .collect::<BTreeSet<_>>();
        let depth = paused.frames.len();
        let answers = paused.answers;
        let exception = paused.exception.as_ref().and_then(|thrown| {
            let filter =
                inspector_protocol::exception_filter(self.exception_stops, thrown.uncaught)?;
            Some(StopReason::LanguageException(crate::LanguageException {
                kind: filter.kind,
                message: Arc::clone(&thrown.description),
                value: None,
            }))
        });
        let hits = if owners.is_empty() {
            Arc::from([])
        } else {
            self.judge_hits(pid, &owners, None)
        };
        if !hits.is_empty() {
            return Some(StopReason::Breakpoint {
                address: None,
                hits,
            });
        }
        if exception.is_some() {
            return exception;
        }
        let answered = match answers {
            Answers::Statement => Some(StopReason::ProgramBreakpoint { address: None }),
            Answers::Interrupt => Some(StopReason::Pause),
            Answers::Nothing => None,
        };
        self.inspector_step_end(key, pid, depth).or(answered)
    }

    fn paused_thread(&self, key: &PauseKey) -> std::result::Result<Pid, Arc<str>> {
        let inferior = self.session.inferior().ok_or("the process is gone")?;
        if key.1.path().is_empty() {
            return Ok(inferior.tgid);
        }
        let paused = self.paused(key).ok_or("V8 no longer holds this pause")?;
        let held = inferior
            .inspectors
            .records
            .values()
            .flat_map(|inspector| inspector.status.isolates().values())
            .filter_map(|isolate| isolate.paused.as_ref()?.thread)
            .collect::<BTreeSet<_>>();
        let matching = inferior
            .threads
            .keys()
            .filter(|&&pid| pid != inferior.tgid && !held.contains(&pid))
            .filter(|&&pid| {
                self.script_frames(pid).is_ok_and(|frames| {
                    paused
                        .frames
                        .first()
                        .is_some_and(|frame| frame.path.is_some())
                        && frames.len() >= paused.frames.len()
                        && frames.iter().zip(&paused.frames).all(|(frame, listed)| {
                            listed.path.as_ref().is_none_or(|path| path == &frame.path)
                                && Some(frame.line) == listed.line.checked_add(1)
                                && Some(frame.column) == listed.column.checked_add(1)
                        })
                })
            })
            .copied()
            .collect::<Vec<_>>();
        match matching.as_slice() {
            [pid] => Ok(*pid),
            [] => Err("no native thread can be proved to hold V8's worker pause; its locals and breakpoint hits are not attributed".into()),
            _ => Err(format!("native threads {matching:?} have identical script stacks at V8's worker pause; its locals and breakpoint hits are not attributed").into()),
        }
    }

    fn script_frames(&self, pid: Pid) -> Result<Vec<ScriptFrame>> {
        let inferior = self.session.inferior().ok_or(Error::NotRunning)?;
        let presentation = self.presentation_for_thread(pid, None)?;
        let root = super::super::frames::StackRoot::of_thread(pid);
        let stack = self.physical_stack(inferior, &root, crate::unwind::DEFAULT_MAX_FRAMES)?;
        let modules = self.unwind_modules(inferior);
        let expanded = self.expand_backtrace(
            inferior,
            &root,
            &stack,
            crate::unwind::DEFAULT_MAX_FRAMES,
            &modules,
            Some(&presentation),
        )?;
        let mut frames = Vec::new();
        for frame in expanded
            .trace
            .frames
            .iter()
            .filter(|frame| protocol::is_script_frame(frame))
        {
            let Some(place) = frame
                .recorded
                .as_ref()
                .and_then(|recorded| recorded.generated.as_ref().or(recorded.place.as_ref()))
            else {
                continue;
            };
            let resolved =
                self.resolved_in(inferior, &root, &stack, &modules, &expanded, frame.id)?;
            frames.push(ScriptFrame {
                path: Arc::clone(&place.path),
                line: u64::from(place.line),
                column: u64::from(place.column),
                frame: match resolved.scope {
                    super::super::frames::FrameScope::Interpreted { frame, .. } => {
                        Some(frame.address)
                    }
                    _ => None,
                },
            });
        }
        Ok(frames)
    }

    pub(super) fn bind_paused_frames(&mut self, key: &PauseKey, pid: Pid) -> Result<()> {
        let frames = self.script_frames(pid)?;
        let Some(paused) = self.paused(key) else {
            return Ok(());
        };
        let mut bound = BTreeMap::new();
        let mut indices = BTreeMap::new();
        for (index, (frame, listed)) in frames.iter().zip(&paused.frames).enumerate() {
            if Some(frame.line) != listed.line.checked_add(1)
                || Some(frame.column) != listed.column.checked_add(1)
                || listed.path.as_ref().is_some_and(|path| path != &frame.path)
            {
                record!(
                    "V8's frames part from the stack's at {}:{}",
                    frame.line,
                    frame.column
                );
                break;
            }
            if let Some(address) = frame.frame {
                bound.insert(address, listed.locals());
                indices.insert(address, index);
            }
        }
        if let Some(paused) = self.paused_mut(key) {
            paused.bound = indices;
        }
        record!("V8 named the locals of {} frames", bound.len());
        if let Some(inferior) = self.session.inferior_mut() {
            inferior.inspectors.locals.extend(bound);
        }
        Ok(())
    }

    pub(in crate::backend::linux) fn mark_published_pause(
        &mut self,
        pid: Pid,
        reason: &StopReason,
    ) {
        if let Some(inferior) = self.session.inferior_mut() {
            for isolate in inferior.inspectors.isolates_mut() {
                if let Some(paused) = isolate.paused.as_mut().filter(|paused| {
                    paused.thread == Some(pid)
                        || (paused.thread.is_none()
                            && matches!(reason, StopReason::RuntimeEvent { .. })
                            && paused.reason.as_ref() == Some(reason))
                }) {
                    paused.published = true;
                }
            }
        }
    }

    pub(in crate::backend::linux) fn resume_inspectors(
        &mut self,
        resumed: &BTreeSet<Pid>,
    ) -> Result<()> {
        let Some(inferior) = self.session.inferior_mut() else {
            return Ok(());
        };
        let main = inferior.tgid;
        let mut held = false;
        let mut ended = false;
        for inspector in inferior.inspectors.records.values_mut() {
            let Inspector {
                status,
                step,
                gate,
                desired,
                assigning,
                through,
                ..
            } = inspector;
            let Some((link, isolates)) = status.connected_mut() else {
                continue;
            };
            for (session, isolate) in isolates.iter_mut() {
                if let Some(assignment) = inspector_assigning(assigning, session, isolate, resumed)
                {
                    super::assignments::begin(&link.connection, session, isolate, assignment);
                    ended |= assignment.ended();
                    continue;
                }
                held |=
                    resume_isolate(&link.connection, session, isolate, (step, through), resumed);
            }
            if resumed.contains(&main)
                && let Some(isolate) = isolates.get_mut(&Session::main())
            {
                pause_for_step(&link.connection, isolate, step.as_mut(), gate.as_mut());
            }
            if super::functions::wanted(desired) {
                for (session, isolate) in isolates.iter_mut() {
                    super::functions::search(&link.connection, session, isolate);
                }
            }
        }
        let judged = inferior.inspectors.records.values().any(|inspector| {
            inspector
                .status
                .isolates()
                .values()
                .any(|isolate| isolate.paused.as_ref().is_some_and(|paused| paused.judged))
        });
        if !judged {
            inferior.inspectors.locals.clear();
        }
        if held {
            self.queue_internal_edit(super::super::Edit::InspectorPaused)?;
        }
        if ended {
            self.queue_internal_edit(super::super::Edit::InspectorAssigned)?;
        }
        Ok(())
    }
}

impl<P: InspectionOps> Controller<P> {
    pub(super) fn paused(&self, (runtime, session): &PauseKey) -> Option<&Paused> {
        self.session
            .inferior()?
            .inspectors
            .records
            .get(runtime)?
            .status
            .isolates()
            .get(session)?
            .paused
            .as_ref()
    }

    fn paused_mut(&mut self, (runtime, session): &PauseKey) -> Option<&mut Paused> {
        self.isolate_mut(*runtime, session)?.paused.as_mut()
    }

    fn isolate_mut(&mut self, runtime: RuntimeId, session: &Session) -> Option<&mut Isolate> {
        self.session
            .inferior_mut()?
            .inspectors
            .records
            .get_mut(&runtime)?
            .status
            .connected_isolate(session)
            .map(|(_, isolate)| isolate)
    }

    pub(in crate::backend::linux) fn inspector_holds(&self, thread: crate::ThreadId) -> bool {
        self.session.inferior().is_some_and(|inferior| {
            inferior
                .inspectors
                .records
                .values()
                .flat_map(|inspector| inspector.status.isolates().values())
                .filter_map(|isolate| isolate.paused.as_ref()?.thread)
                .any(|pid| debug_thread_id(pid) == thread)
        })
    }
}

fn placed_at(isolate: &Isolate, location: &serde_json::Value) -> Vec<u64> {
    let (Some(script), Some(line), Some(column)) = (
        location["scriptId"].as_str(),
        location["lineNumber"].as_u64(),
        location["columnNumber"].as_u64(),
    ) else {
        return Vec::new();
    };
    isolate
        .sites
        .iter()
        .filter(|(_, site)| {
            site.placements.iter().any(|placement| {
                placement.mapped.as_ref().is_some_and(|(id, at)| {
                    let at = placement.actual.unwrap_or_else(|| {
                        (
                            u64::from(at.line.zero_based()),
                            u64::from(at.column.zero_based()),
                        )
                    });
                    **id == *script && at == (line, column)
                })
            })
        })
        .map(|(&number, _)| number)
        .collect()
}

fn inspector_assigning<'a>(
    assigning: &'a mut Option<super::assignments::Assigning>,
    session: &Session,
    isolate: &Isolate,
    resumed: &BTreeSet<Pid>,
) -> Option<&'a mut super::assignments::Assigning> {
    assigning.as_mut().filter(|assignment| {
        assignment.unbegun()
            && assignment.session == *session
            && isolate
                .paused
                .as_ref()
                .and_then(|paused| paused.thread)
                .is_some_and(|thread| resumed.contains(&thread))
    })
}

fn resume_isolate(
    connection: &Connection,
    session: &Session,
    isolate: &mut Isolate,
    (step, through): (&mut Option<Stepping>, &mut Option<super::addons::Through>),
    resumed: &BTreeSet<Pid>,
) -> bool {
    let Some(paused) = isolate.paused.as_mut() else {
        return false;
    };
    if !paused.judged {
        return paused.listing.is_empty();
    }
    if paused
        .thread
        .is_some_and(|thread| !resumed.contains(&thread))
    {
        return false;
    }
    if paused.reason.is_some() && !paused.published {
        record!("the pause in {session:?} stays held for its own stop");
        paused.judged = false;
        paused.thread = None;
        return true;
    }
    let command = match step.as_mut().filter(|step| step.session == *session) {
        Some(step) => {
            step.sent = true;
            step.command
        }
        None => continue_through(through.as_ref(), session, paused),
    };
    if command != inspector_protocol::STEP_OUT
        && through
            .as_ref()
            .is_some_and(|through| through.session == *session)
    {
        *through = None;
    }
    isolate.resuming |= send(connection, session, command, serde_json::json!({})).is_ok();
    isolate.paused = None;
    false
}

fn continue_through(
    through: Option<&super::addons::Through>,
    session: &Session,
    paused: &Paused,
) -> &'static str {
    match through {
        Some(through)
            if through.session == *session
                && paused.thread == Some(through.thread)
                && paused.reason.is_none()
                && paused.frames.len() > through.depth =>
        {
            inspector_protocol::STEP_OUT
        }
        _ => inspector_protocol::RESUME,
    }
}

fn listed_scopes(chain: &[serde_json::Value]) -> Vec<usize> {
    let local = chain
        .iter()
        .position(|scope| scope["type"] == "local")
        .unwrap_or(chain.len());
    let blocks = chain
        .iter()
        .enumerate()
        .take(local)
        .rev()
        .filter(|(_, scope)| scope["type"] == "block")
        .map(|(index, _)| index);
    (local..chain.len().min(local + 1)).chain(blocks).collect()
}

fn list_frame(
    connection: &Connection,
    session: &Session,
    scripts: &BTreeMap<Arc<str>, Arc<str>>,
    frame: &serde_json::Value,
    index: usize,
    listing: &mut BTreeMap<RequestId, (usize, usize)>,
) -> PausedFrame {
    let chain = frame["scopeChain"]
        .as_array()
        .map_or(&[][..], Vec::as_slice);
    let mut scopes = Vec::new();
    for scope in listed_scopes(chain)
        .into_iter()
        .filter_map(|index| chain.get(index))
    {
        let sent = scope
            .pointer("/object/objectId")
            .and_then(serde_json::Value::as_str)
            .ok_or_else(|| Arc::<str>::from("V8 gave the scope no object"))
            .and_then(|object| {
                connection
                    .send(
                        session,
                        inspector_protocol::GET_PROPERTIES,
                        serde_json::json!({
                            "objectId": object,
                            "ownProperties": true,
                            "accessorPropertiesOnly": false,
                            "generatePreview": false,
                        }),
                    )
                    .map_err(|error| Arc::from(error.to_string()))
            });
        match sent {
            Ok(request) => {
                listing.insert(request, (index, scopes.len()));
                scopes.push(None);
            }
            Err(reason) => scopes.push(Some(Err(reason))),
        }
    }
    PausedFrame {
        reported: frame.clone(),
        path: frame
            .pointer("/location/scriptId")
            .and_then(serde_json::Value::as_str)
            .and_then(|id| scripts.get(id))
            .map(|url| Arc::from(inspector_protocol::script_path(url).as_ref())),
        line: frame
            .pointer("/location/lineNumber")
            .and_then(serde_json::Value::as_u64)
            .unwrap_or(u64::MAX),
        column: frame
            .pointer("/location/columnNumber")
            .and_then(serde_json::Value::as_u64)
            .unwrap_or(u64::MAX),
        scopes,
        given: Vec::new(),
    }
}
