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
| `src/backend/linux/bpf/v8.bpf.h`, `src/runtime_model/node/sampled.rs`, `src/unwind/code_copies.rs` | The BPF walker of JavaScript frames, the proof that names what it read, and V8's anonymous code as modules |
| `src/profiler/jit.rs`, `src/debug_info/perf_map.rs`, `src/debug_info/gdb_jit.rs` | Perf maps and registered JIT objects: anonymous code as modules |
| `src/profiler/annotate.rs`, `src/profiler/symbols.rs`, `src/profiler/zones.rs` | Symbol, source, and instrumentation annotation |
| `src/profiler/formats/` | Chrome, folded-stack, and pprof output |
| `src/capture/` | Validated capture file format and table builder |
| `src/analysis/` | Queries, call trees, diffs, costs, and zones |
| `src/cli/profile.rs` | `record`, `report`, `convert`, and `symbolize` command adapters |
| `src/web/profiles.rs`, `web/src/profile/`, `web/src/ui/profile/` | Profile transport, browser model, and profile pages |
| `sdk/c/microscope_kernel.h`, `sdk/rust-probes/`, `sdk/zig/microscope_kernel.zig` | Target-side profiling and instrumentation interfaces |
| `sdk/python/`, `sdk/node/` | The Python and Node packages over the probes SDK's run-time sites |
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

A full ring drops mapping records with everything else. A module whose record
was lost has no frames, and its instrumentation is never found: a program that
loads the Python package's library while its children flood the rings with
mappings waits for microscope to listen and never hears. After a loss record
the recorder reads each process's `/proc/PID/maps` (`Processor::lost`). A
process may have mapped a file since the loss, and that record is not processed
yet, so the mappings read wait until every record stamped before the read has
been processed. Each file mapping still not held under its own image then maps
as its record would have, stamped with the read's time
(`Processor::map_lost_before`). Anonymous code is left to the samples that run
it, as below.

### Code a compiler generated

Anonymous executable memory, where a just-in-time compiler writes code, has
no file to describe it. When the process writes a perf map,
`/tmp/perf-PID.map` (CPython does under `-X perf`), each anonymous range it
covers becomes a module of its own (`src/debug_info/perf_map.rs`,
`src/profiler/jit.rs`). The map is append-only and written while it is read,
so the recorder reads on from where it stopped, and only when a sample lands in
anonymous code no function it named covers; the sample is then unwound again.
The process writes the map, so one read takes at most 16 MiB of it, and the
next goes on from there, and at most 64 MiB of its functions' code; a function
past that has no code, so no frames, and the `perf map PID` setting says so.
A read that finds nothing new, of the map or of the registered objects below,
is not made again for 20 ms of the samples' time unless the process maps
something: a foreign-function library's callbacks run in anonymous code no
map names, and would otherwise open the process's files at each sample.
A mapping over a function whose line was read before the mapping was made
ends the function. One read after may name the new code, since the map is
read as the program runs and mappings as the recorder drains them, so it is
kept until a later line replaces it.

A recording can miss where anonymous code is mapped: a full ring drops
mapping records with everything else, which a loaded machine's recording of
CPython's trampolines did, and `mremap` moves code without a record. A
sample in code the recording has seen nothing of, in no mapping and no
anonymous range, maps the anonymous executable mapping that holds it from
`/proc/PID/maps` as its record would have (`Processor::map_unseen_code`),
then reads the perf map for it. The same 20 ms bound applies. Only
anonymous code is mapped so: a file mapped there now may have replaced what
the sample ran.

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

In a process that carries V8, V8's anonymous code is mapped as its own
modules (see [JavaScript frames in each sample](#javascript-frames-in-each-sample)),
and Node writes a map under `--perf-basic-prof`. Code V8 generated that
no function the map names covers is read for as code in no mapping is. The
map's functions in it become the symbols of its module, made again over
each piece of it (`Processor::perf_map_module`), which keeps V8's
frame-pointer convention for its frames, as the map's proof of a frame from
its code does not reach most of V8's, and the interpreter's role, so the
JavaScript frames placed at it still stand for it. A copy of node's
builtins keeps node's image, which names it already, and no perf-map module
of its own is made in such a process.

A compiler that registers objects through the GDB JIT interface, as CPython
3.15's JIT does, describes its code fully. Each object is an ELF image whose
sections lie at the addresses its code runs at, with symbols and call-frame
information, and it becomes a module of its own over its code. The recorder
reads the objects when a sample lands in code nothing names. That includes
code in no mapping the recording saw, since a busy ring can lose the mapping's
record. It reads them again only when the bytes of the descriptors or of
the entries they list changed since, or when a mapping replaced an object's
code. A new object's entry may be where an old one's was, which leaves the
descriptor's own bytes as they were. A process whose modules hold no
`__jit_debug_descriptor` is never read. A range that holds registered objects
is theirs alone: no perf-map module is made over it. The objects are read
from the running process, so a program that exits before the recorder
ingests its samples leaves them unnamed, and those stacks end short.

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

A stopped snapshot attaches through `DebuggerHandle::observe_process`, which
plants none of the loader's, the runtimes', or the inspector's breakpoints and
opens no inspector: a snapshot writes nothing into the program. In a process
that carries V8, a frame whose code V8 owns stands for the JavaScript function
the debugger names, as an interpreter frame does for Python.

A live snapshot reads the JavaScript frames that V8 publishes in a thread's
isolate while JavaScript calls out to native code
(`RuntimeModel::live_frames`, `runtime_model/node/live.rs`). `/proc` gives a
blocked thread's stack and instruction pointers but not its frame pointer,
so the published frames are the only way in. `LiveRuntime::published` reads
each thread that holds an isolate only while it is proven not to run. The
thread must be asleep in a system call (`/proc/PID/task/TID/syscall`) before
and after the read, with the same record. Its context-switch counts
(`/proc/PID/task/TID/status`) must be the same before and after. To run and
sleep again, a thread gives up its CPU, which counts. A thread that may have
run counts as changed. Every thread left out for another reason is named in
the snapshot's one problem, `frames not read live: threads 12, 13: REASON;
...`. `LiveStop::stack` bounds each thread's stack by the thread pointers
glibc lists: a thread's stack lies below its own thread pointer and above the
next lower one, and the main thread's above them all. The reader treats an
isolate whose outermost entry lies on another thread's stack as held by a
helper thread, with no JavaScript frames of its own. A copy V8 made of its
builtins (a `LoadedModule` with a `window`) runs no runtime of its own, so a
live snapshot reads runtimes only through the full module. Otherwise each
thread would be read twice, the second time with the copy's bias.

A thread that has not run since a quiet read is not read again
(`tasks::Kept`). A read keeps the frames it found with the thread's
context-switch counts and its `syscall` record. The next read reuses the
frames only while the record is unchanged, the counts read before and after
it are both unchanged, and the thread's start time is the same. A thread
that ran between the two reads of a single check and slept again in the same
call leaves the same record, which the second count catches (`ran_since`). Its stack and the objects its frames name can change only while it
runs: V8 collects an isolate's heap on the isolate's own thread. The
exception is the heap isolates share under V8's flags for shared structs
and strings, which Node leaves off and the reader does not check. A program
whose threads stay parked then costs a few `/proc` reads per thread each
time.

Live reads keep the rate asked of them: each is due one interval after the
last was due, not after it ended, and the drain loop wakes for it
(`Snapshots::due_in`). A read that takes longer than the interval records
the note `a live read took N ms, longer than the M ms asked between reads`
(`Processor::snapshot_noted`), so a recording that could not keep up says
so. The note is a snapshot marker like a problem, but it does not count as
a failed snapshot: the read itself succeeded.

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
`Truncation::InterpretedUnknown`, but only where its native stack has an
activation of the interpreter's loop (`interpreted::unknown_cuts`): a thread
in none, as a library's worker thread is, runs no Python frames. Code objects are named and proved as each
record is drained (`RuntimeModel::sampled_frames`), while the program most
likely still runs; one freed since is unreadable. The syscall engine shares
the walker (`dev/syscalls.md`). Segments go before the activations of the
interpreter's loop that run them, as the debugger places them, and where the
two differ in number the frames follow the native ones and the stack ends in
`Truncation::InterpretedUnplaced`. The record's GIL state becomes label spans,
one per run of samples with one value, and from 3.14 the asyncio task the
thread ran joins the samples that task snapshots claim, as a task a Go or
tokio sample keeps in a register does.

### JavaScript frames in each sample

The same program walks a Node process's JavaScript frames when its pid has a
`v8_layout` in `V8_LAYOUTS` (`v8.bpf.h`, a global function verified once).
V8 keeps its frames on the native stack with frame pointers, so the walk
follows the chain from the sample's `rbp`, at most `V8_STEPS` steps and
`V8_FRAMES` frames, and records each frame whose slot at
`v8dbg_off_fp_function` holds a `JSFunction`: the function, its
`SharedFunctionInfo`, the shared information's `Script`, the script's id, the
function literal id, and the bytecode array and offset slots
(`v8dbg_off_fp_bytecode_array`, `v8dbg_off_fp_bytecode_offset`). It reads no
thread-local state, so it needs no isolate address and works where V8 keeps its
isolate in dynamic TLS. The record is `RECORD_V8_STACK`, paired with its sample
as Python's is.

`sampler::v8_layout` fills the layout from the image's `V8Layout`: the
`v8dbg_` frame offsets and tags, and the generated layout's fields
`HeapObjectMap`, `MapInstanceType`, `JSFunctionSharedFunctionInfo`,
`SharedFunctionInfoScript`, `SharedFunctionInfoFunctionLiteralId`, and
`ScriptId`. A layout that lacks one refuses with its name. The recorder sets a
process's layout once its V8 module is mapped and forgets it at exec and exit;
each process of a group binds its own.

A walked frame is kept only where its address is V8's code: a module V8 owns
(`RuntimeModel::owns_code`) or a mapping of code it generated. Its functions
are read as the record is drained (`interpreted::drain_v8`), but a drain may
hold a new code mapping, as a short-lived worker's, beside the samples that
run it, and mappings are processed in time order after the drain reads its
records. So a frame whose code no mapping covers yet is read as each code it
may run, and `DrainedV8::settle` keeps or drops it, and picks the reading, by
the mappings in effect when its sample is processed. Each frame is named
through `RuntimeModel::sampled_code_frame`, which for Node is
`node::sampled::describe`. The proof is the one the script table
uses: the shared information is still a `SharedFunctionInfo` whose script is
the one the walker read, with the same script id, and the same function literal
id. A script id is never reused in an isolate and a literal id names one
function literal in its script, so a frame that passes ran that function; one
that fails is `<unknown>` with the reason. A shared information of no script,
as a native function's, is not a frame of the program.

Lines follow what V8 writes, never what it might hold. The interpreter keeps a
frame's bytecode offset in a register (`r9`, `kInterpreterBytecodeOffsetRegister`
in `register-x64.h` of 22, 24, and 26), but `InterpreterAssembler` treats it as
a variable its handlers allocate freely and stores it to the frame only before a
call. So a CPU sample gives an interpreted frame its line only when a frame of
the program's that it called is inside it (`after_call && called`): a sample in
a bytecode handler that keeps a frame of its own would otherwise read a stale
offset. The innermost frame has no line, and a compiled frame has none either,
since the walk does not prove which code object it ran and V8's GC can move
code. Both say why in the frame's problem.

A recording maps V8's anonymous code as modules (`unwind::code_copies`): a
mapping that copies node's builtins byte for byte binds node's image; any
other anonymous executable mapping is V8's generated code, unwound by frame
pointers. Mappings read from `/proc/PID/maps`, as at an attach, come in
address order, and Debian's V8 reserves its code range below `libnode`, so
anonymous code mapped before any module that carries V8 waits for one
(`ProcessState::before_v8`). `modules_in` keeps a copy beside its image's
own module, keyed by image and window, so the copy does not hide V8 from the
walker. In an image that carries V8, the interpreter's entry trampoline plays
the interpreter role (`roles::v8_role`) and generated code frames do too, so
each stands for the JavaScript functions placed at it. Functions placed at a
frame of another role, as a bytecode handler a sample landed in, go after that
frame, which they run (`Processor::splice_placed`).

Node defines no activation of an interpreter's loop, so the rule that cuts a
Python stack whose frames are unknown does not apply. Instead
`interpreted::v8_cut` cuts a sample with `InterpretedUnknown` when its record
was lost or came before the layout, and with `InterpretedEnded` when its walk
stopped `V8_END_UNREADABLE`, as at a frame pointer that code built without
frame pointers left in `rbp`, but only where one of its native frames runs
V8's code (`Interpreted::runs_v8`): V8's builtins, a copy of them, or code it
generated. A thread that runs none, as a libuv pool thread, runs no
JavaScript. A walk that filled its record ends `InterpretedEnded` wherever it
is. `tests/fixtures/node/frameless.c` is an addon that spins with 8 in `rbp`,
which call-frame information unwinds through.

The sampler's `v8_sample` also reads each thread's current isolate,
`g_current_isolate_`, at each sample: the thread pointer from the task's
`thread.fsbase` (CO-RE, through flavor types, since `kernel_types.h`'s
`pt_regs` would clash with the perf event's), plus the variable's offset
(`ThreadLocal::Offset`, as Node 24 and 26 place it) or the offset a GOT slot
of the image holds (`ThreadLocal::Slot`, as 22 does), which the program reads
at every sample, as the loader fills it after the recorder may have read the
layout. `fsbase` is what the kernel set at clone or `arch_prctl` and saves as
the thread switches out; glibc never writes it with `wrfsbase`. A library's
dynamic TLS (`ThreadLocal::Dynamic`, Debian's `libnode`) is left unplaced,
and the recording's setting `node PID isolates` says why. The syscall
programs leave the record's isolate unread.

`interpreted::IsolateNumbers` numbers each process's isolates by first sight.
Unlike `backend::linux::holdings`, it keeps a number while a thread seen in
the isolate lives, though the thread is seen in none for a moment: samples
find a worker between its isolate's scopes, where a stop seldom does, and
Node runs each isolate on one thread for that thread's life. A thread that
finds the address of an isolate none of whose threads is in it now is in a
new isolate, which took a disposed one's address, and gets a new number. A
thread's exit (`thread_exited`) forgets the numbers only it held. The
debugger keeps its own rule on purpose, which frees a number once every
thread seen holding it is seen holding none, so a profile and the thread
list may give one worker's isolate different numbers. The numbers become
spans of the label `isolate`, built as the GIL's are
(`Interpreted::extend`). `tests/node/profile.rs`
records `busy.js --pool` on 26 and 22: its main thread and worker each carry
one isolate number of their own, and the libuv pool thread that runs its
PBKDF2, which no JavaScript runs on, carries none.

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
not missed until the next drain. Its records can then reach a ring between a
drain's read of the table and its read of the rings, so a ring that names a
site the collector has not seen reads the table again before it counts any of
its records: the SDK counts a site before it records one, so the second read
finds it. `trace_calls` defines a site as each new function first runs, so the
Python package's records fall in that gap whenever a drain is slow. A
malformed table or record is counted as malformed. The decoder still reads ABI
1, which has no table.

The SDK's `c` feature exports C entry points to run-time sites
(`sdk/rust-probes/src/c.rs`), which the Python package in `sdk/python` loads
with ctypes from the SDK built as a cdylib. A site is the pointer
`microscope_probes_define` gives; a zone or label that began ends by its own
call. `tests/python/profile.rs` records `tests/fixtures/python/probes.py`,
which uses every mark and `trace_calls`, and checks the package without its
library.

The Node package in `sdk/node` is `index.js` over a Node-API addon,
`microscope.c`, which `dlopen`s the same library (from
`MICROSCOPE_PROBES_LIBRARY` or beside the addon) and passes calls through;
Node-API keeps one build working on every Node release. A site is keyed by
kind, name, and the caller's file and line, from a `CallSite`.
`scripts/build-test-programs.sh` builds the addon against the dev shell's Node
headers as `build/test-programs/node-microscope.node`, and
`tests/node/profile.rs` records `tests/fixtures/node/probes.js` on the main
thread and a worker, and checks the package without its native parts.

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
