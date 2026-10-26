# System call tracing

microscope sees system calls in three places: `microscope syscalls`, which
prints them as strace does; `record --syscalls`, which saves them beside a
profile; and debugger catchpoints, which stop at them. Two engines produce the
calls, and both turn what they see into one neutral model before anything
formats, counts, or saves it.

## Code map

| Path | What lives there |
| --- | --- |
| `src/syscalls/` | Neutral model: events, call tables, argument specs, the filter language, descriptors, processes, and statistics |
| `src/present/syscalls/` | strace's text format and the `-c` table, shared by the CLI and the browser |
| `src/backend/linux/syscalls/ptrace.rs`, `seccomp.rs`, `info.rs`, `launch.rs` | The ptrace engine |
| `src/backend/linux/syscalls/bpf.rs`, `src/backend/linux/bpf/syscalls.bpf.c`, `bpf/records.h` | The BPF engine: its loader and its kernel programs |
| `src/backend/linux/syscalls/budget.rs`, `cgroup.rs`, `continuous.rs`, `stacks.rs`, `capture.rs` | Budgets, cgroup targets, continuous traces, stacks, and captures |
| `src/backend/linux/catchpoints.rs` | Debugger catchpoints |
| `src/cli/syscalls.rs` | The `microscope syscalls` command |
| `src/profiler/syscalls.rs`, `src/profiler/session.rs` | Captures read back, and `record --syscalls` |
| `src/web/syscalls.rs`, `web/src/profile/syscalls.ts`, `web/src/ui/profile/SyscallsView.tsx` | The browser's Syscalls view |
| `src/sim/tracing.rs`, `src/sim/catches.rs` | The simulator's models of the tracepoint stream and of catchpoints |
| `tests/syscalls.rs`, `tests/catchpoints.rs`, `tests/dap/catchpoints.rs`, `tests/web/syscalls.rs` | Integration tests |
| `tests/fixtures/c/syscalls.c` | The fixture most tests trace, one workload per mode |
| `tests/fixtures/c/syscall-load.c` | Rounds of `openat`, `read`, and `close` on N threads, paced or as a burst, for the ring and the reader |
| `plans/syscalls.html` | Design record, measurements, and what building it changed |

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
given a filter, so attaching stops at every call, and the CLI refuses unless
`--stop-every-call` is given.

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
`send_stack`, after the native stack, and send its frames as a
`RECORD_PY_STACK` record, which the reader pairs with the next call or signal
of its thread, as it pairs the stack. Dropping stacks under the budget drops
them too. The walker reads a process by its `py_layout` in `LAYOUTS`. The
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

How it attaches:

- By default it uses `tp_btf` tracepoints, which every process on the host
  pays for: about 33 ns a call, in or out of the trace.
- For a list of at most 32 calls it attaches `fentry`/`fexit` to their kernel
  functions instead, which costs other processes nothing. `--attach` chooses.
  A call that BTF does not describe, a call with no arguments (whose wrapper
  pahole leaves out), or an x32 call on a kernel without x32 makes the list
  use tracepoints. microscope says why.
- When any tracer attaches to or detaches from a kernel function, the kernel
  skips the old trampoline's `fexit` programs for calls in progress, and
  counts nothing. The engine sees a call that entered and never returned and
  reports it lost, unless the call was `exit`, `exit_group`, `execve`, or
  `execveat`, which end without returning.

How it reads calls:

- Pointer arguments are copied at exit, as each call's spec in
  `src/syscalls/specs.rs` says. `execve`, `execveat`, and `rseq` are copied at
  entry, because their memory is gone or rewritten by exit.
- Every record goes through one `BPF_MAP_TYPE_RINGBUF`, `EVENTS`, shared by
  all CPUs: 16 MiB (`DEFAULT_RING`), or 64 MiB (`STACK_COPY_RING`) with
  `-k=dwarf`, whose 8 KiB copies make a call's records about 8.6 KiB
  against some 440 bytes with `-k` and 160 without stacks. The kernel
  charges a ring to the memory cgroup of the process that made it, not to
  `perf_event_mlock_kb` or `RLIMIT_MEMLOCK`; a cgroup that cannot hold the
  larger default gets a ring half as large, down to 16 MiB. `--ring` is
  never shrunk.
- A record that does not fit counts one loss in `LOST`. A stack goes before
  the call or signal it belongs to; when the stack does not fit, that record
  carries `CALL_STACK_LOST`, and the reader shows it with an empty stack
  ending `no stack` rather than pair it with the thread's last stack. A call
  whose entry record (`execve`, `rseq`) went out is thus never left
  unfinished by a lost stack.
- The reader copies everything the ring holds out of it, then handles the
  batch on the same thread: it unwinds each stack and names its frames, and
  the CLI formats it. That work, not the ring, sets the rate a trace with
  stacks keeps up with, so it is cached per image address: the code role
  and frame-pointer rule (`FrameRules`), the call-frame row
  (`DwarfUnwindInfo::row`), and the symbol and line (`Stacks::frame`). On
  the machine that measured it, a call took about 7 µs with `-k` and 15 µs
  with `-k=dwarf`. While a batch is handled, the ring fills; the ring's size
  only decides how long a burst or a stall it rides out.
- Calls in progress live in task storage. An `iter/task` program lists them
  every 100 ms tick, for `--in-flight`.
- Statistics are a per-CPU array with a row per chosen call. For continuous
  traces the kernel counts into one half of the rows while microscope reads the
  other. Switching halves updates an array-of-maps (`FENCE`), which waits for
  running programs, so counted segments sum exactly.
- Settings fixed at load, such as the cgroup and PID namespace, are `.rodata`
  globals, so the verifier drops the code they turn off.

`--budget` measures microscope's own CPU time, plus the programs' run time when
`kernel.bpf_stats_enabled` is on, and steps down: drop stacks, cut strings,
sample, then only count. Sampled-out calls are still counted in the kernel.
The steps never go back up and never cut paths.

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

The simulator models catch stops with rules K-SYS-1 to K-SYS-6 in
`plans/simulator.md`, each pinned by a dual run in
`src/sim/conformance/kernel.rs`.

## Captures

`-o FILE.usprof` writes a capture, the profiler's file format, at revision 9.
`capture.rs` writes the calls; `src/profiler/syscalls.rs` reads them back into
events, so the browser and `microscope report` reuse the same presentation.

`record --syscalls` starts a BPF trace at the profiler's launch gate, so it
sees the program's first call, and merges it with the samples on one clock.
With `--window` or `--continuous`, each saved file gets its own span's calls.

## Testing

- Pure parts, such as the filter, decoders, statistics, and the budget's
  steps, have unit tests and fuzz targets (`syscall_filter`,
  `syscall_record`).
- `tests/syscalls.rs` holds the CLI's output to strace's, using the ptrace
  engine, which needs no privilege. Its `bpf` module runs the dual-engine
  tests through `just test-bpf`.
- `tests/catchpoints.rs` covers catchpoints through `Scenario`.
- `tests/python/syscalls.rs` traces `tests/fixtures/python/syscalls.py` with
  `-k` on three interpreters. It checks each call's Python lines, a second
  activation that C began, the text, and a saved trace's call site. It also
  checks a catchpoint's backtrace. Its `bpf` module holds the BPF engine to
  the same frames, with `-k=dwarf`, running the fixture under `--hold` as
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
- `a_burst_of_copied_stacks_loses_no_calls` traces 6,600 calls of
  `syscall-load` with `-k=dwarf`, made in about 10 ms: 57 MB of records,
  which the default ring holds whatever the reader does meanwhile, so that
  a loaded machine cannot fail it. The reader's rate is measured, not
  tested, since a loaded machine slows it: run `syscall-load 1 ROUNDS RATE`
  under each build, alternating, and compare the losses and microscope's
  CPU time.
- The BPF tests each map perf rings on every CPU, which count against the
  user's perf memory, so nextest runs at most two at once: one of the
  `perf-rings` group, and one of `bpf-profiles`, which holds Node's BPF
  tests and Python's BPF profile tests, so that Node's spinning programs
  never run beside the Python tests that bound their unread samples
  (`.config/nextest.toml`).
- The browser's view is covered by `tests/web/syscalls.rs`, Vitest, and
  `web/e2e/syscalls.spec.ts`.

Run `just test-bpf` after changing the BPF programs or their loader, and
`just stress` and `just sim 600` after changing catchpoints.

## Overhead

`just syscall-overhead` times a `getppid` loop bare, under each engine, and
untraced beside a BPF trace. Compare a change with
`--compare bench/syscalls-overhead.json`. On the machine that recorded it,
the BPF engine adds about 70–90 ns a call when counting and about 120 ns when
streaming, and ptrace adds about 5 µs.

The machine's own speed drifts over weeks. Before re-recording the baseline
for a slower result, run the commit that recorded it on the same day to see
whether the code or the machine changed.
