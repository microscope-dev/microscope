//! CPython, as the runtime that runs a program's Python code. The module
//! that carries it, `libpython` or an executable that links it, has the
//! layout its loader read (`crate::image::python`); this model reads the
//! interpreters, their threads, and each thread's frames through it at a
//! stop.
//!
//! Python frames are no native frames: the evaluation loop runs them, and
//! each of its activations runs a segment of a thread's chain of frames,
//! which the loop's entry frame ends. A backtrace places each segment
//! before the activation that runs it ([`RuntimeModel::driven`]).
//!
//! The model keeps one cache across stops, the process's code objects
//! ([`code::CodeTable`]), and proves each row again where it is used.

mod asyncio;
mod code;
mod expressions;
mod frames;
mod gil;
mod instances;
mod library;
mod linetable;
mod numpy;
mod objects;
mod text;
mod threads;
mod values;

use std::collections::BTreeSet;
use std::sync::{Arc, Mutex};

use super::{
    Crossing, Driven, DynamicValue, InterpretedChild, InterpretedFailure, InterpretedLocal,
    InterpretedName, InterpretedOperand, InterpretedSlot, InterpretedStacks, InterpretedStep,
    InterpretedThread, Partial, RuntimeException, RuntimeHook, RuntimeImage, RuntimeModel,
    RuntimeSignals, RuntimeStop, StartedTask, StopMemory, StoredValue, TaskContext, TaskPage,
    TaskRef, TaskStarter, ThreadActivity, ThreadLabels, Unbound,
};
use crate::demangle::split_from;
use crate::image::python::{EVALUATION_LOOP, Field, PyVersion, PythonLayout, TAIL_CALL_PREFIX};
use crate::inspection::InspectionBudget;
use crate::unwind::RegisterFile;
use crate::{
    AddressRange, CodeRole, ImageAddress, StackFrame, StackSegment, ThreadId, VirtualAddress,
};

/// The CPython runtime a module carries. One whose frames cannot be read,
/// as one of a version microscope does not know, still finds its loop's
/// activations, where a backtrace says why their frames are missing.
pub fn detect(
    image: &Arc<dyn RuntimeImage + Send + Sync>,
) -> Option<Result<Arc<dyn RuntimeModel>, Unbound>> {
    let runtime = image.python()?;
    let reading = runtime
        .layout
        .and_then(|layout| {
            let code_type = image.symbol("PyCode_Type").ok_or_else(|| -> Arc<str> {
                format!("CPython {} names no PyCode_Type", runtime.version).into()
            })?;
            Ok(Reading {
                runtime: runtime.runtime,
                layout,
                code_type: code_type.address,
                types: types(image.as_ref(), runtime.version),
                secret: image.symbol("_Py_HashSecret").map(|symbol| symbol.address),
                code: Mutex::default(),
            })
        })
        .map_err(|reason| format!("the Python frames cannot be read: {reason}").into());
    Some(Ok(Arc::new(PythonRuntime {
        version: runtime.version,
        reading,
    })))
}

/// The symbols CPython exports the objects the model reads values by as:
/// the type objects of the built-in types it presents, and the function
/// that reads attributes as `object` does.
const TYPES: [&str; 22] = [
    "_PyNone_Type",
    "PyBool_Type",
    "PyLong_Type",
    "PyFloat_Type",
    "PyUnicode_Type",
    "PyBytes_Type",
    "PyList_Type",
    "PyTuple_Type",
    "PyDict_Type",
    "PySet_Type",
    "PyFrozenSet_Type",
    "PyCell_Type",
    "PyModule_Type",
    "PyFunction_Type",
    "PyCFunction_Type",
    "PyCoro_Type",
    "PyGen_Type",
    "PyCode_Type",
    "PyType_Type",
    "PyMemberDescr_Type",
    "PyWrapperDescr_Type",
    "PyObject_GenericGetAttr",
];

/// The objects [`TYPES`] names, in its order; or why the model cannot read
/// values.
fn types(
    image: &dyn RuntimeImage,
    version: PyVersion,
) -> Result<[ImageAddress; TYPES.len()], Arc<str>> {
    let names = TYPES;
    let mut types = [ImageAddress::new(0); TYPES.len()];
    for (ty, name) in types.iter_mut().zip(names) {
        *ty = image
            .symbol(name)
            .ok_or_else(|| format!("CPython {version} names no {name}"))?
            .address;
    }

    Ok(types)
}

#[derive(Debug)]
struct PythonRuntime {
    version: PyVersion,

    /// What reads its frames and values, or why nothing can.
    reading: Result<Reading, Arc<str>>,
}

impl PythonRuntime {
    fn reading(&self) -> Result<&Reading, Arc<str>> {
        self.reading.as_ref().map_err(Arc::clone)
    }

    /// What reads the values expressions compute with, or why nothing can.
    fn expressions(&self) -> Result<&Reading, InterpretedFailure> {
        self.reading().map_err(|reason| {
            InterpretedFailure::Unavailable(crate::VariableUnavailableReason::Interpreted(reason))
        })
    }
}

/// What reads a runtime whose layout is known.
#[derive(Debug)]
struct Reading {
    /// CPython's runtime state, `_PyRuntime`, which its `.PyRuntime`
    /// section holds.
    runtime: AddressRange<ImageAddress>,
    layout: PythonLayout,
    code_type: ImageAddress,

    /// The built-in types' objects, in the order [`TYPES`] names them.
    types: Result<[ImageAddress; TYPES.len()], Arc<str>>,

    /// The secret `str` hashes are keyed by, `_Py_HashSecret`, where the
    /// module exports it.
    secret: Option<ImageAddress>,
    code: Mutex<code::CodeTable>,
}

impl Reading {
    /// The stop's values, read through `budget`, and the code table.
    fn values<R>(
        &self,
        stop: &dyn RuntimeStop,
        budget: &mut InspectionBudget,
        read: impl FnOnce(&values::Values<'_, '_>, &mut code::CodeTable) -> Result<R, Arc<str>>,
    ) -> Result<R, Arc<str>> {
        self.read_values(stop, Some(budget), read)
    }

    /// The stop's values, their reads paid for from `budget`, or unpaid
    /// for as a backtrace's are, which bounds its own work, and the code
    /// table.
    fn read_values<R>(
        &self,
        stop: &dyn RuntimeStop,
        budget: Option<&mut InspectionBudget>,
        read: impl FnOnce(&values::Values<'_, '_>, &mut code::CodeTable) -> Result<R, Arc<str>>,
    ) -> Result<R, Arc<str>> {
        let symbols = self.types.as_ref().map_err(Arc::clone)?;
        let bias = stop.load_bias();
        let loaded = |symbol: ImageAddress| {
            bias.load(symbol)
                .ok_or_else(|| Arc::from("CPython's types lie outside the process"))
        };
        let [
            none,
            bool,
            int,
            float,
            str,
            bytes,
            list,
            tuple,
            dict,
            set,
            frozenset,
            cell,
            module,
            function,
            builtin,
            coroutine,
            generator,
            code,
            class,
            member,
            wrapper,
            generic_getattr,
        ] = symbols.map(loaded);
        let types = values::Types {
            none: none?,
            bool: bool?,
            int: int?,
            float: float?,
            str: str?,
            bytes: bytes?,
            list: list?,
            tuple: tuple?,
            dict: dict?,
            set: set?,
            frozenset: frozenset?,
            cell: cell?,
            module: module?,
            function: function?,
            builtin: builtin?,
            coroutine: coroutine?,
            generator: generator?,
            code: code?,
            class: class?,
            member: member?,
            wrapper: wrapper?,
            generic_getattr: generic_getattr?,
        };
        let runtime = loaded(self.runtime.start)?;

        let charged = budget.is_some();
        let mut unpaid = InspectionBudget::new(crate::InspectionLimits::default());
        let budget = values::Budget::new(budget.unwrap_or(&mut unpaid));
        // Blocks read at the stop are kept while it lasts, where the debugger
        // keeps them, and while this read lasts otherwise.
        let read_now = StopMemory::default();
        let values = values::Values {
            reader: objects::Reader {
                stop,
                layout: &self.layout,
                charge: charged.then_some(&budget as &dyn objects::Charge),
                blocks: Some(stop.memory().unwrap_or(&read_now)),
            },
            types,
            budget: &budget,
            secret: self.secret.and_then(|secret| bias.load(secret)),
            hashing: std::cell::Cell::new(None),
            runtime,
            classes: std::cell::RefCell::default(),
        };
        let mut table = self
            .code
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        read(&values, &mut table)
    }

    /// What an expression's `read` of the stop's values gives.
    fn interpreted<R>(
        &self,
        stop: &dyn RuntimeStop,
        budget: &mut InspectionBudget,
        read: impl FnOnce(
            &values::Values<'_, '_>,
            &mut code::CodeTable,
        ) -> Result<R, InterpretedFailure>,
    ) -> Result<R, InterpretedFailure> {
        self.values(stop, budget, |values, table| Ok(read(values, table)))
            .map_err(|problem| {
                InterpretedFailure::Unavailable(crate::VariableUnavailableReason::Interpreted(
                    problem,
                ))
            })?
    }

    /// What an expression's `read` of the frame at `frame`, with the row
    /// of its code, gives.
    fn interpreted_frame<R>(
        &self,
        stop: &dyn RuntimeStop,
        frame: VirtualAddress,
        budget: &mut InspectionBudget,
        read: impl FnOnce(
            &values::Values<'_, '_>,
            &code::CodeTable,
            code::CodeId,
            &objects::Record<'_>,
        ) -> Result<R, InterpretedFailure>,
    ) -> Result<R, InterpretedFailure> {
        let code_type = stop.load_bias().load(self.code_type).ok_or_else(|| {
            InterpretedFailure::Unavailable(crate::VariableUnavailableReason::Interpreted(
                "CPython's code type lies outside the process".into(),
            ))
        })?;
        self.interpreted(stop, budget, |values, table| {
            let record = values
                .reader
                .record(frame, Field::FrameSize)
                .map_err(|problem| values.unavailable(problem))?;
            let (_, code) = frames::Walk {
                reader: values.reader,
                code: table,
                code_type,
            }
            .code_of(&record)
            .map_err(|problem| values.unavailable(problem))?;
            read(values, table, code, &record)
        })
    }

    /// What `read` gives of asyncio's tasks, given the values, the walk
    /// that reads frames, and every task, read unpaid for, as a
    /// backtrace's frames are, which bounds its own work.
    fn asyncio<R>(
        &self,
        stop: &dyn RuntimeStop,
        read: impl FnOnce(&values::Values<'_, '_>, &mut frames::Walk<'_, '_>, &asyncio::Tasks) -> R,
    ) -> Result<R, Arc<str>> {
        let code_type = stop
            .load_bias()
            .load(self.code_type)
            .ok_or_else(|| Arc::from("CPython's code type lies outside the process"))?;
        self.read_values(stop, None, |values, table| {
            let (states, gaps) = threads::thread_states(&values.reader, values.runtime)?;
            let mut tasks = values.asyncio_tasks(&states);
            tasks.gaps.splice(0..0, gaps);
            let mut walk = frames::Walk {
                reader: values.reader,
                code: table,
                code_type,
            };
            Ok(read(values, &mut walk, &tasks))
        })
    }

    fn stacks(&self, stop: &dyn RuntimeStop, version: PyVersion) -> Partial<InterpretedStacks> {
        let mut stacks = InterpretedStacks::default();
        let mut gaps = Vec::new();
        let reader = objects::Reader {
            stop,
            layout: &self.layout,
            charge: None,
            blocks: None,
        };

        let bias = stop.load_bias();
        let (Some(runtime), Some(code_type)) =
            (bias.load(self.runtime.start), bias.load(self.code_type))
        else {
            gaps.push("CPython's runtime state lies outside the process".into());
            return Partial {
                value: stacks,
                gaps,
            };
        };

        let mut states = match threads::thread_states(&reader, runtime) {
            Ok((states, problems)) => {
                gaps.extend(problems);
                states
            }
            Err(problem) => {
                gaps.push(format!("CPython {version}'s threads cannot be read: {problem}").into());
                Vec::new()
            }
        };

        states.sort_by_key(|state| (state.native, state.interpreter));
        let threads: BTreeSet<ThreadId> = stop.threads().into_iter().collect();
        walk_threads(
            &mut frames::Walk {
                reader,
                code: &mut self
                    .code
                    .lock()
                    .unwrap_or_else(std::sync::PoisonError::into_inner),
                code_type,
            },
            &states,
            &threads,
            &mut stacks,
            &mut gaps,
        );
        stacks.check_invariants();
        Partial {
            value: stacks,
            gaps,
        }
    }
}

impl Reading {
    /// Each thread of the stop that holds its interpreter's GIL, or from
    /// 3.15 asks for it, labeled `gil=held` or `gil=waiting`.
    fn thread_labels(
        &self,
        stop: &dyn RuntimeStop,
        version: PyVersion,
    ) -> Partial<Vec<ThreadLabels>> {
        let mut labels = Vec::new();
        let mut gaps = Vec::new();
        let reader = objects::Reader {
            stop,
            layout: &self.layout,
            charge: None,
            blocks: None,
        };

        let Some(runtime) = stop.load_bias().load(self.runtime.start) else {
            gaps.push("CPython's runtime state lies outside the process".into());
            return Partial {
                value: labels,
                gaps,
            };
        };

        let states = match threads::thread_states(&reader, runtime) {
            Ok((states, problems)) => {
                gaps.extend(problems);
                states
            }
            Err(problem) => {
                gaps.push(format!("CPython {version}'s threads cannot be read: {problem}").into());
                return Partial {
                    value: labels,
                    gaps,
                };
            }
        };

        let threads: BTreeSet<ThreadId> = stop.threads().into_iter().collect();
        let key: Arc<str> = "gil".into();
        for (state, gil) in states.iter().zip(gil::gil_states(&reader, &states)) {
            let thread = ThreadId::new(state.native);
            if !threads.contains(&thread) {
                continue;
            }

            match gil {
                Ok(gil) => {
                    if let Some(value) = gil.and_then(gil::Gil::label) {
                        labels.push(ThreadLabels {
                            thread,
                            labels: vec![(Arc::clone(&key), value.into())],
                        });
                    }
                }
                Err(problem) => gaps.push(
                    format!("thread {thread}'s hold on the GIL cannot be read: {problem}").into(),
                ),
            }
        }

        labels.sort_by_key(|labels| labels.thread);
        Partial {
            value: labels,
            gaps,
        }
    }
}

/// Adds the frames of each thread state that runs on one of the stop's
/// `threads` to `stacks`, and why others' are missing to `gaps`.
fn walk_threads(
    walk: &mut frames::Walk<'_, '_>,
    states: &[threads::ThreadState],
    threads: &BTreeSet<ThreadId>,
    stacks: &mut InterpretedStacks,
    gaps: &mut Vec<Arc<str>>,
) {
    for state in states.iter().filter(|state| state.frame.is_some()) {
        let thread = ThreadId::new(state.native);
        if !threads.contains(&thread) {
            gaps.push(
                format!(
                    "a Python thread runs on native thread {}, which is none of the \
                         process's threads",
                    state.native
                )
                .into(),
            );
            continue;
        }

        let (frames, end) = walk.thread(state);
        if frames.is_empty() {
            gaps.extend(end);
            continue;
        }

        let first = stacks.frames.len();
        stacks.frames.extend(frames);
        stacks.threads.push(InterpretedThread {
            thread,
            interpreter: state.interpreter,
            frames: first..stacks.frames.len(),
            end,
        });
    }
}

impl RuntimeModel for PythonRuntime {
    /// asyncio's tasks, which only 3.14 and later list.
    fn tasks(
        &self,
        stop: &dyn RuntimeStop,
        start: u64,
        limit: usize,
        _program_only: bool,
    ) -> Partial<TaskPage> {
        let empty = |gaps| Partial {
            value: TaskPage {
                tasks: Vec::new(),
                next: None,
            },
            gaps,
        };
        let reading = match self.reading() {
            Ok(reading) if reading.layout.has(Field::TaskNode) => reading,
            Ok(_) => return empty(Vec::new()),
            Err(reason) => {
                return empty(vec![
                    format!("asyncio's tasks cannot be read: {reason}").into(),
                ]);
            }
        };

        let first = usize::try_from(start).unwrap_or(usize::MAX);
        let page = reading.asyncio(stop, |values, _, tasks| {
            let page: Vec<_> = tasks
                .all
                .iter()
                .skip(first)
                .take(limit)
                .map(|&task| values.task(task, tasks.running.get(&task).copied()))
                .collect();
            let end = first.saturating_add(page.len());
            Partial {
                value: TaskPage {
                    tasks: page,
                    next: (end < tasks.all.len()).then_some(end as u64),
                },
                gaps: tasks.gaps.clone(),
            }
        });
        page.unwrap_or_else(|problem| {
            empty(vec![
                format!("asyncio's tasks cannot be read: {problem}").into(),
            ])
        })
    }

    /// The asyncio task a thread runs, which its state names from 3.14.
    fn thread_activity(&self, stop: &dyn RuntimeStop, thread: ThreadId) -> ThreadActivity {
        let reading = match self.reading() {
            Ok(reading) if reading.layout.has(Field::ThreadAsyncioRunningTask) => reading,
            _ => return ThreadActivity::Outside,
        };

        let running = reading.read_values(stop, None, |values, _| {
            let (states, _) = threads::thread_states(&values.reader, values.runtime)?;
            for state in states.iter().filter(|state| state.native == thread.get()) {
                if let Some(task) = values.running_task(state)? {
                    return Ok(Some(task));
                }
            }

            Ok(None)
        });
        match running {
            Ok(Some(task)) => ThreadActivity::Task {
                number: task.get(),
                stack: StackSegment::Thread,
            },
            Ok(None) => ThreadActivity::Outside,
            Err(reason) => ThreadActivity::Unknown(reason),
        }
    }

    /// A task a thread runs begins on the thread; one no thread runs is
    /// its chain of awaits.
    fn task_context(
        &self,
        stop: &dyn RuntimeStop,
        task: TaskRef,
    ) -> Result<Option<TaskContext>, Arc<str>> {
        let reading = self.reading()?;
        if !reading.layout.has(Field::TaskNode) {
            return Ok(None);
        }

        let object = VirtualAddress::new(task.number);
        reading.asyncio(stop, |values, walk, tasks| {
            if !tasks.all.contains(&object) {
                // A task missing from lists that cannot all be read may
                // be in the rest.
                if tasks.gaps.is_empty() {
                    return Ok(None);
                }

                let gaps: Vec<&str> = tasks.gaps.iter().map(AsRef::as_ref).collect();
                return Err(gaps.join("; ").into());
            }

            if let Some(&thread) = tasks.running.get(&object) {
                return Ok(Some(TaskContext::OnThread(thread)));
            }

            let awaits = values.awaits(walk, object);
            Ok(Some(TaskContext::Interpreted {
                frames: awaits.frames.into(),
                end: awaits.end,
            }))
        })?
    }

    fn thread_stacks(
        &self,
        _stop: &dyn RuntimeStop,
        _thread: ThreadId,
    ) -> Result<Vec<(std::ops::Range<u64>, StackSegment)>, Arc<str>> {
        Ok(Vec::new())
    }

    fn cross(
        &self,
        _stop: &dyn RuntimeStop,
        _thread: ThreadId,
        _frame: &RegisterFile,
        _after_call: bool,
    ) -> Result<Crossing, Arc<str>> {
        Ok(Crossing::Stay)
    }

    fn signals(&self) -> RuntimeSignals {
        RuntimeSignals::default()
    }

    fn hooks(&self) -> &[RuntimeHook] {
        &[]
    }

    fn exception(
        &self,
        _stop: &dyn RuntimeStop,
        _hook: ImageAddress,
        _registers: &RegisterFile,
    ) -> Result<RuntimeException, Arc<str>> {
        Err("CPython reports no exceptions to the debugger".into())
    }

    fn dynamic_value(
        &self,
        _stop: &dyn RuntimeStop,
        _representation: &str,
        _value: StoredValue<'_>,
    ) -> Option<Result<DynamicValue, Arc<str>>> {
        None
    }

    fn stack_mover(&self) -> Option<ImageAddress> {
        None
    }

    fn moving_task(
        &self,
        _stop: &dyn RuntimeStop,
        _registers: &RegisterFile,
    ) -> Result<u64, Arc<str>> {
        Err("CPython moves no task".into())
    }

    fn task_stack(
        &self,
        _stop: &dyn RuntimeStop,
        _task: TaskRef,
    ) -> Result<Option<std::ops::Range<u64>>, Arc<str>> {
        Ok(None)
    }

    fn call_out(
        &self,
        _entry: ImageAddress,
        _registers: &RegisterFile,
    ) -> Option<Result<VirtualAddress, Arc<str>>> {
        None
    }

    fn task_starters(&self) -> Vec<TaskStarter> {
        Vec::new()
    }

    fn started_task(
        &self,
        _stop: &dyn RuntimeStop,
        _starter: ImageAddress,
        _registers: &RegisterFile,
    ) -> Result<Option<StartedTask>, Arc<str>> {
        Ok(None)
    }

    fn task_noun(&self) -> &'static str {
        "task"
    }

    fn driver_role(&self) -> Option<CodeRole> {
        Some(CodeRole::Interpreter)
    }

    fn driven(&self, frame: &StackFrame) -> Option<Driven> {
        let name = match (&frame.function, &frame.symbol) {
            (Some(function), _) => &function.name,
            (None, Some(symbol)) => &symbol.name,
            (None, None) => return None,
        };

        let name = split_from(name);
        (name == EVALUATION_LOOP || name.starts_with(TAIL_CALL_PREFIX))
            .then_some(Driven::Interpreted)
    }

    fn interpreted(&self, stop: &dyn RuntimeStop) -> Option<Partial<InterpretedStacks>> {
        Some(match &self.reading {
            Ok(reading) => reading.stacks(stop, self.version),
            Err(reason) => Partial {
                value: InterpretedStacks::default(),
                gaps: vec![Arc::clone(reason)],
            },
        })
    }

    fn thread_labels(&self, stop: &dyn RuntimeStop) -> Partial<Vec<ThreadLabels>> {
        match &self.reading {
            Ok(reading) => reading.thread_labels(stop, self.version),
            Err(reason) => Partial {
                value: Vec::new(),
                gaps: vec![Arc::clone(reason)],
            },
        }
    }

    fn interpreted_locals(
        &self,
        stop: &dyn RuntimeStop,
        frame: VirtualAddress,
        budget: &mut InspectionBudget,
    ) -> Result<Vec<InterpretedLocal>, Arc<str>> {
        let reading = self.reading()?;
        let code_type = stop
            .load_bias()
            .load(reading.code_type)
            .ok_or_else(|| Arc::from("CPython's code type lies outside the process"))?;
        reading.values(stop, budget, |values, table| {
            let record = values.reader.record(frame, Field::FrameSize)?;
            let (_, code) = frames::Walk {
                reader: values.reader,
                code: table,
                code_type,
            }
            .code_of(&record)?;
            values.locals(table, code, &record)
        })
    }

    fn interpreted_children(
        &self,
        stop: &dyn RuntimeStop,
        (object, part): (VirtualAddress, &[u64]),
        range: std::ops::Range<u64>,
        budget: &mut InspectionBudget,
    ) -> Result<Vec<InterpretedChild>, Arc<str>> {
        self.reading()?.values(stop, budget, |values, _| {
            values.children(object, part, range)
        })
    }

    fn interpreted_name(
        &self,
        stop: &dyn RuntimeStop,
        frame: VirtualAddress,
        name: &str,
        budget: &mut InspectionBudget,
    ) -> Result<Option<InterpretedName>, InterpretedFailure> {
        self.expressions()?
            .interpreted_frame(stop, frame, budget, |values, table, code, record| {
                values.name(table, code, record, name)
            })
    }

    fn interpreted_variable(
        &self,
        stop: &dyn RuntimeStop,
        frame: VirtualAddress,
        slot: u32,
        budget: &mut InspectionBudget,
    ) -> Result<VirtualAddress, InterpretedFailure> {
        self.expressions()?
            .interpreted_frame(stop, frame, budget, |values, table, code, record| {
                values.variable(table, code, record, slot)
            })
    }

    fn interpreted_step(
        &self,
        stop: &dyn RuntimeStop,
        object: VirtualAddress,
        taken: InterpretedStep<'_>,
        budget: &mut InspectionBudget,
    ) -> Result<VirtualAddress, InterpretedFailure> {
        self.expressions()?
            .interpreted(stop, budget, |values, _| values.step(object, taken))
    }

    fn interpreted_operand(
        &self,
        stop: &dyn RuntimeStop,
        object: VirtualAddress,
        budget: &mut InspectionBudget,
    ) -> Result<InterpretedOperand, InterpretedFailure> {
        self.expressions()?
            .interpreted(stop, budget, |values, _| values.operand(object))
    }

    fn interpreted_length(
        &self,
        stop: &dyn RuntimeStop,
        object: VirtualAddress,
        budget: &mut InspectionBudget,
    ) -> Result<u64, InterpretedFailure> {
        self.expressions()?
            .interpreted(stop, budget, |values, _| values.length(object))
    }

    fn interpreted_lineage(
        &self,
        stop: &dyn RuntimeStop,
        class: VirtualAddress,
        budget: &mut InspectionBudget,
    ) -> Result<Vec<crate::TypeIdentity>, InterpretedFailure> {
        self.expressions()?.interpreted(stop, budget, |values, _| {
            values
                .lineage(class)
                .map_err(|problem| values.unavailable(problem))
        })
    }

    fn object_types(&self) -> Option<crate::runtime_model::ObjectTypes> {
        Some(crate::runtime_model::ObjectTypes {
            native: "PyObject",
            cast: "py",
        })
    }

    fn interpreted_value(
        &self,
        stop: &dyn RuntimeStop,
        object: VirtualAddress,
        budget: &mut InspectionBudget,
    ) -> InterpretedSlot {
        match self.reading() {
            Ok(reading) => reading
                .values(stop, budget, |values, _| Ok(values.value(object)))
                .unwrap_or_else(InterpretedSlot::Unreadable),
            Err(reason) => InterpretedSlot::Unreadable(reason),
        }
    }
}

#[cfg(test)]
mod tests;
