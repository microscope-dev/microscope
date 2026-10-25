//! Stack unwinding and the logical frames presented for inline code.

use std::collections::BTreeSet;
use std::sync::Arc;

use nix::libc;
use nix::unistd::Pid;

use crate::debug_info::{TailJump, VariableRuntimeError};
use crate::model::FrameMetadata;
use crate::protocol::{FramePresentation, PresentedFrame, StepKind, StopId, StopReason};
use crate::runtime_model::{Crossing, InterpretedFrame, InterpretedFrameRef, futures};
use crate::unwind::{
    CallerProvider, CallerResult, DEFAULT_MAX_FRAMES, FrameContext, MemoryReader, RETURN_ADDRESS,
    RSP, RegisterFile, collect_frames,
};

pub(super) use crate::unwind::{UnwindModule, unwind_module_for};
use crate::{
    AddressDescription, Backtrace, CallFrameUnavailableReason, CodeInstanceId, CodeInstanceKind,
    CodeRole, Error, ExecutionContext, ExecutionLocation, FrameKind, ImageAddress, ImageLocation,
    InlineFrameLookup, LoadedModule, ModuleAddress, ModuleId, ModuleImage, Result, SourceLocation,
    StackFrame, StackFrameId, StackSegment, TypeReference, UnwindTermination,
    VariableUnavailableReason, VirtualAddress,
};

use super::activation::{StackPosition, StackView};
use super::breakpoints::runtime_breakpoint_address;
use super::callers::Callers;
use super::loops::is_loop_body;
use super::memory::PtraceMemory;
use super::native::InspectionOps;
use super::registers::x86_64_registers;
use super::{
    BreakpointOwner, Controller, Inferior, StepOwner, debug_thread_id, validate_image_current,
    validate_public_stop, validate_stopped_thread,
};

impl<P: InspectionOps> Controller<P> {
    pub(super) fn presentation_for_stopped_thread(&self, pid: Pid) -> Result<FramePresentation> {
        let inferior = self.inferior.as_ref().ok_or(Error::NotRunning)?;
        validate_public_stop(inferior, inferior.public_stop.as_ref().map(|stop| stop.id))?;
        validate_stopped_thread(inferior, pid)?;
        let stop = inferior.public_stop.as_ref().ok_or(Error::NotStopped)?;

        // The triggering thread's presentation is cached when the stop is
        // published. Another thread is presented by its own stop reason, or
        // by default when it merely stopped with its siblings.
        stop.presentations.get(&pid).cloned().map_or_else(
            || self.presentation_for_thread(pid, inferior.thread(pid)?.reason.as_ref()),
            Ok,
        )
    }

    /// Chooses the logical frame to present for a stopped thread. `reason` is
    /// the thread's own stop reason, which can reveal or select inline frames.
    pub(super) fn presentation_for_thread(
        &self,
        pid: Pid,
        reason: Option<&StopReason>,
    ) -> Result<FramePresentation> {
        let inferior = self.inferior.as_ref().ok_or(Error::NotRunning)?;
        let registers = self.ptrace.registers(pid)?;
        let instruction = VirtualAddress::new(registers.rip);
        let physical = || FramePresentation {
            instruction,
            frame: PresentedFrame::Physical,
            hidden_inline_frames: 0,
        };

        // Presentations describe inline frames of the main image only; an
        // instruction elsewhere, or anywhere after exec replaced the image,
        // is presented as its physical frame.
        let Some(image_address) = inferior
            .loaded_module
            .image_address(instruction)
            .ok()
            .filter(|address| {
                inferior.unfollowed_exec.is_none() && self.main.image.contains_address(*address)
            })
        else {
            return Ok(physical());
        };

        let location = self.main.image.locate(image_address);
        let chain = match &location.inline_frames {
            InlineFrameLookup::Unique(chain) => chain,
            InlineFrameLookup::Ambiguous(chains) => {
                return Ok(FramePresentation {
                    instruction,
                    frame: PresentedFrame::Ambiguous(
                        chains
                            .iter()
                            .flat_map(|chain| chain.instances.iter().copied())
                            .collect::<BTreeSet<_>>()
                            .into_iter()
                            .collect(),
                    ),
                    hidden_inline_frames: 0,
                });
            }
            InlineFrameLookup::None => return Ok(physical()),
        };

        // Every instance a breakpoint hit lies in the one chain, outermost
        // first. Where several hit together, the stop presents the
        // innermost, as gdb does; the others are its callers.
        if let Some(StopReason::Breakpoint {
            address: Some(address),
            ..
        }) = reason
        {
            let hit = self.breakpoint_code_instances(inferior, *address)?;
            if let Some(target) = location
                .physical_instance
                .into_iter()
                .chain(chain.instances.iter().copied())
                .rfind(|instance| hit.contains(instance))
            {
                let visible = chain
                    .instances
                    .iter()
                    .position(|instance| *instance == target)
                    .map_or(0, |index| index + 1);
                return make_presentation(instruction, chain.instances.as_ref(), visible);
            }
        }

        let reveal_new_inline = match reason {
            Some(StopReason::Step {
                kind: StepKind::IntoSource,
            }) => true,
            Some(StopReason::Step {
                kind: StepKind::IntoNewTask,
            }) => self.entered_new_task(),
            _ => false,
        };

        let visible = default_inline_visible_count(
            &self.main.image,
            chain.instances.as_ref(),
            image_address,
            reveal_new_inline,
        );

        make_presentation(instruction, chain.instances.as_ref(), visible)
    }

    pub(super) fn breakpoint_code_instances(
        &self,
        inferior: &Inferior,
        address: VirtualAddress,
    ) -> Result<BTreeSet<CodeInstanceId>> {
        let Some(site) = inferior.breakpoints.get(&address) else {
            return Ok(BTreeSet::new());
        };

        let mut instances = BTreeSet::new();

        for id in site.owners.iter().filter_map(|owner| match owner {
            BreakpointOwner::User(id) => Some(*id),
            BreakpointOwner::Plan(_)
            | BreakpointOwner::Loader
            | BreakpointOwner::Runtime
            | BreakpointOwner::StackMove
            | BreakpointOwner::Resolver
            | BreakpointOwner::InspectorHook => None,
        }) {
            // A breakpoint removed while sites could not be edited, as SIGKILL
            // tears the process down, leaves its owner on the trap.
            let Some(breakpoint) = self
                .breakpoints
                .iter()
                .find(|breakpoint| breakpoint.id == id)
            else {
                continue;
            };

            for resolved in breakpoint.locations.iter() {
                if runtime_breakpoint_address(inferior, &resolved.location)? == Some(address) {
                    instances.extend(resolved.code_instances.iter().copied());
                }
            }
        }

        Ok(instances)
    }

    pub(super) fn stopped_location(
        &self,
        stop_id: StopId,
        root: &StackRoot,
        frame: StackFrameId,
    ) -> Result<ExecutionLocation> {
        let inferior = self.stopped_root(stop_id, root)?;

        // A task's saved registers locate its innermost frame as a caller's
        // are located, before the call it is parked in.
        let Some(pid) = root.thread() else {
            return self.outer_frame_location(inferior, root, frame);
        };

        // A thread stopped in an interpreter's loop has the frame the loop
        // runs innermost, before the loop's own.
        if frame.get() != 0 || self.interprets_on(inferior, root) {
            return self.outer_frame_location(inferior, root, frame);
        }

        let registers = self.ptrace.registers(pid)?;
        let address = VirtualAddress::new(registers.rip);
        let modules = self.unwind_modules(inferior);
        let (module, image_address) =
            unwind_module_for(&modules, address).ok_or(Error::AddressOutsideModule)?;
        let mut image = module.image.locate(image_address);

        // The stop presentation describes inline frames of the main image only.
        if module.loaded.id == inferior.loaded_module.id {
            let presentation = self.presentation_for_stopped_thread(pid)?;
            apply_presentation(&self.main.image, &mut image, &presentation)?;
        }

        Ok(ExecutionLocation {
            module: module.loaded.id,
            address,
            image,
        })
    }

    /// Describes a process address against the modules loaded at a stop.
    pub(super) fn describe_address(
        &self,
        stop_id: StopId,
        address: VirtualAddress,
    ) -> Result<AddressDescription> {
        let inferior = self.inferior.as_ref().ok_or(Error::NotRunning)?;
        validate_public_stop(inferior, Some(stop_id))?;
        validate_image_current(inferior)?;
        let modules = self.unwind_modules(inferior);
        Ok(describe_address(&modules, address))
    }

    /// A stack's frames, each future one awaits with what it waits for.
    pub(super) fn backtrace(&self, stop_id: StopId, root: &StackRoot) -> Result<Backtrace> {
        let inferior = self.stopped_root(stop_id, root)?;
        let mut trace = if let Some(trace) = self.async_backtrace(inferior, root)? {
            trace
        } else if let Some(trace) = super::interpreted::task_backtrace(root)? {
            trace
        } else {
            let presentation = self.root_presentation(root)?;
            let stack = self.physical_stack(inferior, root, DEFAULT_MAX_FRAMES)?;
            let modules = self.unwind_modules(inferior);
            self.expand_backtrace(
                inferior,
                root,
                &stack,
                DEFAULT_MAX_FRAMES,
                &modules,
                presentation.as_ref(),
            )?
            .trace
        };

        if trace
            .frames
            .iter()
            .any(|frame| matches!(frame.kind, FrameKind::Awaited { .. }))
        {
            let mut frames = trace.frames.to_vec();
            self.describe_awaited(inferior, stop_id, root, &mut frames);
            trace.frames = frames.into();
        }

        Ok(trace)
    }

    /// A stack's logical frames, with the frames of the functions that left
    /// by tail calls that its calls' sites and debug information find.
    pub(super) fn expand_backtrace(
        &self,
        inferior: &Inferior,
        root: &StackRoot,
        stack: &PhysicalStack,
        unwound: usize,
        modules: &[UnwindModule<'_>],
        presentation: Option<&FramePresentation>,
    ) -> Result<Expanded> {
        let callers = Callers::with_stack(self, inferior, root.clone(), stack.clone(), unwound);
        let mut expanded = expand_inline_backtrace(
            stack,
            root.context,
            modules,
            presentation,
            &mut |activation, code| callers.tail_jumps(activation, code),
        )?;
        self.describe_code_frames(inferior, root, stack, modules, &mut expanded)?;
        let mut expanded = self.splice_driven(inferior, root, stack, modules, expanded)?;
        inferior.inspectors.map_frames(&mut expanded.trace.frames);
        Ok(expanded)
    }

    /// The inferior, once `stop_id` is its current stop and `root` begins in
    /// one of its stopped threads or in a task's saved registers.
    pub(super) fn stopped_root(&self, stop_id: StopId, root: &StackRoot) -> Result<&Inferior> {
        self.stopped_inferior(stop_id, root.reader())
    }

    /// The logical frame a stack's innermost activation presents: the
    /// stop's choice for a thread, and every inline frame for a task's
    /// saved registers, which no stop reason selects among.
    pub(super) fn root_presentation(&self, root: &StackRoot) -> Result<Option<FramePresentation>> {
        root.thread()
            .map(|pid| self.presentation_for_stopped_thread(pid))
            .transpose()
    }

    /// Unwinds at most `max_frames` physical activations of a stack,
    /// keeping the registers the unwinder reconstructed for each.
    pub(super) fn physical_stack(
        &self,
        inferior: &Inferior,
        root: &StackRoot,
        max_frames: usize,
    ) -> Result<PhysicalStack> {
        let (native, after_call, (frames, termination)) =
            self.walk_stack(inferior, root, None, |provider, initial| {
                collect_frames(
                    initial,
                    provider,
                    |_, context, provider| PhysicalFrame {
                        context: context.clone(),
                        registers: provider.dwarf.registers.clone(),
                        segment: provider.segment(),
                    },
                    max_frames,
                )
            })?;
        Ok(PhysicalStack {
            native,
            after_call,
            frames,
            termination,
        })
    }

    /// Unwinds a stack a frame at a time, across the stacks its runtimes
    /// switch between: `walk` gets the unwinder and the innermost frame.
    /// A thread's stack begins at `native`, or else its live registers.
    /// Returns, beside what `walk` does, the thread's registers and whether
    /// the innermost instruction is a return address.
    pub(super) fn walk_stack<T>(
        &self,
        inferior: &Inferior,
        root: &StackRoot,
        native: Option<&libc::user_regs_struct>,
        walk: impl FnOnce(&mut RoleCallerProvider<'_, '_>, FrameContext) -> T,
    ) -> Result<(Option<libc::user_regs_struct>, bool, T)> {
        let runtimes = self.runtimes(inferior);
        let (native, registers, after_call, stacks) = match &root.origin {
            RootOrigin::Thread(pid) => {
                let native = match native {
                    Some(native) => *native,
                    None => self.ptrace.registers(*pid)?,
                };

                let stacks = self.thread_stacks(inferior, &runtimes, *pid);
                (Some(native), x86_64_registers(&native), false, stacks)
            }

            // A parked task's frames are all on its own stack.
            RootOrigin::Saved {
                registers,
                after_call,
                ..
            } => (None, registers.clone(), *after_call, Vec::new()),
            RootOrigin::Suspended { .. } => return Err(Error::FrameSuspended),
            RootOrigin::Interpreted { .. } => return Err(Error::FrameInterpreted),
        };

        let instruction = registers
            .get(RETURN_ADDRESS)
            .ok_or(Error::LocationUnavailable)?;
        let initial = FrameContext {
            instruction: VirtualAddress::new(instruction),
            cfa: None,
            signal_frame: false,
        };

        let mut cross = |module: ModuleId, registers: &RegisterFile, after_call: bool| {
            let runtime = runtimes
                .iter()
                .find(|runtime| runtime.module.id == module)?;

            // A parked task's frames are all on its own stack.
            let Some(pid) = root.thread() else {
                return Some(Ok(Crossing::Stay));
            };

            Some(self.with_runtime_stop(inferior, runtime, pid, |stop| {
                runtime
                    .model
                    .cross(stop, debug_thread_id(pid), registers, after_call)
            }))
        };

        let mut provider = RoleCallerProvider {
            dwarf: self.caller_provider(inferior, &runtimes, root.reader(), registers, !after_call),
            stacks,
            other: if root.thread().is_some() {
                StackSegment::Thread
            } else {
                StackSegment::Task
            },
            cross: &mut cross,
            carried: None,
            dispatched: false,
        };

        Ok((native, after_call, walk(&mut provider, initial)))
    }

    /// The stacks a thread runs on for the process's runtimes, and whose
    /// each is. A thread whose runtime state is unreadable has none, so its
    /// frames are on the thread's own stack.
    fn thread_stacks(
        &self,
        inferior: &Inferior,
        runtimes: &[super::runtimes::BoundRuntime],
        pid: Pid,
    ) -> Vec<(std::ops::Range<u64>, StackSegment)> {
        runtimes
            .iter()
            .filter_map(|runtime| {
                self.with_runtime_stop(inferior, runtime, pid, |stop| {
                    runtime.model.thread_stacks(stop, debug_thread_id(pid))
                })
                .ok()
            })
            .flatten()
            .collect()
    }

    /// Finds one logical frame of a stack, numbered as [`Self::backtrace`]
    /// presents it, and the state that evaluates its variables. Only the
    /// activations up to that frame are unwound.
    pub(super) fn resolve_frame(
        &self,
        inferior: &Inferior,
        root: &StackRoot,
        frame: StackFrameId,
    ) -> Result<ResolvedFrame> {
        let presentation = self.root_presentation(root)?;
        self.resolve_presented_frame(inferior, root, frame, presentation.as_ref())
    }

    /// Resolves a frame of a thread whose logical presentation is known,
    /// such as one stopped by a breakpoint hit before any stop is published.
    pub(super) fn resolve_presented_frame(
        &self,
        inferior: &Inferior,
        root: &StackRoot,
        frame: StackFrameId,
        presentation: Option<&FramePresentation>,
    ) -> Result<ResolvedFrame> {
        if let Some(resolved) = self.resolve_async_frame(inferior, root, frame)? {
            return Ok(resolved);
        }

        if let Some(resolved) = super::interpreted::resolve_task_frame(root, frame)? {
            return Ok(resolved);
        }

        let level = frame.get() as usize;

        // Every activation presents at least one logical frame, so unwinding
        // one activation per level always reaches the requested frame; but
        // interpreted frames are placed by every activation of their loop,
        // so a thread that runs them is unwound as its backtrace is.
        let max_frames = if self.interprets_on(inferior, root) {
            DEFAULT_MAX_FRAMES
        } else {
            level.saturating_add(1).min(DEFAULT_MAX_FRAMES)
        };
        let stack = self.physical_stack(inferior, root, max_frames)?;
        let modules = self.unwind_modules(inferior);

        // An innermost frame without one compatible inline chain still has
        // registers and code, but no single source scope or backtrace frame.
        if level == 0
            && let Some(presentation) = presentation
            && let PresentedFrame::Ambiguous(_) = presentation.frame
        {
            let innermost = &stack.frames[0];
            let code = unwind_module_for(&modules, innermost.context.instruction)
                .map(|(module, address)| (module.loaded.id, address));
            return Ok(ResolvedFrame {
                id: frame,
                presented: presentation.frame.clone(),
                frame: None,
                code,
                scope: FrameScope::Unavailable,
                registers: stack.registers(0),
                cfa: self.frame_cfa(root.reader(), &modules, code, &innermost.registers),
                activation: 0,
                below_stack_pointer: self.below_stack_pointer(inferior, root, innermost),
            });
        }

        let expanded =
            self.expand_backtrace(inferior, root, &stack, max_frames, &modules, presentation)?;

        self.resolved_in(inferior, root, &stack, &modules, &expanded, frame)
    }

    /// One logical frame of an expanded stack, and the state that
    /// evaluates its variables.
    pub(super) fn resolved_in(
        &self,
        inferior: &Inferior,
        root: &StackRoot,
        stack: &PhysicalStack,
        modules: &[UnwindModule<'_>],
        expanded: &Expanded,
        frame: StackFrameId,
    ) -> Result<ResolvedFrame> {
        let level = frame.get() as usize;
        let Some(selected) = expanded.trace.frames.get(level).cloned() else {
            return Err(Error::FrameNotFound {
                frame,
                frames: frame_level(expanded.trace.frames.len())?,
            });
        };

        let FrameOrigin { activation, source } = expanded.origins[level];
        let jump = match source {
            FrameSource::Activation | FrameSource::Described { .. } => None,
            FrameSource::TailCall(module, address) => Some((module, address)),
            FrameSource::Future(future) => {
                return Ok(self.suspended_frame(frame, selected, &expanded.futures[future]));
            }
            FrameSource::Interpreted(index) => {
                return Ok(super::interpreted::resolved(
                    frame,
                    selected,
                    activation,
                    &expanded.interpreted[index],
                ));
            }
        };

        let physical = &stack.frames[activation];
        let code = jump.or_else(|| {
            stack
                .lookup_address(activation)
                .and_then(|lookup| unwind_module_for(modules, lookup))
                .map(|(module, address)| (module.loaded.id, address))
        });
        let presented = match selected.kind {
            FrameKind::Inline => PresentedFrame::Inline(
                selected
                    .code_instance
                    .ok_or_else(|| Error::internal("inline frames name their code instance"))?,
            ),
            // A thread's stack has no suspended frames, and an interpreted
            // frame's state is no activation's.
            FrameKind::Physical
            | FrameKind::Signal
            | FrameKind::TailCall
            | FrameKind::Async { .. }
            | FrameKind::Awaited { .. }
            | FrameKind::Interpreted { .. } => PresentedFrame::Physical,
        };

        let scope = source_scope(
            &selected,
            source,
            physical.registers.get(crate::unwind::RBP),
        );

        // The jump discarded the frame's registers and its stack's place:
        // only entry values recover what was passed to it.
        if jump.is_some() {
            return Ok(ResolvedFrame {
                id: frame,
                presented,
                frame: Some(selected),
                code,
                scope,
                registers: FrameRegisters::Discarded,
                cfa: Err(VariableUnavailableReason::CallFrameUnavailable(
                    CallFrameUnavailableReason::TailCall,
                )
                .into()),
                activation,
                below_stack_pointer: None,
            });
        }

        Ok(ResolvedFrame {
            id: frame,
            presented,
            frame: Some(selected),
            code,
            scope,
            registers: stack.registers(activation),
            cfa: self.frame_cfa(root.reader(), modules, code, &physical.registers),
            activation,
            below_stack_pointer: self.below_stack_pointer(inferior, root, physical),
        })
    }

    /// The part of a frame's task stack below the frame's stack pointer,
    /// for a frame on its task's own stack whose stack pointer is known.
    /// Code a runtime runs on its own stacks may handle any task's memory.
    pub(super) fn below_stack_pointer(
        &self,
        inferior: &Inferior,
        root: &StackRoot,
        frame: &PhysicalFrame,
    ) -> Option<std::ops::Range<u64>> {
        if frame.segment != StackSegment::Task {
            return None;
        }

        let stack_pointer = frame.registers.get(RSP)?;
        let low = match (root.thread(), root.context) {
            (Some(pid), _) => self.task_stack(inferior, pid)?.low,
            (None, ExecutionContext::Task(task)) => {
                self.task_stack_bounds(inferior, task).ok()??.start
            }
            (None, ExecutionContext::Thread(_)) => return None,
        };

        Some(low..stack_pointer)
    }

    /// Computes the canonical frame address of the activation executing
    /// `code` with `registers`.
    pub(super) fn frame_cfa(
        &self,
        pid: Pid,
        modules: &[UnwindModule<'_>],
        code: Option<(ModuleId, ImageAddress)>,
        registers: &RegisterFile,
    ) -> std::result::Result<VirtualAddress, VariableRuntimeError> {
        let Some((module, address)) = code.and_then(|(id, address)| {
            modules
                .iter()
                .find(|module| module.loaded.id == id)
                .map(|module| (module, address))
        }) else {
            return Err(VariableRuntimeError::Unavailable(
                VariableUnavailableReason::CallFrameUnavailable(
                    CallFrameUnavailableReason::NoInstructionContext,
                ),
            ));
        };

        module
            .unwind
            .cfa(
                address,
                registers,
                &mut PtraceMemory {
                    ptrace: &self.ptrace,
                    pid,
                },
            )
            .map_err(|termination| variable_cfa_error(&termination))
    }

    /// Locates a frame other than the innermost one as the backtrace
    /// describes it.
    fn outer_frame_location(
        &self,
        inferior: &Inferior,
        root: &StackRoot,
        frame: StackFrameId,
    ) -> Result<ExecutionLocation> {
        let resolved = self.resolve_frame(inferior, root, frame)?;
        let selected = resolved.frame.ok_or(Error::AmbiguousInlineFrame)?;
        if matches!(selected.kind, FrameKind::Interpreted { .. }) {
            return Err(Error::FrameInterpreted);
        }

        // A suspended frame whose resume address is unknown has no code to
        // locate.
        let instruction = selected.instruction.ok_or(Error::FrameSuspended)?;
        let (module, address) = resolved.code.ok_or(Error::AddressOutsideModule)?;
        let module = self
            .modules
            .get(&module)
            .ok_or(Error::ModuleNotLoaded(module))?;
        let mut location = module.image.locate(address);
        location.function = selected.function;
        location.source = selected.source;

        // A caller is located just before its return address, but its
        // symbol offset describes the frame's own instruction.
        let lookup = module.loaded.virtual_address(address)?;
        if let Some(symbol) = &mut location.symbol {
            symbol.offset += instruction.get() - lookup.get();
        }

        Ok(ExecutionLocation {
            module: module.loaded.id,
            address: instruction,
            image: location,
        })
    }

    /// Whether a stopped thread is running the step `owner` names: the
    /// step's task, wherever its runtime runs it, or else its thread.
    pub(super) fn runs_step(&self, owner: StepOwner, pid: Pid) -> bool {
        let Some(task) = owner.task else {
            return owner.thread == pid;
        };

        self.inferior.as_ref().is_some_and(|inferior| {
            self.current_task(inferior, pid)
                .is_some_and(|current| current == Ok(Some(task)))
        })
    }

    /// How the stacks a stopped thread runs on are seen at this stop.
    pub(super) fn stack_view(&self, pid: Pid) -> StackView {
        self.inferior
            .as_ref()
            .and_then(|inferior| self.task_stack(inferior, pid))
            .map_or_else(
                || StackView::thread(pid),
                |stack| StackView::task(pid, stack),
            )
    }

    /// Where a stopped thread's stack pointer lies on its stacks.
    pub(super) fn stack_position(
        &self,
        pid: Pid,
        native: &libc::user_regs_struct,
    ) -> StackPosition {
        self.stack_view(pid).position(native.rsp)
    }

    /// Every loaded module's unwind context, beginning with the main image.
    pub(super) fn unwind_modules<'a>(&'a self, inferior: &Inferior) -> Vec<UnwindModule<'a>> {
        let main = UnwindModule {
            loaded: inferior.loaded_module,
            image: &self.main.image,
            unwind: self.main.unwind.as_ref(),
        };

        std::iter::once(main)
            .chain(
                self.modules
                    .values()
                    .filter(|module| module.loaded.id != inferior.loaded_module.id)
                    .map(|module| UnwindModule {
                        loaded: module.loaded,
                        image: &module.image,
                        unwind: module.unwind.as_ref(),
                    }),
            )
            .collect()
    }
}

impl<P: InspectionOps> Controller<P> {
    /// Selects a frame of a thread or task; each context keeps its own.
    pub(super) fn select_frame(
        &mut self,
        stop_id: StopId,
        context: ExecutionContext,
        frame: StackFrameId,
    ) -> Result<StackFrame> {
        let root = self.stack_root(stop_id, context)?;
        let inferior = self.stopped_root(stop_id, &root)?;
        let selected = self
            .resolve_frame(inferior, &root, frame)?
            .frame
            .ok_or(Error::AmbiguousInlineFrame)?;
        let inferior = self.inferior.as_mut().ok_or(Error::NotRunning)?;

        inferior
            .public_stop
            .as_mut()
            .ok_or(Error::NotStopped)?
            .selected_frames
            .insert(context, frame);
        self.bump_revision();
        Ok(selected)
    }

    /// Selects the thread or task that implicit inspection follows: a
    /// stopped thread, or a task on one, or a parked task.
    pub(super) fn select_context(
        &mut self,
        stop_id: StopId,
        context: ExecutionContext,
    ) -> Result<()> {
        let inferior = self.inferior.as_ref().ok_or(Error::NotRunning)?;
        validate_public_stop(inferior, Some(stop_id))?;
        let root = self.stack_root(stop_id, context)?;
        let thread = root.thread();
        let presentation = self.root_presentation(&root)?;
        let inferior = self.inferior.as_mut().ok_or(Error::NotRunning)?;
        let stop = inferior.public_stop.as_mut().ok_or(Error::NotStopped)?;
        if let (Some(pid), Some(presentation)) = (thread, presentation) {
            stop.presentations.insert(pid, presentation);
        }

        stop.selected = context;
        stop.selected_thread = thread;
        self.bump_revision();
        Ok(())
    }
}

fn variable_cfa_error(termination: &UnwindTermination) -> VariableRuntimeError {
    match termination {
        // Defective unwind metadata makes a value malformed; anything else
        // only leaves the call-frame address unavailable.
        UnwindTermination::CorruptUnwindInfo { .. }
        | UnwindTermination::InvalidCaller { .. }
        | UnwindTermination::CycleDetected => {
            VariableRuntimeError::Malformed(termination.to_string().into())
        }
        _ => VariableUnavailableReason::CallFrameUnavailable(
            CallFrameUnavailableReason::UnwindTerminated(termination.to_string().into()),
        )
        .into(),
    }
}

pub(super) fn frame_lookup_address(level: u32, context: &FrameContext) -> Option<VirtualAddress> {
    if level == 0 || context.signal_frame {
        Some(context.instruction)
    } else {
        context
            .instruction
            .get()
            .checked_sub(1)
            .map(VirtualAddress::new)
    }
}

pub(super) fn make_presentation(
    instruction: VirtualAddress,
    inline_chain: &[CodeInstanceId],
    visible: usize,
) -> Result<FramePresentation> {
    debug_assert!(
        visible <= inline_chain.len(),
        "presenting {visible} inline frames of a chain of {}",
        inline_chain.len()
    );
    let hidden = inline_chain
        .len()
        .checked_sub(visible)
        .ok_or(Error::LocationUnavailable)?;
    let hidden_inline_frames = u32::try_from(hidden).map_err(|_| Error::LocationUnavailable)?;
    let frame = visible
        .checked_sub(1)
        .map_or(PresentedFrame::Physical, |index| {
            PresentedFrame::Inline(inline_chain[index])
        });

    Ok(FramePresentation {
        instruction,
        frame,
        hidden_inline_frames,
    })
}

pub(super) fn default_inline_visible_count(
    module_image: &ModuleImage,
    inline_chain: &[CodeInstanceId],
    image_address: ImageAddress,
    reveal_new_inline: bool,
) -> usize {
    inline_chain
        .iter()
        .position(|instance| {
            module_image
                .code_instance(*instance)
                .is_some_and(|instance| instance.ranges().any(|range| range.start == image_address))
        })
        .map_or(inline_chain.len(), |index| {
            // `position` is a zero-based frame index; presentation uses a
            // count. Source `step` reveals the newly entered frame, while
            // `next`, `finish`, and instruction stops remain in its parent.
            // A loop body is its enclosing function's own code, so any stop
            // where one begins shows it.
            inline_chain[index..]
                .iter()
                .position(|instance| is_loop_body(module_image, *instance))
                .map_or_else(
                    || index + usize::from(reveal_new_inline),
                    |body| index + body + 1,
                )
        })
}

pub(super) fn presentation_visible_count(
    location: &ImageLocation,
    presentation: &FramePresentation,
) -> Result<usize> {
    if matches!(presentation.frame, PresentedFrame::Ambiguous(_)) {
        return Err(Error::AmbiguousInlineFrame);
    }

    let InlineFrameLookup::Unique(chain) = &location.inline_frames else {
        return match presentation.frame {
            PresentedFrame::Physical => Ok(0),
            PresentedFrame::Inline(_) | PresentedFrame::Ambiguous(_) => {
                Err(Error::LocationUnavailable)
            }
        };
    };

    let visible = match presentation.frame {
        PresentedFrame::Physical => 0,
        PresentedFrame::Inline(selected) => chain
            .instances
            .iter()
            .position(|instance| *instance == selected)
            .map(|index| index + 1)
            .ok_or(Error::LocationUnavailable)?,
        PresentedFrame::Ambiguous(_) => return Err(Error::AmbiguousInlineFrame),
    };

    let hidden =
        u32::try_from(chain.instances.len() - visible).map_err(|_| Error::LocationUnavailable)?;
    if hidden != presentation.hidden_inline_frames {
        return Err(Error::LocationUnavailable);
    }

    Ok(visible)
}

pub(super) fn apply_presentation(
    module_image: &ModuleImage,
    location: &mut ImageLocation,
    presentation: &FramePresentation,
) -> Result<()> {
    let visible = presentation_visible_count(location, presentation)?;
    let InlineFrameLookup::Unique(chain) = &location.inline_frames else {
        return Ok(());
    };

    let selected_instance = visible
        .checked_sub(1)
        .and_then(|index| chain.instances.get(index).copied())
        .or(location.physical_instance);

    location.function = selected_instance
        .and_then(|instance| module_image.code_instance(instance))
        .and_then(|instance| module_image.function(instance.function()))
        .map(crate::Function::info);
    location.source = visible_source(module_image, location, &chain.instances, visible);

    Ok(())
}

pub(super) fn selected_code_instance(
    location: &ImageLocation,
    presentation: &FramePresentation,
) -> Result<Option<CodeInstanceId>> {
    presentation_visible_count(location, presentation)?;

    Ok(match presentation.frame {
        PresentedFrame::Physical => location.physical_instance,
        PresentedFrame::Inline(instance) => Some(instance),
        PresentedFrame::Ambiguous(_) => return Err(Error::AmbiguousInlineFrame),
    })
}

pub(super) fn source_for_code_instance(
    module_image: &ModuleImage,
    location: &ImageLocation,
    selected: CodeInstanceId,
) -> Option<SourceLocation> {
    let InlineFrameLookup::Unique(chain) = &location.inline_frames else {
        return (location.physical_instance == Some(selected))
            .then(|| location.source.clone())
            .flatten();
    };

    let visible = if location.physical_instance == Some(selected) {
        0
    } else {
        chain
            .instances
            .iter()
            .position(|instance| *instance == selected)?
            + 1
    };

    visible_source(module_image, location, &chain.instances, visible)
}

/// The source line a frame shows when `visible` of `chain`'s inline frames
/// are: the call site of the outermost hidden one, or the location's own
/// line when none is hidden.
fn visible_source(
    module_image: &ModuleImage,
    location: &ImageLocation,
    chain: &[CodeInstanceId],
    visible: usize,
) -> Option<SourceLocation> {
    chain.get(visible).map_or_else(
        || location.source.clone(),
        |hidden| module_image.code_instance(*hidden).and_then(call_site),
    )
}

/// Where an inline instance was called from.
fn call_site(instance: crate::CodeInstance<'_>) -> Option<SourceLocation> {
    match instance.kind() {
        CodeInstanceKind::Inline { call_site } => call_site,
        CodeInstanceKind::OutOfLine => None,
    }
}

pub(super) fn code_instance_is_active(location: &ImageLocation, selected: CodeInstanceId) -> bool {
    if location.physical_instance == Some(selected) {
        return true;
    }

    match &location.inline_frames {
        InlineFrameLookup::None => false,
        InlineFrameLookup::Unique(chain) => chain.instances.contains(&selected),
        InlineFrameLookup::Ambiguous(chains) => chains
            .iter()
            .any(|chain| chain.instances.contains(&selected)),
    }
}

pub(super) fn source_step_destination(
    module_image: &ModuleImage,
    location: &ImageLocation,
    kind: StepKind,
) -> bool {
    location.source.is_some()
        && (kind == StepKind::Out
            || module_image
                .line_entry_containing(location.address)
                .is_some_and(|entry| entry.statement))
}

pub(super) fn source_line_changed(
    start: Option<&SourceLocation>,
    current: Option<&SourceLocation>,
) -> bool {
    current.is_some_and(|current| {
        start.is_none_or(|start| start.file != current.file || start.line != current.line)
    })
}

/// Where a logical frame of a backtrace comes from.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(super) struct FrameOrigin {
    /// The physical activation whose state the frame has, or, for a frame
    /// whose function left by a tail call, the one whose state replaced it;
    /// for a frame a runtime's frame drives, the one that drives it.
    pub(super) activation: usize,

    pub(super) source: FrameSource,
}

impl FrameOrigin {
    const fn of(activation: usize) -> Self {
        Self {
            activation,
            source: FrameSource::Activation,
        }
    }
}

/// What a logical frame's state is read from.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(super) enum FrameSource {
    /// Its activation: the activation's own function, or one inlined
    /// into it.
    Activation,

    /// A function that left by a tail call, by its module and an address
    /// within its jump.
    TailCall(ModuleId, ImageAddress),

    /// A future its activation drives, in [`Expanded::futures`].
    Future(usize),

    /// An interpreted frame its activation runs, or that its thread runs
    /// where its place is unknown, in [`Expanded::interpreted`].
    Interpreted(usize),

    /// A function `runtime` recorded for the activation, as V8 does: the
    /// activation's own, or one its code inlined; and whether the
    /// interpreter's registers in its frame are live.
    Described {
        runtime: crate::RuntimeId,
        inlined: bool,
        live_registers: bool,
    },
}

/// A stack's logical frames, and where each comes from.
pub(super) struct Expanded {
    pub(super) trace: Backtrace,
    pub(super) origins: Vec<FrameOrigin>,

    /// The futures whose frames the trace shows, where frames drive them.
    pub(super) futures: Vec<futures::AsyncFrame>,

    /// The interpreted frames the trace shows.
    pub(super) interpreted: Vec<super::interpreted::Placed>,

    /// The declared types of the futures frames drive that cannot be read.
    pub(super) lost: Vec<crate::TypeReference>,

    /// The await chains a runtime's frames resumed, each by the frame of
    /// the trace that resumed it, innermost first, which splicing shows
    /// before it.
    pub(super) awaiting: Vec<(usize, crate::RuntimeId, crate::runtime_model::Awaiting)>,
}

impl Expanded {
    /// Panics unless each frame is at its level, has an origin among the
    /// stack's `activations` and the futures, and every note of a future
    /// not followed names a frame of the trace.
    pub(super) fn check_invariants(&self, activations: usize) {
        let frames = &self.trace.frames;
        debug_assert_eq!(
            self.origins.len(),
            frames.len(),
            "every frame has one origin"
        );
        for (level, (frame, origin)) in frames.iter().zip(&self.origins).enumerate() {
            debug_assert!(
                usize::try_from(frame.level).is_ok_and(|claimed| claimed == level),
                "the frame at {level} claims level {}",
                frame.level
            );
            debug_assert_eq!(
                frame.id,
                StackFrameId::new(frame.level),
                "the frame at {level} is not identified by its level"
            );
            debug_assert!(
                origin.activation < activations,
                "the frame at {level} comes from activation {} of {activations}",
                origin.activation
            );
            debug_assert!(
                match origin.source {
                    FrameSource::Future(future) => future < self.futures.len(),
                    FrameSource::Interpreted(frame) => frame < self.interpreted.len(),
                    FrameSource::Activation
                    | FrameSource::TailCall(..)
                    | FrameSource::Described { .. } => true,
                },
                "the frame at {level} is {:?} of {} futures and {} interpreted frames",
                origin.source,
                self.futures.len(),
                self.interpreted.len()
            );
        }

        for note in self.trace.unfollowed.iter() {
            debug_assert!(
                usize::try_from(note.driver.get()).is_ok_and(|driver| driver < frames.len()),
                "a future not followed names frame {} of {}",
                note.driver.get(),
                frames.len()
            );
        }
    }
}

/// A stack's logical frames: each activation's inline frames, innermost
/// first, then the activation itself, then the functions that left by the
/// tail calls `tail_jumps` finds between it and its caller, the last first.
fn expand_inline_backtrace(
    stack: &PhysicalStack,
    subject: ExecutionContext,
    modules: &[UnwindModule<'_>],
    presentation: Option<&FramePresentation>,
    tail_jumps: &mut dyn FnMut(usize, (ModuleId, ImageAddress)) -> Vec<TailJump>,
) -> Result<Expanded> {
    let mut frames = Vec::new();
    let mut origins = Vec::new();

    for (activation, physical) in stack.frames.iter().enumerate() {
        let first = frames.len();
        let code = expand_activation(stack, activation, modules, presentation, &mut frames)?;
        origins.resize(frames.len(), FrameOrigin::of(activation));

        // A signal interrupted its frame rather than calling it.
        if let Some((module, address)) = code
            && !physical.context.signal_frame
        {
            for jump in tail_jumps(activation, (module.loaded.id, address)) {
                let Ok(instruction) = module.loaded.virtual_address(jump.instruction) else {
                    break;
                };

                let lookup = module.loaded.virtual_address(jump.lookup)?;

                push_code_frames(
                    &mut frames,
                    FrameKind::TailCall,
                    module,
                    jump.lookup,
                    instruction,
                    lookup,
                    None,
                )?;
                origins.resize(
                    frames.len(),
                    FrameOrigin {
                        activation,
                        source: FrameSource::TailCall(module.loaded.id, jump.lookup),
                    },
                );
            }
        }

        for frame in &mut frames[first..] {
            frame.segment = physical.segment;
        }
    }

    let expanded = Expanded {
        trace: Backtrace {
            context: subject,
            frames: frames.into(),
            termination: stack.termination.clone(),
            unfollowed: Arc::from([]),
        },
        origins,
        futures: Vec::new(),
        interpreted: Vec::new(),
        lost: Vec::new(),
        awaiting: Vec::new(),
    };

    if cfg!(debug_assertions) {
        expanded.check_invariants(stack.frames.len());
    }

    Ok(expanded)
}

/// One activation's logical frames: its inline frames, innermost first,
/// then the activation itself. Returns the module describing its code, and
/// the code's address in that image.
fn expand_activation<'a>(
    stack: &PhysicalStack,
    activation: usize,
    modules: &[UnwindModule<'a>],
    presentation: Option<&FramePresentation>,
    frames: &mut Vec<StackFrame>,
) -> Result<Option<(UnwindModule<'a>, ImageAddress)>> {
    let context = &stack.frames[activation].context;
    let kind = if context.signal_frame {
        FrameKind::Signal
    } else {
        FrameKind::Physical
    };

    let lookup = stack.lookup_address(activation);
    let located = lookup.and_then(|address| unwind_module_for(modules, address));
    let (Some(lookup), Some((frame_module, image_address))) = (lookup, located) else {
        let level = frame_level(frames.len())?;
        frames.push(StackFrame::new(level, kind, None, context.instruction));
        return Ok(None);
    };

    // The stop presentation describes the main image only; innermost
    // frames in other modules show their complete inline chain.
    let presentation =
        presentation.filter(|_| activation == 0 && frame_module.loaded.id == modules[0].loaded.id);
    push_code_frames(
        frames,
        kind,
        frame_module,
        image_address,
        context.instruction,
        lookup,
        presentation,
    )?;

    Ok(Some((frame_module, image_address)))
}

/// The level of the next frame pushed onto a stack of `len` frames.
fn frame_level(len: usize) -> Result<u32> {
    u32::try_from(len).map_err(|_| Error::internal("frame count fits in u32"))
}

/// The logical frames of code at `image_address` in `frame_module`, the
/// frame's instruction or the byte before its return address: the inline
/// frames `presentation` shows, or else all of them, innermost first, then
/// a frame of `kind` for the function.
fn push_code_frames(
    frames: &mut Vec<StackFrame>,
    kind: FrameKind,
    frame_module: UnwindModule<'_>,
    image_address: ImageAddress,
    instruction: VirtualAddress,
    lookup: VirtualAddress,
    presentation: Option<&FramePresentation>,
) -> Result<()> {
    let module_image = frame_module.image;
    let location = module_image.locate(image_address);
    let module = Some(frame_module.loaded.id);
    let physical_source = if let InlineFrameLookup::Unique(chain) = &location.inline_frames {
        let visible = match presentation {
            Some(presentation) => presentation_visible_count(&location, presentation)?,
            None => chain.instances.len(),
        };

        let mut source = visible_source(module_image, &location, &chain.instances, visible);

        for &instance_id in chain.instances[..visible].iter().rev() {
            let instance = module_image
                .code_instance(instance_id)
                .ok_or_else(|| Error::internal("inline chain references a known instance"))?;
            let function = module_image
                .function(instance.function())
                .map(crate::Function::info);
            let level = frame_level(frames.len())?;
            let role = function
                .as_ref()
                .map_or(CodeRole::Ordinary, |function| function.role);

            frames.push(StackFrame::from_parts(
                level,
                FrameKind::Inline,
                module,
                Some(instruction),
                FrameMetadata {
                    code_instance: Some(instance.id()),
                    function,
                    source,
                    symbol: None,
                    role,
                },
            ));
            source = call_site(instance);
        }

        source
    } else {
        location.source.clone()
    };

    let physical_instance = location
        .physical_instance
        .and_then(|instance| module_image.code_instance(instance));
    let function = physical_instance
        .and_then(|instance| module_image.function(instance.function()))
        .map(crate::Function::info);
    let level = frame_level(frames.len())?;

    frames.push(StackFrame::from_parts(
        level,
        kind,
        module,
        Some(instruction),
        FrameMetadata {
            code_instance: physical_instance.map(crate::image::functions::CodeInstance::id),
            function,
            source: physical_source,
            // A caller is looked up just before its return address, but
            // its offset describes the frame's own instruction.
            symbol: location.symbol.map(|mut symbol| {
                symbol.offset += instruction.get() - lookup.get();
                symbol
            }),
            role: module_image.code_role(image_address),
        },
    ));
    Ok(())
}

/// One physical activation and the registers it held: the thread's own
/// for the innermost activation, otherwise those the unwinder reconstructed.
#[derive(Clone)]
pub(super) struct PhysicalFrame {
    pub(super) context: FrameContext,
    pub(super) registers: RegisterFile,

    /// Whose stack the activation is on.
    pub(super) segment: StackSegment,
}

/// A stack's physical activations, innermost first.
#[derive(Clone)]
pub(super) struct PhysicalStack {
    /// The live registers of a thread's stack; `None` for registers a task
    /// saved, which hold only some of them.
    pub(super) native: Option<libc::user_regs_struct>,

    /// Whether the innermost activation's instruction is a return address,
    /// as a parked task's is.
    pub(super) after_call: bool,
    pub(super) frames: Vec<PhysicalFrame>,
    pub(super) termination: UnwindTermination,
}

impl PhysicalStack {
    /// The address whose code an activation is executing: its instruction,
    /// or the byte before it when that is a return address.
    pub(super) fn lookup_address(&self, activation: usize) -> Option<VirtualAddress> {
        let caller = activation != 0 || self.after_call;
        frame_lookup_address(u32::from(caller), &self.frames[activation].context)
    }

    /// The registers an activation's values are read from.
    pub(super) fn registers(&self, activation: usize) -> FrameRegisters {
        match self.native {
            Some(native) if activation == 0 => FrameRegisters::Thread(native),
            _ => FrameRegisters::Caller(self.frames[activation].registers.clone()),
        }
    }
}

/// Where a stack's frames begin, and the context a request named it by.
#[derive(Debug, Clone)]
pub(super) struct StackRoot {
    pub(super) context: ExecutionContext,
    pub(super) origin: RootOrigin,
}

/// Where a stack's innermost frame comes from.
#[derive(Debug, Clone)]
pub(super) enum RootOrigin {
    /// A stopped thread's live registers.
    Thread(Pid),

    /// The registers a parked task saved. Any other register is unknown.
    Saved {
        registers: RegisterFile,

        /// Whether the saved instruction is a return address.
        after_call: bool,

        /// A stopped thread of the process, through which its memory is
        /// read.
        reader: Pid,
    },

    /// A suspended task's future, of type `ty`, which the code of `module`
    /// describes. Its frames are the chain of awaits the future holds.
    Suspended {
        future: VirtualAddress,
        ty: TypeReference,
        module: LoadedModule,
        reader: Pid,
    },

    /// A suspended task whose frames the interpreter of the runtime
    /// `runtime` keeps, innermost first, and why they end before the
    /// task's outermost, if they do.
    Interpreted {
        runtime: crate::RuntimeId,
        frames: Arc<[InterpretedFrame]>,
        end: Option<Arc<str>>,
        reader: Pid,
    },
}

impl StackRoot {
    /// A stopped thread's own stack.
    pub(super) fn of_thread(pid: Pid) -> Self {
        Self {
            context: ExecutionContext::Thread(debug_thread_id(pid)),
            origin: RootOrigin::Thread(pid),
        }
    }

    /// The stopped thread whose memory and state the stack is read through.
    pub(super) const fn reader(&self) -> Pid {
        match self.origin {
            RootOrigin::Thread(pid)
            | RootOrigin::Saved { reader: pid, .. }
            | RootOrigin::Suspended { reader: pid, .. }
            | RootOrigin::Interpreted { reader: pid, .. } => pid,
        }
    }

    /// The thread whose live registers begin the stack, if one does.
    pub(super) const fn thread(&self) -> Option<Pid> {
        match self.origin {
            RootOrigin::Thread(pid) => Some(pid),
            RootOrigin::Saved { .. }
            | RootOrigin::Suspended { .. }
            | RootOrigin::Interpreted { .. } => None,
        }
    }
}

/// The registers a logical frame's values are read from.
pub(super) enum FrameRegisters {
    /// The thread's own registers, which every logical frame of the
    /// innermost activation shares.
    Thread(libc::user_regs_struct),

    /// The registers the unwinder reconstructed for a caller's activation.
    /// Registers a callee could overwrite without saving are absent.
    Caller(RegisterFile),

    /// None: the frame's function left by a tail call, which discarded
    /// them.
    Discarded,
}

/// The source scope whose variables a frame shows.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(super) enum FrameScope {
    /// No single function scope applies: the code has no debug information,
    /// or an innermost frame has no single compatible inline chain.
    Unavailable,

    /// The physical function's own scope.
    Function,

    /// One inline instance's scope.
    Inline(CodeInstanceId),

    /// A suspended async function's, whose future of type `ty` at
    /// `object` is in the state numbered `state`.
    Suspended {
        object: VirtualAddress,
        ty: TypeReference,
        state: u64,
    },

    /// An interpreted frame's, which `runtime`'s interpreter keeps at
    /// `frame`.
    Interpreted {
        runtime: crate::RuntimeId,
        frame: InterpretedFrameRef,
    },
}

/// The source scope of `frame`, which comes from `source`, and whose
/// frame pointer is `fp`. Only code a function describes has one; a
/// described activation's runtime reads it from the frame at rbp, and an
/// inlined one has none.
fn source_scope(frame: &StackFrame, source: FrameSource, fp: Option<u64>) -> FrameScope {
    match (frame.kind, frame.code_instance, source) {
        (
            _,
            _,
            FrameSource::Described {
                runtime,
                inlined: false,
                live_registers,
            },
        ) => fp.map_or(FrameScope::Unavailable, |fp| FrameScope::Interpreted {
            runtime,
            frame: InterpretedFrameRef {
                address: VirtualAddress::new(fp),
                live_registers,
            },
        }),
        (_, _, FrameSource::Described { inlined: true, .. }) | (_, None, _) => {
            FrameScope::Unavailable
        }
        (FrameKind::Inline, Some(instance), _) => FrameScope::Inline(instance),
        (
            FrameKind::Physical
            | FrameKind::Signal
            | FrameKind::TailCall
            | FrameKind::Async { .. }
            | FrameKind::Awaited { .. }
            | FrameKind::Interpreted { .. },
            Some(_),
            _,
        ) => FrameScope::Function,
    }
}

/// One logical frame of a stopped thread and the state that evaluates its
/// variables.
pub(super) struct ResolvedFrame {
    pub(super) id: StackFrameId,

    /// The logical frame whose source scope the frame's variables follow.
    pub(super) presented: PresentedFrame,

    /// The frame as the backtrace describes it; absent for an innermost
    /// frame whose inline presentation is ambiguous.
    pub(super) frame: Option<StackFrame>,

    /// The module describing the frame's code, and the frame's address in
    /// its image: the instruction itself for the innermost activation and
    /// signal frames, otherwise the byte before the return address.
    pub(super) code: Option<(ModuleId, ImageAddress)>,
    pub(super) scope: FrameScope,
    pub(super) registers: FrameRegisters,
    pub(super) cfa: std::result::Result<VirtualAddress, VariableRuntimeError>,

    /// The index of the physical activation containing the frame.
    pub(super) activation: usize,

    /// For a frame on its task's own stack, the part of that stack below
    /// the frame's stack pointer: only the frame's callees use it, so a
    /// pointer of the frame's to it is stale.
    pub(super) below_stack_pointer: Option<std::ops::Range<u64>>,
}

/// Describes a process address by the module whose image covers it.
pub(super) fn describe_address(
    modules: &[UnwindModule<'_>],
    address: VirtualAddress,
) -> AddressDescription {
    AddressDescription {
        address,
        module: unwind_module_for(modules, address).map(|(module, image_address)| ModuleAddress {
            module: module.loaded.id,
            path: module.image.path_arc(),
            image: module.image.describe(image_address),
        }),
    }
}

/// Unwinds by the roles code plays: outermost code ends a stack, and the
/// runtime that switched stacks says where a stack switch goes. Everything
/// else is unwound by its call-frame information.
pub(super) struct RoleCallerProvider<'a, 'c> {
    pub(super) dwarf: DwarfCallerProvider<'a>,

    /// The stacks a runtime runs the thread on, and whose each is.
    pub(super) stacks: Vec<(std::ops::Range<u64>, StackSegment)>,

    /// Whose stack a frame on none of them is on.
    pub(super) other: StackSegment,
    pub(super) cross: &'c mut CrossStacks<'c>,

    /// Whose stack the current frame is on, when its callee's is: a frame
    /// is on its callee's stack unless the callee switched stacks or a
    /// signal interrupted it. A runtime knows its stacks' bounds only
    /// roughly where the system gave them, as the top of a thread's.
    pub(super) carried: Option<StackSegment>,

    /// Whether a frame on the current stack gave the thread to a task, so
    /// that the task that switched to this stack may have left it.
    pub(super) dispatched: bool,
}

impl RoleCallerProvider<'_, '_> {
    /// Whose stack the current frame is on: its callee's, or else where
    /// its stack pointer points.
    pub(super) fn segment(&self) -> StackSegment {
        self.carried.unwrap_or_else(|| self.bounded_segment())
    }

    /// Whose stack holds the current frame's stack pointer.
    fn bounded_segment(&self) -> StackSegment {
        self.dwarf
            .registers
            .get(RSP)
            .and_then(|pointer| {
                self.stacks
                    .iter()
                    .find_map(|(stack, segment)| stack.contains(&pointer).then_some(*segment))
            })
            .unwrap_or(self.other)
    }

    /// The role of the code at an exact address.
    fn role_at(&self, address: VirtualAddress) -> Option<CodeRole> {
        unwind_module_for(&self.dwarf.modules, address)
            .map(|(module, address)| module.image.code_role(address))
    }

    /// The frame interrupted by a signal, from the registers the kernel
    /// saved in the signal frame above a handler that returned to a signal
    /// trampoline: the trampoline's stack holds the `ucontext`.
    fn interrupted(&mut self) -> CallerResult {
        let Some(context) = self.dwarf.registers.get(RSP) else {
            return CallerResult::Finished(UnwindTermination::RegisterUnavailable {
                register: "rsp".into(),
            });
        };

        let mut registers = RegisterFile::new([]);
        for (slot, register) in SIGCONTEXT_REGISTERS.iter().enumerate() {
            let address = context + UCONTEXT_MCONTEXT + 8 * slot as u64;
            let Some(value) = self.dwarf.memory.read_u64(VirtualAddress::new(address)) else {
                return CallerResult::Finished(UnwindTermination::MemoryReadFailed {
                    address: VirtualAddress::new(address),
                });
            };

            registers.set(*register, value);
        }

        let instruction = registers.get(RETURN_ADDRESS).unwrap_or(0);
        self.dwarf.first = false;
        self.dwarf.registers = registers;
        self.carried = None;
        self.dispatched = false;

        CallerResult::Caller(FrameContext {
            instruction: VirtualAddress::new(instruction),
            cfa: Some(VirtualAddress::new(context)),
            signal_frame: true,
        })
    }
}

pub(super) use crate::unwind::{SIGCONTEXT_REGISTERS, UCONTEXT_MCONTEXT};

/// Asks the runtime whose module holds a frame's code where the frame,
/// with these registers, goes on past the stack switch it makes; `None`
/// when no runtime model reads the module's runtime.
pub(super) type CrossStacks<'c> = dyn FnMut(ModuleId, &RegisterFile, bool) -> Option<std::result::Result<Crossing, Arc<str>>>
    + 'c;

impl CallerProvider for RoleCallerProvider<'_, '_> {
    fn caller(&mut self, current: &FrameContext) -> CallerResult {
        self.carried = Some(self.segment());

        // A handler returns to its signal trampoline's first instruction,
        // so the trampoline is named by its own address, not the one
        // before it, as glibc's alone allows with a byte to spare.
        match self.caller_by_role(current) {
            CallerResult::Caller(caller)
                if self.role_at(caller.instruction) == Some(CodeRole::SignalTrampoline) =>
            {
                CallerResult::Caller(FrameContext {
                    signal_frame: true,
                    ..caller
                })
            }
            result => result,
        }
    }
}

impl RoleCallerProvider<'_, '_> {
    fn caller_by_role(&mut self, current: &FrameContext) -> CallerResult {
        if self.role_at(current.instruction) == Some(CodeRole::SignalTrampoline) {
            return self.interrupted();
        }

        let role = self.dwarf.lookup_address(current).and_then(|lookup| {
            unwind_module_for(&self.dwarf.modules, lookup)
                .map(|(module, address)| (module.loaded.id, module.image.code_role(address)))
        });
        let unresolved = |reason: Arc<str>| {
            CallerResult::Finished(UnwindTermination::UnresolvedStackSwitch { reason })
        };

        match role {
            Some((_, CodeRole::Outermost)) => CallerResult::Finished(UnwindTermination::Complete),
            Some((module, CodeRole::StackSwitch)) => {
                let after_call = !self.dwarf.first && !current.signal_frame;

                match (self.cross)(module, &self.dwarf.registers, after_call) {
                    None => unresolved("no runtime model reads the module's runtime".into()),
                    Some(Err(reason)) => unresolved(reason),
                    Some(Ok(Crossing::Stay)) => self.dwarf.caller(current),
                    Some(Ok(Crossing::Outermost)) => {
                        CallerResult::Finished(UnwindTermination::Complete)
                    }

                    // The task the runtime names now may not be the one
                    // that switched here, whose frames lie beyond.
                    Some(Ok(Crossing::Resume(_) | Crossing::Continue(_))) if self.dispatched => {
                        CallerResult::Finished(UnwindTermination::Complete)
                    }
                    Some(Ok(Crossing::Resume(registers))) => {
                        self.dwarf.registers = registers;
                        self.carried = None;
                        self.dispatched = false;
                        self.dwarf.caller(current)
                    }
                    Some(Ok(Crossing::Continue(registers))) => {
                        let Some(instruction) = registers.get(RETURN_ADDRESS) else {
                            return unresolved("the task saved no instruction".into());
                        };

                        self.dwarf.first = false;
                        self.dwarf.registers = registers;
                        self.carried = None;
                        self.dispatched = false;

                        CallerResult::Caller(FrameContext {
                            instruction: VirtualAddress::new(instruction),
                            cfa: None,
                            signal_frame: false,
                        })
                    }
                }
            }
            Some((_, CodeRole::Dispatch)) => {
                self.dispatched = true;
                self.dwarf.caller(current)
            }

            // The runtime entered the frame by a trap, faking a call from
            // the instruction that trapped, which its caller's pc names.
            Some((_, CodeRole::TrapEntry)) => match self.dwarf.caller(current) {
                CallerResult::Caller(caller) => CallerResult::Caller(FrameContext {
                    signal_frame: true,
                    ..caller
                }),
                finished @ CallerResult::Finished(_) => finished,
            },
            _ => self.dwarf.caller(current),
        }
    }
}

pub(super) struct DwarfCallerProvider<'a> {
    pub(super) modules: Vec<UnwindModule<'a>>,
    pub(super) registers: RegisterFile,
    pub(super) memory: PtraceMemory<'a>,
    pub(super) first: bool,

    /// The code runtimes own as the process says, which keeps the
    /// runtime's frame-pointer convention: a frame there that no
    /// call-frame information describes is unwound by it.
    pub(super) runtime_code: Vec<super::runtimes::ProcessCode>,
}

impl<P: InspectionOps> Controller<P> {
    /// Unwinds `pid`'s stack from `registers` through every loaded
    /// module's call-frame information, and by the convention of
    /// `runtimes` through the code they own where none describes it.
    /// `first` says the registers are a stopped thread's own, whose
    /// instruction is not a return address.
    pub(super) fn caller_provider<'a>(
        &'a self,
        inferior: &Inferior,
        runtimes: &[super::runtimes::BoundRuntime],
        pid: Pid,
        registers: RegisterFile,
        first: bool,
    ) -> DwarfCallerProvider<'a> {
        DwarfCallerProvider {
            modules: self.unwind_modules(inferior),
            registers,
            memory: PtraceMemory {
                ptrace: &self.ptrace,
                pid,
            },
            first,
            runtime_code: self
                .runtime_process_code(inferior, runtimes, pid)
                .into_iter()
                .flatten()
                .collect(),
        }
    }
}

impl DwarfCallerProvider<'_> {
    /// Whether `address`, which `module` holds at its image's
    /// `image_address`, is in code a runtime owns as the process says.
    fn in_runtime_code(
        &self,
        address: VirtualAddress,
        module: &UnwindModule<'_>,
        image_address: ImageAddress,
    ) -> bool {
        let code = Some((module.loaded.image, image_address));
        self.runtime_code
            .iter()
            .any(|range| range.holds(address, code))
    }

    /// The address whose unwind rules describe `current`: its instruction in
    /// the innermost or a signal frame, and otherwise the byte before its
    /// return address, which still belongs to the call. `None` means a
    /// return address of zero, which ends the stack.
    fn lookup_address(&self, current: &FrameContext) -> Option<VirtualAddress> {
        frame_lookup_address(u32::from(!self.first), current)
    }

    /// The canonical frame address of `current`, from its own unwind rules.
    ///
    /// Identifying an activation needs nothing of its caller, so this works
    /// for a frame whose return address is unreadable or corrupt, such as
    /// a coroutine's first frame on a fresh stack.
    pub(super) fn frame_cfa(
        &mut self,
        current: &FrameContext,
    ) -> std::result::Result<VirtualAddress, UnwindTermination> {
        let lookup = self
            .lookup_address(current)
            .ok_or(UnwindTermination::Complete)?;
        let (module, image_address) = unwind_module_for(&self.modules, lookup)
            .ok_or(UnwindTermination::ModuleNotFound { address: lookup })?;

        let cfa = module
            .unwind
            .cfa(image_address, &self.registers, &mut self.memory);
        match cfa {
            Err(UnwindTermination::NoUnwindInfo { .. })
                if self.in_runtime_code(lookup, &module, image_address) =>
            {
                crate::debug_info::runtime_code::cfa_at(lookup, &self.registers, &mut self.memory)
            }
            cfa => cfa,
        }
        .map_err(|mut termination| {
            if let UnwindTermination::NoUnwindInfo { address } = &mut termination {
                *address = lookup;
            }

            termination
        })
    }
}

impl CallerProvider for DwarfCallerProvider<'_> {
    fn caller(&mut self, current: &FrameContext) -> CallerResult {
        // A stopped thread's stack pointer is on a page it has used, which
        // a core's producer writes unless it failed to read it.
        if self.first
            && let Some(stack) = self.registers.get(crate::unwind::RSP)
            && !self.memory.ptrace.memory_written(stack)
        {
            return CallerResult::Finished(UnwindTermination::StackNotSaved {
                address: VirtualAddress::new(stack),
            });
        }

        let Some(lookup) = self.lookup_address(current) else {
            return CallerResult::Finished(UnwindTermination::Complete);
        };

        self.first = false;
        let Some((module, image_address)) = unwind_module_for(&self.modules, lookup) else {
            return CallerResult::Finished(UnwindTermination::ModuleNotFound { address: lookup });
        };

        let step = match module
            .unwind
            .unwind(image_address, &self.registers, &mut self.memory)
        {
            Err(UnwindTermination::NoUnwindInfo { .. })
                if self.in_runtime_code(lookup, &module, image_address) =>
            {
                crate::debug_info::runtime_code::caller_at(
                    lookup,
                    &self.registers,
                    &mut self.memory,
                )
            }
            step => step,
        };
        let step = match step {
            Ok(step) => step,
            Err(mut termination) => {
                if let UnwindTermination::NoUnwindInfo { address } = &mut termination {
                    *address = lookup;
                }

                return CallerResult::Finished(termination);
            }
        };

        let Some(instruction) = step.registers.get(RETURN_ADDRESS) else {
            return CallerResult::Finished(UnwindTermination::Complete);
        };

        if instruction == 0 {
            return CallerResult::Finished(UnwindTermination::Complete);
        }

        if step.cfa.get() == current.cfa.map_or(0, VirtualAddress::get)
            && instruction == current.instruction.get()
        {
            return CallerResult::Finished(UnwindTermination::InvalidCaller {
                description: "caller did not make progress".into(),
            });
        }

        self.registers = step.registers;
        CallerResult::Caller(FrameContext {
            instruction: VirtualAddress::new(instruction),
            cfa: Some(step.cfa),
            signal_frame: step.signal_frame,
        })
    }
}
