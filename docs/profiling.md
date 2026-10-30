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

The report says what recording cost microscope. Starting up, which with
`--kernel` includes loading BPF programs, is shown apart from the cost while
the program ran. Only the latter is a share of a CPU and is compared with
`prod`'s 1% budget. A run with fewer than 20 samples or under 50 ms of CPU time
is too short to judge either cost or coverage, and the report says so.

Recordings drain rings separately from unwinding, at every rate. That queue is
bounded. Once processing falls three quarters of it behind, as while
microscope loads a large program's debug information, each clock sample keeps
only the
top 512 bytes of its stack copy; the kernel's frame-pointer chain carries the
stack on from there, so a program without frame pointers gets shorter stacks
meanwhile, and the record and the report count those samples. If processing
falls behind far enough to fill the queue even so, clock sampling stops until
processing catches up, rather than letting the kernel drop samples. The
record and the report say how long sampling was off, and the browser shades
those spans; a lower `--rate` or `--stack-bytes` keeps more. Function counters
stay on even when the queue fills, so calls are still counted; the kernel
counts any samples its rings cannot hold as lost. Samples the
kernel still drops, such as when the recorder is starved of CPU, are
reported as a count and percentage. A full ring also loses the
records of what the program maps and names and of its threads' context
switches; the report counts those apart, as records, so the samples it says
were dropped are never more than the program's CPU time allowed. If the
kernel reduces ring sizes to fit locked-memory limits, the recorder gives
their actual size and the `ulimit -l` and `kernel.perf_event_mlock_kb`
settings to raise.

Stacks are exact up to the point where the available unwind information and
copied memory end. A truncated stack records its reason. `dev` copies 32 KiB
of each sampled stack, and `max` 16 KiB, since at its rate more makes the
kernel drop samples. A program without frame pointers whose frames
need more ends with `[stack copy exhausted]`, and the report suggests a larger
`--stack-bytes`. perf copies at most 65528 bytes per sample. Past the copy,
the kernel's frame-pointer chain carries a stack on for at most 127
addresses; a deeper stack, as Chrome's layout code makes, ends with
`[kernel call chain limit]`.

Some programs keep frame pointers but ship call-frame information for little
of their code, as Google's build of Chrome does. When call-frame information
describes less than half of a module's code, microscope proves the frames of
the rest from its instructions: a function found from a call, an address its
code loads, a relocation, a symbol, or a prologue after padding, whose code
keeps the frame pointer, carries the stack on. Proving costs a few seconds
the first time such a large module loads; the image cache keeps the result.

The image cache keeps what microscope read from each module's debug
information, in `~/.cache/microscope/images` or the directory
`MICROSCOPE_CACHE_DIR` names, up to 16 GiB; writing an entry removes the
entries used longest ago to make room. A module whose image can't be written
there, as when the disk or a quota is full, is read again by every recording,
which for a program as large as Chrome costs minutes of CPU. `record` and
`report` say which modules and why.

Code a just-in-time compiler generates has no file. A program that writes a
perf map, `/tmp/perf-PID.map`, names it: stacks go on through the functions
the map names, under those names. For Python, run the interpreter with
`-X perf`. A compiler that registers its code through the GDB JIT interface,
as CPython 3.15's JIT does, names it too, and its call-frame information
unwinds it. Both are read from the running program when a sample lands in the
code, so code a program runs just before it exits can stay unnamed.

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

A profile of several processes lists them, most cost first, with each one's
pid, threads, share, and command line, as the recorder read it at the
process's first sample: a program that rewrites its own, as Chromium's
processes write their role, `--type=renderer`, shows it. A process that
ended before the recorder got to its first sample shows
`[command line not read]`. `--pid PID` and `--tid TID` report one process or
thread; both repeat. `--command TEXT` reports the processes whose command
line holds TEXT, and selects the same way in a `--base` profile, whose pids
are another recording's.

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

A CPU sample is given to the task it ran when the snapshots on both sides of it
find that task where the sample found it. Go and tokio keep the running task in
a register; with `--python bpf`, each sample says which asyncio task its thread
ran.

`--live-snapshots` reads tasks without stopping the program. A task that
changes while it is being read is omitted from that snapshot and counted as an
unstable read. It reads the runtimes of every module the program maps, such as
a Go library a C program loads. tokio is found through each thread's thread
pointer, which a live snapshot learns from glibc's own list of the program's
threads; under musl or a statically linked glibc those tasks are left out, and
the snapshot says why.

## Record Python programs

CPU samples see the interpreter's own functions, not the Python code they
run. `--python ENGINE` reads each thread's Python frames beside the samples:

```sh
microscope record --python bpf -o server.usprof -- python3 server.py
microscope report --python --lines server.usprof
```

- `bpf` reads the sampled thread's Python frames in the kernel as each CPU
  sample is taken, and puts them in the sample's stack, before the
  activations of the interpreter that run them. The program is never stopped.
  It needs `CAP_BPF` and `CAP_PERFMON`, as
  [`microscope syscalls`' BPF engine](syscalls.md#give-microscope-the-bpf-privileges)
  does, and without them recording refuses and says which is missing; it
  never falls back to another engine.

- `snapshot[=SECONDS]` stops the program every SECONDS, 0.1 if not given, and
  reads each thread's whole stack: its Python frames among the native frames
  that run them, and the C code they call. Each snapshot holds the program
  for the time its debugger takes to attach, tens of milliseconds with the
  interpreter's debug information cached, which the report states.
- `live[=SECONDS]` reads each thread's Python frames every SECONDS, 0.01 if
  not given, as the program runs, never stopping it. It reads only the
  Python frames, since a running thread's registers cannot be read. A thread
  whose frames change while they are read is left out and counted, never
  kept as a shorter stack.

With `bpf`, the Python frames are part of each CPU sample. A sample taken
before microscope has read the interpreter's layout or the thread's state, as
just after the program or a thread starts, ends in "interpreted frames
unknown"; one whose Python frames end early, as one paged out, ends in
"interpreted frames ended early". `report` counts both with the other stacks
that end short. A Python frame whose code object was freed before microscope
read it is shown as unreadable.

With `snapshot` and `live`, each read thread is a task of its own, named
`thread` and numbered by its id, whose stack stands for the time since the read
before, as a task snapshot's waits do. `report --waits` shows them, and the
browser's Waits source. CPython 3.12 through 3.15 are read, with and without
the GIL.

Three looks show the stacks. The default shows Python frames among the
native frames that run them. `--python`, and the browser's Python stacks
switch, shows each stack from its outermost Python frame in, without the
interpreter's own frames between them; native code that Python calls, such as
a C extension's function, stays where it was called. `--native` shows only the
frames the threads ran, the interpreter's in place of the Python code.
`--lines` tells Python functions apart by line, so a handler's slow line shows
as its own row.

### The GIL

Each read records which thread holds its interpreter's GIL, as the label
`gil=held` on the thread from the read before to this one, and from CPython
3.15, which says so, `gil=waiting` on each thread that asks for it. With `bpf`,
each CPU sample says it of its own thread, and each run of samples with one
label is one span; a thread that waits for the GIL is mostly off its CPU, so
`gil=waiting` shows in CPU samples only as the thread spins up. The labels
select samples and draw lanes as instrumentation labels do, and `report`
lists them apart from the program's zones. CPython before
3.15 does not say which threads wait for the GIL, and a free-threaded build's
GIL is off, so it has no GIL labels. A label is as the GIL was when read.

## Record Node programs

CPU samples of a Node program unwind through V8's own code: its builtins,
its interpreter, and the code its compilers generate. Run Node with
`--perf-basic-prof` and its perf map names the code its compilers generate
in each stack, which still unwinds through it. The same holds for other
programs that embed V8, such as Chrome, when their symbols name V8's
embedded builtins; Chrome's `--js-flags=--perf-basic-prof` (with
`--no-sandbox`) names its JavaScript. Only a V8 built with postmortem
support, as Node is, can have its JavaScript frames read. `--node ENGINE`, the
same option as `--python ENGINE`, reads each thread's JavaScript frames as
well:

```sh
microscope record --node bpf -o server.usprof -- node server.js
microscope report --js --lines server.usprof
```

- `bpf` reads the sampled thread's JavaScript frames in the kernel as each
  CPU sample is taken, without stopping the program, and needs the same
  privileges as Python's. Each frame is named by its function. Its line is
  known only for a frame V8's interpreter runs that has called another
  JavaScript frame of the sample. The innermost frame and frames that
  Sparkplug, Maglev, or Turbofan compiled have their function without a
  line. A sample's line is never a guess: V8 writes an interpreted frame's
  place to the stack only before a call, and the walk does not prove which
  compiled code a frame ran.
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

  A thread is never recorded with a shorter stack or a guessed one. A wait
  shorter than one read of the thread is mostly left out. A read that
  ends after the next was due, because it took longer than SECONDS or a busy
  machine started it late, says so in the profile, as reads then ran less
  often than asked.

A sample taken before microscope has read V8's layout ends in "interpreted
frames unknown". With `bpf`, microscope reads the layout before it takes the
first sample, of a program it launches or of a process it attaches to. With
`bpf`, a sample in
native code that keeps something other than a frame pointer in its
frame-pointer register, as an addon built without frame pointers may, hides
the JavaScript frames under it, and ends in "interpreted frames ended
early". A frame whose function
V8's garbage collector moved or freed before microscope read it is
`<unknown>`, with the reason, never another function's name. Frames of Node's
own scripts, such as `node:internal/timers`, stay in the stacks.

`--js` is the same look as `--python`: each stack from its outermost
JavaScript frame in, without V8's frames between them. With `--lines`,
JavaScript functions are told apart by line where the line is known.

A JavaScript function with no name is shown by where it is declared, as V8's
own stack traces show it: `<anonymous linter.js:557>`, or
`<anonymous node:internal/modules/cjs/loader:1314>` for Node's own scripts.
`--focus` takes that name, and a diff pairs two such functions only when they
are declared in the same place. Two functions V8 gives the same name, such as
two arrow functions it names for the array that holds them, stay apart too,
and `--focus` lists each with its file and line.

With `bpf`, each sample carries the label `isolate` with the number of the V8
isolate its thread ran in: the main thread's, a `worker_threads` worker's, or
the one a V8 helper thread works for. Isolates are numbered 1, 2, and on in
the order the recording first sees them, and a thread keeps its isolate's
number for its life. An isolate that takes the address of one disposed before
it gets a new number. A thread in no isolate, as one of libuv's pool or a
worker between two of its tasks, has no label. The labels select
samples and draw lanes as instrumentation labels do. Debian's `libnode` keeps
the current isolate where the kernel program cannot read it, so its samples
have no isolate labels, and the profile's settings say why.

A recording that follows a program's children, as `sh -c 'node a.js & node
b.js'` starts, reads each Node process by its own V8 and keeps its samples
apart.

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

To upload segments to a microscope service as they finish, see
[Upload a continuous recording](agent.md#upload-a-continuous-recording).

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

A program built with split DWARF (`-gsplit-dwarf`, Chromium's default)
keeps its functions, inlines, and types in `.dwo` files, which microscope
reads from where the compiler wrote them or from beside the program. When
some can't be found, the profile's frames lose their inlines, and `record`
and `report` say which program and the first missing file.

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

The probes SDK lets a program mark named work and values. Recording enables
all discovered sites by default. `--probes off`, a comma-separated list, or a
name ending in `*` limits what is enabled.

A program can also define sites as it runs, as a language that names its
work at run time does. Recording every site, such a site is on from its
definition. The recording finds a program it starts some tens of
milliseconds after the program loads the SDK, so it names itself in the
program's environment as `MICROSCOPE_PROBES_RECORDER`, and the program, as
its child, switches every site on itself the first time it defines a site or
asks whether microscope listens. The Python and Node packages' first zone
is then recorded, however soon it begins. The recording reads that
program's instrumentation alone; its children have another parent, and stay
off. Recording sites by name, a site is switched on, by the same names,
when the recording next reads the program, so its first uses can go
unrecorded. A program that defines more than 4096 sites gets no more, and
the profile reports it. Each distinct name is a separate site, so give a
value that changes, such as a request's id, as a label's value rather than
in a name.

`report` shows what the instrumentation recorded after the summary. A table
of zones by site gives each one's count, total, mean, and longest time. Then
`Messages` lists each message, frame, and trigger in time order, with its time
since the recording began, its thread's name and id, its kind, its site, and
its text. `Zones by thread` gives, for each thread that ran zones, when its
first outermost zone began, when its last ended, the time inside them, and
how many there were, ordered by first start: a pool's workers that all start
late, or finish far apart, stand out there. Only outermost zones count, so a
zone nested in a label or another zone is counted once. Both lists show the
first `--limit` rows and say how many more there are.

The labels microscope gives a thread from its runtime, such as `gil=held`
where a Python thread held the GIL, are not the program's, and stay out of
those tables and counts. `Runtime labels by thread` gives, for each thread
and label, the time its spans cover and how many there were, most time
first, for the first `--limit` rows. The browser draws them as lanes beside
the program's zones.

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

`message(name, text)`, `frame()`, and `trigger(why)` complete it, and
`listening()` says whether a recording reads the program. A program that
`microscope record` starts switches itself on, so that its first marks are
recorded before microscope finds it; `found()` says once microscope has. A
program that exits while microscope is busy elsewhere waits at most a second
for it to read the marks. A zone cannot
decorate a coroutine or generator, whose waits other work fills.
`microscope.trace_calls("app")` times every call of the functions of `app` and
the modules under it as zones named by module and function, using Python
3.12's profiler monitoring tool; it is the program's own choice, as microscope
never adds instrumentation to a Python program.

While no recording reads a traced function's site, the function's monitoring
events are disabled where they first fire, and it runs at its own speed. A
thread named `microscope trace_calls` checks every 20 ms whether such a site
has been switched on, and if so restarts the events with
`sys.monitoring.restart_events()`, which restarts other tools' disabled events
too. Recorded, a call costs about 0.6 µs on CPython 3.14 on a fast core, and
microscope reads 3 to 4 million calls a second for about a fifth of a CPU.
Each thread that writes a ring after `trace_calls` gets one of 2^18 records,
4 MiB, to hold what it writes between microscope's reads; a busier program
loses records, and the report counts them.

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

`zone(name, fn)` runs `fn` as a span of the thread's time, `wrap(name, fn)`
makes each call of `fn` one, and `label(name, value, fn)` labels the time
`fn` runs. `message(name, text)`, `frame()`, `trigger(why)`, `listening()`,
and `found()`, as in Python, complete it. A zone or label spans a function of one thread,
so neither accepts an async function or a generator, whose waits other work
fills. Each mark is a site named by its name and where it was called from.
A `worker_threads` worker marks its own thread.

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
