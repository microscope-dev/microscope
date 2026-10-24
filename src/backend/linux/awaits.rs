//! Steps through an async function's awaits.
//!
//! An async function's body runs in a function that its future's every
//! poll calls anew. A poll that reaches an await of a future that is not
//! ready returns `Pending` to its caller, with the future's state saying
//! where it waits. A step over or out of such a body does not end in the
//! caller then: it waits for the same future to be polled again, at the
//! point where that state resumes, and goes on from there as the step it
//! was. The future is pinned, so its address names it from one poll to the
//! next, whichever thread polls it; another future of the same function
//! that resumes there is not the step's.
//!
//! An optimized build may inline one async function's body into another's,
//! where it has no resume points of its own and may not name its future.
//! The step then follows the future of the function it is inlined into,
//! whose poll runs both, and once that poll returns `Pending`, waits for
//! the future to come back to the inlined body's statements, or to those
//! on another line of the code it is inlined into. A future names its
//! task, so where even that future's address is unavailable, the task
//! does.
//!
//! A step may also begin in a task no thread runs, at one of the async
//! functions it awaits in: it begins waiting for that function's future,
//! as if a poll of it had just returned `Pending`.
//!
//! A future may be dropped while the step waits for it: its runtime drops
//! a task's future as it cancels the task, and a `select!` or a timeout
//! drops a future it no longer awaits. The step watches the future's drop
//! glue for that, every copy of it: each unit of code that drops the type
//! may have its own, and optimization inlines others, which a thread of the
//! step's task runs for the task's future. Dropped by its runtime, the
//! future's task was cancelled, and the step ends there; dropped by the
//! program's code, the step goes on in that code to its next line.
//!
//! The step also watches its runtime take up its task again, as the runtime
//! model names the code that does: the task may end there, cancelled or
//! finished, even within the poll the step waits after, with nothing left
//! to drop that the step can see. Whatever frees the task stops a thread
//! first, so the task's state still says how it ended.

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

/// The future whose body a step runs, which the step follows across its
/// polls.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(super) struct AwaitStep {
    /// The future whose poll runs the step's body: the body's own, or the
    /// future of the function the body is inlined into.
    future: RunningFuture,

    /// For a body inlined into another's, where the step goes on.
    inlined: Option<Inlined>,

    /// The line the step began on, which it goes on past once the future
    /// resumes.
    source: Option<SourceLocation>,

    /// Where the future resumes, once a poll of it returned `Pending` and
    /// the step waits for the next.
    waiting: BTreeSet<VirtualAddress>,

    /// Where each copy of the function that drops the future begins,
    /// which the step watches while it waits.
    drop_glue: BTreeSet<VirtualAddress>,

    /// Where copies of the code that drops the future, inlined into other
    /// code and passed no place the step can check, begin: a thread of the
    /// step's task at one drops the task's future of that type.
    inlined_drops: BTreeSet<VirtualAddress>,

    /// The code of the step's runtime that takes up its task again, which
    /// the step watches while it waits, to see the task end.
    task_entries: Option<(crate::TaskId, TaskEntries)>,

    /// Where the runtime's code that runs the task returns, while it does.
    task_return: Option<TaskReturn>,
}

/// Where a function that runs a task returns to, and the stack pointer
/// there.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
struct TaskReturn {
    address: VirtualAddress,
    stack_pointer: u64,
}

/// An async function's body inlined into another's, which a step runs in.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(super) struct Inlined {
    /// The future whose poll runs the body.
    poll: RunningFuture,

    /// Whether the step follows the body's own future.
    own: bool,

    /// The body's function, which the future must await once only when the
    /// body names no future of its own.
    function: FunctionId,

    /// The body's code.
    instances: BTreeSet<CodeInstanceId>,

    /// Where the step goes on once its future is polled again: the body's
    /// statements, and those on another line of the code the body is
    /// inlined into, but none that only resuming runs.
    statements: BTreeSet<VirtualAddress>,
}

impl AwaitStep {
    /// Whether the step waits for its future to be polled again.
    pub(super) fn waits(&self) -> bool {
        !self.waiting.is_empty()
    }

    /// Where the step stops a thread while it waits.
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

    /// Whether `address` is where the step's runtime takes up its task or
    /// returns from running it.
    fn takes_up_task(&self, address: VirtualAddress) -> bool {
        self.task_return
            .is_some_and(|task_return| task_return.address == address)
            || self.task_entries.as_ref().is_some_and(|(_, entries)| {
                entries.runs.contains(&address) || entries.frees.contains(&address)
            })
    }
}

/// The future a body runs for.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(super) struct RunningFuture {
    object: VirtualAddress,
    ty: TypeReference,
}

impl<P: LinuxTraceOps> Controller<P> {
    /// What a step of `kind` from a stopped thread's innermost frame
    /// follows across its awaits: the future whose body the frame runs.
    pub(super) fn await_step(
        &self,
        pid: Pid,
        kind: StepKind,
        start: &StepStart,
    ) -> Option<AwaitStep> {
        // The futures it reads are the main executable's.
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

    /// For a step in an async function's body inlined into another's, the
    /// body's future, or where it names none, that of the function it is
    /// inlined into, and where the step goes on once the future is polled
    /// again.
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

        // The body may be inlined into a function no future runs, such as
        // a combinator's, and then must name its own.
        let own = self.running_future(pid, Some(instance));
        let poll = self.running_future(pid, None).or(own)?;
        let future = own.unwrap_or(poll);
        let registers = self.ptrace.registers(pid).ok()?;
        let location = self.image_location(VirtualAddress::new(registers.rip))?;
        let InlineFrameLookup::Unique(chain) = &location.inline_frames else {
            return None;
        };

        let depth = chain.instances.iter().position(|id| *id == instance)?;

        // The body's own statements, and those on another line of each
        // async function it is inlined into, out to the one that runs, but
        // none of a combinator's around it, such as `select!`'s.
        let mut statements = self.body_statements(instance).ok()?;
        let awaiters = chain.instances[..depth]
            .iter()
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

    /// Whether a step in an inlined async body that has left the body for
    /// the code at `address`, which is not where the step goes on, must
    /// go on: the code the body is inlined into awaits it there, which the
    /// step follows while the body may be pending. A body that names its
    /// future has returned once the future says so, which it does before
    /// the code it returns to runs; the state it suspends in may be stored
    /// only later.
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

    /// The statements of an async function's body inlined into another's,
    /// less those on its header's line: entering the body there dispatches
    /// on its state, which resuming it does too.
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

    /// Statements less those that only resuming a coroutine runs, such as
    /// its dispatch on its state.
    pub(super) fn without_resume_code(
        &self,
        statements: BTreeSet<VirtualAddress>,
    ) -> BTreeSet<VirtualAddress> {
        let Some(inferior) = self.inferior.as_ref() else {
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

    /// Whether the awaits of `future` hold more than one future of
    /// `function`, which a step in its inlined body cannot tell apart.
    fn awaits_twice(&self, pid: Pid, future: RunningFuture, function: FunctionId) -> bool {
        let Some(inferior) = self.inferior.as_ref() else {
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

    /// Begins a step of `kind` in a task no thread runs, from the async
    /// function `frame` selects among its awaits, or the innermost one that
    /// awaits it: the step waits for the function's future to resume, and
    /// goes on from there as the step it is. A step in goes on as a step
    /// over, since the call the line makes is the await already under way.
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

        let inferior = self.inferior.as_ref().ok_or(Error::NotRunning)?;
        let root = self.stack_root(stop_id, ExecutionContext::Task(task))?;
        let super::frames::RootOrigin::Suspended { reader, .. } = root.origin else {
            return Err(parked());
        };

        let stack = self.async_stack(inferior, &root)?.ok_or_else(parked)?;
        let level = frame.get() as usize;
        let selected = (level..stack.futures.len())
            .find(|&index| matches!(stack.futures[index].kind, AsyncFrameKind::Coroutine { .. }))
            .ok_or_else(|| {
                Error::FrameStepUnsupported("the task's frames hold no async function".into())
            })?;
        if stack.futures[selected].ty.image != inferior.loaded_module.image {
            return Err(Error::FrameStepUnsupported(
                "the async function is in a library, whose awaits steps do not follow".into(),
            ));
        }

        let own = self.named_by_body(&stack.futures[selected]);

        // The function may run out of line in one place and be inlined in
        // another, as an optimized build may make of a function some code
        // awaits and some polls, and either may be the copy that resumes
        // it: the step waits in both.
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
            source: stack.frames[selected].source.clone(),
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

    /// A suspended future as its body names it: the type its awaiter holds
    /// it as is a description of its own, while the body's polls find it
    /// as the type the body names.
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

    /// For a step of a suspended task from frame `selected` of its awaits,
    /// whose async function's body is inlined into another's, the future
    /// whose poll runs it, that of the function it is inlined into, and
    /// where the step goes on once that future is polled again: the body's
    /// statements, and those on another line of each function it is
    /// inlined into.
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

        // A coroutine's function may have a copy in each unit that uses
        // it, and any copy may be the one that runs, or is inlined.
        let functions_of = |index: usize| {
            self.main
                .image
                .coroutine_functions(stack.futures[index].ty.id)
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

        // The innermost future out from the selected one whose function
        // runs out of line, which the inlined bodies run in.
        let (running, physical) = (selected + 1..stack.futures.len())
            .filter(|&index| matches!(stack.futures[index].kind, AsyncFrameKind::Coroutine { .. }))
            .map(|index| (index, out_of_line(&functions_of(index))))
            .find(|(_, physical)| !physical.is_empty())
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

        let selected_functions = functions_of(selected);
        let function = *selected_functions.first().ok_or_else(unknown)?;
        let instances = inlined_in(&selected_functions);
        let mut statements = BTreeSet::new();

        for instance in &instances {
            statements.extend(self.body_statements(*instance)?);
        }

        for index in selected + 1..=running {
            let Some(line) = &stack.frames[index].source else {
                continue;
            };

            let outers = if index == running {
                physical.iter().map(|instance| instance.id()).collect()
            } else {
                inlined_in(&functions_of(index))
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
            self.named_by_body(&stack.futures[selected]),
            Some(Inlined {
                poll: self.named_by_body(&stack.futures[running]),
                own: true,
                function,
                instances,
                statements: statements.clone(),
            }),
            statements,
        ))
    }

    /// The future whose body a stopped thread's innermost activation runs
    /// in the code instance `selected`, or in the activation's own function
    /// when `None`, read from the pointer the body is passed. A body
    /// inlined into another's has a future of its own, or none the step
    /// can name, never the other's.
    fn running_future(&self, pid: Pid, selected: Option<CodeInstanceId>) -> Option<RunningFuture> {
        let inferior = self.inferior.as_ref()?;
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
            registers: stack.registers(0),
            cfa: self.frame_cfa(pid, &modules, code, &innermost.registers),
            activation: 0,
            below_stack_pointer: self.below_stack_pointer(inferior, &root, innermost),
        };

        let coroutine_type = TypeReference {
            image: module.loaded.image,
            id: coroutine,
        };
        let Ok(key) = module.variables.visible_object(
            address,
            (instance != physical).then_some(instance),
            "$future",
        ) else {
            // No Rust before 1.93 describes the parameter that passes a
            // body its future, so its out-of-line bodies have no `$future`;
            // the future its runtime drives says which it is. An inlined
            // body that names none uses the future it is inlined into's,
            // as it does in any build.
            return (instance == physical)
                .then(|| self.polled_future(pid, coroutine_type, None))
                .flatten()
                .map(|object| RunningFuture {
                    object,
                    ty: coroutine_type,
                });
        };
        let mut runtime = self.frame_runtime(inferior, &root, &resolved, module);
        let mut budget =
            crate::inspection::InspectionBudget::new(crate::InspectionLimits::default());
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

    /// The state a future that is not running is in, and its number.
    fn future_state(&self, pid: Pid, future: RunningFuture) -> Option<(u64, CoroutineStateKind)> {
        let inferior = self.inferior.as_ref()?;
        let module = self.module_of(future.ty)?;

        // The walk lists the future it began at last.
        let chain = self.with_module_stop(inferior, &module.loaded, pid, |stop| {
            futures::walk(module.image.as_ref(), stop, future.object, future.ty)
        });
        match chain.frames.last()?.kind {
            AsyncFrameKind::Coroutine { state, kind, .. } => Some((state, kind)),
            AsyncFrameKind::Leaf => None,
        }
    }

    /// Where a future that is not running resumes when it is next polled:
    /// the point its suspended state goes on from, or `None` when it is in
    /// no suspended state, having returned, or its resume point is unknown.
    fn resume_point(&self, pid: Pid, future: RunningFuture) -> Option<VirtualAddress> {
        let (state, CoroutineStateKind::Suspended { .. }) = self.future_state(pid, future)? else {
            return None;
        };

        let module = self.module_of(future.ty)?;
        let functions = module.image.coroutine_functions(future.ty.id);
        let address = super::async_frames::resume_address(&module.image, &functions, state)?;
        module.loaded.virtual_address(address).ok()
    }

    /// Where a step waits for its future once a poll of it returned:
    /// nowhere when the future returned, or why the step ends here when
    /// the future's state leaves it nowhere it can tell.
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

        // Only a future known to have returned lets the step go on as any
        // other does; one whose state is unknown leaves it nowhere to wait.
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

    /// Follows the active step's future once the poll that ran its body
    /// has just returned: when it returned `Pending`, the step waits for the
    /// future to be polled again where it resumes, in place of the rest of
    /// its plan; when the future returned to its runtime as its task's own,
    /// the step ends there, with the task. `None` while the poll runs, or
    /// when the step goes on as any other does. The thread is left stopped
    /// either way.
    pub(super) fn follow_poll_return(
        &mut self,
        pid: Pid,
        kind: StepKind,
    ) -> Result<Option<Followed>> {
        let Some((execution, task, awaiting, activation)) = self
            .inferior
            .as_ref()
            .and_then(|inferior| inferior.active.as_ref())
            .and_then(|active| match &active.kind {
                ActiveKind::Step { owner, start, .. } => {
                    let awaiting = start.awaiting.as_ref().filter(|step| !step.waits())?;
                    if !self.runs_step(*owner, pid) {
                        record!(
                            "thread {pid} runs no task of the step that follows a future, \
                             {owner:?}: {:?}",
                            self.inferior
                                .as_ref()
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

        // The poll that returned `Pending` runs within the runtime's, which
        // may yet end the task before it returns: its state is read there
        // only where what frees the task would have stopped a thread first.
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

        // An advance's targets end it wherever its task reaches them, and
        // a step into a new task still watches for one.
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

    /// The code of `task`'s runtime that takes it up again, read by the
    /// stopped thread `reader`.
    fn task_entries(
        &self,
        reader: Pid,
        task: crate::TaskId,
    ) -> Option<(crate::TaskId, TaskEntries)> {
        let inferior = self.inferior.as_ref()?;
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

    /// Where the runtime's dispatch that runs the task on thread `pid`
    /// returns to, read from the thread's stack.
    fn dispatch_return(&self, pid: Pid) -> Option<TaskReturn> {
        let inferior = self.inferior.as_ref()?;
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
            stack_pointer: caller.registers.get(crate::unwind::RSP)?,
        })
    }

    /// Goes on with a step that waits for its task as thread `pid` reaches
    /// `address`, where the runtime takes up the task or returns from
    /// running it: once the task has ended, the step ends saying how;
    /// while it runs, the step watches for its return.
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
            task_return.address == address && task_return.stack_pointer == registers.rsp
        });
        let entered = !returned
            && self.with_task_runtime(pid, task, |runtime, stop| {
                Ok(runtime.takes_up(
                    stop,
                    &entries,
                    address,
                    &super::registers::x86_64_registers(&registers),
                ))
            }) == Ok(true);

        // Whatever frees the task stops a thread first, so the task's
        // memory still holds it as the code that ran it returns.
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
            // Entered, the return address is the word the stack pointer
            // points at, which the return pops.
            let returns = self.ptrace.read_word(pid, registers.rsp)?;
            self.watch_task_return(TaskReturn {
                address: VirtualAddress::new(returns),
                stack_pointer: registers.rsp + 8,
            })?;
        }

        self.pass_by(pid, address)
    }

    /// Lets thread `pid` go on past the site at `address`, which it
    /// reached for no step of its own.
    fn pass_by(&mut self, pid: Pid, address: VirtualAddress) -> Result<()> {
        if self.barrier_active() {
            self.finish_barrier_if_ready()
        } else {
            self.repair_when_alone(pid, address)
        }
    }

    /// Watches where the code that runs the step's task returns. A return
    /// watched before stays a plan site, where a thread passes.
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

    /// Calls `read` with the model of `task`'s runtime and a stop read by
    /// thread `pid`.
    fn with_task_runtime<T>(
        &self,
        pid: Pid,
        task: crate::TaskId,
        read: impl FnOnce(
            &dyn crate::runtime_model::RuntimeModel,
            &dyn crate::runtime_model::RuntimeStop,
        ) -> std::result::Result<T, std::sync::Arc<str>>,
    ) -> std::result::Result<T, std::sync::Arc<str>> {
        let inferior = self.inferior.as_ref().ok_or("the process has ended")?;
        let runtime = self
            .runtimes(inferior)
            .into_iter()
            .find(|runtime| runtime.id == task.runtime)
            .ok_or("the task's runtime is no longer loaded")?;
        self.with_runtime_stop(inferior, &runtime, pid, |stop| {
            read(runtime.model.as_ref(), stop)
        })
    }

    /// Where the code that drops every future of type `ty` begins, which
    /// rustc names for the type's full name, as
    /// [`crate::rust_names::DROP_GLUE`] lists: each copy of the function,
    /// and each copy inlined elsewhere.
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

        // Each codegen unit may have a copy of its own. A program has its
        // glue under the one name its compiler gives it; a step that looked
        // for another would miss every drop of its future, and run on past
        // a cancelled task without a word.
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

    /// Ends or goes on with a step whose future a thread at its drop glue
    /// is dropping. When the runtime's code drops it, the runtime cancelled
    /// its task, and the step ends; when the program's does, the step goes
    /// on in the frame that drops it, to that frame's next line.
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
        let inferior = self.inferior.as_ref().ok_or(Error::NotRunning)?;
        let stack =
            self.physical_stack(inferior, &StackRoot::of_thread(pid), DEFAULT_MAX_FRAMES)?;

        // The future is dropped by the code that drops what holds it, as
        // the drop glue of each value around it passes it on, whether as a
        // function of its own or inlined into the dropper's.
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
            // No code of the program's drops it: its task was cancelled,
            // when the runtime says so, or code the step cannot follow
            // dropped it.
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
            .inferior
            .as_mut()
            .and_then(|inferior| inferior.active.as_mut())
            .map(|active| &mut active.kind)
        {
            // A step out ends where the step over the dropping frame's line
            // does, and is reported as the step the client asked for.
            *running = StepKind::OverSource;
            **active = start;
        }

        self.go_on_without_plan(pid, address, None)
    }

    /// A step over the line that the physical frame `level` of a stopped
    /// thread's stack is at, to the frame's next line or its return, which
    /// goes on after the frame dropped the future `dropped`.
    fn step_on_in_frame(
        &self,
        pid: Pid,
        stack: &super::frames::PhysicalStack,
        level: usize,
        dropped: RunningFuture,
    ) -> Result<Option<StepStart>> {
        let inferior = self.inferior.as_ref().ok_or(Error::NotRunning)?;
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

    /// The stop a step that goes on after its future was dropped reports
    /// as it completes: that the future was dropped.
    pub(super) fn dropped_step_reason(&self, reason: StopReason) -> StopReason {
        let dropped = self
            .inferior
            .as_ref()
            .and_then(|inferior| inferior.active.as_ref())
            .is_some_and(
                |active| matches!(&active.kind, ActiveKind::Step { start, .. } if start.dropped.is_some()),
            );
        match reason {
            StopReason::Step { kind } if dropped => StopReason::FutureDropped { kind },
            reason => reason,
        }
    }

    /// Whether a thread whose future just returned is in its runtime's
    /// code, polling no future of the program's within the dispatch that
    /// runs its task: it returned from its task's own future, not to an
    /// awaiter, nor to a future of the runtime's that the program awaits,
    /// such as a timeout. Futures that poll the dispatch itself, as a
    /// `LocalSet`'s `run_until` does, are not the task's.
    fn returned_to_runtime(&self, pid: Pid, registers: &nix::libc::user_regs_struct) -> bool {
        if !self
            .code_role(VirtualAddress::new(registers.rip))
            .is_some_and(super::stepping::is_runtime_role)
        {
            return false;
        }

        let Some(inferior) = self.inferior.as_ref() else {
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
        stack.frames[..dispatch].iter().all(|frame| {
            self.image_location(frame.context.instruction)
                .and_then(|location| location.physical_instance)
                .and_then(|instance| self.main.image.code_instance(instance))
                .and_then(|instance| self.main.image.function(instance.function()))
                .is_none_or(|function| function.coroutine().is_none())
        })
    }

    /// Goes on with a step that waits for its future at `address`, once a
    /// thread arrives there. The step's own future goes on as the step it
    /// was, from where it resumed; any other is not the step's, and passes.
    /// Returns whether the site was where the step waits and the arrival
    /// has been handled.
    pub(super) fn reach_resumed_future(
        &mut self,
        pid: Pid,
        address: VirtualAddress,
    ) -> Result<bool> {
        let Some((kind, owner, awaiting)) = self
            .inferior
            .as_ref()
            .and_then(|inferior| inferior.active.as_ref())
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

        // Drop glue is passed the place it drops; a copy of it inlined
        // elsewhere drops one in the task that runs it.
        if awaiting.drop_glue.contains(&address) || awaiting.inlined_drops.contains(&address) {
            let ours = if awaiting.drop_glue.contains(&address) {
                self.ptrace.registers(pid)?.rdi == awaiting.future.object.get()
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

        // A step that waits in both copies of a function, out of line and
        // inlined, goes on from its resume point as from any out of line.
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

                // A statement on another line than the step's of the body
                // or of the code it is inlined into is where the step
                // ends.
                Reentry::Body(_) | Reentry::Outside => {
                    self.begin_visible_stop(pid, StopReason::Step { kind })?;
                    return Ok(true);
                }
            },
        };

        let mut resumed = self.innermost_step_start(pid, kind, || Ok(presentation))?;

        // The step goes on past the line it began on, which the await is
        // on, or past the frame it began in.
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
    /// Whether two futures are one: at one place, and of one type, which
    /// each unit of code that uses it may describe anew.
    fn same_future(&self, left: RunningFuture, right: RunningFuture) -> bool {
        left.object == right.object
            && (left.ty == right.ty
                || self
                    .module_of(left.ty)
                    .is_some_and(|module| module.image.same_type(left.ty, right.ty)))
    }

    /// Whether the future that thread `pid` resumes at `address`, where
    /// the step `awaiting` waits, is the step's: named by the body where it
    /// names its own, or by the future whose poll runs it. An inlined
    /// body's code may hold its future nowhere; the future names its task,
    /// which then names the future.
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

/// Where a future whose step is in an inlined body comes back to.
enum Reentry {
    /// The body, presented as the step's frame, and whether on another line
    /// than the step's.
    Body((crate::FramePresentation, bool)),

    /// The code the body is inlined into, having returned meanwhile.
    Outside,
}

impl<P: LinuxTraceOps> Controller<P> {
    /// Where the future of a step in the inlined body `inlined` has come
    /// back to at `address`, which began on line `source`.
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

        let Some(depth) = chain
            .instances
            .iter()
            .position(|instance| inlined.instances.contains(instance))
        else {
            return Reentry::Outside;
        };

        let instance = chain.instances[depth];
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

/// Why a step in an inlined body cannot follow its future.
const UNKNOWN_RESUMPTION: &str = "where the future the step follows resumes is unknown";

const TWICE: &str = "the async function the step is in is awaited twice in its task, and \
                     inlined, so the step cannot tell which is its own";

/// How a step reports how its runtime ended its task.
const fn ending(end: TaskEnd) -> TaskEnding {
    match end {
        TaskEnd::Finished => TaskEnding::Finished,
        TaskEnd::Cancelled => TaskEnding::Cancelled,
    }
}

/// What a step does once the poll that ran its future's body returned.
pub(super) enum Followed {
    /// It waits for the future to be polled again.
    Waits,

    /// It ends, for this reason.
    Ended(StopReason),
}

/// Whether a step waits for its future to be polled again, and so cannot
/// end before then.
pub(super) fn waits(start: &StepStart) -> bool {
    start.awaiting.as_ref().is_some_and(AwaitStep::waits)
}
