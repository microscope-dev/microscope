# Debugging

microscope can launch a program, attach to a running process, or inspect a core
dump. The terminal, browser, and editor interfaces use the same debugger.

## Launch a program

Pass the executable before `--` and its arguments after it:

```sh
microscope ./server -- --port 8080
```

Use `--cwd DIR` to choose the program's working directory and `--env
NAME=VALUE` to add environment variables. A launched program stops before it
begins. Set breakpoints, then run it.

microscope follows `#!` scripts into their interpreters, and follows wrappers
such as `env`, `bash -c`, and version-manager shims into the programs they
execute. It reports each change:

```console
$ microscope env -- node app.js
(microscope) break uv_run
breakpoint 1 set, pending until a module with its code loads
(pending: nothing loaded has it, and the program may execute one that does)
(microscope) run
process 6001 executed /usr/bin/node
stopped at breakpoint 1 (hit 1) in uv_run from libuv.so.1
```

A breakpoint in an unknown function or file is rejected unless the program
can execute another program; then it stays pending. Misspellings of known
names are still rejected with a suggested correction. `catch exec` stops
after each exec.

## Attach to a process

Attach by process ID or by a name that matches exactly one process:

```sh
microscope --attach 1842
microscope --attach my-server
microscope --attach server.js
```

A name matches the command name, original executable's filename, process title
(npm's `npm start` matches `npm`), or script filename (`node server.js` matches
`server.js`). Multiple matches are listed with their matching names; choose
one by ID.

microscope normally finds the executable through `/proc`. Supply it as the
positional executable when that is not possible.

Quitting an attached session detaches and leaves the process alive. Killing an
attached process requires an explicit command.

## Debug the processes a program starts

A program and its debugged child processes form a group. Each process is a
member, numbered from `[1]` for the original process. Members share breakpoints,
catchpoints, signal policy (`handle`), exception stops, `catch exec`, and view
settings. Changes apply to existing and future members; new members receive
breakpoints before their first instruction.

A stop selects the member that stopped and pauses the others, unless
`[follow] stop = "process"`:

```text
(microscope) break child_work
breakpoint 1 set at child_work at tests/fixtures/c/process-tree.c:31
(microscope) run
[2] process 1063951 (process-tree 1 direct) joined, started by [1]
in [2] process 1063951 (process-tree 1 direct)
stopped at breakpoint 1 (hit 1) in child_work at tests/fixtures/c/process-tree.c:31
(microscope [2]) processes
  [1] process 1063941 (process-tree 1 direct)  inferior paused
* [2] process 1063951 (process-tree 1 direct)  stopped at breakpoint 1 (hit 1)
(microscope [2]) process 1
selected [1]
```

Process IDs differ from run to run. Once the group has a second member, the
prompt shows the selection. `backtrace`, `print`, and `step` act on that
member. `continue` resumes all members, but first reports any additional stops
collected while pausing the group, without resuming. `detach process N`
releases a stopped member while the others remain debugged; `[1]` cannot be
detached alone. Joins, exits, and detachments are reported as they happen,
including between commands.

The follow policy decides which processes are members. Choose it with
`--follow MODE` or the [`[follow]` settings](reference/configuration.md#processes-the-program-starts):

| Mode | Members |
|---|---|
| `auto` (default) | Processes running the original program, Node.js, Python, or Go |
| `all` | Every process the program starts |
| `none` | Only the original process; children run without microscope, with a notice |

A fork child inherits its parent's program, so it joins the group under
`auto` and `all`. Other processes, such as a shell between `npm` and `node`,
are watched until they execute a matching program. A child member that execs
a non-matching program returns to watched status, with a notice. The original
process `[1]` is always followed across exec. A vfork child, as created by
`system` or `posix_spawn`, stays watched until exec.

Repeat `--follow` with filename patterns using `*` and `?` to include more
programs:

```sh
microscope --follow 'worker*' ./server
```

Native and JavaScript breakpoints work in a Node.js member, as described
under [Node.js](languages.md#nodejs). Python breakpoints in members are
described under [Python](languages.md#python).

An attached group follows the processes the program starts from then on.
Processes it started before the attach are not members.

In a `--batch` run, microscope exits with an error when a process executed a
program the policy would debug but microscope could not load.

## Open a core dump

```sh
microscope --core core.1842
```

microscope uses the executable recorded in the dump when it can find a matching
file. For a dump copied from another machine, `--sysroot` points at a copy of
that machine's file tree. `--module-path` adds directories to search for
missing modules.

A core dump is read-only. You can inspect threads, stacks, values, memory, and
instructions, but cannot resume or modify the program.

## Set breakpoints

Break on a function, source location, or address:

```text
break handle_request
break server.c:84
break *0x401240
```

Add `if EXPRESSION` for a condition:

```text
break handle_request if request.id == 42
```

Put a location that holds spaces, or that is one of the words `if`, `hits`,
`log`, and `disabled`, in double quotes:

```text
break "<impl Display for Point>::fmt"
```

Use `info breakpoints` to list breakpoints, and `disable`, `enable`, or
`delete` with a breakpoint number to manage one. `tbreak` creates a temporary
breakpoint removed after its first hit.

A resolved location may stay pending until its runtime enables it; the reason
is shown. Other locations of that breakpoint may already be active, and older
instrumentation may report hits while an update awaits confirmation. All
clients distinguish pending and active locations.

`advance LOCATION` runs to a temporary source location in the current frame.

## Use watchpoints

A watchpoint stops when the CPU writes to an address:

```text
watch counter
watch buffer[3]
```

Watchpoints use the machine's limited hardware debug registers. The watched
expression must resolve to an address and a supported size. A watchpoint stays
at that address; if the variable later lives elsewhere, create a new one.

## Stop at system calls

A catchpoint stops the program when it makes a system call:

```text
catch syscall openat
catch syscall entry connect
catch syscall openat if $path == "config.toml"
```

With no call named, every call stops. `entry` and `exit` select one phase;
the default is both. `$arg0` through `$arg5` hold arguments, `$ret` the return
value, and `$path` the path:

```text
stopped at catchpoint c1 as the call began: openat(AT_FDCWD, "missing", O_RDONLY)
(microscope) print $path
(string) $path = "missing"
(microscope) continue
stopped at catchpoint c1 as the call returned: openat(AT_FDCWD, "missing", O_RDONLY) = -1 ENOENT (No such file or directory)
(microscope) print $ret
(integer) $ret = -2
```

`info catchpoints` lists catchpoints and `delete c1` removes one.

While a catchpoint exists, every system call stops its thread briefly, even
calls no catchpoint names, so the program runs slower. To watch calls without
stopping the program, use [`microscope syscalls`](syscalls.md) instead.

## Step through code

```text
step       enter calls on the next source line
next       run the next source line without entering calls
finish     run until the selected frame returns
stepi      run one machine instruction
nexti      run one instruction, stepping over a call
continue   resume until another stop
```

Source stepping follows inline calls and skips compiler-generated code where
the debug information identifies it. In optimized code, one instruction may
belong to several source locations or none.

The debugger is all-stop: when one thread stops, all threads stop before a
state is shown. Resuming runs them together.

## Inspect the stack

`bt` shows the selected thread's frames. `frame N`, `up`, and `down` select a
frame. Expressions and locals use the selected frame's scope.

```text
bt
frame 2
locals
print request
```

Consecutive runtime frames, such as Go's or CPython's, fold into one line.
`bt -r` shows every frame.

Use `threads` to list native threads and `thread ID` to select one. Programs
with a supported runtime can also expose logical tasks such as goroutines,
async Rust tasks, and asyncio tasks. See [Languages](languages.md).

Threads can have runtime labels. Node threads show their V8 isolate, whose
number is unique and never reused within the process:

```text
* 353561 "node-MainThread" stopped [isolate 1]
  353565 "node-V8Worker" stopped
  353571 "alpha" stopped [isolate 2]
  353572 "beta" stopped [isolate 3]
```

The main thread and `worker_threads` workers always have labels. V8 helper
threads, which compile code or mark the heap, have a label only while working
for an isolate. An unlabeled thread runs no JavaScript, but a label alone
does not imply JavaScript execution.

The debug adapter adds labels to the thread's name, as
`node-MainThread (353561, isolate 1)`. The browser shows them in the
Threads pane.

## Inspect values

`print EXPRESSION` evaluates an expression. `pp` expands it in a multiline
form. Format suffixes choose common numeric displays:

```text
print/x flags
print/d address
print/t mask
```

microscope uses one [expression language](expressions.md) for every source
language. [Views](views.md) display containers as elements, strings as text,
and sum types as their active values.

When a variable was optimized out, a memory read failed, or debug information
cannot describe a value, the output states that reason.

## Inspect low-level state

Use `registers` for the selected frame's registers, `disassemble` for nearby
instructions, and `x` to examine memory. The command reference documents their
forms and output controls.

## Handle signals

`signals` lists signal policy. A signal that stops one thread is kept for that
thread while the debugger gathers the all-stop state. Continue can deliver or
suppress it according to the configured policy.

## Run commands without a prompt

`-e` runs one startup command and may be repeated. `-c` reads commands from a
file. `--batch` exits after those commands instead of opening the REPL:

```sh
microscope --batch \
  -e 'break parse' \
  -e run \
  -e bt \
  ./parser -- input.txt
```

Batch mode returns a failing status when a command fails.
