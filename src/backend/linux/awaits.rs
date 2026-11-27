//! Async-aware stepping that follows a Rust future across `.await` suspension
//! points, task rescheduling, inlined coroutine bodies, and future drops.

use crate::backend::linux::Registers;
use std::collections::BTreeSet;

use nix::unistd::Pid;

use crate::protocol::{
    ExceptionDisposition, ExecutionId, ProcessId, ResumeScope, StepKind, StopId, StopReason,
    TaskEnding,
};

use crate::runtime_model::futures::{self, AsyncFrameKind};
use crate::runtime_model::{TaskEnd, TaskEntries};
use crate::unwind::DEFAULT_MAX_FRAMES;
use crate::{
    CodeInstanceId, CodeInstanceKind, CoroutineStateKind, Error, ExecutionContext, FunctionId,
    InlineFrameLookup, Result, SourceLocation, StackFrameId, TypeReference, VirtualAddress,
};

use super::frames::{FrameScope, ResolvedFrame, StackRoot};
use super::native::LinuxTraceOps;
use super::{ActiveKind, Controller, StepOwner, StepStart};
use crate::PresentedFrame;

#[derive(Debug, Clone, PartialEq, Eq)]
pub(super) struct AwaitStep {
    future: RunningFuture,

    inlined: Option<Inlined>,

    source: Option<SourceLocation>,

    waiting: BTreeSet<VirtualAddress>,

    drop_glue: BTreeSet<VirtualAddress>,

    inlined_drops: BTreeSet<VirtualAddress>,

    task_entries: Option<(crate::TaskId, TaskEntries)>,

    task_return: Option<TaskReturn>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
struct TaskReturn {
    address: VirtualAddress,
    stack_pointer: u64,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub(super) struct Inlined {
    poll: RunningFuture,

    own: bool,

    function: FunctionId,

    instances: BTreeSet<CodeInstanceId>,

    statements: BTreeSet<VirtualAddress>,
}

impl AwaitStep {
    pub(super) fn waits(&self) -> bool {
        !self.waiting.is_empty()
    }

    fn watched(&self) -> impl Iterator<Item = VirtualAddress> + '_ {
        let entries = self
            .task_entries
            .iter()
            .flat_map(|(_, entries)| entries.runs.iter().chain(&entries.frees).copied());
        self.waiting
            .iter()
            .copied()
            .chain(self.drop_glue.iter().copied())
            .chain(self.inlined_drops.iter().copied())
            .chain(entries)
            .chain(self.task_return.map(|task_return| task_return.address))
    }

    fn takes_up_task(&self, address: VirtualAddress) -> bool {
        self.task_return
            .is_some_and(|task_return| task_return.address == address)
            || self.task_entries.as_ref().is_some_and(|(_, entries)| {
                entries.runs.contains(&address) || entries.frees.contains(&address)
            })
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(super) struct RunningFuture {
    object: VirtualAddress,
    ty: TypeReference,
}

impl<P: LinuxTraceOps> Controller<P> {
    pub(super) fn await_step(
        &self,
        pid: Pid,
        kind: StepKind,
        start: &StepStart,
    ) -> Option<AwaitStep> {
        if !matches!(kind, StepKind::OverSource | StepKind::Out)
            || !self.steps_in_main_module(start)
        {
            return None;
        }

        let inline = start
            .code_instance
            .and_then(|instance| self.main.image.code_instance(instance))
            .is_some_and(|instance| matches!(instance.kind(), CodeInstanceKind::Inline { .. }));
        let (future, inlined) = if inline {
            self.inlined_body(pid, start)?
        } else {
            (self.running_future(pid, start.code_instance)?, None)
        };

        record!(
            "the step follows the future at {}{}",
            future.object,
            if inlined.is_some() {
                ", in an inlined body"
            } else {
                ""
            }
        );
        Some(AwaitStep {
            future,
            inlined,
            source: start.source.clone(),
            waiting: BTreeSet::new(),
            drop_glue: BTreeSet::new(),
            inlined_drops: BTreeSet::new(),
            task_entries: None,
            task_return: None,
        })
    }

    fn inlined_body(
        &self,
        pid: Pid,
        start: &StepStart,
    ) -> Option<(RunningFuture, Option<Inlined>)> {
        let instance = start.code_instance?;
        let info = self.main.image.code_instance(instance)?;
        let function = info.function();
        if !matches!(info.kind(), CodeInstanceKind::Inline { .. })
            || self.main.image.function(function)?.coroutine().is_none()
        {
            return None;
        }

        let own = self.running_future(pid, Some(instance));
        let poll = self.running_future(pid, None).or(own)?;
        let future = own.unwrap_or(poll);
        let registers = self.ptrace.registers(pid).ok()?;
        let location = self.image_location(registers.program_counter())?;
        let InlineFrameLookup::Unique(chain) = &location.inline_frames else {
            return None;
        };

        let depth = chain.instances.iter().position(|id| *id == instance)?;

        let mut statements = self.body_statements(instance).ok()?;
        let awaiters = chain
            .instances
            .iter()
            .take(depth)
            .copied()
            .chain(location.physical_instance)
            .filter(|outer| {
                self.main
                    .image
                    .code_instance(*outer)
                    .and_then(|outer| self.main.image.function(outer.function()))
                    .is_some_and(|function| function.coroutine().is_some())
            })
            .collect::<Vec<_>>();

        for outer in awaiters {
            let line = super::frames::source_for_code_instance(&self.main.image, &location, outer)?;
            statements.extend(self.other_lines(outer, &line).ok()?);
        }

        Some((
            future,
            Some(Inlined {
                poll,
                own: own.is_some(),
                function,
                instances: BTreeSet::from([instance]),
                statements: self.without_resume_code(statements),
            }),
        ))
    }

    pub(super) fn left_inlined_body_pending(
        &self,
        pid: Pid,
        start: &StepStart,
        address: VirtualAddress,
    ) -> bool {
        let Some((awaiting, inlined)) = start
            .awaiting
            .as_ref()
            .and_then(|awaiting| Some((awaiting, awaiting.inlined.as_ref()?)))
        else {
            return false;
        };
        !inlined.statements.contains(&address)
            && (!inlined.own
                || !matches!(
                    self.future_state(pid, awaiting.future),
                    Some((_, CoroutineStateKind::Returned))
                ))
    }

    pub(super) fn body_statements(
        &self,
        instance: CodeInstanceId,
    ) -> Result<BTreeSet<VirtualAddress>> {
        let header = self
            .main
            .image
            .code_instance(instance)
            .and_then(|instance| self.main.image.function(instance.function()))
            .and_then(crate::image::functions::Function::declaration);
        self.statements_of(instance, header.as_ref())
    }

    pub(super) fn without_resume_code(
        &self,
        statements: BTreeSet<VirtualAddress>,
    ) -> BTreeSet<VirtualAddress> {
        let Some(inferior) = self.session.inferior() else {
            return statements;
        };

        statements
            .into_iter()
            .filter(|address| {
                inferior
                    .loaded_module
                    .image_address(*address)
                    .is_ok_and(|address| !self.main.image.is_resume_code(address))
            })
            .collect()
    }

    fn awaits_twice(&self, pid: Pid, future: RunningFuture, function: FunctionId) -> bool {
        let Some(inferior) = self.session.inferior() else {
            return false;
        };

        let Some(module) = self.module_of(future.ty) else {
            return false;
        };

        let chain = self.with_module_stop(inferior, &module.loaded, pid, |stop| {
            futures::walk(module.image.as_ref(), stop, future.object, future.ty)
        });
        chain
            .frames
            .iter()
            .filter(|frame| matches!(frame.kind, AsyncFrameKind::Coroutine { .. }))
            .filter(|frame| {
                module
                    .image
                    .coroutine_functions(frame.ty.id)
                    .iter()
                    .any(|candidate| candidate.id() == function)
            })
            .count()
            > 1
    }

    pub(super) fn step_suspended(
        &mut self,
        (process_id, stop_id): (ProcessId, StopId),
        task: crate::TaskId,
        frame: StackFrameId,
        kind: StepKind,
        scope: ResumeScope,
        exception: ExceptionDisposition,
    ) -> Result<ExecutionId> {
        let parked = || Error::TaskParked(task);
        if !matches!(
            kind,
            StepKind::IntoSource | StepKind::OverSource | StepKind::Out
        ) || matches!(scope, ResumeScope::Thread(_))
        {
            return Err(parked());
        }

        let inferior = self.session.inferior().ok_or(Error::NotRunning)?;
        let root = self.stack_root(stop_id, ExecutionContext::Task(task))?;
        let super::frames::RootOrigin::Suspended { reader, .. } = root.origin else {
            return Err(parked());
        };

        let stack = self.async_stack(inferior, &root)?.ok_or_else(parked)?;
        let level = frame.get() as usize;
        let (selected, selected_future) = stack
            .futures
            .iter()
            .enumerate()
            .skip(level)
            .find(|(_, future)| matches!(future.kind, AsyncFrameKind::Coroutine { .. }))
            .ok_or_else(|| {
                Error::FrameStepUnsupported("the task's frames hold no async function".into())
            })?;
        if selected_future.ty.image != inferior.loaded_module.image {
            return Err(Error::FrameStepUnsupported(
                "the async function is in a library, whose awaits steps do not follow".into(),
            ));
        }

        let own = self.named_by_body(selected_future);
        let source = stack
            .frames
            .get(selected)
            .ok_or_else(|| Error::internal("an async stack has a frame for each future"))?
            .source
            .clone();

        let inline = self.suspended_inline_body(&stack, selected);
        let (future, inlined, waiting) = match (self.resume_point(reader, own), inline) {
            (Some(resumes), Ok((future, inlined, mut statements))) => {
                statements.insert(resumes);
                (future, inlined, statements)
            }
            (Some(resumes), Err(_)) => (own, None, BTreeSet::from([resumes])),
            (None, inline) => inline?,
        };

        let (drop_glue, inlined_drops) = self.drop_glue(future.ty);
        let task_entries = self.task_entries(reader, task);
        record!(
            "a step of task {task} waits for the future at {} at {waiting:?}",
            future.object
        );
        let requested = kind;
        let kind = match kind {
            StepKind::IntoSource => StepKind::OverSource,
            kind => kind,
        };

        let awaiting = AwaitStep {
            future,
            inlined,
            source,
            waiting,
            drop_glue,
            inlined_drops,
            task_entries,
            task_return: None,
        };

        let start = StepStart {
            plan_addresses: awaiting.watched().collect(),
            awaiting: Some(awaiting),
            ..StepStart::default()
        };

        self.begin_execution(
            process_id,
            stop_id,
            scope,
            ActiveKind::Step {
                owner: StepOwner {
                    thread: reader,
                    task: Some(task),
                },
                kind,
                requested,
                start: Box::new(start),
                progress_owed: false,
            },
            exception,
        )
    }

    fn named_by_body(&self, future: &futures::AsyncFrame) -> RunningFuture {
        let ty = self
            .main
            .image
            .coroutine_functions(future.ty.id)
            .first()
            .and_then(|function| function.coroutine())
            .map_or(future.ty, |id| TypeReference {
                image: future.ty.image,
                id,
            });
        RunningFuture {
            object: future.object,
            ty,
        }
    }

    fn suspended_inline_body(
        &self,
        stack: &super::async_frames::AsyncStack,
        selected: usize,
    ) -> Result<(RunningFuture, Option<Inlined>, BTreeSet<VirtualAddress>)> {
        let unknown = || {
            Error::FrameStepUnsupported(
                "the async function waits at no await whose resumption is known".into(),
            )
        };

        let functions_of = |future: &futures::AsyncFrame| {
            self.main
                .image
                .coroutine_functions(future.ty.id)
                .iter()
                .map(|function| function.id())
                .collect::<Vec<_>>()
        };

        let out_of_line = |functions: &[FunctionId]| {
            functions
                .iter()
                .flat_map(|function| self.main.image.instances_for_function(*function))
                .filter(|instance| matches!(instance.kind(), CodeInstanceKind::OutOfLine))
                .collect::<Vec<_>>()
        };

        let selected_future = stack
            .futures
            .get(selected)
            .ok_or_else(|| Error::internal("a selected async frame has a future"))?;
        let (running, running_future, physical) = stack
            .futures
            .iter()
            .enumerate()
            .skip(selected + 1)
            .filter(|(_, future)| matches!(future.kind, AsyncFrameKind::Coroutine { .. }))
            .map(|(index, future)| (index, future, out_of_line(&functions_of(future))))
            .find(|(_, _, physical)| !physical.is_empty())
            .ok_or_else(unknown)?;
        let inlined_in = |functions: &[FunctionId]| {
            functions
                .iter()
                .flat_map(|function| self.main.image.instances_for_function(*function))
                .filter(|instance| {
                    matches!(instance.kind(), CodeInstanceKind::Inline { .. })
                        && instance.ranges().next().is_some_and(|range| {
                            physical
                                .iter()
                                .any(|physical| physical.contains(range.start))
                        })
                })
                .map(crate::image::functions::CodeInstance::id)
                .collect::<BTreeSet<_>>()
        };

        let selected_functions = functions_of(selected_future);
        let function = *selected_functions.first().ok_or_else(unknown)?;
        let instances = inlined_in(&selected_functions);
        let mut statements = BTreeSet::new();

        for instance in &instances {
            statements.extend(self.body_statements(*instance)?);
        }

        if stack.frames.len() <= running {
            return Err(Error::internal(
                "an async stack has a frame for each future",
            ));
        }

        let outward = stack
            .frames
            .iter()
            .zip(&stack.futures)
            .enumerate()
            .take(running + 1)
            .skip(selected + 1);
        for (index, (frame, future)) in outward {
            let Some(line) = &frame.source else {
                continue;
            };

            let outers = if index == running {
                physical.iter().map(|instance| instance.id()).collect()
            } else {
                inlined_in(&functions_of(future))
            };

            for instance in outers {
                statements.extend(self.other_lines(instance, line)?);
            }
        }

        let statements = self.without_resume_code(statements);
        if instances.is_empty() || statements.is_empty() {
            return Err(unknown());
        }

        Ok((
            self.named_by_body(selected_future),
            Some(Inlined {
                poll: self.named_by_body(running_future),
                own: true,
                function,
                instances,
                statements: statements.clone(),
            }),
            statements,
        ))
    }

    fn running_future(&self, pid: Pid, selected: Option<CodeInstanceId>) -> Option<RunningFuture> {
        let inferior = self.session.inferior()?;
        let module = self.modules.get(&inferior.loaded_module.id)?;
        let root = StackRoot::of_thread(pid);
        let stack = self.physical_stack(inferior, &root, 1).ok()?;
        let innermost = stack.frames.first()?;
        let address = inferior
            .loaded_module
            .image_address(innermost.context.instruction)
            .ok()?;
        let physical = self.main.image.locate(address).physical_instance?;
        let instance = selected.unwrap_or(physical);
        let coroutine = self
            .main
            .image
            .code_instance(instance)
            .and_then(|instance| self.main.image.function(instance.function()))?
            .coroutine()?;
        let code = Some((inferior.loaded_module.id, address));
        let modules = self.unwind_modules(inferior);
        let resolved = ResolvedFrame {
            id: crate::StackFrameId::new(0),
            presented: PresentedFrame::Physical,
            frame: None,
            code,
            scope: FrameScope::Function,
            registers: stack.registers(0)?,
            cfa: self.frame_cfa(pid, &modules, code, &innermost.registers),
            activation: 0,
            below_stack_pointer: self.below_stack_pointer(inferior, &root, innermost),
        };

        let coroutine_type = TypeReference {
            image: module.loaded.image,
            id: coroutine,
        };
        let selected = (instance != physical).then_some(instance);
        let mut runtime = self.frame_runtime(inferior, &root, &resolved, module);
        let mut budget =
            crate::inspection::InspectionBudget::new(crate::InspectionLimits::default());
        let Ok(key) = module
            .variables
            .visible_object(address, selected, "$future")
        else {
            return module
                .variables
                .captured_future(address, selected, &mut runtime, &mut budget)
                .or_else(|| self.polled_future(pid, coroutine_type, None))
                .map(|object| RunningFuture {
                    object,
                    ty: coroutine_type,
                });
        };
        let located = module
            .variables
            .locate(key, Some(address), &mut runtime, &mut budget)
            .ok()?
            .ok()?;
        let crate::model::ValueStorage::Memory(object) = located.storage else {
            return None;
        };

        (located.ty == coroutine).then_some(RunningFuture {
            object,
            ty: TypeReference {
                image: module.loaded.image,
                id: coroutine,
            },
        })
    }

    fn future_state(&self, pid: Pid, future: RunningFuture) -> Option<(u64, CoroutineStateKind)> {
        let inferior = self.session.inferior()?;
        let module = self.module_of(future.ty)?;

        let chain = self.with_module_stop(inferior, &module.loaded, pid, |stop| {
            futures::walk(module.image.as_ref(), stop, future.object, future.ty)
        });
        match chain.frames.last()?.kind {
            AsyncFrameKind::Coroutine { state, kind, .. } => Some((state, kind)),
            AsyncFrameKind::Leaf => None,
        }
    }

    fn resume_point(&self, pid: Pid, future: RunningFuture) -> Option<VirtualAddress> {
        let (state, CoroutineStateKind::Suspended { .. }) = self.future_state(pid, future)? else {
            return None;
        };

        let module = self.module_of(future.ty)?;
        let functions = module.image.coroutine_functions(future.ty.id);
        let address = super::async_frames::resume_address(&module.image, &functions, state)?;
        module.loaded.virtual_address(address).ok()
    }

    fn waits_at(
        &self,
        pid: Pid,
        kind: StepKind,
        awaiting: &AwaitStep,
    ) -> std::result::Result<BTreeSet<VirtualAddress>, StopReason> {
        let future = awaiting.future;
        let incomplete = |description: &str| StopReason::StepIncomplete {
            kind,
            description: description.into(),
        };

        let state = self.future_state(pid, future).map(|(_, state)| state);
        match (state, &awaiting.inlined) {
            (Some(CoroutineStateKind::Returned), _) => Ok(BTreeSet::new()),
            (Some(CoroutineStateKind::Suspended { .. }), None) => self
                .resume_point(pid, future)
                .map(|resumes| BTreeSet::from([resumes]))
                .ok_or_else(|| incomplete(UNKNOWN_RESUMPTION)),
            (Some(CoroutineStateKind::Suspended { .. }), Some(inlined)) => {
                if !inlined.own && self.awaits_twice(pid, future, inlined.function) {
                    return Err(incomplete(TWICE));
                }

                Ok(inlined.statements.clone())
            }
            (Some(CoroutineStateKind::Unresumed), _) => Err(incomplete(
                "the future the step follows is unresumed after its poll returned",
            )),
            (Some(CoroutineStateKind::Panicked), _) => {
                Err(incomplete("the future the step follows panicked"))
            }
            (None, _) => Err(incomplete(
                "the state of the future the step follows is unreadable",
            )),
        }
    }

    /// Handles a future's `poll` return: arms waiting, drop-glue, and task lifecycle breakpoints on
    /// `Pending`, or ends the step with `StopReason::TaskEnded` on `Ready`.
    pub(super) fn follow_poll_return(
        &mut self,
        pid: Pid,
        kind: StepKind,
    ) -> Result<Option<Followed>> {
        let Some((execution, task, awaiting, activation)) = self
            .session
            .inferior()
            .and_then(|inferior| inferior.run.execution())
            .and_then(|active| match &active.kind {
                ActiveKind::Step { owner, start, .. } => {
                    let awaiting = start.awaiting.as_ref().filter(|step| !step.waits())?;
                    if !self.runs_step(*owner, pid) {
                        record!(
                            "thread {pid} runs no task of the step that follows a future, \
                             {owner:?}: {:?}",
                            self.session
                                .inferior()
                                .and_then(|inferior| self.thread_activity(inferior, pid))
                        );
                        return None;
                    }

                    Some((active.id, owner.task, awaiting.clone(), start.activation?))
                }
                _ => None,
            })
        else {
            return Ok(None);
        };

        let future = awaiting.future;
        let registers = self.ptrace.registers(pid)?;
        if !activation.has_returned(self.stack_position(pid, &registers)) {
            record!("the poll of the future at {} runs on", future.object);
            return Ok(None);
        }

        let waiting = match self.waits_at(pid, kind, &awaiting) {
            Ok(waiting) => waiting,
            Err(reason) => return Ok(Some(Followed::Ended(reason))),
        };

        if waiting.is_empty() {
            let ended = task.filter(|_| self.returned_to_runtime(pid, &registers));
            return Ok(ended.map(|task| {
                record!("the step's task {task} finished");
                Followed::Ended(StopReason::TaskEnded {
                    kind,
                    task,
                    ending: TaskEnding::Finished,
                })
            }));
        }

        record!(
            "the poll of the future at {} returned pending; the step waits at {waiting:?}",
            future.object
        );
        let (drop_glue, inlined_drops) = self.drop_glue(future.ty);
        let inlined_drops = if task.is_some() {
            inlined_drops
        } else {
            BTreeSet::new()
        };

        let task_entries = task.and_then(|task| self.task_entries(pid, task));

        let task_return = task_entries
            .as_ref()
            .filter(|(_, entries)| !entries.frees.is_empty())
            .and_then(|_| self.dispatch_return(pid));

        let start = self.active_step_mut().ok_or_else(|| {
            Error::internal("the step remained active while its future was pending")
        })?;
        let mut awaiting = start
            .awaiting
            .take()
            .ok_or_else(|| Error::internal("the step follows a future"))?;
        awaiting.waiting = waiting;
        awaiting.drop_glue = drop_glue;
        awaiting.inlined_drops = inlined_drops;
        awaiting.task_entries = task_entries;
        awaiting.task_return = task_return;

        let waits = StepStart {
            plan_addresses: awaiting.watched().collect(),
            awaiting: Some(awaiting),
            targets: std::mem::take(&mut start.targets),
            new_task: start.new_task.take(),
            ..StepStart::default()
        };

        self.cleanup_plan_breakpoints(execution)?;
        self.install_additional_plan_breakpoints(execution, &waits.plan_sites())?;

        *self.active_step_mut().ok_or_else(|| {
            Error::internal("the step remained active while its future was pending")
        })? = waits;
        Ok(Some(Followed::Waits))
    }

    fn task_entries(
        &self,
        reader: Pid,
        task: crate::TaskId,
    ) -> Option<(crate::TaskId, TaskEntries)> {
        let inferior = self.session.inferior()?;
        let runtime = self
            .runtimes(inferior)
            .into_iter()
            .find(|runtime| runtime.id == task.runtime)?;
        let entries = self.with_runtime_stop(inferior, &runtime, reader, |stop| {
            runtime
                .model
                .task_entries(stop, Self::task_ref(inferior, task))
        });

        #[cfg(debug_assertions)]
        if let Err(reason) = &entries {
            record!("task {task}'s entries are unreadable: {reason}");
        }

        entries.ok().flatten().map(|entries| (task, entries))
    }

    fn dispatch_return(&self, pid: Pid) -> Option<TaskReturn> {
        let inferior = self.session.inferior()?;
        let stack = self
            .physical_stack(inferior, &StackRoot::of_thread(pid), DEFAULT_MAX_FRAMES)
            .ok()?;
        let level = (0..stack.frames.len()).find(|&level| {
            stack
                .lookup_address(level)
                .and_then(|address| self.code_role(address))
                == Some(crate::CodeRole::Dispatch)
        })?;
        let caller = stack.frames.get(level + 1)?;
        Some(TaskReturn {
            address: caller.context.instruction,
            stack_pointer: caller.registers.get(crate::unwind::STACK_POINTER)?,
        })
    }

    fn task_code_reached(
        &mut self,
        pid: Pid,
        address: VirtualAddress,
        kind: StepKind,
        (task, entries): (crate::TaskId, TaskEntries),
        task_return: Option<TaskReturn>,
    ) -> Result<()> {
        let registers = self.ptrace.registers(pid)?;
        let returned = task_return.is_some_and(|task_return| {
            task_return.address == address && task_return.stack_pointer == registers.stack_pointer()
        });
        let entered = !returned
            && self.with_task_runtime(pid, task, |runtime, stop| {
                Ok(runtime.takes_up(stop, &entries, address, &registers.register_file()))
            }) == Ok(true);

        let ends = returned || entered && entries.frees.contains(&address);
        if ends {
            let end =
                self.with_task_runtime(pid, task, |runtime, stop| runtime.task_end(stop, &entries));
            let reason = match end {
                Ok(Some(end)) => {
                    record!("thread {pid} finds the step's task {task} ended: {end:?}");
                    Some(StopReason::TaskEnded {
                        kind,
                        task,
                        ending: ending(end),
                    })
                }
                Ok(None) if returned => None,
                Ok(None) => Some(StopReason::StepIncomplete {
                    kind,
                    description: "the step's task was freed before it ended".into(),
                }),
                Err(reason) => Some(StopReason::StepIncomplete {
                    kind,
                    description: format!("whether the step's task ended is unknown: {reason}")
                        .into(),
                }),
            };

            if let Some(reason) = reason {
                self.follow_step(pid);
                return self.begin_visible_stop(pid, reason);
            }
        }

        if entered && entries.runs.contains(&address) && !entries.frees.is_empty() {
            let pending =
                registers.pending_return(|address| self.ptrace.read_word(pid, address))?;
            self.watch_task_return(TaskReturn {
                address: pending.returns_to,
                stack_pointer: pending.stack,
            })?;
        }

        self.pass_by(pid, address)
    }

    fn pass_by(&mut self, pid: Pid, address: VirtualAddress) -> Result<()> {
        if self.barrier_active() {
            self.finish_barrier_if_ready()
        } else {
            self.repair_when_alone(pid, address)
        }
    }

    fn watch_task_return(&mut self, task_return: TaskReturn) -> Result<()> {
        let execution = self.active_execution()?;
        let start = self
            .active_step_mut()
            .ok_or_else(|| Error::internal("the step waits for its task"))?;
        let Some(awaiting) = start.awaiting.as_mut() else {
            return Ok(());
        };

        awaiting.task_return = Some(task_return);
        start.plan_addresses.insert(task_return.address);
        self.install_additional_plan_breakpoints(execution, &BTreeSet::from([task_return.address]))
    }

    fn with_task_runtime<T>(
        &self,
        pid: Pid,
        task: crate::TaskId,
        read: impl FnOnce(
            &dyn crate::runtime_model::RuntimeModel,
            &dyn crate::runtime_model::RuntimeStop,
        ) -> std::result::Result<T, std::sync::Arc<str>>,
    ) -> std::result::Result<T, std::sync::Arc<str>> {
        let inferior = self.session.inferior().ok_or("the process has ended")?;
        let runtime = self
            .runtimes(inferior)
            .into_iter()
            .find(|runtime| runtime.id == task.runtime)
            .ok_or("the task's runtime is no longer loaded")?;
        self.with_runtime_stop(inferior, &runtime, pid, |stop| {
            read(runtime.model.as_ref(), stop)
        })
    }

    fn drop_glue(&self, ty: TypeReference) -> (BTreeSet<VirtualAddress>, BTreeSet<VirtualAddress>) {
        let mut named = BTreeSet::new();
        let mut inlined = BTreeSet::new();
        let Some(module) = self.module_of(ty) else {
            return (named, inlined);
        };

        let Some(identity) = module
            .image
            .type_info(ty)
            .and_then(|info| Some((info.identity.as_deref()?.path.clone(), info.name.clone())))
        else {
            return (named, inlined);
        };

        let mut ty_name = String::new();
        for segment in identity.0.iter() {
            ty_name.push_str(segment);
            ty_name.push_str("::");
        }

        ty_name.push_str(&identity.1);
        let entry = |instance: crate::CodeInstance<'_>| {
            instance
                .breakpoint_entry()
                .map(|entry| entry.address)
                .or_else(|| instance.ranges().map(|range| range.start).min())
                .and_then(|address| module.loaded.virtual_address(address).ok())
        };

        for function in crate::rust_names::drop_glue_of(&ty_name)
            .iter()
            .flat_map(|name| module.image.functions_called(name))
        {
            for instance in module.image.instances_for_function(function.id()) {
                let copies = if matches!(instance.kind(), crate::CodeInstanceKind::OutOfLine) {
                    &mut named
                } else {
                    &mut inlined
                };

                copies.extend(entry(instance));
            }
        }

        (named, inlined)
    }

    fn future_dropped(
        &mut self,
        pid: Pid,
        address: VirtualAddress,
        kind: StepKind,
        future: RunningFuture,
    ) -> Result<()> {
        record!("thread {pid} drops the future the step waits for");
        self.follow_step(pid);
        let entries = self
            .active_step()
            .and_then(|start| start.awaiting.as_ref()?.task_entries.clone());
        let inferior = self.session.inferior().ok_or(Error::NotRunning)?;
        let stack =
            self.physical_stack(inferior, &StackRoot::of_thread(pid), DEFAULT_MAX_FRAMES)?;

        let physical = |level: usize| {
            let location = self.image_location(stack.lookup_address(level)?)?;
            let instance = self.main.image.code_instance(location.physical_instance?)?;
            self.main.image.function(instance.function())
        };

        let dropper = (0..stack.frames.len())
            .find(|&level| {
                physical(level)
                    .is_none_or(|function| !crate::rust_names::is_drop_glue(function.name()))
            })
            .filter(|&level| {
                physical(level).is_some_and(|function| function.role() == crate::CodeRole::Ordinary)
            });
        let Some(dropper) = dropper else {
            let ended = entries.and_then(|(task, entries)| {
                let end = self
                    .with_task_runtime(pid, task, |runtime, stop| runtime.task_end(stop, &entries));
                Some((task, end.ok()??))
            });

            let reason = match ended {
                Some((task, end)) => StopReason::TaskEnded {
                    kind,
                    task,
                    ending: ending(end),
                },
                None => StopReason::StepIncomplete {
                    kind,
                    description: "the future the step waited for was dropped by code the step \
                                  cannot follow"
                        .into(),
                },
            };

            return self.begin_visible_stop(pid, reason);
        };

        #[cfg(debug_assertions)]
        record!(
            "frame {dropper} drops it, in {}",
            stack
                .lookup_address(dropper)
                .and_then(|lookup| self.image_location(lookup))
                .and_then(|location| location.function)
                .map_or_else(|| "unnamed code".into(), |function| function.name)
        );

        let Some(start) = self.step_on_in_frame(pid, &stack, dropper, future)? else {
            return self.begin_visible_stop(
                pid,
                StopReason::StepIncomplete {
                    kind,
                    description: "the future the step waited for was dropped by code the step \
                                  cannot follow"
                        .into(),
                },
            );
        };

        let execution = self.active_execution()?;
        self.cleanup_plan_breakpoints(execution)?;
        self.install_additional_plan_breakpoints(
            execution,
            &start
                .plan_addresses
                .union(&start.panic_guards)
                .copied()
                .collect(),
        )?;

        if let Some(ActiveKind::Step {
            kind: running,
            start: active,
            ..
        }) = self
            .session
            .inferior_mut()
            .and_then(|inferior| inferior.run.execution_mut())
            .map(|active| &mut active.kind)
        {
            *running = StepKind::OverSource;
            **active = start;
        }

        self.go_on_without_plan(pid, address, None)
    }

    fn step_on_in_frame(
        &self,
        pid: Pid,
        stack: &super::frames::PhysicalStack,
        level: usize,
        dropped: RunningFuture,
    ) -> Result<Option<StepStart>> {
        let inferior = self.session.inferior().ok_or(Error::NotRunning)?;
        let (Some(frame), Some(lookup)) = (stack.frames.get(level), stack.lookup_address(level))
        else {
            return Ok(None);
        };

        let Some(location) = self.image_location(lookup) else {
            return Ok(None);
        };

        let Some(instance) = location.physical_instance else {
            return Ok(None);
        };

        let Some(source) =
            super::frames::source_for_code_instance(&self.main.image, &location, instance)
        else {
            return Ok(None);
        };

        let modules = self.unwind_modules(inferior);
        let code = inferior
            .loaded_module
            .image_address(lookup)
            .ok()
            .map(|address| (inferior.loaded_module.id, address));
        let Ok(cfa) = self.frame_cfa(pid, &modules, code, &frame.registers) else {
            return Ok(None);
        };

        let mut plan = self.other_lines(instance, &source)?;
        if let Some(caller) = stack.frames.get(level + 1) {
            plan.insert(self.executable_return_address(pid, caller.context.instruction)?);
        }

        let registers = self.ptrace.registers(pid)?;
        Ok(Some(StepStart {
            source: Some(source),
            code_instance: Some(instance),
            physical_instance: Some(instance),
            activation: Some(self.stack_view(pid).activation(cfa)),
            stack_pointer: Some(self.stack_position(pid, &registers)),
            plan_addresses: plan,
            panic_guards: self.panic_entries(inferior),
            dropped: Some(dropped),
            ..StepStart::default()
        }))
    }

    pub(super) fn dropped_step_reason(&self, reason: StopReason) -> StopReason {
        let dropped = self
            .session
            .inferior()
            .and_then(|inferior| inferior.run.execution())
            .is_some_and(
                |active| matches!(&active.kind, ActiveKind::Step { start, .. } if start.dropped.is_some()),
            );
        match reason {
            StopReason::Step { kind } if dropped => StopReason::FutureDropped { kind },
            reason => reason,
        }
    }

    fn returned_to_runtime(&self, pid: Pid, registers: &Registers) -> bool {
        if !self
            .code_role(registers.program_counter())
            .is_some_and(super::stepping::is_runtime_role)
        {
            return false;
        }

        let Some(inferior) = self.session.inferior() else {
            return false;
        };

        let Ok(stack) =
            self.physical_stack(inferior, &StackRoot::of_thread(pid), DEFAULT_MAX_FRAMES)
        else {
            return false;
        };

        let dispatch = (0..stack.frames.len())
            .find(|&level| {
                stack
                    .lookup_address(level)
                    .and_then(|address| self.code_role(address))
                    == Some(crate::CodeRole::Dispatch)
            })
            .unwrap_or(stack.frames.len());
        stack.frames.iter().take(dispatch).all(|frame| {
            self.image_location(frame.context.instruction)
                .and_then(|location| location.physical_instance)
                .and_then(|instance| self.main.image.code_instance(instance))
                .and_then(|instance| self.main.image.function(instance.function()))
                .is_none_or(|function| function.coroutine().is_none())
        })
    }

    pub(super) fn reach_resumed_future(
        &mut self,
        pid: Pid,
        address: VirtualAddress,
    ) -> Result<bool> {
        let Some((kind, owner, awaiting)) = self
            .session
            .inferior()
            .and_then(|inferior| inferior.run.execution())
            .and_then(|active| match &active.kind {
                ActiveKind::Step {
                    kind, owner, start, ..
                } => start
                    .awaiting
                    .clone()
                    .filter(|awaiting| {
                        awaiting.waits()
                            && (awaiting.waiting.contains(&address)
                                || awaiting.drop_glue.contains(&address)
                                || awaiting.inlined_drops.contains(&address)
                                || awaiting.takes_up_task(address))
                    })
                    .map(|awaiting| (*kind, *owner, awaiting)),
                _ => None,
            })
        else {
            return Ok(false);
        };

        if let Some(entries) = awaiting
            .task_entries
            .clone()
            .filter(|_| awaiting.takes_up_task(address))
        {
            self.task_code_reached(pid, address, kind, entries, awaiting.task_return)?;
            return Ok(true);
        }

        if awaiting.drop_glue.contains(&address) || awaiting.inlined_drops.contains(&address) {
            let ours = if awaiting.drop_glue.contains(&address) {
                self.ptrace.registers(pid)?.arguments()[0] == awaiting.future.object.get()
            } else {
                self.runs_step(owner, pid)
            };

            if ours {
                self.future_dropped(pid, address, kind, awaiting.future)?;
            } else {
                self.pass_by(pid, address)?;
            }

            return Ok(true);
        }

        let awaiting = match &awaiting.inlined {
            Some(inlined) if !inlined.statements.contains(&address) => AwaitStep {
                inlined: None,
                ..awaiting
            },
            _ => awaiting,
        };

        let ours = self.resumes_step(pid, address, owner, &awaiting);
        if !ours {
            self.pass_by(pid, address)?;
            return Ok(true);
        }

        record!(
            "the future at {} resumed on thread {pid}",
            awaiting.future.object
        );
        self.follow_step(pid);
        if self.is_advance_target(address) {
            self.reach_advance_target(pid, address)?;
            return Ok(true);
        }

        let presentation = match &awaiting.inlined {
            None => self.presentation_for_thread(pid, None)?,
            Some(inlined) => match self.reentered(address, inlined, awaiting.source.as_ref()) {
                Reentry::Body(presentation) if kind == StepKind::Out || !presentation.1 => {
                    presentation.0
                }

                Reentry::Body(_) | Reentry::Outside => {
                    self.begin_visible_stop(pid, StopReason::Step { kind })?;
                    return Ok(true);
                }
            },
        };

        let mut resumed = self.innermost_step_start(pid, kind, || Ok(presentation))?;

        if kind == StepKind::OverSource {
            resumed.source.clone_from(&awaiting.source);
        }

        resumed.awaiting = Some(AwaitStep {
            waiting: BTreeSet::new(),
            ..awaiting
        });
        if let Some(waited) = self.active_step_mut() {
            resumed.targets = std::mem::take(&mut waited.targets);
            resumed.new_task = waited.new_task.take();
        }

        let execution = self.active_execution()?;
        self.cleanup_plan_breakpoints(execution)?;
        self.install_additional_plan_breakpoints(execution, &resumed.plan_sites())?;
        *self
            .active_step_mut()
            .ok_or_else(|| Error::internal("the step remained active as its future resumed"))? =
            resumed;
        self.go_on_without_plan(pid, address, Some(kind))?;
        Ok(true)
    }
}

impl<P: LinuxTraceOps> Controller<P> {
    fn same_future(&self, left: RunningFuture, right: RunningFuture) -> bool {
        left.object == right.object
            && (left.ty == right.ty
                || self
                    .module_of(left.ty)
                    .is_some_and(|module| module.image.same_type(left.ty, right.ty)))
    }

    fn resumes_step(
        &self,
        pid: Pid,
        address: VirtualAddress,
        owner: StepOwner,
        awaiting: &AwaitStep,
    ) -> bool {
        let Some(inlined) = &awaiting.inlined else {
            return self
                .running_future(pid, None)
                .is_some_and(|running| self.same_future(running, awaiting.future));
        };

        let body = self
            .image_location(address)
            .and_then(|location| match location.inline_frames {
                InlineFrameLookup::Unique(chain) => chain
                    .instances
                    .iter()
                    .copied()
                    .find(|instance| inlined.instances.contains(instance)),
                _ => None,
            });
        let named = if inlined.own {
            body.and_then(|instance| self.running_future(pid, Some(instance)))
                .map(|own| self.same_future(own, awaiting.future))
        } else {
            None
        };

        named
            .or_else(|| {
                (inlined.poll != awaiting.future || !inlined.own)
                    .then(|| self.running_future(pid, None))
                    .flatten()
                    .map(|running| self.same_future(running, inlined.poll))
            })
            .unwrap_or_else(|| owner.task.is_some() && self.runs_step(owner, pid))
    }
}

enum Reentry {
    Body((crate::FramePresentation, bool)),

    Outside,
}

impl<P: LinuxTraceOps> Controller<P> {
    fn reentered(
        &self,
        address: VirtualAddress,
        inlined: &Inlined,
        source: Option<&SourceLocation>,
    ) -> Reentry {
        let Some(location) = self.image_location(address) else {
            return Reentry::Outside;
        };

        let InlineFrameLookup::Unique(chain) = &location.inline_frames else {
            return Reentry::Outside;
        };

        let Some((depth, &instance)) = chain
            .instances
            .iter()
            .enumerate()
            .find(|(_, instance)| inlined.instances.contains(instance))
        else {
            return Reentry::Outside;
        };

        let moved = super::frames::source_for_code_instance(&self.main.image, &location, instance)
            .is_some_and(|line| super::frames::source_line_changed(source, Some(&line)));
        Reentry::Body((
            crate::FramePresentation {
                instruction: address,
                frame: PresentedFrame::Inline(instance),
                hidden_inline_frames: u32::try_from(chain.instances.len() - depth - 1)
                    .unwrap_or(u32::MAX),
            },
            moved,
        ))
    }
}

const UNKNOWN_RESUMPTION: &str = "where the future the step follows resumes is unknown";

const TWICE: &str = "the async function the step is in is awaited twice in its task, and \
                     inlined, so the step cannot tell which is its own";

const fn ending(end: TaskEnd) -> TaskEnding {
    match end {
        TaskEnd::Finished => TaskEnding::Finished,
        TaskEnd::Cancelled => TaskEnding::Cancelled,
    }
}

pub(super) enum Followed {
    Waits,

    Ended(StopReason),
}

pub(super) fn waits(start: &StepStart) -> bool {
    start.awaiting.as_ref().is_some_and(AwaitStep::waits)
}
