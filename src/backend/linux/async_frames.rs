//! A suspended future's frames: the chain of awaits it holds, from the
//! future it waits on out to the async function it began in, for a
//! suspended task's whole stack, or spliced into a thread's stack before
//! the frame that drives it. No thread runs them, so they have no
//! registers; each async function's variables are the members its state
//! keeps.

use std::cell::Cell;
use std::collections::BTreeSet;
use std::sync::Arc;

use crate::debug_info::Located;
use crate::eval::target::Machine;
use crate::inspection::InspectionBudget;
use crate::model::{FrameMetadata, ValueStorage};
use crate::protocol::{PresentedFrame, StopId};
use crate::runtime_model::Driven;
use crate::runtime_model::futures::{self, AsyncFrameKind, ChainEnd};

use crate::{
    Backtrace, CallFrameUnavailableReason, CodeInstanceKind, CodeRole, CoroutineInfo, DrivenFrames,
    Error, FrameKind, ImageAddress, InspectionLimits, InspectionUsage, LoadedModule, ModuleImage,
    PresentedShape, RecordMemberLayout, Result, StackFrame, StackFrameId, StackSegment,
    TypeReference, Unfollowed, UnwindTermination, Variable, VariableKind, VariableQuery,
    VariableState, VariableUnavailableReason, VirtualAddress,
};

use super::evaluation::{StopMachine, StopPlace};
use super::frames::{
    Expanded, FrameOrigin, FrameRegisters, FrameScope, FrameSource, PhysicalStack, ResolvedFrame,
    RootOrigin, StackRoot, UnwindModule,
};

use super::inspection::variable_context;
use super::interpreted::{Placed, interpreted_frame};
use super::native::InspectionOps;
use super::{Controller, Inferior, RuntimeModule};

/// What describing one suspended task in a list may cost: far less than an
/// inspection, since a page may hold thousands, and enough for any of the
/// views of what tokio's futures wait for.
const DESCRIPTION_LIMITS: InspectionLimits = InspectionLimits {
    variables: 16,
    value_nodes: 64,
    aggregate_depth: 16,
    memory_reads: 64,
    memory_bytes: 16 * 1_024,
    expression_work: 256_000,
};

/// A suspended task's frames, innermost first, each with the future it
/// was read from, and why they end.
pub(super) struct AsyncStack {
    pub(super) frames: Vec<StackFrame>,
    pub(super) futures: Vec<futures::AsyncFrame>,
    pub(super) termination: UnwindTermination,
}

/// Why the future a frame drives cannot be read, and its declared type
/// when the debug information says it.
pub(super) struct Unread {
    reason: Arc<str>,
    declared: Option<TypeReference>,
}

impl Unread {
    const fn new(reason: Arc<str>, declared: Option<TypeReference>) -> Self {
        Self { reason, declared }
    }
}

/// One variable of a suspended async frame: a member of its future.
pub(super) struct SavedVariable<'a> {
    pub(super) name: &'a Arc<str>,
    pub(super) ty: TypeReference,
    pub(super) address: VirtualAddress,
    pub(super) kind: VariableKind,
    pub(super) member: &'a crate::RecordMember,
}

impl<P: InspectionOps> Controller<P> {
    /// The frames of a suspended task's stack, or `None` for a stack a
    /// thread's or task's registers begin.
    pub(super) fn async_stack(
        &self,
        inferior: &Inferior,
        root: &StackRoot,
    ) -> Result<Option<AsyncStack>> {
        self.async_stack_counted(inferior, root, None)
    }

    /// [`Self::async_stack`], counting its reads in `usage` when given.
    fn async_stack_counted(
        &self,
        inferior: &Inferior,
        root: &StackRoot,
        usage: Option<&Cell<InspectionUsage>>,
    ) -> Result<Option<AsyncStack>> {
        let RootOrigin::Suspended {
            future,
            ty,
            module,
            reader,
        } = &root.origin
        else {
            return Ok(None);
        };

        let runtime_module = self.suspended_module(module, *ty)?;
        let image = runtime_module.image.as_ref();
        let chain = self.with_process_stop(inferior, module, *reader, usage, |stop| {
            futures::walk(image, stop, *future, *ty)
        });
        let frames = chain
            .frames
            .iter()
            .enumerate()
            .map(|(level, frame)| Ok(async_frame(image, module, frame_u32(level)?, frame)))
            .collect::<Result<_>>()?;
        let termination = termination(chain.end.clone());
        Ok(Some(AsyncStack {
            frames,
            futures: chain.frames,
            termination,
        }))
    }

    /// A suspended task's backtrace, or `None` for any other stack.
    pub(super) fn async_backtrace(
        &self,
        inferior: &Inferior,
        root: &StackRoot,
    ) -> Result<Option<Backtrace>> {
        Ok(self.async_stack(inferior, root)?.map(|stack| Backtrace {
            context: root.context,
            frames: stack.frames.into(),
            termination: stack.termination,
            unfollowed: Arc::from([]),
        }))
    }

    /// One frame of a suspended task, and what evaluates its variables, or
    /// `None` for any other stack.
    pub(super) fn resolve_async_frame(
        &self,
        inferior: &Inferior,
        root: &StackRoot,
        id: StackFrameId,
    ) -> Result<Option<ResolvedFrame>> {
        let Some(stack) = self.async_stack(inferior, root)? else {
            return Ok(None);
        };

        let level = id.get() as usize;
        let Some(frame) = stack.frames.get(level).cloned() else {
            return Err(Error::FrameNotFound {
                frame: id,
                frames: frame_u32(stack.frames.len())?,
            });
        };

        Ok(Some(self.suspended_frame(id, frame, &stack.futures[level])))
    }

    /// Each frame of a future a chain awaits with what it waits for: the
    /// summary of the view that presents the future.
    /// The views share one inspection's budget.
    pub(super) fn describe_awaited(
        &self,
        inferior: &Inferior,
        stop_id: StopId,
        root: &StackRoot,
        frames: &mut [StackFrame],
    ) {
        let mut budget = InspectionBudget::new(InspectionLimits::default());
        for frame in frames {
            frame.awaiting = self.awaited_summary(inferior, stop_id, root, frame, &mut budget);
        }
    }

    /// What a suspended task waits for: what its chain of awaits ends at,
    /// as the view that presents that future summarizes it, within a small
    /// budget of its own, which `usage` is charged with, as are the reads
    /// that follow the chain.
    pub(super) fn task_awaiting(
        &self,
        inferior: &Inferior,
        stop_id: StopId,
        task: crate::TaskId,
        reader: nix::unistd::Pid,
        usage: &Cell<InspectionUsage>,
    ) -> Option<Arc<str>> {
        let root = self.task_root(inferior, task, reader).ok()??;
        let stack = self
            .async_stack_counted(inferior, &root, Some(usage))
            .ok()??;
        let mut budget = InspectionBudget::new(DESCRIPTION_LIMITS);
        let described =
            self.awaited_summary(inferior, stop_id, &root, stack.frames.first()?, &mut budget);
        let (mut total, spent) = (usage.get(), budget.usage());
        total.value_nodes += spent.value_nodes;
        total.memory_reads += spent.memory_reads;
        total.memory_bytes += spent.memory_bytes;
        total.expression_work += spent.expression_work;
        usage.set(total);
        described
    }

    fn awaited_summary(
        &self,
        inferior: &Inferior,
        stop_id: StopId,
        root: &StackRoot,
        frame: &StackFrame,
        budget: &mut InspectionBudget,
    ) -> Option<Arc<str>> {
        let FrameKind::Awaited { object, ty } = frame.kind else {
            return None;
        };

        let module = self.module_of(ty)?;
        let leaf = futures::AsyncFrame {
            object,
            ty,
            kind: AsyncFrameKind::Leaf,
        };

        let resolved = self.suspended_frame(frame.id, frame.clone(), &leaf);
        let scope = self.frame_for(inferior, stop_id, root, &resolved);

        // The backtrace is itself the request run control waits behind.
        let mut machine = StopMachine::new(&scope, budget, false);
        let place = StopPlace {
            module: module.loaded.id,
            located: Located {
                ty: ty.id,
                storage: ValueStorage::Memory(object),
            },
        };

        match machine.present(&place.into()).ok()?.state {
            VariableState::Available {
                presentation: Some(presentation),
                ..
            } if presentation.problem.is_none() && presentation.shape != PresentedShape::Raw => {
                Some(Arc::clone(&presentation.summary))
            }
            _ => None,
        }
    }

    /// The frame of a suspended future, which runs no code, and what
    /// evaluates its variables.
    pub(super) fn suspended_frame(
        &self,
        id: StackFrameId,
        frame: StackFrame,
        future: &futures::AsyncFrame,
    ) -> ResolvedFrame {
        let scope = match future {
            futures::AsyncFrame {
                object,
                ty,
                kind: AsyncFrameKind::Coroutine { state, .. },
            } => FrameScope::Suspended {
                object: *object,
                ty: *ty,
                state: *state,
            },
            futures::AsyncFrame { .. } => FrameScope::Unavailable,
        };

        let code = frame
            .module
            .zip(frame.instruction)
            .and_then(|(id, address)| {
                let module = self.modules.get(&id)?;
                Some((id, module.loaded.image_address(address).ok()?))
            });
        ResolvedFrame {
            id,
            presented: PresentedFrame::Physical,
            frame: Some(frame),
            code,
            scope,
            registers: FrameRegisters::Discarded,
            cfa: Err(VariableUnavailableReason::CallFrameUnavailable(
                CallFrameUnavailableReason::Suspended,
            )
            .into()),
            activation: 0,
            below_stack_pointer: None,
        }
    }

    /// A stack's frames with what a runtime's frames drive spliced in
    /// before each, all renumbered, and why any is shown in part or not at
    /// all: the chain of awaits of each future one polls, and the segment
    /// of interpreted frames each activation of an interpreter's loop runs
    /// (see [`super::interpreted`]). A future being polled runs on the
    /// stack itself, so a driver below a frame that runs a coroutine is
    /// left as it is.
    pub(super) fn splice_driven(
        &self,
        inferior: &Inferior,
        root: &StackRoot,
        stack: &PhysicalStack,
        modules: &[UnwindModule<'_>],
        expanded: Expanded,
    ) -> Result<Expanded> {
        let runtimes = self.runtimes(inferior);
        let drivers = drivers(&expanded.trace.frames, |frame| {
            runtimes
                .iter()
                .enumerate()
                .filter(|(_, runtime)| {
                    Some(runtime.module.id) == frame.module
                        && runtime.model.driver_role() == Some(frame.role)
                })
                .find_map(|(index, runtime)| Some((runtime.model.driven(frame)?, index)))
        });

        let mut placement = self.placement(inferior, root, &runtimes, &drivers);
        let drivers: Vec<(usize, Driven)> = drivers
            .into_iter()
            .map(|(frame, driven, _)| (frame, driven))
            .collect();
        if drivers.is_empty() && placement.is_empty() {
            return Ok(expanded);
        }

        // The notes' frame identifiers would be the unspliced trace's.
        debug_assert!(
            expanded.trace.unfollowed.is_empty(),
            "splicing a trace that already notes {} futures not followed",
            expanded.trace.unfollowed.len()
        );

        let mut spliced = Spliced {
            frames: Vec::with_capacity(expanded.trace.frames.len()),
            origins: Vec::with_capacity(expanded.trace.frames.len()),
            futures: expanded.futures.clone(),
            interpreted: expanded.interpreted.clone(),
            unfollowed: expanded.trace.unfollowed.to_vec(),
        };
        let mut drivers = drivers.into_iter().peekable();

        // Frames of one runtime may each drive the same future, as its
        // scheduler's closure does within its `block_on`: the future shows
        // once, before the innermost frame that reads it, and a frame that
        // cannot read its future is noted only if no other frame shows one.
        let mut shown = BTreeSet::new();
        let mut unread = None;
        let mut lost = Vec::new();
        for (index, frame) in expanded.trace.frames.iter().enumerate() {
            let activation = expanded.origins[index].activation;
            for driven in placement.placed.remove(&index).unwrap_or_default() {
                spliced.push_interpreted(driven, frame.segment, activation);
            }

            if let Some(reason) = placement.notes.remove(&index) {
                spliced.note(DrivenFrames::Interpreted, reason)?;
            }

            if let Some((_, Driven::Future { variable })) =
                drivers.next_if(|(driver, _)| *driver == index)
            {
                let driver = StackFrameId::new(frame_u32(index)?);
                match self.driven_chain(inferior, root, stack, modules, &expanded, driver, variable)
                {
                    Ok((module, chain)) => {
                        let outermost = chain.frames.last().map(|future| future.object);
                        if outermost.is_none_or(|object| shown.insert(object)) {
                            for future in chain.frames {
                                let mut built =
                                    async_frame(module.image.as_ref(), &module.loaded, 0, &future);
                                built.segment = StackSegment::Future;
                                spliced.frames.push(built);
                                spliced.origins.push(FrameOrigin {
                                    activation,
                                    source: FrameSource::Future(spliced.futures.len()),
                                });
                                spliced.futures.push(future);
                            }

                            if let Some(reason) = end_reason(&chain.end) {
                                spliced.note(DrivenFrames::Future, reason)?;
                            }
                        }
                    }
                    Err(Unread { reason, declared }) => {
                        if unread.is_none() {
                            unread = Some(Unfollowed {
                                driver: spliced.here()?,
                                driven: DrivenFrames::Future,
                                reason,
                            });
                        }

                        lost.extend(declared);
                    }
                }
            }

            spliced.frames.push(frame.clone());
            spliced.origins.push(expanded.origins[index]);
        }

        if shown.is_empty() {
            spliced.unfollowed.extend(unread);
        } else {
            lost.clear();
        }

        // Frames whose place is unknown follow every frame of the stack,
        // and stand for its outermost.
        if let (Some(reason), Some(outermost)) = (
            placement.unplaced_reason.take(),
            stack.frames.len().checked_sub(1),
        ) {
            spliced.note(DrivenFrames::Interpreted, reason)?;
            for driven in placement.unplaced {
                spliced.push_interpreted(driven, StackSegment::Unplaced, outermost);
            }
        }

        let spliced = spliced.expanded(expanded.trace, lost)?;
        spliced.check_invariants(stack.frames.len());
        Ok(spliced)
    }

    /// The chain of awaits of the future the frame `driver` drives through
    /// its variable `variable`, and the module that describes it; or why
    /// the future cannot be read, and its declared type when known.
    #[expect(clippy::too_many_arguments, reason = "a stack's parts and the frame")]
    fn driven_chain(
        &self,
        inferior: &Inferior,
        root: &StackRoot,
        stack: &PhysicalStack,
        modules: &[UnwindModule<'_>],
        expanded: &Expanded,
        driver: StackFrameId,
        variable: &str,
    ) -> std::result::Result<(&RuntimeModule, futures::AwaitChain), Unread> {
        let unread = |why: &dyn std::fmt::Display| -> Arc<str> {
            format!("`{variable}`, which holds the future, {why}").into()
        };

        let resolved = self
            .resolved_in(inferior, root, stack, modules, expanded, driver)
            .map_err(|error| Unread::new(unread(&format_args!("cannot be read: {error}")), None))?;
        let (module, address, selected) = self.frame_scope(&resolved).ok_or_else(|| {
            Unread::new(
                unread(&"is in no scope the debug information describes"),
                None,
            )
        })?;
        let key = module
            .variables
            .visible_object(address, selected, variable)
            .map_err(|error| {
                Unread::new(unread(&format_args!("cannot be found: {error}")), None)
            })?;
        let declared = module
            .variables
            .object_type(key)
            .ok()
            .map(|id| TypeReference {
                image: module.loaded.image,
                id,
            });
        let mut runtime = self.frame_runtime(inferior, root, &resolved, module);
        let mut budget = InspectionBudget::new(crate::InspectionLimits::default());
        let located = match module
            .variables
            .locate(key, Some(address), &mut runtime, &mut budget)
        {
            Ok(Ok(located)) => located,
            Ok(Err(VariableState::Unavailable(reason))) => {
                return Err(Unread::new(
                    unread(&format_args!("is unavailable: {reason}")),
                    declared,
                ));
            }
            Ok(Err(_)) => return Err(Unread::new(unread(&"is unreadable"), declared)),
            Err(error) => {
                return Err(Unread::new(
                    unread(&format_args!("cannot be read: {error}")),
                    declared,
                ));
            }
        };

        let ValueStorage::Memory(object) = located.storage else {
            return Err(Unread::new(unread(&"is not in memory"), declared));
        };

        let ty = TypeReference {
            image: module.loaded.image,
            id: located.ty,
        };

        // The future a function was passed moves to be pinned, and where
        // it was still reads as a future that never began.
        if !futures::pinned(module.image.as_ref(), ty) {
            return Err(Unread::new(
                unread(&"is not the pinned future here"),
                declared,
            ));
        }

        let chain = self.with_module_stop(inferior, &module.loaded, root.reader(), |stop| {
            futures::walk(module.image.as_ref(), stop, object, ty)
        });
        Ok((module, chain))
    }

    /// The variables a suspended async frame shows, each read from its
    /// future: those of `query`.
    pub(super) fn async_variables(
        &self,
        stop_id: StopId,
        root: &StackRoot,
        resolved: &ResolvedFrame,
        query: &VariableQuery,
        budget: &mut InspectionBudget,
    ) -> Result<Vec<Variable>> {
        let FrameScope::Suspended { ty, .. } = resolved.scope else {
            return Err(Error::VariableContextUnsupported);
        };

        let module = self.module_of(ty).ok_or(Error::AddressOutsideModule)?;
        let saved = self.saved_variables(resolved)?;
        let wanted = |variable: &SavedVariable<'_>| match query {
            VariableQuery::All => true,
            VariableQuery::Name(name) => **variable.name == **name,
            VariableQuery::Global(_) => false,
        };

        let inferior = self.inferior.as_ref().ok_or(Error::NotRunning)?;
        let mut runtime = self.frame_runtime(inferior, root, resolved, module);
        let context = variable_context(stop_id, root.context, resolved.id, module, None);
        let mut variables = Vec::new();
        for variable in saved.iter().filter(|variable| wanted(variable)) {
            if budget.consume_variable_value().is_err() {
                break;
            }

            let located = Located {
                ty: variable.ty.id,
                storage: ValueStorage::Memory(variable.address),
            };

            let inspected =
                module
                    .variables
                    .materialize(&located, context, &mut runtime, budget)?;
            variables.push(Variable {
                kind: variable.kind,
                global: None,
                name: Arc::clone(variable.name),
                declaration: variable.member.declaration.clone(),
                type_info: inspected.type_info,
                unresolved_shape: None,
                state: inspected.state,
            });
        }

        if let VariableQuery::Name(name) = query
            && variables.is_empty()
        {
            return Err(Error::VariableNotFound(name.clone()));
        }

        Ok(variables)
    }

    /// The variables a suspended async frame holds, in its future.
    pub(super) fn saved_variables<'a>(
        &'a self,
        resolved: &ResolvedFrame,
    ) -> Result<Vec<SavedVariable<'a>>> {
        let FrameScope::Suspended { object, ty, state } = resolved.scope else {
            return Ok(Vec::new());
        };

        let module = self.module_of(ty).ok_or(Error::AddressOutsideModule)?;
        let Some(Ok(coroutine)) = module.image.coroutine(ty.id) else {
            return Ok(Vec::new());
        };

        let Some(state) = coroutine.state(state) else {
            return Ok(Vec::new());
        };

        Ok(saved(coroutine, state, object, ty))
    }

    /// The module whose image describes a suspended task's future.
    fn suspended_module(&self, module: &LoadedModule, ty: TypeReference) -> Result<&RuntimeModule> {
        self.modules
            .get(&module.id)
            .filter(|found| found.loaded.image == ty.image)
            .ok_or(Error::StaleModuleImage)
    }
}

/// Why a chain of awaits ends, as a backtrace's termination.
fn termination(end: ChainEnd) -> UnwindTermination {
    match end {
        ChainEnd::Leaf | ChainEnd::Unresumed | ChainEnd::Finished => UnwindTermination::Complete,
        ChainEnd::NoAwaitee => UnwindTermination::BrokenAwaitChain {
            reason: "the debug information does not say what the innermost async function \
                     awaits"
                .into(),
        },
        ChainEnd::Broken(reason) => UnwindTermination::BrokenAwaitChain { reason },
        ChainEnd::TooDeep => UnwindTermination::DepthLimit,
        ChainEnd::Cycle => UnwindTermination::CycleDetected,
    }
}

/// Why a chain of awaits ends before its end, if it does.
fn end_reason(end: &ChainEnd) -> Option<Arc<str>> {
    match termination(end.clone()) {
        UnwindTermination::Complete => None,
        incomplete => Some(incomplete.to_string().into()),
    }
}

/// A trace with frames spliced in, as it is built.
struct Spliced {
    frames: Vec<StackFrame>,
    origins: Vec<FrameOrigin>,
    futures: Vec<futures::AsyncFrame>,
    interpreted: Vec<Placed>,
    unfollowed: Vec<Unfollowed>,
}

impl Spliced {
    /// The frame the next frame pushed will be.
    fn here(&self) -> Result<StackFrameId> {
        frame_u32(self.frames.len()).map(StackFrameId::new)
    }

    /// Notes that the frames the next frame pushed drives are not all
    /// shown, and why.
    fn note(&mut self, frames: DrivenFrames, reason: Arc<str>) -> Result<()> {
        let driver = self.here()?;
        self.unfollowed.push(Unfollowed {
            driver,
            driven: frames,
            reason,
        });
        Ok(())
    }

    /// Pushes an interpreted frame on the stack `segment`, which the
    /// physical frame `activation` runs or stands for.
    fn push_interpreted(&mut self, frame: Placed, segment: StackSegment, activation: usize) {
        self.frames.push(interpreted_frame(&frame.frame, segment));
        self.origins.push(FrameOrigin {
            activation,
            source: FrameSource::Interpreted(self.interpreted.len()),
        });
        self.interpreted.push(frame);
    }

    /// The finished trace, its frames numbered by their levels.
    fn expanded(mut self, trace: Backtrace, lost: Vec<TypeReference>) -> Result<Expanded> {
        for (level, frame) in self.frames.iter_mut().enumerate() {
            let level = frame_u32(level)?;
            frame.level = level;
            frame.id = StackFrameId::new(level);
        }

        Ok(Expanded {
            trace: Backtrace {
                frames: self.frames.into(),
                unfollowed: self.unfollowed.into(),
                ..trace
            },
            origins: self.origins,
            futures: self.futures,
            interpreted: self.interpreted,
            lost,
        })
    }
}

/// Each frame of `frames`, innermost first, that drives frames the stack
/// does not show, with what `driven` says it drives and who says so. A
/// future being polled runs on the stack itself, so a frame below one that
/// runs a coroutine drives no future the stack does not already show.
fn drivers<R>(
    frames: &[StackFrame],
    driven: impl Fn(&StackFrame) -> Option<(Driven, R)>,
) -> Vec<(usize, Driven, R)> {
    let mut polling = false;
    let mut drivers = Vec::new();
    for (index, frame) in frames.iter().enumerate() {
        match driven(frame) {
            Some((Driven::Future { .. }, _)) if polling => {}
            Some((driving, by)) => drivers.push((index, driving, by)),
            None => {}
        }

        polling |= frame
            .function
            .as_ref()
            .is_some_and(|function| function.coroutine.is_some());
    }

    drivers
}

/// The variables a coroutine of type `ty` at `object` holds in `state`.
fn saved<'a>(
    coroutine: &'a CoroutineInfo,
    state: &'a crate::CoroutineState,
    object: VirtualAddress,
    ty: TypeReference,
) -> Vec<SavedVariable<'a>> {
    coroutine
        .variables(state)
        .filter_map(|(member, capture)| {
            let RecordMemberLayout::ByteOffset(offset) = member.layout else {
                return None;
            };

            // A member past the end of memory is no variable to read.
            let address = VirtualAddress::new(object.get().checked_add(offset)?);
            Some(SavedVariable {
                name: member.name.as_ref()?,
                ty: TypeReference {
                    image: ty.image,
                    id: member.type_ref.id,
                },
                address,
                // An async function's captures are its arguments.
                kind: if capture && coroutine.kind == crate::CoroutineKind::AsyncFunction {
                    VariableKind::Parameter
                } else {
                    VariableKind::Local
                },
                member,
            })
        })
        .collect()
}

/// A frame level or count as `u32`. No unwound stack comes near `u32::MAX`
/// frames, but a longer one must not wrap into duplicate levels.
fn frame_u32(value: usize) -> Result<u32> {
    u32::try_from(value).map_err(|_| Error::internal("a stack has more frames than u32 levels"))
}

/// A frame of a suspended task's chain of awaits.
fn async_frame(
    image: &ModuleImage,
    module: &LoadedModule,
    level: u32,
    frame: &futures::AsyncFrame,
) -> StackFrame {
    let mut built = match &frame.kind {
        AsyncFrameKind::Coroutine {
            state, location, ..
        } => {
            let functions = image.coroutine_functions(frame.ty.id);
            let function = functions.first().map(|function| function.info());
            let role = function
                .as_ref()
                .map_or(CodeRole::Ordinary, |function| function.role);
            let resumes = resume_address(image, &functions, *state)
                .map(|address| module.load_bias.wrapping_load(address));
            StackFrame::from_parts(
                level,
                FrameKind::Async {
                    object: frame.object,
                },
                Some(module.id),
                resumes,
                FrameMetadata {
                    code_instance: None,
                    function,
                    source: location.clone(),
                    symbol: None,
                    role,
                },
            )
        }
        AsyncFrameKind::Leaf => StackFrame::from_parts(
            level,
            FrameKind::Awaited {
                object: frame.object,
                ty: frame.ty,
            },
            Some(module.id),
            None,
            FrameMetadata {
                code_instance: None,
                function: None,
                source: None,
                symbol: None,
                role: CodeRole::Ordinary,
            },
        ),
    };

    built.segment = StackSegment::Task;
    built
}

/// Where the one out-of-line function that runs a coroutine resumes in
/// `state`, when the debugger decoded its dispatch.
pub fn resume_address(
    image: &ModuleImage,
    functions: &[crate::Function<'_>],
    state: u64,
) -> Option<ImageAddress> {
    let mut instances = functions
        .iter()
        .flat_map(|function| function.instances())
        .filter(|instance| matches!(instance.kind(), CodeInstanceKind::OutOfLine));
    let instance = instances.next()?;
    if instances.next().is_some() {
        return None;
    }

    let points = image.resume_points(instance.id())?.ok()?;
    Some(points.point(state)?.address)
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A frame of code in `role` that no debug information describes, or
    /// of an async function's coroutine when `coroutine`.
    fn frame(level: u32, role: CodeRole, coroutine: bool) -> StackFrame {
        let mut frame = StackFrame::new(level, FrameKind::Physical, None, VirtualAddress::new(1));
        frame.role = role;
        frame.function = coroutine.then(|| crate::FunctionInfo {
            id: crate::FunctionId::new(level),
            name: "poll".into(),
            linkage_name: None,
            declaration: None,
            language: crate::SourceLanguage::Rust,
            role,
            enclosing: None,
            coroutine: Some(crate::TypeId::new(0)),
            generics: Arc::from([]),
            main_subprogram: false,
        });
        frame
    }

    /// Drivers are found by what the model says of a frame, which a
    /// frame's code role alone can decide, and a future polled within a
    /// coroutine's poll is not driven again.
    #[test]
    fn drivers_are_the_frames_a_model_says_drive_and_no_future_runs_within() {
        let future = Driven::Future { variable: "future" };
        let driven =
            |frame: &StackFrame| (frame.role == CodeRole::RuntimeInternal).then_some((future, ()));
        let frames = [
            frame(0, CodeRole::Ordinary, false),
            frame(1, CodeRole::RuntimeInternal, false),
            frame(2, CodeRole::Ordinary, true),
            frame(3, CodeRole::RuntimeInternal, false),
        ];
        assert_eq!(drivers(&frames, driven), [(1, future, ())]);
        assert_eq!(drivers(&frames[3..], driven), [(0, future, ())]);
        assert!(drivers(&frames, |_| None::<(Driven, ())>).is_empty());

        // An interpreter's loop runs its frames on the stack, wherever it is.
        let interpreted = |frame: &StackFrame| {
            (frame.role != CodeRole::Ordinary).then_some((Driven::Interpreted, ()))
        };
        assert_eq!(
            drivers(&frames, interpreted),
            [(1, Driven::Interpreted, ()), (3, Driven::Interpreted, ())]
        );
    }
}
