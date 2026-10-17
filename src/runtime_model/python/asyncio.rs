//! asyncio's tasks, from CPython 3.14, which lists each `asyncio.Task`,
//! and each object of a class deriving from it, in the state of the thread
//! that made it, and those of threads that have exited in their
//! interpreter's state. Each list is circular, linked through each task's
//! node, and its head is no task's. A thread's state names the task it
//! runs.
//!
//! A task is one only where its type is, or derives from, the type C
//! defines as `_asyncio.Task` with objects of the size of the structure
//! the layouts copy (`scripts/cpython-layouts.c`), which conformance.rs
//! checks against `_asyncio`'s own `.AsyncioDebug` section.
//!
//! A task no thread runs is suspended in its coroutine, which awaits
//! another coroutine, which may await another: each a coroutine or
//! generator suspended in its `await` of the object atop its frame's
//! stack. Their frames, innermost first, are the task's logical stack.
//! The chain ends at an object no coroutine is, such as the future the
//! task waits for. Its walk is bounded, and a loop ends it with a problem.

use std::collections::{BTreeMap, HashSet};
use std::sync::Arc;

use super::frames::Walk;
use super::objects::{Problem, malformed};
use super::threads::ThreadState;
use super::values::{Object, Values, exact};
use crate::image::python::{Constant, Field};
use crate::runtime_model::{InterpretedFrame, RuntimeTask, TaskLabels, TaskState};
use crate::{ThreadId, VirtualAddress};

/// The most tasks of one list read.
const MAX_TASKS: usize = 1 << 16;

/// The most coroutines of one task's chain of awaits read.
const MAX_AWAITS: usize = 1 << 10;

/// The most tasks awaiting one task named.
const MAX_AWAITERS: u64 = 64;

/// The most values a suspended frame's stack is taken to hold, beyond
/// which its stack pointer is taken for a wrong one.
const MAX_STACK: u64 = 1 << 16;

/// Every task of the process, and the tasks threads run.
pub(super) struct Tasks {
    /// Each task, by its object: each thread state's, in the order the
    /// states are given, then each interpreter's.
    pub(super) all: Vec<VirtualAddress>,

    /// The thread running each task one runs.
    pub(super) running: BTreeMap<VirtualAddress, ThreadId>,

    /// Why some tasks may be missing.
    pub(super) gaps: Vec<Problem>,
}

/// A suspended task's logical stack: its coroutines' frames, innermost
/// first, and why they end before its outermost coroutine's, if they do.
pub(super) struct Awaits {
    pub(super) frames: Vec<InterpretedFrame>,
    pub(super) end: Option<Problem>,
}

/// What a coroutine's state says it is doing.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Coroutine {
    /// Not yet started, or suspended at a bare `yield`: it awaits
    /// nothing a frame shows.
    Resting,

    /// Suspended in an `await` of the object atop its frame's stack.
    Awaiting,
    Running,
    Finished,
}

impl Values<'_, '_> {
    /// Every task of the threads of `states`, and of their interpreters.
    pub(super) fn asyncio_tasks(&self, states: &[ThreadState]) -> Tasks {
        let mut found = Tasks {
            all: Vec::new(),
            running: BTreeMap::new(),
            gaps: Vec::new(),
        };

        let mut interpreters = Vec::new();
        for state in states {
            if !interpreters.contains(&(state.interpreter_state, state.interpreter)) {
                interpreters.push((state.interpreter_state, state.interpreter));
            }

            let thread = ThreadId::new(state.native);
            match self.running_task(state) {
                Ok(Some(task)) => {
                    found.running.insert(task, thread);
                }
                Ok(None) => {}
                Err(problem) => found.gaps.push(problem),
            }

            let whose = format!("thread {}", state.native);
            self.task_list(
                (state.address, Field::ThreadAsyncioTasksHead),
                &whose,
                &mut found,
            );
        }

        for (interpreter, id) in interpreters {
            let whose = format!("interpreter {id}'s exited threads");
            self.task_list(
                (interpreter, Field::InterpreterAsyncioTasksHead),
                &whose,
                &mut found,
            );
        }

        found
    }

    /// The task the thread of `state` runs, if it runs one.
    pub(super) fn running_task(
        &self,
        state: &ThreadState,
    ) -> Result<Option<VirtualAddress>, Problem> {
        let task = self
            .reader
            .field_word(state.address, Field::ThreadAsyncioRunningTask)?;
        if task == 0 {
            return Ok(None);
        }

        let task = VirtualAddress::new(task);
        if !self.is_task(task)? {
            return Err(format!(
                "thread {} runs the task at {task:#x}, which is no asyncio.Task",
                state.native
            )
            .into());
        }

        Ok(Some(task))
    }

    /// Adds the tasks of the list whose head is at `field` of the state at
    /// `owner` to `found`, and why the rest cannot be read, if they cannot.
    fn task_list(&self, (owner, field): (VirtualAddress, Field), whose: &str, found: &mut Tasks) {
        if let Err(problem) = self.each_task((owner, field), whose, &mut found.all) {
            found.gaps.push(problem);
        }
    }

    fn each_task(
        &self,
        (owner, field): (VirtualAddress, Field),
        whose: &str,
        tasks: &mut Vec<VirtualAddress>,
    ) -> Result<(), Problem> {
        let reader = &self.reader;
        let head = owner.wrapping_add(reader.offset(field)?);
        let next = reader.offset(Field::LlistNext)?;
        let node_offset = reader.offset(Field::TaskNode)?;
        let mut seen = HashSet::new();
        let mut node = VirtualAddress::new(reader.word(head.wrapping_add(next))?);
        while node != head {
            if node == VirtualAddress::new(0) {
                return Err(format!("the list of {whose} tasks is broken").into());
            }

            if !seen.insert(node) {
                return Err(format!("the list of {whose} tasks loops").into());
            }

            if seen.len() > MAX_TASKS {
                return Err(format!("{whose} has more than {MAX_TASKS} tasks").into());
            }

            let task = VirtualAddress::new(node.get().wrapping_sub(node_offset));
            if !self.is_task(task)? {
                return Err(format!(
                    "the list of {whose} tasks holds {task:#x}, which is no asyncio.Task"
                )
                .into());
            }

            tasks.push(task);
            node = VirtualAddress::new(reader.word(node.wrapping_add(next))?);
        }

        Ok(())
    }

    /// Whether the object at `object` is an `asyncio.Task`, or of a class
    /// deriving from it.
    pub(super) fn is_task(&self, object: VirtualAddress) -> Result<bool, Problem> {
        let reader = &self.reader;
        let ty = VirtualAddress::new(reader.field_word(object, Field::ObjectType)?);
        let class = self.class(ty)?;
        let laid_out = match self.static_base(&class)? {
            Some(base) => self.class(base)?,
            None => class,
        };
        if laid_out.module.as_deref() != Some("_asyncio") || &*laid_out.name != "Task" {
            return Ok(false);
        }

        let size = reader.field_word(laid_out.address, Field::TypeBasicsize)?;
        Ok(reader.layout.field(Field::TaskObjectSize) == Some(size))
    }

    /// The task at `task`, which the thread `running` runs, if one does.
    pub(super) fn task(&self, task: VirtualAddress, running: Option<ThreadId>) -> RuntimeTask {
        let (state, detail) = match running {
            Some(_) => (TaskState::Running, None),
            None => match self.waiting(task) {
                Ok(state) => state,
                Err(problem) => (TaskState::Unknown(problem), None),
            },
        };

        let mut labels = TaskLabels::new();
        match self.task_name(task) {
            Ok(name) => labels.push(("name".into(), name.into())),
            Err(problem) => labels.push(("name".into(), format!("<{problem}>").into())),
        }

        match self.awaiters(task) {
            Ok(awaiters) if awaiters.is_empty() => {}
            Ok(awaiters) => labels.push(("awaited_by".into(), awaiters.join(", ").into())),
            Err(problem) => labels.push(("awaited_by".into(), format!("<{problem}>").into())),
        }

        // A task's number is its address, which is Python's `id()` of it.
        RuntimeTask {
            number: task.get(),
            locator: task.get(),
            state,
            detail,
            thread: running,
            resume: None,
            creation: None,
            spawned: None,
            entry: None,
            parent: None,
            internal: false,
            labels,
        }
    }

    /// What a task no thread runs is doing: done, waiting for the future
    /// its detail names, or ready to run its next step.
    fn waiting(&self, task: VirtualAddress) -> Result<(TaskState, Option<Arc<str>>), Problem> {
        let reader = &self.reader;
        let layout = reader.layout;
        let state = reader.field_bytes(task, Field::TaskState, 4)?;
        let state = u32::from_le_bytes(
            state
                .try_into()
                .map_err(|_| malformed(task, "asyncio.Task"))?,
        );
        if state != u32::from(layout.constant(Constant::FuturePending)) {
            return Ok((TaskState::Exited, None));
        }

        let waiter = reader.field_word(task, Field::TaskFutWaiter)?;
        if waiter == 0 {
            return Ok((TaskState::Runnable, None));
        }

        let waiter = VirtualAddress::new(waiter);
        let detail = if self.is_task(waiter)? {
            format!("awaits task {}", self.task_name(waiter)?)
        } else {
            let ty = VirtualAddress::new(reader.field_word(waiter, Field::ObjectType)?);
            format!("awaits {}", self.class(ty)?.qualified())
        };
        Ok((TaskState::Blocked, Some(detail.into())))
    }

    /// The task's name, as `get_name()` gives it: until Python asks for
    /// it, a task given none keeps the number of its name, `Task-N`.
    pub(super) fn task_name(&self, task: VirtualAddress) -> Result<String, Problem> {
        let name = VirtualAddress::new(self.reader.field_word(task, Field::TaskName)?);
        match self.object(name)? {
            Object::Int { negative, digits } => exact(negative, &digits, self.shift())
                .map(|number| format!("Task-{number}"))
                .ok_or_else(|| malformed(task, "asyncio.Task")),
            Object::Str { .. } => self
                .str_text(name)
                .map(|name| name.to_string())
                .ok_or_else(|| malformed(task, "asyncio.Task")),
            _ => Err(malformed(task, "asyncio.Task")),
        }
    }

    /// The names of the tasks that await the task at `task`, in the order
    /// it keeps them; the futures that await it that are no tasks are
    /// left out.
    fn awaiters(&self, task: VirtualAddress) -> Result<Vec<String>, Problem> {
        let reader = &self.reader;
        let awaited_by = reader.field_word(task, Field::TaskAwaitedBy)?;
        if awaited_by == 0 {
            return Ok(Vec::new());
        }

        let awaited_by = VirtualAddress::new(awaited_by);
        let is_set = reader.field_bytes(task, Field::TaskAwaitedByIsSet, 1)?;
        let mut awaiters = Vec::new();
        if is_set.first() == Some(&0) {
            if self.is_task(awaited_by)? {
                awaiters.push(self.task_name(awaited_by)?);
            }

            return Ok(awaiters);
        }

        let Object::Set {
            table, mask, used, ..
        } = self.object(awaited_by)?
        else {
            return Err(malformed(task, "asyncio.Task"));
        };

        self.set_items(
            awaited_by,
            table,
            mask,
            used,
            0..used.min(MAX_AWAITERS),
            |awaiter| {
                if self.is_task(awaiter)? {
                    awaiters.push(self.task_name(awaiter)?);
                }

                Ok(())
            },
        )?;
        if used > MAX_AWAITERS {
            awaiters.push(format!("and {} more", used - MAX_AWAITERS));
        }

        Ok(awaiters)
    }

    /// The logical stack of the task at `task`, which no thread runs: the
    /// frames of its coroutine and of each coroutine it awaits.
    pub(super) fn awaits(&self, walk: &mut Walk<'_, '_>, task: VirtualAddress) -> Awaits {
        let mut frames = Vec::new();
        let end = self.each_await(walk, task, &mut frames).err();
        frames.reverse();
        if let Some(outermost) = frames.last_mut() {
            outermost.outermost = true;
        }

        Awaits { frames, end }
    }

    /// Adds the frame of each coroutine of the task's chain, outermost
    /// first, to `frames`, or says why the chain ends before its last.
    fn each_await(
        &self,
        walk: &mut Walk<'_, '_>,
        task: VirtualAddress,
        frames: &mut Vec<InterpretedFrame>,
    ) -> Result<(), Problem> {
        let reader = &self.reader;
        let mut seen = HashSet::new();
        let mut next = VirtualAddress::new(reader.field_word(task, Field::TaskCoro)?);
        loop {
            if !seen.insert(next) {
                return Err("the task's coroutines await each other in a loop".into());
            }

            if seen.len() > MAX_AWAITS {
                return Err(format!("the task awaits more than {MAX_AWAITS} coroutines").into());
            }

            let ty = VirtualAddress::new(reader.field_word(next, Field::ObjectType)?);
            if ty != self.types.coroutine && ty != self.types.generator {
                // The chain ends at what the innermost coroutine awaits,
                // such as a future; a task whose own is no coroutine
                // runs code no frame shows.
                if frames.is_empty() {
                    return Err(format!(
                        "the task runs a `{}`, which is no coroutine",
                        self.class(ty)?.qualified()
                    )
                    .into());
                }

                return Ok(());
            }

            let frame = next.wrapping_add(reader.offset(Field::GenFrame)?);
            let state = self.coroutine(next)?;
            match state {
                Coroutine::Finished => {
                    return Err(format!("the coroutine at {next:#x} has finished").into());
                }
                Coroutine::Running => {
                    return Err(format!(
                        "the coroutine at {next:#x} is running, on no thread asyncio names"
                    )
                    .into());
                }
                Coroutine::Resting | Coroutine::Awaiting => {
                    frames.push(walk.frame(&reader.record(frame, Field::FrameSize)?));
                }
            }

            if state == Coroutine::Resting {
                return Ok(());
            }

            next = self.awaited(frame)?;
        }
    }

    /// What the coroutine or generator at `generator` is doing.
    fn coroutine(&self, generator: VirtualAddress) -> Result<Coroutine, Problem> {
        let layout = self.reader.layout;
        let state = self
            .reader
            .field_bytes(generator, Field::GenFrameState, 1)?
            .first()
            .copied()
            .ok_or_else(|| malformed(generator, "coroutine"))?;
        let is = |constant| state == layout.constant(constant);
        Ok(
            if is(Constant::GeneratorYieldFrom) || is(Constant::GeneratorYieldFromLocked) {
                Coroutine::Awaiting
            } else if is(Constant::GeneratorCreated) || is(Constant::GeneratorSuspended) {
                Coroutine::Resting
            } else if is(Constant::GeneratorExecuting) {
                Coroutine::Running
            } else {
                Coroutine::Finished
            },
        )
    }

    /// The object the suspended frame at `frame` awaits, which its stack
    /// holds as near its top as the layout says.
    fn awaited(&self, frame: VirtualAddress) -> Result<VirtualAddress, Problem> {
        let reader = &self.reader;
        let values = frame.wrapping_add(reader.offset(Field::FrameLocalsplus)?);
        let top = reader.field_word(frame, Field::FrameStackpointer)?;
        let below = 8 * u64::from(reader.layout.constant(Constant::GeneratorAwaitedDepth));
        let bad = || -> Problem {
            format!("the frame at {frame:#x} has its stack pointer at {top:#x}, outside its stack")
                .into()
        };
        let depth = top.checked_sub(values.get()).ok_or_else(bad)?;
        if depth < below || below == 0 || !depth.is_multiple_of(8) || depth > 8 * MAX_STACK {
            return Err(bad());
        }

        let bits = reader.word(VirtualAddress::new(top - below))?;
        self.reference(bits)?
            .ok_or_else(|| format!("the frame at {frame:#x} awaits nothing").into())
    }
}
