# System call tracing

Every `record` sees the program's system calls, at the level `--syscalls`
or the preset chooses, and saves them in its capture beside the samples, on
their clock. `report --syscalls` and the browser read them back from the
capture. The debugger's catchpoints stop at calls instead. Two engines
produce the calls, and both turn what they see into one neutral model before
anything formats, counts, or saves it. This page is about those engines and
the model; [profiling](profiling.md) covers the recording around them.

## Code map

| Path | What lives there |
| --- | --- |
| `src/syscalls/` | Neutral model: events, call tables, argument specs, the filter language, descriptors, processes, and statistics |
| `src/present/syscalls/` | strace's text format and the `-c` table, shared by `report` and the browser |
| `src/backend/linux/syscalls/ptrace.rs`, `seccomp.rs`, `info.rs`, `launch.rs` | The ptrace engine |
| `src/backend/linux/syscalls/bpf.rs`, `src/backend/linux/bpf/syscalls.bpf.c`, `bpf/records.h` | The BPF engine: its loader and its kernel programs |
| `src/backend/linux/syscalls/rings.rs`, `pairs.rs`, `stacks.rs`, `unwinder.rs`, `capture.rs` | Each CPU's ring merged by time, each stack paired with its call or signal, stacks and the thread that unwinds them, and captures |
| `src/backend/linux/catchpoints.rs` | Debugger catchpoints |
| `src/profiler/syscall_trace.rs` | The trace a recording runs beside its samples: levels, the filter, save triggers, cuts, and `--print` |
| `src/profiler/ladder.rs`, `src/profiler/session.rs` | The one budget's ladder, whose upper rungs shed call detail, and the recording loop that climbs it |
| `src/profiler/syscalls.rs` | Captures read back, call sites |
| `src/analysis/query.rs`, `src/analysis/kernel.rs` | Calls selected through the samples' query, samples during calls, calls as leaves, and each thread's kernel lane |
| `src/cli/profile/report.rs` | `report --syscalls`, its log and `--summary` table |
| `src/web/syscalls.rs`, `web/src/profile/syscalls.ts`, `web/src/ui/profile/SyscallsView.tsx` | The browser's Kernel pane and its Syscalls tab |
| `src/sim/tracing.rs`, `src/sim/catches.rs` | The simulator's models of the tracepoint stream and of catchpoints |
| `tests/syscalls.rs`, `tests/catchpoints.rs`, `tests/dap/catchpoints.rs`, `tests/web/syscalls.rs` | Integration tests |
| `tests/fixtures/c/syscalls.c` | The fixture most tests trace, one workload per mode |
| `tests/fixtures/c/syscall-load.c` | Rounds of `openat`, `read`, and `close` on N threads, paced or as a burst, for the ring and the reader |

## The neutral model

Both engines emit `TraceEvent`s through a `Sink`: calls with their arguments,
result, and the bytes copied from behind their pointers, plus process
creation, exec, exit, descriptor changes, and losses. Formatting, filtering,
statistics, captures, and the browser all work from these events and never
ask which engine made them.

That is what keeps the engines honest. The dual-engine tests trace the same
program with each and require the same output.

`src/syscalls/table.gen.rs` holds call names and numbers for x86-64, i386,
and x32, generated from the kernel's UAPI headers. A test fails when it is
stale; regenerate it with `MICROSCOPE_UPDATE_SYSCALLS=1`. `entries.gen.rs` names
the kernel function each call enters through. `scripts/syscall-entries.sh`
writes it from a kernel tree.

## The ptrace engine

The ptrace engine launches the program with a seccomp filter that returns
`SECCOMP_RET_TRACE` for the chosen calls, so calls nobody asked for never stop.
It reads each stop with `PTRACE_GET_SYSCALL_INFO`. A running process cannot be
given a filter, so attaching stops at every call. It stops at each call it
sees, so it cannot only count them: `SyscallOptions::check` refuses `counts`
with ptrace.

This engine has its own tracer thread and does not use the debugger's
controller.

A new thread or child can stop, or even run and exit, before the tracer
sees the fork or clone event that made it. Until that event arrives, the
tracee is held stopped (and its end, if it ends, is kept), so the trace
always announces a tracee before anything it does.

## Interpreted frames

A `CallStack` holds native addresses and, in `interpreted`, the frames an
interpreter ran, each naming the native frame it shows before. `Stacks` finds
a process's interpreter once per mapping, by `runtime_model::detect`, and
gives each process its own model, which learns that process's code objects.

At a ptrace stop the thread is stopped, so its Python frames are exact:
`Stacks::unwind_live` reads them through the runtime model over a
`profiler::LiveStop` of that one thread. Each segment goes just before its
activation of the interpreter's loop, which `RuntimeModel::activation` picks
out by symbol name, as the debugger does: CPython's other machinery, such as
`_PyEval_Vector`, is not an activation. Where segments and activations differ
in number, the frames follow the native ones and the stack ends with
`InterpretedUnplaced`. A thread in the loop whose frames could not be read ends
with `InterpretedUnknown`. A native truncation wins over both. A capture
records each frame as an `Interpreted` frame row, and a call's site is its
innermost interpreted frame with a line, if any.

The BPF engine's programs run the sampler's walker (`python.bpf.h`) in
`send_stack`, and send its frames as a `RECORD_PY_STACK` record just before
the native stack, whose record says whether they went. The reader pairs them
with that stack, and drops frames whose own stack did not fit. Dropping
stacks under the budget drops them too. The walker reads a process by its `py_layout` in `LAYOUTS`. The
reader sets that layout once per process, as soon as the process's mappings
show its interpreter (`Stacks::python_layout`), and sets it again after an exec
or a fork. Python need not be running yet: the layout holds the image's
offsets, where the process keeps `_PyRuntime`, and its pid namespace. The
walker then finds the thread's state itself. It reads every interpreter's
states for the one whose `native_thread_id` is the thread's id in that
namespace (`bpf_get_ns_current_pid_tgid`). A thread that starts after the
layout was set is found the same way. No state, more than one, or a search
past its bound gives unknown frames. `Stacks::unwind_sampled` names the frames
through `RuntimeModel::sampled_frames` and places them by the same rule as
`unwind_live`. The kernel's process ids key the map, so the reader sets
layouts only when microscope runs in the initial PID namespace.

### JavaScript frames

V8 runs JavaScript on the native stack, so a Node process's stacks need its
anonymous code as modules before they unwind past the first V8 frame. In a
process that carries V8 (`Stacks::carries_v8`), each anonymous executable
mapping becomes a module as it is mapped: a byte-for-byte copy of node's
builtins (`unwind::code_copies`, the proof the controller uses too) binds
node's image, and any other is V8's generated code, unwound by frame pointers.
They are read at the `mmap` record, not when a stack needs them, because an
isolate unmaps its code range when it is disposed and a worker's calls are
often read after it has gone. A stack that ends in code no module holds reads
the process's mappings once more.

At a ptrace stop, `Stacks::unwind_live` names each frame whose code is V8's
builtins or code it generated through the Node model's `code_frame`, with the
frame pointer and the thread's `fs_base` for its isolate, as the debugger
does: every tier has its line.

The BPF programs run the V8 walker (`v8.bpf.h`, described in
[profiling](profiling.md#javascript-frames-in-each-sample)) in `send_stack`
when the process has a `v8_layout` in `V8_LAYOUTS`, and send a
`RECORD_V8_STACK`. `Stacks::v8_layout` gives the layout once the V8 module is
mapped. `Stacks::sampled_code_frames` places each walked frame before the
native frame with its address and frame pointer (`profiler::positions`), keeps
it only where its code is V8's, and names it by `RuntimeModel::sampled_code_frame`
and its proof. A system call is always made from inside a call, so each
interpreted frame's slot holds the offset of the call it made and has its line;
a compiled frame's line is unknown, as in a sample. As in a sample, a stack
one of whose native frames runs V8's code (`Stacks::owned_at`) ends with
`InterpretedUnknown` where it has no V8 record, or one from before the
layout, and with `InterpretedEnded` where the walk stopped at a frame pointer
it could not follow, as at a call made from code that keeps no frame pointer
in `rbp`; a stack in none of V8's code ends neither way.

## The BPF engine

The kernel programs are C in `src/backend/linux/bpf/`, a directory the
syscall programs share with the profiler's. `build.rs` compiles them with the
clang that the dev shell names in `MICROSCOPE_BPF_CLANG`, against the headers
in `MICROSCOPE_BPF_INCLUDE`, and embeds the objects. aya loads them. `records.h` is the record layout shared with the Rust decoders,
and a test compares the two through the object's BTF.

Rules the engine keeps:

- **It never falls back.** If BPF cannot run, the probe in `bpf.rs` names the
  one missing piece and its fix. microscope never switches to ptrace by itself.
- **It never traces the whole host.** Every program filters by process or by
  cgroup before doing any work.
- **Loss is reported.** Ring overflows, sampling, and calls whose return was
  never seen all become counted losses, never silent gaps.

Signals come from `tp_btf/signal_deliver`, whose arguments are `int sig`,
`struct kernel_siginfo *info`, and `struct k_sigaction *ka`. The BPF context
starts with `sig`; the tracepoint callback's private `__data` is not an
argument in that context. Handled signals retain their siginfo. If reading
it fails, code, sender, and siginfo stay unknown rather than become zeros.

The kernel's group-exit path also emits `SIGKILL` with `SEND_SIG_NOINFO`
(`NULL`), even when another signal caused the exit. These internal wakeups
are not delivery events in microscope. The exit record supplies the actual
fatal signal. Unlike ptrace, this tracepoint does not supply the original
fatal signal's delivery stop or its sender; the BPF engine does not invent
them from the preceding `kill` call.

Worker exits and unfinished calls come from `kprobe/exit_notify`, before
the task leaves its thread group. Terminal syscall entries (`exit` and
`exit_group`) also use kprobes, in both attachment modes. Kprobes disable
preemption before the BPF recursion guard. BTF tracepoints and fentry do
not: an exiting task can preempt another task's callback on the same CPU,
then be skipped by its per-CPU guard even though the ring has room. The
call hooks stay on tracepoints and fentry, and the engine recovers what
their guard skips (below).
The kprobe PMU attaches without needing permission to read tracefs.

The final process exit comes from `fentry/do_notify_parent`, only when the
task is the leader, no live threads remain, and `signal->nr_threads` is
one. At that point every worker
has finished its exit callback and left the thread group. The final record
therefore follows all worker records. It uses `group_exit_code` when
`SIGNAL_GROUP_EXIT` is set, otherwise the leader's `exit_code`, matching the
parent's wait status. A worker record keeps that task's own `exit_code`;
ptrace can instead report an already-selected group status for a worker.

Do not elect the final callback by reading `signal->live == 0` before
group membership is removed: several callbacks can see zero, and some may
not have started yet. Removing the PID target there both duplicates
process exits and hides later worker exits. Target removal now follows the final record.
For cgroups, a marker on the leader records ownership without testing the
notifying task's cgroup, and consumes it once even if reparenting notifies
again. Ignored SIGCHLD still reaches this hook; no parent reap is required.
The reader also checks the thread count before treating a zombie leader as
finished, and waits for target removal or disappearance from `/proc` before
its lost-record fallback ends a trace.

How it attaches:

- By default it uses `tp_btf` tracepoints, which every process on the host
  pays for: about 33 ns a call, in or out of the trace.
- For a `--syscall-filter` of at most 32 calls it attaches `fentry`/`fexit`
  to their kernel functions instead, which costs other processes nothing.
  A call that BTF does not describe, a call with no arguments (whose wrapper
  pahole leaves out), or an x32 call on a kernel without x32 makes the list
  use tracepoints. microscope says why.
- Process completion always needs `do_notify_parent` in kernel BTF and an
  attachable fentry hook, whatever the calls attach to. Missing
  support is an error, not a return to the racy shared-counter check.
- Exit tracing also needs an attachable `exit_notify` kprobe and, when
  terminal calls are selected, kprobes on their syscall wrappers. Attachment
  failures are errors, not a fallback to preemptible exit hooks.
- When any tracer attaches to or detaches from a kernel function, the kernel
  skips the old trampoline's `fexit` programs for calls in progress, and
  counts nothing. The engine sees a call that entered and never returned and
  reports it lost, unless the call was `exit`, `exit_group`, `execve`, or
  `execveat`, which end without returning.
- The call programs run with preemption on, and the kernel's recursion
  guard (`prog->active`) is per CPU, not per task. A task preempted inside
  `sys_enter` makes the kernel skip `sys_enter` for every task that enters
  a call on that CPU until it runs again, and count only
  `recursion_misses`. Those skips are mostly of processes outside the
  trace, so the engine does not add the call programs' skips to its
  losses; it counts the other programs' skips there. A traced thread's
  skipped exit leaves an entry that its next call counts lost as unseen. A
  skipped entry leaves a call with no entry at its exit, which a launched
  program's trace would otherwise drop as begun before tracing. Task
  storage's `known` marks a thread that entered a traced call, or that the
  trace saw made or executing a program, and such a thread's call is shown
  at its exit with its entry unseen. A call whose entry and exit are both
  skipped leaves no trace and no count.

How it reads calls:

- Pointer arguments are copied at exit, as each call's spec in
  `src/syscalls/specs.rs` says. `execve`, `execveat`, and `rseq` are copied at
  entry, because their memory is gone or rewritten by exit.
- A worker that executes becomes its process's leader. Task storage keeps
  its original id in the trace's PID namespace. Before its terminal call,
  a handoff record moves the captured entry to the new id, only when the
  process and entry timestamp match. This also covers exec failures after
  the id changes, which emit no successful exec event. A displaced leader
  entry stays visibly unfinished; a lost handoff counts as a ring loss.
- Each CPU has a `BPF_MAP_TYPE_RINGBUF` of its own in `EVENTS`, and
  `rings.rs` merges them by time: 2 MiB each (`DEFAULT_RING`), or 8 MiB
  (`STACK_COPY_RING`) when the recording copies stacks, whose 8 KiB copies
  make a call's records about 8.6 KiB against some 440 bytes with the
  kernel's walk alone and 160 without stacks. The kernel charges a ring to
  the memory cgroup of the process that made it, not to
  `perf_event_mlock_kb` or `RLIMIT_MEMLOCK`; a cgroup that cannot hold the
  rings gets smaller ones, down to 256 KiB (`SMALLEST_RING`).
- A record that does not fit counts one loss in `LOST`. A stack goes before
  the call or signal it belongs to; when the stack does not fit, that record
  carries `CALL_STACK_LOST`, and the reader shows it with an empty stack
  ending `no stack` rather than pair it with the thread's last stack. A call
  whose entry record (`execve`, `rseq`) went out is thus never left
  unfinished by a lost stack.
- A call that waits, for another thread, a device, or a clock, has its stack
  taken as it begins rather than at its exit (`waits` in `bpf.rs`: reads,
  receives, polls, sleeps, waits for children and signals, and `futex` as
  its operation says). Taken at the exit, the stack cost lands on the woken
  thread's way back to the lock or data it waited for: under `dev`, on 8
  threads taking turns at a lock, about 1.4 µs a wakeup, which held the
  lock's next owner up. Taken at the entry, it costs the thread while it
  waits: on a request and reply over a socket, what `dev` adds to the median
  fell from 8.2 µs to 4.6 µs. It still delays a waiter before the kernel
  reads the futex word, so on a contended lock more `futex` waits fail with
  `EAGAIN`, and fewer sleep: 4,000 to 8,000 in two seconds of the lock
  workload, against about 200 with stacks at exit.
  A call still waiting as the trace ends then has its stack too. The
  call can return on another CPU, so this stack carries the call's entry
  time, by which it is merged; `pairs.rs` pairs it with the call that began
  then, not with whatever the thread sends next. A counted call kept because
  it failed or was slow takes its stack at its exit, since its entry cannot
  know it will be kept.
- The reader copies everything the ring holds out of it and handles the
  batch: it pairs each stack with its call or signal, and gives each event
  to the recording's sink. Each stack is unwound on a thread of its own
  (`syscalls/unwinder.rs`), through the process's modules as they are when
  the reader hands it over (`Stacks::unwinding`, an `Arc` of the module
  list that later mappings replace rather than change). The reader keeps
  each event, in the order of its record, until its stack comes back, then
  finishes the stack (`Stacks::finish_sampled`: modules read again for code
  no module held, and the interpreter's frames) and gives the event to the
  sink. Stacks come back in the order they were sent. At most 2,048 events
  wait (`MOST_PENDING`; with stack copies each holds 8 KiB); past
  that, the reader waits for their stacks rather than drain more, and the
  ring fills as before. Before each tick, the reader gives the sink every
  waiting event, so what a tick tells the sink follows them. A panic while
  unwinding is raised again on the reader, which ends the trace as it did
  when the reader unwound the stack itself.
- Frame-pointer mode copies no stack bytes. If its chain ends short,
  `Unwinding::unwind` uses the last frame's
  `FramePointerRule` to distinguish a missing frame pointer from an ended
  chain or missing unwind information. A nonempty DWARF copy that runs out
  retains `StackCopyExhausted`. The release Rust syscall fixture exercises
  both failures, including calls from worker threads.
- Unwinding is cached per image address: the code role and frame-pointer
  rule (`FrameRules`, one per thread), the call-frame row
  (`DwarfUnwindInfo::row`), and the symbol and line (`Stacks::frame`). On a
  Ryzen 9 9950X, a call with a copied stack costs the reader about 5 µs and the
  unwinder about 3 µs, so the reader, mostly naming frames and formatting
  them, sets the rate a trace keeps up with: about 200,000 calls a second
  from one thread, against 135,000 when the reader unwound them too. One
  unwinder is enough: two or three showed no more calls and used more CPU.
  The handoff costs about 6% more CPU in all. On a machine with no idle
  CPU, the unwinder only competes with the reader, and the rate is as
  before. While a batch is handled, the ring fills; the ring's size only
  decides how long a burst or a stall it rides out.
- Calls in progress live in task storage. An `iter/task` program lists them
  every 100 ms tick, for a watched recording's Blocked now table.
- Statistics are a per-CPU array with a row per chosen call. For cut
  recordings the kernel counts into one half of the rows while microscope
  reads the other. Switching halves updates an array-of-maps (`FENCE`), which waits for
  running programs, so counted segments sum exactly.
- Settings fixed at load, such as the PID namespace, are `.rodata` globals,
  so the verifier drops the code they turn off.
- Loading is mostly verifier time. The verifier walks every step of a plain
  loop, so the long ones (counting a string array, searching for a Python
  thread's state) run through `bpf_loop` on 5.17 and later, where it checks
  the callback once. That cut `record --syscalls all -- /bin/true` from about
  0.53 s to 0.2 s. Older kernels keep the plain loops (`loop_helper`).

The recording's one budget measures microscope's own CPU time, plus the
programs' run time when `kernel.bpf_stats_enabled` is on. Over it, the
ladder in `src/profiler/ladder.rs` sheds one rung a period: stacks, strings,
all but one call in 16, every call but the failed and slow, then samples.
The engine is told each rung through `BpfOptions`; calls it lets go are
still counted in the kernel, and paths are never cut. Under budget for
`HOLD`, a rung comes back.

## Privileges in development

Only the BPF tests need privilege. They live in `mod bpf` in
`tests/syscalls.rs`, are ignored by the normal suite, and start microscope
through a launcher that grants `CAP_BPF` and `CAP_PERFMON`:

```sh
just test-bpf                       # through /run/wrappers/bin/microscope-bpf
just test-bpf LAUNCHER -E 'test(/cgroup/)'
```

The default launcher is the wrapper that `nixosModules.microscope-bpf` installs.
Nothing else in the suite runs with privilege, and no test uses sudo.

## Catchpoints

While any catchpoint exists, the controller resumes threads with
`PTRACE_SYSCALL`, so every call stops at entry and exit. `catchpoints.rs`
judges each stop against the catchpoints and their conditions and resumes
the rest. With no catchpoints, run control is unchanged.

A catchpoint's stop carries the call, its arguments, its result at exit, and
its path, which the evaluator exposes as `$arg0`–`$arg5`, `$ret`, and
`$path` (see [expressions](../docs/expressions.md)). A step whose single step makes a system
call, including the step over a breakpoint's trap, has to pass through the
call's two stops; `catchpoints.rs` handles that case.

Each thread's `CallPosition` says where it stands: outside any call the
controller knows of, past a call's entry stop with what the entry read, or
at a ptrace event inside a call whose entry went unseen. In either of the
last two, the thread's next step finishes the call, so it resumes to the
call's exit stop. Only a resume with `PTRACE_SYSCALL` keeps a call's entry
for its exit; any other resume forgets it.

Two threads can reach caught calls at once, as when both return from
`clone3`. One stop reports both: the stop names one thread, and each
thread's snapshot carries its own call as its reason. A test that waits
for one thread's catch must look at every thread's reason, not only at the
thread the stop names. The simulator's `CatchCoHit` mark reaches this, and
catch accounting fails a run that resumes such a thread unreported.

The simulator models catch stops with numbered K-SYS rules, each documented
and pinned by a dual run in `src/sim/conformance/kernel.rs`.

## Captures

A recording's capture holds its calls. `capture.rs` writes them;
`src/profiler/syscalls.rs` reads them back into events, so the browser and
`microscope report` reuse the same presentation.

A call's site is `Profile::syscall_site_name`: `syscall_site`, or else the
stack's innermost function. The browser's grouping by site and `report`'s
table of sites (`Profile::syscall_sites`, printed through
`present::syscalls::summary::labelled`) both use it, so they agree.

A recording starts its trace at the profiler's launch gate, so it sees the
program's first call, and merges it with the samples on one clock. With
`--window` or `--continuous`, each saved file gets its own span's calls. A
watched recording, as the browser's, publishes the calls so far beside each
snapshot of its samples, with the calls its threads are blocked in; an
unwatched one copies none, which would cost it CPU for nobody.

## Times in the text

Every engine times calls in nanoseconds. `report --syscalls` prints each
call's time since the recording began in seconds and its length in the unit
that reads best (`present::syscalls::duration`). The `--summary` table keeps
strace's `-c` columns, in microseconds.

## Testing

- Pure parts, such as the filter, decoders, statistics, and the budget's
  steps, have unit tests, and the filter has a fuzz target
  (`syscall_filter`).
- `tests/syscalls.rs` records with `record` and reads back with
  `report --syscalls`, using the ptrace engine, which needs no privilege.
  strace is only an oracle there: a few tests compare decoded arguments with
  one strace run's. Its `bpf` module runs the dual-engine tests, the budget,
  and the cut recordings through `just test-bpf`. No test asserts how long a
  call takes: that depends on the machine.
- `tests/catchpoints.rs` covers catchpoints through `Scenario`.
- `tests/python/syscalls.rs` records `tests/fixtures/python/syscalls.py`
  on three interpreters. It checks each call's Python lines, a second
  activation that C began, the text, and a saved trace's call site. It also
  checks a catchpoint's backtrace. Its `bpf` module holds the BPF engine to
  the same frames, running the fixture under `--hold` as
  the Node tests below do (`tests/support/held.rs`). Its probe is `probe`'s
  open of `/dev/random`. The fixture lives until the test has read its
  calls: the engine reads a frame's code object as it drains the record,
  and a program that has exited leaves that code unreadable.
- `tests/node/syscalls.rs` traces `tests/fixtures/node/syscalls.js`, which
  prints the call sites V8 reports (`CallSite`) on the line before each
  `openat`, in every tier and on a worker, and holds each engine's frames to
  them. Its BPF tests run the fixture under `--hold`: it opens a probe path
  until the trace names its JavaScript frames and the test writes `go` to its
  stdin, then makes its calls, and its worker lives until stdin ends. They
  check that the BPF engine's frames agree with V8's and with the ptrace
  engine's. Each comparison has a self-test that it fails on each lie.
  Another traces `tests/fixtures/node/frameless.js --opens`, whose opens of
  `/dev/zero` an addon (`frameless.c`) makes with 8 in `rbp`: each must name
  the function that made it or end saying its JavaScript frames are missing.
- `pairs.rs`'s property sends each thread's stacks, frames, calls, and
  signals, losing any of them, in any interleaving of threads, and checks
  that each call and signal gets its own stack and frames or none. In
  `tests/syscalls.rs`, `a_waiting_calls_stack_is_taken_as_it_begins` moves
  a thread to another CPU while it waits in each read, and checks that each
  read, and the last, still waiting as the trace ends, has its stack.
- Test builds have the `bpf-faults` feature, whose syscall programs skip
  the entry of a getppid whose first argument is `0x5ca1ab1e`, as the
  kernel's guard does. `a_call_whose_entry_the_kernel_skipped_is_shown_unseen`
  makes one on the first thread and one on a new thread, each its thread's
  first traced call, under fentry and tracepoints, and checks that each is
  shown with its entry unseen.
- `a_burst_of_copied_stacks_loses_no_calls` traces 6,600 calls of
  `syscall-load` with copied stacks, made in about 10 ms: 57 MB of records,
  which the rings hold whatever the reader does meanwhile, so that
  a loaded machine cannot fail it. The reader's rate is measured, not
  tested, since a loaded machine slows it: run `syscall-load 1 ROUNDS RATE`
  under each build, alternating, and compare the losses and microscope's
  CPU time.
- The BPF tests each map perf rings on every CPU, which count against the
  user's perf memory; test builds lease it, as every recording in tests
  does (`src/backend/linux/perf/lease.rs`). Node's BPF tests and Python's
  BPF profile tests run one at a time in the `bpf-profiles` group, so that
  Node's spinning programs never run beside the Python tests that bound
  their unread samples (`.config/nextest.toml`).
- The browser's view is covered by `tests/web/syscalls.rs`, Vitest, and
  `web/e2e/syscalls.spec.ts`.

Run `just test-bpf` after changing the BPF programs or their loader, and
`just stress` and `just sim 600` after changing catchpoints.

## Overhead

`just syscall-overhead` times a `getppid` loop bare, recorded at `counts`
and at `calls` (the `dev` preset, with its stack copy), and unrecorded beside
a recording of another program, the tax every process on the host pays for
the tracepoints. Compare a change with
`--compare bench/syscalls-overhead.json`. On the machine that recorded it on
13 November 2026, `counts` added about 130 ns a call, `calls` about 1.1 µs,
and the untargeted loop about 70 ns.

The machine's own speed drifts over weeks. Before re-recording the baseline
for a slower result, run the commit that recorded it on the same day to see
whether the code or the machine changed.
