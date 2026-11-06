# Profiling

microscope records where a native program spends CPU time, where its threads
wait, and where supported runtime tasks are suspended. Recording happens from
outside the target process.

## Record a program

Start a program under the recorder:

```sh
microscope record -o server.usprof -- ./server --port 8080
```

Or attach to a running process:

```sh
microscope record --pid 1842 -o server.usprof
```

Stop an attached recording with Ctrl-C. Use `--duration SECONDS` when the
recording should stop on its own.

See [profile reports](profile-reports.md) for comparisons, focused caller/callee
reports, and focused subtrees.

## Include kernel CPU time

`microscope record --kernel -- ./program` also samples time spent in the
kernel. It keeps the calling user stack and adds a leaf such as `[kernel: read]`.
Interrupts or contexts without a syscall are labeled `[kernel: unknown]`.
The recorder needs CAP_PERFMON and CAP_BPF, or CAP_SYS_ADMIN. On hosts configured
with the BPF wrapper, prefix the command with
`/run/wrappers/bin/microscope-bpf`. Restrictive `kernel.perf_event_paranoid`
settings can also prohibit kernel sampling. Without `--kernel`, recording
continues to sample user CPU time only, including with the production preset.

## Count function calls

```sh
microscope record --function hot -o calls.usprof -- ./program
microscope report calls.usprof
```

The kernel counts every execution while stacks are captured every 1,024 calls
per thread and CPU. The header separates the count from the share with stacks;
the function table estimates costs from those sampled stacks. A function with
fewer calls may have no stacks. Use `--function-period 1` to capture every call,
at a higher cost and with possible stack loss. Hardware breakpoints still add
work to every call in the program, even when its stack is not captured.

Go stack checks can retry within one call. Those functions require a sample
at every hit to distinguish retries from calls, regardless of the chosen period.

## Choose a preset

`--preset` controls sample rate, stack copying, blocked-time recording, and
microscope's CPU budget:

| Preset | Intended use |
|---|---|
| `prod` | Long-running production recording with a 1% budget and 49 samples per second |
| `dev` | Local investigation with deeper stacks and 999 samples per second; the default |
| `max` | Short, high-rate recording at 9,999 samples per second |

Override individual choices with `--rate`, `--stack-bytes`, `--off-cpu`, and
`--budget`. A higher rate or deeper stack copy increases recorder work and the
amount of data written.

The report separates startup from the remaining recorder CPU time, including
processing after the target exits. Startup includes loading BPF programs with
`--kernel`. These totals are not concurrent overhead percentages: draining and
symbolizing a queued capture can continue beyond the target's interval. The
sampling budget does not cap startup or final processing.

For ordinary native recordings, full source/inline metadata is read after
collection ends, only for images whose frames were recorded. Symbols and
unwind rules are available during collection. The report separates final debug-loading
CPU time from the total. `--symbols-later` leaves that work for a later
symbolization command. If the original file is gone or changed and no debug
directory has its build ID, collection symbols are kept with an explanation.
Live watched sessions, task snapshots, interpreter/runtime images, rolling windows, and continuous
segments retain full loading during collection.

Sampling is per thread. At 49 Hz, a thread with less than about 20 ms of CPU
time can exit before its first sample. Repeating a test by creating more short
threads does not guarantee coverage, even with substantial aggregate CPU time.
The report warns when a run has fewer than 20 samples or under 50 ms of CPU
time. For short-lived work, raise `--rate` or use `--function SYMBOL
--function-period 1` to count a known function; both increase recorder work.

At every rate, the recorder drains the kernel's ring buffers separately from
unwinding stacks, using a bounded queue. If the queue reaches three quarters
full, for example while loading a large program's debug information, clock
samples keep only the top 512 bytes of their stack copies. The kernel's
frame-pointer chain continues the stack beyond that, but programs without
frame pointers get shorter stacks. `record` and `report` count these samples.

If the queue fills, clock sampling pauses until processing catches up.
`record` and `report` show the time spent paused, and the browser shades those
spans. Lower `--rate` or `--stack-bytes` to retain more samples. Function
counters stay on when the queue fills: calls are still counted, and the kernel
counts samples that cannot fit in its rings as lost.

The report gives a count and percentage for samples the kernel drops, for
example when the recorder lacks CPU time. A full ring can also lose mapping,
name, and context-switch records. The report counts these separately so they
do not inflate the sample-loss count beyond what the program's CPU time
allows. If the kernel shrinks rings to fit locked-memory limits, the recorder
shows their actual size and the `ulimit -l` and `kernel.perf_event_mlock_kb`
settings to raise.

Stacks are exact until the available unwind information or copied memory runs
out. Each truncated stack records its reason. `dev` copies 32 KiB per sampled
stack, and so does `prod` unless the program keeps frame pointers, when 512
bytes are enough; `max` copies 16 KiB because larger copies at its rate cause
kernel sample loss. Without frame pointers, a stack that needs more bytes ends with
`[stack copy exhausted]`, and the report suggests a larger `--stack-bytes`.
perf copies at most 65528 bytes per sample. Beyond that copy, the kernel's
frame-pointer chain supplies at most 127 addresses. Deeper stacks, such as
Chrome's layout stacks, end with `[kernel call chain limit]`.

Some programs, including Google's Chrome builds, keep frame pointers but ship
little call-frame information (the rules used to unwind stacks). When those
rules cover less than half a module's code, microscope checks the remaining
code's instructions for frames that keep the frame pointer. It finds functions
through calls, addresses loaded by code, relocations, symbols, and prologues
after padding. Proven frames let the stack continue. This check costs a few
seconds when a large module first loads; the image cache keeps the result.

The image cache stores parsed module debug information in
`~/.cache/microscope/images`, or the directory named by `MICROSCOPE_CACHE_DIR`.
It holds up to 16 GiB and evicts the least recently used entries when writing
new ones. If a module cannot be cached, for example because the disk or quota
is full, every recording reads it again. For programs as large as Chrome, that
costs minutes of CPU time. `record` and `report` name uncached modules and the
reasons.

Just-in-time compiled code has no file. A program can name it with a perf map,
`/tmp/perf-PID.map`; microscope unwinds through those functions and uses their
names. For Python, run the interpreter with `-X perf`. Compilers can also
register code and call-frame information through the GDB JIT interface, as
CPython 3.15's JIT does. microscope reads both from the running program when a
sample lands in the code, so code run just before exit can remain unnamed.

## Read a profile

List the most expensive functions:

```sh
microscope report server.usprof
```

Use `--tree` for a call tree, `--limit N` to change the function count, and
`--min-percent PERCENT` to hide small tree nodes. `--native` shows the stacks
that native threads ran instead of logical task stacks. `--invert --tree`
prints a caller tree instead: each function that ran, with its callers nested
under it. `--invert` needs `--tree` and does not work with `--base`.

A multi-process profile lists processes by cost, highest first, with each
process's PID, threads, share, and command line. The recorder reads the command
line at lifecycle events and refreshes it at the first sample, including
changes such as Chromium's `--type=renderer`. A command line longer than 100
characters shows its program's file name without the directory, as
`…/node server.js`, and is cut at its end only if still too long. If the
process exits before argv can be read, the report labels its recorded
kernel name as
`[comm: NAME; argv not read]`. Comm may be truncated or renamed; it is not
an executable path or argument list. With neither available, the report says
`[command line not read]`. Use `--pid PID` or `--tid TID` to select processes
or threads; both flags repeat. `--command TEXT` matches argv when present,
otherwise recorded comm, including in a `--base` profile where PIDs differ.

Each inlined function gets its own row. When a small inlined helper takes
most of a loop's own time, `--fold-inlines` counts it as part of the function
it was inlined into, as the browser's "functions, inlines folded" does. It
works with `--tree`, `--focus`, and `--base`, but not with `--lines`.

Deep recursion pushes a tree off the right edge of the terminal.
`--collapse-recursion` cuts each stack back to the first call of any
function it calls again, directly or through others, so `visit children
visit children work` reads `visit children work`. `--collapse SYMBOL` does
the same for one function calling itself. `--merge SYMBOL` hides a function
and gives its cost to its caller, and `--drop SYMBOL` removes every sample
through it. These repeat, apply in the order given, and work with `--tree`,
`--focus`, `--invert`, and `--base`. See
[profile reports](profile-reports.md#reshape-the-stacks).

Open the profile in the browser for flame views, timelines, source costs,
disassembly, tasks, blocked time, and instrumentation zones:

```sh
microscope web server.usprof
```

Pass a second profile to compare it with the first. A diff compares each
function's time, as `report --base` does. Switch it to "share" to compare each
side's share of its own profile instead, which suits two recordings of
different lengths.

File details, at the left of the profile's toolbar, shows what the file holds
and downloads it. Metadata has the command that was recorded, one argument per
line, with its settings, modules, and the bytes each table takes. Samples has
their totals, how their stacks ended, how deep they were, and each thread's
time on and off its CPU. Recordings made before microscope kept each argument
apart show the command as one line.

## Record blocked time

Blocked-time recording tracks intervals when a thread is off its CPU. It is on
for the `dev` and `max` presets and off for `prod` unless `--off-cpu on` is
given.

Blocked time can mean a thread slept, waited for I/O or synchronization, or was
preempted. The profile reports the evidence available for each interval rather
than assigning a more specific cause without one.

## Record runtime tasks

`--task-snapshots SECONDS` briefly stops the program at that interval and reads
where Go goroutines, tokio tasks, and asyncio tasks (CPython 3.14 and later)
wait. The report includes the pause cost.

A CPU sample is attributed to a task when the snapshots before and after it
agree with the sample's task identity. Go and tokio keep the running task in a
register; with `--python bpf`, each sample identifies its thread's asyncio task.

`--live-snapshots` reads tasks without stopping the program. It omits tasks
that change during the read and counts them as unstable reads. It reads
runtimes in every mapped module, including a Go library loaded by a C program.
To find tokio tasks, it reads each thread's thread pointer from glibc's thread
list. With musl or statically linked glibc, it omits those tasks and reports why.

## Record Python programs

CPU samples see the interpreter's own functions, not the Python code they
run. `--python ENGINE` reads each thread's Python frames beside the samples:

```sh
microscope record --python bpf -o server.usprof -- python3 server.py
microscope report --python --lines server.usprof
```

- `bpf` reads Python frames in the kernel at each CPU sample and inserts them
  before the interpreter activations that run them, without stopping the
  program. It needs `CAP_BPF` and `CAP_PERFMON`, like the
  [`microscope syscalls` BPF engine](syscalls.md#give-microscope-the-bpf-privileges).
  If either is missing, recording stops with an error naming it. It never
  falls back to another engine.
- `snapshot[=SECONDS]` stops the program every SECONDS (default 0.1) and reads
  each thread's whole stack: Python frames, their native interpreter frames,
  and the C code they call. The pause lasts as long as the debugger takes to
  attach, tens of milliseconds with cached interpreter debug information.
  The report shows the pause cost.
- `live[=SECONDS]` reads Python frames every SECONDS (default 0.01) without
  stopping the program. It cannot read a running thread's registers, so it
  reads only Python frames. It omits and counts threads whose frames change
  during the read rather than keeping shorter stacks.

With `bpf`, a sample taken before microscope knows the interpreter's layout or
thread state, for example just after startup, ends in `interpreted frames
unknown`. If Python frames end early, for example because a frame is paged
out, it ends in `interpreted frames ended early`. `report` counts both among
truncated stacks. A frame whose code object was freed before microscope read
it appears as unreadable.

With `snapshot` and `live`, each thread becomes a task named `thread`, numbered
by its ID. Its stack represents the time since the previous read, like task
snapshot waits. View these with `report --waits` or the browser's Waits source.
microscope supports CPython 3.12 through 3.15, with and without the GIL.

By default, stacks show Python frames among the native frames that run them.
`--python`, or the browser's Python stacks switch, starts at the outermost
Python frame and hides interpreter frames between Python calls. Native code
called by Python, such as a C extension, stays at its call site. `--native`
shows native frames only, including the interpreter rather than Python code.
`--lines` separates Python functions by line, so a handler's slow line gets its
own row.

### The GIL

Each read labels the thread holding its interpreter's GIL with `gil=held` for
the interval since the previous read. CPython 3.15 also exposes threads waiting
for the GIL; microscope labels them `gil=waiting`. With `bpf`, each CPU sample
records its thread's GIL state, and consecutive samples with the same label
form a span. Waiting threads are mostly off CPU, so `gil=waiting` appears in
CPU samples only as they spin up.

These labels filter samples and draw lanes like instrumentation labels.
`report` lists them separately from program zones. Before CPython 3.15, waiting
threads cannot be identified. Free-threaded builds have no GIL labels because
their GIL is off. Each label reflects the state at the time of the read.

## Record Node programs

Node CPU stacks unwind through V8's builtins, interpreter, and generated code.
Run Node with `--perf-basic-prof` to name generated code through its perf map.
This also works for other V8 hosts, such as Chrome, when their symbols name
V8's embedded builtins. For Chrome, use `--js-flags=--perf-basic-prof` with
`--no-sandbox` to name JavaScript code.

Reading JavaScript frames requires a V8 build with postmortem support, as Node
has. `--node ENGINE`, an alias for `--python ENGINE`, reads each thread's
JavaScript frames:

```sh
microscope record --node bpf -o server.usprof -- node server.js
microscope report --js --lines server.usprof
```

- `bpf` reads JavaScript frames in the kernel at each CPU sample without
  stopping the program. It needs the same privileges as Python's BPF engine.
  Each frame has a function name, but only interpreted frames that called
  another JavaScript frame in the sample have a line. The innermost frame and
  frames compiled by Sparkplug, Maglev, or Turbofan have no line. V8 writes an
  interpreted frame's position to the stack only before a call, and the walker
  cannot prove which compiled code a frame ran.
- `snapshot[=SECONDS]` stops the program and reads each thread's whole
  stack, as the debugger does: every frame has its function and line, in
  every tier. A snapshot writes nothing into the program and does not use
  Node's inspector.
- `live[=SECONDS]` reads each thread's JavaScript frames every SECONDS,
  0.01 if not given, without stopping the program. It reads a thread only
  while the thread waits in a system call that JavaScript reached through
  native code, such as `fs.readSync`, `Atomics.wait`, `execFileSync`, an
  addon's function, or a pause at `debugger`. V8 then keeps where the
  thread's JavaScript frames begin in its own memory. Each frame has its
  function and line in every tier, as with `snapshot`. A thread idle in its
  event loop has no JavaScript frames, and the profile records none for it.
  A thread is left out, with the reason in the profile, when it:
  - runs JavaScript or anything else, so it is not waiting in a system call;
  - ran during the read;
  - is collecting garbage, or V8 is rewriting its frames to deoptimize them;
  - waits in a fast API call that Node 22's optimizing compiler made, whose
    caller's line V8 does not record;
  - runs WebAssembly.

  microscope never records a shortened or guessed stack. Reads rarely capture
  waits shorter than the read itself. If a read ends after the next was due,
  whether it took too long or started late on a busy machine, the profile
  notes that reads fell below the requested rate.

A sample taken before microscope knows V8's layout ends in `interpreted frames
unknown`. With `bpf`, microscope reads the layout before sampling a launched
program or an attached process. Native code that reuses the frame-pointer
register, such as an addon built without frame pointers, can hide JavaScript
frames from the BPF walker. The stack then ends in `interpreted frames ended
early`. If V8's garbage collector moved or freed a frame's function before
microscope read it, the frame is `<unknown>` with a reason, never another
function's name. Stacks retain Node's own scripts, such as
`node:internal/timers`.

`--js` works like `--python`: it starts at the outermost JavaScript frame and
hides intervening V8 frames. `--lines` separates JavaScript functions by line
where known.

A JavaScript function with no name is shown by where it is declared, as V8's
own stack traces show it: `<anonymous linter.js:557>`, or
`<anonymous node:internal/modules/cjs/loader:1314>` for Node's own scripts.
`--focus` takes that name, and a diff pairs two such functions only when they
are declared in the same place. Two functions V8 gives the same name, such as
two arrow functions it names for the array that holds them, stay apart too,
and `--focus` lists each with its file and line.

With `bpf`, each sample's `isolate` label identifies its thread's V8 isolate:
the main thread's, a `worker_threads` worker's, or one served by a V8 helper
thread. microscope numbers isolates from 1 in the order it sees them. A thread
keeps its isolate's number for life; a new isolate at a disposed isolate's
address gets a new number. Threads outside an isolate, such as libuv pool
threads or workers between tasks, have no label. These labels filter samples
and draw lanes like instrumentation labels. Debian's `libnode` stores the
current isolate where the kernel program cannot read it, so samples have no
isolate labels; the profile's settings explain why.

When following children, such as those started by `sh -c 'node a.js & node
b.js'`, microscope reads each Node process through its own V8 and keeps the
samples separate.

## Keep a rolling window

`--window SECONDS` retains only the latest part of a recording. Sending
`SIGUSR2` saves the current window to a numbered profile while recording
continues. The main output file receives the final window.

For continuous recording, write independent segments to a directory:

```sh
microscope record --continuous profiles --segment 60 --keep 24h --pid 1842
```

`microscope report profiles` and `microscope web profiles` read the retained
segments as one profile. Old segments are deleted after the `--keep` duration.

## Record system calls too

`--syscalls SET` traces system calls beside the samples, with their stacks:

```sh
microscope record --syscalls %file,%net -o server.usprof -- ./server
```

`SET` is `all`, or calls and classes as `microscope syscalls -e trace=` takes
them. The browser shows the calls on the timeline with the samples. A rolling
window or continuous recording keeps each saved file's own calls.

It works only for a program `record` starts, not with `--pid`, and needs the
BPF engine's privileges. See [System calls](syscalls.md).

## Resolve symbols now or later

The default, `--symbols now`, reads source lines from debug information on the
recording host. `--symbols later` stores module build IDs and addresses so a
different host can add source lines later:

```sh
microscope symbolize --debug-dir /srv/debug server.usprof
```

Pass `--debuginfod` to fetch missing debug files from the servers listed by
`DEBUGINFOD_URLS`.

A program built with split DWARF (`-gsplit-dwarf`, Chromium's default) keeps
functions, inlines, and types in `.dwo` files. microscope reads these from
their compiler-written locations or beside the program. Missing files leave
profile frames without their inlines; `record` and `report` name the program
and first missing file.

## Convert profile formats

`microscope convert` reads microscope profiles, pprof files, and folded stacks.
The output extension chooses microscope, pprof, folded-stack, or Chrome-trace
output:

```sh
microscope convert server.usprof server.pb.gz
microscope convert server.usprof server.folded
microscope convert server.usprof server.json
```

Formats have different data models. Converting can lose tasks, blocked-time
intervals, zones, or other information the destination cannot represent.

A trace that `microscope syscalls -o` saved has no samples; pprof and folded
stacks get its calls instead, as
[System calls](syscalls.md#save-a-trace-and-open-it-later) describes. A
recording made with `--syscalls` converts its samples to pprof and folded
stacks, and its calls only to a Chrome trace.

## Instrument work in the program

The probes SDK lets a program mark named work and values at instrumentation
sites. Recording enables all discovered sites by default. Use `--probes off`
to disable them, or select names with a comma-separated list or a name ending
in `*`.

Programs can define sites at run time. When recording all sites, microscope
enables each as it is defined. It discovers a launched program tens of
milliseconds after the SDK loads, so it sets `MICROSCOPE_PROBES_RECORDER` in
the program's environment. The SDK checks that this is its parent and enables
all sites on the first site definition or listening check. This captures even
the Python and Node packages' first zone. Child programs have a different
parent; their instrumentation stays off and is not read.

When selecting sites by name, microscope enables matching sites on its next
read, so early uses can go unrecorded. A program can define at most 4096 sites;
the profile reports attempts beyond that limit. Each distinct name is a
separate site. Put changing values, such as request IDs, in label values rather
than site names.

After the summary, `report` shows:

| Table | Contents |
|---|---|
| Zones by site | Count, total, mean, and longest time for each site |
| `Messages` | Messages, frames, and triggers in time order, with time since recording began, thread name and ID, kind, site, and text |
| `Zones by thread` | First outermost zone's start, last zone's end, time inside outermost zones, and zone count, ordered by first start |

`Zones by thread` helps spot workers that start late or finish far apart. It
counts only outermost zones, so nested zones or labels do not double-count
work. `Messages` and `Zones by thread` show the first `--limit` rows and the
number omitted.

Runtime labels, such as Python's `gil=held`, stay out of program instrumentation
tables and counts. `Runtime labels by thread` shows each thread and label's
covered time and span count, highest time first, up to `--limit` rows. The
browser draws these labels as lanes beside program zones.

### Instrument a Python program

The `microscope` package in `sdk/python` marks a Python program's work as the
SDK's macros mark a native program's:

```python
import microscope

@microscope.zone("handle")
def handle(request):
    with microscope.label("route", request.route):
        with microscope.zone("query"):
            rows = query(request)
        microscope.plot("rows", len(rows))
        return render(rows)
```

Use `message(name, text)`, `frame()`, and `trigger(why)` for other marks.
`listening()` reports whether recording is active; `found()` reports whether
microscope has discovered the program. Programs launched by `microscope record`
enable their first marks before discovery. If the program exits while
microscope is busy, it waits at most one second for its marks to be read.
Zones cannot decorate coroutines or generators because other work can run
during their waits.

`microscope.trace_calls("app")` times calls in `app` and its submodules as zones
named by module and function. It uses Python 3.12's profiler monitoring API.
The program must opt in; microscope never adds Python instrumentation itself.

When a traced site is off, its monitoring events disable themselves on first
use, leaving the function to run at its normal speed. The `microscope-sdk`
thread checks disabled sites every 20 ms while recording is active and every
second otherwise. It sleeps when none need checking. When a site is enabled,
it calls `sys.monitoring.restart_events()`, which also restarts other tools'
disabled events. A recorded call costs about 0.6 µs on CPython 3.14 on a fast
core; microscope reads 3 to 4 million calls per second using about a fifth of
one CPU. After `trace_calls`, each thread's new ring holds 2^18 records
(4 MiB) between reads. Busier programs lose records; the report counts them.

The package loads the probes SDK's shared library, built with
`cargo rustc --release --features c --crate-type cdylib` in
`sdk/rust-probes`, from `libmicroscope_probes.so` beside the package or from
`MICROSCOPE_PROBES_LIBRARY`. Without it, every mark does nothing and
`microscope.problem` says why. It works with every Python 3 version and GIL
mode; nothing is written unless a recording switches a site on.

### Instrument a Node program

The `microscope` package in `sdk/node` marks a Node program's work the same
way:

```js
const microscope = require('microscope');

const handle = microscope.wrap('handle', (request) =>
  microscope.label('route', request.route, () => {
    const rows = microscope.zone('query', () => query(request));
    microscope.plot('rows', rows.length);
    return render(rows);
  }));
```

`zone(name, fn)` records `fn` as a span of thread time. `wrap(name, fn)` records
each call as a span, and `label(name, value, fn)` labels the time `fn` runs.
`message(name, text)`, `frame()`, `trigger(why)`, `listening()`, and `found()`
work as in Python. Zones and labels cannot wrap async functions or generators:
other work can run during their waits. Each mark's site is identified by its
name and call location. A `worker_threads` worker marks its own thread.

The package's Node-API addon, `microscope.node`, works with every Node
release, and loads the probes SDK's shared library, built as for Python, from
`libmicroscope_probes.so` beside the package or from
`MICROSCOPE_PROBES_LIBRARY`. `MICROSCOPE_NODE_ADDON` names the addon when it
is not beside `index.js`. `sdk/node/README.md` shows how to build both.
Without them every mark does nothing and `microscope.problem` says why.

> TODO: write a short public probes tutorial after the SDK has a supported
> installation and versioning story.

## Production permissions

Recording another process depends on Linux perf and process-inspection
permissions. The exact setup varies by distribution, service manager, and
container runtime.

> TODO: publish supported systemd and container deployment recipes after the
> recorder's privileged installation path is defined.
