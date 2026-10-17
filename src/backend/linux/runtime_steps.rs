//! Steps through code a runtime runs itself, which have no instructions of
//! their own to step: the runtime is armed with the step, every thread runs
//! as for a continue, and the step ends where the runtime's internal hook
//! reports its end on the step's own thread or task. Another thread's
//! report is not the step's end, as another thread reaching a native
//! step's plan is not. The runtime is disarmed at whatever stop ends the
//! step.

use nix::unistd::Pid;

use crate::protocol::{
    ExceptionDisposition, ExecutionId, ProcessId, ResumeScope, StopId, StopReason,
};
use crate::runtime_model::RuntimeStep;
use crate::{Error, ExecutionContext, Result, RuntimeId, StackFrameId, StepKind, VirtualAddress};

use super::frames::FrameScope;
use super::native::LinuxTraceOps;
use super::{ActiveKind, Controller, StepOwner, debug_thread_id};

impl<P: LinuxTraceOps> Controller<P> {
    /// The runtime that interprets frame `frame` of thread `pid`'s stack at
    /// the stop `stop_id`, and the frame's address, when the frame is one a
    /// runtime interprets. A thread no runtime interprets on is not
    /// unwound to tell.
    pub(super) fn interpreted_frame(
        &self,
        stop_id: StopId,
        pid: Pid,
        frame: StackFrameId,
    ) -> Option<(RuntimeId, VirtualAddress)> {
        let inferior = self.inferior.as_ref()?;
        let root = self
            .stack_root(stop_id, ExecutionContext::Thread(debug_thread_id(pid)))
            .ok()?;
        if !self.interprets_on(inferior, &root) {
            return None;
        }

        match self.resolve_frame(inferior, &root, frame).ok()?.scope {
            FrameScope::Interpreted { runtime, frame } => Some((runtime, frame)),
            _ => None,
        }
    }

    /// Steps thread `pid` from the interpreted frame at `frame` of the
    /// runtime `runtime`, as that runtime steps. A runtime that cannot step
    /// so refuses the step, and nothing runs.
    #[expect(
        clippy::too_many_arguments,
        reason = "a step names its stop, thread, runtime, frame, kind, scope, and exception disposition"
    )]
    pub(super) fn begin_runtime_step(
        &mut self,
        process_id: ProcessId,
        stop_id: StopId,
        pid: Pid,
        (runtime, frame): (RuntimeId, VirtualAddress),
        kind: StepKind,
        scope: ResumeScope,
        exception: ExceptionDisposition,
    ) -> Result<ExecutionId> {
        self.with_arming_notification(runtime, |controller| {
            let inferior = controller.inferior.as_mut().ok_or(Error::NotRunning)?;
            let number = inferior.next_runtime_step;
            inferior.next_runtime_step = number.saturating_add(1);
            let step = RuntimeStep {
                number,
                kind,
                thread: debug_thread_id(pid),
                frame,
            };
            inferior.runtime_step = Some((runtime, step));
            if let Err(reason) = controller.arm_runtime(runtime) {
                controller.disarm_runtime_step(runtime);
                return Err(Error::FrameStepUnsupported(
                    format!("the runtime cannot step: {reason}").into(),
                ));
            }

            let owner = StepOwner {
                thread: pid,
                task: controller.step_task(pid),
            };
            let begun = controller.begin_execution(
                process_id,
                stop_id,
                scope,
                ActiveKind::RuntimeStep {
                    owner,
                    kind,
                    runtime,
                    number,
                },
                exception,
            );
            if begun.is_err() {
                controller.disarm_runtime_step(runtime);
            }

            begun
        })
    }

    /// Why thread `pid` stops where the runtime `runtime` reports the end
    /// of its step numbered `number`: when that is the active step, and
    /// `pid` runs it.
    pub(super) fn runtime_step_end(
        &self,
        pid: Pid,
        runtime: RuntimeId,
        number: u64,
    ) -> Option<StopReason> {
        let active = self.inferior.as_ref()?.active.as_ref()?;
        match active.kind {
            ActiveKind::RuntimeStep {
                owner,
                kind,
                runtime: stepping,
                number: step,
            } if stepping == runtime && step == number && self.runs_step(owner, pid) => {
                Some(StopReason::Step { kind })
            }
            _ => None,
        }
    }

    /// Disarms the active runtime step, if there is one, as the stop that
    /// ends it forms.
    pub(super) fn end_runtime_step(&mut self) {
        let runtime = self
            .inferior
            .as_ref()
            .and_then(|inferior| inferior.active.as_ref())
            .and_then(|active| match active.kind {
                ActiveKind::RuntimeStep { runtime, .. } => Some(runtime),
                _ => None,
            });
        if let Some(runtime) = runtime {
            self.with_arming_notification(runtime, |controller| {
                controller.disarm_runtime_step(runtime);
            });
        }
    }

    /// Arms the runtime with no step. A runtime that cannot be disarmed
    /// reports its stale step's end to no step.
    #[cfg_attr(
        not(debug_assertions),
        expect(unused_variables, reason = "only recorded")
    )]
    fn disarm_runtime_step(&mut self, runtime: RuntimeId) {
        if let Some(inferior) = self.inferior.as_mut() {
            inferior.runtime_step = None;
        }

        if let Err(reason) = self.arm_runtime(runtime) {
            record!("runtime {runtime} keeps its step: {reason}");
        }
    }
}
