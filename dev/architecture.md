# Architecture

microscope separates target-specific facts from its public debugger model.
Linux, x86-64, ELF, DWARF, ptrace, and native registers stay at the edges.
Clients talk to the same model whether they are the terminal, browser, DAP
server, or a future interface.

## Code map

| Path | What lives there |
| --- | --- |
| `src/lib.rs` | Public debugger API, session construction, and `DebuggerHandle` |
| `src/model.rs`, `src/model/` | Platform-neutral public types and module-image model |
| `src/protocol.rs` | Requests, replies, events, `StopId`, and execution identity |
| `src/backend/mod.rs` | Backend interface used by the public debugger |
| `src/backend/linux.rs`, `src/backend/linux/` | Linux controller and target-specific implementation |
| `src/present/` | Formatting shared by user interfaces |
| `src/cli/`, `src/dap/`, `src/web/` | Terminal, Debug Adapter Protocol, and web clients |
| `tests/debugger/`, `tests/dap/`, `tests/web/` | Public-boundary integration tests |

## Debugger clients

`DebuggerHandle` is the client boundary. A client sends requests through a
bounded channel and receives acknowledgements and events. Published snapshots
are immutable. Clients do not borrow or lock mutable debugger state.

Run-control requests are acknowledged before the stop they eventually cause.
A client must use the `StopId` from the state it read when it makes a mutation.
The debugger rejects a stale ID instead of applying an action to a later stop.

Presentation shared by clients belongs in `src/present`. Do not duplicate
formatting rules in the CLI, DAP server, or browser.

## Addresses and modules

`ModuleImage` contains immutable metadata read from a program or debug file.
`LoadedModule` binds that image to one mapping in one process. An image can be
shared by several mappings and stored in the image cache.

`ImageAddress` is an address relative to an image. `VirtualAddress` is an
address in a process. Keep the types separate and convert only where a mapping
is available.

The module path, identifier, mapping, and separate debug-file location belong
to the binding. They are not properties of the image bytes. A cache key names
the input bytes and the loader sources that interpret them.

## Expressions

All source languages use one expression language. `src/eval` is pure: it sees
a program only through traits implemented by the debugger. Parsing needs no
program. Binding resolves names and target types for one frame. Evaluation
reads one validated stop.

Integer arithmetic is exact. Bit operations preserve operand width. Only a
cast truncates. Target data layout comes from debug-information providers; the
evaluator does not calculate field offsets or ABI rules.

Every example in [`docs/expressions.md`](../docs/expressions.md) runs as a
test. Change the reference and implementation together.

## Views

Views turn stored representations into logical values. Parsing and matching
live in `src/view`; built-in files live under `views`. A failed check or read
produces an explicit problem and falls back to the stored value.

View examples are executable tests. See
[`docs/views.md`](../docs/views.md).

## Runtimes

Language runtimes are exposed through neutral tasks, logical stacks, code
roles, and resume points. Go and async Rust providers may discover those facts
differently, but clients must not depend on a runtime's private layout.

The runtime model describes the facts available at one stop. It must not
retain pointers into mutable debugger state. Missing runtime facts produce
typed partial results.

## Library and binary boundaries

The `microscope` library owns the debugger model and behavior. `src/main.rs` and
the modules compiled by the binary (`src/cli`, `src/dap`, `src/present`, and
`src/web`) adapt that library to user interfaces.

The library exports model records, protocol options, `Debugger`, and
`DebuggerHandle`. Most implementation modules remain private. Keep a type
private until an external client or a clean internal boundary needs it; a
public debugger API is expensive to reshape once several clients depend on
it.

Feature-gated binaries have narrower purposes:

- `microscope-tools` contains image, timing, benchmark, and overhead tools used
  by repository recipes.
- `microscope-sim` sweeps and replays deterministic simulated sessions.
- `fuzz/` builds structured fuzz entry points exposed only by the `fuzzing`
  feature.

Debug builds compile the flight recorder. Release builds remove its calls and
do not evaluate their arguments.

## Ownership and concurrency

There is one owner of mutable debugger state: the controller. Clients do not
lock it and helper tasks do not borrow it. This makes request order, stop
transitions, and shutdown behavior visible in one place.

The main concurrency roles are:

| Role | May block in | May call ptrace | Owns debugger state |
|---|---|---:|---:|
| Controller OS thread | native control operations | yes | yes |
| Waiter thread | `waitpid` | no | no |
| Tokio tasks | channels, sockets, client work | no | no |
| Source and file readers | host file I/O | no | no |
| Profiler workers | perf events and capture processing | no debugger ptrace | profiler state only |

Channels are bounded where a slow client or producer could otherwise grow
memory without limit. An unbounded channel is acceptable only for a stream
whose producer and lifetime impose a separate strict bound; document that
bound at the channel.

Shutdown is a protocol, not a dropped object. The controller must release or
reap every process it owns, waiter threads must end, replies must resolve, and
clients must observe a terminal event or error. Tests check the process table,
not only returned futures.

### Cancellation

A future that is dropped stops at the await it reached, and nothing after
that await runs. In microscope that happens when a `select!` branch loses,
a timeout expires, a task is aborted, or the runtime shuts down. Code that
changed shared state before an await and fixes it after leaves it broken.
Oxide's [RFD 397](https://rfd.shared.oxide.computer/rfd/0397) and
[RFD 400](https://rfd.shared.oxide.computer/rfd/0400) describe the problem
and the patterns used here.

- A `select!` branch and a timeout wrap only futures that are safe to drop:
  `mpsc` and `broadcast` receives, `watch::changed`, `Sender::reserve`,
  accepts, signals, sleeps. Resume a future that is not, through `&mut`,
  instead of making it again on every pass.
- Work that must not stop partway runs on a task of its own that nothing
  aborts, and others send it messages. Debugger requests do this: the
  controller performs a request whole whether or not the caller still
  waits for the reply. So do the web server's requests and a launched
  program's input.
- Shared data lives under a `std::sync::Mutex` that is never held across an
  await. A `tokio::sync::Mutex` holder that is cancelled releases the lock
  with its change half done; Clippy refuses the type unless an `#[expect]`
  says why no holder can be cancelled.
- Ask a task to stop rather than aborting it. Clippy refuses `abort`
  unless an `#[expect]` says why nothing the task can be doing matters,
  as for `dap::output::Forwarder`, which only forwards output.
- Dropping an owner requests the same cleanup as shutting it down, without
  waiting: `Debugger` and `ProcessGroup` both do. Prefer the explicit
  `shutdown`, which reports whether cleanup succeeded.
- Blocking work, such as reading a program's debug information, runs under
  `spawn_blocking`. A runtime thread that blocks stalls every task on it,
  and a read of a FIFO never ends.

## Requests, replies, and events

`src/protocol.rs` holds request inputs and public event records. Each request
has a typed one-shot reply. Events use a broadcast channel because several
clients can observe one session.

A request reply answers whether the controller accepted and performed the
immediate operation. An event describes a state transition other clients also
need to see. Do not use events as private replies, and do not hide a public
state transition in a reply.

Execution has two identities:

- `ExecutionId` names one accepted run-control operation.
- `StopId` names one complete stopped state.

Starting a run returns or publishes the execution identity before its future
stop. Reads and stopped-state writes name the stop they apply to. This ordering
allows clients to remain responsive and prevents an old UI action from
controlling a newer stop.

`StateSnapshot` is the cheap summary a client can request at any time. Detailed
data such as backtraces, variables, children, global pages, disassembly, and
source text is requested separately. Do not make every stop eagerly compute
everything every client might display.

## State model

`InferiorState` distinguishes a program that has not started, is running, is
stopped, or has ended. A stopped state is public only after the all-stop
barrier completes. Internal tracees can be in finer states while that barrier
is being gathered.

Selection is client-facing session state: execution context, thread or task,
and frame. A selected logical task may be parked and have no native thread at
the stop. APIs that need registers or memory must resolve the selected context
and return a typed failure when no native access exists.

Module lists, breakpoint lists, watchpoints, signal policy, and loaded views
are session state. Values, frames, registers, and task snapshots are stop
state. Keep that distinction clear when deciding what invalidates a cache.

## Static and dynamic data

Static data is immutable after loading:

- image-relative code and data locations;
- source lines and functions;
- types, declarations, and variable location programs;
- unwind rules;
- language code roles and resume points;
- symbols and package information.

Dynamic data comes from a process or core at one stop:

- loaded mappings and relocation bias;
- registers and memory;
- thread and task state;
- selected frames;
- expression values and presented children.

Never store a runtime virtual address in an image table. Never evaluate a
location program without a module binding and stopped execution context.

## Inspection and bounded work

Program inspection is adversarial input processing. Debug information can
describe enormous arrays, cycles, malformed ranges, or locations that require
many reads. The public model therefore carries completion and exhaustion
states instead of assuming every request finishes fully.

Inspection limits cover reads, bytes, recursion, children, and related work.
Pass a caller's limits through every nested evaluator and view. A helper that
starts a fresh unlimited inspection defeats the outer bound.

Paginate catalogs and aggregate children with deterministic ordering. A page
token or offset must continue the same logical sequence at the same stop.
Never gather an unbounded collection merely to return its first page.

## Errors and partial answers

Use the distinction that helps a caller act:

- An operation error means the request could not be performed.
- An unavailable value means the program or debug information lacks a needed
  fact.
- A malformed value means available facts contradict their format.
- A partial result contains useful data and a typed reason it ended.
- An unsupported result names a feature microscope deliberately does not handle.

Do not collapse these into an empty vector, zero address, short read, or
generic string when the public model has a typed representation. Strings are
appropriate at presentation boundaries and for context attached to a typed
cause.

## Adding a client-visible feature

A feature that affects all clients usually follows this order:

1. Add or extend neutral model records.
2. Add the request and typed reply to the protocol.
3. Implement it in the real controller and any post-mortem variant.
4. Decide how the simulator treats it: modeled behavior, explicit model gap,
   or no kernel interaction.
5. Add shared formatting to `src/present` where output semantics are common.
6. Expose it in CLI, DAP, and web where each protocol can represent it.
7. Test debugger behavior separately from each client's parsing or rendering.

Do not force DAP concepts into the public debugger model. DAP handles,
capabilities, scopes, and client quirks belong in `src/dap`. Likewise, React
component state and URL encoding belong in `web`.

## Adding platform or format support

New operating systems, architectures, object formats, and debug formats should
implement existing neutral contracts before adding public concepts. If a
neutral concept truly cannot express the target, describe the target fact and
the user-visible need before changing the model.

Keep target word size, byte order, register descriptions, and address rules in
`TargetDescription` and providers. Avoid `cfg(target_arch)` in generic model,
evaluation, view, or client code when a target description can carry the
answer.
