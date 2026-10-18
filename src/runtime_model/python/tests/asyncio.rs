//! asyncio's tasks, read from synthetic memory: the lists of tasks each
//! thread state and interpreter keeps, each task's state, name, and
//! awaiters, and each suspended task's chain of awaits.

use std::sync::Arc;

use super::super::asyncio::{Awaits, Tasks};
use super::super::code::CodeTable;
use super::super::frames::Walk;
use super::super::threads::ThreadState;
use super::super::values::Types;
use super::values::with_values;
use super::{FRAME_SIZE, Process, layout};
use crate::image::python::{Constant, Field, PythonLayout};
use crate::runtime_model::TaskState;
use crate::{InspectionLimits, ThreadId, VirtualAddress};

/// The size of the tests' `asyncio.Task`s, whose fields lie within it.
const TASK_SIZE: u64 = 72;

/// Where a coroutine embeds its frame, and the size of one.
const GEN_FRAME: u64 = 32;
const GEN_SIZE: u64 = GEN_FRAME + FRAME_SIZE;

/// The size of the tests' thread and interpreter states, past their lists.
const STATE_SIZE: u64 = 64;

/// The tests' layout with asyncio's fields: a task's, a thread state's and
/// an interpreter's lists of tasks, and a coroutine's state and frame.
fn asyncio_layout() -> PythonLayout {
    let mut layout = layout();
    let fields = [
        (Field::ThreadAsyncioRunningTask, 24),
        (Field::ThreadAsyncioTasksHead, 32),
        (Field::InterpreterAsyncioTasksHead, 24),
        (Field::LlistNext, 0),
        (Field::LlistPrev, 8),
        (Field::TaskAwaitedBy, 16),
        (Field::TaskState, 24),
        (Field::TaskIsTask, 28),
        (Field::TaskAwaitedByIsSet, 29),
        (Field::TaskFutWaiter, 32),
        (Field::TaskCoro, 40),
        (Field::TaskName, 48),
        (Field::TaskNode, 56),
        (Field::TaskObjectSize, TASK_SIZE),
        (Field::GenFrameState, 16),
        (Field::GenFrame, GEN_FRAME),
        (Field::FrameStackpointer, 88),
    ];
    for (field, offset) in fields {
        layout.set_field(field, offset);
    }

    // 3.14's states, each the byte of a negative `int8_t`.
    layout.set_constant(Constant::GeneratorCreated, 0xfd);
    layout.set_constant(Constant::GeneratorSuspended, 0xfe);
    layout.set_constant(Constant::GeneratorYieldFrom, 0xff);
    layout.set_constant(Constant::GeneratorYieldFromLocked, 0xff);
    layout.set_constant(Constant::GeneratorExecuting, 0);
    layout.set_constant(Constant::GeneratorAwaitedDepth, 1);
    layout.set_constant(Constant::FuturePending, 0);
    layout
}

/// A process with asyncio's layout, its built-in types, and `_asyncio`'s
/// `Task`, whose objects are `size` bytes.
fn world(size: u64) -> (Process, Types, VirtualAddress) {
    let mut process = Process::new(asyncio_layout());
    let types = process.builtin_types();
    let task = process.type_object("_asyncio.Task");
    process.word(task.get(), Field::TypeBasicsize, size);
    (process, types, task)
}

impl Process {
    /// An empty circular list whose head is at `field` of `owner`.
    fn empty_list(&mut self, owner: u64, field: Field) -> u64 {
        let head = owner + self.offset(field);
        self.memory.write(head, head, 8);
        head
    }

    /// Links the tasks into the list whose head is at `head`, in order.
    fn link(&mut self, head: u64, tasks: &[u64]) {
        let node = self.offset(Field::TaskNode);
        let next = self.offset(Field::LlistNext);
        let prev = self.offset(Field::LlistPrev);
        let mut previous = head;
        for task in tasks {
            self.memory.write(previous + next, task + node, 8);
            self.memory.write(task + node + prev, previous, 8);
            previous = task + node;
        }
        self.memory.write(previous + next, head, 8);
        self.memory.write(head + prev, previous, 8);
    }

    /// A task of type `ty` running `coroutine`, named by `name`, an object.
    fn task(&mut self, ty: VirtualAddress, coroutine: u64, name: u64) -> u64 {
        let task = self.object(ty, TASK_SIZE);
        self.word(task, Field::TaskCoro, coroutine);
        self.word(task, Field::TaskName, name);
        task
    }

    /// A coroutine of `types` in the state `state`, a constant, whose frame
    /// runs `code`, and awaits `awaited` when it is one.
    fn coroutine(
        &mut self,
        types: &Types,
        state: Constant,
        code: u64,
        awaited: Option<u64>,
    ) -> u64 {
        let coroutine = self.object(types.coroutine, GEN_SIZE);
        let byte = self.owner(state);
        self.put(coroutine, Field::GenFrameState, byte, 1);
        let frame = coroutine + GEN_FRAME;
        self.word(frame, Field::FrameExecutable, code);
        let instruction = self.instruction(code, 0);
        self.word(frame, Field::FrameInstruction, instruction);
        let owner = self.owner(Constant::OwnerGenerator);
        self.put(frame, Field::FrameOwner, owner, 1);
        let values = frame + self.offset(Field::FrameLocalsplus);
        match awaited {
            Some(awaited) => {
                self.memory.write(values, awaited, 8);
                self.word(frame, Field::FrameStackpointer, values + 8);
            }
            None => self.word(frame, Field::FrameStackpointer, values),
        }
        coroutine
    }
}

/// A thread state of interpreter 0, whose state is `interpreter`, running
/// on native thread `native`, whose own state is at `address`.
const fn state(address: u64, interpreter: u64, native: u64) -> ThreadState {
    ThreadState {
        address: VirtualAddress::new(address),
        interpreter_state: VirtualAddress::new(interpreter),
        interpreter: 0,
        native,
        frame: None,
        base: None,
    }
}

fn tasks(process: &Process, types: Types, states: &[ThreadState]) -> Tasks {
    with_values(process, types, InspectionLimits::default(), |values| {
        values.asyncio_tasks(states)
    })
}

fn awaits(process: &Process, types: Types, task: u64) -> Awaits {
    with_values(process, types, InspectionLimits::default(), |values| {
        let mut code = CodeTable::default();
        let mut walk = Walk {
            reader: values.reader,
            code: &mut code,
            code_type: VirtualAddress::new(process.code_type),
        };
        values.awaits(&mut walk, VirtualAddress::new(task))
    })
}

/// A 3.14 model of `process`, whose runtime state is at `runtime`.
fn model(process: &Process, types: &Types, runtime: u64) -> super::super::PythonRuntime {
    super::super::PythonRuntime {
        free_threaded: false,
        entries: Vec::new(),
        evaluation: Vec::new(),
        version: crate::image::python::PyVersion(0x030e_00f0),
        reading: Ok(super::super::Reading {
            runtime: crate::AddressRange {
                start: crate::ImageAddress::new(runtime),
                end: crate::ImageAddress::new(runtime + 8),
            },
            layout: process.layout.clone(),
            code_type: crate::ImageAddress::new(process.code_type),
            types: Ok(types.images()),
            secret: None,
            code: std::sync::Mutex::new(CodeTable::default()),
        }),
    }
}

fn addresses(tasks: &[u64]) -> Vec<VirtualAddress> {
    tasks.iter().copied().map(VirtualAddress::new).collect()
}

#[test]
fn each_thread_lists_its_tasks_then_its_interpreter_those_of_exited_threads() {
    let (mut process, types, ty) = world(TASK_SIZE);
    process.memory.threads = vec![ThreadId::new(7), ThreadId::new(8)];
    let name = process.str_object(types.str, "worker");
    let [a, b, c, d] = [0; 4].map(|_| process.task(ty, 0, name));
    let interpreter = process.alloc(STATE_SIZE);
    let first = process.alloc(STATE_SIZE);
    let second = process.alloc(STATE_SIZE);
    for (owner, field, list) in [
        (first, Field::ThreadAsyncioTasksHead, &[a, b][..]),
        (second, Field::ThreadAsyncioTasksHead, &[c][..]),
        (interpreter, Field::InterpreterAsyncioTasksHead, &[d][..]),
    ] {
        let head = process.empty_list(owner, field);
        process.link(head, list);
    }
    process.word(second, Field::ThreadAsyncioRunningTask, c);

    let found = tasks(
        &process,
        types,
        &[state(first, interpreter, 7), state(second, interpreter, 8)],
    );
    assert!(found.gaps.is_empty(), "{:?}", found.gaps);
    assert_eq!(found.all, addresses(&[a, b, c, d]));
    assert_eq!(
        found.running.into_iter().collect::<Vec<_>>(),
        [(VirtualAddress::new(c), Ok(ThreadId::new(8)))]
    );
}

#[test]
fn a_list_that_loops_or_holds_no_task_keeps_the_tasks_before_and_says_why() {
    let (mut process, types, ty) = world(TASK_SIZE);
    let name = process.str_object(types.str, "worker");
    let [a, b] = [0; 2].map(|_| process.task(ty, 0, name));
    let interpreter = process.alloc(STATE_SIZE);
    let looping = process.alloc(STATE_SIZE);
    let head = process.empty_list(looping, Field::ThreadAsyncioTasksHead);
    process.link(head, &[a, b]);
    let node = process.offset(Field::TaskNode);
    process.memory.write(b + node, a + node, 8);

    let foreign = process.alloc(STATE_SIZE);
    let list = process.object(types.list, TASK_SIZE);
    let head = process.empty_list(foreign, Field::ThreadAsyncioTasksHead);
    process.link(head, &[list]);
    process.empty_list(interpreter, Field::InterpreterAsyncioTasksHead);

    let found = tasks(
        &process,
        types,
        &[
            state(looping, interpreter, 7),
            state(foreign, interpreter, 8),
        ],
    );
    assert_eq!(found.all, addresses(&[a, b]));
    let gaps: Vec<&str> = found.gaps.iter().map(AsRef::as_ref).collect();
    assert_eq!(
        gaps,
        [
            "the list of Python native thread 7 tasks loops".to_owned(),
            format!(
                "the list of Python native thread 8 tasks holds {list:#x}, which is no asyncio.Task"
            ),
        ]
    );
}

#[test]
fn a_type_named_task_whose_objects_are_another_size_is_none() {
    let (mut process, types, ty) = world(TASK_SIZE + 8);
    let name = process.str_object(types.str, "worker");
    let task = process.task(ty, 0, name);
    let is_task = with_values(&process, types, InspectionLimits::default(), |values| {
        values.is_task(VirtualAddress::new(task))
    });
    assert_eq!(is_task, Ok(false));
}

#[test]
fn a_task_is_named_by_its_text_or_until_python_asks_by_its_number() {
    let (mut process, types, ty) = world(TASK_SIZE);
    let text = process.str_object(types.str, "worker");
    let number = process.int(types.int, 12);
    let named = process.task(ty, 0, text);
    let numbered = process.task(ty, 0, number);
    let read = with_values(&process, types, InspectionLimits::default(), |values| {
        [named, numbered].map(|task| values.task_name(VirtualAddress::new(task)))
    });
    assert_eq!(read, [Ok("worker".to_owned()), Ok("Task-12".to_owned())]);
}

#[test]
fn a_task_no_thread_runs_is_done_waiting_or_ready_and_names_its_awaiters() {
    let (mut process, types, ty) = world(TASK_SIZE);
    let future = process.type_object("_asyncio.Future");
    let names =
        ["done", "waits", "ready", "awaited"].map(|name| process.str_object(types.str, name));
    let [done, waits, ready, awaited] = names.map(|name| process.task(ty, 0, name));
    process.put(done, Field::TaskState, 2, 4);
    let waiting_for = process.object(future, 32);
    process.word(waits, Field::TaskFutWaiter, waiting_for);
    process.word(ready, Field::TaskAwaitedBy, awaited);
    process.word(awaited, Field::TaskFutWaiter, ready);

    let read = with_values(&process, types, InspectionLimits::default(), |values| {
        [done, waits, ready, awaited].map(|task| values.task(VirtualAddress::new(task), None))
    });
    let shown: Vec<_> = read
        .iter()
        .map(|task| {
            (
                task.state.clone(),
                task.detail.as_deref(),
                task.labels.clone(),
            )
        })
        .collect();
    let label = |key: &str, value: &str| -> (Arc<str>, Arc<str>) { (key.into(), value.into()) };
    assert_eq!(
        shown,
        [
            (TaskState::Exited, None, vec![label("name", "done")]),
            (
                TaskState::Blocked,
                Some("awaits _asyncio.Future"),
                vec![label("name", "waits")]
            ),
            (
                TaskState::Runnable,
                None,
                vec![label("name", "ready"), label("awaited_by", "awaited")]
            ),
            (
                TaskState::Blocked,
                Some("awaits task ready"),
                vec![label("name", "awaited")]
            ),
        ]
    );
    assert_eq!(read[0].number, done, "a task's number is its id()");
}

#[test]
fn a_suspended_task_is_the_frames_of_its_chain_of_awaits_innermost_first() {
    let (mut process, types, ty) = world(TASK_SIZE);
    let future = process.type_object("_asyncio.Future");
    let waiting_for = process.object(future, 32);
    let [outer, middle, inner] = [("outer", 10), ("middle", 20), ("inner", 30)]
        .map(|(name, line)| process.code(name, line, &[(1, 0)]));
    let inner = process.coroutine(
        &types,
        Constant::GeneratorYieldFrom,
        inner,
        Some(waiting_for),
    );
    let middle = process.coroutine(&types, Constant::GeneratorYieldFrom, middle, Some(inner));
    let outer = process.coroutine(&types, Constant::GeneratorYieldFrom, outer, Some(middle));
    let name = process.str_object(types.str, "chained");
    let task = process.task(ty, outer, name);

    let read = awaits(&process, types, task);
    assert_eq!(read.end, None);
    let frames: Vec<_> = read
        .frames
        .iter()
        .map(|frame| {
            (
                frame.function.as_deref(),
                frame.place.as_ref().map(|place| place.line),
                frame.outermost,
            )
        })
        .collect();
    assert_eq!(
        frames,
        [
            (Some("inner"), Some(30), false),
            (Some("middle"), Some(20), false),
            (Some("outer"), Some(10), true),
        ]
    );
    assert_eq!(read.frames[0].frame, VirtualAddress::new(inner + GEN_FRAME));
}

#[test]
fn a_chain_ends_at_a_coroutine_not_yet_started_or_resting_at_a_bare_yield() {
    let (mut process, types, ty) = world(TASK_SIZE);
    let name = process.str_object(types.str, "task");
    for state in [Constant::GeneratorCreated, Constant::GeneratorSuspended] {
        let code = process.code("rests", 5, &[(1, 0)]);
        let resting = process.coroutine(&types, state, code, None);
        let task = process.task(ty, resting, name);
        let read = awaits(&process, types, task);
        assert_eq!(read.end, None, "{state:?}");
        assert_eq!(read.frames.len(), 1, "{state:?}");
    }
}

#[test]
fn a_chain_that_loops_runs_or_runs_no_coroutine_ends_with_why() {
    let (mut process, types, ty) = world(TASK_SIZE);
    let name = process.str_object(types.str, "task");
    let code = process.code("again", 5, &[(1, 0)]);
    let looping = process.coroutine(&types, Constant::GeneratorYieldFrom, code, None);
    let values = looping + GEN_FRAME + process.offset(Field::FrameLocalsplus);
    process.memory.write(values, looping, 8);
    process.word(looping + GEN_FRAME, Field::FrameStackpointer, values + 8);
    let task = process.task(ty, looping, name);
    let read = awaits(&process, types, task);
    assert_eq!(
        read.end.as_deref(),
        Some("the task's coroutines await each other in a loop")
    );
    assert_eq!(read.frames.len(), 1);

    let running = process.coroutine(&types, Constant::GeneratorExecuting, code, None);
    let task = process.task(ty, running, name);
    let read = awaits(&process, types, task);
    assert_eq!(
        read.end,
        Some(
            format!("the coroutine at {running:#x} is running, on no thread asyncio names").into()
        )
    );
    assert!(read.frames.is_empty());

    let list = process.object(types.list, 32);
    let task = process.task(ty, list, name);
    let read = awaits(&process, types, task);
    assert_eq!(
        read.end.as_deref(),
        Some("the task runs a `list`, which is no coroutine")
    );
}

#[test]
fn a_frame_whose_stack_pointer_lies_below_what_it_awaits_ends_the_chain() {
    let (mut process, types, ty) = world(TASK_SIZE);
    let name = process.str_object(types.str, "task");
    let code = process.code("broken", 5, &[(1, 0)]);
    let broken = process.coroutine(&types, Constant::GeneratorYieldFrom, code, None);
    let task = process.task(ty, broken, name);
    let read = awaits(&process, types, task);
    let frame = broken + GEN_FRAME;
    let top = frame + process.offset(Field::FrameLocalsplus);
    assert_eq!(
        read.end,
        Some(
            format!("the frame at {frame:#x} has its stack pointer at {top:#x}, outside its stack")
                .into()
        )
    );
    assert_eq!(read.frames.len(), 1, "the frame itself is read");
}

#[test]
fn running_tasks_use_the_proven_target_thread_instead_of_a_colliding_host_id() {
    let (mut process, types, ty) = world(TASK_SIZE);
    let name = process.str_object(types.str, "worker");
    let task = process.task(ty, 0, name);
    let interpreter = process.alloc(STATE_SIZE);
    let thread = process.alloc(STATE_SIZE);
    let head = process.empty_list(thread, Field::ThreadAsyncioTasksHead);
    process.link(head, &[task]);
    process.empty_list(interpreter, Field::InterpreterAsyncioTasksHead);
    process.word(thread, Field::ThreadAsyncioRunningTask, task);
    let host = ThreadId::new(5000);
    let collision = ThreadId::new(7);
    process.memory.threads = vec![collision, host];
    process
        .memory
        .target_ids
        .insert(host, crate::runtime_model::TargetThreadId::new(7));
    process
        .memory
        .target_ids
        .insert(collision, crate::runtime_model::TargetThreadId::new(8));
    let found = tasks(&process, types, &[state(thread, interpreter, 7)]);
    assert!(found.gaps.is_empty(), "{:?}", found.gaps);
    let snapshot = with_values(&process, types, InspectionLimits::default(), |values| {
        values.task(
            VirtualAddress::new(task),
            found.running.get(&VirtualAddress::new(task)),
        )
    });
    assert_eq!(snapshot.state, TaskState::Running);
    assert_eq!(snapshot.thread, Some(host));
}

#[test]
fn a_running_task_keeps_its_state_when_thread_identity_is_unproven_or_ambiguous() {
    for duplicate in [false, true] {
        let (mut process, types, ty) = world(TASK_SIZE);
        let name = process.str_object(types.str, "worker");
        let task = process.task(ty, 0, name);
        let interpreter = process.alloc(STATE_SIZE);
        let thread = process.alloc(STATE_SIZE);
        let head = process.empty_list(thread, Field::ThreadAsyncioTasksHead);
        process.link(head, &[task]);
        process.empty_list(interpreter, Field::InterpreterAsyncioTasksHead);
        process.word(thread, Field::ThreadAsyncioRunningTask, task);
        process.memory.threads = vec![ThreadId::new(7), ThreadId::new(5000)];
        for host in process.memory.threads.clone() {
            process.memory.target_ids.insert(
                host,
                duplicate.then(|| crate::runtime_model::TargetThreadId::new(7).unwrap()),
            );
        }
        let found = tasks(&process, types, &[state(thread, interpreter, 7)]);
        assert!(!found.gaps.is_empty());
        let snapshot = with_values(&process, types, InspectionLimits::default(), |values| {
            values.task(
                VirtualAddress::new(task),
                found.running.get(&VirtualAddress::new(task)),
            )
        });
        assert_eq!(snapshot.state, TaskState::Running);
        assert_eq!(snapshot.thread, None);
        assert!(
            snapshot
                .detail
                .as_deref()
                .is_some_and(|detail| detail.contains(if duplicate {
                    "ambiguous"
                } else {
                    "no proven"
                }))
        );
    }
}

#[test]
fn a_task_reported_as_running_twice_has_no_guessed_owner() {
    let (mut process, types, ty) = world(TASK_SIZE);
    let name = process.str_object(types.str, "worker");
    let task = process.task(ty, 0, name);
    let interpreter = process.alloc(STATE_SIZE);
    let first = process.alloc(STATE_SIZE);
    let second = process.alloc(STATE_SIZE);
    for thread in [first, second] {
        let head = process.empty_list(thread, Field::ThreadAsyncioTasksHead);
        if thread == first {
            process.link(head, &[task]);
        }
        process.word(thread, Field::ThreadAsyncioRunningTask, task);
    }
    process.empty_list(interpreter, Field::InterpreterAsyncioTasksHead);
    process.memory.threads = vec![ThreadId::new(7), ThreadId::new(8)];
    let found = tasks(
        &process,
        types,
        &[state(first, interpreter, 7), state(second, interpreter, 8)],
    );
    assert!(found.running[&VirtualAddress::new(task)].is_err());
    let snapshot = with_values(&process, types, InspectionLimits::default(), |values| {
        values.task(
            VirtualAddress::new(task),
            found.running.get(&VirtualAddress::new(task)),
        )
    });
    assert_eq!(snapshot.state, TaskState::Running);
    assert_eq!(snapshot.thread, None);
    assert!(
        snapshot
            .detail
            .as_deref()
            .is_some_and(|detail| detail.contains("multiple thread states"))
    );
    assert!(
        found
            .gaps
            .iter()
            .any(|gap| gap.contains("multiple thread states"))
    );
}

#[test]
fn current_task_and_context_use_proven_identity_and_refuse_missing_proof() {
    use crate::runtime_model::{RuntimeModel as _, TaskContext, TaskRef, ThreadActivity};
    let (mut process, types, ty) = world(TASK_SIZE);
    process.layout.set_field(Field::ThreadSize, STATE_SIZE);
    let name = process.str_object(types.str, "worker");
    let task = process.task(ty, 0, name);
    let thread = process.thread_state(0, 0, 7);
    let interpreter = process.interpreter(0, 0, thread);
    let runtime = process.runtime(interpreter);
    let head = process.empty_list(thread, Field::ThreadAsyncioTasksHead);
    process.link(head, &[task]);
    process.empty_list(interpreter, Field::InterpreterAsyncioTasksHead);
    process.word(thread, Field::ThreadAsyncioRunningTask, task);
    let host = ThreadId::new(5000);
    let collision = ThreadId::new(7);
    process.memory.threads = vec![collision, host];
    process
        .memory
        .target_ids
        .insert(host, crate::runtime_model::TargetThreadId::new(7));
    process
        .memory
        .target_ids
        .insert(collision, crate::runtime_model::TargetThreadId::new(8));
    let model = model(&process, &types, runtime);
    assert_eq!(
        model.thread_activity(&process.memory, host),
        ThreadActivity::Task {
            number: task,
            stack: crate::StackSegment::Thread
        }
    );
    assert_eq!(
        model.thread_activity(&process.memory, collision),
        ThreadActivity::Outside
    );
    let reference = TaskRef {
        number: task,
        locator: Some(task),
    };
    assert!(
        matches!(model.task_context(&process.memory, reference), Ok(Some(TaskContext::OnThread(thread))) if thread == host)
    );
    process.memory.target_ids.insert(host, None);
    assert!(matches!(
        model.thread_activity(&process.memory, host),
        ThreadActivity::Unknown(_)
    ));
    assert!(model.task_context(&process.memory, reference).is_err());
}

/// A task's context is read from the task and the thread states alone, so
/// a snapshot that asks for each task's reads each list once: a task is
/// one while its node is linked between neighbors that link back to it,
/// and with a thread state's running task unknown, which task it runs is.
#[test]
fn a_tasks_context_is_its_own_node_and_the_running_tasks() {
    use crate::runtime_model::{RuntimeModel as _, TaskContext, TaskRef};
    let (mut process, types, ty) = world(TASK_SIZE);
    process.layout.set_field(Field::ThreadSize, STATE_SIZE);
    let code = process.code("leaf", 1, &[(1, 0)]);
    let resting = process.coroutine(&types, Constant::GeneratorCreated, code, None);
    let listed = process.task(ty, resting, 0);
    let other = process.task(ty, resting, 0);
    let unlinked = process.task(ty, resting, 0);
    let more: Vec<u64> = (0..64).map(|_| process.task(ty, resting, 0)).collect();
    let thread = process.thread_state(0, 0, 7);
    let interpreter = process.interpreter(0, 0, thread);
    let runtime = process.runtime(interpreter);
    let head = process.empty_list(thread, Field::ThreadAsyncioTasksHead);
    process.link(head, &[listed, other]);
    process.empty_list(interpreter, Field::InterpreterAsyncioTasksHead);
    process.memory.threads = vec![ThreadId::new(7)];
    let model = model(&process, &types, runtime);
    let context = |process: &Process, task: u64| {
        model.task_context(
            &process.memory,
            TaskRef {
                number: task,
                locator: Some(task),
            },
        )
    };

    assert!(matches!(
        context(&process, listed),
        Ok(Some(TaskContext::Interpreted { .. }))
    ));
    assert!(matches!(context(&process, unlinked), Ok(None)));
    assert!(matches!(context(&process, types.str.get()), Ok(None)));

    // However many tasks the list holds, the context reads as much.
    let reads = |process: &Process| {
        process.memory.reads.set(0);
        let _ = context(process, listed);
        process.memory.reads.get()
    };
    let few = reads(&process);
    process.link(head, &[&[listed, other][..], &more].concat());
    assert_eq!(reads(&process), few);
    process.link(head, &[listed, other]);

    // A node whose neighbors do not link back to it is no proof.
    let node = process.offset(Field::TaskNode);
    let next = process.offset(Field::LlistNext);
    process.memory.write(other + node + next, listed + node, 8);
    let broken = context(&process, other);
    assert!(
        broken
            .as_ref()
            .is_err_and(|problem| problem.contains("neighbors")),
        "{broken:?}"
    );
    process.memory.write(other + node + next, head, 8);

    // A thread whose running task cannot be read may run any task.
    process.word(thread, Field::ThreadAsyncioRunningTask, types.str.get());
    let unknown = context(&process, listed);
    assert!(
        unknown
            .as_ref()
            .is_err_and(|problem| problem.contains("no asyncio.Task")),
        "{unknown:?}"
    );
}
