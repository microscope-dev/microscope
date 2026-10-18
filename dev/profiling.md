# Profiling architecture

The profiler records native execution without sharing mutable state with the
debugger controller. It can start a program, attach to one, or run beside an
active debugging session, but profiler workers do not call debugger ptrace
operations.

The main code is split across three layers:

- `src/profiler` records, unwinds, annotates, and reads profiles.
- `src/capture` defines the validated on-disk capture tables.
- `src/analysis` builds call trees, function costs, diffs, timelines, and
  instrumentation-zone results.

The CLI commands `record`, `report`, `convert`, and `symbolize` are adapters
over those layers. The browser reads the same profile and analysis results.

## Code map

| Path | What lives there |
| --- | --- |
| `src/profiler/collector.rs`, `src/profiler/record.rs`, `src/profiler/session.rs` | perf collection, record decoding, and session lifecycle |
| `src/profiler/tasks.rs`, `src/profiler/calls.rs`, `src/profiler/frame_pointers.rs` | Task and thread snapshots, and stack reconstruction |
| `src/backend/linux/reaped.rs` | The exit status of a recorded program that a snapshot's debugger reaped |
| `src/backend/linux/bpf/sampler.bpf.c`, `src/backend/linux/bpf/python.bpf.h`, `src/backend/linux/bpf/sampler.rs`, `src/profiler/interpreted.rs` | The BPF engine for Python stacks: its program, walker, loader, and splice |
| `src/profiler/jit.rs`, `src/debug_info/perf_map.rs` | Perf maps: anonymous code as modules |
| `src/profiler/annotate.rs`, `src/profiler/symbols.rs`, `src/profiler/zones.rs` | Symbol, source, and instrumentation annotation |
| `src/profiler/formats/` | Chrome, folded-stack, and pprof output |
| `src/capture/` | Validated capture file format and table builder |
| `src/analysis/` | Queries, call trees, diffs, costs, and zones |
| `src/cli/profile.rs` | `record`, `report`, `convert`, and `symbolize` command adapters |
| `src/web/profiles.rs`, `web/src/profile/`, `web/src/ui/profile/` | Profile transport, browser model, and profile pages |
| `sdk/c/microscope_kernel.h`, `sdk/rust-probes/`, `sdk/zig/microscope_kernel.zig` | Target-side profiling and instrumentation interfaces |
| `tests/profiler.rs`, `src/capture/tests.rs`, `src/analysis/tests.rs` | End-to-end and format/analysis tests |

## Recording model

The recorder opens perf events for target threads and receives bounded binary
records. Samples can contain process and thread IDs, user registers, a kernel
frame-pointer call chain, and a copied stack region. Other records describe
thread and mapping lifecycle or instrumentation data.

Collection and processing are separate. The collector owns perf resources and
reads event buffers. The processor validates records, tracks mappings and
threads, reconstructs stacks, and appends normalized capture rows.

Treat kernel records as untrusted binary input. Validate sizes, alignments,
sample field order, IDs, and ranges before indexing. A truncated or unknown
record produces a counted problem or an error according to whether the stream
can continue safely.

## Presets and budgets

A preset chooses sample rate, copied stack depth, blocked-time collection, and
an optional CPU budget. Command-line overrides produce one `RecordOptions`
value before recording begins.

The budget controller measures recorder work and lowers sampling duty when it
would exceed its share of one CPU. Keep that feedback deterministic for the
same observations. A budget is about microscope's own cost, not a claim that the
target's total overhead is exactly that percentage.

Do not add work to the sample hot path without measuring it under each preset.
Avoid per-sample allocation where a reused buffer or dense table works.

## Stack capture and unwinding

Perf may provide a frame-pointer call chain and a copy of user stack memory.
Registers plus copied memory feed `src/unwind/sample.rs`, which uses the same
generic unwind loop as the debugger through a sampled-state reader.

The sample has only the bytes captured at interrupt time. It cannot fetch
missing process memory later and must not use current live memory as if it came
from the sample. A stack ends with a typed reason when unwind rules, registers,
or copied bytes are missing.

Frame-pointer and DWARF-derived results can provide different evidence. Merge
or select them according to the explicit stack-copy policy. Never append a
plausible chain after an unproven transition.

Addresses are normalized through the mapping active at the sample's time.
Module unload, remap, exec, and late mapping records matter. A source line is
attached only after the address is bound to the matching module image.

### Code a compiler generated

Anonymous executable memory, where a just-in-time compiler writes code, has
no file to describe it. When the process writes a perf map,
`/tmp/perf-PID.map` (CPython does under `-X perf`), each anonymous range it
covers becomes a module of its own (`src/debug_info/perf_map.rs`,
`src/profiler/jit.rs`). The map is append-only and written while it is read,
so the recorder reads on from where it stopped, and only when a sample lands in
anonymous code no function it named covers; the sample is then unwound again.

Nothing describes those functions' frames, so their frames are proved from
their code, read from the process when the map is
(`src/debug_info/x86_64/frames.rs`). The proof follows the instructions that
fall through from the entry, as call-frame information takes a frame to be a
function of the instruction, and proves nothing past a jump or return. Not
every trampoline keeps a frame pointer: CPython 3.12's does not, and assuming
one would skip its caller. A frame the proof does not reach ends the stack
with no unwind information.

A map names the process's addresses but no build ID proves which code it
describes, so its functions are symbolized as they are recorded, never later,
and a frame in a range outside every function it names belongs to no module.
A map made before its process began was written by an earlier process with
its pid, and is not read; the capture's `perf map PID` setting says why.

## Symbolization now and later

Immediate symbolization loads module debug information on the recording host
and stores normalized frame information in the capture.

Deferred symbolization stores module identities, build IDs, mappings, and
addresses. `microscope symbolize` later finds matching debug files and adds
source facts. The later host must prove module identity; a matching path alone
is not enough.

Keep capture identity independent of local paths so a profile can move between
machines. Record symbolization failures with enough module and address context
to diagnose them.

## Logical task snapshots

CPU samples describe native threads. To present Go goroutines and tokio tasks,
the recorder can take periodic runtime snapshots.

Stopped snapshots pause the target, read one consistent runtime state, and
record the pause duration. Live snapshots read without stopping. Every live
read is subject to change; use runtime consistency checks and omit an unstable
task rather than combining fields from different states.

A live snapshot never falls back to a stopped one: engines do not switch by
themselves. It reads every module the recorder has seen the process map,
through the debug information the recorder loaded for the mapping. Facts that
only a stop would give, such as a thread's thread pointer, come from memory
instead, here from glibc's `_thread_db` descriptors and its lists of threads,
or the tasks that need them are left out with the reason.

Snapshots connect sampled native activity with task identities and record
where parked tasks wait. They do not rewrite the native sample. Analysis can
choose a logical task stack or the original native stack.

A sample names a task only when its innermost frame is code that keeps the
runtime's current task, as Go's compiled code keeps `g` in R14. The runtime
also runs on a goroutine's thread: Go preempts a goroutine that has run for
10 ms with a signal, and the handler and scheduler run on goroutines of their
own or in assembly. Samples there name no task, so a test may not expect every
sample of a busy goroutine to name it.

Bound snapshot frequency, task count, reads, bytes, and time. A program with a
million tasks must degrade explicitly rather than exhaust the recorder.

Python's BPF metadata uses the same host/target thread identity resolver as
the debugger. The live edge proves namespace and process membership before
mapping a CPython native ID to a kernel thread key. Missing evidence never
implies numeric equality. The kernel map holds one Python state per host
thread; multiple states for that host are omitted with a gap. This mapping
runs during metadata refresh, outside sample decoding. Mapping diagnostics keep
eight examples and an omitted count; valid states later in the catalog still
survive. The Python BPF recorder currently requires the initial PID namespace:
perf reports IDs in the event creator's namespace, while the BPF helper uses
initial-namespace IDs. The loader verifies the recorder through an nsfs file
descriptor and refuses a mismatch before launching or attaching. Targets may
run in nested namespaces. Snapshot and live engines support a recorder in a
nested namespace when its proc view can be verified.

### Thread snapshots

A recording's Python engine (`RecordOptions::python`) other than BPF takes
snapshots of threads rather than tasks (`tasks::Reads`). Each thread is a
task row named `thread`, numbered by its id, and its stack a wait row, so
thread snapshots need no table of their own. A stopped one stores each
thread's debugger backtrace, Python frames among native ones. A live one
stores each thread's interpreted frames from one read, which is complete
only when the walk ends where the runtime says it ends and every frame's
code is proved; any other read is counted as changed, never kept shorter.
Each also stores the labels the runtimes give each thread
(`RuntimeModel::thread_labels`), which the recording keeps as label spans on
the thread from the snapshot before to this one.

### Python frames in each sample

The BPF engine (`--python bpf`) runs a program on the sampler's own perf
events (`perf::Events::run_program`, `PERF_EVENT_IOC_SET_BPF`), which aya
cannot attach to an existing descriptor. Events a thread inherits later run
it too, so it is attached before the launched program leaves its gate. As
each sample is taken, the program walks the thread's CPython frames
(`python.bpf.h`, one global function the verifier checks once) and writes
them through that CPU's BPF output event, which `perf::Events::outputs`
points at the CPU's ring, then returns 1 so that perf writes the sample as
always. Records of both kinds share the ring; the decoder tells them apart
by the output events' identifiers (`decode::next_of`).

The recorder pairs each record with the next sample of the same thread and
CPU in ring order, never by time: kernels prepare a sample before or after
they run its program, so either may carry the earlier stamp. A record held
back past the watermark keeps its pairing by going back into the carry just
before its sample; one whose sample never comes is dropped once the
watermark passes it by `interpreted::PAIRING_SLACK`, as its sample was.

The program reads each process through a `py_layout` the recorder fills from
the interpreter's `PythonLayout` once its module is mapped, with where the
process keeps `_PyRuntime` and its pid namespace. It begins each thread at
the state a `tid → py_thread` map names, which the recorder refreshes every
20 ms from the interpreter's thread list (`RuntimeModel::walked_threads`),
when that state's `native_thread_id` is the thread's id in its namespace
(`bpf_get_ns_current_pid_tgid`). Otherwise, as for a thread that started since
the refresh or a reused thread id, the walker looks through every
interpreter's states for the one that names the thread (`py_find`). None, more
than one, or a search past its bound gives unknown frames, never another
thread's. A sample before the layout is known ends in
`Truncation::InterpretedUnknown`. Code objects are named and proved as each
record is drained (`RuntimeModel::sampled_frames`), while the program most
likely still runs; one freed since is unreadable. The syscall engine shares
the walker (`dev/syscalls.md`). Segments go before the activations of the
interpreter's loop that run them, as the debugger places them, and where the
two differ in number the frames follow the native ones and the stack ends in
`Truncation::InterpretedUnplaced`. The record's GIL state becomes label spans,
one per run of samples with one value, and from 3.14 the asyncio task the
thread ran joins the samples that task snapshots claim, as a task a Go or
tokio sample keeps in a register does.

### Reaping the program

A stopped snapshot's debugger runs in the recorder's process and waits for
any child, as a debugger must, and the program a recording starts is the
recorder's child. Any thread of a process may reap any of its children, so
when the program exits as a snapshot attaches or detaches, the debugger's
waiter or the recorder could each take the other's exit.
`backend::reaped` keeps both whole: the recorder reaps its child only once
no snapshot holds it, so a snapshot's debugger sees the exit itself, and
every wait leaves a watched child's status there for the recorder, which
takes it when it finds its child already reaped.

## Off-CPU intervals

Scheduler events delimit periods when a thread is not executing on a CPU.
Store the interval and available cause evidence separately. Being off CPU can
mean blocked, sleeping, or preempted; do not label every interval as I/O or
lock contention.

An unmatched start or end can occur at recording boundaries or after record
loss. Represent incomplete intervals and loss counts. Do not extend them to an
arbitrary timestamp to make totals balance.

## Instrumentation probes

The probes SDK places a static site table and shared ring protocol in the
program. The recorder discovers selected sites, enables them through the
control structure, and decodes records from the ring.

ABI 2 adds run-time sites: `microscope_probes::define` appends a record to a
fixed table of 4096 in the program, whose address the control block holds at
offset 56 once the first is defined. Each record carries the site's kind,
line, name and file, and its own state byte; records name it by
`RUNTIME_KEY` plus its index. A static site's key, its state byte's offset
from the control block, must then lie within 2^30, so the key spaces never
meet. The collector keeps an ABI 2 module though no static site is selected,
rereads the table's count at each drain, selects new sites by name as each
is defined, and switches the control block on once any site is on. Recording
every site, it also sets `ENABLE_DEFINED` in the enable word, and the SDK
switches each run-time site on as it is defined, so a site used at once is
not missed until the next drain. A malformed table or record is counted as
malformed. The decoder still reads ABI 1, which has no table.

The SDK's `c` feature exports C entry points to run-time sites
(`sdk/rust-probes/src/c.rs`), which the Python package in `sdk/python` loads
with ctypes from the SDK built as a cdylib. A site is the pointer
`microscope_probes_define` gives; a zone or label that began ends by its own
call. `tests/python/profile.rs` records `tests/fixtures/python/probes.py`,
which uses every mark and `trace_calls`, and checks the package without its
library.

Probe writers must remain safe when no recorder exists, when the recorder is
killed, and when a reader falls behind. The timeout turns instrumentation off
after reader loss. Changes to record layout require coordinated SDK, decoder,
fixture, and format tests.

The Rust SDK writes records as atomic words and never frees a ring, so its
unsafe code is limited to making a ring and walking the site table. Its tests
run natively and under `just miri`. `just probe-cost` measures what a zone
costs.

Probe names and prefix selection are configuration. Capture records use dense
site IDs after validation. Zone analysis pairs begin and end records without
assuming well-nested input unless the SDK contract proves it.

## Capture format

The capture is an immutable validated set of tables, similar in spirit to the
debug image but with a profile-specific schema. Dense keys identify strings,
modules, mappings, threads, tasks, frames, stacks, samples, intervals, probes,
and records.

Writers build tables in deterministic order and seal them. Readers validate
all counts, indexes, ranges, and cross-references before analysis. Importers
for pprof and folded stacks must satisfy the same validated model.

A frame of a kind keyed by a string (`FrameKind::named`), an awaited
future's or an interpreted frame's, has the string's id as its address and
no module. Rebuilding a capture from others keys such a frame by its text.
An interpreted frame's key is its file, function, line, and the line its
function begins at, and its one line declares the function at that file
and first line, so functions of one name stay apart.

Format changes need:

- a compatibility decision and version update;
- round-trip tests for preserved fields;
- malformed-input tests or fuzz coverage;
- import and export tests for affected external formats;
- browser and report tests for the new meaning.

Do not let an external format dictate the internal model. Conversion may lose
facts; make that loss a property of the converter.

A capture can also hold system calls, from `microscope syscalls -o` or
`record --syscalls`. Their tables, and how a recording merges a trace with its
samples, are described in [system call tracing](syscalls.md#captures).

## Rolling and continuous recordings

A rolling window evicts records older than its retained interval while keeping
the dictionaries and mappings needed to interpret remaining data. A signal can
snapshot the current window without stopping collection.

Continuous mode writes complete segment files and removes segments older than
the retention policy. A directory reader presents valid segments as one
logical profile. It must tolerate a segment being written, a missing segment,
and retention occurring while the directory is read.

Use atomic file publication: an incomplete segment must not have the name of a
complete one. Retention deletes only files owned by the recording layout.

## Analysis

`src/analysis::keys` maps frame and function identities into dense keys at a
chosen granularity, an interpreted frame by its line even by address. `query` aggregates samples into call trees, flat costs,
density lanes, and off-CPU lanes. `diff` compares compatible results. `zones`
builds blocks and statistics from instrumentation.

Analysis is pure over a validated capture. It does not open process memory,
read clocks, or fetch debug files. Keep ordering deterministic so reports,
tests, and comparisons do not change with hash iteration.

A query's `Look` chooses how it sees each path. The native look drops
interpreted frames, which the interpreter's frames that run them stand
for. The logical look makes each async task's frames a stack of their own.
The Python look is the logical one from the outermost interpreted frame in,
without `CodeRole::Interpreter` frames, so the segments that activations of
the interpreter run join, and native code they call stays where it was
called.

Separate the measured quantity from grouping. CPU samples, wall intervals,
blocked time, probe values, and counts have different units. A transform must
state which source and unit it consumes.

## Profile clients

The terminal report is a compact summary or call tree. Formatting belongs in
shared presentation code where the browser and terminal need the same names or
numbers.

The web profile mode serves immutable profile data and query results. It does
not create a `DebuggerHandle`. Browser filters and view state become typed
queries to analysis; React should not reimplement aggregation over raw rows.

## Testing profiler changes

Use pure tests for decoders, budgets, unwind samples, capture validation,
analysis, and format conversion. Use native fixtures for perf semantics,
mapping lifecycle, task snapshots, probes, and permissions.

Conformance tests pin only the kernel behavior the simulator models. The
simulator's perf samples pass through the real recorder decoder and unwinder,
so a modeled byte-layout change needs a real-kernel probe first.

For hot-path changes, run:

```sh
just test PROFILE_FILTER
just bench-smoke
just profile-overhead
```

`profile-overhead` takes minutes and is required when collection cost may
change. Compare with `bench/overhead.json`; do not record a new baseline until
the cause is understood and accepted.

For capture or analysis changes, also run import/export, malformed-input, and
web profile tests. For lifecycle changes, test cancellation, target exit,
recorder death, file publication, and the absence of surviving processes or
open workers.

The full decision and measurement history is in `plans/profilers.html`.
