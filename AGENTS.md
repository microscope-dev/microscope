# microscope development guide

microscope is a Linux x86-64 native debugger written in Rust. Keep the
implementation small and exact. Unsupported states and partial results must be
visible; never guess when the result could look correct.

## Before changing code

Run project commands inside the pinned Nix environment. A `just` recipe runs
there itself, so `just test breakpoint` from the host is safe. The few recipes
that drive host programs say so in the justfile. For any other non-interactive
command, use `just dev --command`, for example:

```sh
just dev --command cargo check
```

Do not run Cargo, compilers, linters, or tests directly from the host.

Read the pages that match the work:

| Work | Read first |
|---|---|
| Repository setup or an unfamiliar subsystem | [`dev/index.md`](dev/index.md), [`dev/architecture.md`](dev/architecture.md) |
| New data structures, ids, or numeric parameters | [`dev/style.md`](dev/style.md) |
| Run control, breakpoints, signals, or lifecycle | [`dev/process-control.md`](dev/process-control.md), [`dev/testing.md`](dev/testing.md) |
| ELF, DWARF, unwinding, images, or the cache | [`dev/debug-information.md`](dev/debug-information.md) |
| Go goroutines, Rust coroutines, tokio tasks, or CPython | [`dev/runtime-models.md`](dev/runtime-models.md), [`dev/testing.md`](dev/testing.md) |
| Recording, captures, or profile analysis | [`dev/profiling.md`](dev/profiling.md), [`dev/performance.md`](dev/performance.md) |
| System call tracing, its BPF programs, or catchpoints | [`dev/syscalls.md`](dev/syscalls.md) |
| Expressions, inspection, views, or visualizers | [`dev/evaluation-and-views.md`](dev/evaluation-and-views.md), [`docs/expressions.md`](docs/expressions.md), [`docs/views.md`](docs/views.md) |
| Tests or native fixtures | [`dev/testing.md`](dev/testing.md) |
| Diagnosing a failure or reading recordings | [`dev/debugging-failures.md`](dev/debugging-failures.md) |
| Simulator or kernel model | [`dev/simulator.md`](dev/simulator.md) |
| Timings, allocation, or benchmarks | [`dev/performance.md`](dev/performance.md) |
| Browser client or web protocol | [`dev/web-ui.md`](dev/web-ui.md) |
| Documentation | [`dev/documentation.md`](dev/documentation.md) |
| Assertions, invariants, or what happens on a panic | [`dev/assertions.md`](dev/assertions.md) |

## Rules that apply everywhere

- Keep the public model and request/event protocol independent of ELF, DWARF,
  ptrace, and native register layouts.
- `ModuleImage` is immutable static metadata. `LoadedModule` is a runtime
  mapping. Never mix `ImageAddress` and `VirtualAddress`.
- UI clients use `DebuggerHandle`, bounded request channels, events, and
  immutable snapshots. They never share mutable debugger state.
- Only the controller OS thread that created the tracee may call ptrace. The
  waiter may call `waitpid` and send messages to the controller.
- The public debugger is all-stop. Publish a stop only after every live thread
  is known to be stopped.
- A stopped-state mutation must carry the `StopId` that the client observed.
  Reject stale requests.
- Keep physical breakpoint installation separate from user and execution-plan
  ownership. Hide trap bytes from user memory reads.
- Debug-information providers normalize data at their boundary. The evaluator
  does not calculate target layouts or read a process except through its
  traits.
- Resolve source paths while loading metadata, but read source files lazily and
  outside the controller thread.
- Assert internal invariants that the type system cannot prove, and never
  assert on input. Use `debug_assert!` by default and `assert!` only when
  continuing would damage the tracee. See
  [`dev/assertions.md`](dev/assertions.md).
- Production code never calls `unwrap`, `expect`, `panic!`, or
  `unreachable!`; Clippy denies them outside tests. Return an error instead.
- Unsafe code is denied unless narrowly required. Every exception needs a
  safety comment and must pass the configured lints. Unsafe code must be
  tested under Miri.

## Testing

Write a behavioral test before fixing a bug and watch it fail. Use the public
`tests/support::Scenario` path for debugger workflows. Tests must wait for
observable state, enforce deadlines, and leave no inferior process behind.

A protocol of threads and locks whose orderings a test can't force gets a
loom model in `loom/`; see [`dev/testing.md`](dev/testing.md#loom).

During development, run the focused tests and checks that finish in seconds.
Run the broader gate once the change is ready:

```sh
just                 # formatting, Clippy, tests, and web checks
just all             # full pre-commit gate
```

Run `just stress` before merging lifecycle, run-control, attach, or concurrency
changes. Run `just sim 600` for those changes and for simulator model changes.

## Agent harnesses

This file is the instruction set for every agent harness. Claude Code reads it
through `CLAUDE.md`, which imports it, and Codex reads it directly. Keep harness
settings out of this file.

- `.claude/settings.json` and `.codex/config.toml` both run the command guard
  `scripts/agent/require-nix-shell.sh`, which blocks host builds and tests.
- Codex caps the combined instructions at 32 KiB by default. Keep this file
  under that size; check with `wc -c AGENTS.md`.
- Choose the model per session. The repository does not pin one.
