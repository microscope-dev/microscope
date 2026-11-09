# Diagnosing failures

Start with the evidence microscope already records. A debugger failure often
depends on the order of requests, ptrace calls, wait statuses, and events; an
extra print can change that order.

## Code map

| Path | What lives there |
| --- | --- |
| `src/flight_recorder.rs` | Recorder format, destinations, retention, and recording macros |
| `src/backend/linux/recorded.rs` | Native control calls captured by the recorder |
| `tests/support/flight_recordings.rs` | Test log naming, retention, and failure reporting |
| `target/flight-recorder/` | Logs from local development runs |
| `src/sim/report.rs`, `src/sim/audit.rs` | Simulator failure reports, traces, and invariant audits |
| `tests/support/memory_cap.rs`, `src/test_memory.rs` | Test memory-limit enforcement and diagnostics |
| `scripts/contained.sh` | CPU and memory containment for expensive runs |
| `dev/testing.md`, `dev/simulator.md` | Test selection and simulator-specific diagnosis |

## Reproduce narrowly

Find the smallest command or named test that still fails. Keep the same build
profile, fixture variant, nextest process isolation, cache setting, and Nix
environment as the original failure.

Examples:

```sh
just test -E 'test(=debugger::breakpoints::case_name)'
just web-e2e --project=chromium -g 'case name'
just sim-seed SEED
```

Do not replace `just test` with a host `cargo test`: it changes fixture
building, cache isolation, process caps, address randomization, and test
process structure.

## Flight recordings

Development builds record one timestamped line for each client request,
native control call, wait status, stop classification, published event, and
panic.

On a scenario failure, the harness prints the retained path under the test
suite: `target/flight-recorder/tests/<suite>/<test>.<millis>-<pid>.log`. The
name is the run's own, so rerunning the test, or a later stress iteration,
leaves it in place; each test keeps the recordings of its newest 5 runs. DAP
failures can have both a scenario log and an adapter log. Read both
from the beginning of the last successful operation through the first broken
invariant.

Ordinary microscope runs keep recent files under `target/flight-recorder/runs`
and update `latest.log`. Choose a path explicitly with:

```sh
MICROSCOPE_FLIGHT_RECORDING=/tmp/microscope.log just run PROGRAM
```

An empty value disables recording. Release builds compile it out completely.

Read a recording as a causality chain:

1. Which client request began the operation?
2. Which `StopId` or `ExecutionId` did it name?
3. Which native calls changed each tracee?
4. Which wait evidence arrived, in what order?
5. How was each status classified?
6. When did the all-stop become complete?
7. Which reply and event were published?

Do not assume adjacent timestamps came from the same thread. The line records
contain the ownership needed to distinguish controller and waiter activity.

## Adding temporary diagnostics

If the recorder lacks a needed fact, add a `record!` call or route the native
operation through the `Recorded` wrapper. Record inputs, returned evidence, and
stable IDs. Avoid dumping arbitrary process memory or huge tables.

Recording must not perform another process read, change error handling, hold a
lock longer, or allocate in a release build. Arguments to `record!` are not
evaluated in release builds, but they still must not mutate state in debug
builds.

Remove temporary records when an existing durable event now explains the
failure. Keep a new record when it represents a control path future failures
will need.

## Hung tests

The harness should fail at a deadline and preserve a recording. If a command
appears hung outside the harness:

- inspect the latest flight recording;
- inspect process and thread state under `/proc`;
- check whether a waiter is blocked as expected or the controller awaits a
  status no live tracee can produce;
- check bounded request channels for a producer waiting on a client that is
  itself awaiting the reply;
- check fixture output for the last reached barrier;
- check for an orphaned previous test process before rerunning.

Do not extend a timeout until you know the expected operation can legitimately
take longer. A control deadlock and a slow debug-data load need different
fixes.

## Surviving inferiors

When a test reports a surviving process, identify who created it and which
owner should have reaped or detached it. Check normal completion, assertion
panic, request cancellation, controller panic, adapter disconnect, and harness
drop paths.

Use process parentage and trace ownership rather than killing every process by
name. Broad cleanup can hide the lifecycle bug and kill unrelated developer
programs.

For attach, survival may be correct; verify that the process is detached and
running or stopped according to the public contract. For launch, microscope owns
the child and must reap it.

## Intermittent run-control failures

Run the one test through stress with its flight recording preserved:

```sh
just stress 100 -E 'test(=FULL_TEST_NAME)'
```

Compare successful and failing recordings at the first ordering difference.
Common sources are:

- assuming a wait order Linux does not promise;
- losing a pending signal while gathering siblings;
- replying after an event a client expects to follow the reply;
- repairing a breakpoint while a sibling runs;
- treating an exit-phase thread as fully gone;
- accepting a stale stopped request;
- dropping a cancellation path before cleanup completes.

If the behavior is modeled, run or add a simulator action and oracle. If it is
not modeled, probe Linux before extending the model.

## Simulator failures

Replay the reported shortest seed on the same commit. Use `--at STEP` just
before the failure to inspect world, controller, and public state.

Classify it before editing:

- debugger state disagrees with a valid oracle;
- the controller requested behavior the model lacks;
- the model disagrees with its numbered kernel rule;
- the oracle expects something the contract does not promise;
- determinism broke and replay no longer matches.

Follow [Simulator](simulator.md) for the required response. Never keep a seed
as a permanent test.

## Debug-information failures

First separate discovery, parsing, image validation, binding, and stop-time
evaluation.

- A module absent from the list points to mapping or file discovery.
- A missing separate debug file points to identity and search policy.
- An image-cache-only failure points to cache key, serialization, or
  validation.
- A type or function missing in every stop points to static loading.
- A variable missing only at one PC points to scope or location evaluation.
- A wrong runtime address points to image/virtual conversion or binding.
- A short stack points to CFI, register state, memory, or a runtime transition.

Use `--no-cache` to compare a fresh load. Use `microscope-tools` dump commands
and timing reports through existing Just recipes rather than writing ad hoc
object parsers.

Keep the original compiler, optimization, linker, and fixture variant in the
reproduction. Producer differences are often the fact under investigation.

## Expression and view failures

Determine the phase:

- parse errors need the source span and token expectation;
- binding errors need scope, normalized type, and candidate facts;
- evaluation errors need the bound operation and stopped read;
- view selection needs the ordered candidate and rejection list;
- view execution needs checks, budgets, and failed reads;
- visualizer failures need serialized inputs, worker error, and picture
  validation.

`microscope views check` and `microscope views explain` expose binding without
an interactive session. Executable documentation tests provide small fake worlds
for language semantics.

## Web failures

Choose the lowest reproducing layer: store/unit test, browser component test,
replay transcript, real-server Playwright test, or `web-probe`.

Check browser console output, WebSocket traffic, current URL state, and the
`StopId` attached to requests and responses. A stale reply replacing new state
usually indicates a missing request key or invalidation boundary.

For drawing failures, separate debugger input evaluation, wire decoding,
sandbox execution, picture validation, and SVG/canvas rendering.

## Performance regressions

Reproduce with the same report or benchmark command and compare phases and
counts before inspecting profiles. A wall-time change with unchanged work can
come from machine noise; a count or allocation change usually points closer to
the code.

Use [Performance work](performance.md). Do not update baselines during
diagnosis.

## Useful handoff information

A good failure report contains:

- exact command and checkout commit;
- shortest failing test or program;
- target compiler, flags, and relevant runtime version;
- whether cache, optimization, PIE, and frame pointers matter;
- flight-recording or simulator trace path;
- first violated expectation, not only the final timeout;
- whether stress, fresh load, another browser, or replay changes it.

Remove secrets and inspect source paths and target output before sharing logs.
