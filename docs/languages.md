# Languages

microscope uses DWARF and native process state to debug programs written in C,
C++, Rust, Go, Zig, Odin, Fortran, D, Nim, and Ada. The core debugger behavior
is shared across languages. Runtime support adds logical tasks and
language-specific presentation where the program provides enough information.

## C and C++

Source stepping, native stacks, variables, expressions, and standard-library
views are supported. Built-in views cover common libstdc++ and libc++ strings,
sequences, maps, pointers, variants, and related containers.

Optimized C++ often has inlined calls, split variables, and template names that
depend on compiler output. microscope reports unavailable locations instead of
reconstructing values without evidence.

## Rust

Rust programs get native debugging plus views for common standard-library
types. Enums, slices, strings, vectors, maps, smart pointers, and other
containers are presented as logical values where their debug information and
views permit it.

### Async Rust and tokio

microscope can discover tokio runtimes and show their tasks. An async backtrace
follows the chain of suspended futures rather than only the native worker
thread. Stepping follows the selected task across an `.await` when the runtime
and debug information expose its continuation.

Task state is a snapshot of one debugger stop. A task changing while the
program runs is read again at the next stop.

Runtime internals and compiler output change across versions. When microscope
cannot identify a task or continuation safely, it leaves that part unavailable
instead of guessing.

## Go

microscope reads Go runtime metadata and presents goroutines as tasks. A running
goroutine is associated with its native thread; a parked goroutine can be
unwound from its saved context. Stacks can cross Go stack growth boundaries.

Stepping belongs to the selected goroutine, so work performed by another
goroutine does not complete its plan. Panics and fatal runtime failures stop at
the relevant program state when the runtime reports enough information.

Go maps, slices, strings, interfaces, channels, and common runtime-backed
values use built-in views. Compiler optimization and runtime-version changes
can make individual variables or goroutines unavailable.

## Python

microscope reads the memory of a CPython interpreter and shows its Python frames
in the native backtrace. Nothing runs inside the Python process.

microscope supports CPython 3.12, 3.13, 3.14, and 3.15, with the GIL and
free-threaded builds (3.13t and later). It is tested with nixpkgs builds,
python-build-standalone (as uv installs), and Debian and Ubuntu builds. That
includes statically linked, stripped interpreters, and builds with or without
frame pointers. libpython may be a shared library or linked into the
executable. Debug symbols for the interpreter are not required.

Python works when you launch a program, attach to a process, or open a core
dump:

```sh
microscope python3 app.py
microscope -p PID
microscope --core core python3
```

A CPython version outside 3.12 to 3.15, or a release microscope has not
checked, is refused by name. The backtrace says why at the interpreter's
evaluation loop, and the native frames still appear as usual.

### Stacks

Each thread's Python frames appear in its native backtrace, before the
interpreter's evaluation loop (`_PyEval_EvalFrameDefault`) that runs them. A
Python frame shows its qualified function name, file, and line, with no
address:

```text
#9                     in Condition.wait at /usr/lib/python3.14/threading.py:369
#10                    in Event.wait at /usr/lib/python3.14/threading.py:670
#11                    in containers at app.py:72
#12                    in Thread.run at /usr/lib/python3.14/threading.py:1024
    … #13–#18: 6 frames of the runtime; `bt -r` shows them
#19                    in Thread._bootstrap_inner at /usr/lib/python3.14/threading.py:1082
```

The interpreter's own frames between Python frames, such as its evaluation loop
and call machinery, fold into one line. Go and tokio runtimes fold their frames
the same way. C functions that Python calls, such as a lock's acquire, are
shown and not folded. `bt -r` shows every frame. The
[`[backtrace]` setting](reference/configuration.md) and `set runtime-frames`
choose whether backtraces show them by default. `up` and `down` pass over
folded frames; `frame N` selects any frame by its level.

`list`, `frame`, `where`, `context`, and `edit` work in a Python frame. They
read the file at the path the interpreter recorded, through source path maps
like any other source. `where` prints a location such as `containers at
app.py:72`. Python source is syntax-highlighted in the terminal and the
browser.

### Variables

`print` with no expression shows a Python frame's parameters and locals, in
order:

```text
(list) numbers = [1, 2, 3]
(tuple) single = (1,)
(dict) by_name = {'a': 1, 'b': 'two', 'c': None}
(int) big = 1267650600228229401496703217721
(truth.Gate) gate = <truth.Gate object at 0x51a68591990>
(module) truth = <module 'truth'>
(builtin_function_or_method) len = <built-in function len>
```

Values use Python's `repr` for None, True and False, int of any size, float,
str, bytes, list, tuple, dict, set, and frozenset. A float uses the shortest
round-trip form, such as `0.1`, `1e+16`, `inf`, or `nan`. A dict keeps its
insertion order, and a set the order of its table, which is the order Python
iterates it in. Lists, tuples, dicts, and sets expand into their items in
clients that expand values, such as the browser and editors.

An instance of a class shows its type and address, as Python's default
`repr` does, and expands into its attributes: those its dict holds, in
insertion order, then those its `__slots__` hold. An instance of a class
that derives from a built-in type, such as a `namedtuple` or an `IntEnum`
member, shows that type's value and expands into its items, then its
attributes. Classes, modules, functions, built-in functions and methods,
and code objects show as Python's `repr` names them. Nothing runs in the
program, so a class's own `__repr__` is not used. Any other object shows
its type and address.

A summary reads at most 6 containers deep, 64 items of each container, and 1024
characters of a string. It says when it stops short. `pp` lays containers out
over lines in the generic form microscope uses for every language, such as
`len=3 [1, 2, 3]`.

A local that is not yet assigned shows as unbound. A local that a nested
function shares (a cell) shows the value the cell holds.

### Expressions

In a Python frame, a name is the frame's local, then a global of its module,
then a built-in, as Python finds it. `::name` skips to the native program's
globals. Expressions index lists and tuples, find a dict's keys as Python's
own lookup finds them, read attributes, take `len`, compare, and compute
with numbers:

```text
(microscope) print numbers[-1]
(int) numbers[-1] = 3
(microscope) print by_name["b"]
(str) by_name["b"] = 'two'
(microscope) print numbers[0] + len(by_name) * 10
(integer) numbers[0] + len(by_name) * 10 = 31
(microscope) print mixed[None]
(bytes) mixed[None] = b'n'
(microscope) print gate._count
(int) gate._count = 4
(microscope) print truth._DEPTH
(int) truth._DEPTH = 6
(microscope) print gate.wait
error: `wait` of a `truth.Gate` is a `function`, which binds with code microscope does not run, as a method does
```

A Python variable's type is whatever object it holds when the expression
runs. microscope never runs code in the program: it refuses what only a
type's own code could answer, such as comparing lists, finding a dict key
of a class's instance, or reading a property, rather than guess. Python
divides integers into a fraction and floors a remainder, so an integer `/`,
and `%` of a negative number or a float, are refused.
[Expressions](expressions.md#python-values) gives the rules.

### C extensions

In a native frame of a CPython process, such as a C extension's function, a
`PyObject *` shows the Python value it points to and expands as that value
does, so the frame's locals read as the objects they hold:

```text
(PyObject *) numbers = [1, 2, 3]
(PyObject *) point = <__main__.Point object at 0x7fffe9234d70>
(PyObject *) missing = 0x0000000000000000
```

`(py)` casts an address to the object there, so that expressions reach into
it as in a Python frame:

```text
(microscope) print ((py)point).x
(int) ((py)point).x = 3
(microscope) print len((py)numbers)
(integer) len((py)numbers) = 3
```

### Not supported yet

Python breakpoints, stepping through Python code, and Python exceptions as
catchpoints are not supported yet. Python stacks do not appear in `microscope record`, and
asyncio tasks are not shown. [Known limits](reference/limitations.md) lists
them all.

In editors, Python frames appear in the call stack with their source file and
line, but no instruction reference. Their scopes are Arguments and Locals.

Native breakpoints, stepping, and everything else in the native code of a Python
process work as usual.

## Zig, Odin, D, Nim, Ada, and Fortran

These languages use the shared native debugger and expression engine. microscope
normalizes their DWARF types and names where needed and includes built-in views
for common runtime or standard-library values where maintained.

Runtime-specific task scheduling is currently limited to Go and tokio. Other
languages appear as native threads and frames.

## Debug information decides what is available

Language support does not mean every value survives optimization. Compilers
may omit a variable, reuse its storage, describe only part of its lifetime, or
emit no unwind rule for a frame. microscope shows the available part and the
reason for a gap.

See [Known limits](reference/limitations.md) for host and format support, and
[Troubleshooting](reference/troubleshooting.md) when source, symbols, or values
are missing.
