//! V8's pauses under the all-stop: each isolate's pause lists its frames'
//! variables while its thread can answer (G5), then is judged once every
//! thread is stopped. The main isolate runs on the process's main thread;
//! a worker's pause is placed on the thread whose script frames are the
//! ones V8 listed. A pause that stops for nothing microscope asked for is
//! resumed as though it never happened, and one whose stop another
//! outranked stays held, to be published at a stop of its own.
//!
//! Steps are V8's, owned by the isolate they began in; interrupts ask
//! every isolate to pause and fall back to the native all-stop.

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

/// A pause V8 reported.
#[derive(Debug)]
pub(super) struct Paused {
    /// The ids of V8's breakpoints the pause hit.
    hits: Vec<Arc<str>>,

    /// The managed locations set where a sentinel paused, before V8 could
    /// count them hit.
    placed_here: Vec<u64>,

    /// The thread V8 holds, once found at an all-stop.
    pub(super) thread: Option<Pid>,

    /// Whether the pause has been judged, at an all-stop.
    judged: bool,

    /// Why it stops, once judged; `None` for one that stops for nothing
    /// microscope asked for.
    reason: Option<StopReason>,

    /// Whether its stop was published. One that was not stays held when
    /// its thread resumes, to be published at a stop of its own.
    published: bool,

    /// The paused thread's frames, innermost first, as V8 listed them.
    pub(super) frames: Vec<PausedFrame>,

    /// Which of `frames` each interpreted frame of the thread's stack is,
    /// by the frame's address, once bound at an all-stop.
    pub(super) bound: BTreeMap<VirtualAddress, usize>,

    /// The scope listings asked for and not yet answered, by the frame and
    /// scope each lists. The pause is judged once all are answered: the
    /// paused thread answers only until every thread is stopped.
    pub(super) listing: BTreeMap<RequestId, (usize, usize)>,

    /// The exception V8 paused at, if it paused at one.
    exception: Option<Thrown>,

    /// What it answers, when it answers no breakpoint, step, or exception.
    answers: Answers,
}

/// What a pause answers besides microscope's breakpoints, steps, and
/// exceptions.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Answers {
    /// A client's interrupt.
    Interrupt,

    /// A `debugger` statement.
    Statement,

    /// Nothing: an interrupt's late pause, which is absorbed.
    Nothing,
}

impl Paused {
    /// Whether the pause waits to be judged: its variables are listed.
    fn ready(&self) -> bool {
        !self.judged && self.listing.is_empty()
    }

    /// How many frames the pause has, once judged to hold `thread`.
    pub(super) fn depth_holding(&self, thread: crate::ThreadId) -> Option<usize> {
        (self.judged && self.thread.map(debug_thread_id) == Some(thread))
            .then_some(self.frames.len())
    }
}

/// An exception a pause reported.
#[derive(Debug)]
struct Thrown {
    /// Whether V8 found nothing that catches it.
    uncaught: bool,

    /// V8's description of the value: an error's stack, or the value.
    description: Arc<str>,
}

impl Thrown {
    /// The exception a pause `params` describes, if it paused at one.
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

/// A frame of a pause, as V8 listed it.
#[derive(Debug)]
pub(super) struct PausedFrame {
    /// The frame as the pause reported it: its `callFrameId` and scope
    /// chain, which name it to V8 while the pause lasts.
    pub(super) reported: serde_json::Value,

    path: Option<Arc<str>>,

    /// Its place: zero-based line and column.
    line: u64,
    column: u64,

    /// Its local scope and then its block scopes, outermost first, which
    /// is the order V8 declares their variables in; each with its
    /// variables once listed.
    scopes: Vec<Option<std::result::Result<Vec<ReportedLocal>, Arc<str>>>>,
}

impl PausedFrame {
    /// Notes that V8 gave the variable `name` of the scope at `scope` in
    /// the frame's chain `value`. V8's listing of a scope is a copy made
    /// as it paused, which listing again would repeat; the binding checks
    /// the value given against the frame's slots as it checks V8's own.
    pub(super) fn assigned(&mut self, scope: usize, name: &str, value: &ReportedValue) {
        let chain = self.reported["scopeChain"]
            .as_array()
            .map_or(&[][..], Vec::as_slice);
        let Some(position) = listed_scopes(chain)
            .iter()
            .position(|&index| index == scope)
        else {
            // A context's variables are read from the context.
            return;
        };
        if let Some(Some(Ok(listed))) = self.scopes.get_mut(position) {
            for local in listed.iter_mut().filter(|local| &*local.name == name) {
                local.value = value.clone();
            }
        }
    }

    /// The frame's variables in declaration order, or why they are not
    /// known.
    fn locals(&self) -> crate::runtime_model::ReportedLocals {
        let mut locals = Vec::new();
        for scope in &self.scopes {
            match scope {
                Some(Ok(scope)) => locals.extend(scope.iter().cloned()),
                Some(Err(reason)) => return Err(Arc::clone(reason)),
                None => return Err("V8 did not list the frame's variables before the stop".into()),
            }
        }
        Ok(locals.into())
    }
}

/// A script frame of a thread's stack: its one-based place, and the
/// interpreted frame it is, when the runtime places it.
struct ScriptFrame {
    path: Arc<str>,
    line: u64,
    column: u64,
    frame: Option<VirtualAddress>,
}

/// Which pause in which isolate.
pub(super) type PauseKey = (RuntimeId, Session);

impl<P: LinuxTraceOps> Controller<P> {
    /// Asks V8 for the variables of each frame of the pause it reported in
    /// the isolate `session` reaches, while the paused thread can still
    /// answer (G5), and has the pause judged once they are listed.
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
        // A worker that waited for its debugger pauses before its first
        // statement, for nothing microscope asked for.
        if params["reason"] == inspector_protocol::BREAK_ON_START && hits.is_empty() {
            self.let_v8_go(runtime, session);
            return Ok(());
        }
        // A pause for no reason V8 names, and not the one asked for, is
        // at a `debugger` statement.
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
        // Any pause answers an interrupt: once V8 holds a thread, the
        // all-stop that judges the pause publishes it.
        let interrupted = self
            .inferior
            .as_mut()
            .is_some_and(|inferior| inferior.inspectors.end_interrupt());
        let Some(inspector) = self
            .inferior
            .as_mut()
            .and_then(|inferior| inferior.inspectors.records.get_mut(&runtime))
        else {
            return Ok(());
        };
        let (Some(link), Some(isolate)) = (
            inspector.connection.as_ref(),
            inspector.isolates.get_mut(session),
        ) else {
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

    /// Takes the isolate's sentinel out of `hits`, those of the pause V8
    /// reported at `location`. Returns whether the pause is at the
    /// sentinel, with the managed locations a source map set there, or
    /// `None` when it is at nothing else and V8 is let go: the script runs
    /// on with its mapped breakpoints set.
    fn sentinel_pause(
        &mut self,
        runtime: RuntimeId,
        session: &Session,
        hits: &mut Vec<Arc<str>>,
        params: &serde_json::Value,
    ) -> Option<(bool, Vec<u64>)> {
        let isolate = self.isolate_mut(runtime, session)?;
        let location = &params["callFrames"][0]["location"];
        let sentinel = isolate
            .sentinel
            .as_ref()
            .and_then(|sentinel| sentinel.placement.id.clone());
        let at_sentinel = sentinel.as_ref().is_some_and(|id| hits.contains(id));
        if !at_sentinel {
            return Some((false, Vec::new()));
        }
        hits.retain(|hit| Some(hit) != sentinel.as_ref());
        // The script's functions are compiled, and its declarations bound,
        // as it starts: a search finds them before any runs.
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

    /// Resumes the isolate `session` reaches from a pause no one sees.
    fn let_v8_go(&self, runtime: RuntimeId, session: &Session) {
        if let Some(link) = self
            .inferior
            .as_ref()
            .and_then(|inferior| inferior.inspectors.records.get(&runtime))
            .and_then(|inspector| inspector.connection.as_ref())
        {
            let _ = send(
                &link.connection,
                session,
                inspector_protocol::RESUME,
                serde_json::json!({}),
            );
        }
    }

    /// Keeps the variables V8 listed for a scope of a pause in `runtime`,
    /// and has the pause judged once every scope is listed.
    pub(super) fn scope_listed(
        &mut self,
        runtime: RuntimeId,
        id: RequestId,
        result: std::result::Result<serde_json::Value, super::super::cdp::CdpError>,
    ) -> Result<()> {
        let Some(paused) = self
            .inferior
            .as_mut()
            .and_then(|inferior| inferior.inspectors.records.get_mut(&runtime))
            .and_then(|inspector| {
                inspector
                    .isolates
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

    /// Has the pauses whose variables are listed judged: at once while the
    /// program runs, or as it resumes when it is stopped.
    fn judge_inspector_pause(&mut self) -> Result<()> {
        let inferior = self.inferior.as_ref().ok_or(Error::NotRunning)?;
        if inferior.public_stop.is_none() && inferior.active.is_some() {
            self.queue_internal_edit(super::super::Edit::InspectorPaused)?;
            self.finish_barrier_if_ready()?;
        }
        Ok(())
    }

    /// Judges every pause V8 reported whose variables are listed, once
    /// every thread is stopped.
    #[cfg_attr(
        not(debug_assertions),
        expect(unused_variables, reason = "only recorded")
    )]
    pub(in crate::backend::linux) fn apply_inspector_pauses(&mut self) {
        let Some(inferior) = self.inferior.as_ref() else {
            return;
        };
        // The main isolate's first, so that its thread is known before a
        // worker's is looked for.
        let mut ready = Vec::new();
        for (&runtime, inspector) in &inferior.inspectors.records {
            for (session, isolate) in &inspector.isolates {
                if isolate.paused.as_ref().is_some_and(Paused::ready) {
                    ready.push((runtime, session.clone()));
                }
            }
        }
        ready.sort_by_key(|(_, session)| session.path().len());
        // A worker's code may be in memory mapped since modules were last
        // read, and its frames cannot be unwound without it.
        if !ready.is_empty()
            && let Err(error) = self.refresh_libraries()
        {
            record!("modules are not refreshed for V8's pauses: {error}");
        }
        for key in ready {
            self.judge_pause(&key);
        }
    }

    /// Judges the pause `key` names: it stops for the breakpoints it hit
    /// whose conditions hold, the step it ends, the exception or statement
    /// it paused at, or the interrupt it answers; or else the program
    /// resumes as though it never paused.
    #[cfg_attr(
        not(debug_assertions),
        expect(unused_variables, reason = "only recorded")
    )]
    fn judge_pause(&mut self, key: &PauseKey) {
        let thread = self.paused_thread(key);
        let fallback = self.inferior.as_ref().map(|inferior| inferior.tgid);
        let Some(paused) = self.paused_mut(key) else {
            return;
        };
        paused.judged = true;
        paused.thread = thread.as_ref().ok().copied();
        // A pause held over from an earlier stop keeps the reason it was
        // judged to have, and counts no hit again.
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
        let Some(inferior) = self.inferior.as_mut() else {
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
        if let Some(barrier) = inferior.barrier.as_mut() {
            if barrier.reason.as_ref().is_none_or(|current| {
                super::super::classify::visible_stop_priority(&reason)
                    > super::super::classify::visible_stop_priority(current)
            }) {
                barrier.triggering_thread = pid;
                barrier.reason = Some(reason);
            }
            barrier.paused |= interrupted;
        }
    }

    /// Why the pause `key` names, on `pid`, stops, if it does.
    fn pause_reason(&mut self, key: &PauseKey, pid: Pid) -> Option<StopReason> {
        let (runtime, session) = key;
        let inferior = self.inferior.as_ref()?;
        let isolate = inferior
            .inspectors
            .records
            .get(runtime)?
            .isolates
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
            // An interrupt's pause that V8 took after the program stopped
            // otherwise, or one that could not be withdrawn.
            Answers::Nothing => None,
        };
        self.inspector_step_end(key, pid, depth).or(answered)
    }

    /// The thread the pause `key` names holds: the process's main thread
    /// for the main isolate, and for a worker the one thread no other
    /// pause holds whose script frames are the ones V8 listed.
    fn paused_thread(&self, key: &PauseKey) -> std::result::Result<Pid, Arc<str>> {
        let inferior = self.inferior.as_ref().ok_or("the process is gone")?;
        if key.1.path().is_empty() {
            return Ok(inferior.tgid);
        }
        let paused = self.paused(key).ok_or("V8 no longer holds this pause")?;
        let held = inferior
            .inspectors
            .records
            .values()
            .flat_map(|inspector| inspector.isolates.values())
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

    /// The script frames of `pid`'s stack, innermost first.
    fn script_frames(&self, pid: Pid) -> Result<Vec<ScriptFrame>> {
        let inferior = self.inferior.as_ref().ok_or(Error::NotRunning)?;
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
            // V8 places frames in the scripts it runs.
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

    /// Binds the variables V8 listed for the frames of the pause `key`
    /// names to the interpreted frames of `pid`'s stack, pairing them
    /// innermost first while their places agree.
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
        if let Some(inferior) = self.inferior.as_mut() {
            inferior.inspectors.locals.extend(bound);
        }
        Ok(())
    }

    /// Notes that the stop published is the pause V8 holds `pid` in, if
    /// it is one.
    pub(in crate::backend::linux) fn mark_published_pause(
        &mut self,
        pid: Pid,
        reason: &StopReason,
    ) {
        if let Some(inferior) = self.inferior.as_mut() {
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

    /// Lets go the threads V8 holds paused among `resumed`, which have just
    /// been resumed. A pause not yet judged, or one whose stop was not the
    /// one published, stays held and is judged at an internal stop.
    pub(in crate::backend::linux) fn resume_inspectors(
        &mut self,
        resumed: &BTreeSet<Pid>,
    ) -> Result<()> {
        let Some(inferior) = self.inferior.as_mut() else {
            return Ok(());
        };
        let main = inferior.tgid;
        let mut held = false;
        let mut ended = false;
        for inspector in inferior.inspectors.records.values_mut() {
            let Inspector {
                connection,
                isolates,
                step,
                desired,
                assigning,
                ..
            } = inspector;
            for (session, isolate) in isolates.iter_mut() {
                // The isolate an assignment is made in stays paused while
                // the program runs, and is asked to make it.
                if let (Some(assignment), Some(link)) = (
                    inspector_assigning(assigning, session, isolate, resumed),
                    connection.as_ref(),
                ) {
                    super::assignments::begin(&link.connection, session, isolate, assignment);
                    // One that could not be begun ends at once.
                    ended |= assignment.ended();
                    continue;
                }
                held |= resume_isolate(
                    connection.as_ref().map(|link| &link.connection),
                    session,
                    isolate,
                    step,
                    resumed,
                );
            }
            if resumed.contains(&main)
                && let (Some(link), Some(isolate)) =
                    (connection.as_ref(), isolates.get_mut(&Session::main()))
            {
                pause_for_step(&link.connection, isolate, step.as_mut());
            }
            // A function compiled while the program ran is found as it
            // runs on.
            if let Some(link) = connection.as_ref()
                && super::functions::wanted(desired)
            {
                for (session, isolate) in isolates.iter_mut() {
                    super::functions::search(&link.connection, session, isolate);
                }
            }
        }
        // Names hold only while their thread stays where V8 paused it.
        let judged = inferior.inspectors.records.values().any(|inspector| {
            inspector
                .isolates
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
        self.inferior
            .as_ref()?
            .inspectors
            .records
            .get(runtime)?
            .isolates
            .get(session)?
            .paused
            .as_ref()
    }

    fn paused_mut(&mut self, (runtime, session): &PauseKey) -> Option<&mut Paused> {
        self.isolate_mut(*runtime, session)?.paused.as_mut()
    }

    fn isolate_mut(&mut self, runtime: RuntimeId, session: &Session) -> Option<&mut Isolate> {
        self.inferior
            .as_mut()?
            .inspectors
            .records
            .get_mut(&runtime)?
            .isolates
            .get_mut(session)
    }

    /// Whether V8 holds thread `thread` paused, where its innermost
    /// JavaScript frame is the stop's.
    pub(in crate::backend::linux) fn inspector_holds(&self, thread: crate::ThreadId) -> bool {
        self.inferior.as_ref().is_some_and(|inferior| {
            inferior
                .inspectors
                .records
                .values()
                .flat_map(|inspector| inspector.isolates.values())
                .filter_map(|isolate| isolate.paused.as_ref()?.thread)
                .any(|pid| debug_thread_id(pid) == thread)
        })
    }
}

/// The managed locations `isolate` set by a source map at `location`, a
/// place V8 paused at.
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

/// The assignment not yet begun that `assigning` asks of the isolate
/// `session` reaches, whose paused thread is among `resumed`.
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

/// Lets go the pause of the isolate `session` reaches, if V8 holds one
/// on a thread among `resumed`, with the step it runs if `step` began
/// there, or `Debugger.resume`. Returns whether the pause stays held: one
/// not yet judged, or whose stop was not published.
fn resume_isolate(
    connection: Option<&Connection>,
    session: &Session,
    isolate: &mut Isolate,
    step: &mut Option<Stepping>,
    resumed: &BTreeSet<Pid>,
) -> bool {
    let Some(paused) = isolate.paused.as_mut() else {
        return false;
    };
    if !paused.judged {
        // A pause still listing its variables is judged once they are.
        return paused.listing.is_empty();
    }
    // A pause no thread was found for is let go with any.
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
        None => inspector_protocol::RESUME,
    };
    if let Some(connection) = connection {
        isolate.resuming |= send(connection, session, command, serde_json::json!({})).is_ok();
    }
    isolate.paused = None;
    false
}

/// The scopes of a frame whose variables are its stack locals, by their
/// index in its scope chain, in the order they are listed: the local
/// scope, then the blocks inside it from the outermost. V8 gives the chain
/// innermost first.
fn listed_scopes(chain: &[serde_json::Value]) -> Vec<usize> {
    let local = chain
        .iter()
        .position(|scope| scope["type"] == "local")
        .unwrap_or(chain.len());
    (local..chain.len().min(local + 1))
        .chain(
            (0..local)
                .rev()
                .filter(|&index| chain[index]["type"] == "block"),
        )
        .collect()
}

/// The frame `frame` of a pause, the `index`th, whose scopes' variables
/// are asked for over `connection`, each request entered in `listing`.
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
        let sent = scope["object"]["objectId"]
            .as_str()
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
        path: frame["location"]["scriptId"]
            .as_str()
            .and_then(|id| scripts.get(id))
            .map(|url| Arc::from(inspector_protocol::script_path(url).as_ref())),
        line: frame["location"]["lineNumber"].as_u64().unwrap_or(u64::MAX),
        column: frame["location"]["columnNumber"]
            .as_u64()
            .unwrap_or(u64::MAX),
        scopes,
    }
}
