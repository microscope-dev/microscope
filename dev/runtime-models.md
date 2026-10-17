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
| `src/runtime_model/thread_ids.rs` | Shared proof-based host/target identity resolution, including ambiguity and bounds |
| `src/runtime_model/go/` | Go runtime layout and goroutine discovery |
| `src/runtime_model/tokio/` | Tokio runtime, task, and future discovery |
| `src/runtime_model/futures.rs`, `src/runtime_model/rust/` | Rust future inspection and shared Rust rules |
| `src/runtime_model/python/` | CPython's threads, frames, code objects, line tables, and built-in values |
| `src/runtime_model/python/agent.rs`, `agent.py`, `hook.rs` | Whole-state scripts, monitoring transport, native hook ELF and ABI |
| `src/runtime_model/python/injection.rs` | Bounded PEP 768 request preparation |
| `src/backend/linux/agent_files.rs` | Private target-namespace artifacts and explicit acknowledgement/exit cleanup |
| `src/backend/linux/runtime_agents.rs` | Controller-owned agent installations, generations, acknowledgements, and detach |
| `src/backend/linux/interpreted.rs` | Interpreted frames placed in backtraces, and their variables |
| `src/debug_info/coroutines.rs`, `src/debug_info/roles.rs` | Static coroutine and code-role metadata |
| `src/image/resumes.rs` | Validated resume-point tables |
| `src/backend/linux/runtimes.rs`, `src/backend/linux/async_frames.rs`, `src/backend/linux/frames.rs` | Stop-scoped runtime snapshots and logical stacks |
| `tests/go/`, `tests/tokio/`, `tests/python/` | Runtime integration suites |
| `tests/fixtures/go/`, `tests/fixtures/rust/tokio/`, `tests/fixtures/python/` | Programs that expose scheduler, task, and frame states |
| `views/` | Built-in Go, Rust, Tokio, and Python value views |

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

## Runtime breakpoints

A runtime model names the runtime functions run control keeps a breakpoint
at, which `BreakpointOwner::Runtime` owns and `language_exceptions.rs`
installs:

- `hooks()` lists the functions that report an exception as they are
  entered, such as Go's `gopanic`. Each is armed only while its exception
  filter stops.
- `internal_hooks()` lists functions a runtime calls to tell the debugger
  of its own events. Each stays armed while its runtime is loaded, and at
  every hit `hook_event` reads what the thread reports and says whether it
  stops (`HookEvent`). A hit that goes on is stepped over unseen. A report
  that cannot be read stops with `StopReason::RuntimeEvent`, and says why.
  A hook is at an address in the runtime's own image, or at a function a
  companion image exports (`HookPlace::Companion`). A companion is a
  library the runtime loads beside its own, as Python's agent loads its hook
  library. Its hook is armed in each loaded image that exports the name, and
  goes when the image unloads.

Both are judged when no user breakpoint at the site stops, and before a
step's plan meets the thread. The private sites run control keeps for itself,
such as Go's stack mover, are separate owners.

A managed location is a runtime's line or function rather than a trap
address. `managed.rs` records its owners and arms the runtime with its whole
configuration. Resolution alone does not prove that arming succeeded.
`Breakpoint.pending_locations` exposes each resolved location whose runtime
could not accept arming, without discarding the desired location. Public
replies and snapshots derive these reasons from the controller's current
arming state; `ready_locations()` filters them for clients. A refusal leaves
that runtime's whole current configuration unconfirmed. Earlier instrumentation
may still report hits, which retain their logical ownership and hit policy.
Every whole-state arm, including step setup and cleanup, updates the same
readiness record. Recovery or a changed reason publishes `BreakpointsChanged`, while an unchanged refusal
does not. Disabling a breakpoint excludes it from the arming request.

The CLI, DAP, and browser share the pending explanation in
`src/present/breakpoints.rs`. A partly armed breakpoint retains its ready
locations and reports the others. The browser adds optional `pending`
metadata and lists only ready placements in `places`; absent metadata keeps
older messages and open tabs usable.

## Runtime writes

A runtime model never writes: `RuntimeStop` only reads. A model that needs
the process changed, as installing Python's agent sets fields of a thread
state, returns `RuntimeWrite`s. Each carries the bytes the model read there.
`Controller::write_for_runtime` checks every expected byte before making the
first write, while every thread is stopped. A kernel write failure can leave
earlier bytes changed; its error reports the partial progress. A caller must
retain any artifacts a partially published request might still consume until
it proves cancellation, acknowledgement, or target exit. A trap byte is hidden,
as for any write. Every runtime write goes through that one function, which
records it.

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

## asyncio

From 3.14, CPython links every `asyncio.Task` into a circular list in the
state of the thread that made it, and moves the list of an exiting thread
into its interpreter's. A thread's state names the task it runs.
`src/runtime_model/python/asyncio.rs` walks those lists, bounded and checked
for loops, and takes an object as a task only where its type is, or derives
from, the C type `_asyncio.Task` with objects of exactly the size of the
`TaskObj` the layouts copy (`scripts/cpython-layouts.c`). conformance.rs checks
that copy against the `.AsyncioDebug` section `_asyncio` publishes for
external debuggers, which lives in an extension module the Python runtime's
own image does not carry. A task's number is its address, Python's `id()`.

A task no thread runs is suspended in its coroutine: a coroutine or generator
in the state `FRAME_SUSPENDED_YIELD_FROM` awaits the object below the top of
its embedded frame's stack (`Constant::GeneratorAwaitedDepth`: 1, or 2 from
3.15, whose `GET_ITER` pushes an index above it). The chain ends at the first
object no coroutine is, such as a future, or at a coroutine resting at its
start or a bare `yield`. Its frames, innermost first, are the task's
`TaskContext::Interpreted`, which the backend roots as
`RootOrigin::Interpreted`: each frame resolves as an interpreted frame
(`FrameScope::Interpreted`), so locals, expressions, and children work as in
a thread's Python frames, and a reason the chain ends early is the trace's
`BrokenAwaitChain`. A running task is `TaskContext::OnThread`.

`tests/fixtures/python/tasks.py` reports every task `asyncio.all_tasks()`
lists in each loop, with its state, what it awaits, who awaits it
(`capture_call_graph`), and each coroutine's frame and locals by `cr_await`;
`tests/python/tasks.rs` compares them on 3.14 and 3.15, with and without the
GIL.

## CPython

CPython inspection reads the interpreter from outside, as a core dump allows,
and puts no code in the process. It covers CPython 3.12 through 3.15, with the
GIL and free-threaded. The monitoring agent below is a separate transport for
execution control; inspection and profiling do not load it.

**Injection preparation.** `python/injection.rs` computes PEP 768 writes from
one stopped state and a preference-ordered list of threads the caller has
proved execute Python, with both controller and target-namespace thread IDs.
The caller first proves the build supports PEP 768; the runtime configuration
flag controls policy and cannot establish compiled support for custom embedders.
Every write carries its expected bytes. The preparer
uses the caller's read budget, with a bounded default when none is supplied,
and refuses finalization, malformed control spans, reserved flags, and multiple
interpreters, including ones without thread states. It passes over a thread
with an occupied request or an uncleared script slot, which CPython never
clears, and returns that reason only when no candidate is free. Read limits and unavailable memory have distinct outcomes;
the caller decides when to retry. Initialization and a lack of eligible threads
are retryable waits. The caller owns outstanding generations across all threads
and must await acknowledgement before clearing a previous slot or preparing the
next request. Candidate eligibility must exclude CPython's request-consumption
and path-copy window.

**Agent transport.** `python/agent.py` translates explicitly configured
`sys.monitoring` probes into calls to `microscope_python_hook`.
`python/agent.rs` builds standalone whole-state scripts; the source executes
once in `_microscope_agent` in `sys.modules`, so fresh script namespaces share
one generation and installation. Atomic `dict.setdefault` publishes a module
with its lock before source initialization; that lock serializes initialization
and configuration. The atomic publication relies on CPython's guarantee for
[built-in string dictionary keys](https://docs.python.org/3.14/builtins/threadsafety.html#thread-safety-for-dict-objects).
Older or identical generations from the same library owner do nothing.
A replacement owner starts a new generation sequence. Library loading occurs
outside the configuration lock, avoiding an inversion with the dynamic loader.
An empty configuration frees the tool ID, its local/global events, and its
callbacks. No tool is registered while idle. The agent prefers ID 0, tries
other free IDs, and reports exhaustion without changing another tool.

The transport's flat probes specify an event, report kind, number, and exact
file/line/qualified-name selectors. The runtime supplies event policy;
conditions, hit counts, expressions, step ownership, and exception judgments
remain debugger work. `PY_START`, `PY_RESUME`, and `PY_THROW` discover code
objects; local events apply only to matching code, including frames running
at installation and generators
or coroutines that were already suspended when configuration changed.
A free-threaded build never reads another thread's frames, which that thread
can change or free concurrently. Its collector tracks code objects, so
configuration instead takes every live code object from `gc.get_objects()`,
which lists them while every thread is at a safe point, and instruments the
matching ones, running or not. That costs about 30–50 ns per live object, once
per configuration. A build whose collector does not track code refuses to load
the agent. Freshly instrumented code first clears events a freed tool left
on it, which would otherwise keep that tool's disabled locations disabled.
With the GIL, frames running at installation are walked directly.
Discovery keeps weak references keyed by code identity, because code equality
can ignore the filename. Each code object is instrumented once per generation,
and zero masks require no instrumentation call. Callbacks never return
`DISABLE`: CPython can process an older callback's return after a newer
configuration, disabling its locations. Freeing the old tool ID resets its
local-event version, so configuration needs no global `restart_events()` and
avoids global reactivation of other tools' disabled locations. CPython can
reactivate their disabled locations on code microscope instruments with the
same event; their registrations and event masks remain intact. Unrelated code
stays disabled. While armed, discovery still takes
one Python callback per function entry, resumption, or thrown-in exception;
matching code takes local callbacks at each enabled event, including
nonmatching lines. Idle unregisters everything.

`python/hook.rs` generates a small ELF shared library, including its export
and unwind information. It needs no target compiler or libpython. Its dynamic
section is writable, because glibc before 2.35 relocates it in place. Its function
is a no-op without a debugger. At entry its three unsigned 64-bit arguments
are in rdi/rsi/rdx: kind, number, argument. Kind 0 acknowledges a generation
(argument 0–5 is the tool ID; 6 means idle), 1 reports a breakpoint, 2 a step's
completion (both require a nonzero number and zero argument), and 3 reports an
error (argument 1: tool exhaustion; 2: callback/configuration failure with
confirmed disarming; 3: cleanup failed and prior state is unconfirmed;
4: initialization failed and any previous installation is unconfirmed;
5: observable user reentry makes monitoring coverage unconfirmed;
6: a user interruption is reported and re-raised, with the agent disarmed). Error
reports retain the failed generation. Malformed reports produce a visible
runtime stop. The frame comes from the stopped thread's interpreter state.

The library also exports a bounded 64 KiB command slot and a sequence reader
that uses one aligned native 64-bit load. The header carries magic, sequence,
payload length and capacity; payloads are JSON whole configurations. The
controller publishes payload and length before sequence while all threads are
stopped, with one outstanding request until its acknowledgement. Only detach
publishes over an unacknowledged request: its empty configuration is the
last, and nothing awaits it. Callbacks
check that sequence under the configuration lock. Configuration acknowledgements
can immediately accept another command in an iterative loop, so repeated edits
do not grow the Python stack. A malformed command disarms and reports its
generation, including invalid reserved header bytes. Before decoding, a
strict UTF-8 decoding and a string-aware scan bound container nesting to eight levels, and the integer
parser bounds tokens to 20 characters; malformed input cannot exhaust the
recursive decoder or reach Python's integer-string conversion limit. When any
thread replaces the configuration while a callback reports, the callback
reports what its event hits under the replacement and has not reported yet;
sites keep their numbers across configurations. Discovery
failure supersedes an acknowledgement that has not yet been sent. Observable
user reentry during configuration disarms with an explicit coverage failure.
Non-Exception interruptions (including cancellation) are reported and re-raised.
Command decoding distinguishes malformed requests from unexpected exceptions,
so an ordinary exception raised by a user signal handler during decoding is
also preserved. Ordinary faults during instrumentation remain contained and
reported as configuration failures. Delayed interruption and reentry handling
cannot disarm a newer generation. The outer exception boundary captures
interruptions after hooks as well as inside decoding and discovery; a
superseded operation reports unconfirmed cleanup without changing its successor. An idle agent has no callbacks: activating it again requires
source injection. The first source configuration loads a pristine companion
whose published sequence is zero; subsequent commands use its slot. Generations
are monotonic within one library owner, and controller hook binding must
distinguish owners.
The bootstrap reports initialization failures through its request's own library,
even when an incompatible interpreter-local module already exists.
A single lock serializes configuration and callbacks on free-threaded builds,
and captures each report's hook before releasing the lock. That keeps an old
report bound to its original sink if a later owner replaces it. The lock is
released before every native report. Callback failures disarm the agent
and report the failure instead of raising into the program.

**Installation.** `backend/linux/runtime_agents.rs` owns one installation per
runtime: its files, one outstanding generation, and the PEP 768 request's
ticket. An idle agent, or one not yet installed, is configured by a source
request; an armed one through its command slot. Each source request
revalidates the private files through the thread that will read it. A
location stays pending until the matching acknowledgement. A change refused
before any write reaches the target keeps the acknowledged configuration,
which the agent still holds. Acknowledgements must arrive within five seconds
of running time; public and internal stops do not count. A free-threaded
interpreter needs every thread resumed while an agent is configuring or armed,
because monitoring changes stop its world. Detach cancels an unconsumed
request and disarms an armed agent before removing traps; exit and exec
remove the files. Breakpoint, step, and exception policy follow in P4.8–P4.10.
`tests/python/agent.rs` checks the real monitoring API on 3.14/3.15 in both GIL
modes and the native register/all-stop boundary through `Scenario`.

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
by its `native_thread_id` through `RuntimeStop::target_thread_id`. Target
IDs and debugger `ThreadId`s occupy different namespaces. The Linux edge
checks the proc mount's namespace and the thread's process membership before
using the innermost `NSpid`; missing evidence never implies identity. Batched
lookups anchor one proc mount and share its namespace check; the mapping has
an explicit 16384-host-thread resource bound. Published stops retain the
identity evidence with `StopMemory`, invalidated with its memory blocks, so
per-thread activity queries do not repeat proc reads. Cores
read the target ID from the saved thread's TCB with an unambiguous glibc
`_thread_db_pthread_tid` descriptor, resolved once for the immutable core.
Missing descriptors, unreadable TCBs,
and ambiguous mappings produce gaps. A running asyncio task keeps its running
state when its host thread is unknown, with an explanation and no guessed
thread or parked-frame context. The simulator supplies its explicit single
namespace mapping.

Linux stat/status readers decode selected numeric fields from bytes. A kernel
command name can end in a partial UTF-8 character or contain parentheses and
newlines; it must not prevent attach, identity checks, signal handling, detach,
or profiling. Signal queue classification requires both the pending and blocking
masks; a missing or malformed mask is an error, never an unblocked signal.

A thread's chain of frames begins at its current frame (3.12: through its `_PyCFrame`). `frames.rs` walks it innermost first.
Frames the thread owns run code; the interpreter's entry frame ends each
segment that one activation of the evaluation loop runs. The walk ends on a
loop, an unknown owner, or an unreadable frame, and says why. From 3.15 every
chain ends at the thread's base frame, so one that ends before it says it was
read as it changed: a live profile leaves such a thread out.

**The GIL.** `gil.rs` reads who holds each interpreter's GIL, which
`RuntimeModel::thread_labels` gives as `gil=held` and `gil=waiting`. From
3.15 each thread state's `holds_gil` and `gil_requested` say so. Before
that, only the GIL itself does: it is locked, and `last_holder` is the
thread state that took it; nothing says who waits. A free-threaded build's
`enabled` may be 0, and then no thread has a label. The GIL is
`ceval.gil`'s, which may be another interpreter's; the table gives `locked`
and `last_holder` from the start of the interpreter's own copy, so they are
read at their distance from it.

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
the inspection budget. The stop keeps the 256-byte blocks every reading has
read (`StopMemory`, which a write forgets): a page holds whole blocks, so a
block is readable wherever its bytes are, and the fields of one object, a
table's entries, and objects allocated together cost one read for the whole
stop, however many views and expansions read them. `text.rs` writes each value's
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
interpreter's state, found by the index `tp_subclasses` keeps; from 3.13 a
static type of an extension the interpreter manages, as `_datetime`'s, is at
that index among those types' states, where the built-in types' state there
names another type. A class whose bases lead to a type C defines (static,
or a heap type with `Py_TPFLAGS_IMMUTABLETYPE`) other than `object` holds
that type's value, which it is presented as, with its attributes after its
items. A dataclass's fields come first among its attributes, in the order of
its `__dataclass_fields__`. A cell's variable is the object it holds.

`library.rs` reads the standard library's types that only C defines, whose
structures `scripts/cpython-layouts.c` copies: `collections.deque`,
`defaultdict`, and `OrderedDict`, `datetime`'s five types, and
`decimal.Decimal`. A type is one only where C defines it and its module,
name, and `tp_basicsize` are the copied structure's. Each is written as its
type's own C `repr` writes it, with the name of the object's type its C
uses. An `OrderedDict`'s items follow its list of nodes, each of whose key
is found in the dict by identity and the node's hash. A `Decimal` is
written as libmpdec's `mpd_to_sci` writes it with capitals.

`numpy.rs` reads `numpy.ndarray`, whose fields after the object's header
are numpy's public `PyArrayObject_fields`; `Field::ObjectSize` gives where
the header ends. An array is one where its type passes the library test
with a `tp_basicsize` at least that header and its fields. Its dtype's type
number and byte order decide its element: `bool`, the integers, `float32`,
and `float64` are read, and any other dtype, or big-endian elements, makes
the array `Element::Unread`, shown by its shape with the reason as its
problem. An array's rows are parts of it: an `InterpretedValue` carries the
index path (`part`) from the object, and `RuntimeModel::interpreted_children`
takes `(object, part)`, so a row is paged without Python making the row's
view object. An element is an `InterpretedSlot::Number`, a value as the
machine keeps it, which the backend presents as a base type. A value's
`InterpretedArray` gives its data, shape, strides, and element, and
`contiguous()` gives the one run of memory a one-axis array lies in, which
`Controller::numbers` reads at once for a drawing.

Python classes are presented through views as well (`views/python-std.views`;
[Views of dynamic types](evaluation-and-views.md#views-of-dynamic-types)).
`truth.canonical` mirrors each built-in view and each library type's `repr`,
so the fixtures compare every presented value whole.

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
