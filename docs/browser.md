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

The [`[follow]` setting](reference/configuration.md#processes-the-program-starts)
controls which child processes are debugged. With multiple processes, a list
appears above the stop banner. Selecting a process changes it in every tab;
a stop selects and marks the process that stopped. Detach releases a stopped
child process while the others remain debugged; it cannot release the original
process alone. See
[Debug the processes a program starts](debugging.md#debug-the-processes-a-program-starts).

## Move around

The workspace can show source, disassembly, memory, values, registers,
breakpoints, threads, tasks, signals, the console, and drawings. The command
palette lists available actions and keyboard shortcuts.

Ctrl+P lists source files named in debug information. For Python programs
launched through the browser or `microscope web`, it also lists `.py` files
from the script's directory (the working directory for `-c` or `-m`). It adds
`PYTHONPATH` directories unless `-E` or `-I` ignores the environment. It skips caches,
hidden directories, `node_modules`, `site-packages`, virtual environments, and
symlinks. Large trees may be listed only partly, with a notice in the palette.
The script opens before launch; standard-library and installed-package files
become available when they appear in a stack.

Stack frames have language labels such as `py`, `c`, `rs`, or `go` when known.
Consecutive runtime frames fold into a row such as "6 frames of the runtime
folded". Click to expand it; a group containing the selected frame stays open.

Each tab selects its own thread or task, frame, source location, and panel. All
tabs control the same session and receive the same stops.

The URL records the selected stop, frame, source, panel, and memory location.
Open it in another tab to restore that view while the state remains available.

## Draw values

Views can attach visualizers to values. The Drawings panel can show a buffer as
an image, samples as a chart, or a mesh as an interactive model. See
[Views](views.md) and [Visualizers](visualizers.md).

Renderers run in a sandbox. They receive only the data selected by a view and
cannot read arbitrary process memory.

## Open profiles

Pass one profile to explore it or several profiles to compare them:

```sh
microscope web current.usprof
microscope web current.usprof baseline.usprof
```

Profile pages show call trees, functions, flame views, timelines, source and
disassembly costs, blocked time, task stacks, and instrumentation zones when
recorded. The Python stacks switch hides interpreter frames in profiles
containing Python stacks.

A recording from `microscope syscalls -o FILE.usprof` or
`microscope record --syscalls` has a Syscalls view: calls grouped by name, class,
process, thread, file, or call site; an event log; a latency heat map; and tabs
for files, network connections, and processes. See [System calls](syscalls.md).

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
