# Performance work

Performance changes must use the project's measurements. Do not add one-off
timing prints.

## Code map

| Path | What lives there |
| --- | --- |
| `src/timings/mod.rs` | `span!`, `count!`, measurement storage, and report entry points |
| `src/timings/alloc.rs`, `src/timings/report.rs` | Allocation counting and timing report rendering |
| `bench/corpus/`, `flake.nix` | Sources and hermetic build of the smoke corpus |
| `bench/smoke.json` | Reviewed input hashes and smoke resource ceilings |
| `src/bin/microscope-tools/smoke.rs` | Smoke measurement contract and gate |
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

`just bench-smoke` checks major loader regressions. Nix builds four dedicated
C++, Rust, Go, and Zig programs from `bench/corpus/`; it does not use ordinary
test fixtures or the simulator's compiler matrix. The pinned compilers target
baseline x86-64, retain debug information, and use fixed source paths and
private caches. The shell exposes the result as `MICROSCOPE_BENCH_CORPUS`.

Each program loads in a fresh process, with one worker and the image cache
off. The gate ignores host worker and cache environment settings and rejects
explicit incompatible flags. It checks both allocation count and total
allocated bytes against absolute ceilings in `bench/smoke.json`. Wall time,
RSS, hardware counters, and multiworker performance are not portable gates;
use `just bench --compare BASE` for those investigations.

Missing or extra workloads, changed SHA-256 hashes, incompatible measurement
configuration, unavailable allocation counters, and failed loads all fail.
Input/configuration errors are separate from resource regressions. The gate
never regenerates its budgets or skips a mismatching input.

Save measurements and machine/build provenance without changing budgets:

```sh
just bench-smoke --out target/bench-smoke.json
```

The initial ceilings allow 10% over these measured loads, rounded up to the
next 1,000 allocations and 100,000 bytes. This headroom tolerates small
implementation changes while catching duplicated loading and substantial
growth; it is not an exact-count oracle.

| Workload | Measured allocations | Measured allocated bytes |
| --- | ---: | ---: |
| C++ | 51,090 | 11,864,703 |
| Go | 163,908 | 58,694,378 |
| Rust | 645,944 | 139,520,294 |
| Zig | 307,175 | 92,184,222 |

For an intentional loader change, inspect the phases, explain the resource
increase, and edit only the affected ceilings after review. Do not widen
budgets merely to make a machine pass. For an intentional corpus or pinned
toolchain change, rebuild `.#bench-corpus`, verify reproducibility with a clean
rebuild (`nix build .#bench-corpus --rebuild`), then review hashes and resource
ceilings together. Ordinary fixture edits and checkout moves need no updates.
The former `bench --corpus/--check/--record` gate and its baseline are removed;
historical broad benchmark reports remain readable by `bench --compare`.

## Recorder overhead

`just profile-overhead` measures reference workloads under each recording
preset. It takes minutes and stays outside the normal gate. Compare results
with `bench/overhead.json`; update the baseline only for a deliberate change.

`just syscall-overhead` measures what each system call tracing engine adds to
a call. See [system call tracing](syscalls.md#overhead).

The kernel's own BPF statistics need `CAP_SYS_ADMIN` to switch on, which the
`microscope-bpf` wrapper does not grant. To time the sampler's walk of
JavaScript frames alone, build with the `bpf-walk-timing` feature, which loads
a second sampler that brackets each walk with `bpf_ktime_get_ns()` and records
the totals in the profile's setting `v8 walk times`: the walks, their
nanoseconds and frames, the slowest, and the walks that filled all 64 frames.
The shipping sampler has no clock. Build it beside the normal target, so the
tests keep the shipping program, and record `busy.js`, whose `--depth 60`
holds the main thread's walks at the bound:

```sh
just dev --command bash -c '
  CARGO_TARGET_DIR=$PWD/target/walk-timing \
    cargo build --release --features bpf-walk-timing --bin microscope
  /run/wrappers/bin/microscope-bpf target/walk-timing/release/microscope record --node bpf \
    -o walk.usprof -- $MICROSCOPE_NODE/26/node --allow-natives-syntax \
    --sparkplug-filter=sparkplugSpin tests/fixtures/node/busy.js --seconds 10 --depth 60'
```

Record `node -e 0` the same way first, so that node's debug information is
cached: until it is read, the kernel drops samples. On the machine that
measured it, a walk of `busy.js`'s 12 frames took about 2 µs, and one of 64
frames 9 to 12 µs, about 150 to 190 ns a frame.

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
bytes. `bench-smoke` enforces the count and byte ceilings described above.

When the count moves:

1. identify the changed phase in the timing report;
2. use an allocation profiler if the source is unclear;
3. decide whether the added ownership or index is necessary;
4. change an affected ceiling only after reviewing the exact reason.

Do not hide an allocation regression by widening headroom or combining
several unrelated budget changes.

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

microscope can profile itself too: record a recorder with `microscope record
--pid`, through the BPF wrapper. Every Cargo profile builds with frame
pointers (`Cargo.toml`), so its stacks unwind whole however deep rayon nests
its joins; loading debug information costs about 1% more for it.

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
