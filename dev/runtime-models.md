# Runtime models

microscope presents language-level work as tasks and logical stacks without
making the public debugger depend on one runtime. Go goroutines and tokio tasks
use the same public concepts even though their metadata, scheduling, stack
shape, and saved state differ. CPython's frames, which run no machine code of
their own, join native backtraces as interpreted frames.

## Code map

| Path | What lives there |
| --- | --- |
| `src/runtime_model/mod.rs`, `src/runtime_model/records.rs` | Neutral runtime records and discovery interface |
| `src/runtime_model/go/` | Go runtime layout and goroutine discovery |
| `src/runtime_model/tokio/` | Tokio runtime, task, and future discovery |
| `src/runtime_model/futures.rs`, `src/runtime_model/rust/` | Rust future inspection and shared Rust rules |
| `src/runtime_model/python/` | CPython's threads, frames, code objects, line tables, and built-in values |
| `src/backend/linux/interpreted.rs` | Interpreted frames placed in backtraces, and their variables |
| `src/debug_info/coroutines.rs`, `src/debug_info/roles.rs` | Static coroutine and code-role metadata |
| `src/image/resumes.rs` | Validated resume-point tables |
| `src/backend/linux/runtimes.rs`, `src/backend/linux/async_frames.rs`, `src/backend/linux/frames.rs` | Stop-scoped runtime snapshots and logical stacks |
| `tests/go/`, `tests/tokio/`, `tests/python/` | Runtime integration suites |
| `tests/fixtures/go/`, `tests/fixtures/rust/tokio/`, `tests/fixtures/python/` | Programs that expose scheduler, task, and frame states |
| `views/` | Built-in Go, Rust, and Tokio value views |

## Boundary

Runtime support has two inputs:

- Static facts loaded from module bytes: code roles, coroutine descriptions,
  resume points, function relationships, and runtime symbols.
- Dynamic facts read at one stop: runtime instances, task catalogs, task
  state, active-thread relationships, saved contexts, and await chains.

Static facts belong in `ModuleImage`. Dynamic discovery belongs in
`src/runtime_model` and the Linux runtime edge. Clients receive neutral
`RuntimeId`, `TaskId`, `TaskState`, `TaskLocation`, `CoroutineInfo`, and logical
frame records.

Do not expose a Go `g` pointer, tokio task header address, compiler generator
variant, or runtime-private enum as the identity clients must retain. Those are
evidence used to construct a public identity for one session and stop.

## Terms

A **native thread** is an operating-system tracee with registers and a native
stack.

A **task** is a unit of language runtime work: a goroutine or async task. It
may be running on a native thread, parked with saved state, runnable but not
active, or in a runtime-specific terminal state.

A **coroutine** is compiler-generated state that can resume at several source
locations. An async task can contain a chain of coroutines awaiting one
another.

A **runtime instance** is one scheduler or runtime found in the program. A
process may contain more than one supported instance.

A **logical frame** represents source execution in a task even when no native
machine frame currently exists for it.

An **interpreted frame** is a frame an interpreter keeps in its own record,
such as a CPython `_PyInterpreterFrame`. It has a function, file, and line the
interpreter recorded, and variables, but no registers, no address, and no code
in any module image.

## Stop-scoped snapshots

Runtime state is read only at a validated stop. A task catalog and every
relationship within it belong to that `StopId`. Do not carry task pointers,
saved registers, or await links into a later stop.

Task enumeration is paginated and deterministic. The same task must not appear
twice in one snapshot, and a cycle or malformed link must end with a typed
problem rather than loop. Bound memory reads, task count, stack depth, and
runtime instances.

An active task is linked to the native thread that currently runs it. That
relationship can be unknown. Idle scheduler threads have no current task.
Parked tasks have no active native thread.

## Logical stacks

A logical stack combines proven frames from one or more sources:

- native unwind state for a running task;
- saved registers or stack context for a parked stackful task;
- compiler coroutine state and resume points for a suspended stackless task;
- explicit transitions between runtime and user stacks.

Each frame records its kind and evidence. Do not make a logical frame look
native if it has no machine register context. Commands that require registers,
instruction stepping, or arbitrary memory locations must reject a context that
cannot supply them.

Logical unwinding keeps established frames when a later transition fails. The
termination says whether state was unreadable, metadata was absent, a runtime
layout was unsupported, a cycle was found, or a configured bound was reached.

## Code roles

Providers classify known functions and address ranges by role: user code,
runtime scheduling, stack growth, panic machinery, coroutine polling, and
other transitions that affect stepping or unwinding. Generic run control uses
the role, not a hard-coded runtime symbol.

Code roles are evidence, not broad name filters in clients. Add a role when a
generic algorithm needs to make a stable distinction. Keep version-specific
symbol matching in the provider.

The session setting for entering or skipping runtime code applies to these
roles. Skipping must not hide a user function merely because its name happens
to resemble a runtime helper.

## Resume points

Compiler metadata maps a coroutine state to the code location where it can
resume. A resume point is image-relative static data. The runtime model reads
the current state and binds it to a loaded module.

Several states may share a resume location, and a state can be terminal or
otherwise have no resume point. Treat a missing or ambiguous mapping as
unavailable. Do not pick the nearest line or address.

Stepping across an await uses the selected task, coroutine identity, current
activation, and proven resume sites. The execution plan remains owned by that
task. Another task reaching the same user code does not complete it.

## Go

Go support combines pclntab, DWARF, known runtime symbols, and live runtime
structures.

The runtime model locates goroutines, assigns task state, and determines which
goroutine each native thread runs. A parked goroutine exposes a saved scheduler
context. A running goroutine begins with the native thread's registers.

Go stacks can grow and move. Unwinding recognizes stack-transition functions
and switches between goroutine and system stacks only with the required saved
context. A stack boundary is not permission to scan memory for a plausible
return address.

The top of a goroutine stack can contain runtime frames that have no useful
source-level meaning. Frame presentation and stepping use code roles to skip
them under the default policy while preserving them when runtime entry is
requested.

Panic and fatal-error handling uses runtime stop evidence to publish neutral
language exceptions. Keep raised, uncaught, and terminal distinctions where
the runtime can prove them.

Go runtime layouts change. Version dispatch belongs in the provider, with a
fixture for each supported family and an explicit unsupported result for an
unknown one.

## Rust coroutines and tokio

Rust async functions compile to state-machine values. DWARF describes their
captured fields and compiler-generated variants. Static loading identifies
coroutine functions, relationships, and resume points without depending on
tokio.

tokio support then discovers runtimes and task storage, reads task state, and
finds each task's root future. Walking nested future fields reconstructs the
await chain. The walker follows type and layout facts; it does not scan the
heap for values that look like futures.

A task can be running, scheduled, idle, completed, or in a state the current
runtime provider does not understand. A running task can join its logical
future chain to a native worker stack. A parked task may have only its future
chain and no native registers.

`tokio_unstable` can provide extra facts such as spawn locations. Treat those
as optional enrichment. Core task identity, state, and stack behavior must
state clearly when they require unstable runtime instrumentation.

Pinned and boxed futures, trait-object erasure, combinators, and compiler
layout changes are common sources of partial chains. Each traversal step needs
a concrete type and storage relationship. Stop where proof ends.

## CPython

CPython support reads the interpreter from outside, as a core dump allows: it
puts no code in the process. It covers CPython 3.12 through 3.15, with the GIL
and free-threaded.

**Layout.** The module that carries the runtime, `libpython` or an executable
that links it, has a `.PyRuntime` section. From 3.13 its `_Py_DebugOffsets`
table gives most field offsets; `debug_info::cpython` reads it by name under a
schema per minor version (`schemas.rs`) and completes it from a layout
generated from that version's headers (`layouts.gen.rs`). 3.12 has no table, so
only a release whose generated layout microscope has checked is read. Any other
version is refused by name. The layout, its constants, and why it cannot be
read are stored in the image's `PythonRuntime` record (`src/image/python.rs`),
so the runtime model never sees a version number. This is the stated exception
to Go's "from DWARF by name, never from a table of versions" rule:
generated constants cover only what the target's own table leaves out.
[Debug information](debug-information.md#cpython) describes how each layout is
checked.

**Binding.** `runtime_model::python::detect` binds every module that carries a
runtime. A runtime whose layout is unknown is still bound, without a reader: it
finds its loop's activations and reports why their frames are missing, so a
backtrace says so instead of silently showing native frames only.

**Threads and frames.** At a stop, the model walks `_PyRuntime`'s interpreters
and each one's thread states (`threads.rs`), and maps each to a native thread
by its `native_thread_id`. A thread's chain of frames begins at its current
frame (3.12: through its `_PyCFrame`). `frames.rs` walks it innermost first.
Frames the thread owns run code; the interpreter's entry frame ends each
segment that one activation of the evaluation loop runs. The walk ends on a
loop, an unknown owner, or an unreadable frame, and says why.

**Code and lines.** Each frame's code object is decoded once per runtime into
`code::CodeTable`: its qualified name, file, first line, and line table. The
table survives stops, so each row is proved again where it is used: the object
must still be a code object with the same identity. `linetable.rs` decodes
`co_linetable` to the line of the frame's instruction. A free-threaded 3.14+
frame may run a thread-local copy of the bytecode, which `tlbc_index` selects.

**Splicing.** `RuntimeModel::driven` recognizes the activations of the
evaluation loop: `_PyEval_EvalFrameDefault`, or, in a build whose loop
tail-calls a function for each instruction, those functions. `interpreted.rs` places each segment of a thread's frames just
before the activation that runs it, innermost first. Placement is proved by
count: when the thread's segments and activations differ in number, or the
thread runs frames in several interpreters' states, the frames are shown
together on `StackSegment::Unplaced` with the reason, never paired by guess.
A thread that runs interpreted frames is unwound whole to place them, so
frame numbers stay stable.

**Values.** `values.rs` reads the built-in types the model presents: `None`,
`bool`, `int`, `float`, `str`, `bytes`, `list`, `tuple`, `dict`, `set`,
`frozenset`, modules, functions, built-in functions, and code objects. An
object is identified by its type object, which the module's symbols name. Its
first bytes are read at once (`objects::Head`), and each read is charged to
the inspection budget. A reading keeps the 256-byte blocks it has read
(`objects::Blocks`): a page holds whole blocks, so a block is readable
wherever its bytes are, and the fields of one object, a table's entries, and
objects allocated together cost one read. `text.rs` writes each value's
summary as `repr` would. A summary is bounded in depth, items, objects, and
characters, and says where it stopped. The backend presents values as
language-neutral presentations, whose children page through an
`InterpretedChildren` capability; clients see no Python object model.

`instances.rs` reads everything else as CPython's object model lays it out. A
class's identity is its `__module__` and `ht_qualname`, or a static type's
`tp_name`. An instance's attributes are its dict, wherever the class keeps
it: a managed dict before the object, with 3.12's tagged values pointer or
3.13's inline values after it, sharing keys with the class's other instances;
or a dict at a positive `tp_dictoffset`. Its `__slots__` follow, read through
each class's member descriptors. A static built-in type's dict is in its
interpreter's state, found by the index `tp_subclasses` keeps. A class whose
bases lead to a built-in type other than `object` holds that type's value,
which it is presented as, with its attributes after its items. A cell's
variable is the object it holds.

**Interpreted frames in the debugger.** An interpreted frame has
`FrameKind::Interpreted` and a `RecordedFrame` with its function, language,
and place. Locating it is `Error::FrameInterpreted`. Its source context reads
the recorded path. Clients fold the interpreter's frames
(`CodeRole::Interpreter`) between Python frames, with the rule in
`src/present/frames.rs`.

**Expressions.** A name in a Python frame binds as a dynamic value
([Dynamic types](evaluation-and-views.md#dynamic-types)).
`expressions.rs` resolves it as Python does: a variable of the frame's code
(`InterpretedName::Variable`, read at run time from its slot), then the
frame's `f_globals`, then its `f_builtins` (`InterpretedName::Object`, found
at bind time). `src/backend/linux/interpreted_values.rs` adapts the trait's
methods to the evaluator's machine. An item of a list or tuple is found by
index, and a dict's key by CPython's own lookup: the key's hash, then the
table's probe sequence, comparing only the keys of equal hash it passes. A key
of `None`, `bool`, `int`, `float`, `str`, `bytes`, or `tuple` compares by value;
meeting a key of any other type with an equal hash refuses the lookup, since
only its `__eq__` could compare it. A number's hash is computed as CPython
computes it. A `str`'s is SipHash-1-3 keyed by `_Py_HashSecret`, used only once a
`str` key of the dict proves that hashing with the secret gives the hash the
key keeps. Without a known hash, as for a NaN key, every entry is compared,
and a key of another type is ruled out only by the hash its entry keeps.
An attribute is found as `PyObject_GenericGetAttr` finds it, only for a class
whose `tp_getattro` is that function, or whose `__getattribute__` is the
wrapper of it and which defines `__getattr__`, which then refuses a miss: a
data descriptor of the class's MRO first, of which only a member descriptor
(a slot) is read, then the instance's dict, then a class attribute that does
not bind. A module's attribute is in its dict, and a class's in its MRO when
its metaclass is `type`. `x[i]` and `len(x)` on an instance use its built-in
value only where the class's MRO finds the special method on the built-in
type itself; any other definition refuses them, and none is a type error, as
Python's is.

A runtime model whose objects native code also holds names them through
`RuntimeModel::object_types`: the native type a pointer to an object has
(`PyObject`) and the cast that reaches one (`py`). Presentation shows a native
pointer whose pointee's typedef chain reaches that type as the object it points
to, through the same `interpreted_value` path a Python frame's local takes. In
the evaluator, the binder reads `(py)x` as a cast whenever
`Scope::dynamic_type` accepts the name, including where `(py)&x` could
otherwise be a bitwise and, and binds it to `Op::Interpreted`, which runs
`Machine::dynamic_at` on the address. The result is a `Place::Interpreted` like
any Python value, so every operation on Python values applies to it.

## Expressions and values in tasks

Selecting a logical frame changes name binding to that frame's source scope.
Variables stored in a coroutine are read through the compiler-described future
value. Variables that exist only in a native activation use native frame
locations.

`$task` is the selected task ID or the task running on the selected thread. It
is unavailable when no task relationship is known. `$future` is available in
a frame whose coroutine value is known.

Views apply after a task variable has been located and typed. Runtime support
must not bypass the evaluator or synthesize container layouts in client code.

## Run control

Task-aware stepping adds constraints to an ordinary execution plan:

- The selected task owns progress.
- Runtime scheduling and poll frames may be skipped according to policy.
- A suspension can move the task to another native thread.
- Entry breakpoints may be reached by unrelated tasks.
- A task can complete, panic, or disappear before reaching a planned site.

At each candidate stop, identify the current task before deciding whether the
plan completed. If identity is unavailable, return a clear stop or plan
failure; do not let an unrelated thread satisfy it.

Watchpoints on task locals have a similar lifetime problem. The variable's
storage can move when a Go stack grows or a future moves before it is pinned.
Keep the watch scope and invalidation reason explicit.

## Adding another runtime

Start with the neutral behavior the user needs:

1. How are runtime instances found?
2. What is one task's stable identity during a stop?
3. How are tasks enumerated with a hard bound?
4. How is a running task associated with a native thread?
5. What saved state exists for a parked task?
6. How are logical frames proven and variables located?
7. Which code roles affect stepping and exceptions?
8. How do version and build differences fail visibly?

Implement static facts at the debug-information edge and dynamic facts through
the runtime traits. Reuse public tasks, frames, states, and exception records.
Add a public concept only when the new runtime has a user-visible fact that
cannot be represented honestly.

## Testing runtime support

Runtime tests need synchronized programs whose task topology is known. A
fixture should expose barriers for creation, running, parking, waking,
migration, panic, and exit. The test waits for those barriers; it does not
sleep and hope the scheduler reached one.

Check invariants at every relevant stop:

- task IDs are unique and deterministic within the snapshot;
- active task and native thread relationships agree;
- logical stacks end at the expected root or state a reason;
- frame variables come from the selected activation;
- stepping follows one task across scheduling;
- task completion and runtime shutdown clean every plan and cache;
- unsupported versions fail as unsupported, not as empty task lists;
- work stays within task, read, depth, and time bounds.

Use a version matrix only where runtime or compiler output changes behavior.
Run runtime integration tests, DAP and web task tests, stress, soak where
applicable, and the long simulator sweep for run-control changes.

The detailed historical rationale and producer facts remain in
`plans/go.md` and `plans/tokio.md`.
