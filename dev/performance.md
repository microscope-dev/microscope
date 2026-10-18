# Performance work

Performance changes must use the project's measurements. Do not add one-off
timing prints.

## Code map

| Path | What lives there |
| --- | --- |
| `src/timings/mod.rs` | `span!`, `count!`, measurement storage, and report entry points |
| `src/timings/alloc.rs`, `src/timings/report.rs` | Allocation counting and timing report rendering |
| `bench/baseline.json` | Expected load-allocation counts for smoke checks |
| `bench/overhead.json` | Flight-recorder overhead baseline |
| `bench/reports/` | Checked-in comparison reports used during performance work |
| `src/bin/microscope-tools.rs`, `src/tools/` | Benchmark and overhead helper commands |
| `justfile` | `timings`, `bench-*`, and `profile-*` recipes |
| `dev/profiling.md` | Architecture of microscope's user-facing profiler |

## Timings

Give a new phase one `span!` and count repeated work with `count!` from
`src/timings`. Do not create a span for every row or entry.

Run microscope with `--timings FILE`, then summarize or compare reports:

```sh
just timings FILE
just timings OLD NEW
```

Reports include spans, stops, and requests, so a change can be tied to the
work that caused it.

## Allocation and load benchmarks

`just bench-smoke` compares load allocation counts with
`bench/baseline.json`. Record a new baseline only when the allocation change is
intentional. Use `just bench --compare BASE` to compare a larger run.

## Recorder overhead

`just profile-overhead` measures reference workloads under each recording
preset. It takes minutes and stays outside the normal gate. Compare results
with `bench/overhead.json`; update the baseline only for a deliberate change.

`just syscall-overhead` measures what each system call tracing engine adds to
a call. See [system call tracing](syscalls.md#overhead).

## Profilers

The default shell contains command-line profiling tools. For the larger tools
used interactively, enter:

```sh
nix develop .#profile
```

Use the existing `just profile-*` recipes so measurements use the same build
and environment as their baselines.

## What to measure

Start from the user-visible cost:

- program and debug-information load time;
- allocations and peak memory while loading;
- stop-to-event latency;
- expression or inspection latency for a bounded request;
- stepping and breakpoint-control overhead;
- recorder CPU, memory, target throughput, and service latency;
- profile query and browser rendering latency.

Choose the existing benchmark or fixture closest to that cost. A microbenchmark
is useful only when it isolates a decision the end-to-end measurement cannot
explain.

## Timing spans

A span should name a phase a developer can act on: parsing DWARF, sealing an
image, validating tables, binding modules, or answering one request. Nest
coarse spans to explain totals.

Do not create one span per type, DIE, row, sample, or child. Count those items
with `count!` and time the enclosing batch. Per-item spans change the workload
being measured and can make reports larger than the program work.

Labels must have bounded cardinality. A module or major operation can be a
label; a source variable, address, task ID, or every function cannot.

The timing recorder also tracks allocation totals and emits Chrome trace
events. Keep event order and parent relationships valid when work crosses a
thread boundary.

## Allocation accounting

The development allocator counts allocations, frees, live bytes, and peak
bytes. `bench-smoke` compares stable load workloads against
`bench/baseline.json`.

When the count moves:

1. identify the changed phase in the timing report;
2. use an allocation profiler if the source is unclear;
3. decide whether the added ownership or index is necessary;
4. record a baseline only after reviewing the exact reason.

Do not hide an allocation regression by widening tolerance or combining
several unrelated baseline changes.

Large immutable data should normally live in the sealed image or a shared
`Arc`, not be rebuilt for each module binding, frame, or client request.

## Load benchmarks

Image loading has several costs: reading files, parsing providers, building
tables, sealing, validating, mapping a cached image, and binding it to a
module. Compare the phase that changed rather than only total wall time.

Test a cold load when changing the loader. Test a cache hit when changing the
container, validation, or mapping. A warm operating-system page cache is not
the same as a microscope image-cache hit; record which one a report measured.

`just bench --compare BASE` gives a larger before/after comparison. Keep the
baseline checkout, toolchain, CPU policy, and fixture set fixed.

## Recorder overhead method

`just profile-overhead` interleaves bare and recorded runs of reference
workloads. It measures target work, service latency where applicable, and
microscope resource use under each preset. The Python workload
(`tests/fixtures/python/overhead.py`, `--only python`) also runs under the
production preset with each Python engine: `prod+live` and `prod+snapshot`
everywhere, and `prod+bpf` only where the tool itself holds `CAP_BPF` and
`CAP_PERFMON`. A stopped snapshot holds the program for the time its
debugger takes to attach, so `prod+snapshot` costs what its pauses do: about
16% at one snapshot every 100 ms in the recorded baseline, against under 1%
for `prod+live`.

Run it on an otherwise quiet machine with the pinned environment. Look at the
distribution and noise, not only one ratio. A change smaller than observed
bare-run variance is not evidence of improvement.

The production preset has a strict CPU budget, but changes can still affect
target cache behavior, memory bandwidth, file I/O, and latency. Keep those
measurements in the report.

## Profiling microscope

Use the profiler that answers the current question:

- `perf` or `samply` for CPU instructions and call paths;
- `heaptrack`, Valgrind, or allocation counts for heap behavior;
- `hotspot` for interactive perf inspection;
- `hyperfine` for repeated command-level wall time;
- Cachegrind/KCachegrind for deterministic instruction and cache comparisons.

The `system-alloc` feature replaces mimalloc when a heap tool cannot observe
it. Use the repository recipes so feature flags and profiles match the intended
measurement.

Do not optimize a debug build artifact if users run the test or release
profile. Confirm the compiler profile and embedded web behavior in the command
being measured.

## Performance-safe design

The largest wins usually come from preserving architecture:

- parse static debug data once into a shared image;
- validate once, then use cheap checked views;
- keep lookup indexes dense and deterministic;
- page large results rather than materializing them;
- pass inspection budgets through nested work;
- batch contiguous memory reads;
- avoid formatting until a client needs text;
- keep recorder hot paths allocation-light;
- cache stop-scoped reads only for the exact stop and request.

Do not trade correctness for an unmeasured speedup. An unsafe unchecked table,
guessed unwind, stale stop cache, or silently truncated value is not an
acceptable optimization.

## Reviewing a performance change

Include:

- workload and command;
- before and after commit or report files;
- relevant phase, count, allocation, and wall-time changes;
- run count and noise;
- hardware or environment facts that matter;
- why any baseline update is expected.

Keep benchmark JSON changes separate enough that a reviewer can see which
code change caused them.
