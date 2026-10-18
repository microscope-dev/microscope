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

Stacks are exact up to the point where the available unwind information and
copied memory end. A truncated stack records its reason.

Code a just-in-time compiler generates has no file. A program that writes a
perf map, `/tmp/perf-PID.map`, names it: stacks go on through the functions
the map names, under those names. For Python, run the interpreter with
`-X perf`.

## Read a profile

List the most expensive functions:

```sh
microscope report server.usprof
```

Use `--tree` for a call tree, `--limit N` to change the function count, and
`--min-percent PERCENT` to hide small tree nodes. `--native` shows the stacks
that native threads ran instead of logical task stacks.

Open the profile in the browser for flame views, timelines, source costs,
disassembly, tasks, blocked time, and instrumentation zones:

```sh
microscope web server.usprof
```

Pass a second profile to compare it with the first.

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
select samples and draw lanes as instrumentation labels do. CPython before
3.15 does not say which threads wait for the GIL, and a free-threaded build's
GIL is off, so it has no GIL labels. A label is as the GIL was when read.

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

## Instrument work in the program

The probes SDK lets a program mark named work and values. Recording enables
all discovered sites by default. `--probes off`, a comma-separated list, or a
name ending in `*` limits what is enabled.

A program can also define sites as it runs, as a language that names its
work at run time does. Recording every site, such a site is on from its
definition. Recording sites by name, it is switched on, by the same names,
when the recording next reads the program, so its first uses can go
unrecorded. A program that defines more than 4096 sites gets no more.

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
`listening()` says whether a recording reads the program. A zone cannot
decorate a coroutine or generator, whose waits other work fills.
`microscope.trace_calls("app")` times every call of the functions of `app` and
the modules under it as zones named by module and function, using Python
3.12's profiler monitoring tool; it is the program's own choice, as microscope
never adds instrumentation to a Python program.

The package loads the probes SDK's shared library, built with
`cargo rustc --release --features c --crate-type cdylib` in
`sdk/rust-probes`, from `libmicroscope_probes.so` beside the package or from
`MICROSCOPE_PROBES_LIBRARY`. Without it, every mark does nothing and
`microscope.problem` says why. It works with every Python 3 version and GIL
mode; nothing is written unless a recording switches a site on.

> TODO: write a short public probes tutorial after the SDK has a supported
> installation and versioning story.

## Production permissions

Recording another process depends on Linux perf and process-inspection
permissions. The exact setup varies by distribution, service manager, and
container runtime.

> TODO: publish supported systemd and container deployment recipes after the
> recorder's privileged installation path is defined.
