//! Steps through code a runtime runs itself, which have no instructions of
//! their own to step: the runtime is armed with the step, every thread runs
//! as for a continue, and the step ends where the runtime's internal hook
//! reports its end on the step's own thread or task. Another thread's
//! report is not the step's end, as another thread reaching a native
//! step's plan is not. The runtime is disarmed at whatever stop ends the
//! step.

use std::sync::Arc;

use nix::unistd::Pid;

use crate::protocol::{
    ExceptionDisposition, ExecutionId, ProcessId, ResumeScope, StopId, StopReason,
};
use crate::runtime_model::RuntimeStep;
use crate::{
    CodeRole, Error, ExecutionContext, ImageAddress, LoadedModule, ModuleImage, Result, RuntimeId,
    StackFrameId, StepKind, VirtualAddress,
};

use super::breakpoints::install_plan_breakpoint;
use super::frames::FrameScope;
use super::native::{InspectionOps, LinuxTraceOps};
use super::{ActiveKind, Controller, Inferior, StepOwner, debug_thread_id};

impl<P: InspectionOps> Controller<P> {
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

    /// Whether `selection` selects an outer frame a runtime interprets.
    pub(super) fn selects_interpreted(&self, selection: &crate::StoppedSelection) -> bool {
        selection.frame != StackFrameId::INNERMOST
            && self
                .context_thread(selection.stop, selection.execution)
                .is_ok_and(|pid| {
                    self.interpreted_frame(selection.stop, pid, selection.frame)
                        .is_some()
                })
    }
}

impl<P: LinuxTraceOps> Controller<P> {
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
        // An agent binds a step to its frame where it stopped the thread:
        // from anywhere else, the thread would run on unfollowed first.
        let agent = self.inferior.as_ref().is_some_and(|inferior| {
            self.runtimes(inferior)
                .into_iter()
                .any(|bound| bound.id == runtime && bound.model.agent().is_some())
        });
        if agent && !self.at_agent_hook(pid, runtime) {
            return Err(Error::FrameStepUnsupported(
                "the runtime steps from where its agent stopped the thread, as at a breakpoint it reported or a step's end".into(),
            ));
        }

        self.with_arming_notification(runtime, |controller| {
            let inferior = controller.inferior.as_mut().ok_or(Error::NotRunning)?;
            let number = inferior.next_runtime_step;
            inferior.next_runtime_step = number.saturating_add(1);
            let step = RuntimeStep {
                number,
                kind,
                thread: debug_thread_id(pid),
                frame,
                returning: false,
            };
            inferior.runtime_step = Some((runtime, step));
            // An agent has begun the step once it has been sent it.
            if let Err(reason) = controller.arm_runtime(runtime)
                && !controller.agent_step_sent(runtime)
            {
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
    /// of its step numbered `number`, or, with a description, where it could
    /// follow the step no further: when that is the active step, and `pid`
    /// runs it.
    pub(super) fn runtime_step_end(
        &self,
        pid: Pid,
        runtime: RuntimeId,
        number: u64,
        incomplete: Option<Arc<str>>,
    ) -> Option<StopReason> {
        let active = self.inferior.as_ref()?.active.as_ref()?;
        match active.kind {
            ActiveKind::RuntimeStep {
                owner,
                kind,
                runtime: stepping,
                number: step,
            } if stepping == runtime && step == number && self.runs_step(owner, pid) => {
                Some(incomplete.map_or(StopReason::Step { kind }, |description| {
                    StopReason::StepIncomplete { kind, description }
                }))
            }
            _ => None,
        }
    }

    /// Lets the active runtime step into numbered `number` end in the native
    /// function at `target`, which its thread `pid` is about to call: a trap
    /// at the function's first statement joins the step's plan, which the
    /// step's own thread ends it at. Code of the runtime's own image, or
    /// without debug information, is the runtime's work, and the step goes
    /// on through it.
    #[cfg_attr(
        not(debug_assertions),
        expect(unused_variables, reason = "only recorded")
    )]
    pub(super) fn step_into_native(
        &mut self,
        pid: Pid,
        runtime: RuntimeId,
        number: u64,
        target: VirtualAddress,
    ) {
        let Some(inferior) = self.inferior.as_ref() else {
            return;
        };
        let Some(active) = inferior.active.as_ref() else {
            return;
        };
        let ActiveKind::RuntimeStep {
            owner,
            kind: StepKind::IntoSource,
            runtime: stepping,
            number: step,
        } = active.kind
        else {
            return;
        };
        if stepping != runtime || step != number || !self.runs_step(owner, pid) {
            return;
        }

        let execution = active.id;
        let Some(destination) = self.native_step_destination(inferior, runtime, target) else {
            record!("the step into {target} leaves it to the runtime");
            return;
        };
        let Some(inferior) = self.inferior.as_mut() else {
            return;
        };
        if let Err(error) = install_plan_breakpoint(&self.ptrace, inferior, destination, execution)
        {
            record!("the step into {target} cannot trap at {destination}: {error}");
        }
    }

    /// Where a step into the native function at `target` ends: the first
    /// statement after its prologue, when a module other than the
    /// runtime's describes it.
    fn native_step_destination(
        &self,
        inferior: &Inferior,
        runtime: RuntimeId,
        target: VirtualAddress,
    ) -> Option<VirtualAddress> {
        let own = self
            .runtimes(inferior)
            .into_iter()
            .find(|bound| bound.id == runtime)?
            .module
            .id;
        let (module, image, address) = self.holding(inferior, target)?;
        if module.id == own {
            return None;
        }
        let instance = image.locate(address).physical_instance?;
        let entry = image.recommended_entries_for_instance(instance).next()?;
        module.virtual_address(entry.address).ok()
    }

    /// The loaded module whose image holds `address`, its image, and the
    /// address within it.
    fn holding<'a>(
        &'a self,
        inferior: &'a Inferior,
        address: VirtualAddress,
    ) -> Option<(LoadedModule, &'a Arc<ModuleImage>, ImageAddress)> {
        self.loaded_images(inferior).find_map(|(module, image)| {
            let held = module
                .image_address(address)
                .ok()
                .filter(|held| image.contains_address(*held))?;
            Some((module, image, held))
        })
    }

    /// Whether the active source step over or out of a native frame has
    /// returned, on its thread `pid`, into the interpreter of a runtime
    /// whose agent steps its code: the native code was the runtime's
    /// callee, and the step goes on as the runtime's step back to the code
    /// that called it.
    pub(super) fn source_step_returned_to_runtime(&self, pid: Pid, kind: StepKind) -> Result<bool> {
        if !matches!(kind, StepKind::OverSource | StepKind::Out) {
            return Ok(false);
        }
        let inferior = self.inferior.as_ref().ok_or(Error::NotRunning)?;
        let Some(activation) = inferior
            .active
            .as_ref()
            .and_then(|active| match &active.kind {
                ActiveKind::Step { start, .. } if !start.running_on => start.activation,
                _ => None,
            })
        else {
            return Ok(false);
        };

        let registers = self.ptrace.registers(pid)?;
        if !activation.has_returned(self.stack_position(pid, &registers)) {
            return Ok(false);
        }
        let Some((module, _, _)) = self.holding(inferior, VirtualAddress::new(registers.rip))
        else {
            return Ok(false);
        };
        Ok(self
            .runtimes(inferior)
            .iter()
            .any(|bound| bound.module.id == module.id && bound.model.agent().is_some()))
    }

    /// Turns the active source step of thread `pid`, which returned into a
    /// runtime's interpreter (see [`Self::source_step_returned_to_runtime`]),
    /// into that runtime's step back from the thread's innermost
    /// interpreted frame, which the agent ends at the thread's next line
    /// there or in a caller. The step goes on unfollowed, as into any code
    /// without source, where the runtime cannot step, and at a stop that
    /// ends it anyway.
    #[cfg_attr(
        not(debug_assertions),
        expect(unused_variables, reason = "only recorded")
    )]
    pub(super) fn return_to_runtime(&mut self, pid: Pid) {
        let Some(inferior) = self.inferior.as_ref() else {
            return;
        };
        let visible = inferior
            .barrier
            .as_ref()
            .is_some_and(|barrier| barrier.reason.is_some())
            || inferior
                .threads
                .values()
                .any(|thread| thread.reason.is_some());
        let Some((owner, requested)) =
            inferior
                .active
                .as_ref()
                .and_then(|active| match &active.kind {
                    ActiveKind::Step {
                        owner, requested, ..
                    } if !visible && self.runs_step(*owner, pid) => Some((*owner, *requested)),
                    _ => None,
                })
        else {
            return;
        };

        let thread = debug_thread_id(pid);
        let Some((runtime, frame)) = self
            .runtimes(inferior)
            .into_iter()
            .filter(|bound| bound.model.agent().is_some())
            .find_map(|bound| {
                let stacks = self.with_runtime_stop(inferior, &bound, pid, |stop| {
                    bound.model.interpreted(stop)
                })?;
                let on = stacks.value.threads.iter().find(|on| on.thread == thread)?;
                let frame = stacks
                    .value
                    .frames_of(on)
                    .iter()
                    .find(|frame| frame.role == CodeRole::Ordinary)?
                    .frame;
                Some((bound.id, frame))
            })
        else {
            record!("thread {thread} returned into a runtime that runs none of its code");
            return;
        };

        self.with_arming_notification(runtime, |controller| {
            let Some(inferior) = controller.inferior.as_mut() else {
                return;
            };
            let number = inferior.next_runtime_step;
            inferior.next_runtime_step = number.saturating_add(1);
            inferior.runtime_step = Some((
                runtime,
                RuntimeStep {
                    number,
                    kind: requested,
                    thread,
                    frame,
                    returning: true,
                },
            ));
            if let Err(reason) = controller.arm_runtime(runtime)
                && !controller.agent_step_sent(runtime)
            {
                record!("the runtime cannot step back: {reason}");
                controller.disarm_runtime_step(runtime);
                return;
            }
            if let Some(active) = controller
                .inferior
                .as_mut()
                .and_then(|inferior| inferior.active.as_mut())
            {
                active.kind = ActiveKind::RuntimeStep {
                    owner,
                    kind: requested,
                    runtime,
                    number,
                };
            }
        });
    }

    /// The kind of the active runtime step whose plan has a trap at
    /// `address`, where its own thread `pid` reached it.
    pub(super) fn runtime_step_arrival(
        &self,
        pid: Pid,
        address: VirtualAddress,
    ) -> Option<StepKind> {
        let inferior = self.inferior.as_ref()?;
        let active = inferior.active.as_ref()?;
        let ActiveKind::RuntimeStep { owner, kind, .. } = active.kind else {
            return None;
        };
        let planned = inferior.breakpoints.get(&address).is_some_and(|site| {
            site.owners
                .contains(&super::BreakpointOwner::Plan(active.id))
        });
        (planned && self.runs_step(owner, pid)).then_some(kind)
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
