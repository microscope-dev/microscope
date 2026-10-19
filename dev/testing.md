# Testing

Tests protect behavior, invariants, and cleanup. Prefer a small number of
high-value scenarios over assertions that restate implementation details.

## Code map

| Path | What lives there |
| --- | --- |
| `tests/support/mod.rs` | `Scenario`, fixture lookup, process cleanup, and shared assertions |
| `tests/support/dap.rs`, `web.rs` | DAP and web test clients |
| `tests/support/flight_recordings.rs`, `memory_cap.rs` | Failure artifacts and per-process memory limits |
| `tests/debugger/` | Public debugger workflows |
| `tests/dap/`, `tests/web/` | Client protocol and server behavior |
| `tests/go/`, `tests/tokio/`, `tests/python/` | Runtime-specific integration suites |
| `tests/support/truth.rs`, `tests/fixtures/python/truth.py` | Fixtures' reports of themselves, and their reader |
| `tests/fixtures/` | Native source programs, view files, and fixture packages |
| `tests/golden/` | Deterministic simulator programs and manifests |
| `scripts/build-test-programs.sh`, `scripts/golden.sh` | Fixture and golden-program builds |
| `scripts/contained.sh`, `.config/nextest.toml` | Resource containment and nextest process policy |
| `loom/` | Loom models of concurrent protocols, built from their source |
| `justfile` | Supported test, stress, simulator, web, and gate commands |

## Workflow

Write the test before the fix and run it while the bug is present. This proves
that the test catches the problem. Do not prove it later by reverting a
completed fix.

Run focused tests while developing. Run the full gate once the change is
ready. Expensive sweeps and stress runs should not follow every edit.

Namespace scenarios require unprivileged user, PID and mount namespaces.
The pinned shell includes `util-linux`. A host policy that blocks `unshare`
is a test setup failure; run the gate where these namespace scenarios can run.

## Debugger scenarios

Use `tests/support::Scenario` for real debugger workflows. It uses the public
request/event path, records a transcript, applies deadlines, shuts the
debugger down, and verifies that the inferior was reaped.

`Scenario::launch` reads a fixture's debug information once per test process
and shares it among that test's scenarios. Use `Scenario::new` when the test
changes the program.

Use `support::ScratchDir` for temporary files and `support::ExternalProcess`
for attach targets. A failed test must not leave a process or file lifecycle
unmanaged.

Run integration tests with nextest. A test process may have only one live
tracing session.

## Synchronization

A test may wait only for something it can observe: an event, debugger state, a
`/proc` fact, or a fixture output line. Polls must fail at a deadline. Do not
sleep and assume the inferior reached a point, or assert that an event did not
happen within an arbitrary window.

Fixtures synchronize their own threads. Before attaching, pausing, or sending
a signal, wait for the fixture to expose the state the assertion needs.

## Fixtures

Native sources live in language directories under `tests/fixtures`. Each Go
executable has its own package directory. Scenario filenames describe the
program without repeating the language.

Rust tests launch built fixtures; they do not invoke compilers. Build fixtures
with `just build-test-programs`. The script builds in parallel. Any step that
reads an output must first call `wait_for` for those outputs or `wait_builds`
for all active compiles.

Use a compact compiler and linker matrix when generated code changes behavior:
GCC and Clang, optimized and unoptimized, PIE and non-PIE, and frame pointers
on and off where relevant.

## Bounds

Every test process aborts if its live heap exceeds 1 GiB. Tests and stress runs
also execute in memory-capped scopes. Every output-collection loop must consume
input on each pass.

Run fuzzers, broad parameter sweeps, and wide runs only through
`scripts/contained.sh` or a Just recipe that uses it.

## Flight recorder

Development builds record client requests, ptrace calls, wait statuses, stop
classification, events, and panics under `target/flight-recorder`. Read the
recording before adding temporary logging.

A failing scenario keeps `tests/<suite>/<test>.log`; DAP tests may also keep an
adapter log. Ordinary runs stream under `runs/`, retain the latest 20, and
update `latest.log`. Set `MICROSCOPE_FLIGHT_RECORDING=PATH` to choose a path or
to an empty value to disable recording.

Record new native control paths with `record!` or the `Recorded` ptrace
wrapper. Recording must never change what the inferior observes.

## Test layers

Choose the highest layer that proves the behavior without making the test
needlessly slow or fragile.

| Layer | Use it for | Avoid it for |
|---|---|---|
| Pure unit test | Parsers, reducers, indexes, validation, arithmetic, formatting rules | A public workflow already covered by a scenario |
| Library integration test | Requests, events, process state, cleanup, real compiler output | CLI spelling or terminal decoration |
| CLI test | Argument parsing, command files, batch status, rendered terminal output | Core debugger semantics |
| DAP test | Protocol mapping, handles, capabilities, client lifecycle | Behavior already established below the adapter |
| Web unit/component test | Store transitions, URL state, rendering, sandbox behavior | Real ptrace behavior |
| Web end-to-end test | Browser/server integration and visible user workflows | Every branch of a pure helper |
| Simulator | Long interleavings and oracle checks over modeled behavior | Kernel behavior without conformance evidence |
| Loom model | Every schedule of a small protocol of threads and locks | Async tasks, syscalls, whole workflows |
| Fuzz target | Structured malformed input and parser/validator bounds | Expected user workflows |

Tests can cross layers when the boundary itself is the subject. Avoid
duplicating the same assertions at every layer. One debugger scenario plus one
thin client mapping test is usually stronger than three full copies.

## Scenario lifecycle

A `Scenario` owns the debugger, its handle, event subscription, transcript,
deadline policy, and inferior cleanup. Use its operations instead of manually
coordinating request and event futures.

A typical scenario should:

1. launch or attach through a support constructor;
2. wait for the initial event or synchronized fixture marker;
3. perform one public operation at a time;
4. assert replies and resulting events separately where ordering matters;
5. inspect state through `DebuggerHandle`;
6. call `shutdown` on every successful path.

The harness finalizer treats an unconsumed process or missing shutdown as a
test failure. Do not bypass it with `mem::forget`, detached tasks, or a raw
child process.

The transcript should contain user-visible operations and enough state to
understand a failure. Keep assertions close to the operation they establish;
an enormous expected transcript is hard to review and tends to preserve
irrelevant formatting.

## Launch versus new scenarios

`Scenario::launch` uses debug information cached once in the test process.
This is the normal choice for a checked-in fixture. It keeps the suite fast
without using the user's on-disk cache.

`Scenario::new` loads from the program for that scenario. Use it when the test
changes the executable, debug file, source map, view file, or another loader
input. A test of loading behavior must not accidentally pass through shared
preloaded metadata.

For attach tests, use `ExternalProcess`. It owns the target and provides
observable output synchronization. Test both ordinary detach and debugger
failure where the process is expected to survive.

## Deadlines

Every blocking test operation needs a deadline with a failure message that
names what was awaited. A deadline is a test failure, not permission to proceed
with incomplete state.

Negative timing assertions are forbidden. “No event arrived for 100 ms” does
not prove that no event can arrive. Instead, perform an operation that creates
an observable boundary, then inspect state or the event stream at that
boundary.

If a test needs a retry loop over `/proc`, memory, or output, each iteration
must consume or observe new information and the loop must end at a deadline.
Busy polling with no yield can starve the controller and manufacture failures.

## Native fixture design

A fixture is a small deterministic program, not a second test harness. Give it
named source markers and simple synchronization points. Keep the behavior that
produces the debugger state separate from the behavior that announces it.

Useful synchronization mechanisms include:

- writing a complete line after reaching a barrier;
- waiting on a pipe, eventfd, condition variable, or atomic state;
- exposing a stable `/proc` fact;
- stopping at a named function whose entry is itself the observed event.

Do not use sleeps to order fixture threads. Do not make success depend on OS
scheduler luck. A fixture loop must have a bounded input or be terminated by
the scenario owner.

Put sources under `tests/fixtures/<language>`. A Go executable needs its own
package directory. Keep one conceptual program per fixture unless sharing is
needed to hold a compiler matrix constant.

Source markers should describe their role, not a test line number. Tests find
the marker text and derive the line. This keeps harmless source edits from
requiring numeric updates.

## Building fixtures

`scripts/build-test-programs.sh` is the only fixture build entry point. It
builds independent outputs concurrently up to the configured job count.

If a build step consumes another output, call `wait_for OUTPUT...` first. If a
step examines the complete set, call `wait_builds`. A missing wait can read a
previous build's file and pass locally while racing in a clean checkout.

The tokio fixtures build in their own shell because their Cargo dependencies
and target directory have separate handling. All fixture dependencies are
pinned and available to the pure Nix shell; a test run must not fetch from the
network.

Each output's stamp names the checkout it was built from, since outputs keep
its absolute paths in their debug information. A checkout whose `build/` is
a link to another checkout's rebuilds what the other built rather than taking
it as current.

`MICROSCOPE_FIXTURE_JOBS=1` is useful when debugging the build script. It is not
a fix for a dependency race in the script.

## Compiler matrix

Add a matrix dimension only when it changes emitted code or debug information
relevant to the behavior. Common dimensions are:

- GCC and Clang;
- no optimization and optimized output;
- PIE and fixed-address executables;
- frame pointers present and omitted;
- glibc and musl thread/runtime layouts, and older glibc builds;
- selected compiler or runtime versions.

Avoid a Cartesian product without a reason. Pick a compact set where each
variant covers a distinct producer or ABI behavior. Document that reason next
to the fixture list or test table.

## Truth fixtures

A runtime fixture can report what its own runtime knows at a checkpoint, as
`TRUTH` lines on its output: each thread's frames, each frame's locals in a
canonical text, and its code objects' lines. A test stops the fixture there and
compares the debugger's public results against the report. Expected values come
from the program as it runs, never from an earlier run of microscope, so they
stay right when a line moves or an interpreter changes.
`tests/support/truth.rs` reads the lines for the Go, tokio, and Python suites.

Each comparison returns what disagrees rather than asserting, and each suite
proves its comparisons fail on the lies they look for. The Python suite feeds
its checks a dropped frame, a wrong line, a swapped thread, a wrong value, and
a missing local in `tests/python/oracles.rs`.

### Python

`tests/fixtures/python/truth.py` prints a checkpoint's report and parks every
thread until the test lets it go, or aborts for a core dump with `--abort`.
Fixtures run with `-I -B`, so the environment cannot change them and they write
no bytecode beside their sources. `tests/python/session.rs` launches or
attaches to one under a named interpreter.

The interpreters come from the flake and are gathered in `MICROSCOPE_CPYTHON`:
3.12, 3.13, 3.13t, 3.14, 3.14t, 3.15, 3.15t, Debian's 3.13, and Ubuntu's 3.12.
A missing interpreter fails the test. Each scenario family runs only where its
subject differs between them: stacks and locals on every interpreter, attach
on the 3.14 pair, cores on one interpreter per frame shape. The build saves a
core of each core fixture with its report beside it, as
`build/test-programs/python-*.core` and `.core.log`. Every interpreter must
present the same locals alike; a difference fails the test.

The values gallery, `tests/fixtures/python/values.py`, prints the gallery
format the native languages use, one `TRUTH CHECKPOINT PATH summary VALUE`
line per value, then calls the C extension's `reached`. A path is an
expression both Python and microscope read, so double-quoted. The test stops
there through a pending native breakpoint whose condition reads the
function's argument through `(py)`, which proves conditions over Python
values at native breakpoints, and evaluates each path in the Python caller.
The extension is built per interpreter by `scripts/build-test-programs.sh`.

`tests/python/profile.rs` records `tests/fixtures/python/profiled.py`, two
threads that spin in Python and one that sleeps, with each Python engine on
3.12, 3.14, 3.14t, and 3.15. The lines it checks are marked in the fixture by
a `# LINE name` comment, which the test finds in the source. Recording runs
until the profile so far holds what the test checks, or a deadline, then
releases the program by its standard input. `just test` warms the image
cache; without it, a stopped snapshot reloads the interpreter's debug
information each time and holds the program for seconds. Its `bpf` module
records through the `microscope-bpf` wrapper and runs only under
`just test-bpf`; without the privileges, the suite checks that the BPF engine
refuses and names the one it lacks. The BPF records' layouts, their
decoding, and their pairing with samples are unit tests that need no
privilege (`backend::bpf::sampler`, `profiler::record`).

## Unit tests

Unit-test complicated deterministic logic where a failure can identify the
rule directly. Good subjects include stop classification, hit conditions,
address translations, image validation, DWARF-expression evaluation, exact
integer arithmetic, view parsing, capture decoding, and profile aggregation.

Avoid tests whose only assertion repeats a constructor field or private call
sequence. Such tests make refactoring expensive without protecting a user or
invariant.

Property tests are useful for algebraic rules, round trips, and bounded input
spaces. Keep generated cases small enough to shrink quickly and store genuine
regressions under `proptest-regressions`.

## CLI tests

CLI tests own command parsing, aliases, configuration precedence, batch mode,
exit status, suggestions, and terminal rendering. They can use a fake or real
debugger according to the behavior under test.

Do not assert ANSI bytes when semantic tokens or color roles are the contract.
When exact output is the interface, make the fixture and terminal width
explicit and keep the expected text short.

## DAP tests

The DAP support harness starts the adapter, speaks framed JSON, tracks request
sequence numbers, and records events. It also owns terminal requests, fork
children, and adapter process cleanup.

Test the mapping between DAP and the public debugger:

- capabilities match implemented requests;
- stopped, continued, thread, module, output, and terminated events have the
  required ordering;
- frame, variable, source, and memory handles expire at the right boundary;
- cancellation and disconnect release all processes and pending replies;
- child-session and terminal requests have explicit accept and refusal paths.

Use the real-client acceptance scripts only for behavior a protocol test
cannot establish, such as launch configuration shape or client-specific
request order.

## Web tests

`just web-test` runs TypeScript checking, SDK declaration checking, Biome,
Vitest unit tests, and browser component tests. Fake-server transcripts let UI
tests drive deterministic state without starting ptrace sessions.

`just web-e2e` starts real servers and browsers. Use it for routing, joining,
sharing, run control, source navigation, large values, drawings, profiles, and
other complete workflows. Filter one case while developing:

```sh
just web-e2e --project=chromium -g 'test name'
```

Run both Chromium and Firefox before merging a browser behavior change. A
snapshot or screenshot should assert a layout or rendering contract that text
and DOM state cannot express more clearly.

## Executable documentation

Several public reference pages are test inputs:

- expression rows execute against named evaluator worlds;
- view examples parse, bind, and run against a fake typed program;
- tutorial blocks are compared byte-for-byte with fixture source fragments;
- visualizer examples run in the browser sandbox and validate their pictures.

Preserve fence markers and row grammar when editing these pages. If the
language changes, update documentation first, observe its failure, then update
the implementation.

## Cache isolation

No test may read the user's image cache. Under nextest the cache is disabled
unless `MICROSCOPE_CACHE_DIR` names one. `just test` warms `target/image-cache`
and passes that path to tests. Cache-specific tests use scratch directories.

`just stress` intentionally omits the warmed cache so repeated runs cover the
loader. Do not “speed it up” by adding the test cache; that removes part of the
stress coverage.

Configuration and user view discovery are similarly isolated through a test
`XDG_CONFIG_HOME` and explicit paths.

## Cleanup assertions

Lifecycle tests must establish more than a returned success. Depending on the
operation, verify:

- the inferior was reaped or detached as promised;
- attached processes continue with the intended signal state;
- held fork children are accepted, released, or reaped;
- waiter and controller threads terminate;
- pending requests resolve with a result or terminal error;
- adapters and web servers close sockets and child sessions;
- temporary files and continuous-recording workers stop;
- no process remains under `/proc` after its expected end.

Prefer support owners whose `Drop` aborts the test on a leak over cleanup code
that runs only after successful assertions.

## Stress and soak

`just stress` builds once, then runs the suite repeatedly with twice as many
test threads as physical cores and busy loops competing for processors. It
counts cores rather than hyperthreads: a core's second thread adds little, so
counting both overloads the machine past the tests' deadlines. It stops at
the first failure so the failing test's flight recording survives.

Use a count and nextest expression to chase a rare test:

```sh
just stress 100 -E 'binary(dap) & test(/disconnect/)'
```

Soak tests exercise a long-running scenario repeatedly and are ignored by the
normal suite. Set `SOAK_MINUTES` to run them after the stress passes:

```sh
SOAK_MINUTES=30 just stress 1
```

They keep the stress recipe's contained memory/process limits. A soak test
still needs observable progress and per-operation deadlines.

## Fuzzing

Fuzz targets live in `fuzz/` and call feature-gated library entry points. They
cover object and debug parsing, image and capture validation, expressions,
views, disassembly, perf records, and other binary boundaries.

Run fuzzers only through a contained recipe or `scripts/contained.sh`. Keep
corpora and artifact growth bounded. A discovered crash becomes the smallest
deterministic regression test at the owning layer; do not rely on a large fuzz
artifact forever.

## Miri

`just miri` runs the tests of microscope's unsafe code under Miri, which stops
at undefined behavior: an access out of bounds, misaligned, or after a free, a
block freed with the wrong layout, a data race, or a pointer made from an
integer. Miri cannot start processes or call most C code, so these tests live
in their own target, `tests/miri.rs`, and reach the code through
`microscope::unsafe_parts`. When you add or change unsafe code that Miri can
reach, test it there. The same tests run natively with the rest of the suite.

`just miri` also runs the Rust probe SDK's own tests, in
`sdk/rust-probes/src/probes/tests.rs`. They play microscope's part: they read
the rings that threads write and free the rings that exiting threads leave.

A test of their own keeps the loop short: Miri checks only that file, not the
whole crate, so a warm run takes about four seconds. Tests whose names contain
`concurrent` run under several seeds, each a different schedule and
weak-memory outcome; `just miri_seeds=0..256 miri` tries more. Some ordering
bugs show up only at a few dozen seeds, so try more after changing atomics.

Under Miri the test process keeps Rust's default allocator. Miri cannot run
mimalloc, and it checks a freed block's layout only on its own allocator.
`tests/miri_global_allocator.rs` is the exception: it installs the counting
allocator over the C library's as the process's own, so that every thread's
allocations pass through it as they start and as their thread-locals are
destroyed, as in microscope. It gives up the layout check to do so, which is
why it is a target of its own.

## Loom

[Loom](https://docs.rs/loom) runs a concurrent protocol under every schedule
of a few threads, which finds orderings that a test can't force and a stress
run reaches only by luck. `just loom` runs the models in `loom/` in about
two seconds.

A model builds the protocol's own source file, not a copy. The file uses
only the types `crate::sync` names, which are std's in microscope. The `loom/`
crate includes the file with `#[path]` and gives it a `sync` of loom's
types. Its `Mutex` and `Condvar` stand in for loom's, which miss three
things: loom never runs another thread right after an unlock, so a
`try_lock` handoff can't be checked; its `Condvar` never wakes spuriously,
so a wait outside a loop passes; and a model thread's panic poisons the
locks of the others. `loom/src/mutex.rs` says how. Tests that only std can
run sit under `#[cfg(all(test, not(loom)))]`. A model has no clock, so a
timed wait there returns at once, having let the other threads run.

Keep models small: two threads and three or four items find the
interleaving bugs these protocols have. Check what a model adds, not only
that it finishes: the unit-walk models also check that every walk begins
inside its window. `just loom BOUND` sets how many preemptions a schedule
may have: 3 by default, and 4 in `just all`. Each step up takes about ten
times as long, so sweeps over every small case, and models with a third
thread, run with fewer.

Before trusting a new model, break the protocol on purpose and watch the
model fail. Loom passes a model that never reaches the broken code.
`just loom-mutants` does this for every model, in under a minute: it
applies each bug in `loom/mutants/` and then each break cargo-mutants
makes, and fails if the models pass any. Keep a bug there for each thing
the models' `Mutex` and `Condvar` exist to catch, since cargo-mutants
makes none of those. A break that changes nothing goes in
`loom/mutants.toml`, with the reason.

Loom only runs code built on its types: not tokio tasks, rayon, syscalls, or
the kernel side of a shared ring. Model a protocol of threads and locks, and
test the rest the usual ways.

## Choosing the final gate

Every change runs `just`. Add these checks according to scope:

| Change | Additional final check |
|---|---|
| Lifecycle, run control, attach, signals, concurrency | `just stress` and `just sim 600` |
| Simulator model or scheduler | `just sim 600` |
| Web behavior | `just web-e2e` |
| Image loading or hot debug tables | `just bench-smoke` |
| Recorder hot path | `just profile-overhead` |
| BPF programs or their loader | `just test-bpf`, and `just syscall-overhead` for hot-path changes |
| Fixture or golden program | `just build-test-programs`; record a manifest only when intentional |
| Public docs | `just docs` plus executable-doc tests when affected |
| Unsafe code | `just miri` |
| A protocol `loom/` models | `just loom 4` and `just loom-mutants` |

`just all` includes the repository's broad pre-commit checks. Run specialized
long measurements only when the table calls for them; repeating them after
every small edit wastes time and makes failures harder to attribute.
