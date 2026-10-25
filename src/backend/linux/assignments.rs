//! Assignments to variables a runtime keeps, which only its agent can make
//! safely: the variable's frame's thread is the only one the interpreter
//! runs, its agent stopped it, and it runs alone, as for a step, until the
//! agent reports the assignment made. The stop that ends it is published
//! under a new stop, and the assignment's request is answered after it with
//! the variable read again there. Anywhere else, the assignment is refused
//! and nothing runs.

use std::sync::Arc;

use nix::unistd::Pid;

use crate::eval::Evaluation;
use crate::eval::error::ErrorKind;
use crate::eval::interp::Assigned;
use crate::eval::syntax::{Expression, Span};
use crate::protocol::{ExceptionDisposition, Reply, ResumeScope, StopId, StopReason};
use crate::runtime_model::{AssignedValue, RuntimeAssignment};
use crate::{
    Error, ExecutionContext, FrameKind, InspectionLimits, Result, RuntimeId, VirtualAddress,
};

use super::evaluation::{Place, StopObject};
use super::frames::StackRoot;
use super::native::LinuxTraceOps;
use super::{ActiveKind, Controller, debug_thread_id, process_id};

/// An assignment a runtime's agent is making, and the request it answers.
pub(super) struct PendingAssignment {
    /// The request, once the evaluation has begun the assignment.
    reply: Option<Reply<Evaluation>>,
    thread: Pid,
    frame: VirtualAddress,
    expression: Expression,
    limits: InspectionLimits,
    span: Span,

    /// Why the agent did not make it, as it reported.
    failure: Option<Arc<str>>,

    /// Whether V8's inspector reported it made, whatever stop followed.
    made: bool,
}

/// An assignment refused at `span` for `reason`.
fn refused(span: Span, reason: impl Into<String>) -> Error {
    Error::Expression(crate::ExpressionError::new(
        ErrorKind::Assignment,
        span,
        reason.into(),
    ))
}

impl<P: LinuxTraceOps> Controller<P> {
    /// Begins giving the variable `target` names the value `value`, as its
    /// runtime's agent gives it on the variable's frame's thread, when that
    /// thread is the only one the interpreter runs and its agent stopped
    /// it.
    pub(super) fn assign_interpreted(
        &mut self,
        stop_id: StopId,
        root: &StackRoot,
        (expression, limits): (&Expression, InspectionLimits),
        target: Option<StopObject>,
        value: Assigned<Place>,
        span: Span,
    ) -> Result<()> {
        let (runtime, frame, slot) = target
            .as_ref()
            .and_then(StopObject::interpreted_variable)
            .ok_or_else(|| {
                refused(
                    span,
                    "only a variable of the frame can be given a value, not a global or a \
                     built-in",
                )
            })?;
        let pid = root.thread().ok_or_else(|| {
            refused(
                span,
                "only a frame a thread runs can be changed, not a suspended one",
            )
        })?;
        if self.inspects(runtime) {
            return self.assign_javascript(
                stop_id,
                pid,
                (runtime, frame, slot),
                (expression, limits),
                value,
                span,
            );
        }
        let value = assigned_value(runtime, value, span)?;
        self.assignable(pid, runtime, span)?;

        let inferior = self.inferior.as_mut().ok_or(Error::NotRunning)?;
        let number = inferior.next_runtime_assignment.saturating_add(1);
        inferior.next_runtime_assignment = number;
        inferior.runtime_assignment = Some((
            runtime,
            RuntimeAssignment {
                number,
                thread: debug_thread_id(pid),
                frame: frame.address,
                slot,
                value,
            },
        ));
        let process = process_id(inferior.tgid);
        let scope = ResumeScope::Thread(debug_thread_id(pid));
        let execution_id = self.with_arming_notification(runtime, |controller| {
            // The agent makes it once it has been sent it.
            if let Err(reason) = controller.arm_runtime(runtime)
                && !controller.agent_assignment_sent(runtime)
            {
                controller.disarm_runtime_assignment(runtime);
                return Err(refused(
                    span,
                    format!("the runtime's agent cannot make it: {reason}"),
                ));
            }

            let begun = controller.begin_execution(
                process,
                stop_id,
                scope,
                ActiveKind::RuntimeAssignment {
                    thread: pid,
                    runtime,
                    number,
                },
                ExceptionDisposition::Pass,
            );
            if begun.is_err() {
                controller.disarm_runtime_assignment(runtime);
            }

            begun
        })?;

        if let Some(inferior) = self.inferior.as_mut() {
            inferior.assignment = Some(PendingAssignment {
                reply: None,
                thread: pid,
                frame: frame.address,
                expression: expression.clone(),
                limits,
                span,
                failure: None,
                made: false,
            });
        }
        let _ = self.events.send(crate::DebuggerEvent::InferiorContinued {
            revision: self.revision,
            process_id: process,
            execution_id,
            resumed: scope,
        });
        Ok(())
    }

    /// Whether the runtime `runtime` is driven through its inspector, which
    /// makes its assignments, rather than an agent.
    fn inspects(&self, runtime: RuntimeId) -> bool {
        self.inferior.as_ref().is_some_and(|inferior| {
            inferior.inspectors.has(runtime)
                && self
                    .runtimes(inferior)
                    .into_iter()
                    .any(|bound| bound.id == runtime && bound.model.agent().is_none())
        })
    }

    /// Begins giving the variable in slot `slot` of the JavaScript frame
    /// `frame` the value `value`, which V8's inspector gives it while V8
    /// holds the thread paused and every thread runs.
    fn assign_javascript(
        &mut self,
        stop_id: StopId,
        pid: Pid,
        (runtime, frame, slot): (RuntimeId, crate::runtime_model::InterpretedFrameRef, u32),
        (expression, limits): (&Expression, InspectionLimits),
        value: Assigned<Place>,
        span: Span,
    ) -> Result<()> {
        let inferior = self.inferior.as_ref().ok_or(Error::NotRunning)?;
        let number = inferior.next_runtime_assignment.saturating_add(1);
        let assignment = self
            .javascript_assignment(
                pid,
                runtime,
                frame,
                slot,
                value,
                expression.assigned_name(),
                number,
            )
            .map_err(|reason| refused(span, &*reason))?;
        let process = process_id(inferior.tgid);
        // V8 answers only while its inspector's thread runs.
        let scope = ResumeScope::Process(process);
        // The execution's resume sends V8 its first request.
        self.await_javascript_assignment(runtime, Some(assignment));
        let begun = self.begin_execution(
            process,
            stop_id,
            scope,
            ActiveKind::RuntimeAssignment {
                thread: pid,
                runtime,
                number,
            },
            ExceptionDisposition::Pass,
        );
        let execution_id = match begun {
            Ok(execution_id) => execution_id,
            Err(error) => {
                self.await_javascript_assignment(runtime, None);
                return Err(error);
            }
        };
        if let Some(inferior) = self.inferior.as_mut() {
            inferior.next_runtime_assignment = number;
            inferior.assignment = Some(PendingAssignment {
                reply: None,
                thread: pid,
                frame: frame.address,
                expression: expression.clone(),
                limits,
                span,
                failure: None,
                made: false,
            });
        }
        let _ = self.events.send(crate::DebuggerEvent::InferiorContinued {
            revision: self.revision,
            process_id: process,
            execution_id,
            resumed: scope,
        });
        Ok(())
    }

    /// Notes how the pending JavaScript assignment ended: made, or, with
    /// why, not.
    pub(super) fn javascript_assignment_ended(&mut self, made: bool, failure: Option<Arc<str>>) {
        if let Some(pending) = self
            .inferior
            .as_mut()
            .and_then(|inferior| inferior.assignment.as_mut())
        {
            pending.made |= made;
            if failure.is_some() {
                pending.failure = failure;
            }
        }
    }

    /// Refuses an assignment the runtime `runtime`'s agent would make on
    /// thread `pid` unless it can make it exactly: the agent stopped the
    /// thread, takes commands there, and the thread is the interpreter's
    /// only one, of an interpreter that runs one thread at a time.
    fn assignable(&self, pid: Pid, runtime: RuntimeId, span: Span) -> Result<()> {
        let inferior = self.inferior.as_ref().ok_or(Error::NotRunning)?;
        let bound = self
            .runtimes(inferior)
            .into_iter()
            .find(|bound| bound.id == runtime)
            .ok_or_else(|| refused(span, "the frame's runtime is no longer loaded"))?;
        let agent = bound
            .model
            .agent()
            .ok_or_else(|| refused(span, "the frame's runtime has no agent to make it"))?;
        if agent.process_scope_required() {
            return Err(refused(
                span,
                "a free-threaded interpreter's other threads may hold the objects it changes",
            ));
        }
        if let Some(reason) = agent.assignments_refused() {
            return Err(refused(span, reason));
        }
        if !self.at_agent_hook(pid, runtime) {
            return Err(refused(
                span,
                "its runtime's agent makes it where the agent stopped the thread, as at a \
                 breakpoint it reported or a step's end",
            ));
        }
        if inferior
            .threads
            .get(&pid)
            .is_some_and(|thread| thread.pending_signal.is_some())
        {
            return Err(refused(
                span,
                "the thread has a signal to take first; continue it",
            ));
        }

        let sole = self
            .with_runtime_stop(inferior, &bound, pid, |stop| {
                bound.model.sole_thread(stop, debug_thread_id(pid))
            })
            .map_err(|reason| {
                refused(
                    span,
                    format!("the interpreter's threads cannot be read: {reason}"),
                )
            })?;
        if !sole {
            return Err(refused(
                span,
                "another thread the interpreter runs may be holding what it changes; it is \
                 made only where the frame's thread is the interpreter's only one",
            ));
        }

        if !self.agent_takes_commands(runtime) {
            return Err(refused(
                span,
                "the runtime's agent is not ready to take it; continue to its next stop",
            ));
        }

        Ok(())
    }

    /// Holds an assignment's request until the stop that ends it.
    pub(super) fn await_assignment(&mut self, reply: Reply<Evaluation>) {
        let pending = self
            .inferior
            .as_mut()
            .and_then(|inferior| inferior.assignment.as_mut());
        match pending {
            Some(pending) => pending.reply = Some(reply),
            None => {
                let _ = reply.send(Err(Error::internal("a begun assignment is not pending")));
            }
        }
    }

    /// Why thread `pid` stops where the runtime `runtime` reports the
    /// assignment numbered `number`, made or, with why, not: when that is
    /// the active assignment and `pid` makes it.
    pub(super) fn runtime_assignment_end(
        &mut self,
        pid: Pid,
        runtime: RuntimeId,
        number: u64,
        failure: Option<Arc<str>>,
    ) -> Option<StopReason> {
        let inferior = self.inferior.as_mut()?;
        let active = inferior.active.as_ref()?;
        let ActiveKind::RuntimeAssignment {
            thread,
            runtime: assigning,
            number: assignment,
        } = active.kind
        else {
            return None;
        };
        if thread != pid || assigning != runtime || assignment != number {
            return None;
        }

        if let Some(pending) = inferior.assignment.as_mut() {
            pending.failure = failure;
        }
        Some(StopReason::Assignment)
    }

    /// Disarms the active assignment, if there is one, as the stop that
    /// ends it forms: the agent makes each assignment once, by its number,
    /// so that being armed with it again never makes it twice.
    pub(super) fn end_runtime_assignment(&mut self) {
        self.end_javascript_assignments();
        // Only an agent is armed with an assignment.
        let runtime = self
            .inferior
            .as_ref()
            .filter(|inferior| inferior.runtime_assignment.is_some())
            .and_then(|inferior| inferior.active.as_ref())
            .and_then(|active| match active.kind {
                ActiveKind::RuntimeAssignment { runtime, .. } => Some(runtime),
                _ => None,
            });
        if let Some(runtime) = runtime {
            self.with_arming_notification(runtime, |controller| {
                controller.disarm_runtime_assignment(runtime);
            });
        }
    }

    /// Selects the frame of the pending assignment's variable at the stop
    /// that ends it.
    pub(super) fn select_assigned_frame(&mut self, reason: &StopReason) {
        if !matches!(reason, StopReason::Assignment) {
            return;
        }
        let Some((thread, frame)) = self
            .inferior
            .as_ref()
            .and_then(|inferior| inferior.assignment.as_ref())
            .map(|pending| (pending.thread, pending.frame))
        else {
            return;
        };
        let Some((stop_id, context)) = self
            .inferior
            .as_ref()
            .and_then(|inferior| inferior.public_stop.as_ref())
            .map(|stop| (stop.id, stop.selected))
        else {
            return;
        };
        let Ok(trace) = self.backtrace(stop_id, &StackRoot::of_thread(thread)) else {
            return;
        };
        let Some(id) = self.assigned_frame(stop_id, thread, &trace.frames, frame) else {
            return;
        };
        if let Some(stop) = self
            .inferior
            .as_mut()
            .and_then(|inferior| inferior.public_stop.as_mut())
        {
            stop.selected_frames.insert(context, id);
        }
    }

    /// Answers the pending assignment's request at the stop `stop_id`,
    /// published for `reason`: with its variable read again there, when
    /// the agent made it; otherwise with why not.
    pub(super) fn answer_assignment(&mut self, stop_id: StopId, reason: &StopReason) {
        let Some(pending) = self
            .inferior
            .as_mut()
            .and_then(|inferior| inferior.assignment.take())
        else {
            return;
        };
        let answer = match (reason, &pending.failure) {
            (StopReason::Assignment, None) => self.read_assignment(stop_id, &pending),
            (_, None) if pending.made => self.read_assignment(stop_id, &pending),
            (_, Some(failure)) => Err(refused(pending.span, &**failure)),
            _ => Err(refused(
                pending.span,
                "the thread stopped for another reason before its agent reported the \
                 assignment, which it may yet make as the thread runs on",
            )),
        };

        if let Some(reply) = pending.reply {
            let _ = reply.send(answer);
        }
    }

    /// Answers the pending assignment's request, if there is one, as the
    /// process ends before its stop.
    pub(super) fn abandon_assignment(inferior: &mut super::Inferior) {
        if let Some(reply) = inferior
            .assignment
            .take()
            .and_then(|pending| pending.reply.map(|reply| (reply, pending.span)))
        {
            let (reply, span) = reply;
            let _ = reply.send(Err(refused(
                span,
                "the process ended before the assignment was made",
            )));
        }
    }

    /// The assigned variable, read again in its frame at the stop
    /// `stop_id`.
    fn read_assignment(&self, stop_id: StopId, pending: &PendingAssignment) -> Result<Evaluation> {
        let root = self.stack_root(
            stop_id,
            ExecutionContext::Thread(debug_thread_id(pending.thread)),
        )?;
        let trace = self.backtrace(stop_id, &root)?;
        let frame = self
            .assigned_frame(stop_id, pending.thread, &trace.frames, pending.frame)
            .ok_or_else(|| refused(pending.span, "the variable's frame is gone"))?;

        self.read_assigned(stop_id, &root, frame, &pending.expression, pending.limits)
    }

    /// The frame among `frames`, `pid`'s at the stop `stop_id`, that shows
    /// the interpreted frame at `address`: an interpreter's own frame, or a
    /// frame of code a runtime generated, as V8's are, whose frame pointer
    /// it is.
    fn assigned_frame(
        &self,
        stop_id: StopId,
        pid: Pid,
        frames: &[crate::StackFrame],
        address: VirtualAddress,
    ) -> Option<crate::StackFrameId> {
        frames
            .iter()
            .find(|candidate| candidate.kind == FrameKind::Interpreted { frame: address })
            .or_else(|| {
                frames.iter().find(|candidate| {
                    self.interpreted_frame(stop_id, pid, candidate.id)
                        .is_some_and(|(_, found)| found == address)
                })
            })
            .map(|candidate| candidate.id)
    }

    /// Arms the runtime with no assignment.
    #[cfg_attr(
        not(debug_assertions),
        expect(unused_variables, reason = "only recorded")
    )]
    fn disarm_runtime_assignment(&mut self, runtime: RuntimeId) {
        if let Some(inferior) = self.inferior.as_mut() {
            inferior.runtime_assignment = None;
        }

        if let Err(reason) = self.arm_runtime(runtime) {
            record!("runtime {runtime} keeps its assignment: {reason}");
        }
    }
}

/// What the runtime `runtime`'s agent gives a variable for `value`: a value
/// it makes, or one of its own objects. A value of a program's native types
/// is no object of the runtime's.
fn assigned_value(runtime: RuntimeId, value: Assigned<Place>, span: Span) -> Result<AssignedValue> {
    Ok(match value {
        Assigned::Integer(integer) => AssignedValue::Integer(integer.to_string().into()),
        Assigned::Float(float) => AssignedValue::Float(float.to_bits()),
        Assigned::Bool(value) => AssignedValue::Bool(value),
        Assigned::Text(text) => AssignedValue::Text(text.into()),
        Assigned::Nothing => AssignedValue::Nothing,
        Assigned::Undefined | Assigned::BigInt(_) => {
            return Err(refused(span, "Python has no such value"));
        }
        Assigned::Object(Place::Interpreted {
            runtime: keeper,
            object,
        }) if keeper == runtime => AssignedValue::Object(object),
        Assigned::Object(Place::Interpreted { .. }) => {
            return Err(refused(span, "the object belongs to another interpreter"));
        }
        Assigned::Object(Place::Native(_)) => {
            return Err(refused(
                span,
                "a value of a program's native type cannot be given to a variable of a \
                 dynamic type",
            ));
        }
    })
}
