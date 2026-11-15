use std::collections::BTreeSet;
use std::sync::Arc;

use nix::errno::Errno;
use nix::libc;
use nix::unistd::Pid;

use crate::protocol::{
    DebuggerEvent, ExecutionId, FramePresentation, PresentedFrame, ProcessId, StepKind, StopId,
    StopReason,
};

use crate::unwind::{CallerProvider, CallerResult, DEFAULT_MAX_FRAMES, FrameContext};
use crate::{
    CodeInstanceId, CodeInstanceKind, CodeRole, Error, ImageLocation, InlineFrameLookup,
    LoadedModule, ModuleImage, Result, SourceLocation, StackFrameId, StackSegment, TaskId,
    ThreadActivity, VirtualAddress,
};

use super::activation::{Activation, StackPosition, StackView};
use super::awaits::Followed;
use super::breakpoints::install_plan_breakpoint;
use super::frames::{
    DwarfCallerProvider, RoleCallerProvider, StackRoot, code_instance_is_active,
    frame_lookup_address, make_presentation, presentation_visible_count, selected_code_instance,
    source_for_code_instance, source_line_changed, source_step_destination,
};
use super::trap::Trap;

use super::loops::{StepLoops, inline_loop_step_is_complete};
use super::native::LinuxTraceOps;
use super::registers::x86_64_registers;
use super::returns::Returning;
use crate::disassembly::{AssemblySyntax, ControlFlow, RawDecode, decoder_for};

use super::memory::read_logical_memory;
use super::{
    ActiveKind, BreakpointOwner, Controller, Edit, EpilogueTraversal, ExpectedStop, Inferior,
    LinuxError, NativeThreadState, Resume, ReturnTraversal, StepStart, backend_error,
    debug_thread_id, is_superseded, process_id, steps_instructions, validate_process,
    validate_public_stop, validate_resumable, validate_stopped_thread,
};

impl<P: LinuxTraceOps> Controller<P> {
    pub(super) fn active_execution(&self) -> Result<ExecutionId> {
        self.session
            .inferior()
            .and_then(|inferior| inferior.run.execution())
            .map(|active| active.id)
            .ok_or(Error::NotRunning)
    }

    pub(super) fn is_advance_target(&self, address: VirtualAddress) -> bool {
        self.session
            .inferior()
            .and_then(|inferior| inferior.run.execution())
            .is_some_and(|active| {
                matches!(&active.kind, ActiveKind::Step { start, .. } if start.targets.contains(&address))
            })
    }

    pub(super) fn take_standing_arrival(&mut self, address: VirtualAddress) -> bool {
        self.active_step_mut()
            .is_some_and(|start| start.standing.take_if(|at| *at == address).is_some())
    }

    pub(super) fn retire_step_plan(&mut self, execution: ExecutionId) -> Result<()> {
        let targets = self
            .active_step_mut()
            .map(|start| start.targets.clone())
            .unwrap_or_default();
        if targets.is_empty() {
            return self.cleanup_plan_breakpoints(execution);
        }

        let owner = BreakpointOwner::Plan(execution);
        let inferior = self.session.inferior().ok_or(Error::NotRunning)?;
        let retired = inferior
            .plan_sites
            .get(&execution)
            .into_iter()
            .flatten()
            .copied()
            .filter(|address| {
                !targets.contains(address)
                    && inferior
                        .breakpoints
                        .get(address)
                        .is_some_and(|site| site.owners.contains(&owner))
            })
            .collect::<Vec<_>>();
        for address in retired {
            self.remove_breakpoint_owner(address, owner)?;
        }

        if let Some(sites) = self
            .session
            .inferior_mut()
            .and_then(|inferior| inferior.plan_sites.get_mut(&execution))
        {
            sites.retain(|address| targets.contains(address));
        }

        Ok(())
    }

    pub(super) fn active_step_mut(&mut self) -> Option<&mut StepStart> {
        match &mut self.session.inferior_mut()?.run.execution_mut()?.kind {
            ActiveKind::Step { start, .. } => Some(start),
            _ => None,
        }
    }

    pub(super) fn try_virtual_step(
        &mut self,
        requested_process: ProcessId,
        stop_id: StopId,
        pid: Pid,
        kind: StepKind,
    ) -> Result<Option<(ExecutionId, DebuggerEvent)>> {
        if kind != StepKind::IntoSource {
            return Ok(None);
        }

        let (process_id, execution_id, next_stop_id, presentation) = {
            let inferior = self.session.inferior().ok_or(Error::NotRunning)?;
            validate_process(inferior, requested_process)?;
            validate_public_stop(inferior, Some(stop_id))?;
            validate_resumable(inferior)?;
            validate_stopped_thread(inferior, pid)?;
            let stop = inferior.run.stop().ok_or(Error::NotStopped)?;
            let Some(current) = stop.presentations.get(&pid) else {
                return Ok(None);
            };

            if current.hidden_inline_frames == 0
                || matches!(current.frame, PresentedFrame::Ambiguous(_))
            {
                return Ok(None);
            }

            let image_address = inferior.loaded_module.image_address(current.instruction)?;
            let location = self.main.image.locate(image_address);
            let InlineFrameLookup::Unique(chain) = &location.inline_frames else {
                return Err(Error::AmbiguousInlineFrame);
            };

            let visible = presentation_visible_count(&location, current)?;
            let presentation =
                make_presentation(current.instruction, chain.instances.as_ref(), visible + 1)?;

            (
                process_id(inferior.tgid),
                ExecutionId::new(inferior.next_execution + 1),
                self.ptrace.allocate_stop_id()?,
                presentation,
            )
        };

        let inferior = self.session.inferior_mut().ok_or(Error::NotRunning)?;
        let stop = inferior.run.stop_mut().ok_or(Error::NotStopped)?;
        inferior.next_execution = execution_id.get();
        stop.id = next_stop_id;
        stop.triggering_thread = pid;
        stop.selected = crate::ExecutionContext::Thread(debug_thread_id(pid));
        stop.selected_thread = Some(pid);
        stop.selected_frames.clear();
        stop.returned = None;
        stop.reason = StopReason::Step { kind };
        stop.presentations.insert(pid, presentation);
        inferior.thread_mut(pid)?.reason = Some(StopReason::Step { kind });

        self.bump_revision();
        let stopped = DebuggerEvent::InferiorStopped {
            revision: self.revision,
            process_id,
            execution_id: Some(execution_id),
            stop_id: next_stop_id,
            thread_id: debug_thread_id(pid),
            reason: StopReason::Step { kind },
        };

        Ok(Some((execution_id, stopped)))
    }
}

impl<P: LinuxTraceOps> Controller<P> {
    pub(super) fn start_user_step(&mut self, pid: Pid, kind: StepKind) -> Result<()> {
        let uses_plan_breakpoints = self
            .session
            .inferior()
            .and_then(|inferior| inferior.run.execution())
            .is_some_and(|active| {
                matches!(
                    &active.kind,
                    ActiveKind::Step { start, .. }
                        if !start.plan_addresses.is_empty()
                            || start.resume_guard.is_some()
                            || start.escape.is_some()
                )
            });
        if uses_plan_breakpoints {
            return self.continue_thread(pid);
        }

        let rip = VirtualAddress::new(self.ptrace.registers(pid)?.rip);
        if !self.before_breakpoint(pid, rip)? && self.pass_unchosen_call(pid)? {
            return Ok(());
        }

        let mode = self.step_mode(kind);
        if matches!(mode, StepKind::IntoSource | StepKind::OverSource)
            && (self.stopped_outside_described_code(pid)? || self.stopped_where_step_leaves(pid)?)
            && self.escape_undescribed_code(pid, mode == StepKind::IntoSource)?
        {
            return Ok(());
        }

        self.resume_native(pid, Resume::Step, true, ExpectedStop::UserStep { kind })
    }

    pub(super) fn stopped_outside_described_code(&self, pid: Pid) -> Result<bool> {
        let registers = self.ptrace.registers(pid)?;
        Ok(self
            .located(VirtualAddress::new(registers.rip))
            .is_none_or(|located| undescribed(&located.location)))
    }

    pub(super) fn stopped_where_step_leaves(&self, pid: Pid) -> Result<bool> {
        let registers = self.ptrace.registers(pid)?;
        let enters_runtime = self.step_enters_runtime();
        Ok(match self.code_role(VirtualAddress::new(registers.rip)) {
            Some(CodeRole::StackSwitch) => true,
            Some(role) => is_runtime_role(role) && !enters_runtime,
            None => false,
        })
    }

    fn step_enters_runtime(&self) -> bool {
        self.session
            .inferior()
            .and_then(|inferior| inferior.run.execution())
            .is_some_and(|active| {
                matches!(&active.kind, ActiveKind::Step { start, .. } if start.enters_runtime)
            })
    }

    pub(super) fn escape_undescribed_code(&mut self, pid: Pid, into: bool) -> Result<bool> {
        let registers = self.ptrace.registers(pid)?;
        let call_out = into.then(|| self.call_out(pid, &registers)).flatten();
        let candidate = match self.caller_address(pid, &registers) {
            Ok(address) => address,
            Err(_) => match self.ptrace.read_word(pid, registers.rsp) {
                Ok(word) => VirtualAddress::new(word),
                Err(_) => return Ok(false),
            },
        };

        let described = self
            .located(candidate)
            .is_some_and(|located| located.location.physical_instance.is_some());
        if !described {
            return Ok(false);
        }

        let execution = self.active_execution()?;
        let inferior = self.session.inferior_mut().ok_or(Error::NotRunning)?;
        install_plan_breakpoint(&self.ptrace, inferior, candidate, execution)?;
        if let Some(target) = call_out {
            install_plan_breakpoint(&self.ptrace, inferior, target, execution)?;
        }

        if let Some(start) = self.active_step_mut() {
            start.escape = Some(candidate);
        }

        self.continue_thread(pid)?;
        Ok(true)
    }
}

impl<P: LinuxTraceOps> Controller<P> {
    pub(super) fn complete_user_step(&mut self, pid: Pid, kind: StepKind) -> Result<()> {
        if let Some(description) = self.task_left_step(pid, kind) {
            record!("step {kind:?} lost its task: {description}");
            return self.begin_visible_stop(
                pid,
                StopReason::StepIncomplete {
                    kind,
                    description: description.into(),
                },
            );
        }

        match self.advance_user_step(pid, kind) {
            Err(error) if is_lost_step_evidence(&error) && self.thread_is_stopped(pid) => {
                self.check_still_stopped(pid)?;
                self.begin_visible_stop(pid, step_incomplete(kind, &error))
            }
            result => result,
        }
    }

    fn check_still_stopped(&self, pid: Pid) -> Result<()> {
        if is_superseded(&self.ptrace.signal_metadata(pid)) {
            return Err(backend_error(LinuxError::System(Errno::ESRCH)));
        }

        Ok(())
    }

    pub(super) fn user_step_stop(&self, pid: Pid, kind: StepKind) -> Result<Option<StopReason>> {
        self.judge_user_step(pid, kind).map(|(reason, _)| reason)
    }

    fn judge_user_step(
        &self,
        pid: Pid,
        kind: StepKind,
    ) -> Result<(Option<StopReason>, Option<VirtualAddress>)> {
        match self.step_completion(pid, kind) {
            Ok(Completion::Complete) => Ok((Some(StopReason::Step { kind }), None)),
            Ok(Completion::Incomplete) => Ok((None, None)),
            Ok(Completion::AtTrap(address)) => Ok((None, Some(address))),
            Err(error) if is_lost_step_evidence(&error) => {
                self.check_still_stopped(pid)?;
                Ok((Some(step_incomplete(kind, &error)), None))
            }
            Err(error) => Err(error),
        }
    }

    pub(super) fn step_mode(&self, kind: StepKind) -> StepKind {
        let following = self
            .session
            .inferior()
            .and_then(|inferior| inferior.run.execution())
            .is_some_and(
                |active| matches!(&active.kind, ActiveKind::Step { start, .. } if start.following),
            );
        if following {
            StepKind::IntoSource
        } else {
            kind
        }
    }

    pub(super) fn panic_entries(&self, inferior: &Inferior) -> BTreeSet<VirtualAddress> {
        self.unwind_modules(inferior)
            .into_iter()
            .flat_map(|module| {
                module
                    .image
                    .panic_entries()
                    .iter()
                    .filter_map(|&entry| module.loaded.virtual_address(entry).ok())
                    .collect::<Vec<_>>()
            })
            .collect()
    }

    pub(super) fn begin_following(&mut self, pid: Pid, kind: StepKind) -> Result<bool> {
        if !matches!(kind, StepKind::OverSource | StepKind::Out) {
            return Ok(false);
        }

        let Some((following, frame, inlined, start_module)) = self
            .session
            .inferior()
            .and_then(|inferior| inferior.run.execution())
            .and_then(|active| match &active.kind {
                ActiveKind::Step { owner, start, .. } if self.runs_step(*owner, pid) => Some((
                    start.following,
                    start.returned_to.or(start.activation),
                    start
                        .code_instance
                        .filter(|instance| start.physical_instance != Some(*instance))
                        .zip(start.physical_instance),
                    start.module,
                )),
                _ => None,
            })
        else {
            return Ok(false);
        };

        if following {
            return Ok(false);
        }

        let registers = self.ptrace.registers(pid)?;
        let position = self.stack_position(pid, &registers);
        let Some(Located {
            module, location, ..
        }) = self.located(VirtualAddress::new(registers.rip))
        else {
            return Ok(false);
        };

        let inlined = inlined.filter(|_| {
            self.session.inferior().is_some_and(|inferior| {
                start_module.unwrap_or(inferior.loaded_module).id == module.id
            })
        });

        let role = self
            .code_role(VirtualAddress::new(registers.rip))
            .unwrap_or_default();
        let returned = frame.is_some_and(|frame| frame.has_returned(position));

        let left_inlined = inlined.is_some_and(|(instance, physical)| {
            location.physical_instance == Some(physical)
                && !code_instance_is_active(&location, instance)
        });

        let pending = left_inlined
            && self.active_step().is_some_and(|start| {
                self.left_inlined_body_pending(pid, start, VirtualAddress::new(registers.rip))
            });
        let enters = match role {
            CodeRole::Panic => true,
            CodeRole::Wrapper => returned || left_inlined && !pending,
            _ => false,
        };

        if !enters {
            return Ok(false);
        }

        record!(
            "step {kind:?} follows the runtime's calls from {}",
            location
                .function
                .as_ref()
                .map_or("unnamed code", |function| function.name.as_ref())
        );

        let execution = self.active_execution()?;
        self.cleanup_plan_breakpoints(execution)?;
        let guards = self.frame_return_guards(pid);
        self.install_additional_plan_breakpoints(execution, &guards)?;
        let activation = self.top_activation(pid, &registers).ok();
        let start = self
            .active_step_mut()
            .ok_or_else(|| Error::internal("the step remained active while it began following"))?;
        *start = StepStart {
            module: Some(module),
            source: location.source.clone(),
            code_instance: location.physical_instance,
            physical_instance: location.physical_instance,
            activation,
            stack_pointer: Some(position),
            following: true,
            ..StepStart::default()
        };

        Ok(true)
    }

    fn frame_return_guards(&self, pid: Pid) -> BTreeSet<VirtualAddress> {
        let Some(inferior) = self.session.inferior() else {
            return BTreeSet::new();
        };

        let Ok(stack) =
            self.physical_stack(inferior, &StackRoot::of_thread(pid), DEFAULT_MAX_FRAMES)
        else {
            return BTreeSet::new();
        };

        stack
            .frames
            .iter()
            .skip(1)
            .map(|frame| frame.context.instruction)
            .filter(|address| {
                self.located(*address)
                    .is_some_and(|located| located.location.physical_instance.is_some())
                    && self.code_role(*address) == Some(CodeRole::Ordinary)
            })
            .collect()
    }

    pub(super) fn step_task(&self, pid: Pid) -> Option<TaskId> {
        match self.thread_activity(self.session.inferior()?, pid)? {
            ThreadActivity::Task {
                task,
                stack: StackSegment::Task,
            } => Some(task),
            _ => None,
        }
    }

    fn task_left_step(&self, pid: Pid, kind: StepKind) -> Option<String> {
        if steps_instructions(kind) {
            return None;
        }

        let inferior = self.session.inferior()?;
        let ActiveKind::Step { owner, .. } = &inferior.run.execution()?.kind else {
            return None;
        };

        let task = owner.task?;
        if self.runs_step(*owner, pid) {
            return None;
        }

        let noun = self
            .runtimes(inferior)
            .into_iter()
            .find(|runtime| runtime.id == task.runtime)
            .map_or("task", |runtime| runtime.model.task_noun());
        Some(format!(
            "{noun} {} stopped running on thread {pid} during the step",
            task.number
        ))
    }

    pub(super) fn follow_step(&mut self, pid: Pid) {
        if let Some(ActiveKind::Step { owner, .. }) = self
            .session
            .inferior_mut()
            .and_then(|inferior| inferior.run.execution_mut())
            .map(|active| &mut active.kind)
            && owner.thread != pid
        {
            record!(
                "step follows its task from thread {} to thread {pid}",
                owner.thread
            );
            owner.thread = pid;
        }
    }

    fn thread_is_stopped(&self, pid: Pid) -> bool {
        self.session
            .inferior()
            .and_then(|inferior| inferior.threads.get(&pid))
            .is_some_and(|thread| matches!(thread.state, NativeThreadState::Stopped))
    }

    fn advance_user_step(&mut self, pid: Pid, kind: StepKind) -> Result<()> {
        if let Some(start) = self.active_step_mut() {
            start.judged_by_trap = None;
        }

        match self.follow_poll_return(pid, kind)? {
            Some(Followed::Waits) => return self.continue_thread(pid),
            Some(Followed::Ended(reason)) => return self.begin_visible_stop(pid, reason),
            None => {}
        }

        self.note_returned_values(pid);
        self.retire_return_guard()?;
        self.retire_epilogue_return_guard()?;
        let mode = self.step_mode(kind);
        self.note_returned_activation(pid, mode)?;
        if self.begin_following(pid, kind)? || self.wait_for_loop(pid, kind)? {
            return self.start_user_step(pid, kind);
        }

        self.note_loop_progress(pid)?;
        self.retire_returned_plan(pid)?;
        if !steps_instructions(kind) && self.begin_epilogue_traversal(pid)? {
            return self.start_user_step(pid, kind);
        }

        if self.source_step_returned_to_runtime(pid, mode)? {
            self.let_step_run_on()?;
            self.queue_internal_edit(Edit::ReturnToRuntime { pid })?;
            return self.finish_barrier_if_ready();
        }

        if self.source_step_returned_to_undescribed_code(pid, mode)? {
            self.let_step_run_on()?;
            return self.continue_thread(pid);
        }

        if matches!(mode, StepKind::OverSource | StepKind::Out) && !self.step_frame_returned() {
            match self.begin_return_traversal(pid) {
                Ok(true) => return self.start_user_step(pid, kind),
                Ok(false) => {}

                Err(error) if is_caller_unavailable(&error) => {
                    return self.start_user_step(pid, kind);
                }
                Err(error) => return Err(error),
            }
        }

        let (reason, at_trap) = self.judge_user_step(pid, kind)?;
        if let Some(reason) = reason {
            return self.begin_visible_stop(pid, reason);
        }

        if let Some(start) = self.active_step_mut() {
            start.judged_by_trap = at_trap;
        }

        self.guard_returned_callee(pid)?;
        self.start_user_step(pid, kind)
    }

    pub(super) fn arrival_left_to_a_lost_trap(&self, pid: Pid) -> Result<bool> {
        let Some(address) = self
            .session
            .inferior()
            .and_then(|inferior| inferior.run.execution())
            .and_then(|active| match &active.kind {
                ActiveKind::Step { start, .. } => start.judged_by_trap,
                _ => None,
            })
        else {
            return Ok(false);
        };

        Ok(self.ptrace.registers(pid)?.rip == address.get()
            && !self.before_breakpoint(pid, address)?)
    }

    pub(super) fn forget_trap_judgement(&mut self, pid: Pid, address: VirtualAddress) {
        let owner = self
            .session
            .inferior()
            .and_then(|inferior| inferior.run.execution())
            .and_then(|active| match &active.kind {
                ActiveKind::Step { owner, start, .. } if start.judged_by_trap == Some(address) => {
                    Some(*owner)
                }
                _ => None,
            });
        if owner.is_some_and(|owner| self.runs_step(owner, pid))
            && let Some(start) = self.active_step_mut()
        {
            start.judged_by_trap = None;
        }
    }

    fn guard_returned_callee(&mut self, pid: Pid) -> Result<()> {
        let plan = self
            .session
            .inferior()
            .and_then(|inferior| inferior.run.execution())
            .and_then(|active| match &active.kind {
                ActiveKind::Step {
                    owner,
                    kind: StepKind::OverSource | StepKind::Out,
                    start,
                    ..
                } if self.runs_step(*owner, pid) && start.plan_addresses.is_empty() => {
                    start.returned_to.map(|caller| (active.id, caller))
                }
                _ => None,
            });
        let Some((execution, caller)) = plan else {
            return Ok(());
        };

        let registers = self.ptrace.registers(pid)?;
        let Ok(cfa) = self.top_cfa(pid, &registers) else {
            return Ok(());
        };

        if !self.stack_view(pid).activation(cfa).is_callee_of(caller) {
            return Ok(());
        }

        let Some(slot) = cfa.get().checked_sub(8) else {
            return Ok(());
        };

        let (Ok(return_address), Ok(stacked)) = (
            self.caller_address(pid, &registers),
            self.ptrace.read_word(pid, slot),
        ) else {
            return Ok(());
        };

        if return_address.get() != stacked {
            return Ok(());
        }

        let guard = BTreeSet::from([return_address]);
        self.install_additional_plan_breakpoints(execution, &guard)?;
        if let Some(start) = self.active_step_mut() {
            start.plan_addresses.extend(guard);
        }

        Ok(())
    }

    fn step_frame_returned(&self) -> bool {
        self.session
            .inferior()
            .and_then(|inferior| inferior.run.execution())
            .is_some_and(|active| {
                matches!(&active.kind, ActiveKind::Step { start, .. } if start.returned_to.is_some())
            })
    }

    pub(super) fn note_returned_activation(&mut self, pid: Pid, kind: StepKind) -> Result<()> {
        if !matches!(kind, StepKind::OverSource | StepKind::Out) {
            return Ok(());
        }

        let activation = self
            .session
            .inferior()
            .and_then(|inferior| inferior.run.execution())
            .and_then(|active| match &active.kind {
                ActiveKind::Step { owner, start, .. }
                    if self.runs_step(*owner, pid)
                        && (kind == StepKind::Out || start.code_instance.is_some())
                        && !start.running_on
                        && start.epilogue_traversal.is_none()
                        && start.return_traversal.is_none() =>
                {
                    start.returned_to.or(start.activation)
                }
                _ => None,
            });
        let Some(activation) = activation else {
            return Ok(());
        };

        let registers = self.ptrace.registers(pid)?;
        if !activation.has_returned(self.stack_position(pid, &registers)) {
            return Ok(());
        }

        let Ok(caller) = self.top_activation(pid, &registers) else {
            return Ok(());
        };

        if let Some(start) = self.active_step_mut() {
            start.returned_to = Some(caller);
        }

        Ok(())
    }

    fn retire_returned_plan(&mut self, pid: Pid) -> Result<()> {
        let plan = self
            .session
            .inferior()
            .and_then(|inferior| inferior.run.execution())
            .and_then(|active| match &active.kind {
                ActiveKind::Step { start, .. } if !start.plan_addresses.is_empty() => {
                    start.returned_to.map(|caller| (active.id, caller))
                }
                _ => None,
            });
        let Some((execution, caller)) = plan else {
            return Ok(());
        };

        let registers = self.ptrace.registers(pid)?;
        if self
            .top_activation(pid, &registers)
            .is_ok_and(|current| current.is_callee_of(caller))
        {
            return Ok(());
        }

        self.retire_step_plan(execution)?;

        if let Some(start) = self.active_step_mut() {
            start.plan_addresses.clear();
        }

        Ok(())
    }

    pub(super) fn let_step_run_on(&mut self) -> Result<()> {
        let execution = self.active_execution()?;
        self.retire_step_plan(execution)?;

        if let Some(start) = self.active_step_mut() {
            start.running_on = true;
            start.plan_addresses.clear();
        }

        Ok(())
    }

    pub(super) fn source_step_returned_to_undescribed_code(
        &self,
        pid: Pid,
        kind: StepKind,
    ) -> Result<bool> {
        if !matches!(kind, StepKind::OverSource | StepKind::Out) {
            return Ok(false);
        }

        let start = self
            .session
            .inferior()
            .and_then(|inferior| inferior.run.execution())
            .and_then(|active| match &active.kind {
                ActiveKind::Step { start, .. } => Some(start),
                _ => None,
            })
            .ok_or(Error::LocationUnavailable)?;

        let Some(activation) = start.activation.filter(|_| !start.running_on) else {
            return Ok(false);
        };

        let registers = self.ptrace.registers(pid)?;

        Ok(
            activation.has_returned(self.stack_position(pid, &registers))
                && self
                    .located(VirtualAddress::new(registers.rip))
                    .is_none_or(|located| undescribed(&located.location)),
        )
    }

    pub(super) fn begin_epilogue_traversal(&mut self, pid: Pid) -> Result<bool> {
        let (execution, already_traversing, start_source, start_module, kind, activation) = self
            .session
            .inferior()
            .and_then(|inferior| inferior.run.execution())
            .and_then(|active| match &active.kind {
                ActiveKind::Step {
                    owner, start, kind, ..
                } if self.runs_step(*owner, pid) => Some((
                    active.id,
                    start.epilogue_traversal.is_some() || start.return_traversal.is_some(),
                    start.source.clone(),
                    start.module,
                    if start.following {
                        StepKind::IntoSource
                    } else {
                        *kind
                    },
                    start.activation,
                )),
                _ => None,
            })
            .ok_or(Error::NotRunning)?;
        if already_traversing || !matches!(kind, StepKind::IntoSource | StepKind::OverSource) {
            return Ok(false);
        }

        let registers = self.ptrace.registers(pid)?;
        if let Some(activation) = activation
            && kind == StepKind::OverSource
            && self
                .top_activation(pid, &registers)
                .is_ok_and(|current| current.is_callee_of(activation))
        {
            return Ok(false);
        }

        let instruction = VirtualAddress::new(registers.rip);
        let Some(Located {
            image, location, ..
        }) = self.located(instruction)
        else {
            return Ok(false);
        };

        if location.physical_instance.is_none()
            || !image
                .control_boundaries_at(location.address)
                .any(|row| row.flags.epilogue_begin())
        {
            return Ok(false);
        }

        let Ok(return_address) = self.caller_address(pid, &registers) else {
            return Ok(false);
        };

        let passed_over = self.code_role(return_address).is_some_and(|role| {
            self.active_step()
                .is_some_and(|start| passes_over(role, start))
        });
        if passed_over {
            return Ok(false);
        }

        let Some(caller) = self.located(return_address) else {
            return Ok(false);
        };

        let main = self
            .session
            .inferior()
            .ok_or(Error::NotRunning)?
            .loaded_module;
        let start_source =
            start_source.filter(|_| start_module.unwrap_or(main).id == caller.module.id);
        let completion_addresses = caller_statements(&caller, start_source.as_ref())?;

        let mut plan_addresses = completion_addresses.clone();
        plan_addresses.insert(return_address);
        self.install_additional_plan_breakpoints(execution, &plan_addresses)?;

        let start = self.active_step_mut().ok_or_else(|| {
            Error::internal("source step remained active while installing its epilogue plan")
        })?;
        start.plan_addresses.extend(plan_addresses);
        start.epilogue_traversal = Some(EpilogueTraversal {
            return_address,
            completion_addresses,
            retire_return_after_repair: false,
        });

        Ok(true)
    }

    pub(super) fn begin_return_traversal(&mut self, pid: Pid) -> Result<bool> {
        let (execution, already_traversing, activation, start_instance, start_physical, start) =
            self.session
                .inferior()
                .and_then(|inferior| inferior.run.execution())
                .and_then(|active| match &active.kind {
                    ActiveKind::Step { owner, start, .. } if self.runs_step(*owner, pid) => Some((
                        active.id,
                        start.return_traversal.is_some() || start.epilogue_traversal.is_some(),
                        start.activation,
                        start.code_instance,
                        start.physical_instance,
                        start,
                    )),
                    _ => None,
                })
                .ok_or(Error::NotRunning)?;
        if already_traversing {
            return Ok(false);
        }

        let (Some(activation), Some(start_instance), Some(start_physical)) =
            (activation, start_instance, start_physical)
        else {
            return Ok(false);
        };

        let registers = self.ptrace.registers(pid)?;
        if activation.has_returned(self.stack_position(pid, &registers)) {
            return Ok(false);
        }

        let Some(starting_activation_location) =
            self.located_activation(pid, &registers, activation)?
        else {
            return Ok(false);
        };

        let Ok(current_activation) = self.top_activation(pid, &registers) else {
            return Ok(false);
        };

        let same_module = self.in_step_module(start, &starting_activation_location);
        let tail_replacement = starting_activation_location
            .location
            .physical_instance
            .is_some_and(|current_physical| !same_module || current_physical != start_physical);
        let selected_is_inline = self
            .step_code(start)
            .and_then(|(_, image)| image.code_instance(start_instance))
            .is_some_and(|instance| matches!(instance.kind(), CodeInstanceKind::Inline { .. }));
        let entered_nested_callee =
            selected_is_inline && current_activation.is_callee_of(activation);
        let guarded_activation = if tail_replacement {
            activation
        } else if entered_nested_callee {
            current_activation
        } else {
            return Ok(false);
        };

        let Some(return_slot) = self
            .stack_view(pid)
            .cfa_of(guarded_activation)
            .and_then(|cfa| cfa.get().checked_sub(8))
        else {
            return Ok(false);
        };

        let (Ok(cfi_return), Ok(stack_return)) = (
            self.caller_address(pid, &registers),
            self.ptrace.read_word(pid, return_slot),
        ) else {
            return Ok(false);
        };

        let stack_return = VirtualAddress::new(stack_return);
        if cfi_return != stack_return {
            return Ok(false);
        }

        let plan_addresses = BTreeSet::from([cfi_return]);
        self.install_additional_plan_breakpoints(execution, &plan_addresses)?;

        let start = self.active_step_mut().ok_or_else(|| {
            Error::internal("source step remained active while installing its return plan")
        })?;
        start.plan_addresses.extend(plan_addresses);
        start.return_traversal = Some(ReturnTraversal {
            return_address: cfi_return,
            guarded_activation,
            retire_return_after_repair: false,
        });

        Ok(true)
    }

    pub(super) fn retire_epilogue_return_guard(&mut self) -> Result<()> {
        let retirement = self
            .session
            .inferior()
            .and_then(|inferior| inferior.run.execution())
            .and_then(|active| match &active.kind {
                ActiveKind::Step { start, kind, .. } => start
                    .epilogue_traversal
                    .as_ref()
                    .filter(|traversal| traversal.retire_return_after_repair)
                    .map(|traversal| (active.id, traversal.return_address, *kind)),
                _ => None,
            });
        let Some((execution, address, kind)) = retirement else {
            return Ok(());
        };

        if kind == StepKind::IntoSource {
            self.retire_step_plan(execution)?;
            let start = self.active_step_mut().ok_or_else(|| {
                Error::internal("source step remained active while retiring its return guard")
            })?;
            start.plan_addresses.clear();
            start.epilogue_traversal = None;
            return Ok(());
        }

        self.remove_breakpoint_owner(address, BreakpointOwner::Plan(execution))?;

        let start = self.active_step_mut().ok_or_else(|| {
            Error::internal("source step remained active while retiring its return guard")
        })?;
        start.plan_addresses.remove(&address);
        start.epilogue_traversal = None;

        Ok(())
    }

    pub(super) fn retire_return_guard(&mut self) -> Result<()> {
        let retirement = self
            .session
            .inferior()
            .and_then(|inferior| inferior.run.execution())
            .and_then(|active| match &active.kind {
                ActiveKind::Step { start, .. } => start
                    .return_traversal
                    .as_ref()
                    .filter(|traversal| traversal.retire_return_after_repair)
                    .map(|traversal| (active.id, traversal.return_address)),
                _ => None,
            });
        let Some((execution, address)) = retirement else {
            return Ok(());
        };

        self.remove_breakpoint_owner(address, BreakpointOwner::Plan(execution))?;

        let start = self.active_step_mut().ok_or_else(|| {
            Error::internal("source step remained active while retiring its return guard")
        })?;
        start.plan_addresses.remove(&address);
        start.return_traversal = None;

        Ok(())
    }

    pub(super) fn mark_epilogue_return_for_retirement(&mut self, address: VirtualAddress) {
        let Some(start) = self.active_step_mut() else {
            return;
        };

        if let Some(traversal) = start.epilogue_traversal.as_mut()
            && traversal.return_address == address
            && !traversal.completion_addresses.contains(&address)
        {
            traversal.retire_return_after_repair = true;
        }
    }

    pub(super) fn mark_return_guard_for_retirement(
        &mut self,
        pid: Pid,
        address: VirtualAddress,
    ) -> Result<()> {
        let registers = self.ptrace.registers(pid)?;
        let position = self.stack_position(pid, &registers);
        let Some(start) = self.active_step_mut() else {
            return Ok(());
        };

        if let Some(traversal) = start.return_traversal.as_mut()
            && traversal.return_address == address
            && traversal.guarded_activation.has_returned(position)
        {
            traversal.retire_return_after_repair = true;
        }

        Ok(())
    }

    fn before_breakpoint(&self, pid: Pid, instruction: VirtualAddress) -> Result<bool> {
        let inferior = self.session.inferior().ok_or(Error::NotRunning)?;
        let hit = inferior
            .threads
            .get(&pid)
            .is_some_and(|thread| thread.trap.owed() == Some(instruction));
        Ok(!hit
            && inferior
                .breakpoints
                .get(&instruction)
                .is_some_and(|site| site.trap == Trap::Installed))
    }

    fn step_completion(&self, pid: Pid, kind: StepKind) -> Result<Completion> {
        if kind == StepKind::Instruction {
            return Ok(Completion::Complete);
        }

        let registers = self.ptrace.registers(pid)?;
        let start = self
            .session
            .inferior()
            .and_then(|inferior| inferior.run.execution())
            .and_then(|active| match &active.kind {
                ActiveKind::Step { start, .. } => Some(start),
                _ => None,
            })
            .ok_or_else(|| Error::internal("source step has a starting state"))?;

        if start.running_on
            || start.loops.as_ref().is_some_and(StepLoops::waits)
            || super::awaits::waits(start)
        {
            return Ok(Completion::Incomplete);
        }

        if kind == StepKind::OverInstruction {
            let position = self.stack_position(pid, &registers);
            return Ok(Completion::from(start.call_return.is_none_or(
                |(address, stack)| registers.rip == address.get() && position == stack,
            )));
        }

        if let Some(complete) = self.traversal_step_is_complete(pid, &registers, start, kind) {
            return Ok(Completion::from(complete));
        }

        let instruction = VirtualAddress::new(registers.rip);
        if self.before_breakpoint(pid, instruction)? {
            return Ok(Completion::AtTrap(instruction));
        }

        self.reaches_step_destination(pid, &registers, start, kind, instruction)
            .map(Completion::from)
    }

    fn reaches_step_destination(
        &self,
        pid: Pid,
        registers: &libc::user_regs_struct,
        start: &StepStart,
        kind: StepKind,
        instruction: VirtualAddress,
    ) -> Result<bool> {
        match kind {
            StepKind::Advance => Err(Error::internal("an advance runs as a step out")),
            StepKind::IntoNewTask => Err(Error::internal(STEPS_OVER)),
            StepKind::Instruction | StepKind::OverInstruction => Ok(true),
            StepKind::IntoSource => self.step_into_source_is_complete(pid, registers, start),
            _ if start.following => self.step_into_source_is_complete(pid, registers, start),
            StepKind::OverSource if start.code_instance.is_none() => {
                self.step_into_source_is_complete(pid, registers, start)
            }
            StepKind::OverSource | StepKind::Out if let Some(caller) = start.returned_to => {
                Ok(self.returned_step_is_complete(pid, registers, caller, kind))
            }
            StepKind::OverSource | StepKind::Out => {
                let Some(activation) = start.activation else {
                    return Err(Error::LocationUnavailable);
                };

                let Some(code_instance) = start.code_instance else {
                    return self.undescribed_step_out_is_complete(pid, registers, activation);
                };

                let Some(located) = self.located_activation(pid, registers, activation)? else {
                    return Ok(self.located(instruction).is_some_and(|located| {
                        source_step_destination(located.image, &located.location, kind)
                    }));
                };

                let Located {
                    image, location, ..
                } = &located;
                if let Some(complete) = inline_loop_step_is_complete(image, location, start, kind) {
                    return Ok(complete);
                }

                let same_module = self.in_step_module(start, &located);
                if !same_module || !code_instance_is_active(location, code_instance) {
                    let returned = activation.has_returned(self.stack_position(pid, registers));
                    if (!same_module || location.physical_instance != start.physical_instance)
                        && !returned
                    {
                        return Ok(false);
                    }

                    if !returned && self.left_inlined_body_pending(pid, start, instruction) {
                        return Ok(false);
                    }

                    return Ok(source_step_destination(image, location, kind));
                }

                let source = source_for_code_instance(image, location, code_instance);

                Ok(kind == StepKind::OverSource
                    && source_step_destination(image, location, kind)
                    && source_line_changed(start.source.as_ref(), source.as_ref()))
            }
        }
    }

    fn traversal_step_is_complete(
        &self,
        pid: Pid,
        registers: &libc::user_regs_struct,
        start: &StepStart,
        kind: StepKind,
    ) -> Option<bool> {
        let instruction = VirtualAddress::new(registers.rip);
        let trapped = self
            .session
            .inferior()
            .and_then(|inferior| inferior.threads.get(&pid))
            .and_then(|thread| thread.trap.owed());
        if let Some(traversal) = &start.return_traversal {
            if trapped != Some(instruction) || instruction != traversal.return_address {
                return Some(false);
            }

            if !traversal
                .guarded_activation
                .has_returned(self.stack_position(pid, registers))
            {
                return Some(false);
            }

            if start.activation == Some(traversal.guarded_activation) {
                if kind == StepKind::Out {
                    return Some(true);
                }

                return Some(self.located(instruction).is_some_and(|located| {
                    source_step_destination(located.image, &located.location, kind)
                        && self.line_changed(start, &located, located.location.source.as_ref())
                }));
            }
        }

        start.epilogue_traversal.as_ref().map(|traversal| {
            trapped == Some(instruction) && traversal.completion_addresses.contains(&instruction)
        })
    }

    fn returned_step_is_complete(
        &self,
        pid: Pid,
        registers: &libc::user_regs_struct,
        caller: Activation,
        kind: StepKind,
    ) -> bool {
        let current = self.top_activation(pid, registers).ok();
        current.is_some_and(|current| current == caller || caller.is_callee_of(current))
            && self
                .located(VirtualAddress::new(registers.rip))
                .is_some_and(|located| {
                    source_step_destination(located.image, &located.location, kind)
                })
    }

    fn undescribed_step_out_is_complete(
        &self,
        pid: Pid,
        registers: &libc::user_regs_struct,
        activation: Activation,
    ) -> Result<bool> {
        Ok(self
            .located_activation(pid, registers, activation)?
            .is_none()
            && self
                .located(VirtualAddress::new(registers.rip))
                .is_some_and(|located| {
                    source_step_destination(located.image, &located.location, StepKind::Out)
                }))
    }

    pub(super) fn step_into_source_is_complete(
        &self,
        pid: Pid,
        registers: &libc::user_regs_struct,
        start: &StepStart,
    ) -> Result<bool> {
        let Some(located) = self
            .located(VirtualAddress::new(registers.rip))
            .filter(|located| !undescribed(&located.location))
        else {
            return Ok(false);
        };

        let image = located.image;
        let described = &located.location;
        let presentation = self.presentation_for_thread(
            pid,
            Some(&StopReason::Step {
                kind: StepKind::IntoSource,
            }),
        )?;
        let current_instance = self.selected_instance(&located, &presentation)?;
        if runs_for_runtime(image, described, current_instance, start) {
            return Ok(false);
        }

        let physical_role = self.code_role(VirtualAddress::new(registers.rip));

        let role = current_instance
            .filter(|instance| described.physical_instance != Some(*instance))
            .and_then(|instance| image.code_instance(instance))
            .and_then(|instance| image.function(instance.function()))
            .map(crate::image::functions::Function::role)
            .or(physical_role);
        if role.is_some_and(|role| passes_over(role, start)) {
            return Ok(false);
        }

        let inlined_in_passed = physical_role.is_some_and(|role| passes_over(role, start));
        let source = current_instance
            .and_then(|instance| source_for_code_instance(image, described, instance));

        let leads_into_body = current_instance
            .and_then(|instance| image.code_instance(instance))
            .and_then(|instance| image.function(instance.function()))
            .filter(|function| function.coroutine().is_some())
            .and_then(crate::image::functions::Function::declaration)
            .zip(source.as_ref())
            .is_some_and(|(header, at)| header.file == at.file && header.line == at.line);
        if leads_into_body {
            return Ok(false);
        }

        let activation = self.top_activation(pid, registers).ok();
        let statement = image
            .line_entry_containing(described.address)
            .is_some_and(|entry| entry.statement);

        let same_module = self.in_step_module(start, &located);
        let current_physical = described.physical_instance;
        let other_physical = current_physical
            .is_some_and(|current| !same_module || Some(current) != start.physical_instance);
        let activation_changed = activation
            .zip(start.activation)
            .is_some_and(|(current, start)| current != start);
        let entered_physical_activation = match start.activation {
            Some(start_activation) if activation_changed => self
                .located_activation(pid, registers, start_activation)?
                .is_some(),
            Some(_) => other_physical,
            None => {
                other_physical
                    && start.stack_pointer.is_some_and(|start| {
                        self.stack_position(pid, registers).is_deeper_than(start)
                    })
            }
        };

        let at_recommended_entry = described.source.is_some()
            && described.physical_instance.is_some_and(|instance| {
                image
                    .recommended_entries_for_instance(instance)
                    .any(|entry| entry.address == described.address)
            });
        if entered_physical_activation && !at_recommended_entry && !inlined_in_passed {
            return Ok(false);
        }

        Ok(
            (statement || entered_physical_activation && at_recommended_entry)
                && (activation_changed
                    || !same_module
                    || current_instance != start.code_instance
                    || source_line_changed(start.source.as_ref(), source.as_ref())),
        )
    }

    pub(super) fn step_start(
        &self,
        pid: Pid,
        kind: StepKind,
        frame: StackFrameId,
    ) -> Result<StepStart> {
        let _span = crate::span!("step plan");
        let inferior = self.session.inferior().ok_or(Error::NotRunning)?;
        let thread = inferior.thread(pid)?;
        if !matches!(thread.state, NativeThreadState::Stopped) {
            return Err(Error::NotStopped);
        }

        if frame.get() != 0 {
            let registers = self.ptrace.registers(pid)?;
            return self.outer_step_out_start(pid, &registers, frame);
        }

        self.innermost_step_start(pid, kind, || self.presentation_for_stopped_thread(pid))
    }

    pub(super) fn innermost_step_start(
        &self,
        pid: Pid,
        kind: StepKind,
        presentation: impl FnOnce() -> Result<FramePresentation>,
    ) -> Result<StepStart> {
        let inferior = self.session.inferior().ok_or(Error::NotRunning)?;
        let registers = self.ptrace.registers(pid)?;
        let located = self.located(VirtualAddress::new(registers.rip));

        let code_instance = if steps_instructions(kind) {
            None
        } else {
            let presentation = presentation()?;
            located
                .as_ref()
                .map(|located| self.selected_instance(located, &presentation))
                .transpose()?
                .flatten()
        };

        let source = located
            .as_ref()
            .zip(code_instance)
            .and_then(|(located, instance)| {
                source_for_code_instance(located.image, &located.location, instance)
            });

        let activation = self.step_activation(pid, &registers, kind, code_instance)?;
        let mut plan_addresses = BTreeSet::new();
        let call_return = if kind == StepKind::OverInstruction {
            self.call_return(inferior, pid, &registers)?
        } else {
            None
        };

        plan_addresses.extend(call_return.map(|(address, _)| address));

        let selected_is_inline = located
            .as_ref()
            .zip(code_instance)
            .and_then(|(located, instance)| located.image.code_instance(instance))
            .is_some_and(|instance| matches!(instance.kind(), CodeInstanceKind::Inline { .. }));

        let mut returning = None;
        if kind == StepKind::Out && !selected_is_inline {
            let return_address = self.caller_address(pid, &registers)?;
            plan_addresses.insert(return_address);
            returning = activation
                .zip(located.as_ref())
                .map(|(activation, located)| Returning {
                    activation,
                    return_address,
                    module: located.module.id,
                    function: located.location.address,
                });
        } else if kind == StepKind::OverSource
            && !selected_is_inline
            && let (Some(source), Some(instance_id), Some(located)) =
                (&source, code_instance, &located)
        {
            self.plan_source_lines(
                pid,
                &registers,
                located,
                source,
                instance_id,
                &mut plan_addresses,
            )?;
        }

        let panic_guards = if matches!(kind, StepKind::OverSource | StepKind::Out) {
            self.panic_entries(inferior)
        } else {
            BTreeSet::new()
        };

        let in_main = |located: &Located<'_>| located.module.id == inferior.loaded_module.id;
        let loops = if matches!(kind, StepKind::OverSource | StepKind::Out) {
            self.step_loops(
                pid,
                &registers,
                kind,
                code_instance.filter(|_| located.as_ref().is_some_and(in_main)),
                activation,
                &mut plan_addresses,
            )?
        } else {
            None
        };

        #[cfg(debug_assertions)]
        if let Some(loops) = &loops {
            record!("step {kind:?} treats loops as its own code: {loops:?}");
        }

        Ok(StepStart {
            module: located.as_ref().map(|located| located.module),
            source,
            code_instance,
            physical_instance: located
                .as_ref()
                .and_then(|located| located.location.physical_instance),
            activation,
            stack_pointer: Some(self.stack_position(pid, &registers)),
            plan_addresses,
            call_return,
            panic_guards,
            enters_runtime: self.step_into_runtime
                || self
                    .code_role(VirtualAddress::new(registers.rip))
                    .is_some_and(is_runtime_role),
            loops,
            returning,
            ..StepStart::default()
        })
    }

    fn step_activation(
        &self,
        pid: Pid,
        registers: &libc::user_regs_struct,
        kind: StepKind,
        code_instance: Option<CodeInstanceId>,
    ) -> Result<Option<Activation>> {
        Ok(match kind {
            StepKind::Advance => return Err(Error::internal("an advance runs as a step out")),
            StepKind::IntoNewTask => return Err(Error::internal(STEPS_OVER)),
            StepKind::Instruction | StepKind::OverInstruction => None,
            StepKind::IntoSource => self.top_activation(pid, registers).ok(),
            StepKind::OverSource if code_instance.is_none() => {
                self.top_activation(pid, registers).ok()
            }
            StepKind::OverSource | StepKind::Out => Some(self.top_activation(pid, registers)?),
        })
    }

    fn plan_source_lines(
        &self,
        pid: Pid,
        registers: &libc::user_regs_struct,
        located: &Located<'_>,
        source: &SourceLocation,
        instance_id: crate::CodeInstanceId,
        plan_addresses: &mut BTreeSet<VirtualAddress>,
    ) -> Result<()> {
        if located.image.code_instance(instance_id).is_none() {
            return Ok(());
        }

        let return_address = match self.caller_address(pid, registers) {
            Ok(address) => Some(address),
            Err(error) if is_caller_unavailable(&error) => None,
            Err(error) => return Err(error),
        };

        plan_addresses.extend(statements_in(
            located.module,
            located.image,
            instance_id,
            Some(source),
        )?);
        plan_addresses.extend(return_address);

        Ok(())
    }

    pub(super) fn other_lines(
        &self,
        instance_id: crate::CodeInstanceId,
        source: &SourceLocation,
    ) -> Result<BTreeSet<VirtualAddress>> {
        self.statements_of(instance_id, Some(source))
    }

    pub(super) fn statements_of(
        &self,
        instance_id: crate::CodeInstanceId,
        except: Option<&SourceLocation>,
    ) -> Result<BTreeSet<VirtualAddress>> {
        let inferior = self.session.inferior().ok_or(Error::NotRunning)?;
        statements_in(
            inferior.loaded_module,
            &self.main.image,
            instance_id,
            except,
        )
    }

    pub(super) fn call_return(
        &self,
        inferior: &Inferior,
        pid: Pid,
        registers: &libc::user_regs_struct,
    ) -> Result<Option<(VirtualAddress, StackPosition)>> {
        let mut decoder = decoder_for(self.main.image.target(), AssemblySyntax::Intel)?;
        let read = read_logical_memory(
            &self.ptrace,
            pid,
            &inferior.breakpoints,
            VirtualAddress::new(registers.rip),
            decoder.max_instruction_length(),
        )?;

        Ok(match decoder.decode(registers.rip, &read.bytes, None) {
            RawDecode::Instruction {
                length,
                flow: ControlFlow::Call | ControlFlow::IndirectCall,
                ..
            } => Some((
                VirtualAddress::new(
                    registers
                        .rip
                        .checked_add(length as u64)
                        .ok_or(Error::AddressOverflow)?,
                ),
                self.stack_position(pid, registers),
            )),
            _ => None,
        })
    }

    fn outer_step_out_start(
        &self,
        pid: Pid,
        registers: &libc::user_regs_struct,
        frame: StackFrameId,
    ) -> Result<StepStart> {
        let inferior = self.session.inferior().ok_or(Error::NotRunning)?;
        let resolved = self.resolve_frame(inferior, &StackRoot::of_thread(pid), frame)?;
        let selected = resolved.frame.as_ref().ok_or(Error::AmbiguousInlineFrame)?;

        let Some((_, address)) = resolved
            .code
            .filter(|(module, _)| *module == inferior.loaded_module.id)
        else {
            return Err(Error::FrameStepUnsupported(
                "stepping out applies only to frames of the main executable".into(),
            ));
        };

        let location = self.main.image.locate(address);
        let code_instance = match &resolved.presented {
            PresentedFrame::Inline(instance) => Some(*instance),
            PresentedFrame::Physical => location.physical_instance,
            PresentedFrame::Ambiguous(_) => return Err(Error::AmbiguousInlineFrame),
        }
        .ok_or(Error::LocationUnavailable)?;
        let selected_is_inline = matches!(resolved.presented, PresentedFrame::Inline(_));
        let mut plan_addresses = BTreeSet::new();

        let mut return_address = None;
        let activation = if resolved.activation == 0 {
            if !selected_is_inline {
                return_address = Some(self.caller_address(pid, registers)?);
            }

            self.top_activation(pid, registers)?
        } else {
            if selected_is_inline {
                return Err(Error::FrameStepUnsupported(
                    "stepping out of an inline frame is supported only in the innermost activation"
                        .into(),
                ));
            }

            let caller_unavailable = |reason| backend_error(LinuxError::CallerUnavailable(reason));
            let activation = self
                .stack_view(pid)
                .activation(resolved.cfa.clone().map_err(|_| {
                    caller_unavailable(crate::UnwindTermination::InvalidCaller {
                        description: "the frame's call-frame address is unavailable".into(),
                    })
                })?);
            let stack = self.physical_stack(
                inferior,
                &StackRoot::of_thread(pid),
                resolved.activation + 2,
            )?;
            let caller = stack
                .frames
                .get(resolved.activation + 1)
                .map(|caller| caller.context.instruction)
                .ok_or_else(|| caller_unavailable(stack.termination.clone()))?;

            return_address = Some(self.executable_return_address(pid, caller)?);
            activation
        };

        plan_addresses.extend(return_address);

        Ok(StepStart {
            source: selected.source.clone(),
            code_instance: Some(code_instance),
            physical_instance: location.physical_instance,
            activation: Some(activation),
            stack_pointer: Some(self.stack_position(pid, registers)),
            plan_addresses,
            panic_guards: self.panic_entries(inferior),
            returning: return_address.map(|return_address| Returning {
                activation,
                return_address,
                module: inferior.loaded_module.id,
                function: address,
            }),
            ..StepStart::default()
        })
    }

    pub(super) fn top_activation(
        &self,
        pid: Pid,
        native: &libc::user_regs_struct,
    ) -> Result<Activation> {
        Ok(self.stack_view(pid).activation(self.top_cfa(pid, native)?))
    }

    pub(super) fn top_cfa(
        &self,
        pid: Pid,
        native: &libc::user_regs_struct,
    ) -> Result<VirtualAddress> {
        let inferior = self.session.inferior().ok_or(Error::NotRunning)?;
        self.stack_unwinder(inferior, pid, native)
            .frame_cfa(&innermost_frame(native))
            .map_err(|reason| backend_error(LinuxError::CallerUnavailable(reason)))
    }

    pub(super) fn location_for_activation(
        &self,
        pid: Pid,
        native: &libc::user_regs_struct,
        activation: Activation,
    ) -> Result<Option<ImageLocation>> {
        let Some(address) = self.activation_address(pid, native, activation)? else {
            return Ok(None);
        };

        let inferior = self.session.inferior().ok_or(Error::NotRunning)?;
        Ok(inferior
            .loaded_module
            .image_address(address)
            .ok()
            .filter(|address| self.main.image.contains_address(*address))
            .map(|address| self.main.image.locate(address)))
    }

    fn located_activation(
        &self,
        pid: Pid,
        native: &libc::user_regs_struct,
        activation: Activation,
    ) -> Result<Option<Located<'_>>> {
        Ok(self
            .activation_address(pid, native, activation)?
            .and_then(|address| self.located(address)))
    }

    fn activation_address(
        &self,
        pid: Pid,
        native: &libc::user_regs_struct,
        activation: Activation,
    ) -> Result<Option<VirtualAddress>> {
        if activation.has_returned(self.stack_position(pid, native)) {
            return Ok(None);
        }

        let view = self.stack_view(pid);
        let inferior = self.session.inferior().ok_or(Error::NotRunning)?;

        let (_, _, found) = self.walk_stack(
            inferior,
            &StackRoot::of_thread(pid),
            Some(native),
            |provider, innermost| find_activation(view, provider, innermost, activation),
        )?;

        found
    }

    pub(super) fn caller_address(
        &self,
        pid: Pid,
        native: &libc::user_regs_struct,
    ) -> Result<VirtualAddress> {
        let inferior = self.session.inferior().ok_or(Error::NotRunning)?;
        match self
            .stack_unwinder(inferior, pid, native)
            .caller(&innermost_frame(native))
        {
            CallerResult::Caller(caller) => self.executable_return_address(pid, caller.instruction),
            CallerResult::Finished(reason) => {
                Err(backend_error(LinuxError::CallerUnavailable(reason)))
            }
        }
    }

    pub(super) fn executable_return_address(
        &self,
        pid: Pid,
        address: VirtualAddress,
    ) -> Result<VirtualAddress> {
        if self.ptrace.executable(pid, address)? {
            return Ok(address);
        }

        Err(backend_error(LinuxError::CallerUnavailable(
            crate::UnwindTermination::InvalidCaller {
                description: format!("return address {address} is not in executable memory").into(),
            },
        )))
    }

    pub(super) fn stack_unwinder<'a>(
        &'a self,
        inferior: &Inferior,
        pid: Pid,
        native: &libc::user_regs_struct,
    ) -> DwarfCallerProvider<'a> {
        self.caller_provider(
            inferior,
            &self.runtimes(inferior),
            pid,
            x86_64_registers(native),
            true,
        )
    }

    fn call_out(&self, pid: Pid, registers: &libc::user_regs_struct) -> Option<VirtualAddress> {
        let inferior = self.session.inferior()?;
        let rip = VirtualAddress::new(registers.rip);
        let file = x86_64_registers(registers);
        let target = self.runtimes(inferior).into_iter().find_map(|runtime| {
            let entry = runtime.module.image_address(rip).ok()?;
            runtime.model.call_out(entry, &file)
        })?;
        match target {
            Ok(target) if self.ptrace.executable(pid, target).unwrap_or(false) => Some(target),

            #[cfg_attr(
                not(debug_assertions),
                expect(unused_variables, reason = "only recorded")
            )]
            Ok(target) => {
                record!("the runtime calls {target}, which is not executable");
                None
            }

            #[cfg_attr(
                not(debug_assertions),
                expect(unused_variables, reason = "only recorded")
            )]
            Err(reason) => {
                record!("the runtime's call out is unknown: {reason}");
                None
            }
        }
    }

    pub(super) fn code_role(&self, address: VirtualAddress) -> Option<CodeRole> {
        let inferior = self.session.inferior()?;
        let (image, address) = self.module_holding(inferior, address)?;
        Some(image.code_role(address))
    }

    pub(super) fn image_location(&self, address: VirtualAddress) -> Option<ImageLocation> {
        let inferior = self.session.inferior()?;
        let image = inferior.loaded_module.image_address(address).ok()?;
        Some(self.main.image.locate(image))
    }

    fn located(&self, address: VirtualAddress) -> Option<Located<'_>> {
        let inferior = self.session.inferior()?;
        self.loaded_images(inferior).find_map(|(module, image)| {
            let at = module
                .image_address(address)
                .ok()
                .filter(|at| image.contains_address(*at))?;
            Some(Located {
                module,
                image,
                location: image.locate(at),
            })
        })
    }

    fn step_code(&self, start: &StepStart) -> Option<(LoadedModule, &Arc<ModuleImage>)> {
        let inferior = self.session.inferior()?;
        let module = start.module.unwrap_or(inferior.loaded_module);
        self.loaded_images(inferior)
            .find(|(loaded, _)| loaded.id == module.id)
    }

    fn in_step_module(&self, start: &StepStart, located: &Located<'_>) -> bool {
        self.session.inferior().is_some_and(|inferior| {
            start.module.unwrap_or(inferior.loaded_module).id == located.module.id
        })
    }

    pub(super) fn steps_in_main_module(&self, start: &StepStart) -> bool {
        self.session.inferior().is_some_and(|inferior| {
            start
                .module
                .is_none_or(|module| module.id == inferior.loaded_module.id)
        })
    }

    fn line_changed(
        &self,
        start: &StepStart,
        located: &Located<'_>,
        source: Option<&SourceLocation>,
    ) -> bool {
        if self.in_step_module(start, located) {
            source_line_changed(start.source.as_ref(), source)
        } else {
            source.is_some()
        }
    }

    fn selected_instance(
        &self,
        located: &Located<'_>,
        presentation: &FramePresentation,
    ) -> Result<Option<CodeInstanceId>> {
        let main = self
            .session
            .inferior()
            .is_some_and(|inferior| located.module.id == inferior.loaded_module.id);
        if main {
            selected_code_instance(&located.location, presentation)
        } else {
            Ok(located.location.physical_instance)
        }
    }
}

struct Located<'a> {
    module: LoadedModule,
    image: &'a Arc<ModuleImage>,
    location: ImageLocation,
}

fn caller_statements(
    caller: &Located<'_>,
    start_source: Option<&SourceLocation>,
) -> Result<BTreeSet<VirtualAddress>> {
    let Located {
        module,
        image,
        location,
    } = caller;
    let mut statements = BTreeSet::new();
    let Some(caller_instance_id) = location.physical_instance else {
        return Ok(statements);
    };

    let Some(caller_instance) = image.code_instance(caller_instance_id) else {
        return Ok(statements);
    };

    for line in image.line_entries_in(caller_instance) {
        if !line.statement {
            continue;
        }

        if image
            .control_boundaries_at(line.range.start)
            .any(|row| row.flags.epilogue_begin())
        {
            continue;
        }

        let candidate_location = image.locate(line.range.start);
        if source_for_code_instance(image, &candidate_location, caller_instance_id)
            .is_some_and(|candidate| source_line_changed(start_source, Some(&candidate)))
        {
            statements.insert(module.virtual_address(line.range.start)?);
        }
    }

    Ok(statements)
}

fn runs_for_runtime(
    image: &ModuleImage,
    location: &ImageLocation,
    current: Option<CodeInstanceId>,
    start: &StepStart,
) -> bool {
    let passed = |instance: &CodeInstanceId| {
        image
            .code_instance(*instance)
            .and_then(|instance| image.function(instance.function()))
            .is_some_and(|function| is_runtime_role(function.role()) && !start.enters_runtime)
    };

    let enclosing = match &location.inline_frames {
        InlineFrameLookup::Unique(chain) => {
            let inner = current
                .and_then(|current| chain.instances.iter().position(|id| *id == current))
                .unwrap_or(0);
            chain.instances.get(..inner).unwrap_or_default()
        }
        _ => &[],
    };

    location.physical_instance != current
        && (location.physical_instance.iter().any(passed) || enclosing.iter().any(passed))
}

fn find_activation(
    view: StackView,
    provider: &mut RoleCallerProvider<'_, '_>,
    mut context: FrameContext,
    activation: Activation,
) -> Result<Option<VirtualAddress>> {
    for level in 0..DEFAULT_MAX_FRAMES {
        let frame = view.activation(
            provider
                .dwarf
                .frame_cfa(&context)
                .map_err(|reason| backend_error(LinuxError::CallerUnavailable(reason)))?,
        );
        if frame == activation {
            let level =
                u32::try_from(level).map_err(|_| Error::internal("frame limit fits u32"))?;
            return Ok(frame_lookup_address(level, &context));
        }

        if activation.is_callee_of(frame) {
            return Ok(None);
        }

        context = match provider.caller(&context) {
            CallerResult::Caller(caller) => caller,
            CallerResult::Finished(crate::UnwindTermination::Complete) => return Ok(None),
            CallerResult::Finished(reason) => {
                return Err(backend_error(LinuxError::CallerUnavailable(reason)));
            }
        };
    }

    Ok(None)
}

fn statements_in(
    module: LoadedModule,
    image: &ModuleImage,
    instance_id: CodeInstanceId,
    except: Option<&SourceLocation>,
) -> Result<BTreeSet<VirtualAddress>> {
    let Some(instance) = image.code_instance(instance_id) else {
        return Ok(BTreeSet::new());
    };

    let mut statements = BTreeSet::new();
    for line in image.line_entries_in(instance) {
        if !line.statement {
            continue;
        }

        let location = image.locate(line.range.start);
        if source_for_code_instance(image, &location, instance_id).is_some_and(|candidate| {
            except.is_none_or(|source| source_line_changed(Some(source), Some(&candidate)))
        }) {
            statements.insert(module.virtual_address(line.range.start)?);
        }
    }

    Ok(statements)
}

pub(super) const fn innermost_frame(native: &libc::user_regs_struct) -> FrameContext {
    FrameContext {
        instruction: VirtualAddress::new(native.rip),
        cfa: None,
        signal_frame: false,
    }
}

const STEPS_OVER: &str = "a step into a new task runs as a step over";

pub(super) const fn is_runtime_role(role: CodeRole) -> bool {
    matches!(
        role,
        CodeRole::RuntimeInternal
            | CodeRole::Outermost
            | CodeRole::TrapEntry
            | CodeRole::Dispatch
            | CodeRole::Interpreter
    )
}

const fn passes_over(role: CodeRole, start: &StepStart) -> bool {
    match role {
        CodeRole::Wrapper | CodeRole::StackSwitch | CodeRole::Panic => true,
        role => is_runtime_role(role) && !start.enters_runtime,
    }
}

const fn undescribed(location: &ImageLocation) -> bool {
    location.physical_instance.is_none() && location.source.is_none()
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Completion {
    Complete,
    Incomplete,

    AtTrap(VirtualAddress),
}

impl From<bool> for Completion {
    fn from(complete: bool) -> Self {
        if complete {
            Self::Complete
        } else {
            Self::Incomplete
        }
    }
}

fn is_caller_unavailable(error: &Error) -> bool {
    matches!(
        error,
        Error::Backend(error)
            if matches!(error.downcast_ref::<LinuxError>(), Some(LinuxError::CallerUnavailable(_)))
    )
}

fn is_lost_step_evidence(error: &Error) -> bool {
    matches!(error, Error::LocationUnavailable) || is_caller_unavailable(error)
}

fn step_incomplete(kind: StepKind, error: &Error) -> StopReason {
    let description = match error {
        Error::Backend(inner) => inner.to_string(),
        other => other.to_string(),
    };

    record!("step {kind:?} lost track of its frame: {description}");

    StopReason::StepIncomplete {
        kind,
        description: description.into(),
    }
}
