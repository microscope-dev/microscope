# Developing microscope

This directory documents how to change microscope. It stays in the repository
and is not part of the public documentation website.

Start with [architecture](architecture.md), then read the page for the area you
will change:

- [Process control](process-control.md)
- [Debug information](debug-information.md)
- [Expressions, inspection, and views](evaluation-and-views.md)
- [Runtime models](runtime-models.md)
- [Profiling](profiling.md)
- [System call tracing](syscalls.md)
- [Testing](testing.md)
- [Coding style](style.md)
- [Assertions](assertions.md)
- [Diagnosing failures](debugging-failures.md)
- [Simulator](simulator.md)
- [Performance](performance.md)
- [Web UI](web-ui.md)
- [Documentation](documentation.md)

## Find code by task

| Task | Start here | Read next |
| --- | --- | --- |
| Change public debugger state or requests | `src/model.rs`, `src/protocol.rs` | `src/lib.rs`, [architecture](architecture.md) |
| Launch, attach, stop, resume, or detach | `src/backend/linux.rs`, `src/backend/linux/lifecycle.rs` | [process control](process-control.md) |
| Change stepping or breakpoints | `src/backend/linux/stepping.rs`, `src/backend/linux/breakpoints.rs` | `tests/debugger/stepping.rs`, `tests/debugger/execution.rs` |
| Change watchpoints or memory writes | `src/backend/linux/watchpoints.rs`, `src/backend/linux/writes.rs` | `tests/watchpoints.rs`, `tests/debugger/writes.rs` |
| Load symbols, types, variables, or source lines | `src/debug_info/`, `src/image/` | [debug information](debug-information.md) |
| Change stack unwinding or frames | `src/unwind/`, `src/backend/linux/frames.rs` | `tests/debugger/unwind.rs`, `tests/frames.rs` |
| Change expressions or value inspection | `src/eval/`, `src/inspection.rs` | [expressions, inspection, and views](evaluation-and-views.md) |
| Change views or visualizers | `src/view/`, `views/`, `web/src/visualize/` | `tests/debugger/views.rs`, `web/test/drawing.test.tsx` |
| Change goroutines, tasks, or async frames | `src/runtime_model/`, `src/backend/linux/runtimes.rs` | [runtime models](runtime-models.md) |
| Change profiling or capture analysis | `src/profiler/`, `src/capture/`, `src/analysis/` | [profiling](profiling.md) |
| Change system call tracing or catchpoints | `src/syscalls/`, `src/backend/linux/syscalls/` | [system call tracing](syscalls.md) |
| Change the CLI, DAP server, or web UI | `src/cli/`, `src/dap/`, `src/web/`, `web/src/` | [web UI](web-ui.md) and the client test suites |
| Add or diagnose an integration test | `tests/support/mod.rs`, `tests/fixtures/` | [testing](testing.md), [diagnosing failures](debugging-failures.md) |
| Change simulated kernel behavior | `src/sim/`, `src/backend/linux/sim_edge.rs` | [simulator](simulator.md), `plans/simulator.md` |
| Measure debugger cost | `src/timings/`, `bench/` | [performance](performance.md) |
| Change public or developer docs | `docs/`, `dev/`, `AGENTS.md` | [documentation](documentation.md) |

## Development environment

Every project command runs in the pinned Nix environment. Enter it with:

```sh
just dev
```

For scripts and agents, wrap one command:

```sh
just dev --command just
just dev --command just test EXPRESSION
```

The shell is pure. Tools, compilers, fixtures, and tests must not depend on
host paths or packages. Add a missing tool to `flake.nix`.

## Common commands

```sh
just build-test-programs
just test NAME
just web-test
just sim
just stress
just all
```

Use focused checks while editing. `just all` is the final local gate.

## Static builds

`just build-static` builds a fully static microscope that runs on any x86-64
Linux, at `target/x86_64-unknown-linux-musl/debug/microscope`. `just
build-static release` builds an optimized one, with the web page embedded, under
`release/`. The recipe fails if the binary would link anything dynamically.

These builds link the musl and libunwind from the flake's `pkgsStatic`, so
`flake.lock` pins their versions. The shell's `RUSTFLAGS` set the glibc loader
for development builds. The recipe unsets `RUSTFLAGS` so that Cargo uses the
musl target's own flags.

microscope must not depend on its own C library when it reads a tracee. Write
kernel ABI numbers, such as ioctl requests or the signal numbers a program
uses, as the kernel defines them. Do not take them from `libc`, whose types
and real-time signal range differ between glibc and musl. `just all` builds
the static binary so that the musl build keeps compiling.

## Repository map

| Path | Contents |
|---|---|
| `src/model`, `src/protocol.rs` | Public model and debugger protocol |
| `src/backend` | Native process-control edge |
| `src/debug_info` | ELF, DWARF, Go metadata, and architecture providers |
| `src/image` | Validated immutable debug-information tables |
| `src/eval` | Pure expression parser, binder, and evaluator |
| `src/view` | View parser, binding, and presentation |
| `src/profiler`, `src/analysis` | Recording and profile analysis |
| `src/syscalls`, `src/backend/linux/syscalls` | System call model and tracing engines |
| `src/sim` | Deterministic debugger simulator |
| `src/cli`, `src/dap`, `src/web` | User-interface clients |
| `web` | Browser client |
| `tests` | Public debugger, DAP, runtime, and web scenarios |
| `views` | Built-in views and visualizers |
| `sdk` | Headers and libraries used by debugged programs |
| `plans` | Design decisions and implementation records |

## How a session is put together

A normal debugging session crosses four boundaries:

1. A client creates a `Debugger`, takes a `DebuggerHandle`, and subscribes to
   events.
2. The handle sends a typed request to the controller and waits for its typed
   reply.
3. The controller owns the mutable session, calls the native edge, and
   publishes immutable state after each completed transition.
4. The client renders that state and sends later actions with the `StopId` it
   observed.

The terminal, DAP server, and web server all follow this path. If a feature
exists only in one client, first ask whether it is presentation or debugger
behavior. Presentation may belong in one client. Debugger behavior belongs in
the model and protocol so every client can use it.

Static program facts take a different route. A debug-information provider
reads ELF, DWARF, language metadata, and unwind data into a validated
`ModuleImage`. A runtime mapping binds the image to virtual addresses. Reads at
a stop combine immutable image data with registers and process memory.

Profiles do not use the live debugger protocol. The recorder writes a bounded,
validated capture. Analysis builds trees, costs, timelines, and zones from that
capture. The browser can serve a debugging session or a profile, but those are
different server modes.

## Finding the right layer

Use these questions before editing:

- Is this a stable concept clients should see? Put it in `model` or
  `protocol`.
- Is it a Linux, ptrace, ELF, DWARF, x86-64, compiler, or runtime detail? Keep
  it behind its provider or backend edge.
- Is it formatting shared by CLI, DAP, and web? Put it in `src/present`.
- Is it pure parsing, reduction, indexing, or analysis? Keep it free of
  process control and test it directly.
- Does it read a stopped program? Give it explicit limits and a `StopId`.
- Does it perform blocking process control? Keep it on the controller thread.
- Does it read a source file or other host file? Keep that work outside the
  controller thread.

Avoid placing behavior in a client because that client happened to need it
first. Doing so creates several subtly different debuggers.

## A normal change

For a behavior change:

1. Identify the public result and the layer that owns it.
2. Write the smallest test that observes the result through the intended
   boundary.
3. Run it before the fix and keep the failure.
4. Implement the change without widening unrelated interfaces.
5. Run the focused test and a build or Clippy check for the changed crate.
6. Run the extra checks named by the subsystem page.
7. Run `just` when the change is ready. Use `just all` before committing.

For a refactor, pin the behavior first when existing tests do not make the
boundary clear. Refactors should preserve public transcripts, events, cleanup,
and partial-result behavior.

## Reading design records

`plans/` explains why large features have their present shape. A plan may also
contain rejected approaches, work lists, old measurements, or future phases.
Use the code and tests for current behavior. Use plans for rationale and for
facts that the implementation deliberately depends on.

When a durable rule is missing from `AGENTS.md` or `dev/`, add it here after
the implementation establishes it. Do not turn a work log into current
documentation by copying it whole.

## Keeping a change small

microscope has many interacting models: Linux tracees, public threads, runtime
tasks, source frames, image addresses, virtual addresses, stored values,
presented values, and profile stacks. Make type boundaries carry those
distinctions. A conversion should happen in one named place with the evidence
needed to perform it.

Prefer an explicit unsupported result to a fallback that is correct only for
the fixture at hand. Debuggers are trusted precisely when programs are already
misbehaving; plausible but false output is a serious defect.

## Hermetic environment

The Justfile runs every recipe through `scripts/hermetic.sh`. That script
enters `scripts/dev.sh` unless `MICROSCOPE_HERMETIC=1` already marks the pinned
shell. `dev.sh` uses `nix develop --ignore-env` and keeps only the small set of
user, terminal, Cargo, and microscope variables listed in the script.

This is part of the test contract. It prevents a host compiler, debug file,
view, configuration file, Node package, or profiler setting from changing a
result. If a recipe needs another environment variable, add it deliberately to
the keep list and document why it is an input.

Do not fix a missing command by using an absolute host path. Add the tool to
`flake.nix` and use it through the shell. Do not use `npx`; it can download a
package with the requested name when the locked dependency is missing.

Recipes that intentionally drive a host application say so. Keep that
exception narrow; builds and tests remain pure.

## Build profiles and features

The ordinary development build and test artifacts share the test profile,
which keeps useful debug information while loading target metadata faster than
a fully unoptimized debugger build.

`just build-debug` gives microscope its own full debug information when
debugging the debugger with itself. The next normal build returns to the
standard test profile.

Cargo features have specific uses:

| Feature | Purpose |
|---|---|
| `sim` | Build the sweep and replay binary and simulator-only exports |
| `tools` | Build image, timing, benchmark, and overhead utilities |
| `fuzzing` | Expose structured fuzz entry points and object writing support |
| `system-alloc` | Replace mimalloc for heap profilers that cannot observe it |

Do not make ordinary library semantics depend on one of these development
features.

## Generated files

Generated artifacts checked into the repository have one supported update
path:

| Artifact | Update command |
|---|---|
| `web/src/protocol.gen.ts` | `MICROSCOPE_UPDATE_PROTOCOL=1 just test protocol` |
| Web replay transcripts | `just web-transcripts` |
| `src/debug_info/cpython/layouts.gen.rs` | `scripts/cpython-layouts.sh`, in the dev shell |
| `src/debug_info/v8/layouts.gen.rs` | `MICROSCOPE_UPDATE_V8=1 just test layouts_match_torque` (Torque output and V8 headers) |
| `src/image/v8/slices.gen.rs` | `MICROSCOPE_UPDATE_V8=1 just test slices_match_torque` |
| `src/image/v8/translation.gen.rs` | `MICROSCOPE_UPDATE_V8=1 just test translations_match_torque` |
| `src/image/v8/bytecodes.gen.rs` | `MICROSCOPE_UPDATE_V8=1 just test bytecode_sizes_match_torque` |
| `src/runtime_model/python/printable.gen.rs` | `scripts/python-printable.sh`, in the dev shell |
| Golden manifests | `just golden-record NAME` |
| Benchmark baselines | The owning benchmark's explicit record option, only after review |
| pnpm locks | Run pnpm in the pinned shell after changing its package file |

Inspect generated diffs. Do not edit generated protocol, transcript, or
manifest content by hand.

Build output belongs under `target/` or `build/` and is not committed.

## Source conventions

Public Rust APIs need useful rustdoc. Comments should explain a rule, evidence,
ownership, or non-obvious reason. Do not narrate the next line of code.

Group related code with whitespace and keep modules focused on one boundary.
Avoid unrelated renames or formatting churn in a behavior change; small diffs
make concurrency and binary-format work reviewable.

Unsafe Rust is denied by default. A narrow exception needs a safety comment
that states the caller and data invariants, and it must satisfy the configured
lints. Prefer validated byte views and existing abstractions over widening an
unsafe block.

Unsafe code lives in a few files, which `scripts/unsafe-sites.sh` lists and
`just lint` enforces:

- `backend/linux/sys.rs`: what a child does before exec, the ptrace requests
  nix cannot make, and `tgkill`.
- `backend/linux/perf/sys.rs`: `perf_event_open`, its ioctls, and the ring
  mapping.
- `backend/linux/syscalls/ptrace.rs`: the SIGURG handler that interrupts a
  blocking `waitpid`.
- `timings/alloc.rs`: the counting allocator.
- `cli/syscalls.rs`: `localtime_r`.
- `main.rs` and `cli/config.rs`: removing environment variables before
  microscope starts any other thread, and the global allocator.
- `main.rs` and `bin/microscope-tools.rs`: setting mimalloc's purge delay
  before either starts any other thread.

The rest of the backend reaches these through safe functions. Code that Miri
can run has tests in `tests/miri.rs`, which `just miri` runs (see
[testing](testing.md#miri)). Adding a file to the list needs a reason a safe
crate cannot give.

## Before committing

Review the diff for:

- unrelated changes or generated build output;
- user paths, logs, profiles, core dumps, and other private data;
- changed public behavior without docs;
- changed protocol or schema without generated output and validation;
- lifecycle work without cleanup tests;
- new unbounded loops, collections, channels, reads, or recursion;
- swallowed partial results or generic errors where a typed reason exists;
- baseline changes without a measurement explanation.

Then run `just all`. Add the subsystem-specific long checks from
[Testing](testing.md). Keep the worktree clean after committing.
