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
