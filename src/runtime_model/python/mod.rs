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

mod code;
mod frames;
mod linetable;
mod objects;
mod text;
mod threads;
mod values;

use std::collections::BTreeSet;
use std::sync::{Arc, Mutex};

use super::{
    Crossing, Driven, DynamicValue, InterpretedChild, InterpretedLocal, InterpretedStacks,
    InterpretedThread, Partial, RuntimeException, RuntimeHook, RuntimeImage, RuntimeModel,
    RuntimeSignals, RuntimeStop, StartedTask, StoredValue, TaskContext, TaskPage, TaskRef,
    TaskStarter, ThreadActivity, Unbound,
};
use crate::demangle::split_from;
use crate::image::python::{EVALUATION_LOOP, Field, PyVersion, PythonLayout, TAIL_CALL_PREFIX};
use crate::inspection::InspectionBudget;
use crate::unwind::RegisterFile;
use crate::{
    AddressRange, CodeRole, ImageAddress, StackFrame, StackSegment, ThreadId, VirtualAddress,
};

/// The CPython runtime a module carries, when its layout is known.
pub fn detect(
    image: &Arc<dyn RuntimeImage + Send + Sync>,
) -> Option<Result<Arc<dyn RuntimeModel>, Unbound>> {
    let runtime = image.python()?;
    let unbound = |reason: String| Unbound {
        reason: format!("the Python frames cannot be read: {reason}").into(),
        deferrable: &[],
    };

    let layout = match runtime.layout {
        Ok(layout) => layout,
        Err(reason) => return Some(Err(unbound(reason.to_string()))),
    };

    let Some(code_type) = image.symbol("PyCode_Type") else {
        return Some(Err(unbound(format!(
            "CPython {} names no PyCode_Type",
            runtime.version
        ))));
    };

    Some(Ok(Arc::new(PythonRuntime {
        version: runtime.version,
        runtime: runtime.runtime,
        layout,
        code_type: code_type.address,
        types: types(image.as_ref(), runtime.version),
        code: Mutex::default(),
    })))
}

/// The type objects of the built-in types whose values the model reads,
/// by the symbols CPython exports them as; or why it cannot read values.
fn types(image: &dyn RuntimeImage, version: PyVersion) -> Result<[ImageAddress; 9], Arc<str>> {
    let names = [
        "_PyNone_Type",
        "PyBool_Type",
        "PyLong_Type",
        "PyFloat_Type",
        "PyUnicode_Type",
        "PyBytes_Type",
        "PyList_Type",
        "PyTuple_Type",
        "PyDict_Type",
    ];
    let mut types = [ImageAddress::new(0); 9];
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

    /// CPython's runtime state, `_PyRuntime`, which its `.PyRuntime`
    /// section holds.
    runtime: AddressRange<ImageAddress>,
    layout: PythonLayout,
    code_type: ImageAddress,

    /// The built-in types' objects, in the order [`types`] names them.
    types: Result<[ImageAddress; 9], Arc<str>>,
    code: Mutex<code::CodeTable>,
}

impl PythonRuntime {
    /// The stop's values, read through `budget`, and the code table.
    fn values<R>(
        &self,
        stop: &dyn RuntimeStop,
        budget: &mut InspectionBudget,
        read: impl FnOnce(&values::Values<'_, '_>, &mut code::CodeTable) -> Result<R, Arc<str>>,
    ) -> Result<R, Arc<str>> {
        let symbols = self.types.as_ref().map_err(Arc::clone)?;
        let bias = stop.load_bias();
        let loaded = |symbol: ImageAddress| {
            bias.load(symbol)
                .ok_or_else(|| Arc::from("CPython's types lie outside the process"))
        };
        let [none, bool, int, float, str, bytes, list, tuple, dict] = symbols.map(loaded);
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
        };

        let budget = values::Budget::new(budget);
        let values = values::Values {
            reader: objects::Reader {
                stop,
                layout: &self.layout,
                charge: Some(&budget),
            },
            types,
            budget: &budget,
        };
        let mut table = self
            .code
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        read(&values, &mut table)
    }

    fn stacks(&self, stop: &dyn RuntimeStop) -> Partial<InterpretedStacks> {
        let mut stacks = InterpretedStacks::default();
        let mut gaps = Vec::new();
        let reader = objects::Reader {
            stop,
            layout: &self.layout,
            charge: None,
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
                gaps.push(
                    format!(
                        "CPython {}'s threads cannot be read: {problem}",
                        self.version
                    )
                    .into(),
                );
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
    fn tasks(
        &self,
        _stop: &dyn RuntimeStop,
        _start: u64,
        _limit: usize,
        _program_only: bool,
    ) -> Partial<TaskPage> {
        Partial {
            value: TaskPage {
                tasks: Vec::new(),
                next: None,
            },
            gaps: Vec::new(),
        }
    }

    fn thread_activity(&self, _stop: &dyn RuntimeStop, _thread: ThreadId) -> ThreadActivity {
        ThreadActivity::Outside
    }

    fn task_context(
        &self,
        _stop: &dyn RuntimeStop,
        _task: TaskRef,
    ) -> Result<Option<TaskContext>, Arc<str>> {
        Ok(None)
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
        Some(self.stacks(stop))
    }

    fn interpreted_locals(
        &self,
        stop: &dyn RuntimeStop,
        frame: VirtualAddress,
        budget: &mut InspectionBudget,
    ) -> Result<Vec<InterpretedLocal>, Arc<str>> {
        let code_type = stop
            .load_bias()
            .load(self.code_type)
            .ok_or_else(|| Arc::from("CPython's code type lies outside the process"))?;
        self.values(stop, budget, |values, table| {
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
        object: VirtualAddress,
        range: std::ops::Range<u64>,
        budget: &mut InspectionBudget,
    ) -> Result<Vec<InterpretedChild>, Arc<str>> {
        self.values(stop, budget, |values, _| values.children(object, range))
    }
}

#[cfg(test)]
mod tests;
