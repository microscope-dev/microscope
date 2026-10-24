//! Steps V8 runs for microscope: `stepInto`, `stepOver`, and `stepOut`,
//! owned by the isolate they began in, which go on through Node's own
//! scripts, skipped files (F19), and generated code a source map leads to
//! no source. A step from a native stop of the main thread first pauses
//! V8 at its next statement.

use std::collections::BTreeMap;
use std::sync::Arc;

use nix::unistd::Pid;

use super::super::Controller;
use super::super::cdp::{Connection, Session};
use super::super::debug_thread_id;
use super::super::native::{InspectionOps, LinuxTraceOps};
use super::interrupts::ask_pause;
use super::pauses::PauseKey;
use super::{Isolate, send};
use crate::runtime_model::inspector_protocol;
use crate::{RuntimeId, StopReason};

/// A step V8 runs for microscope.
#[derive(Debug)]
pub(super) struct Stepping {
    pub(super) number: u64,
    pub(super) kind: crate::StepKind,

    /// The isolate that runs it, where it began: a pause in another does
    /// not end it.
    pub(super) session: Session,

    /// How many frames the paused thread had where the step began; `None`
    /// for a step begun where V8 did not hold the thread, which ends at
    /// V8's next pause outside skipped scripts.
    pub(super) depth: Option<usize>,

    /// The command the step continues with as its thread resumes: the
    /// step's own, then `stepOut` from where a breakpoint whose condition
    /// failed paused it deeper than it ends.
    pub(super) command: &'static str,

    /// Whether V8 was sent the command, so that its next pause ends it.
    pub(super) sent: bool,

    /// How many pauses in skipped scripts the step has gone on from.
    pub(super) skipped: u32,
}

impl<P: LinuxTraceOps> Controller<P> {
    /// Whether V8 goes on from the pause that `params` describes in the
    /// isolate `session` reaches, which no breakpoint stopped, with the
    /// program left running: a step that pauses in a skipped script goes
    /// on into the program's next statement, and an exception caught or a
    /// `debugger` statement there is ignored.
    pub(super) fn step_goes_on(
        &mut self,
        runtime: RuntimeId,
        session: &Session,
        params: &serde_json::Value,
        statement: bool,
    ) -> bool {
        let (skips, enter) = (self.step_skips.clone(), self.step_into_runtime);
        let Some(inspectors) = self
            .inferior
            .as_mut()
            .map(|inferior| &mut inferior.inspectors)
        else {
            return false;
        };
        let (Some(inspector), maps) = (inspectors.records.get_mut(&runtime), &inspectors.maps)
        else {
            return false;
        };
        let location = &params["callFrames"][0]["location"];
        let top = location["scriptId"]
            .as_str()
            .and_then(|id| inspector.isolates.get(session)?.scripts.get(id).cloned());
        let Some(link) = inspector.connection.as_ref() else {
            return false;
        };
        let original = top.as_ref().and_then(|url| {
            let map = maps.get(&*inspector_protocol::script_path(url))?;
            let generated = crate::RecordedPlace {
                path: Arc::clone(url),
                line: u32::try_from(location["lineNumber"].as_u64()? + 1).ok()?,
                column: u32::try_from(location["columnNumber"].as_u64()? + 1).ok()?,
            };
            Some(map.original(&generated))
        });
        // Generated code with no source is no statement of the program.
        let unmapped = matches!(original, Some(Err(_)));
        let skip = match (&top, original) {
            (None, _) => false,
            (Some(_), Some(Err(_))) => true,
            (Some(_), Some(Ok(original))) => skipped(&original.path, &skips, enter),
            (Some(url), None) => skipped(url, &skips, enter),
        };
        if !skip {
            return false;
        }
        let step = inspector
            .step
            .as_mut()
            .filter(|step| step.sent && step.session == *session);
        let command = match (params["reason"].as_str(), step) {
            // A step from a native stop pauses for no reason V8 names, and
            // one into a script just loaded also at its sentinel.
            (Some("step" | "other" | "ambiguous"), Some(step)) if step.skipped < MAX_SKIPPED => {
                step.skipped += 1;
                // Out of generated code by the step's own kind, and out of
                // Node's into the program's next statement.
                if unmapped && step.depth.is_some() {
                    step.command
                } else {
                    inspector_protocol::STEP_INTO
                }
            }
            // An exception caught in code steps pass through is that
            // code's own business, as Node's module loader's are.
            (Some("exception" | "promiseRejection"), step)
                if params["data"]["uncaught"] != true =>
            {
                step.map_or(inspector_protocol::RESUME, |step| step.command)
            }
            // So is a `debugger` statement there.
            (Some("other"), None) if statement => inspector_protocol::RESUME,
            _ => return false,
        };
        send(&link.connection, session, command, serde_json::json!({})).is_ok()
    }

    /// Whether a step V8 was told to run runs in the isolate `session`
    /// reaches.
    pub(super) fn steps_in(&self, runtime: RuntimeId, session: &Session) -> bool {
        self.inferior
            .as_ref()
            .and_then(|inferior| inferior.inspectors.records.get(&runtime)?.step.as_ref())
            .is_some_and(|step| step.sent && step.session == *session)
    }

    /// Why a pause of `pid` with `depth` frames, in the isolate `key`
    /// names, that no breakpoint stopped ends the step V8 runs there, if it
    /// does. One that does not end it continues it out of the call it
    /// paused in.
    pub(super) fn inspector_step_end(
        &mut self,
        key: &PauseKey,
        pid: Pid,
        depth: usize,
    ) -> Option<StopReason> {
        let (runtime, session) = key;
        let step = self
            .inferior
            .as_mut()?
            .inspectors
            .records
            .get_mut(runtime)?
            .step
            .as_mut()
            .filter(|step| step.sent && step.session == *session)?;
        if !ends_step(step, depth) {
            step.command = inspector_protocol::STEP_OUT;
            step.sent = false;
            return None;
        }
        let number = step.number;
        let incomplete = (step.skipped >= MAX_SKIPPED).then(|| {
            Arc::from(format!(
                "the step went through {MAX_SKIPPED} statements of Node's own scripts and stopped there"
            ))
        });
        self.runtime_step_end(pid, *runtime, number, incomplete)
    }
}

impl<P: InspectionOps> Controller<P> {
    /// The runtime whose inspector runs a step of `kind` from frame
    /// `frame` of `pid`, the main isolate's thread, which V8 does not hold:
    /// into or over from native code with no lines, from which a native
    /// step would run to whatever native code outside has them. It runs to
    /// the program's next JavaScript statement instead. A step out keeps
    /// its native meaning.
    pub(in crate::backend::linux) fn javascript_step_from_native(
        &self,
        stop_id: crate::protocol::StopId,
        pid: Pid,
        frame: crate::StackFrameId,
        kind: crate::StepKind,
    ) -> Option<RuntimeId> {
        let inferior = self.inferior.as_ref()?;
        if !matches!(
            kind,
            crate::StepKind::IntoSource | crate::StepKind::OverSource
        ) || pid != inferior.tgid
            || self.inspector_holds(debug_thread_id(pid))
        {
            return None;
        }
        let runtime = inferior
            .inspectors
            .records
            .iter()
            .find(|(_, inspector)| inspector.isolates.contains_key(&Session::main()))
            .map(|(&runtime, _)| runtime)?;
        let trace = self
            .backtrace(stop_id, &super::super::frames::StackRoot::of_thread(pid))
            .ok()?;
        trace
            .frames
            .iter()
            .find(|candidate| candidate.id == frame)
            .filter(|frame| frame.source.is_none() && frame.recorded.is_none())
            .map(|_| runtime)
    }
}

/// Asks the main isolate to pause for the step from a native stop that
/// `step` is, if it is one not yet begun, as its thread resumes.
pub(super) fn pause_for_step(
    connection: &Connection,
    isolate: &mut Isolate,
    step: Option<&mut Stepping>,
) {
    let Some(step) = step.filter(|step| !step.sent && step.depth.is_none()) else {
        return;
    };
    step.sent = true;
    ask_pause(connection, &Session::main(), isolate);
}

/// The step V8 runs for `step`, which begins where an isolate of
/// `isolates` holds the step's thread paused. From anywhere else on
/// `main`, the thread of the main isolate, it pauses at the next statement
/// V8 runs (F19).
pub(super) fn stepping(
    isolates: &BTreeMap<Session, Isolate>,
    step: &crate::runtime_model::RuntimeStep,
    main: crate::ThreadId,
) -> std::result::Result<Stepping, Arc<str>> {
    let command = match step.kind {
        crate::StepKind::IntoSource => inspector_protocol::STEP_INTO,
        crate::StepKind::OverSource => inspector_protocol::STEP_OVER,
        crate::StepKind::Out => inspector_protocol::STEP_OUT,
        kind => return Err(format!("V8 cannot step JavaScript by {kind:?}").into()),
    };
    let paused = isolates.iter().find_map(|(session, isolate)| {
        let depth = isolate.paused.as_ref()?.depth_holding(step.thread)?;
        Some((session.clone(), depth))
    });
    let (session, depth, command) = match paused {
        Some((session, depth)) => (session, Some(depth), command),
        None if step.thread == main => (Session::main(), None, inspector_protocol::PAUSE),
        None => {
            return Err(
                "JavaScript steps from a native stop run on the main thread; on a worker, they begin where V8 paused it".into(),
            );
        }
    };
    Ok(Stepping {
        number: step.number,
        kind: step.kind,
        session,
        depth,
        command,
        sent: false,
        skipped: 0,
    })
}

/// The most pauses in skipped scripts a step goes on from before it ends
/// where it is. From a timer's landing in Node's code, the program's next
/// statement took 59 (F19).
const MAX_SKIPPED: u32 = 1000;

/// Whether a step goes on from a pause in the script at `url`: Node's own
/// scripts, unless steps enter a runtime's code, and the files `skips`
/// names. V8's blackboxing cannot skip Node's: it decides by a script's
/// context, which the builtins in Node's snapshot do not record.
fn skipped(url: &str, skips: &crate::StepSkips, enter_runtime: bool) -> bool {
    (url.starts_with("node:") && !enter_runtime)
        || skips.matches(&inspector_protocol::script_path(url))
}

/// Whether a pause with `depth` frames ends `step`, which no breakpoint
/// stopped: V8 ends a step at its next pause, unless a breakpoint whose
/// condition failed paused it in a call the step runs through.
pub(super) const fn ends_step(step: &Stepping, depth: usize) -> bool {
    match (step.kind, step.depth) {
        (crate::StepKind::OverSource, Some(began)) => depth <= began,
        (crate::StepKind::Out, Some(began)) => depth < began,
        _ => true,
    }
}
