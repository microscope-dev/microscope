# Browser

`microscope web` serves one debugging or profiling session to one or more
browser tabs.

## Start a session

```sh
microscope web ./program -- ARG...
microscope web --attach PID
microscope web --core core.1234
microscope web profile.usprof
```

With no target, the page lets you choose what to open. The server listens on
`127.0.0.1:7341` by default and chooses another free port when that port is in
use.

A launched program waits for the first continue. Pass `--run` to start it at
once. An attached process waits at its attach stop unless `--resume` is used.

## Move around

The workspace can show source, disassembly, memory, values, registers,
breakpoints, threads, tasks, signals, the console, and drawings. The command
palette lists available actions and keyboard shortcuts.

The stack marks each frame with a short language mark, such as `py`, `c`, `rs`,
or `go`, where its function or interpreter says which language it is. Each run
of a runtime's frames folds into one row, such as "6 frames of the runtime
folded". Click the row to open it. A row that holds the selected frame stays
open.

Each tab selects its own thread or task, frame, source location, and panel. All
tabs control the same session and receive the same stops.

The address bar records the tab's complete view. Copying the URL gives another
tab the same selected stop, frame, source, panel, and memory location when that
state is still available.

## Draw values

Views can attach visualizers to values. The Drawings panel can show a buffer as
an image, samples as a chart, or a mesh as an interactive model. See
[Writing views](writing-views.md) and [Visualizers](visualizers.md).

Renderers run in a sandbox. They receive only the data selected by a view and
cannot read arbitrary process memory.

## Open profiles

Pass one profile to explore it or several profiles to compare them:

```sh
microscope web current.usprof
microscope web current.usprof baseline.usprof
```

The profile pages include call trees, functions, flame views, timelines,
source and disassembly costs, blocked-time lanes, task stacks, and
instrumentation zones when the recording contains them.

A file with system calls, from `microscope syscalls -o FILE.usprof` or
`microscope record --syscalls`, has a Syscalls view. It lists calls in a table
you can group by call, class, process, thread, file, or call site, with a log of
the calls themselves, a heat map of their latency over time, and tabs for files,
network connections, and processes. See [System calls](syscalls.md).

With no target, the page can also start a program and trace its system calls.
It uses the ptrace engine, which needs no privilege. The trace updates every
two seconds while the program runs, and the Processes tab shows what each
thread is blocked in.

## Serve through a tunnel or proxy

The default loopback listener is available only on the same machine. Use an
SSH tunnel to reach it remotely.

`--listen ADDRESS` exposes the session directly. Anyone who can reach the
address and has a session link can control the debugger, so do not expose it to
an untrusted network.

For a reverse proxy, `--public-url URL` gives microscope the external origin and
path. The proxy is responsible for authentication and access control.
