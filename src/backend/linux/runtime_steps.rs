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
    pub(super) fn interpreted_frame(
        &self,
        stop_id: StopId,
        pid: Pid,
        frame: StackFrameId,
    ) -> Option<(RuntimeId, VirtualAddress)> {
        let inferior = self.session.inferior()?;
        let root = self
            .stack_root(stop_id, ExecutionContext::Thread(debug_thread_id(pid)))
            .ok()?;
        if !self.interprets_on(inferior, &root)
            && !self
                .runtimes(inferior)
                .iter()
                .any(|runtime| runtime.model.owns_generated_code())
        {
            return None;
        }

        match self.resolve_frame(inferior, &root, frame).ok()?.scope {
            FrameScope::Interpreted { runtime, frame } => Some((runtime, frame.address)),
            _ => None,
        }
    }

    pub(super) fn runtime_step_origin(
        &self,
        stop_id: StopId,
        pid: Pid,
        frame: StackFrameId,
        kind: StepKind,
    ) -> Option<(RuntimeId, Option<VirtualAddress>)> {
        self.interpreted_frame(stop_id, pid, frame)
            .map(|(runtime, frame)| (runtime, Some(frame)))
            .or_else(|| {
                self.javascript_step_from_native(stop_id, pid, frame, kind)
                    .map(|runtime| (runtime, None))
            })
    }

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
    #[expect(
        clippy::too_many_arguments,
        reason = "a step names its stop, thread, runtime, frame, kind, scope, and exception disposition"
    )]
    pub(super) fn begin_runtime_step(
        &mut self,
        process_id: ProcessId,
        stop_id: StopId,
        pid: Pid,
        (runtime, frame): (RuntimeId, Option<VirtualAddress>),
        kind: StepKind,
        scope: ResumeScope,
        exception: ExceptionDisposition,
    ) -> Result<ExecutionId> {
        let agent = self.session.inferior().is_some_and(|inferior| {
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
            let inferior = controller.session.inferior_mut().ok_or(Error::NotRunning)?;
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

    pub(super) fn runtime_step_end(
        &self,
        pid: Pid,
        runtime: RuntimeId,
        number: u64,
        incomplete: Option<Arc<str>>,
    ) -> Option<StopReason> {
        let active = self.session.inferior()?.run.execution()?;
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
    ) -> bool {
        let Some(inferior) = self.session.inferior() else {
            return false;
        };
        let Some(active) = inferior.run.execution() else {
            return false;
        };
        let ActiveKind::RuntimeStep {
            owner,
            kind: StepKind::IntoSource,
            runtime: stepping,
            number: step,
        } = active.kind
        else {
            return false;
        };
        if stepping != runtime || step != number || !self.runs_step(owner, pid) {
            return false;
        }

        let execution = active.id;
        let Some(destination) = self.native_step_destination(inferior, runtime, target) else {
            record!("the step into {target} leaves it to the runtime");
            return false;
        };

        let Some(inferior) = self.session.inferior_mut() else {
            return false;
        };

        match install_plan_breakpoint(&self.ptrace, inferior, destination, execution) {
            Ok(()) => true,
            Err(error) => {
                record!("the step into {target} cannot trap at {destination}: {error}");
                false
            }
        }
    }

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

    pub(super) fn source_step_returned_to_runtime(&self, pid: Pid, kind: StepKind) -> Result<bool> {
        if !matches!(kind, StepKind::OverSource | StepKind::Out) {
            return Ok(false);
        }
        let inferior = self.session.inferior().ok_or(Error::NotRunning)?;
        let Some(activation) = inferior
            .run
            .execution()
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

    pub(super) fn returned_into_unlined_code(&self, pid: Pid) -> Result<bool> {
        let inferior = self.session.inferior().ok_or(Error::NotRunning)?;
        let Some(activation) = inferior
            .run
            .execution()
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
            .any(|bound| bound.module.id == module.id))
    }

    #[cfg_attr(
        not(debug_assertions),
        expect(unused_variables, reason = "only recorded")
    )]
    pub(super) fn return_to_runtime(&mut self, pid: Pid) {
        let Some(inferior) = self.session.inferior() else {
            return;
        };
        let visible = inferior
            .run
            .barrier()
            .is_some_and(|barrier| barrier.reason().is_some())
            || inferior
                .threads
                .values()
                .any(|thread| thread.reason.is_some());
        let Some((owner, requested)) =
            inferior
                .run
                .execution()
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
        let through = self.returns_through(pid).map(|runtime| (runtime, None));
        let Some((runtime, frame)) = through.or_else(|| {
            self.runtimes(inferior)
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
                    Some((bound.id, Some(frame)))
                })
        }) else {
            record!("thread {thread} returned into a runtime that runs none of its code");
            return;
        };

        self.with_arming_notification(runtime, |controller| {
            let Some(inferior) = controller.session.inferior_mut() else {
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
                .session
                .inferior_mut()
                .and_then(|inferior| inferior.run.execution_mut())
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

    pub(super) fn runtime_step_arrival(
        &self,
        pid: Pid,
        address: VirtualAddress,
    ) -> Option<StepKind> {
        let inferior = self.session.inferior()?;
        let active = inferior.run.execution()?;
        let ActiveKind::RuntimeStep { owner, kind, .. } = active.kind else {
            return None;
        };
        let planned = inferior.breakpoints.get(&address).is_some_and(|site| {
            site.owners
                .contains(&super::BreakpointOwner::Plan(active.id))
        });
        (planned && self.runs_step(owner, pid)).then_some(kind)
    }

    pub(super) fn end_runtime_step(&mut self) {
        let runtime = self
            .session
            .inferior()
            .and_then(|inferior| inferior.run.execution())
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

    #[cfg_attr(
        not(debug_assertions),
        expect(unused_variables, reason = "only recorded")
    )]
    fn disarm_runtime_step(&mut self, runtime: RuntimeId) {
        if let Some(inferior) = self.session.inferior_mut() {
            inferior.runtime_step = None;
        }

        if let Err(reason) = self.arm_runtime(runtime) {
            record!("runtime {runtime} keeps its step: {reason}");
        }
    }
}
