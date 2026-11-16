use std::cell::Cell;
use std::collections::BTreeSet;
use std::sync::Arc;

use crate::debug_info::Located;
use crate::eval::target::Machine;
use crate::inspection::InspectionBudget;
use crate::model::{FrameMetadata, ValueStorage};
use crate::protocol::{PresentedFrame, StopId};
use crate::runtime_model::futures::{self, AsyncFrameKind, ChainEnd};
use crate::runtime_model::{Driven, DrivenFuture};

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

const DESCRIPTION_LIMITS: InspectionLimits = InspectionLimits {
    variables: 16,
    value_nodes: 64,
    aggregate_depth: 16,
    memory_reads: 64,
    memory_bytes: 16 * 1_024,
    expression_work: 256_000,
};

pub(super) struct AsyncStack {
    pub(super) frames: Vec<StackFrame>,
    pub(super) futures: Vec<futures::AsyncFrame>,
    pub(super) termination: UnwindTermination,
}

pub(super) struct Unread {
    pub(super) reason: Arc<str>,
    pub(super) declared: Option<TypeReference>,
}

impl Unread {
    const fn new(reason: Arc<str>, declared: Option<TypeReference>) -> Self {
        Self { reason, declared }
    }
}

pub(super) struct SavedVariable<'a> {
    pub(super) name: &'a Arc<str>,
    pub(super) ty: TypeReference,
    pub(super) address: VirtualAddress,
    pub(super) kind: VariableKind,
    pub(super) member: &'a crate::RecordMember,
}

impl<P: InspectionOps> Controller<P> {
    pub(super) fn async_stack(
        &self,
        inferior: &Inferior,
        root: &StackRoot,
    ) -> Result<Option<AsyncStack>> {
        self.async_stack_counted(inferior, root, None)
    }

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

        let future = stack
            .futures
            .get(level)
            .ok_or_else(|| Error::internal("an async stack has a future for each frame"))?;
        Ok(Some(self.suspended_frame(id, frame, future)))
    }

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

    pub(super) fn splice_driven(
        &self,
        inferior: &Inferior,
        root: &StackRoot,
        stack: &PhysicalStack,
        modules: &[UnwindModule<'_>],
        expanded: Expanded,
    ) -> Result<Expanded> {
        let runtimes = self.runtimes(inferior);
        let drivers = drivers(&expanded.trace.frames, |frame, inner| {
            runtimes
                .iter()
                .enumerate()
                .filter(|(_, runtime)| {
                    Some(runtime.module.id) == frame.module
                        && runtime.model.driver_role() == Some(frame.role)
                })
                .find_map(|(index, runtime)| Some((runtime.model.driven(frame, inner)?, index)))
        });

        let mut placement = self.placement(inferior, root, &runtimes, &drivers);
        let drivers: Vec<(usize, Driven)> = drivers
            .into_iter()
            .map(|(frame, driven, _)| (frame, driven))
            .collect();
        if drivers.is_empty() && placement.is_empty() && expanded.awaiting.is_empty() {
            return Ok(expanded);
        }

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

        let mut shown = BTreeSet::new();
        let mut unread = None;
        let origins = expanded.trace.frames.iter().zip(&expanded.origins);
        for (index, (frame, &origin)) in origins.enumerate() {
            let activation = origin.activation;
            for driven in placement.placed.remove(&index).unwrap_or_default() {
                spliced.push_interpreted(driven, frame.segment, activation);
            }

            if let Some(reason) = placement.notes.remove(&index) {
                spliced.note(DrivenFrames::Interpreted, reason)?;
            }
            spliced.push_awaiting(&expanded.awaiting, index, activation)?;

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
                    Err(Unread { reason, .. }) => {
                        if unread.is_none() {
                            unread = Some(Unfollowed {
                                driver: spliced.here()?,
                                driven: DrivenFrames::Future,
                                reason,
                            });
                        }
                    }
                }
            }

            spliced.frames.push(frame.clone());
            spliced.origins.push(origin);
        }

        if shown.is_empty() {
            spliced.unfollowed.extend(unread);
        }

        if let (Some(reason), Some(outermost)) = (
            placement.unplaced_reason.take(),
            stack.frames.len().checked_sub(1),
        ) {
            spliced.note(DrivenFrames::Interpreted, reason)?;
            for driven in placement.unplaced {
                spliced.push_interpreted(driven, StackSegment::Unplaced, outermost);
            }
        }

        let spliced = spliced.expanded(expanded.trace)?;
        spliced.check_invariants(stack.frames.len());
        Ok(spliced)
    }

    pub(super) fn polled_future(
        &self,
        pid: nix::unistd::Pid,
        coroutine: TypeReference,
        at: Option<StackFrameId>,
    ) -> Option<VirtualAddress> {
        let inferior = self.session.inferior()?;
        let root = StackRoot::of_thread(pid);
        let stack = self
            .physical_stack(inferior, &root, crate::unwind::DEFAULT_MAX_FRAMES)
            .ok()?;
        let modules = self.unwind_modules(inferior);
        let expanded = self
            .expand_backtrace(
                inferior,
                &root,
                &stack,
                crate::unwind::DEFAULT_MAX_FRAMES,
                &modules,
                None,
            )
            .ok()?;
        let frames = &expanded.trace.frames;
        let runs = |frame: &StackFrame| {
            frame
                .function
                .as_ref()
                .filter(|_| frame.module == Some(inferior.loaded_module.id))
                .and_then(|function| function.coroutine)
        };

        let body = match at {
            Some(at) => frames
                .iter()
                .position(|frame| frame.id == at)
                .filter(|&body| frames.get(body).and_then(runs) == Some(coroutine.id))?,
            None => frames
                .iter()
                .zip(&expanded.origins)
                .enumerate()
                .find(|(_, (frame, _))| runs(frame) == Some(coroutine.id))
                .filter(|(_, (_, origin))| origin.activation == 0)
                .map(|(body, _)| body)?,
        };

        let runtimes = self.runtimes(inferior);
        let mut bodies = Vec::new();
        let mut driver = None;
        for (index, frame) in frames.iter().enumerate().skip(body) {
            let inner = index.checked_sub(1).and_then(|inner| frames.get(inner));
            let polls = runtimes
                .iter()
                .filter(|runtime| {
                    Some(runtime.module.id) == frame.module
                        && runtime.model.driver_role() == Some(frame.role)
                })
                .find_map(|runtime| {
                    runtime.model.polling(frame).or_else(|| {
                        match runtime.model.driven(frame, inner) {
                            Some(Driven::Future { variable }) => Some(variable),
                            _ => None,
                        }
                    })
                });
            if let Some(variable) = polls {
                driver = Some((index, variable));
                break;
            }

            if let Some(function) = runs(frame).and(frame.function.as_ref()) {
                bodies.push((function.id, frame.source.clone()));
            }
        }

        let (driver, variable) = driver?;
        let driver = StackFrameId::new(frame_u32(driver).ok()?);
        let (module, object, ty) = self
            .driven_root(
                inferior, &root, &stack, &modules, &expanded, driver, variable,
            )
            .ok()?;

        bodies.reverse();
        let lines: Vec<_> = bodies
            .iter()
            .take(bodies.len().saturating_sub(1))
            .map(|(_, source)| source.clone())
            .collect();
        let chain = self.with_module_stop(inferior, &module.loaded, root.reader(), |stop| {
            futures::walk_choosing(
                module.image.as_ref(),
                stop,
                object,
                ty,
                &mut |at, _, coroutine| {
                    lines.get(at).map_or(futures::StateChoice::Stored, |line| {
                        awaiting_at(coroutine, line.as_ref())
                    })
                },
            )
        });

        let outermost_first: Vec<_> = bodies.iter().map(|(function, _)| *function).collect();
        polled_by(&outermost_first, &chain, |future, function| {
            module
                .image
                .coroutine_functions(future.ty.id)
                .iter()
                .any(|candidate| candidate.id() == function)
        })
    }

    pub(super) fn driven_futures(
        &self,
        inferior: &Inferior,
        root: &StackRoot,
        stack: &PhysicalStack,
        modules: &[UnwindModule<'_>],
        expanded: &Expanded,
    ) -> (Vec<(&RuntimeModule, DrivenFuture)>, Vec<Unread>) {
        let frames = &expanded.trace.frames;
        let runtimes = self.runtimes(inferior);
        let in_role = |frame: &StackFrame| {
            runtimes
                .iter()
                .filter(move |runtime| {
                    Some(runtime.module.id) == frame.module
                        && runtime.model.driver_role() == Some(frame.role)
                })
                .collect::<Vec<_>>()
        };
        let runs = |frame: &StackFrame| {
            frame
                .function
                .as_ref()
                .filter(|_| {
                    frame.module == Some(inferior.loaded_module.id)
                        && frame.segment != StackSegment::Future
                })
                .and_then(|function| Some((function.id, function.coroutine?)))
        };

        let mut driven: Vec<(&RuntimeModule, DrivenFuture)> = Vec::new();
        let mut unread = Vec::new();
        for (index, frame) in frames.iter().enumerate() {
            let inner = index.checked_sub(1).and_then(|inner| frames.get(inner));
            let Some(variable) = in_role(frame).iter().find_map(|runtime| {
                match runtime.model.driven(frame, inner) {
                    Some(Driven::Future { variable }) => Some(variable),
                    _ => None,
                }
            }) else {
                continue;
            };

            let polls = |frame: &StackFrame| {
                in_role(frame)
                    .iter()
                    .any(|runtime| runtime.model.polling(frame).is_some())
            };
            let bodies = bodies(frames, index, polls, runs);

            let Ok(at_driver) = frame_u32(index).map(StackFrameId::new) else {
                continue;
            };
            let (module, object, ty) = match self.driven_root(
                inferior, root, stack, modules, expanded, at_driver, variable,
            ) {
                Ok(found) => found,
                Err(why) => {
                    unread.push(why);
                    continue;
                }
            };

            let chain = self.bodies_chain(
                inferior,
                root,
                stack,
                modules,
                expanded,
                (module, object, ty),
                &bodies,
            );
            let outermost = chain.frames.last().map_or(object, |future| future.object);
            if driven.iter().any(|(_, future)| {
                future.chain.last().map_or(future.object, |f| f.object) == outermost
            }) {
                continue;
            }

            let Some(running) = running_beside(module, &chain, &bodies) else {
                unread.push(Unread::new(
                    "the frames that poll it run other coroutines than it awaits".into(),
                    Some(ty),
                ));
                continue;
            };

            driven.push((
                module,
                DrivenFuture {
                    object,
                    ty,
                    chain: chain.frames,
                    running,
                },
            ));
        }

        (driven, unread)
    }

    #[expect(clippy::too_many_arguments, reason = "a stack's parts and the future")]
    fn bodies_chain(
        &self,
        inferior: &Inferior,
        root: &StackRoot,
        stack: &PhysicalStack,
        modules: &[UnwindModule<'_>],
        expanded: &Expanded,
        (module, object, ty): (&RuntimeModule, VirtualAddress, TypeReference),
        bodies: &[Body],
    ) -> futures::AwaitChain {
        let polled: Vec<_> = bodies
            .iter()
            .map(|body| {
                let inner = StackFrameId::new(frame_u32(body.frame.checked_sub(1)?).ok()?);
                Some(self.polled_self(inferior, root, stack, modules, expanded, inner))
            })
            .collect();

        self.with_module_stop(inferior, &module.loaded, root.reader(), |stop| {
            futures::walk_choosing(
                module.image.as_ref(),
                stop,
                object,
                ty,
                &mut |at, object, coroutine| {
                    let Some(body) = bodies.get(at) else {
                        return futures::StateChoice::Stored;
                    };
                    match (awaiting_at(coroutine, body.line.as_ref()), polled.get(at)) {
                        (futures::StateChoice::Unknown, Some(Some(Polled::At(polled)))) => {
                            awaiting(coroutine, |awaitee| {
                                matches!(awaitee.layout, RecordMemberLayout::ByteOffset(offset)
                                    if object.checked_add(offset) == Some(*polled))
                            })
                        }
                        (futures::StateChoice::Unknown, Some(Some(Polled::Typed(Some(ty))))) => {
                            let image = module.image.as_ref();
                            futures::pinned_target(image, *ty).map_or(
                                futures::StateChoice::Unknown,
                                |polled| {
                                    awaiting(coroutine, |awaitee| {
                                        image.same_type(awaitee.type_ref, polled)
                                    })
                                },
                            )
                        }
                        (choice, _) => choice,
                    }
                },
            )
        })
    }

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
        let (module, object, ty) =
            self.driven_root(inferior, root, stack, modules, expanded, driver, variable)?;
        let chain = self.with_module_stop(inferior, &module.loaded, root.reader(), |stop| {
            futures::walk(module.image.as_ref(), stop, object, ty)
        });
        Ok((module, chain))
    }

    #[expect(clippy::too_many_arguments, reason = "a stack's parts and the frame")]
    fn driven_root(
        &self,
        inferior: &Inferior,
        root: &StackRoot,
        stack: &PhysicalStack,
        modules: &[UnwindModule<'_>],
        expanded: &Expanded,
        driver: StackFrameId,
        variable: &str,
    ) -> std::result::Result<(&RuntimeModule, VirtualAddress, TypeReference), Unread> {
        let unread = |why: &dyn std::fmt::Display| -> Arc<str> {
            format!("`{variable}`, which holds the future, {why}").into()
        };
        let (module, storage, ty, declared) =
            self.frame_variable(inferior, root, stack, modules, expanded, driver, variable)?;

        let ValueStorage::Memory(object) = storage else {
            return Err(Unread::new(unread(&"is not in memory"), declared));
        };

        if !futures::pinned(module.image.as_ref(), ty) {
            return Err(Unread::new(
                unread(&"is not the pinned future here"),
                declared,
            ));
        }

        Ok((module, object, ty))
    }

    fn polled_self(
        &self,
        inferior: &Inferior,
        root: &StackRoot,
        stack: &PhysicalStack,
        modules: &[UnwindModule<'_>],
        expanded: &Expanded,
        frame: StackFrameId,
    ) -> Polled {
        let (module, storage, ty, _) =
            match self.frame_variable(inferior, root, stack, modules, expanded, frame, "self") {
                Ok(found) => found,
                Err(unread) => return Polled::Typed(unread.declared),
            };
        if !futures::pinned(module.image.as_ref(), ty) {
            return Polled::Typed(None);
        }

        self.polled_word(inferior, root, module, storage)
            .map_or(Polled::Typed(Some(ty)), Polled::At)
    }

    fn polled_word(
        &self,
        inferior: &Inferior,
        root: &StackRoot,
        module: &RuntimeModule,
        storage: ValueStorage,
    ) -> Option<VirtualAddress> {
        let word = match storage {
            ValueStorage::Memory(at) => {
                self.with_module_stop(inferior, &module.loaded, root.reader(), |stop| {
                    let mut word = [0; 8];
                    stop.read(at, &mut word).then_some(word)
                })?
            }
            ValueStorage::Bytes {
                raw, start, end, ..
            } => raw.get(start..end)?.try_into().ok()?,
            _ => return None,
        };
        Some(VirtualAddress::new(u64::from_le_bytes(word)))
    }

    #[expect(clippy::too_many_arguments, reason = "a stack's parts and the frame")]
    fn frame_variable(
        &self,
        inferior: &Inferior,
        root: &StackRoot,
        stack: &PhysicalStack,
        modules: &[UnwindModule<'_>],
        expanded: &Expanded,
        driver: StackFrameId,
        variable: &str,
    ) -> std::result::Result<
        (
            &RuntimeModule,
            ValueStorage,
            TypeReference,
            Option<TypeReference>,
        ),
        Unread,
    > {
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

        let ty = TypeReference {
            image: module.loaded.image,
            id: located.ty,
        };
        Ok((module, located.storage, ty, declared))
    }

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

        let inferior = self.session.inferior().ok_or(Error::NotRunning)?;
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
                unnamed: None,
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

    fn suspended_module(&self, module: &LoadedModule, ty: TypeReference) -> Result<&RuntimeModule> {
        self.modules
            .get(&module.id)
            .filter(|found| found.loaded.image == ty.image)
            .ok_or(Error::StaleModuleImage)
    }
}

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

fn end_reason(end: &ChainEnd) -> Option<Arc<str>> {
    match termination(end.clone()) {
        UnwindTermination::Complete => None,
        incomplete => Some(incomplete.to_string().into()),
    }
}

struct Spliced {
    frames: Vec<StackFrame>,
    origins: Vec<FrameOrigin>,
    futures: Vec<futures::AsyncFrame>,
    interpreted: Vec<Placed>,
    unfollowed: Vec<Unfollowed>,
}

impl Spliced {
    fn hide_inside_agent(&mut self) {
        let Some(agent) = self.frames.iter().position(|frame| {
            matches!(frame.kind, FrameKind::Interpreted { .. })
                && frame.role == CodeRole::Interpreter
        }) else {
            return;
        };

        for frame in self.frames.iter_mut().take(agent) {
            if matches!(frame.role, CodeRole::Ordinary | CodeRole::Wrapper) {
                frame.role = CodeRole::Interpreter;
            }
        }
    }

    fn here(&self) -> Result<StackFrameId> {
        frame_u32(self.frames.len()).map(StackFrameId::new)
    }

    fn note(&mut self, frames: DrivenFrames, reason: Arc<str>) -> Result<()> {
        let driver = self.here()?;
        self.unfollowed.push(Unfollowed {
            driver,
            driven: frames,
            reason,
        });
        Ok(())
    }

    fn push_interpreted(&mut self, frame: Placed, segment: StackSegment, activation: usize) {
        self.frames.push(interpreted_frame(&frame.frame, segment));
        self.origins.push(FrameOrigin {
            activation,
            source: FrameSource::Interpreted(self.interpreted.len()),
        });
        self.interpreted.push(frame);
    }

    fn push_awaiting(
        &mut self,
        chains: &[(usize, crate::RuntimeId, crate::runtime_model::Awaiting)],
        index: usize,
        activation: usize,
    ) -> Result<()> {
        for (_, runtime, chain) in chains.iter().filter(|(at, ..)| *at == index) {
            self.push_chain(*runtime, chain, activation)?;
        }
        Ok(())
    }

    fn push_chain(
        &mut self,
        runtime: crate::RuntimeId,
        chain: &crate::runtime_model::Awaiting,
        activation: usize,
    ) -> Result<()> {
        for frame in &chain.frames {
            self.push_interpreted(
                Placed {
                    runtime,
                    frame: frame.clone(),
                },
                StackSegment::Awaiting,
                activation,
            );
            if let Some(recorded) = self
                .frames
                .last_mut()
                .and_then(|built| built.recorded.as_mut())
            {
                recorded.problem.clone_from(&frame.problem);
            }
        }
        if let Some(reason) = &chain.end {
            self.note(DrivenFrames::Awaiting, Arc::clone(reason))?;
        }
        Ok(())
    }

    fn expanded(mut self, trace: Backtrace) -> Result<Expanded> {
        self.hide_inside_agent();
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
            awaiting: Vec::new(),
        })
    }
}

fn polled_by(
    bodies: &[crate::FunctionId],
    chain: &futures::AwaitChain,
    runs: impl Fn(&futures::AsyncFrame, crate::FunctionId) -> bool,
) -> Option<VirtualAddress> {
    let mut coroutines = chain
        .frames
        .iter()
        .rev()
        .filter(|future| matches!(future.kind, AsyncFrameKind::Coroutine { .. }));
    let mut polled = None;
    for function in bodies {
        let future = coroutines.next()?;
        if !runs(future, *function) {
            return None;
        }

        polled = Some(future.object);
    }

    polled
}

fn awaiting_at(
    coroutine: &CoroutineInfo,
    line: Option<&crate::SourceLocation>,
) -> futures::StateChoice {
    let Some(line) = line else {
        return futures::StateChoice::Unknown;
    };

    let mut at = coroutine.states.iter().filter(|state| {
        matches!(state.kind, crate::CoroutineStateKind::Suspended { .. })
            && state
                .location
                .as_ref()
                .is_some_and(|location| location.file == line.file && location.line == line.line)
    });
    match (at.next(), at.next()) {
        (Some(state), None) => futures::StateChoice::Chosen(state.value),
        _ => futures::StateChoice::Unknown,
    }
}

fn bodies(
    frames: &[StackFrame],
    driver: usize,
    polls: impl Fn(&StackFrame) -> bool,
    runs: impl Fn(&StackFrame) -> Option<(crate::FunctionId, crate::TypeId)>,
) -> Vec<Body> {
    frames
        .get(..driver)
        .unwrap_or_default()
        .iter()
        .rev()
        .take_while(|frame| !polls(frame))
        .enumerate()
        .filter_map(|(outward, frame)| {
            let (function, coroutine) = runs(frame)?;
            Some(Body {
                function,
                coroutine,
                line: frame.source.clone(),
                frame: driver.checked_sub(outward + 1)?,
            })
        })
        .collect()
}

fn running_beside(
    module: &RuntimeModule,
    chain: &futures::AwaitChain,
    bodies: &[Body],
) -> Option<Vec<TypeReference>> {
    let coroutines: Vec<_> = chain
        .frames
        .iter()
        .rev()
        .filter(|future| matches!(future.kind, AsyncFrameKind::Coroutine { .. }))
        .collect();
    let agrees = coroutines.iter().zip(bodies).all(|(future, body)| {
        module
            .image
            .coroutine_functions(future.ty.id)
            .iter()
            .any(|candidate| candidate.id() == body.function)
    });

    agrees.then(|| {
        bodies
            .iter()
            .skip(coroutines.len())
            .map(|body| TypeReference {
                image: module.loaded.image,
                id: body.coroutine,
            })
            .collect()
    })
}

struct Body {
    function: crate::FunctionId,
    coroutine: crate::TypeId,
    line: Option<crate::SourceLocation>,

    frame: usize,
}

enum Polled {
    At(VirtualAddress),

    Typed(Option<TypeReference>),
}

fn awaiting(
    coroutine: &CoroutineInfo,
    polls: impl Fn(&crate::RecordMember) -> bool,
) -> futures::StateChoice {
    let mut at = coroutine.states.iter().filter(|state| {
        matches!(state.kind, crate::CoroutineStateKind::Suspended { .. })
            && state.awaitee().is_some_and(&polls)
    });
    match (at.next(), at.next()) {
        (Some(state), None) => futures::StateChoice::Chosen(state.value),
        _ => futures::StateChoice::Unknown,
    }
}

fn drivers<R>(
    frames: &[StackFrame],
    driven: impl Fn(&StackFrame, Option<&StackFrame>) -> Option<(Driven, R)>,
) -> Vec<(usize, Driven, R)> {
    let mut polling = false;
    let mut drivers = Vec::new();
    for (index, frame) in frames.iter().enumerate() {
        let inner = index.checked_sub(1).and_then(|inner| frames.get(inner));
        match driven(frame, inner) {
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

            let address = VirtualAddress::new(object.get().checked_add(offset)?);
            Some(SavedVariable {
                name: member.name.as_ref()?,
                ty: TypeReference {
                    image: ty.image,
                    id: member.type_ref.id,
                },
                address,
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

fn frame_u32(value: usize) -> Result<u32> {
    u32::try_from(value).map_err(|_| Error::internal("a stack has more frames than u32 levels"))
}

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

    fn chain(coroutines: &[(u64, u32)], leaf: bool) -> futures::AwaitChain {
        let reference = |id: u32| TypeReference {
            image: crate::ModuleImageId::new(0),
            id: crate::TypeId::new(id),
        };
        let mut frames = coroutines
            .iter()
            .map(|&(object, ty)| futures::AsyncFrame {
                object: VirtualAddress::new(object),
                ty: reference(ty),
                kind: AsyncFrameKind::Coroutine {
                    state: 3,
                    kind: crate::CoroutineStateKind::Suspended { index: 0 },
                    location: None,
                },
            })
            .collect::<Vec<_>>();
        frames.reverse();
        if leaf {
            frames.insert(
                0,
                futures::AsyncFrame {
                    object: VirtualAddress::new(0xdead),
                    ty: reference(99),
                    kind: AsyncFrameKind::Leaf,
                },
            );
        }
        futures::AwaitChain {
            frames,
            end: if leaf {
                ChainEnd::Leaf
            } else {
                ChainEnd::NoAwaitee
            },
        }
    }

    #[test]
    fn a_body_polls_the_future_at_its_place_in_the_driven_chain() {
        let id = crate::FunctionId::new;
        let runs = |future: &futures::AsyncFrame, function: crate::FunctionId| {
            future.ty.id.get() == function.get() + 10
        };
        let awaits = chain(&[(0x100, 11), (0x200, 12), (0x300, 13)], true);

        assert_eq!(
            polled_by(&[id(1), id(2), id(3)], &awaits, runs),
            Some(VirtualAddress::new(0x300))
        );
        assert_eq!(
            polled_by(&[id(1), id(2)], &awaits, runs),
            Some(VirtualAddress::new(0x200))
        );
        assert_eq!(
            polled_by(&[id(1)], &awaits, runs),
            Some(VirtualAddress::new(0x100))
        );

        assert_eq!(polled_by(&[id(1), id(5), id(3)], &awaits, runs), None);
        assert_eq!(polled_by(&[id(2), id(3)], &awaits, runs), None);

        assert_eq!(
            polled_by(&[id(1), id(2), id(3), id(4)], &awaits, runs),
            None
        );

        let recursive = chain(&[(0x100, 11), (0x200, 11)], false);
        assert_eq!(
            polled_by(&[id(1), id(1)], &recursive, runs),
            Some(VirtualAddress::new(0x200))
        );
        assert_eq!(
            polled_by(&[id(1)], &recursive, runs),
            Some(VirtualAddress::new(0x100))
        );

        assert_eq!(polled_by(&[], &awaits, runs), None);
    }

    #[test]
    fn a_running_coroutine_is_in_the_await_at_its_line() {
        let at = |file: u32, line: u64| crate::SourceLocation {
            file: crate::SourceFileId::new(file),
            line: crate::LineNumber::new(line).expect("a line"),
            column: None,
        };
        let state = |value, kind, location: Option<crate::SourceLocation>| crate::CoroutineState {
            value,
            kind,
            location,
            saved: Arc::new([]),
        };
        let suspended = |index| crate::CoroutineStateKind::Suspended { index };
        let coroutine = CoroutineInfo {
            kind: crate::CoroutineKind::AsyncFunction,
            state: crate::StateMember { offset: 0, size: 1 },
            states: Arc::new([
                state(0, crate::CoroutineStateKind::Unresumed, Some(at(0, 10))),
                state(3, suspended(0), Some(at(0, 12))),
                state(4, suspended(1), Some(at(0, 14))),
                state(5, suspended(2), Some(at(0, 14))),
                state(6, suspended(3), Some(at(1, 12))),
            ]),
            captures: Arc::new([]),
        };

        let choice = |line: Option<crate::SourceLocation>| awaiting_at(&coroutine, line.as_ref());
        assert_eq!(choice(Some(at(0, 12))), futures::StateChoice::Chosen(3));
        assert_eq!(choice(Some(at(1, 12))), futures::StateChoice::Chosen(6));
        assert_eq!(choice(Some(at(0, 10))), futures::StateChoice::Unknown);
        assert_eq!(choice(Some(at(0, 14))), futures::StateChoice::Unknown);
        assert_eq!(choice(Some(at(0, 99))), futures::StateChoice::Unknown);
        assert_eq!(choice(None), futures::StateChoice::Unknown);
    }

    #[test]
    fn drivers_are_the_frames_a_model_says_drive_and_no_future_runs_within() {
        let future = Driven::Future { variable: "future" };
        let driven = |frame: &StackFrame, _: Option<&StackFrame>| {
            (frame.role == CodeRole::RuntimeInternal).then_some((future, ()))
        };
        let frames = [
            frame(0, CodeRole::Ordinary, false),
            frame(1, CodeRole::RuntimeInternal, false),
            frame(2, CodeRole::Ordinary, true),
            frame(3, CodeRole::RuntimeInternal, false),
        ];
        assert_eq!(drivers(&frames, driven), [(1, future, ())]);
        assert_eq!(drivers(&frames[3..], driven), [(0, future, ())]);
        assert!(drivers(&frames, |_, _| None::<(Driven, ())>).is_empty());

        let interpreted = |frame: &StackFrame, _: Option<&StackFrame>| {
            (frame.role != CodeRole::Ordinary).then_some((Driven::Interpreted, ()))
        };
        assert_eq!(
            drivers(&frames, interpreted),
            [(1, Driven::Interpreted, ()), (3, Driven::Interpreted, ())]
        );
        let calling = |frame: &StackFrame, inner: Option<&StackFrame>| {
            (frame.role != CodeRole::Ordinary && inner.is_some_and(|inner| inner.level == 2))
                .then_some((Driven::Interpreted, ()))
        };
        assert_eq!(drivers(&frames, calling), [(3, Driven::Interpreted, ())]);
    }

    #[test]
    fn frames_inside_the_agent_are_machinery() {
        let interpreted = |level, role| {
            let mut frame = frame(level, role, false);
            frame.kind = FrameKind::Interpreted {
                frame: VirtualAddress::new(0x1000),
            };
            frame
        };
        let roles = |frames: Vec<StackFrame>| {
            let mut spliced = Spliced {
                frames,
                origins: Vec::new(),
                futures: Vec::new(),
                interpreted: Vec::new(),
                unfollowed: Vec::new(),
            };
            spliced.hide_inside_agent();
            spliced
                .frames
                .iter()
                .map(|frame| frame.role)
                .collect::<Vec<_>>()
        };
        assert_eq!(
            roles(vec![
                frame(0, CodeRole::Ordinary, false),
                frame(1, CodeRole::Wrapper, false),
                frame(2, CodeRole::RuntimeInternal, false),
                interpreted(3, CodeRole::Interpreter),
                frame(4, CodeRole::Interpreter, false),
                interpreted(5, CodeRole::Ordinary),
                interpreted(6, CodeRole::Interpreter),
            ]),
            [
                CodeRole::Interpreter,
                CodeRole::Interpreter,
                CodeRole::RuntimeInternal,
                CodeRole::Interpreter,
                CodeRole::Interpreter,
                CodeRole::Ordinary,
                CodeRole::Interpreter,
            ]
        );

        let unchanged = vec![
            frame(0, CodeRole::Ordinary, false),
            frame(1, CodeRole::Interpreter, false),
            interpreted(2, CodeRole::Ordinary),
        ];
        assert_eq!(
            roles(unchanged),
            [
                CodeRole::Ordinary,
                CodeRole::Interpreter,
                CodeRole::Ordinary
            ]
        );
    }
}
