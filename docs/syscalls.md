# System calls

`microscope syscalls` shows which files a program opens, why calls fail, and
where it waits on the kernel. Its output and most flags match strace. A strace
command line usually works with `microscope syscalls` in place of `strace`.

## Trace a program

Put the program and its arguments after `--`:

```sh
microscope syscalls -- ./server --port 8080
```

Each call prints as it returns:

```text
openat(AT_FDCWD, "/etc/hostname", O_RDONLY) = 3
read(3, "localhost\n", 262144)          = 10
close(3)                                = 0
+++ exited with 0 +++
```

Children and threads are traced too. Pass `--no-follow` to trace only the
program itself. When the trace ends, `microscope syscalls` exits with the
program's exit status.

To trace a process that is already running, give its ID:

```sh
microscope syscalls -p 1842
```

Stop tracing with Ctrl-C. The process keeps running. microscope never traces the
whole machine, so a target is always required.

## Choose what to show

`-e trace=` picks calls by name or by class, as strace does:

```sh
microscope syscalls -e trace=openat,close -- ./server
microscope syscalls -e trace=%file,%net -- ./server
```

Other filters narrow the output further:

| Option | Shows |
|---|---|
| `-z` | Only calls that succeeded |
| `-Z` | Only calls that failed |
| `-P PATH` | Only calls that name `PATH`, by argument or descriptor |
| `--trace-fds 3,4` | Only calls on these descriptors |
| `--slower-than 10ms` | Only calls that took at least this long |
| `--sample 1/N` | One call in N; the table still counts every call |

Use `-y` to show what each descriptor refers to, `-T` for call durations,
`-t` for the time of day, and `-s SIZE` to limit string length.
`--format ndjson` writes one JSON object per line for other programs.

`-T` and `-r`, which shows the time since the previous line, print
microseconds. As with strace, their long forms take a precision of `s`,
`ms`, `us`, or `ns`:

```sh
microscope syscalls --syscall-times=ns -e trace=close -- ./server
```

```text
close(3)                                = 0 <0.000003887>
```

The short forms take no value, as in strace: write `--syscall-times=ns`,
not `-Tns` or `-T=ns`. When both forms are given, the last one wins.

`-t` prints the time of day, `-tt` adds microseconds, and `-ttt` prints seconds
since the epoch. `--absolute-timestamps` (or `--timestamps`) accepts a format
(`time`, `unix`, or `none`) and precision (`s`, `ms`, `us`, or `ns`) in either
order, as strace does. For example, `--absolute-timestamps=unix,ns` prints
`1793023895.396960108`. Setting only format or precision keeps the other from
the preceding flag. `-t` and `--absolute-timestamps` cannot be combined. With
`-r`, the time since the previous line follows in `(+ )`.

A call that is still blocked after a second is reported while it waits, with
how long it has been blocked so far. Change the threshold with
`--in-flight`, or turn it off with `--in-flight off`.

Run `microscope syscalls --help` for every option.

## Count calls

`-c` prints a table of calls, errors, and time instead of the calls
themselves:

```sh
microscope syscalls -c -- ls /
```

```text
% time     seconds  usecs/call     calls    errors syscall
------ ----------- ----------- --------- --------- ----------------
 ...
100.00    0.003840          37       103        36 total
```

`-C` prints both. `-S` sorts the table and `-U` chooses its columns, which
include `p50`, `p90`, and `p99` latencies.

The table uses microsecond precision, like strace, so calls shorter than one
microsecond count as 0. `--syscall-times=ns` uses nanoseconds: `seconds` gets
nine decimal places, and the other time columns become `nsecs/call`,
`min nsecs`, `max nsecs`, and `p50 nsecs` through `p99 nsecs`:

```sh
microscope syscalls -c --syscall-times=ns -e trace=getppid -U name,calls,min-time,p50 -- ./server
```

```text
syscall              calls   min nsecs   p50 nsecs
---------------- --------- ----------- -----------
getppid                 20          60         202
---------------- --------- ----------- -----------
total                   20          60         202
```

strace ignores `-T` with `-c`. With `-c`, microscope takes only `us` or
`ns`, since milliseconds or seconds would turn even more calls into 0.
With `-C`, `s` and `ms` set the precision of the lines, and the table stays
in microseconds.

## Show where calls come from

`-k` prints the stack of each call. `-kk` adds source lines:

```sh
microscope syscalls -kk -e trace=openat -- ./server
```

The BPF engine walks frame pointers to find stacks. Programs built without
them give short stacks. Use `-k=dwarf` to copy part of the stack and unwind it
with debug information instead. Truncated stacks include a reason; broken
frame-pointer chains also name the module where they stopped. If missing
frame pointers truncate more than a quarter of displayed stacks, a final hint
names the module responsible for the most breaks and suggests `-k=dwarf`.

`-k=dwarf` still has a bounded stack copy. If it runs out of copied bytes,
the stack ends with `stack copy exhausted`, not a frame-pointer warning.

A Python program's stacks show each Python frame's function, file, and line,
with either engine; see [Python](languages.md#system-calls). In ndjson, such
a frame has `function`, `file`, and `line`, where a native frame has
`symbol`.

Node stacks show JavaScript frames among the native V8 frames that run them.
The ptrace engine reads stopped threads like the debugger, giving every frame
a line in every tier. The BPF engine names each function and gives interpreted
frames their lines. Sparkplug, Maglev, and Turbofan frames have no line because
the engine cannot prove which compiled code they ran. If V8's garbage collector
moves a function before microscope reads it, its frame is `<unknown>`, never
another function.

Calls made before microscope reads V8's layout, just after startup, end with
`interpreted frames unknown`. Native code that reuses the frame-pointer
register, such as an addon built without frame pointers, can hide JavaScript
frames from the BPF engine. Those stacks end with `interpreted frames ended
early`. See [Node.js](languages.md#system-calls-1).

## Save a trace and open it later

When the `-o` file name ends in `.usprof`, microscope saves the whole trace:

```sh
microscope syscalls -o server.usprof -- ./server
```

Any other `-o` name gets the text output, as strace writes it.

`microscope report server.usprof` prints the `-c` table. When the calls have
stacks, it then shows where they were made, the sites that took the most time
first:

```text
By call site:
% time     seconds     calls    errors   p99 usecs site
------ ----------- --------- --------- ----------- ----------------
 66.70    0.001027         1                   775 ld-linux-x86-64.so.2+0x1fd40
  5.83    0.000089        11                    13 mmap
  4.99    0.000076         9         7          12 __GI___open64_nocancel
  2.66    0.000041         1                    36 files · syscalls.c:19
 ...
------ ----------- --------- --------- ----------- ----------------
100.00    0.001540        63        16         775 total
and 10 more sites, counted in the total; --limit shows more
```

A call site identifies the program code that made the call by function, file,
and line. microscope skips C and Rust standard-library wrappers. Without debug
information, sites identify functions instead of lines. `--limit` sets the
number of sites shown (default 20).

`microscope web server.usprof` opens the trace in the browser, where you can
filter calls, group them by file, process, or call site, see latency over
time, and open any call to read its arguments and stack.

`microscope convert` exports saved calls for other tools:

| Format | Call representation |
|---|---|
| pprof | Stack samples with the call's name as the innermost frame, weighted by call count and duration in nanoseconds |
| Folded stacks | The same stacks, weighted only by call count |
| Chrome trace | Each returned call as a span on its thread, with result and call site |

```sh
microscope convert server.usprof server.pb.gz
microscope convert server.usprof server.folded
microscope convert server.usprof server.json
```

## Choose an engine

The default **BPF engine** runs small programs in the kernel. The traced
program never stops, and each call costs it about a tenth of a microsecond.
It needs the `CAP_BPF` and `CAP_PERFMON` capabilities, Linux 5.15 or newer, and
the kernel's BTF type information.

The **ptrace engine** stops the program at each traced call, as strace does.
It needs no special privilege, but each call costs several microseconds.
Choose it with `--engine ptrace`:

```sh
microscope syscalls --engine ptrace -- ./server
```

microscope never switches engines on its own. If the BPF engine cannot run,
microscope says why and stops.

When launching a program, ptrace filters calls in the kernel, so unselected
calls cost nothing. It cannot add that filter to a running process. Attaching
therefore stops the process at every system call and requires
`--stop-every-call` to acknowledge the cost.

## Give microscope the BPF privileges

The BPF engine needs `CAP_BPF` and `CAP_PERFMON`. Without them it stops and
explains what is missing.

From a systemd unit, grant them as ambient capabilities:

```ini
[Service]
AmbientCapabilities=CAP_BPF CAP_PERFMON
```

On NixOS, microscope's flake has a module that installs a wrapper with these
capabilities. Import `nixosModules.microscope-bpf`, rebuild, and run microscope
through it:

```sh
/run/wrappers/bin/microscope-bpf microscope syscalls -- ./server
```

The wrapper is open to members of the `wheel` group.

microscope cannot use a BPF token, which some container managers offer instead
of capabilities. When it finds one, the error says so.

## Trace a container or cgroup

Trace every process in a cgroup, including ones that start later:

```sh
microscope syscalls --cgroup /sys/fs/cgroup/system.slice/server.service
```

Or name a container by its ID. Docker, Podman, containerd, and CRI-O
containers are found by their cgroup names, and the first 12 characters of the
ID are enough:

```sh
microscope syscalls --container 3f9c2a1b8e07
```

Process IDs are shown as the processes inside see them, when they all share
one PID namespace. Otherwise microscope shows the host's IDs and says why. The
trace ends when you stop it or the cgroup is removed.

A cgroup trace needs the BPF engine. It cannot show stacks.

## Limit what tracing costs

`--budget PERCENT` holds microscope to a share of one CPU:

```sh
microscope syscalls --budget 1 --container 3f9c2a1b8e07
```

Each second over budget, microscope reduces detail another step: it drops
stacks, truncates long strings, samples one call in 2, then 4, up to 64, and
finally only counts calls. It never truncates paths and still counts every
call. It prints each step, saves it in the trace, and shows it below the trace
in the browser.

## Trace continuously

`--continuous DIR` traces until stopped and writes a file of statistics every
minute:

```sh
microscope syscalls --continuous traces --keep 24h --container 3f9c2a1b8e07
```

`--segment` sets how long each file covers, and `--keep` how long files are
kept. `microscope report traces` and `microscope web traces` read the files as
one trace.

The files hold counts and latencies, not individual calls. To keep the calls
around a problem, microscope holds the latest `--window` of calls (10 seconds by
default) and saves them to `traces/windows` when:

- a call takes at least `--trigger-slower-than`;
- a call fails with an error named by `--trigger-errno`, such as
  `EIO,ENOSPC`;
- microscope receives `SIGUSR2`.

microscope saves at most one window per window length, avoiding duplicate
windows during bursts of failures.

## Record system calls with a profile

`microscope record --syscalls` traces calls alongside a CPU profile, on the same
clock:

```sh
microscope record --syscalls %file,%net -o server.usprof -- ./server
```

It takes the same sets as `-e trace=`, or `all`. In the browser, the calls
appear on the timeline beside the samples. It uses the BPF engine and needs
its privileges, and it cannot be used with `--pid`. See
[Profiling](profiling.md#record-system-calls-too).

## Stop at a system call in the debugger

The debugger can stop a program at a system call. See
[Stop at system calls](debugging.md#stop-at-system-calls).

## What is not traced

Some calls, such as `clock_gettime` and `gettimeofday`, usually run in the
vDSO without entering the kernel. Neither engine sees them.

Under heavy load, the BPF engine can fall behind and drop calls. It says how
many:

```text
--- lost 4502 events ---
```

The kernel sends calls through a 16 MiB ring buffer, or 64 MiB with `-k=dwarf`,
whose stack copies make each call's records about 20 times larger. A separate
thread unwinds stacks; microscope names them as it displays them. This takes
a few microseconds per call. On a fast machine with a CPU to spare, more than
about 250,000 calls per second with `-k`, or 200,000 with `-k=dwarf`, can outrun
the reader. A busy machine handles fewer. `--ring 256M` gives bursts more room
but does not speed up the reader. Calls with lost stacks still appear with
`[stack ends: no stack]`; their stacks count among the losses.

For a short list of calls, the BPF engine attaches to the kernel functions
behind them, which costs other processes nothing. `exit` and `exit_group`
attach only at entry; the thread's exit completes their `= ?` record.
With function attachments, a call in progress is lost if another tracer
attaches to or detaches from the same function. microscope counts it as lost
rather than showing it without a result.
