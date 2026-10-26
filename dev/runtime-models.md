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
| `src/runtime_model/node/` | V8's frames, scripts, objects, and values in Node, and `util.inspect`'s text |
| `src/debug_info/v8.rs`, `src/debug_info/v8/`, `src/image/v8.rs` | V8 detection and the layouts generated from each version's Torque output and headers |
| `src/backend/linux/agent_files.rs` | Private target-namespace artifacts and explicit acknowledgement/exit cleanup |
| `src/backend/linux/runtime_agents.rs` | Controller-owned agent installations, generations, acknowledgements, and detach |
| `src/backend/linux/interpreted.rs` | Interpreted frames placed in backtraces, and their variables |
| `src/debug_info/coroutines.rs`, `src/debug_info/roles.rs` | Static coroutine and code-role metadata |
| `src/image/resumes.rs` | Validated resume-point tables |
| `src/backend/linux/runtimes.rs`, `src/backend/linux/async_frames.rs`, `src/backend/linux/frames.rs` | Stop-scoped runtime snapshots and logical stacks |
| `src/backend/linux/holdings.rs` | Process-lifetime numbers for the thread labels, such as `isolate 1` |
| `tests/go/`, `tests/tokio/`, `tests/python/`, `tests/node/` | Runtime integration suites |
| `tests/fixtures/go/`, `tests/fixtures/rust/tokio/`, `tests/fixtures/python/`, `tests/fixtures/node/` | Programs that expose scheduler, task, and frame states |
| `views/` | Built-in Go, Rust, Tokio, Python, and JavaScript value views |

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

## Thread labels

`ThreadSnapshot::labels` holds what runtimes say of each stopped thread,
such as `isolate 1` for the V8 isolate a Node thread runs. The CLI's
`threads`, the debug adapter's thread names, and the browser's Threads pane
show them. Any runtime model can fill them.

`RuntimeModel::thread_holding` returns the runtime resource a thread uses:
a kind (`"isolate"`) and an opaque `HeldIdentity`, such as its address.
It returns `None` when there is no resource, or an error when the identity
cannot be read; the label then shows the error. Identities must be unique
among live resources but may be reused after a resource is destroyed.

The backend turns identities into numbers, per runtime and kind, in the
table `Inferior::holdings`. The table lasts for the life of the process, so
a restart or a new process starts again at 1. All threads are numbered at
once, the first time a stop is asked for labels, and the result is cached in
`PublicStop::labels`. At each numbering:

- The table forgets an identity once no live thread was last seen holding
  it. A thread whose runtime could not be read keeps what it held. If the
  same address appears later, it gets a new number. A number is never
  given out twice.
- New identities are numbered in the order of their threads' ids, so Node's
  main thread, which has the lowest id, is `isolate 1`.
- A copy V8 made of its builtins binds the runtime of the module it copies
  a second time. Labels skip that binding, so each thread gets each label
  once.

`RuntimeModel::thread_labels` is a separate method. It gives the profiler
key/value text per sample, such as `gil=held`, and needs no identity across
stops.

Node reads the thread-local `g_current_isolate_`. Node enters the main
thread's isolate, and each worker's, for the whole of its event loop, so a
thread that runs JavaScript always has its isolate's label. The value is 0
on libuv's pool and the inspector. From V8 13.6 (Node 24), a `LocalHeap`
on any other thread also sets it (`heap/local-heap.cc`), so V8's platform
workers have the isolate's label while they compile or mark for it, and a
label does not mean the thread runs JavaScript. Telling those workers apart
would need `g_current_local_heap_` and `LocalHeap::is_main_thread_`, which
Debian's libnode does not name. The variable is a thread-local, so cores
show labels too.

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
An agent's acknowledged sites are the exception: while a newer configuration
awaits acknowledgement, the agent holds one configuration or the other, so a
location both name stays ready (`Installation::confirms`). Only a failure
report or a timeout makes them pending, so an editor does not gray out every
breakpoint while a step's command is acknowledged.
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

### Compiler and tokio versions

A debugger built with one toolchain meets programs built by many, so neither
rustc's output nor tokio's internals may be assumed to match the pinned ones.
The rules that keep this working:

- **Names rustc and std choose live in `src/rust_names.rs`.** Each lists every
  spelling a supported release uses, with the release that changed it, and
  callers try them all. Examples: `drop_in_place` became `drop_glue` in 1.97,
  `rust_panic` became `__rustc::rust_panic` in 1.87, and the thread-local
  `VAL` became `__RUST_STD_INTERNAL_VAL` in 1.92. When a release renames one,
  add the new spelling and keep the old one.
- **What DWARF does not say is recovered from rustc's naming, not guessed.**
  Before 1.93 an async body's parameters do not name its coroutine. Loading
  links `{async_fn#N}` to the one `{async_fn_env#N}` of the same namespace and
  arguments; copies of that type in several units are one type. At a stop, the body's
  future is found at its place in the await chain of the future that the
  runtime frame polling it holds (`RuntimeModel::polling` for a task, or a
  `block_on` driver), and only when every frame between that frame and the
  body agrees. A future a hand-written executor polls has no such frame, so
  `$future` and await steps stay unavailable there.
- **Legacy mangling numbers no closure.** Function paths matched against a
  linkage name take a trailing `{{closure}}`'s number from the debug
  information's own name, as tokio's poller and `block_on` driver are
  closures.
- **Legacy-mangled symbols name generic parameters, not arguments.** Before
  1.97 a vtable's method symbol reads `T`; the method's DWARF generics supply
  the argument.
- **A Rust typedef is a synonym.** Nightlies after 1.97 emit `usize` and
  `isize` as typedefs of `u64` and `i64`.
- **std views match paths with `**`** where std moves modules: `Rc` and `Arc`
  moved under `alloc::rcs` after 1.97.
- **tokio's layout binds by member name, per shape.** tokio 1.54 moved the
  blocking queue from `Inner.shared` to the `Inner.inner_impl` enum (`Locked`
  or `Sharded`), and the model binds whichever shape a program has. tokio 1.51
  to 1.54 are in `VERIFIED` in `tokio/mod.rs`; another release is read as the
  newest verified one and every page says so.

CI builds the fixtures with the pinned toolchain and tokio 1.52 and 1.53 only.
Other versions were checked by hand: build the tokio fixtures with another
`RUSTC` (keeping the pinned cargo) or another tokio lockfile, then run the
tokio suite with the pinned microscope. Rerun that check when a stable Rust
or tokio release changes code generation or tokio's runtime internals.

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

A task's context is read without walking the lists, because a snapshot asks
for the context of each task, and walking them every time would cost
O(tasks²). A task is listed while its node lies between neighbours that link
back to it: CPython links a task when it makes it and clears the node when
it frees it. It runs on the thread whose state names it. When some thread
state's running task can't be read, the task's context is an error, because
that thread may be running it.

A task not running on a thread is suspended in its coroutine. A coroutine or
generator in `FRAME_SUSPENDED_YIELD_FROM` awaits an object on its embedded
frame's stack. `Constant::GeneratorAwaitedDepth` locates it: 1, or 2 from
3.15, where `GET_ITER` pushes an index above it. The await chain ends at a
non-coroutine object, such as a future, or a coroutine at its start or a bare
`yield`. The task's `TaskContext::Interpreted` lists its frames innermost first.
The backend uses `RootOrigin::Interpreted` and `FrameScope::Interpreted`, so
locals, expressions, and children work as in thread frames. A prematurely
ended chain carries `BrokenAwaitChain`. Running tasks use `TaskContext::OnThread`.

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

**Launch bootstrap (3.12 and 3.13).** These versions have no PEP 768
request, so `PythonRuntime::bootstrapped` makes `inject` return a request
with no writes and `bootstrapped` set, and `eligible_thread` names any thread.
The controller hands the script's path to the launch's bootstrap
(`backend/linux/python_launch.rs`), and refuses the request when there is no
bootstrap.

When the executable is CPython 3.12 or 3.13, `LinuxTraceOps::python_launch`
prepares one before the spawn. The version comes from a `libpython3.12` or
`libpython3.13` the executable needs, or from its own `Py_Version`. The
bootstrap is a private `/tmp/.microscope-python-*` directory that holds:

- `sitecustomize.py`, from `python/launch.py`;
- a FIFO, which microscope holds open for reading and writing;
- `pid`, the process the launch made, written before it runs.

The launch prepends the directory to `PYTHONPATH`, and names the FIFO and
the original `PYTHONPATH` in `MICROSCOPE_PYTHON_WAKE` and
`MICROSCOPE_PYTHON_PATH`. The bootstrap first removes its directory from
`sys.path`, and restores both variables unless `MICROSCOPE_PYTHON_FOLLOW`
is `1`. A launch whose group follows children sets it, so the variables
reach child interpreters. A process the group adopts that runs 3.12 or
3.13 gets a FIFO of its own, `wake-PID` in the same directory, made before
it runs (`LinuxTraceOps::python_join`); a forked interpreter's bootstrap
reopens its own FIFO at fork. Only in the process named in `pid`, or one
whose `wake-PID` exists, does it then go on:

- It records the main thread's ident as `_microscope_bootstrap.main`, since
  3.12 has no `_thread._get_main_thread_ident`.
- It runs, on the main thread, each script already written. The first
  arming happens at the loader stop, before `site` runs, so pending
  breakpoints are armed before the program's code.
- It starts one `_thread` thread, which names itself `microscope`, blocks
  reading the FIFO, and runs each later script with fresh globals, as PEP
  768 does. Its frames, like the agent's, have `CodeRole::Interpreter`
  (`agent::is_bootstrap_filename`).

It then imports the `sitecustomize` it hid. A request is one line, written
whole; a full FIFO is refused. One that is retired without acknowledgement
is read back out of the FIFO (`withdraw_bootstrap`). One the bootstrap
already took stays unconfirmed, since it may yet run. A thread-scoped
resume is refused while one is outstanding, since only the bootstrap's
thread takes it. An unacknowledged request times out with
`BOOTSTRAP_TIMED_OUT`, which names `-I`, `-E`, and `-S`. A step back is
refused, since it cannot run on its own thread, so the native step runs on.

Before 3.14, `free_tool_id` leaves a tool's callbacks and events, so `clear`
removes them first. 3.12's function `f_locals` is a copy, so assignments are
refused there. 3.13t immortalizes, and so stops listing, every existing code
object once a second thread starts, so `gc.get_objects` cannot find running
code. The model and the agent both refuse it.

`tests/python/bootstrap.rs` checks that the program and its child see their
own `PYTHONPATH`, `sys.path`, and `sitecustomize`, and that a breakpoint set
while the program runs reaches the hidden thread. The breakpoint and step
suites run on 3.12 and 3.13, and the agent's fixture modes run there too.

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

The transport's flat probes specify an event, report kind, number, and
file/line/qualified-name selectors. A qualified name matches itself or its
last dotted parts, so `area` matches `Shape.area` and `outer.<locals>.area`
but `rea` matches neither. A relative path matches filenames that end with its
whole components. An absolute path matches its own filename exactly, or an
absolute filename with the same final component that `os.path.realpath`
resolves to the same file; each such comparison is resolved once per
configuration. The runtime supplies event policy;
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
nonmatching lines. Idle unregisters everything. The agent ignores the events of
its own code, the functions its own namespace defines; one it imports, as
`os.path.realpath`, is the program's to break in.

`python/hook.rs` generates a small ELF shared library, including its export
and unwind information. It needs no target compiler or libpython. Its dynamic
section is writable, because glibc before 2.35 relocates it in place. Its function
is a no-op without a debugger. At entry its three unsigned 64-bit arguments
are in rdi/rsi/rdx: kind, number, argument. Kind 0 acknowledges a generation
(argument 0–5 is the tool ID; 6 means idle), 1 reports breakpoint hits, 2 a
step's end by its nonzero number (argument 0: it stopped where it should;
1: the frame it begins in was not found on its thread; 2: its thread ran on
before the agent could bind it), and 3 reports an
error (argument 1: tool exhaustion; 2: callback/configuration failure with
confirmed disarming; 3: cleanup failed and prior state is unconfirmed;
4: initialization failed and any previous installation is unconfirmed;
5: observable user reentry makes monitoring coverage unconfirmed;
6: a user interruption is reported and re-raised, with the agent disarmed),
and 4 an exception: the number is its kind's bit (1 raised, 2 uncaught) and
the argument the address of three words, the exception's address and its
message's address and byte length, which the calling thread keeps alive while
it is in the hook. Error
reports retain the failed generation. One breakpoint hit is a nonzero number
with a zero argument. Several hits of one event are one report, so they stop
once together as hits at one native address do: the number is their count (2
to 4096) and the argument the address of an array of that many nonzero
numbers, which the calling thread keeps alive while it is in the hook.
Malformed reports produce a visible runtime stop. The frame comes from the
stopped thread's interpreter state.

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
A forked child is disarmed by the agent's `after_in_child` fork hook, which
frees its tool, unless the installation was made in the child itself: a
group that adopts the child can configure the agent from an earlier
at-fork handler, and that installation stays. The agent's `before` hook
takes its lock for the fork, and `after_in_parent` releases it, so no other
thread holds it in the child: the child's earlier fork handlers, such as
`threading`'s, run monitored, and their callbacks would otherwise wait
forever for a thread that does not exist there. The child reinitializes the
lock. The controller removes the child's traps before releasing it.
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
request; an armed one through its command slot. A source request goes to a
thread that is running Python: its instruction is in the evaluation loop, or
in a tail-call build in any instruction's function, or at one of the model's
entries. The entries are the loop and its first instruction's function where
something calls them, and `_PyFunction_Vectorcall` and `PyEval_EvalCode`,
which enter the loop where LTO and PGO inline it into them, as in
python-build-standalone's tail-call builds of 3.14 and 3.15. With no such
thread at a stop, the controller arms the entries and configures the agent at
the first thread that reaches one once the interpreter is initialized. Each source request
revalidates the private files through the thread that will read it. A
location stays pending until the matching acknowledgement. A change refused
before any write reaches the target keeps the acknowledged configuration,
which the agent still holds. Acknowledgements must arrive within five seconds
of running time; public and internal stops do not count. The controller
blocks on its messages meanwhile, and an alarm (`backend/linux/alarm.rs`)
wakes it at the earliest deadline, which `agent_timeout` then judges, so a
stale ring times nothing out. A free-threaded
interpreter needs every thread resumed while an agent is configuring or armed,
because monitoring changes stop its world. Detach cancels an unconsumed
request and disarms an armed agent before removing traps; exit and exec
remove the files.
`tests/python/agent.rs` checks the real monitoring API on 3.14/3.15 in both GIL
modes and the native register/all-stop boundary through `Scenario`.

**Breakpoints.** `PythonRuntime::resolve_breakpoint` claims a source path
ending in `.py`, without its `.` components, as a line probe, and a function
named `py:NAME` as a `PY_START` probe; `py::run` remains a native name. The
CLI adds such breakpoints pending, as their code loads only as the program
imports it. The agent's code is compiled under the filename
`<microscope agent LIBRARY>` (`agent::is_agent_filename`), so its interpreted
frames have `CodeRole::Interpreter`, as do those of the scripts that set it up,
`/tmp/.microscope-HEX/agent-GENERATION.py` (`agent::is_script_filename`);
splicing gives every frame inside the
innermost such frame, the hook and ctypes among them, that role too, and
`CPYTHON_MACHINERY` names the monitoring dispatch between the program's frame
and the agent. A hit's stop selects, and its condition and log message read,
the innermost ordinary frame that is not the runtime's own source
(`Controller::blamed_frame`), as an exception's stop does. A backtrace folds
frame 0 when it is an interpreter's machinery and another frame is selected.
DAP deemphasizes the source of each frame above the selected one. VS Code
ignores that hint at a stop it was told was a breakpoint or a step: it
focuses the first frame that has a source. At those stops the frames have no
source at all (`dap::inspect`'s `FOCUSED_REGARDLESS`).
`tests/python/breakpoints.rs` arms every kind in one run per build, beside
probes that must not match, and checks each stop's exact hits.

**Stepping.** Python steps by lines: into, over, and out, which
`Reading::step` turns into the command's third element, `[number, kind,
native thread, depth, code id]`, where depth counts the frame's callers and
the code id is its code object's address. A step begins only from where an
agent stopped the thread (`Controller::at_agent_hook`), as at a breakpoint it
reported or a step's end, because the thread must be inside a callback for
the agent to bind the step before it runs on; elsewhere it is refused with
why. A source step or a step out from the innermost frame, as DAP always
sends, steps the frame the agent's report blames
(`Controller::reported_frame`), not its hook. A step is begun
once its command is published, and a command no thread has read since its
stop may be replaced, so a step after a step at one stop works. The agent
binds the frame object on its own thread after the callback emits, matching
the native thread, depth, and code; a late bind or a missing frame ends the
step with `StepIncomplete` and its reason. It then follows phases: in the
frame, LINE (over and into, and into globally on the thread), `PY_RETURN`,
`PY_YIELD` and a global `PY_UNWIND`; in the caller after a return or unwind,
INSTRUCTION until an instruction that has a line, such as the call's or a
handler's; and, past the thread's outermost frame, global LINE on the thread.
A yield of a coroutine or async generator suspends without leaving it, so a
step over an `await` stays with the task, and into one is over it. A step's
stop selects the blamed frame as a breakpoint's does.
`tests/fixtures/python/agent_stepping.py` drives each phase through the real
monitoring API, and `tests/python/stepping.rs` takes every kind in one run per
build, ending with a step refused from a pause.

**Stepping into C and back.** A step into also takes CALL in its frame. For
a callable that may run the program's own C, a built-in function or method
or a method descriptor other than `builtins`' and its types', the agent
reports `(5, number, id(callable))`. `Reading::native_callee` reads the C
function from the callable's `PyMethodDef`, and `Controller::step_into_native`
traps the function's first statement as part of the step's plan, unless the
function is the runtime's own or no debug information describes it; then the
step goes on in Python. In the extension, a native step over or out plans
its lines by breakpoints as it would the main executable's. A native step
over or out whose frame returns into
the interpreter of a runtime with an agent
(`Controller::source_step_returned_to_runtime`) becomes, at an internal stop
(`Edit::ReturnToRuntime`), that runtime's step back, kind 3, from the
thread's innermost Python frame: the agent ends it at the thread's next LINE
in that frame or a caller. A thread in C may run no callback that would read
a command, so a step back is sent by a source request on its own thread,
which runs at the eval-breaker check after the call returns. It is sent even
over an unread command, whose older generation the agent then ignores, and
the callback that ends it binds a step configured at its stop. Where it
cannot be sent, the native step runs on, as into any code without source.
`tests/python/stepping.rs` steps into an extension's function, out of it and
over its end, through a builtin and a stripped twin of the extension, out
from a native breakpoint while the agent watches no line, and over the
function's lines and its call back into Python, on each build the extension
is built for.

**Assignments.** The binder makes `name = value`, and a compound
assignment, to a variable of dynamic type an `Op::Assign` whose value the
interpreter turns into `Assigned`: an exact integer, a binary64 float that
holds the value exactly, a truth value, UTF-8 text, `None`, or an object an
expression reached (a `Place::Interpreted`). Anything else is refused there.
`Controller::assign_interpreted` (`backend/linux/assignments.rs`) refuses,
before anything runs, any of these:

- a target that is not a variable of the frame's code
  (`InterpretedName::Variable`);
- a frame no thread runs;
- a runtime without an agent, or a free-threaded one
  (`process_scope_required`);
- CPython 3.12 and 3.13 (`assignments_refused`), whose agent runs through
  the launch bootstrap's own thread, and whose 3.12 `f_locals` is a copy;
- a thread its agent did not stop (`at_agent_hook`), or one with a pending
  signal;
- an interpreter with any thread state other than the frame's thread's
  (`RuntimeModel::sole_thread`), which reads the whole list and refuses
  when it cannot;
- an agent that does not take commands (`agent_takes_commands`).

The last two keep the write exact. The agent's lock or the GIL could be
held by another stopped thread, and running the frame's thread alone would
then deadlock or race.

`RuntimeAssignment` joins the arming beside the step. `Reading::assignment`
turns it into the command's fifth element, `[number, native thread, depth,
code id, name, value]`, where `value` is `["int", DECIMAL]`,
`["float", BITS]`, `["bool", 0|1]`, `["str", TEXT]`, `["none"]`, or
`["object", ADDRESS]`. Only the frame's thread runs, as
`ActiveKind::RuntimeAssignment`. The command is read as the agent's hook
returns, whatever events the agent monitors, so a ready agent takes it even
with only exceptions armed; a source request never carries an assignment.

`assigning` applies each numbered assignment once, on the frame's thread,
through `frame.f_locals[name] = value`. An object ID is accepted only if it
matches an object still held by a local, global, or builtin, or is the ID of
`None`, `True`, or `False`. Addresses are never cast to objects.
The old value stays in `_kept` until the next callback to prevent its
destructors from running while other threads are stopped.
The result is `(6, number, status)`: 0 assigned, 1 frame gone,
2 object no longer found by name, 3 interpreter refused.

`HookEvent::Assigned` ends the execution with `StopReason::Assignment`,
which ranks with a step's end. `publish_stop` selects the variable's frame
and, after the stop event, `answer_assignment` replies with the target read
again at the new stop, or with why it was not made. The stop that ends it
disarms it. A stop for any other reason answers that the assignment was not
made, and a process that ends answers likewise.
`tests/python/assignments.rs` gives each kind of value, checks the
program prints them, and checks each refusal leaves the stop as it was.

**Watchpoints.** A watch expression of dynamic type is accepted only when
it names an interpreted frame's variable and nothing more
(`Program::named_object`). `RuntimeModel::interpreted_watch` returns the
word in `localsplus` that holds the variable's object, with the code object
the frame runs; it refuses a cell or free variable and a frame its thread
does not own. The watch's scope is `WatchScope::Interpreted`. Its bytes are
observed through `interpreted_watched`, which untags a stack reference, so
the object's address stands for the word and a store of the same object is
no change; its value is the object's summary, kept from the last
observation, since the old object may be freed by the store. The frame is
live while its thread's chain holds it running the same code
(`interpreted_frame_runs`), checked at each stop and at each unchanged hit:
a new frame in the same place writes every slot before it joins the chain,
which ends the watch even where it stores what the slot held. A frame that
cannot be read keeps its watch, unproven. The stop selects the watched
frame, and `stopped_location` resolves an innermost interpreted frame, one
a stop in the loop itself has, through the trace rather than the program
counter. `tests/python/watches.rs` watches from a checkpoint on 3.12, 3.13t,
3.14, 3.15t and standalone-3.14.

**Exceptions.** `python-raised` and `python-uncaught` are the model's
`EXCEPTION_FILTERS`, off by default so that nothing runs in the process until
asked. The command's fourth element is their bits (`exceptions::bits`), and
`RuntimeAgent::wanted` installs the agent for them alone; a runtime whose
agent is not installed is marked stale as each module loads, so a filter
chosen before the program runs arms it. Raised arms global `RAISE`; since
`RAISE` fires again in each frame an exception unwinds into, a frame whose
traceback goes on into a frame it called (`tb_next.tb_frame.f_back is
frame`) does not report. Uncaught arms global `PY_UNWIND` and reports an
exception leaving the main module's code on the main thread (a `<module>`
frame whose globals name `__main__` and whose callers are only runpy's, as
for `-m`), the first frame of any other thread, or a frame whose caller is
`threading.Thread._bootstrap_inner`, which hands what a thread raises to
`threading.excepthook`; `SystemExit` is left alone. Any other frame with no
Python caller on the main thread returns to C, which may handle what it
raises: the interpreter catches the `ZipImportError` and `ImportError` its
path hooks raise as `site` starts, and reports an `atexit` callback's as
ignored. The agent forms the
message without running the program's code: the type's qualified name, by
`type`'s own descriptors, and its text only where the class's `__str__`,
found through the MRO's dictionaries, is `BaseException`'s or `KeyError`'s
and its only argument an exact `str`, cut at 1000 characters with `…`. An
uncaught report adds where its traceback's innermost entry raised it. The
value is `(py)ADDRESS`. Exceptions during the agent's own work, which runs
with its busy flag set, are not reported and are not interference.
`tests/fixtures/python/agent_exceptions.py` checks each rule in process;
`tests/python/stepping.rs` stops at both kinds after its steps, and a CLI
test stops at an uncaught exception chosen alone before `run`.
`tests/dap/python.rs` drives a breakpoint, steps, and an uncaught exception
as VS Code does, launched on 3.14 and attached on 3.15t, and checks that a
confirmed breakpoint never shows as pending again while the agent runs.

**Layout.** The module that carries the runtime, `libpython` or an executable
that links it, has a `.PyRuntime` section. From 3.13 its `_Py_DebugOffsets`
table gives most field offsets; `debug_info::cpython` reads it by name under a
schema per minor version (`schemas.rs`) and completes it from a layout
generated from the headers of a build of that version (`layouts.gen.rs`): the
build whose layout agrees with the table wherever both name a field, since
patch releases change what the table leaves out. 3.12 has no table, so
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
must still be a code object with the same identity. A frame the profiler's
walker read in the kernel carries the object's qualified-name and line-table
pointers and first line, which its row must match: an object put in a freed
one's place, as `exec` makes, has a line table of its own. `linetable.rs` decodes
`co_linetable` to the line of the frame's instruction. A free-threaded 3.14+
frame may run a thread-local copy of the bytecode, which `tlbc_index` selects.

**Splicing.** `RuntimeModel::driven` recognizes the activations of the
evaluation loop, by the names `RuntimeModel::activation` judges, which the
profiler's BPF engine and syscall stacks, having only symbols, ask directly: `_PyEval_EvalFrameDefault`, or, in a build whose loop
tail-calls a function for each instruction, those functions. Such a loop
enters its first instruction's function with a call, and PGO inlines the loop
into `_PyFunction_Vectorcall` and `_PyEval_Vector` but not into `gen_send_ex`,
so a loop frame whose inner frame is an instruction's function is the same
activation and does not count again. `interpreted.rs` places each segment of a thread's frames just
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

## Node

Node stack and value inspection read V8 from outside, including core dumps,
without executing JavaScript. Live run control uses Node's inspector and
the launch preload described below. Support covers V8 12.4, 13.6, and 14.6
(Node 22, 24, and 26), the official and nixpkgs builds, and Debian's shared
`libnode`.

**Layouts.** `src/debug_info/v8.rs` detects V8 and binds one generated
layout per version. The layouts come from each version's Torque output and
headers (`src/debug_info/v8/headers.rs`, `conformance.rs`), and stale tests
regenerate and compare them. A build's own `v8dbg_` exports complete the
layout and are checked against it. Nothing reads a V8 object except
through `Field` and `Constant` names; a field one version lacks is absent,
never guessed.

**Frames.** `node/frames.rs` describes a physical frame in V8's code. A
frame that keeps a Smi marker is V8's own. Otherwise the frame's function
gives its shared info, and:

- an interpreted frame's bytecode offset gives a script offset through the
  position table (`positions.rs`);
- a Sparkplug frame maps its pc to a bytecode offset first;
- a Maglev or Turbofan frame reads its safepoint and deoptimization
  translation (`compiled.rs`, `translation.rs`), which names inlined
  functions too.

**Live frames.** `node/live.rs` reads a running thread's JavaScript frames
from memory alone, for live snapshots (`RuntimeModel::live_frames`). V8
publishes where they begin only while JavaScript calls out to native code.
`CEntry`, API callbacks, and accessors build an exit frame and store its
frame pointer as `ThreadLocalTop::c_entry_fp_`. A fast C call builds none and
stores its caller's frame pointer and return address in `IsolateData`.
`JSEntry` saves both in its entry frame, clears them as it enters JavaScript
again, and keeps the outermost entry frame's pointer as `js_entry_sp_`, which
is 0 while the thread runs no JavaScript. The thread's isolate comes from
`g_current_isolate_` by its `ThreadLocal` placement. A `Dynamic` placement is
resolved through the loader's link maps and glibc's DTV in memory
(`backend::RunningTls`). Before the reader believes a word, it checks that:

- `ThreadLocalTop::isolate_` names the isolate;
- the thread entered it from no other isolate (`EntryStackItem`);
- a frame is published only with an outermost entry;
- `stack_is_iterable` holds, which the deoptimizer clears while it rewrites
  frames;
- the VM state is not the collector's;
- the exit frame's marker is an exit frame's;
- every frame lies on the thread's own stack, above its stack pointer and
  below the outermost entry, each above the one before;
- every frame's code is V8's builtins or code V8 generated.

An inner entry frame continues the walk at the exit frame or fast call it
saved. Each frame is described by `frames::describe`, as at a stop, so lines
are the stopped snapshot's. A fast C call from a bytecode handler runs in the
interpreter's frame and saves no bytecode offset first, so it is left out, as
is any frame `describe` cannot place, such as Node 22's Turbofan frame at a
fast API call. WebAssembly frames are left out by marker. Every reason is a
constant of `live.rs`. The caller proves the thread did not run during the
read ([profiling](profiling.md#thread-snapshots)).

**Await chains.** `node/awaits.rs` follows V8's `CaptureAsyncStackTrace`
(`isolate.cc`). It starts at the frame of the closure that resumed an async
function after an await: a frame V8 hides, whose function's builtin number
is one of the await closures'. The closure's await context holds the
running generator, whose promise the chain follows. A pending promise's one
reaction names what waits next: an await's closure names a suspended
generator, which becomes a frame at its saved bytecode offset; `Promise.all`,
`allSettled`, and `any` name a combinator frame and the capability they
settle; a capability's default resolve names its promise; and any other
reaction names the promise it settles. The chain ends where V8's does, and
says why when several reactions wait or a step cannot be read. Builtin
numbers come from `builtins.gen.rs`, checked at bind against the module's
`Builtins_` symbols, which V8 embeds in number order; a stripped build
such as Debian's uses its release's numbers. The frames splice after the
resumed function's frames on `StackSegment::Awaiting`, kept at their
generators' tagged addresses. `variables.rs` reads such a frame's receiver,
contexts, and the arguments and registers its `SuspendGenerator` saved. The
current microtask, which would start a chain inside a `.then` handler, is
not read: V8 keeps it among the isolate's roots, whose index microscope has
no generated layout for.

Names follow `JSFunction::GetDebugName` (`names.rs`). Scripts, function
names, and line ends are kept in `scripts.rs`'s process-lifetime table.
Each use proves its row again: a script by its id and source, a function
by its script's address, id, and source, and its function literal id.

**Values.** `node/objects.rs`'s `Reader` reads the heap at a stop through
the stop's block cache, charging an inspection's budget. `values.rs` and
`own.rs` read objects, properties, and elements into a value graph, and
`inspect/` writes it as `util.inspect` does for the target's Node release,
read from `napi_get_node_version`. Release-specific rules are named
`Change`s in `inspect/format.rs`. Errors whose stack nothing read are
written from their raw call sites (`call_sites.rs`), as Node's
`prepareStackTrace` would. Getters, proxy traps, and interceptors are
never called; a value that needs one says so.

**Variables and expressions.** `variables.rs` gives a frame its receiver
and arguments, its own contexts' locals, captured, module, and global
bindings, and its unnamed registers (F11). `access.rs` resolves names in
JavaScript's order and reads members, elements, `.length`, and `.size`.
`lineage.rs` names a class by its constructor and the script that defines
it, which chooses its view.

**Threads.** `thread_holding` labels a thread with the isolate it works
for, as described under [Thread labels](#thread-labels).

**Tasks.** `node/tasks.rs` lists each JavaScript thread's event loop work.
The isolate's current context (offset decoded from
`v8::Isolate::GetCurrentContext`) leads to its native context's embedder
data, whose Node context tag is checked before its `Environment` slot is
read (slots decoded from `node::timers::BindingData::SetupTimers`). The
`Environment`'s handle and request lists are walked by the `nodedbg_`
variables, read from the process or core at each stop, since 22 and 24 set
them at run time. Each node must link back to its neighbour. Each wrapper's
libuv handle or request must point back at it. Its class comes from its
vtable symbol, its provider name from `AsyncWrap`'s name table, and its
async id and trigger from the offsets `GetAsyncId` reads. The static facts
are decoded in `debug_info/v8/node_tasks.rs` and cached in the image
record. A refused shape is named, never guessed.

`node/timers.rs` reaches `internal/timers` through the context of the
principal realm's timers callback, which must be that module's
`processTimers`, and walks `timerListMap`'s `Timeout` lists. A callback
that is a function of Node's own script capturing exactly one of the
program's functions shows that function, as for `fs`'s `makeCallback`.
`node/pool.rs` finds libuv's pool threads by their `pthread_t`s. A thread
blocked in a futex on the pool's condition or mutex is idle, and any other
runs a job. Builds that link a shared libuv have no pool statics, and the
list says so as a gap.

Handles, requests, and timers are `stackless`: they have no frames, and
their `TaskSnapshot::resume` names the callback. Numbers are async ids, a
worker's offset by its thread id `<< 32`, a pool job's `1 << 62 | tid`.
`tests/fixtures/node/tasks.js` prints what `async_hooks`, `hasRef()`, and
`/proc` say, and `tests/node/tasks.rs` compares against it, live, in cores,
and on Debian.

**Inspector.** `backend/linux/inspector/` owns CDP breakpoints, steps,
exception stops, and worker sessions under the native all-stop controller.
`backend/linux/cdp.rs` owns the bounded, nonblocking transport. Worker
detach cancels outstanding requests for that worker and its descendants,
including their envelopes, without closing the main connection.
A step from a native stop sends `Debugger.pause`, which the main thread
takes only at an interrupt check after the I/O thread hands it on. The main
thread stays stopped until the I/O thread returns from
`InspectorIoDelegate::MessageReceived` with that request (`steps::Gate`),
so it cannot run past the statement the step should end at first.

Static native inspector facts belong in `debug_info/v8/inspector.rs`,
`inspector_activation.rs`, and `inspector_targets.rs`, and are cached in
`image/v8.rs`. Instruction proofs refuse unknown or conflicting layouts.
Proven printing and listing calls return an empty target vector before
allocating its strings; upgrade validation runs normally. Caller identity,
not a shared one-shot exception, keeps listing requests from consuming the
debugger's upgrade permission.
`runtime_model/node/native_inspector.rs` reads those facts at one stop to
identify an existing server without evaluating JavaScript or probing
application listeners. Only the Linux controller may activate a missing
server: `backend/linux/signal_action.rs` queries the stopped thread's
kernel signal disposition and compares it with the proven native handler.
Seccomp-filtered tasks are refused before the injected syscall executes.

`inspector/teardown.rs` owns asynchronous detach cleanup. Attach records
ownership when it activates Node's native handler. The launch preload
acknowledges ownership over CDP with a per-launch nonce and target UUID;
an existing server is acknowledged as borrowed. Cleanup finishes any
pending startup. For attach activation, `inspector/quiescence.rs` observes
the watchdog finishing its startup postings, a complete main-thread libuv
check callback, and a cleared startup async pending flag at all-stop. This
prevents either deferred startup callback from reopening a closed server.
It relies on libuv's non-reentrant event-loop contract. Cleanup then validates
the native agent and UUID at an exact nonce-bearing main-thread dispatch
entry and redirects that entry to `Agent::Stop`.
All threads run while Stop joins the I/O thread. Its return starts an all-stop
barrier that verifies the server closed before ptrace detach. Borrowed servers
only release debugger pauses and acknowledge the protocol cleanup. Native
entry extraction and StringView layout are provider facts, not controller
guesses. Cleanup has a deadline and reports failures, including a main thread
that never reaches the check callback. It does not prevent the application
or another signal sender from deliberately reopening the inspector.

**Assignments.** `Controller::assign_interpreted` hands a JavaScript
assignment to `assign_javascript` when the runtime is driven through its
inspector. `RuntimeModel::assigned_binding` names the variable and which
scopes may hold it (`BindingReach`): the frame's own, an enclosing
function's, or the script's. The receiver, a global, an unnamed register,
and a variable found through `with` are refused there. A variable kept by
optimized code is tried, since V8 assigns it only if a context holds it,
and its refusal says why. `inspector/assignments.rs` makes the
`Debugger.setVariableValue` request, which needs a `callFrameId` from a
pause V8 holds: `Paused::bound` maps each bound interpreted frame to the
frame V8 listed. No request goes out while a stop is published. The
assignment is an execution of every thread (`ActiveKind::RuntimeAssignment`).
`resume_inspectors` sends its first request in place of the pause's
`Debugger.resume`, so the paused thread stays in V8's message loop. V8 is
asked for each candidate scope, innermost first, until one holds the name.
An object is given as the `objectId` V8 lists for the variable that holds
it, found with `Runtime.getProperties` on the frame's scopes. Once V8
answers, `Edit::InspectorAssigned` stops every thread and publishes
`StopReason::Assignment`. The pause stays held for that stop and is let go
at its resume.

V8 snapshots scope objects at the pause; listing them after an assignment
still returns old values. `PausedFrame::assigned` replaces the cached value
and retains its `CallArgument`, so a later `x = y` uses `y`'s updated object.
Assignments in another frame or scope may share the context, so those names
cannot supply objects until the program runs again. Frame names bind only if
every listed value matches its slot; a mismatch leaves the frame unnamed.
The assignment reply reads the variable from memory again. Turbofan variables
cannot be read back, and the reply says so.
`tests/node/assignments.rs` checks each binding and value against
the program's own output on every build, Debian's included. The scripted
test `an_assignment_holds_v8_s_pause_until_its_own_stop_resumes` fixes the
order of requests.

**Addons.** `node/napi.rs` reads the C function a Node-API call runs, where
V8 enters `v8impl::FunctionCallbackWrapper::Invoke` with the call's
`FunctionCallbackInfo` in `rdi`. On 12.4 the template's data is in the
info's implicit arguments. On 13.6 the template is there, and on 14.6 it is
in the info's own words. The data, `FunctionTemplateInfo::CallbackData`, is
a `JSExternalObject` holding the function's `CallbackBundle`, whose third
word is the callback. Each step is checked against the type V8 records
there. [Process control](process-control.md) describes the step into the
callback and back. `tests/node/addon_steps.rs` holds the C stops to the
addon's marked lines, and the JavaScript stops to V8's own `stepInto` and
`stepOut` under the plain CDP oracle. A V8 `Debugger.pause` asked while the
thread runs C pauses at the next function entry, not at the caller's next
statement, so the step back relies on the step V8 still has armed, never on
a pause.

**Watches.** `node/watches.rs` resolves `watch name` in a frame to the
word that holds the variable: an interpreter or Sparkplug frame's register
or argument slot, or a slot of a context the frame reaches. Each is an
`InterpretedPlace`, either an offset from the frame or a context depth and
slot, so that the place can be found again after V8 moves the context.
`variables.rs` keeps each variable's slot address for this. Refused: the
receiver, a global, a script binding, a variable found through `with`, an
optimized frame's variable, a resumable function's frame slot, a
block's register variable, and, on 14.6, a context slot that holds or
will hold a context cell (`Context::Set`). V8 gives each block's variable
a register of its own (`Scope::AllocateStackSlot`), but each entry of the
block binds it anew there with no bytecode to mark it; V8's listing says
which names are a block's (`ReportedLocal::block`). A block's captured
variable is in the block's own context, which each entry makes anew, so
its watch ends with the block.

`backend/linux/interpreted_watches.rs` follows V8's collector. A
collection runs on its isolate's thread, so a breakpoint on
`Heap::PerformGarbageCollection` sets the watches of that thread's frames
aside, noting each place. At the collector's return the debugger places
them where the frames reach their words now, with their values read anew.
The collector's own writes are never reported. Probes under gdb on 12.4,
13.6, and 14.6, with `--minor-ms`, `--stress-compaction`, and
`--single-threaded-gc` and with a worker, found every scavenge,
mark-compact, and evacuation inside that function on the isolate's own
thread. A breakpoint on the on-stack replacement builtins, planted in
every mapping of the image because V8 may run a copy of its builtins, ends
the watches of the replaced frame's own slots with `FrameReplaced`, even if
the replacement then fails. A `FrameReplacer` names how many saved frame
pointers lie between the entry's rbp and the replaced frame.
`InterpreterOnStackReplacement` is entered from the `JumpLoop` handler's
own frame, which it drops before entering the code, so its replaced frame
is `[rbp]`. Sparkplug's builtin is entered from the frame itself. V8
re-enters a builtin on each back edge while it compiles the replacement,
and each entry stops every thread. So the builtins are planted only while
a frame-slot watch exists, and its first entry ends that watch.
`stack_frame_left` ends a frame-slot watch when the frame returned (no
frame of the thread's unwound stack is at its address, the innermost CFA
is above it, or rbp is above it in V8's code) or when
a frame builder (`InterpreterEntryTrampoline`, Sparkplug's prologue) runs
at the frame's address. A store stops inside a bytecode handler.
`frames.rs` describes that frame as the handler's function, and the
unwinder walks handlers by frame pointer (`frame_pointer_code`). The
collector's sites are planted only while an interpreted watch exists. Debian's `libnode` names neither function, so every watch is
refused there. `tests/node/watches.rs` checks each hit against the program's
own prints under `--stress-compaction`.

The launch preload precedes user `NODE_OPTIONS` preloads. Followed launches
retain a content-addressed private cache copy so released descendants do
not inherit a deleted require path. An untraced descendant restores the
original environment; workers leave their supplied environment unchanged.

The suite is `just node`; Debian's tests run inside its namespace
(`tests/node/debian.rs`). `tests/fixtures/node/truth.js` makes each fixture
report V8's own frames and `util.inspect`'s text at named checkpoints, and
the tests compare microscope against them on every build.

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
