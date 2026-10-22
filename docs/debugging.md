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

A script that starts with `#!`, such as `./cli.js` with `#!/usr/bin/env node`,
runs as the kernel runs it: microscope launches the script and follows its
interpreter. A program that executes another, as `env`, `bash -c`, and
version managers' shims do, is followed into the one it executes, with a line
saying so:

```console
$ microscope env -- node app.js
(microscope) break uv_run
breakpoint 1 set, pending until a module with its code loads
(pending: nothing loaded has it, and the program may execute one that does)
(microscope) run
process 6001 executed /usr/bin/node
stopped at breakpoint 1 (hit 1) in uv_run from libuv.so.1
```

A breakpoint whose function or file nothing loaded has is refused, unless
the program can execute another; then it waits for one that has it. A
misspelling of a name the program has is still refused with the nearest
name. `catch exec` stops at each program the process executes.

## Attach to a process

Attach by process ID or by a name that matches exactly one process:

```sh
microscope --attach 1842
microscope --attach my-server
microscope --attach server.js
```

A name matches a process's command name, the file name of the program it
was started as, a title it set (npm's `npm start` matches `npm`), or the
file name of a script it runs (`node server.js` matches `server.js`). When
several processes match, microscope lists each with what matched, and you
attach to one by its ID.

microscope normally finds the executable through `/proc`. Supply it as the
positional executable when that is not possible.

Quitting an attached session detaches and leaves the process alive. Killing an
attached process requires an explicit command.

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

Use `info breakpoints` to list breakpoints, and `disable`, `enable`, or
`delete` with a breakpoint number to manage one. `tbreak` creates a temporary
breakpoint removed after its first hit.

A resolved location can still be waiting for its language runtime to enable
it. Pending locations show the reason; other locations of the same breakpoint
can already be ready. Earlier instrumentation can still report hits while a
new configuration waits for confirmation. The terminal, browser, and editor
retain that distinction.

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

With no call named, every call stops. `entry` or `exit` stops only as the
call begins or as it returns; the default is both. At the stop, `$arg0` to
`$arg5` hold the call's arguments, `$ret` what it returned, and `$path` the
path it names:

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

Each run of a runtime's own frames, such as Go's or CPython's, folds into one
line in `bt`. `bt -r` shows every frame.

Use `threads` to list native threads and `thread ID` to select one. Programs
with a supported runtime can also expose logical tasks such as goroutines,
async Rust tasks, and asyncio tasks. See [Languages](languages.md).

## Inspect values

`print EXPRESSION` evaluates an expression. `pp` expands it in a multiline
form. Format suffixes choose common numeric displays:

```text
print/x flags
print/d address
print/t mask
```

microscope uses one [expression language](expressions.md) for every source
language. [Views](views.md) make containers, strings, maps, and sum types read
as their logical values.

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
