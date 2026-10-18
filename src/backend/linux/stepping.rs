//! Source and instruction stepping plans and their completion rules.

use std::collections::BTreeSet;

use nix::errno::Errno;
use nix::libc;
use nix::unistd::Pid;

use crate::protocol::{
    DebuggerEvent, ExecutionId, FramePresentation, PresentedFrame, ProcessId, StepKind, StopId,
    StopReason,
};

use crate::unwind::{CallerProvider, CallerResult, DEFAULT_MAX_FRAMES, FrameContext};
use crate::{
    CodeInstanceId, CodeInstanceKind, CodeRole, Error, ImageAddress, ImageLocation,
    InlineFrameLookup, LoadedModule, Result, SourceLocation, StackFrameId, StackSegment, TaskId,
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

use super::loops::{StepLoops, inline_loop_step_is_complete};
use super::memory::PtraceMemory;
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
        self.inferior
            .as_ref()
            .and_then(|inferior| inferior.active.as_ref())
            .map(|active| active.id)
            .ok_or(Error::NotRunning)
    }

    /// Whether the active step is an advance that `address` ends.
    pub(super) fn is_advance_target(&self, address: VirtualAddress) -> bool {
        self.inferior
            .as_ref()
            .and_then(|inferior| inferior.active.as_ref())
            .is_some_and(|active| {
                matches!(&active.kind, ActiveKind::Step { start, .. } if start.targets.contains(&address))
            })
    }

    /// Whether the advance's thread arrived at `address` where it stood
    /// when the advance began, which happens once and does not end it.
    pub(super) fn take_standing_arrival(&mut self, address: VirtualAddress) -> bool {
        self.active_step_mut()
            .is_some_and(|start| start.standing.take_if(|at| *at == address).is_some())
    }

    /// Removes the plan sites of a step that goes on by other means,
    /// keeping an advance's targets, which end it however it goes on.
    pub(super) fn retire_step_plan(&mut self, execution: ExecutionId) -> Result<()> {
        let targets = self
            .active_step_mut()
            .map(|start| start.targets.clone())
            .unwrap_or_default();
        if targets.is_empty() {
            return self.cleanup_plan_breakpoints(execution);
        }

        let owner = BreakpointOwner::Plan(execution);
        let inferior = self.inferior.as_ref().ok_or(Error::NotRunning)?;
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
            .inferior
            .as_mut()
            .and_then(|inferior| inferior.plan_sites.get_mut(&execution))
        {
            sites.retain(|address| targets.contains(address));
        }

        Ok(())
    }

    /// The state of the active step, if one is active.
    pub(super) fn active_step_mut(&mut self) -> Option<&mut StepStart> {
        match &mut self.inferior.as_mut()?.active.as_mut()?.kind {
            ActiveKind::Step { start, .. } => Some(start),
            _ => None,
        }
    }

    /// Steps into an inline frame hidden at the current instruction without
    /// running the inferior. On success returns the execution and the stop
    /// event to publish once the step is acknowledged.
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
            let inferior = self.inferior.as_ref().ok_or(Error::NotRunning)?;
            validate_process(inferior, requested_process)?;
            validate_public_stop(inferior, Some(stop_id))?;
            validate_resumable(inferior)?;
            validate_stopped_thread(inferior, pid)?;
            let stop = inferior.public_stop.as_ref().ok_or(Error::NotStopped)?;
            let Some(current) = stop.presentations.get(&pid) else {
                return Ok(None);
            };

            if current.hidden_inline_frames == 0
                || matches!(current.frame, PresentedFrame::Ambiguous(_))
            {
                return Ok(None);
            }

            let image_address = inferior.loaded_module.image_address(current.instruction)?;
            let location = self.module_image.locate(image_address);
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

        let inferior = self.inferior.as_mut().ok_or(Error::NotRunning)?;
        let stop = inferior.public_stop.as_mut().ok_or(Error::NotStopped)?;
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
            .inferior
            .as_ref()
            .and_then(|inferior| inferior.active.as_ref())
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

        // A call behind a breakpoint is passed once the breakpoint's hit has
        // been judged, which may end the step before the call.
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

    /// Whether the thread is stopped in code without debug information.
    pub(super) fn stopped_outside_described_code(&self, pid: Pid) -> Result<bool> {
        let registers = self.ptrace.registers(pid)?;
        Ok(self
            .image_location(VirtualAddress::new(registers.rip))
            .is_none_or(|location| undescribed(&location)))
    }

    /// Whether the thread is stopped in code a source step leaves for its
    /// caller rather than stepping through: a stack switch, whose call-frame
    /// information cannot follow it, or the runtime's own machinery when
    /// the step may not enter it.
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
        self.inferior
            .as_ref()
            .and_then(|inferior| inferior.active.as_ref())
            .is_some_and(|active| {
                matches!(&active.kind, ActiveKind::Step { start, .. } if start.enters_runtime)
            })
    }

    /// Runs to the caller instead of single-stepping through code without
    /// debug information. The return address comes from call-frame
    /// information, as a PLT stub's does, or else from the top of the stack,
    /// and is trusted only where debug information describes it. Returns
    /// false, to single-step instead, when no return address is trusted.
    ///
    /// A step `into` a runtime function that calls the program's code for
    /// it, as cgo's calls between Go and C do, also runs to that code.
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
            .image_location(candidate)
            .is_some_and(|location| location.physical_instance.is_some());
        if !described {
            return Ok(false);
        }

        let execution = self.active_execution()?;
        let inferior = self.inferior.as_mut().ok_or(Error::NotRunning)?;
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
    /// Advances a user step after its thread stopped, publishing the step's
    /// stop once it completes.
    ///
    /// A step the debugger can no longer follow, because evidence such as
    /// the stepping frame's caller cannot be gathered, stops where its
    /// thread is with [`StopReason::StepIncomplete`]. The program is never
    /// harmed for the debugger's lack of evidence.
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

    /// Fails as the ptrace request did when evidence could not be read
    /// because SIGKILL took the thread out of its stop meanwhile, which is
    /// no lost frame: its handler then lets the thread run on to its exit.
    fn check_still_stopped(&self, pid: Pid) -> Result<()> {
        if is_superseded(&self.ptrace.signal_metadata(pid)) {
            return Err(backend_error(LinuxError::System(Errno::ESRCH)));
        }

        Ok(())
    }

    /// The stop a step publishes from where its thread stopped: completion,
    /// an explicit stop where it lost track of its frame, or `None` to go on.
    pub(super) fn user_step_stop(&self, pid: Pid, kind: StepKind) -> Result<Option<StopReason>> {
        self.judge_user_step(pid, kind).map(|(reason, _)| reason)
    }

    /// The stop [`Self::user_step_stop`] publishes, and when there is none,
    /// the trap the thread stands before whose hit is left to judge where
    /// the step arrived.
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

    /// The kind of step an active step now takes: a step in, once a step
    /// over or out follows the runtime's calls into the program.
    pub(super) fn step_mode(&self, kind: StepKind) -> StepKind {
        let following = self
            .inferior
            .as_ref()
            .and_then(|inferior| inferior.active.as_ref())
            .is_some_and(
                |active| matches!(&active.kind, ActiveKind::Step { start, .. } if start.following),
            );
        if following {
            StepKind::IntoSource
        } else {
            kind
        }
    }

    /// The entries of the code that begins a panic, in every image.
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

    /// Turns a step over or out into one that follows the runtime's calls
    /// into the program, when its task began a panic, or its frame returned
    /// into a wrapper, such as the one that calls a function's deferred
    /// functions as it returns, or left an inlined function for the wrapper
    /// it was inlined into. The program's code those call runs as the
    /// step's own, so the step goes on as a step in does, from here, to the
    /// next statement the program runs. Returns whether it began following.
    ///
    /// A deferred function may recover, and the runtime then resumes the
    /// function that deferred it, which returns. Guards on the return
    /// addresses of the task's frames catch that return.
    pub(super) fn begin_following(&mut self, pid: Pid, kind: StepKind) -> Result<bool> {
        if !matches!(kind, StepKind::OverSource | StepKind::Out) {
            return Ok(false);
        }

        let Some((following, frame, inlined)) = self
            .inferior
            .as_ref()
            .and_then(|inferior| inferior.active.as_ref())
            .and_then(|active| match &active.kind {
                ActiveKind::Step { owner, start, .. } if self.runs_step(*owner, pid) => Some((
                    start.following,
                    start.returned_to.or(start.activation),
                    start
                        .code_instance
                        .filter(|instance| start.physical_instance != Some(*instance))
                        .zip(start.physical_instance),
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
        let Some(location) = self.image_location(VirtualAddress::new(registers.rip)) else {
            return Ok(false);
        };

        let role = self
            .code_role(VirtualAddress::new(registers.rip))
            .unwrap_or_default();
        let returned = frame.is_some_and(|frame| frame.has_returned(position));

        // Left for the code it is inlined into, not for a function it
        // calls.
        let left_inlined = inlined.is_some_and(|(instance, physical)| {
            location.physical_instance == Some(physical)
                && !code_instance_is_active(&location, instance)
        });

        // An async body that may be pending is awaited by the code it
        // left, whose poll the step follows to its return.
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

    /// The return addresses in the program's own code of a stopped
    /// thread's frames: where the task goes on when a frame returns,
    /// however the code between got there.
    fn frame_return_guards(&self, pid: Pid) -> BTreeSet<VirtualAddress> {
        let Some(inferior) = self.inferior.as_ref() else {
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
                self.image_location(*address)
                    .is_some_and(|location| location.physical_instance.is_some())
                    && self.code_role(*address) == Some(CodeRole::Ordinary)
            })
            .collect()
    }

    /// The task a step begun on a stopped thread follows, whichever thread
    /// runs it: the task whose own stack the thread is on. Code on the
    /// thread's own stacks, such as a runtime's scheduler, stays on the
    /// thread whatever task it serves.
    pub(super) fn step_task(&self, pid: Pid) -> Option<TaskId> {
        match self.thread_activity(self.inferior.as_ref()?, pid)? {
            ThreadActivity::Task {
                task,
                stack: StackSegment::Task,
            } => Some(task),
            _ => None,
        }
    }

    /// Why a source step can follow its task no further, when the task
    /// stopped running on the thread stepping it. That happens only inside
    /// the runtime's scheduler, which source steps pass over by breakpoints.
    fn task_left_step(&self, pid: Pid, kind: StepKind) -> Option<String> {
        if steps_instructions(kind) {
            return None;
        }

        let inferior = self.inferior.as_ref()?;
        let ActiveKind::Step { owner, .. } = &inferior.active.as_ref()?.kind else {
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

    /// Moves the active step to `pid`, which now runs the step's task.
    pub(super) fn follow_step(&mut self, pid: Pid) {
        if let Some(ActiveKind::Step { owner, .. }) = self
            .inferior
            .as_mut()
            .and_then(|inferior| inferior.active.as_mut())
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
        self.inferior
            .as_ref()
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

                // An unavailable unwind (tail call into a shared library, PLT
                // stub, or CFI-less code) is expected lack of evidence, not a
                // controller failure. Stay on the instruction-stepping path.
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

    /// Whether the stepping thread stands where the step left its arrival
    /// for a trap's hit to judge, but the trap is gone, as an edit made
    /// while the step ran takes it out before the thread runs it. The step
    /// then judges the arrival itself.
    pub(super) fn arrival_left_to_a_lost_trap(&self, pid: Pid) -> Result<bool> {
        let Some(address) = self
            .inferior
            .as_ref()
            .and_then(|inferior| inferior.active.as_ref())
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

    /// Forgets the arrival a trap's hit was to judge once the stepping
    /// thread hits that trap, at `address`.
    pub(super) fn forget_trap_judgement(&mut self, pid: Pid, address: VirtualAddress) {
        let owner = self
            .inferior
            .as_ref()
            .and_then(|inferior| inferior.active.as_ref())
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

    /// When a step over or out whose frame returned enters a call its
    /// caller makes, it runs the call to its return instead of stepping
    /// through it: it guards the return address, which call-frame
    /// information and the stack's return slot must agree on, as a return
    /// traversal's does. Without that evidence it goes on by single steps.
    fn guard_returned_callee(&mut self, pid: Pid) -> Result<()> {
        let plan = self
            .inferior
            .as_ref()
            .and_then(|inferior| inferior.active.as_ref())
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

    /// Whether the frame the active step began in has returned, by what the
    /// step saw: see [`StepStart::returned_to`].
    fn step_frame_returned(&self) -> bool {
        self.inferior
            .as_ref()
            .and_then(|inferior| inferior.active.as_ref())
            .is_some_and(|active| {
                matches!(&active.kind, ActiveKind::Step { start, .. } if start.returned_to.is_some())
            })
    }

    /// Notes that the frame a step over or out began in has returned short
    /// of where the step ends, with no traversal guarding the return: short
    /// of a source statement for a step over, of code a line describes for
    /// a step out. Its plan then guards a frame that no longer exists, and
    /// a later call can create a new activation at the same CFA, which the
    /// plan cannot tell from the one that returned. The step instead
    /// records the activation it returned to and goes on by single steps.
    /// When that activation returns too, the step records the one it
    /// returned to in turn.
    pub(super) fn note_returned_activation(&mut self, pid: Pid, kind: StepKind) -> Result<()> {
        if !matches!(kind, StepKind::OverSource | StepKind::Out) {
            return Ok(());
        }

        let activation = self
            .inferior
            .as_ref()
            .and_then(|inferior| inferior.active.as_ref())
            .and_then(|active| match &active.kind {
                // A step over begun in code no debug information describes
                // steps as stepping in does, which follows no frame.
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

        // Without unwind information where it returned, the step keeps its
        // plan and judges as before.
        let Ok(caller) = self.top_activation(pid, &registers) else {
            return Ok(());
        };

        if let Some(start) = self.active_step_mut() {
            start.returned_to = Some(caller);
        }

        Ok(())
    }

    /// Removes the plan of a step whose frame returned, once the thread is
    /// in the frame it returned to or one further out, so that it goes on
    /// by single steps: the plan the step began with, or the guard on a
    /// callee's return. A thread stopped at one of the plan's traps is
    /// repaired first: this runs once it has stepped off.
    fn retire_returned_plan(&mut self, pid: Pid) -> Result<()> {
        let plan = self
            .inferior
            .as_ref()
            .and_then(|inferior| inferior.active.as_ref())
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

    /// Lets a source step that returned into code without source run on:
    /// its plan is removed, and no stop of its thread completes it, so only
    /// a stop the user sees, such as a breakpoint's, ends it.
    pub(super) fn let_step_run_on(&mut self) -> Result<()> {
        let execution = self.active_execution()?;
        self.retire_step_plan(execution)?;
        if let Some(start) = self.active_step_mut() {
            start.running_on = true;
            start.plan_addresses.clear();
        }

        Ok(())
    }

    /// Source stepping has no truthful stop to publish after its starting
    /// activation returns into code for which the debugger has no source.
    /// Keep the operation active so an exit, signal, or user breakpoint is
    /// reported instead of exposing an unusable synthetic source stop.
    pub(super) fn source_step_returned_to_undescribed_code(
        &self,
        pid: Pid,
        kind: StepKind,
    ) -> Result<bool> {
        if !matches!(kind, StepKind::OverSource | StepKind::Out) {
            return Ok(false);
        }

        let start = self
            .inferior
            .as_ref()
            .and_then(|inferior| inferior.active.as_ref())
            .and_then(|active| match &active.kind {
                ActiveKind::Step { start, .. } => Some(start),
                _ => None,
            })
            .ok_or(Error::LocationUnavailable)?;

        // A step over begun where no frame could be found steps as stepping
        // in does, and has no activation to return from.
        let Some(activation) = start.activation.filter(|_| !start.running_on) else {
            return Ok(false);
        };

        let registers = self.ptrace.registers(pid)?;
        Ok(
            activation.has_returned(self.stack_position(pid, &registers))
                && self
                    .image_location(VirtualAddress::new(registers.rip))
                    .is_none_or(|location| undescribed(&location)),
        )
    }

    /// At a DWARF `epilogue_begin` row, guards the caller's return address and
    /// next statements, unwinding before the frame's teardown begins: a
    /// partly torn-down frame cannot be unwound reliably.
    ///
    /// Only source steps cross an epilogue, and only the stepping frame's or
    /// one it returned to: a callee's, which a step over reaches by skipping
    /// a hit inside the call, returns to a frame the step has not finished.
    pub(super) fn begin_epilogue_traversal(&mut self, pid: Pid) -> Result<bool> {
        let (execution, already_traversing, start_source, kind, activation) = self
            .inferior
            .as_ref()
            .and_then(|inferior| inferior.active.as_ref())
            .and_then(|active| match &active.kind {
                ActiveKind::Step {
                    owner, start, kind, ..
                } if self.runs_step(*owner, pid) => Some((
                    active.id,
                    start.epilogue_traversal.is_some() || start.return_traversal.is_some(),
                    start.source.clone(),
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
        let (loaded_module, image_address) = {
            let inferior = self.inferior.as_ref().ok_or(Error::NotRunning)?;
            (
                inferior.loaded_module,
                inferior.loaded_module.image_address(instruction).ok(),
            )
        };

        let Some(image_address) = image_address else {
            return Ok(false);
        };

        let location = self.module_image.locate(image_address);
        if location.physical_instance.is_none()
            || !self
                .module_image
                .control_boundaries_at(image_address)
                .any(|row| row.flags.epilogue_begin())
        {
            return Ok(false);
        }

        // Without unwind evidence for the return, step through the epilogue
        // instruction by instruction instead.
        let Ok(return_address) = self.caller_address(pid, &registers) else {
            return Ok(false);
        };

        // Nor does a step end in a caller it passes over, such as the
        // runtime's switch back from C's stack; it goes on from there.
        let passed_over = self.code_role(return_address).is_some_and(|role| {
            self.active_step()
                .is_some_and(|start| passes_over(role, start))
        });
        if passed_over {
            return Ok(false);
        }

        let Ok(caller_image) = loaded_module.image_address(return_address) else {
            return Ok(false);
        };

        let completion_addresses =
            self.caller_statements(loaded_module, caller_image, start_source.as_ref())?;

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

    /// The statements of the function containing `caller_image` where a
    /// source step from `start_source` that returned there may end: those
    /// on other lines, outside epilogues.
    fn caller_statements(
        &self,
        loaded_module: LoadedModule,
        caller_image: ImageAddress,
        start_source: Option<&SourceLocation>,
    ) -> Result<BTreeSet<VirtualAddress>> {
        let mut statements = BTreeSet::new();
        let caller_location = self.module_image.locate(caller_image);
        let Some(caller_instance_id) = caller_location.physical_instance else {
            return Ok(statements);
        };

        let Some(caller_instance) = self.module_image.code_instance(caller_instance_id) else {
            return Ok(statements);
        };

        for line in self.module_image.line_entries_in(caller_instance) {
            if !line.statement {
                continue;
            }

            if self
                .module_image
                .control_boundaries_at(line.range.start)
                .any(|row| row.flags.epilogue_begin())
            {
                continue;
            }

            let candidate_location = self.module_image.locate(line.range.start);
            if source_for_code_instance(&self.module_image, &candidate_location, caller_instance_id)
                .is_some_and(|candidate| source_line_changed(start_source, Some(&candidate)))
            {
                statements.insert(loaded_module.virtual_address(line.range.start)?);
            }
        }

        Ok(statements)
    }

    /// Runs through a frame that must not become a source-step destination:
    /// a tail call that replaced the starting frame at the same CFA, or a
    /// callee entered while stepping an inline frame. Its return address is
    /// guarded only when call-frame information and the `[CFA - 8]` return
    /// slot agree; otherwise the step goes on by single steps.
    pub(super) fn begin_return_traversal(&mut self, pid: Pid) -> Result<bool> {
        let (execution, already_traversing, activation, start_instance, start_physical) = self
            .inferior
            .as_ref()
            .and_then(|inferior| inferior.active.as_ref())
            .and_then(|active| match &active.kind {
                ActiveKind::Step { owner, start, .. } if self.runs_step(*owner, pid) => Some((
                    active.id,
                    start.return_traversal.is_some() || start.epilogue_traversal.is_some(),
                    start.activation,
                    start.code_instance,
                    start.physical_instance,
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
            self.location_for_activation(pid, &registers, activation)?
        else {
            return Ok(false);
        };

        let Ok(current_activation) = self.top_activation(pid, &registers) else {
            return Ok(false);
        };

        let tail_replacement = starting_activation_location
            .physical_instance
            .is_some_and(|current_physical| current_physical != start_physical);
        let selected_is_inline = self
            .module_image
            .code_instance(start_instance)
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

    /// Removes the caller guard after its original instruction has been
    /// repaired. If no caller statement breakpoint remains, source stepping
    /// safely falls back to instruction stepping in the now-valid caller.
    ///
    /// A step in, which otherwise always moves by single steps, drops the
    /// caller's statement breakpoints too: the caller may return before it
    /// reaches any of them, and they would then let the thread run on.
    pub(super) fn retire_epilogue_return_guard(&mut self) -> Result<()> {
        let retirement = self
            .inferior
            .as_ref()
            .and_then(|inferior| inferior.active.as_ref())
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

    /// Removes a return guard after its activation reached it without yet
    /// finding a valid source destination. Deeper activations that share a
    /// tail-call guard leave it installed.
    pub(super) fn retire_return_guard(&mut self) -> Result<()> {
        let retirement = self
            .inferior
            .as_ref()
            .and_then(|inferior| inferior.active.as_ref())
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

    /// Whether the thread stopped at `instruction` before an installed
    /// breakpoint's `int3`, which it has yet to hit.
    fn before_breakpoint(&self, pid: Pid, instruction: VirtualAddress) -> Result<bool> {
        let inferior = self.inferior.as_ref().ok_or(Error::NotRunning)?;
        let hit = inferior
            .threads
            .get(&pid)
            .is_some_and(|thread| thread.stopped_at_breakpoint == Some(instruction));
        Ok(!hit
            && inferior
                .breakpoints
                .get(&instruction)
                .is_some_and(|site| site.installed))
    }

    /// Whether the step is complete where its thread stopped, or is left
    /// for the hit of a trap the thread stands before to judge.
    fn step_completion(&self, pid: Pid, kind: StepKind) -> Result<Completion> {
        if kind == StepKind::Instruction {
            return Ok(Completion::Complete);
        }

        let registers = self.ptrace.registers(pid)?;
        let start = self
            .inferior
            .as_ref()
            .and_then(|inferior| inferior.active.as_ref())
            .and_then(|active| match &active.kind {
                ActiveKind::Step { start, .. } => Some(start),
                _ => None,
            })
            .ok_or_else(|| Error::internal("source step has a starting state"))?;

        // Waiting for its loop, a step completes only at its plan's
        // breakpoints.
        if start.running_on
            || start.loops.as_ref().is_some_and(StepLoops::waits)
            || super::awaits::waits(start)
        {
            return Ok(Completion::Incomplete);
        }

        if kind == StepKind::OverInstruction {
            // A stepped-over call completes when it returns to its caller's
            // stack, not when recursion reaches the same return address.
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
            // A trace trap can land immediately before an installed `int3`.
            // Resume once so breakpoint classification and user-over-plan
            // precedence happen through the normal breakpoint path.
            return Ok(Completion::AtTrap(instruction));
        }

        self.reaches_step_destination(pid, &registers, start, kind, instruction)
            .map(Completion::from)
    }

    /// Whether the thread's instruction is where a source step `kind` from
    /// `start` ends.
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
            // Begun in code no debug information describes, a step over
            // has no source line to step over: it ends at the first source
            // statement it reaches, as stepping in does, and needs no frame.
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

                let Some(location) = self.location_for_activation(pid, registers, activation)?
                else {
                    return Ok(self.image_location(instruction).is_some_and(|location| {
                        source_step_destination(&self.module_image, &location, kind)
                    }));
                };

                if let Some(complete) =
                    inline_loop_step_is_complete(&self.module_image, &location, start, kind)
                {
                    return Ok(complete);
                }

                if !code_instance_is_active(&location, code_instance) {
                    // A different physical frame at the same live CFA is a
                    // tail-called replacement, not the caller. Keep stepping
                    // when its return address could not be independently
                    // proven for accelerated traversal.
                    let returned = activation.has_returned(self.stack_position(pid, registers));
                    if location.physical_instance != start.physical_instance && !returned {
                        return Ok(false);
                    }

                    if !returned && self.left_inlined_body_pending(pid, start, instruction) {
                        return Ok(false);
                    }

                    return Ok(source_step_destination(&self.module_image, &location, kind));
                }

                let source = source_for_code_instance(&self.module_image, &location, code_instance);

                Ok(kind == StepKind::OverSource
                    && source_step_destination(&self.module_image, &location, kind)
                    && source_line_changed(start.source.as_ref(), source.as_ref()))
            }
        }
    }

    /// Whether a step that is traversing a return or an epilogue is
    /// complete, or `None` when no traversal decides.
    fn traversal_step_is_complete(
        &self,
        pid: Pid,
        registers: &libc::user_regs_struct,
        start: &StepStart,
        kind: StepKind,
    ) -> Option<bool> {
        let instruction = VirtualAddress::new(registers.rip);
        let stopped_at_breakpoint = self
            .inferior
            .as_ref()
            .and_then(|inferior| inferior.threads.get(&pid))
            .and_then(|thread| thread.stopped_at_breakpoint);
        if let Some(traversal) = &start.return_traversal {
            if stopped_at_breakpoint != Some(instruction) || instruction != traversal.return_address
            {
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

                return Some(self.image_location(instruction).is_some_and(|location| {
                    source_step_destination(&self.module_image, &location, kind)
                        && source_line_changed(start.source.as_ref(), location.source.as_ref())
                }));
            }
        }

        start.epilogue_traversal.as_ref().map(|traversal| {
            stopped_at_breakpoint == Some(instruction)
                && traversal.completion_addresses.contains(&instruction)
        })
    }

    /// Whether a step over or out whose frame returned is complete: in the
    /// frame it returned to, or one further out, never in a callee of
    /// theirs, at a source statement for a step over and at code a line
    /// describes for a step out.
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
                .image_location(VirtualAddress::new(registers.rip))
                .is_some_and(|location| {
                    source_step_destination(&self.module_image, &location, kind)
                })
    }

    /// Whether a step out begun in code no debug information describes,
    /// such as a program's entry point, is complete: once its activation
    /// has returned to a source statement.
    fn undescribed_step_out_is_complete(
        &self,
        pid: Pid,
        registers: &libc::user_regs_struct,
        activation: Activation,
    ) -> Result<bool> {
        Ok(self
            .location_for_activation(pid, registers, activation)?
            .is_none()
            && self
                .image_location(VirtualAddress::new(registers.rip))
                .is_some_and(|location| {
                    source_step_destination(&self.module_image, &location, StepKind::Out)
                }))
    }

    pub(super) fn step_into_source_is_complete(
        &self,
        pid: Pid,
        registers: &libc::user_regs_struct,
        start: &StepStart,
    ) -> Result<bool> {
        let location = self.image_location(VirtualAddress::new(registers.rip));

        // Undescribed instructions are not source-step destinations, and
        // neither is code the program's author did not write.
        let Some(described) = location.as_ref().filter(|location| !undescribed(location)) else {
            return Ok(false);
        };

        let presentation = self.presentation_for_thread(
            pid,
            Some(&StopReason::Step {
                kind: StepKind::IntoSource,
            }),
        )?;
        let current_instance = selected_code_instance(described, &presentation)?;
        if self.runs_for_runtime(described, current_instance, start) {
            return Ok(false);
        }

        let physical_role = self.code_role(VirtualAddress::new(registers.rip));

        // Code inlined into other code is its own function's, as the
        // program's function a wrapper calls may be.
        let role = current_instance
            .filter(|instance| described.physical_instance != Some(*instance))
            .and_then(|instance| self.module_image.code_instance(instance))
            .and_then(|instance| self.module_image.function(instance.function()))
            .map(crate::image::functions::Function::role)
            .or(physical_role);
        if role.is_some_and(|role| passes_over(role, start)) {
            return Ok(false);
        }

        // Such code begins past the entry of the code it is inlined into,
        // which the step passed over.
        let inlined_in_passed = physical_role.is_some_and(|role| passes_over(role, start));
        let source = location.as_ref().and_then(|location| {
            current_instance.and_then(|instance| {
                source_for_code_instance(&self.module_image, location, instance)
            })
        });

        // What leads into an async body, its dispatch and the moves of its
        // arguments, carries the body's header line; the step goes on into
        // the body.
        let leads_into_body = current_instance
            .and_then(|instance| self.module_image.code_instance(instance))
            .and_then(|instance| self.module_image.function(instance.function()))
            .filter(|function| function.coroutine().is_some())
            .and_then(crate::image::functions::Function::declaration)
            .zip(source.as_ref())
            .is_some_and(|(header, at)| header.file == at.file && header.line == at.line);
        if leads_into_body {
            return Ok(false);
        }

        // As when it starts, stepping in never requires the activation: code
        // without unwind information is judged by its location alone.
        let activation = self.top_activation(pid, registers).ok();
        let statement = location.as_ref().is_some_and(|location| {
            self.module_image
                .line_entry_containing(location.address)
                .is_some_and(|entry| entry.statement)
        });
        let current_physical = location
            .as_ref()
            .and_then(|location| location.physical_instance);
        let activation_changed = activation
            .zip(start.activation)
            .is_some_and(|(current, start)| current != start);
        let entered_physical_activation = match start.activation {
            Some(start_activation) if activation_changed => self
                .location_for_activation(pid, registers, start_activation)?
                .is_some(),
            Some(_) => current_physical.is_some() && current_physical != start.physical_instance,
            // Without the starting activation, as in code without unwind
            // information, a different function deeper on the stack than
            // where the step began was entered by a call.
            None => {
                current_physical.is_some()
                    && current_physical != start.physical_instance
                    && start.stack_pointer.is_some_and(|start| {
                        self.stack_position(pid, registers).is_deeper_than(start)
                    })
            }
        };

        // A recommended entry row may carry no source attribution. Source
        // stepping must keep going until it can publish a renderable source
        // stop, so a source-less entry is a waypoint, not a destination.
        let at_recommended_entry = location.as_ref().is_some_and(|location| {
            location.source.is_some()
                && location.physical_instance.is_some_and(|instance| {
                    self.module_image
                        .recommended_entries_for_instance(instance)
                        .any(|entry| entry.address == location.address)
                })
        });
        if entered_physical_activation && !at_recommended_entry && !inlined_in_passed {
            return Ok(false);
        }

        Ok(
            (statement || entered_physical_activation && at_recommended_entry)
                && (activation_changed
                    || current_instance != start.code_instance
                    || source_line_changed(start.source.as_ref(), source.as_ref())),
        )
    }

    /// Whether code the step reached runs for runtime code it does not
    /// enter: inlined into that code, as the standard library's code the
    /// runtime runs is, or into what that code is inlined into. Such code
    /// is the runtime's work; the step leaves it for the runtime's caller.
    fn runs_for_runtime(
        &self,
        location: &ImageLocation,
        current: Option<CodeInstanceId>,
        start: &StepStart,
    ) -> bool {
        let passed = |instance: &CodeInstanceId| {
            self.module_image
                .code_instance(*instance)
                .and_then(|instance| self.module_image.function(instance.function()))
                .is_some_and(|function| is_runtime_role(function.role()) && !start.enters_runtime)
        };

        let enclosing = match &location.inline_frames {
            InlineFrameLookup::Unique(chain) => {
                let inner = current
                    .and_then(|current| chain.instances.iter().position(|id| *id == current))
                    .unwrap_or(0);
                &chain.instances[..inner]
            }
            _ => &[],
        };

        location.physical_instance != current
            && (location.physical_instance.iter().any(passed) || enclosing.iter().any(passed))
    }

    pub(super) fn step_start(
        &self,
        pid: Pid,
        kind: StepKind,
        frame: StackFrameId,
    ) -> Result<StepStart> {
        let inferior = self.inferior.as_ref().ok_or(Error::NotRunning)?;
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

    /// How a step from a stopped thread's innermost activation begins, in
    /// the logical frame `presentation` selects.
    pub(super) fn innermost_step_start(
        &self,
        pid: Pid,
        kind: StepKind,
        presentation: impl FnOnce() -> Result<FramePresentation>,
    ) -> Result<StepStart> {
        let inferior = self.inferior.as_ref().ok_or(Error::NotRunning)?;
        let registers = self.ptrace.registers(pid)?;
        let location = self.image_location(VirtualAddress::new(registers.rip));

        // An instruction step executes one instruction whichever frame is
        // presented, so only source steps need the selected one.
        let code_instance = if steps_instructions(kind) {
            None
        } else {
            let presentation = presentation()?;
            location
                .as_ref()
                .map(|location| selected_code_instance(location, &presentation))
                .transpose()?
                .flatten()
        };

        let source = location.as_ref().and_then(|location| {
            code_instance.and_then(|instance| {
                source_for_code_instance(&self.module_image, location, instance)
            })
        });

        // Stepping into source never needs the activation, so a thread
        // stopped in code without unwind information can still step in.
        // Neither does stepping over from code no debug information
        // describes, which steps as stepping in does.
        let activation = match kind {
            StepKind::Advance => return Err(Error::internal("an advance runs as a step out")),
            StepKind::IntoNewTask => return Err(Error::internal(STEPS_OVER)),
            StepKind::Instruction | StepKind::OverInstruction => None,
            StepKind::IntoSource => self.top_activation(pid, &registers).ok(),
            StepKind::OverSource if code_instance.is_none() => {
                self.top_activation(pid, &registers).ok()
            }
            StepKind::OverSource | StepKind::Out => Some(self.top_activation(pid, &registers)?),
        };

        let mut plan_addresses = BTreeSet::new();
        let call_return = if kind == StepKind::OverInstruction {
            self.call_return(inferior, pid, &registers)?
        } else {
            None
        };

        plan_addresses.extend(call_return.map(|(address, _)| address));

        let selected_is_inline = code_instance
            .and_then(|instance| self.module_image.code_instance(instance))
            .is_some_and(|instance| matches!(instance.kind(), CodeInstanceKind::Inline { .. }));

        // An inline instance has no stack return address of its own. Leaving
        // its physical caller's return address as the only reachable plan
        // breakpoint would run the entire containing activation. Instruction
        // stepping lets `step_is_complete` observe either the next statement
        // in this instance or the point where the logical frame disappears.
        let mut returning = None;
        if kind == StepKind::Out && !selected_is_inline {
            let return_address = self.caller_address(pid, &registers)?;
            plan_addresses.insert(return_address);
            returning = activation
                .zip(location.as_ref())
                .map(|(activation, location)| Returning {
                    activation,
                    return_address,
                    function: location.address,
                });
        } else if kind == StepKind::OverSource
            && !selected_is_inline
            && let (Some(source), Some(instance_id)) = (&source, code_instance)
        {
            self.plan_source_lines(pid, &registers, source, instance_id, &mut plan_addresses)?;
        }

        let panic_guards = if matches!(kind, StepKind::OverSource | StepKind::Out) {
            self.panic_entries(inferior)
        } else {
            BTreeSet::new()
        };

        let loops = if matches!(kind, StepKind::OverSource | StepKind::Out) {
            self.step_loops(
                pid,
                &registers,
                kind,
                code_instance,
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
            source,
            code_instance,
            physical_instance: location
                .as_ref()
                .and_then(|location| location.physical_instance),
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

    /// Plans a step over by line in a frame of `instance_id`: a breakpoint
    /// at each statement of the instance on another line than `source`,
    /// and at its return address.
    fn plan_source_lines(
        &self,
        pid: Pid,
        registers: &libc::user_regs_struct,
        source: &SourceLocation,
        instance_id: crate::CodeInstanceId,
        plan_addresses: &mut BTreeSet<VirtualAddress>,
    ) -> Result<()> {
        if self.module_image.code_instance(instance_id).is_none() {
            return Ok(());
        }

        // A frame without a trustworthy caller, such as a coroutine's
        // first frame or one a stack overflow corrupted, still steps by
        // line. Its return, if it comes, is then followed like a return
        // into code without source.
        let return_address = match self.caller_address(pid, registers) {
            Ok(address) => Some(address),
            Err(error) if is_caller_unavailable(&error) => None,
            Err(error) => return Err(error),
        };

        plan_addresses.extend(self.other_lines(instance_id, source)?);
        plan_addresses.extend(return_address);
        Ok(())
    }

    /// The statements of the code instance `instance_id` on other lines
    /// than `source`.
    pub(super) fn other_lines(
        &self,
        instance_id: crate::CodeInstanceId,
        source: &SourceLocation,
    ) -> Result<BTreeSet<VirtualAddress>> {
        self.statements_of(instance_id, Some(source))
    }

    /// The statements of the code instance `instance_id`, on any line but
    /// `except`'s.
    pub(super) fn statements_of(
        &self,
        instance_id: crate::CodeInstanceId,
        except: Option<&SourceLocation>,
    ) -> Result<BTreeSet<VirtualAddress>> {
        let inferior = self.inferior.as_ref().ok_or(Error::NotRunning)?;
        let Some(instance) = self.module_image.code_instance(instance_id) else {
            return Ok(BTreeSet::new());
        };

        let mut statements = BTreeSet::new();
        for line in self.module_image.line_entries_in(instance) {
            if !line.statement {
                continue;
            }

            let location = self.module_image.locate(line.range.start);
            if source_for_code_instance(&self.module_image, &location, instance_id).is_some_and(
                |candidate| {
                    except.is_none_or(|source| source_line_changed(Some(source), Some(&candidate)))
                },
            ) {
                statements.insert(inferior.loaded_module.virtual_address(line.range.start)?);
            }
        }

        Ok(statements)
    }

    /// Returns the return address and stack pointer of the call instruction
    /// a thread is about to execute, or `None` for any other instruction.
    pub(super) fn call_return(
        &self,
        inferior: &Inferior,
        pid: Pid,
        registers: &libc::user_regs_struct,
    ) -> Result<Option<(VirtualAddress, StackPosition)>> {
        let mut decoder = decoder_for(self.module_image.target(), AssemblySyntax::Intel)?;
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

    /// Plans running until a frame other than the innermost one returns.
    ///
    /// A logical frame of the innermost activation steps out like the
    /// innermost frame does. An outer activation runs to its return
    /// address, which recursion may reach first from deeper activations;
    /// completion requires the selected activation itself to have returned.
    fn outer_step_out_start(
        &self,
        pid: Pid,
        registers: &libc::user_regs_struct,
        frame: StackFrameId,
    ) -> Result<StepStart> {
        let inferior = self.inferior.as_ref().ok_or(Error::NotRunning)?;
        let resolved = self.resolve_frame(inferior, &StackRoot::of_thread(pid), frame)?;
        let selected = resolved.frame.as_ref().ok_or(Error::AmbiguousInlineFrame)?;

        // Source stepping plans are limited to code the main image describes.
        let Some((_, address)) = resolved
            .code
            .filter(|(module, _)| *module == inferior.loaded_module.id)
        else {
            return Err(Error::FrameStepUnsupported(
                "stepping out applies only to frames of the main executable".into(),
            ));
        };

        let location = self.module_image.locate(address);
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
                function: address,
            }),
            ..StepStart::default()
        })
    }

    /// The innermost activation of a stopped thread.
    pub(super) fn top_activation(
        &self,
        pid: Pid,
        native: &libc::user_regs_struct,
    ) -> Result<Activation> {
        Ok(self.stack_view(pid).activation(self.top_cfa(pid, native)?))
    }

    /// The canonical frame address of a stopped thread's innermost frame.
    pub(super) fn top_cfa(
        &self,
        pid: Pid,
        native: &libc::user_regs_struct,
    ) -> Result<VirtualAddress> {
        let inferior = self.inferior.as_ref().ok_or(Error::NotRunning)?;
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
        // A live activation's CFA remains beyond the stack pointer. Once the
        // stack pointer reaches it, the return has already restored the
        // caller's stack. Recognize that transition without unwinding.
        if activation.has_returned(self.stack_position(pid, native)) {
            return Ok(None);
        }

        let view = self.stack_view(pid);
        let inferior = self.inferior.as_ref().ok_or(Error::NotRunning)?;

        // The activation may be on another stack than the thread is now,
        // such as a goroutine's that called C a callback runs below.
        let (_, _, found) = self.walk_stack(
            inferior,
            &StackRoot::of_thread(pid),
            Some(native),
            |provider, innermost| {
                self.find_activation(inferior, view, provider, innermost, activation)
            },
        )?;
        found
    }

    /// Walks a stack outward to `activation`: where it runs, or `None`
    /// once the walk passes it, as when it has returned.
    fn find_activation(
        &self,
        inferior: &Inferior,
        view: StackView,
        provider: &mut RoleCallerProvider<'_, '_>,
        mut context: FrameContext,
        activation: Activation,
    ) -> Result<Option<ImageLocation>> {
        for level in 0..DEFAULT_MAX_FRAMES {
            // Each frame is known by its own CFA, so the starting activation
            // is recognized even when its return address cannot be read.
            let frame = view.activation(
                provider
                    .dwarf
                    .frame_cfa(&context)
                    .map_err(|reason| backend_error(LinuxError::CallerUnavailable(reason)))?,
            );
            if frame == activation {
                let level =
                    u32::try_from(level).map_err(|_| Error::internal("frame limit fits u32"))?;
                let location = frame_lookup_address(level, &context)
                    .and_then(|address| inferior.loaded_module.image_address(address).ok())
                    .filter(|address| self.module_image.contains_address(*address))
                    .map(|address| self.module_image.locate(address));

                return Ok(location);
            }

            // Once unwinding passes the starting activation, it has returned.
            if activation.is_callee_of(frame) {
                return Ok(None);
            }

            context = match provider.caller(&context) {
                CallerResult::Caller(caller) => caller,
                // The whole stack was walked without meeting the activation,
                // so it is not live, as for a thread whose first frame began
                // in its creator's function after a raw clone.
                CallerResult::Finished(crate::UnwindTermination::Complete) => return Ok(None),
                CallerResult::Finished(reason) => {
                    return Err(backend_error(LinuxError::CallerUnavailable(reason)));
                }
            };
        }

        Ok(None)
    }

    /// The innermost activation's return address, where a step plants a
    /// trap to regain control once the activation returns.
    pub(super) fn caller_address(
        &self,
        pid: Pid,
        native: &libc::user_regs_struct,
    ) -> Result<VirtualAddress> {
        let inferior = self.inferior.as_ref().ok_or(Error::NotRunning)?;
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

    /// Accepts an unwound return address as a trap site only where the
    /// process executes code. A corrupted stack can name any address,
    /// and a trap planted in data would change what the program reads.
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

    /// Unwinds `pid`'s stack from `native` through every loaded module's
    /// call-frame information.
    pub(super) fn stack_unwinder<'a>(
        &'a self,
        inferior: &Inferior,
        pid: Pid,
        native: &libc::user_regs_struct,
    ) -> DwarfCallerProvider<'a> {
        DwarfCallerProvider {
            modules: self.unwind_modules(inferior),
            registers: x86_64_registers(native),
            memory: PtraceMemory {
                ptrace: &self.ptrace,
                pid,
            },
            first: true,
        }
    }

    /// The program's code that the runtime function a stopped thread is
    /// entering goes on to call, where that is executable.
    fn call_out(&self, pid: Pid, registers: &libc::user_regs_struct) -> Option<VirtualAddress> {
        let inferior = self.inferior.as_ref()?;
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

    /// What the physical function holding `address` is to stepping, not
    /// any function inlined into it there, in whichever loaded module holds
    /// it; `None` when no module does.
    pub(super) fn code_role(&self, address: VirtualAddress) -> Option<CodeRole> {
        let inferior = self.inferior.as_ref()?;
        let (image, address) = self.module_holding(inferior, address)?;
        Some(image.code_role(address))
    }

    pub(super) fn image_location(&self, address: VirtualAddress) -> Option<ImageLocation> {
        let inferior = self.inferior.as_ref()?;
        let image = inferior.loaded_module.image_address(address).ok()?;
        Some(self.module_image.locate(image))
    }
}

pub(super) const fn innermost_frame(native: &libc::user_regs_struct) -> FrameContext {
    FrameContext {
        instruction: VirtualAddress::new(native.rip),
        cfa: None,
        signal_frame: false,
    }
}

/// Why no step runs as a step into a new task: run control steps over its
/// line until a task starts (see `new_task`).
const STEPS_OVER: &str = "a step into a new task runs as a step over";

/// Whether code in this role is a language runtime's own: its machinery,
/// an interpreter's, its outermost frames, and what it enters by a trap.
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

/// Whether a source step goes on through code in this role rather than
/// end there: code the program's author did not write, except the
/// runtime's own machinery when the step may enter it. Wrappers and the
/// code that begins a panic are stepped through to what they call.
const fn passes_over(role: CodeRole, start: &StepStart) -> bool {
    match role {
        CodeRole::Wrapper | CodeRole::StackSwitch | CodeRole::Panic => true,
        role => is_runtime_role(role) && !start.enters_runtime,
    }
}

/// Whether code without debug information, such as a PLT stub or a
/// library, holds `location`.
const fn undescribed(location: &ImageLocation) -> bool {
    location.physical_instance.is_none() && location.source.is_none()
}

/// What judging a step where its thread stopped found.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Completion {
    Complete,
    Incomplete,

    /// Nothing yet: the thread stands before the `int3` at this address,
    /// and that trap's hit judges the arrival.
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

/// Whether unwinding found no caller the debugger can trust.
fn is_caller_unavailable(error: &Error) -> bool {
    matches!(
        error,
        Error::Backend(error)
            if matches!(error.downcast_ref::<LinuxError>(), Some(LinuxError::CallerUnavailable(_)))
    )
}

/// Whether a step failed for lack of the evidence it is followed by, such
/// as a frame's caller or location, rather than because tracing failed.
fn is_lost_step_evidence(error: &Error) -> bool {
    matches!(error, Error::LocationUnavailable) || is_caller_unavailable(error)
}

fn step_incomplete(kind: StepKind, error: &Error) -> StopReason {
    // A backend error's own message, without the prefix naming its layer.
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
