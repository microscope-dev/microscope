# Languages

microscope debugs native programs, Python, and Node.js on Linux x86-64.
This page covers what changes between languages; [Debugging](debugging.md)
covers the shared commands.

Launch a program, attach to a process, or inspect a core dump:

```sh
microscope ./server -- --port 8080
microscope --attach 1842
microscope --core core ./server
```

Put the executable before `--` and its arguments after it. Core dumps are
read-only. The terminal, browser, and [editors](editors.md) use the same
debugger.

## C and C++

Build with debug information for source stepping and local variables; see
[Build a program for debugging](getting-started.md#build-a-program-for-debugging).
Built-in [views](views.md) display common libstdc++ and libc++ containers,
including strings, vectors, maps, and smart pointers.

## Rust

Rust has native debugging and views for enums, strings, slices, vectors,
maps, and smart pointers.

### Async Rust and tokio

`tasks` lists tokio tasks. Select one with `task ID`, then use `bt` to follow
its chain of suspended futures and inspect their variables. Stepping follows
the selected task across `.await`, even when another worker resumes it.

Support covers Rust 1.85 and later with tokio 1.51 through 1.54. Other tokio
versions are marked unverified. Before Rust 1.93, optimized async frames and
non-tokio executors have additional limits.

## Go

`tasks` lists goroutines, including parked ones. Select one with `task ID`
to inspect its stack and variables. Stepping stays with that goroutine rather
than stopping when another goroutine reaches the next line.

Built-in views display maps, slices, strings, interfaces, and channels.

## Python

CPython 3.12 through 3.15 is supported, including free-threaded builds.
Interpreter debug symbols are not required. Python frames appear alongside
native frames, so you can inspect Python and its C extensions in one stack.

### Breakpoints and stepping

Save this as `app.py`:

```python
def total(prices):
    subtotal = sum(prices)
    return subtotal

print(total([12, 8, 5]))
```

Launch it, then stop before the return and inspect the result:

```sh
microscope python3 -- app.py
```

```text
(microscope) break app.py:3
(microscope) run
(microscope) print subtotal
(int) subtotal = 25
```

Use `break py:total` for a function, or `break py:Shape.area` for a method.
`next` steps over a line, `step` enters a call, and `finish` returns to the
caller. Stepping over an `await` stays with the coroutine. A step can enter
a C extension when its function has debug information.

Python breakpoints, steps, and exception stops load a small agent into the
interpreter. On CPython 3.14 and later, they work on launch or attach when
remote debugging is allowed. On 3.12 and 3.13, launch the interpreter directly,
without `-I`, `-E`, or `-S`. Python breakpoints do not work on free-threaded
3.13. A Python step must start at an agent stop, such as a Python breakpoint;
from a native stop, set a Python breakpoint and continue to it first.

### Values and tasks

`print` reads locals, attributes, and container items without running Python
code. For the example above, `print prices[-1]` gives `5`. Built-in types,
common standard-library objects, and NumPy arrays have readable summaries
and expandable items. Custom `__repr__`, properties, and methods do not run.
See [Python expressions](expressions.md#python-values).

In a C extension, a `PyObject *` displays the Python object it points to.
Use `(py)` to inspect an object by address, for example `print ((py)point).x`.

From CPython 3.14, `tasks` lists asyncio tasks. Use `task ID` and `bt` to
inspect a waiting task's coroutine stack and locals. A suspended task cannot
be stepped.

`watch subtotal` stops when a frame variable changes to a different object.
It does not watch attributes or container items. `set var subtotal = 41`
can change a local only on CPython 3.14 or later with the GIL, at an agent
stop, while its thread is the interpreter's only thread. See
[Assignment](expressions.md#assignment) for the restrictions.

### Exceptions

```text
(microscope) catch python-uncaught on
(microscope) catch python-raised on
```

Choose `python-uncaught` for exceptions leaving a thread's outermost Python
frame, or `python-raised` for every raise, including caught exceptions.
Both are off by default. `python-uncaught` excludes `SystemExit`. An uncaught
stop occurs after the inner frames have returned; its message names where
the exception was raised.

### System calls

`microscope syscalls -k -- python3 app.py` includes Python frames in syscall
stacks. With the BPF engine, `-k=dwarf` places them among native frames;
plain `-k` puts them after the first native frame.

For profiles, see [Record Python programs](profiling.md#record-python-programs).

## Node.js

Node 22, 24, and 26 are supported. JavaScript frames appear alongside native
frames, with values displayed in Node's `util.inspect` style. Reading stacks
and values does not run JavaScript. Breakpoints, stepping, and exception
stops use Node's inspector, which microscope opens and connects to itself.

### Breakpoints and stepping

Save this as `app.js`:

```js
function total(prices) {
  const subtotal = prices.reduce((sum, price) => sum + price, 0);
  return subtotal;
}

console.log(total([12, 8, 5]));
```

Launch it, then stop before the return and inspect the result:

```sh
microscope node -- app.js
```

```text
(microscope) break app.js:3
(microscope) run
(microscope) print subtotal
(number) subtotal = 25
```

Use `break js:total` for a function. Function breakpoints take the function's
own name, such as `js:area`, not `js:Shape.area`. TypeScript breakpoints and
stepping use the original source when the script provides a source map.

`next`, `step`, and `finish` step over, into, and out of calls. Steps skip
Node's own scripts by default. To skip dependencies too, add this to your
[configuration](reference/configuration.md):

```toml
[step]
skip = ["**/node_modules/**"]
```

Workers have their own breakpoints and steps. A step can enter a Node-API
addon's C function when debug information is available, except on Debian's
Node build.

A breakpoint that cannot be armed stays pending with a reason. When attaching,
an application signal handler or disabled inspector can prevent microscope
from opening it. See [Known limits](reference/limitations.md#languages-and-runtimes).

### Values and async work

`print` reads arguments, locals, and closure variables. At native stops,
some locals appear as unnamed registers because V8 does not retain their
names. Inspector stops provide more names. See
[JavaScript expressions](expressions.md#javascript-values).

Backtraces include async callers waiting on a resumed async function. Select
a suspended frame to inspect its saved values. `tasks` (also `handles`)
lists event-loop handles, pending requests, and timers across the main thread
and workers. These items have callbacks, not suspended call stacks.

`watch subtotal` watches a frame variable, not an object property. Watches
follow captured variables when garbage collection moves them, but some
optimized variables cannot be watched. JavaScript watches are unavailable
on Debian's Node build. See [Known limits](reference/limitations.md#languages-and-runtimes).

### Assignments

At an inspector stop, `set var subtotal = 41` changes a frame variable.
Properties, globals, `this`, and some optimized variables cannot be assigned.
The statement V8 paused at may still use the old value. See
[Assignment](expressions.md#assignment).

### Exceptions

```text
(microscope) catch javascript-uncaught on
(microscope) catch javascript-thrown on
```

Choose `javascript-uncaught` for uncaught exceptions and unhandled promise
rejections, or `javascript-thrown` for all throws and rejections. Both are
off by default.

### System calls

`microscope syscalls -k -- node app.js` includes JavaScript frames in syscall
stacks. The ptrace engine provides source lines for every frame; the BPF
engine names compiled frames by function, with lines for interpreted frames.
See [Syscall stacks](syscalls.md#show-where-calls-come-from).

For profiles, see [Record Node programs](profiling.md#record-node-programs).

## Zig, Odin, D, Nim, Ada, and Fortran

These languages use the native debugger for source stepping, stacks,
variables, and expressions. Built-in views cover selected runtime and
standard-library types. Tasks appear as native threads unless microscope
has a model for their runtime.

## Missing source or values

Optimized builds can lose variables or make stepping differ from the source.
Unknown runtime versions can leave logical frames or tasks unavailable.
`bt -r` reveals runtime frames normally folded out of a backtrace.

See [Troubleshooting](reference/troubleshooting.md) for missing source,
symbols, or values, and [Known limits](reference/limitations.md) for the
support boundaries.
