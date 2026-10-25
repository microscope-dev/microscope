# Process control

The Linux backend controls each tracee independently while the public debugger
presents one all-stop process.

## Code map

| Path | What lives there |
| --- | --- |
| `src/backend/linux.rs` | Controller state and the main Linux event loop |
| `src/backend/linux/native.rs`, `src/backend/linux/native_tracee.rs` | Real kernel operations and native tracee access |
| `src/backend/linux/lifecycle.rs`, `src/backend/linux/new_task.rs` | Launch, attach, fork, clone, exec, exit, and detach |
| `src/backend/linux/tracer.rs`, `src/backend/linux/wait_hub.rs` | The tracer thread that hosts controllers, and the one waiter that routes their statuses |
| `src/backend/linux/follow.rs` | The follow policy, fork transfer, exec promotion and demotion, and watched processes |
| `src/group.rs` | `ProcessGroup` and `GroupHandle`: members, group stops, and breakpoint mirroring |
| `src/backend/linux/classify.rs`, `src/backend/linux/internal_stops.rs` | Raw wait-status classification and internal stops |
| `src/backend/linux/run_control.rs`, `src/backend/linux/stepping.rs`, `src/backend/linux/step_targets.rs` | Resume, pause, and stepping plans |
| `src/backend/linux/breakpoints.rs`, `src/backend/linux/managed.rs`, `src/backend/linux/watchpoints.rs`, `src/backend/linux/writes.rs` | Breakpoint sites and runtime arming, hardware watches, and stopped-state writes |
| `src/backend/linux/signals.rs`, `src/backend/linux/awaits.rs` | Signal policy and asynchronous wait coordination |
| `src/backend/linux/catchpoints.rs` | System call catchpoints; see [system call tracing](syscalls.md) |
| `src/backend/linux/recorded.rs`, `src/backend/linux/sim_edge.rs` | Recorded native calls and the simulator boundary |
| `tests/debugger/`, `tests/watchpoints.rs`, `tests/hit_counts.rs` | End-to-end process-control behavior |
| `tests/debugger/group.rs`, `tests/dap/forks.rs` | Process groups through the library and through DAP child sessions |
| `tests/fixtures/c/` | Native programs used to force lifecycle and concurrency states |

## Thread ownership

ptrace ties a tracee to the OS thread that traced it, and the kernel traces a
traced process's fork children to that same thread. So each live session has
a tracer thread (`tracer.rs`) that creates or attaches its tracees, and every
ptrace call runs there. A tracer thread hosts one controller per debugged
process. It serves each controller's queue in turn, through the same
`handle_message` a controller always runs, and parks while none has a
message. A session of one process has a tracer thread to itself
(`Tracer::alone`); a process group shares one. A core dump's controller runs
on a thread of its own and makes no ptrace calls.

`waitpid(-1)` returns any traced child's status, so each tracer thread has
exactly one waiter, the wait hub (`wait_hub.rs`). It routes each status to
the controller that owns the thread it reports on: the controller of the
thread's process, or, for a new process whose fork event has not arrived
yet, its parent's. It remembers each routed thread until that thread's exit
status. The hub never calls ptrace.

The tracer thread lives until every controller has ended and every handle to
it is dropped, since a launched process dies with the thread that launched
it.

Tokio coordinates clients and message passing. It does not own blocking native
process control.

## Stops

Classify raw wait statuses before publishing events. Track each live tracee,
its stopped state, and its pending signal. Publish a public stop only when all
live tracees are known to be stopped.

Run-control requests have two distinct results:

1. The request is accepted or rejected.
2. The resumed program later stops or exits.

The acknowledgement comes first. Never make a client wait for the later stop
before acknowledging the request.

Every stopped-state mutation carries a `StopId`. Compare it with the current
stop before changing state. This protects a client whose view lags behind
another client or a rapid sequence of stops.

## Signals

Keep pending signals per tracee. A signal observed while gathering an all-stop
state must stay attached to the thread that received it. Resume policy decides
whether to deliver or suppress it.

## Software breakpoints

A software breakpoint site belongs to the process address space. Physical
installation is separate from the user breakpoints and execution plans that
own the site.

Memory reads shown to users must contain the program byte, not the installed
trap byte. When several threads meet one site, repair them one at a time while
their siblings remain stopped.

A vfork child, and anything `system` or `posix_spawn` starts, shares the
parent's memory until it execs or exits. Every installed trap is lifted for
that window, from the parent's `VFORK` event to its `VFORK_DONE`, and the
parent's siblings stay stopped meanwhile. The child is released, or handed to
its group's watcher, only after the traps are lifted. A pause requested
during the window completes once the child execs or exits, since the
vforking thread cannot stop until then.

## Exec

The program a process runs is the controller's `MainProgram`. An exec of the
same file keeps it. An exec of another file loads that file, read through
`/proc/PID/exe`, as the main program under a new `ModuleId` and
`ModuleImageId`, so no client cache keyed by either can serve the old image.
The old program's module unloads, its runtime models are forgotten, and every
breakpoint's locations are dropped and resolved again in the new program
before it runs; one set by address stays pending. `ProgramChanged` is sent
once every handle's `module_image()` already describes the new program. Each
launch and attach starts from the program the session was made for again.

A followed exec in a continued process runs on unless `set_exec_stops` asked
it to stop; a step's plan was the old image's, so a step ends there. A
program that cannot be loaded stops the process with `Exec { followed: false }`
and the reason, which every image-based request then refuses with.

A group's root follows every program it executes this way, and so does a
member whose new program the follow policy debugs. A member whose new
program the policy does not debug is demoted at the exec stop: its
controller lets the process go to the group's watcher (`demotes_at_exec`).
See [Process groups](#process-groups).

## Testing control changes

Cover deterministic reducers or classifiers directly when they contain real
branching logic. Cover public behavior through `tests/support::Scenario` and a
synchronized native fixture. Test cleanup, cancellation, event ordering,
state consistency, and the absence of surviving inferiors.

Run `just stress` and `just sim 600` before merging lifecycle, run-control,
attach, or concurrency work.

## Linux backend layout

`src/backend/linux.rs` defines the controller and the state it owns. The
submodules divide work by behavior:

| Module | Responsibility |
|---|---|
| `native` | Real Linux operations and the trait boundary used by simulation |
| `lifecycle` | Launch, attach, detach, termination, and process ownership |
| `classify` | Convert raw wait evidence into debugger stop classes |
| `awaits` | Gather run results and complete the all-stop transition |
| `stepping`, `step_targets` | Source and instruction execution plans |
| `breakpoints` | Physical sites, logical owners, hit processing, and repair |
| `writes` | Mutations that require a valid public stop |
| `frames`, `async_frames`, `returns` | Native and logical frames and return state |
| `inspection`, `evaluation`, `presentation` | Read stopped program state |
| `memory`, `disassembly` | Low-level reads and decoded code |
| `runtimes` | Runtime discovery and task snapshots |
| `post_mortem` | Core dumps as a permanent stopped session |
| `recorded` | Flight-recording wrapper around native operations |
| `sim_edge` | Adapter from the controller's native contract to the simulator |

Keep a native operation in the smallest interface that represents what the
controller asks Linux to do. The simulator implements the same observable
operation. Do not make the simulated edge call controller internals to skip a
hard behavior.

## Launch and attach

A launched program is created under controller ownership. The controller is
responsible for its initial stop, configured ptrace options, later execution,
and eventual reap. If startup fails after creating a process, the failure path
still owns cleanup.

An attached program already has an external owner. microscope seizes its
threads, interrupts them, gathers a complete stop, and detaches them on ordinary
shutdown. Detach must preserve the signal and job-control behavior the session
promised. Never treat an attached process as one microscope may silently kill.

Threads can appear or disappear while attach is gathering them. The algorithm
must converge using observed `/proc` and wait state, with a deadline at the
client boundary. A single process-list snapshot is not proof that every thread
is controlled.

Core dumps use the same public stopped APIs but no live controller operations.
Their single stop never resumes. Keep post-mortem behavior explicit rather
than scattering `if core` checks through the live controller.

## Wait processing

The waiter reports raw wait statuses to the controller. A status is evidence,
not yet a public stop. Classification considers:

- signal-delivery stops;
- software and hardware traps;
- single-step traps;
- ptrace clone, fork, exec, and exit events;
- internal interrupt stops used to gather siblings;
- execution-plan breakpoints;
- process and thread exits.

Classify before mutating public state. Keep enough raw evidence in development
recordings to reconstruct why a status received its class.

Linux can report statuses in several valid orders. Code must not rely on a
particular sibling stopping first or a clone parent arriving before its child.
The controller reducer should accept every order permitted by the conformance
rules.

## Building an all-stop

When one tracee stops for a publishable reason, the controller interrupts
running siblings. During this gathering phase, more statuses may arrive:
another breakpoint, a signal, an exit, or a thread creation event. Track each
tracee separately until every live tracee has a known stopped or terminal
state.

Only then:

1. choose the public stop reason according to the reducer's rules;
2. preserve each tracee's pending signal;
3. allocate a new `StopId`;
4. establish the selected execution context and frame;
5. invalidate stop-scoped caches;
6. publish the stopped snapshot and event.

An internal interrupt used to gather a sibling is not a user-visible signal.
Do not publish it as a pause unless the public pause request is the reason the
gather began.

## Resume and execution plans

A resume validates the caller's stop, computes how every stopped tracee should
continue, installs needed execution-plan sites, acknowledges the accepted
execution, then lets tracees run.

Execution plans implement operations such as source step, next, finish,
advance, and selected-call stepping. Their breakpoint ownership is separate
from user breakpoint ownership. Retiring a plan removes only that plan's
ownership; a physical site remains installed while any owner still needs it.

A native source step reads its code in whichever loaded module holds it, a
shared library's as the main executable's (`Located` in `stepping.rs`). The
step keeps that module in `StepStart::module`, since its source, code
instance, and physical instance mean something only in their own image, and
code of another module is never the step's line or instance. Some helpers
still read the main executable alone, so in a shared library a step has no
loop bodies treated as its own code, no awaits followed, no chosen call to
step into, and no inline frames to select; a step out of an outer frame is
refused there.

Each accepted plan gets an `ExecutionId`. A trap may complete the plan, be an
unrelated user breakpoint, hit a watchpoint, carry a signal, or expose a
runtime transition the plan follows. Keep those cases explicit. “Trap means
step complete” is not a valid rule.

A step from a frame a runtime interprets, which has no instructions of its own
to step, is a runtime step (`runtime_steps.rs`). Such a frame may be at any
level. The runtime's arming gains the step, with a number never reused in
the process. The execution,
`ActiveKind::RuntimeStep`, runs threads as a continue does. It keeps the
native step's owner, a thread or a task. The step ends only where the
runtime's internal hook reports `HookEvent::StepEnded` with the step's number
on a thread that runs the step. Any other report goes on unseen. The runtime
is armed with no step as whatever stop ends the step forms. A runtime that
cannot make a step refuses it, and nothing runs.

A JavaScript step into, begun where V8 holds the thread paused, also plants
a trap in its plan where V8 enters Node-API callbacks
(`RuntimeModel::native_entry`, `inspector/addons.rs`). When the step's own
thread reaches it, `native_callee` reads the C function from the call's info.
`step_into_native` then plants a trap at that function's first statement, and
the step ends there. When another thread reaches the entry trap, it is
repaired and goes on. V8's `stepInto` stays armed meanwhile, and
`Inspector::through` records it. A native step over or out that returns from
the addon into Node's image becomes the runtime step back
(`Edit::ReturnToRuntime`, `RuntimeStep::returning`). Its `Stepping` counts as
already sent, so V8's next pause in that isolate ends it. A pause no one
claimed in a function the addon calls, deeper than the step began, continues
with `stepOut`, so the step back stays armed until the addon returns. Any
other resume of that isolate forgets the record.

## Breakpoint installation and repair

A software breakpoint replaces the first byte of an instruction with `int3`.
The physical site stores the original byte and its owners. Logical breakpoint
records store user options such as enabled state, condition, hit count, ignore
count, and log message.

On a hit, x86-64 reports the instruction pointer after the trap byte. Repair
rewinds the selected thread, restores the original byte, executes the
instruction, and reinstalls the trap when owners remain.

More than one thread may have reached the same site before the all-stop
completed. Repair each co-hit thread sequentially while every sibling remains
stopped. Letting siblings run during repair can lose hits or execute a
temporarily unpatched site.

All user memory reads overlay original bytes at installed sites. Writes must
update the original-byte model correctly so removing a breakpoint does not
restore stale memory.

## Managed breakpoint locations

A runtime that runs code itself, as CPython runs Python, can resolve a
breakpoint there. Resolution runs a list of resolvers: the native one over
every loaded image, then `RuntimeModel::resolve_breakpoint` for each loaded
runtime. A runtime's location is `BreakpointLocation::Managed`: a runtime and
a line or function, with no address. Native and managed locations of one spec
are combined. A spec only a runtime knows resolves through the runtime alone,
and a runtime's refusal is the error when nothing else resolves it.

Ownership follows the native split. `managed.rs` records which user
breakpoints own each managed location, and numbers it. The number is never
reused in a process. A runtime is armed whole (`Arming`) with its
locations, its step, and which exceptions stop. The arming is redone whenever
any of them changes. `RuntimeModel::arm` computes the writes, so a model keeps
no state between armings, and `write_for_runtime` makes them at an all-stop.
A runtime that cannot be armed keeps why. A hit comes through the runtime's
internal hook as `HookEvent::Breakpoint` with the location's number. It is
counted and judged as a trap's hit is, and stops as `StopReason::Breakpoint`
with no address.

## Watchpoints

Hardware watchpoints are planned against x86 debug-register constraints:
limited slots, supported lengths, alignment, access modes, and addresses. A
plan must be transactional. If one register assignment fails, restore the
previous usable plan.

New threads do not necessarily begin with usable copies of the debugger's
desired debug-register state. Arm and verify them as part of tracee lifecycle.
DR6 is evidence from the latest debug exception and can be stale at other
stops; read it only in the context that gives it meaning.

The simulator has conformance rules for watch traps, single-step overlap,
thread creation, rejected register writes, repeated-string instructions, and
ptrace memory writes. Update those rules only from real-kernel probes.

## Signals and exceptions

A signal has three independent public choices: stop, print, and pass. Keep the
pending signal attached to its tracee until resume decides whether to deliver
it. Suppression is an explicit resume action, not loss during all-stop
gathering.

Language exceptions such as Go panics and Rust panics are built from runtime
evidence and reported through neutral exception records. They may share native
breakpoint machinery, but clients should not need to know the runtime symbol
that caused them.

SIGKILL, group exit, exec, and fork can invalidate normal assumptions about
memory, thread lists, and ownership. Handle them as lifecycle transitions,
not ordinary signals.

## Process groups

A launch or attach through `ProcessGroup` debugs the program and the
processes it starts. The `FollowPolicy` (`src/protocol.rs`) decides which
are debugged: its `FollowMode`, `programs` globs matched against a
program's file name, and a `StopScope`. `Policy::decide` in `follow.rs`
judges a program by its path and inode. `auto` debugs the program the group
was made for wherever it runs again, by inode, a program
`program_runtime` recognizes as Node.js, CPython, or Go, and the named
programs. `all` debugs everything. With `none`, the root never joins the
group, so its fork children are scrubbed and released as a single session's
are.

Every process of a group is traced by the group's one tracer thread, and
each debugged process, a member, has a controller of its own there. A
process moves between owners on that thread only, so none runs untraced in
between. A controller hands a process over by queuing a `Handover` in the
shared `Following`; the tracer thread takes it once the message that found
it has been served:

- **Fork.** A member's controller removes the traps a fork child inherited,
  then transfers the child, stopped in its first stop, with the parent's
  program already read. The child gets a controller (`Start::Adopt`) under
  every mode but `none`, since it runs a program the group debugs. A child
  that cannot be scrubbed is killed rather than left with traps.
- **vfork.** A vfork child goes to the watcher until it executes a program,
  since it shares its parent's memory.
- **Promotion at exec.** A watched process that executes a program the
  policy debugs gets a controller whose initial stop is that exec event
  (`Start::AdoptExec`). A program that cannot be loaded is reported as
  `GroupNotice::Unfollowed`, and the process stays watched.
- **Demotion at exec.** A member, other than the root, that executes a
  program the policy does not debug goes to the watcher.

An adopted CPython 3.12 or 3.13 process also gets its own channel to the
launch's Python bootstrap before it runs (`join_python_bootstrap`); see
[runtime models](runtime-models.md).

The watcher (`Watcher` in `follow.rs`) runs on the tracer thread and keeps
processes traced without debugging them. It resumes each of their stops at
once with its signal, and notes only forks, threads, execs, and exits.

`src/group.rs` drives the members from a tokio task. Each new member is a
`DebuggerHandle` of its own, numbered from 1, the root, in the order members
joined. While the member is in its first stop, the driver copies every one
of the root's breakpoints into it, then resumes it unless the group is
stopped, so a member runs no instruction without them. The root's
breakpoints are the group's. A change to them is mirrored into every
member. `Shared::copies` maps each member's copy back to the root's
breakpoint, and `GroupHandle::group_breakpoint` translates a member's hit
for clients. The first stop a temporary breakpoint causes in any member
deletes the group's breakpoint.

The other session settings are shared in the backend, not by the driver.
A group controller that serves a client's change to signal policy,
exception stops, exec stops, stepping into runtimes, views, or catchpoints
records it in `Following`'s settings, and every group controller takes the
changes it lacks at the start of each message it serves, a new member before
its first. Taking a change repeats the client's request, so taking it twice
changes nothing. Catchpoint ids come from the group, so a member's
catchpoint stop names the group's id without translation. A DAP member's
session therefore makes no system-call catchpoint of its own.

With `StopScope::Group`, one member's stop pauses every other running
member. `GroupEvent::GroupStopped` is published once none is left running.
A stop that another member made while the group was stopping is queued.
`continue_all` reports a queued stop before it resumes anything;
`resume_all` drops the queue, for a client that already shows each
member's stop. With `StopScope::Process`, each member's stop is reported
alone.

Ending the group ends every member, killing launched processes and
releasing attached ones, then the watched processes, and then joins the
tracer thread. `GroupHandle::detach` releases one stopped member; the root
cannot be detached alone. A member whose session ends while it still debugs
its process releases it and reports `MemberLost`. An attached group adopts
only the processes started after the attach.

A process must end hosted by one controller, watched, running untraced, or
reaped; there is no valid forgotten state. Test group changes for parent
exit, child exit, shutdown, detach, and client disconnect, as
`tests/debugger/group.rs` and `tests/dap/forks.rs` do.

## Memory access

Process memory reads can cross mappings, encounter short availability, and
overlap trap sites. Return the bytes read and a typed completion state. Do not
turn a partial read into an all-zero suffix.

Writes require the current public stop and should report their actual extent.
After a write, invalidate any cached presentation, expression result, or
disassembly that depends on changed state.

## Controller review checklist

For any control-path edit, review these questions:

- Which OS thread executes every native call?
- Can a reply or event be lost if the tracee exits at this point?
- What if a sibling stops, exits, clones, or receives a signal between any two
  native operations?
- Does every failure path release or retain ownership explicitly?
- Are pending signals preserved per tracee?
- Is a public stop emitted only after all live tracees are accounted for?
- Does a stopped mutation validate its `StopId`?
- Are user and execution-plan breakpoint owners independent?
- Can cancellation leave a process, waiter, or reply alive?
- Does the simulator model this native behavior, or should it report a model
  gap?

Use the flight recorder to answer these questions for a failure before adding
new logging.
