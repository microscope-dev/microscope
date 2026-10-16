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
where Go goroutines and tokio tasks wait. The report includes the pause cost.

`--live-snapshots` reads tasks without stopping the program. A task that
changes while it is being read is omitted from that snapshot and counted as an
unstable read. It reads the runtimes of every module the program maps, such as
a Go library a C program loads. tokio is found through each thread's thread
pointer, which a live snapshot learns from glibc's own list of the program's
threads; under musl or a statically linked glibc those tasks are left out, and
the snapshot says why.

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

> TODO: write a short public probes tutorial after the SDK has a supported
> installation and versioning story.

## Production permissions

Recording another process depends on Linux perf and process-inspection
permissions. The exact setup varies by distribution, service manager, and
container runtime.

> TODO: publish supported systemd and container deployment recipes after the
> recorder's privileged installation path is defined.
