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

Programs built by Rust 1.85 or later and tokio 1.51 to 1.54 are supported.
Another tokio release is read as tokio 1.54 is laid out, and the task list
says that its version is unverified. Rust before 1.93 does not say in its
debug information which future an async function's frame runs, so
microscope finds it from the task's chain of awaits. Task lists, backtraces,
and `$future` and steps in unoptimized tokio tasks work there; a future run
by an executor other than tokio's, an async function inlined into its caller
by an optimized build, and some steps whose future is cancelled stay
unavailable.

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
in the native backtrace. Stacks, values, and profiles run nothing inside the
Python process. [Breakpoints](#breakpoints), [stepping](#stepping), and
[exceptions](#exceptions) do: microscope asks the interpreter to load a small
agent of its own, and only once one of them is set.

microscope supports CPython 3.12, 3.13, 3.14, and 3.15, with the GIL and
free-threaded builds (3.13t and later). It is tested with nixpkgs builds,
python-build-standalone (as uv installs), and Debian and Ubuntu builds. That
includes statically linked, stripped interpreters, builds with or without
frame pointers, and builds whose evaluation loop tail-calls a function for each
instruction, as python-build-standalone's 3.14 and 3.15 do. libpython may be a shared library or linked into the
executable. Debug symbols for the interpreter are not required.

Python works when you launch a program, attach to a process, or open a core
dump:

```sh
microscope python3 -- app.py
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

CPython 3.15's JIT, on under `PYTHON_JIT=1`, writes each trace it compiles to
memory and registers it through the GDB JIT interface. Each registered trace
is a module of its own: a stack goes on through its code, named
`py::jit:executor` and folded with the interpreter's frames, and its Python
frames appear before the evaluation loop that entered it.

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
round-trip form, such as `0.1`, `1e+16`, `inf`, or `nan`. A str escapes what
does not print, such as `\xa0` or `\u202e`, as `repr` does; a character only
a later Unicode version than the interpreter's assigns shows as itself. A dict keeps its
insertion order, and a set the order of its table, which is the order Python
iterates it in. Lists, tuples, dicts, and sets expand into their items in
clients that expand values, such as the browser and editors.

An instance of a class shows its type and address, as Python's default
`repr` does, and expands into its attributes: those its dict holds, in
insertion order, then those its `__slots__` hold. An instance of a class
that derives from a built-in type, such as a `namedtuple` or an `IntEnum`
member, shows that type's value and expands into its items, then its
attributes. A dataclass's fields come first among its attributes, in the
order it defines them. Classes, modules, functions, built-in functions and
methods, and code objects show as Python's `repr` names them. Nothing runs
in the program, so a class's own `__repr__` is not used. Any other object
shows its type and address.

The standard library's types that only C defines show as their own `repr`
writes them: `collections.deque`, `defaultdict`, and `OrderedDict`, which
expand into their items, an `OrderedDict`'s in the order `move_to_end`
gives them; `datetime`'s `date`, `time`, `datetime`, `timedelta`, and
`timezone`; and `decimal.Decimal`, whose exponent is written `E`, as the
default context writes it. An object of a class deriving from one of them
shows its value too. The [built-in views](views.md#python-objects) present an
enum's member as its name and value, a `fractions.Fraction` as `str` writes
it, and a `pathlib` path as its text once Python has made it, which any use
of the path as a file name does:

```text
(collections.deque) bounded = deque([0, 1, 2], maxlen=5)
(collections.OrderedDict) ordered = OrderedDict({'c': 3, 'a': 1, 'd': 4})
(datetime.datetime) utc_moment = datetime.datetime(2000, 1, 1, 0, 0, tzinfo=datetime.timezone.utc)
(decimal.Decimal) small = Decimal('1E-7')
(__main__.Color) color = {name: 'RED', value: 1}
(fractions.Fraction) half = 1/2
(pathlib.PosixPath) home = '/home'
```

A numpy array shows as `array(ELEMENTS, dtype=NAME)`, its elements written
as Python writes the numbers, and expands into its elements, or into its
rows when it has more than one axis.
microscope reads arrays of `bool`, the signed and unsigned integers, `float32`,
and `float64`, in any strides; an array of another dtype, or of big-endian
elements, shows its shape and dtype. A `float32` is written in the shortest
digits of its own width. An array whose numbers lie next to each other,
and each such row of a larger array, is read at once for
[drawings](visualizers.md), so **Draw as…** a line plot of a million
numbers takes one read:

```text
(numpy.ndarray) grid = array([[0, 1, 2, 3], [4, 5, 6, 7], [8, 9, 10, 11]], dtype=int32)
(numpy.ndarray) singles = array([0.1, 16777216.0, 1e-05], dtype=float32)
(numpy.ndarray) complex_numbers = <numpy.ndarray of shape (1,), dtype complex128>
```

A summary reads at most 6 containers deep, 64 items of each container, and 1024
characters of a string. It says when it stops short. `pp` lays containers out
over lines in the generic form microscope uses for every language, such as
`len=3 [1, 2, 3]`.

A local that is not yet assigned shows as unbound. A local that a nested
function shares (a cell) shows the value the cell holds.

### Expressions

In a Python frame, a name is the frame's local, then a global of its module,
then a built-in, as Python finds it. `::name` skips to the native program's
globals. Expressions index lists, tuples, and deques, find the keys of a
dict, a `defaultdict`, or an `OrderedDict` as Python's own lookup finds them,
read attributes, take `len`, compare, and compute with numbers. A dict keyed
by tuples takes a tuple key, `grid[1, 2]` or `grid[cell]`, compared item by
item as Python compares tuples. An item of a
numpy array is a new object numpy makes, so indexing one is refused; expand
the array instead:

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

`set var`, the console, and an editor's **Set Value** give a variable of a
Python frame a number, text, a truth value, `None`, or an object another
name holds. This works on CPython 3.14 or later built with the GIL, at a stop
microscope's agent made, while the frame's thread is the interpreter's only
thread:

```text
(microscope) set var count = 41
(int) count = 41
(microscope) set var held = other
(list) held = [1, 2]
```

Elsewhere it is refused, since another thread could be using what it
changes. [Assignment](expressions.md#assignment) says why.

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

A native breakpoint's condition reads Python values through `(py)` as well,
so a breakpoint in an extension's C function can stop only for some
objects. A `PyObject *` itself compares as a pointer, as C compares it:

```text
(microscope) break reached if (py)checkpoint == "containers"
```

A condition that cannot be judged, such as comparing a `str` with an `int`,
stops and says why, as any condition does.

### asyncio tasks

From CPython 3.14, which lists every `asyncio.Task` for debuggers,
`tasks` shows each task of every event loop, a task of a class deriving
from `asyncio.Task` among them. A task's number is Python's `id()` of
it, its name is a label, and so are the tasks that await it. A task a
thread runs is running on that thread. One that waits says what it awaits;
one that is ready for its next step is runnable:

```text
(microscope) tasks
* [140402375100848] park at truth.py:582 {name: main} (thread 3128782)
  [140402375101328] Event.wait at asyncio/locks.py:213 — awaits _asyncio.Future {name: chained, awaited_by: awaiting}
  [140402383563792] waiter at tasks.py:56 — awaits task chained {name: awaiting}
  [140402375166064] sleeper at tasks.py:51 {name: fresh}
```

The stack of a task no thread runs is its coroutine's frame and the frame
of each coroutine it awaits, innermost first, as `cr_await` leads. Select
the task with `task ID`, and its frames' locals and expressions read as a
thread's Python frames' do:

```text
(microscope) task 140402375101328
(microscope) bt
#0  in Event.wait at asyncio/locks.py:213
#1  in leaf at tasks.py:41
#2  in middle at tasks.py:47
```

A running task's stack is its thread's. A suspended task cannot be stepped.
Python 3.12 and 3.13 keep no list of their tasks, so they show none.

### Breakpoints

A breakpoint can be in Python code. On CPython 3.14 and later, that works
whether you launch the program or attach to it. On 3.12 and 3.13, it works
when microscope launches the interpreter:

```text
(microscope) break app.py:12
(microscope) break py:Shape.area
(microscope) break py:area if scale > 2
```

A path ending in `.py` names a line. An absolute path names that file, or a
link to it under the same name. A relative path names every file whose path
ends with it, by whole directories: `models.py` and `app/models.py` both match
`/srv/app/models.py`, but `els.py` does not. `py:` names a function by its
qualified name, as Python's `__qualname__` writes it, or by its last parts:
`py:area` matches the method `Shape.area` and a function `area` nested in
another. `py::run` is still a native function in a `py` namespace.

Python code loads when the program imports it, so a Python breakpoint stays
pending until the interpreter has loaded microscope's agent and armed it. Its
pending reason says what it is waiting for. In the browser, a pending
breakpoint is a hollow dot until then, and a filled one once it is armed.
Remote debugging that was turned
off in the interpreter (`PYTHON_DISABLE_REMOTE_DEBUG` or
`-X disable-remote-debug`) is one such reason; stacks and values still work.
Python 3.12 and 3.13 cannot be asked to run the agent. When microscope
launches one of them, it gives the interpreter a `sitecustomize` through
`PYTHONPATH` that takes the agent from microscope. That `sitecustomize`
first removes its directory from `sys.path`, and then imports the
program's own `sitecustomize`, if it has one. With `--follow none`, it also
restores `PYTHONPATH`, so neither the program nor its children see the
change. With the other follow modes, it leaves `PYTHONPATH` and its
`MICROSCOPE_PYTHON_*` variables in the environment, so that the
interpreters the program starts take the agent too. It keeps one idle
thread waiting for microscope, which `threading` does not list. The
bootstrap does not run in these cases:

- an interpreter microscope attached to;
- an interpreter started with `-I`, `-E`, or `-S`, which skip
  `PYTHONPATH` or `site`;
- one started through a wrapper.

There, Python breakpoints stay pending with the reason. CPython 3.13's
free-threaded build stops listing code to its collector once a second
thread starts, so the agent could not find code that is already running.
It takes no Python breakpoints; 3.14t does.

The interpreters a program starts, as `subprocess` and `multiprocessing`
do, and the processes it forks are debugged as the follow policy says (see
[Debug the processes a program starts](debugging.md#debug-the-processes-a-program-starts)).
Python breakpoints work in each one microscope debugs. Python 3.14 and
later load the agent as an attach does. 3.12 and 3.13 take it from the
bootstrap, which reaches a child only when microscope launched a 3.12 or
3.13 interpreter, the follow mode is not `none`, and the child kept the
environment and does not skip it with `-I`, `-E`, or `-S`. Like the
program, a child running CPython 3.13's free-threaded build takes no Python
breakpoints. With `--follow none`, each child runs without
microscope: its breakpoints and microscope's agent are removed from it.

A wrapper that execs Python, such as a pyenv shim or `env python3`, is
followed into the interpreter. Python 3.14 and later then load the agent as
an attach does; 3.12 and 3.13 take it only from a launch of the interpreter
itself, so launch those directly to set Python breakpoints.

A hit selects the program's frame. The backtrace folds the frames inside it
that report the hit: microscope's agent and the interpreter's own. Conditions, hit counts, and log
messages work as they do for native breakpoints, and read the program's
frame. Breakpoints that match the same line or function entry stop once
together, as native breakpoints at one address do. When microscope detaches,
the agent stops monitoring and the program runs as if microscope had never
attached.

### Stepping

From a Python breakpoint, or where a step ended, `next` steps over a line of
the selected frame, `step` into the line's first call, and `finish` out to
the caller. Each stops at a line of Python code and selects its frame:

- `next` over a call or an `await` stops at the frame's next line. An `await`
  that suspends the coroutine stays with it, so the next stop is the same
  task's, even when other tasks run in between.
- `step` stops in the first line of Python code the line calls, or as `next`
  does when it calls none. `step` on an `await` steps over it.
- `step` on a line that calls a C extension's function stops at the
  function's first line, in C, when debug information describes it. Python's
  built-in functions, and C functions without debug information, are stepped
  through, as code that is not the program's.
- From such a C function, `next` past its last line, or `finish`, returns to
  the Python that called it and stops at its next line.
- `finish`, or an editor's step out, stops in the caller, at the call, once
  the frame returns. The
  caller of a task's outermost coroutine is the code that resumed it, often
  asyncio's event loop.
- When an exception leaves the frame, the step stops in the frame that
  catches it, at its handler.
- A step past the thread's outermost frame stops at the next line the thread
  runs.

A Python step starts only where microscope's agent stopped the thread. From
any other stop, such as an interrupt with the thread blocked in C, microscope
refuses the step and says why; set a breakpoint and continue to it instead.
If the thread runs on before the agent can follow it, the step stops
incomplete, with the reason. Native steps in a Python process work as usual.

### Watchpoints

From a stop in a Python frame, `watch name` watches one of the frame's
variables. Each assignment that gives it a different object stops in the
frame, at the line that made it, with the value before and after:

```text
stopped by watchpoint 1 (change, hit 2) on total in count at app.py:4
  old: 0
  new: 1
```

An assignment of the object the variable already holds is no change. The
watch ends when the frame returns or unwinds, and microscope says so. It
works on every supported CPython, without the agent: from a breakpoint or a
step, and also from a pause or a native stop with the frame selected.

Only a variable itself is watched. An expression such as `total.real` or
`items[0]` is refused, because CPython moves and replaces objects as it
likes. So is a variable a closure shares through a cell, and a generator's
or coroutine's variable, which lives in its object rather than its frame.

### Exceptions

Two exception filters stop at Python's exceptions. Both are off until you
choose them, with `catch` or an editor's exception settings:

```text
(microscope) catch python-uncaught on
(microscope) catch python-raised on
```

- `python-uncaught` stops where an exception leaves a thread's outermost
  Python frame, before Python prints its traceback: the main module's code,
  run as a script, with `-c`, or with `-m`; a `threading.Thread`'s `run`; or
  a function another thread started with nothing in Python below it. The
  frames it left have already returned, so the stop says where it was
  raised: `ValueError: boom, raised in parse at /srv/app.py:12`.
  `SystemExit` does not stop, as Python ends the program without a
  traceback. Nor does an exception that leaves other code C called on the
  main thread, which C may catch, as the interpreter does with the import
  errors it meets as it starts, or which Python prints as ignored, as in an
  `atexit` callback.
- `python-raised` stops wherever Python code raises an exception, or an
  exception raised in C reaches Python code, with the raising frame selected.
  It stops once for each raise, not again in each frame the exception passes
  through, and again when a handler raises it anew. It stops at exceptions
  the program catches too, the standard library's among them.

A stop names the exception as Python's traceback would, `KeyError: 'id'`,
and an editor's exception view can inspect the exception itself. The text appears only when Python would print it
without running the program's own code, as for the built-in exceptions with a
message; an exception class that defines `__str__` shows its type alone.

### Profiling

`microscope record --python snapshot` or `--python live` reads each thread's
Python frames, by line, beside the samples, with which thread holds the GIL.
A task snapshot records each waiting asyncio task's coroutine frames. Samples
in the code of CPython 3.15's JIT unwind through it, as stacks do. See
[Record Python programs](profiling.md#record-python-programs).

### System calls

`microscope syscalls -k` shows each call's Python frames among its native
ones, before the evaluation loop that runs them, as a backtrace does:

```text
openat(AT_FDCWD, "settings.toml", O_RDONLY|O_CLOEXEC) = 3
 > /usr/lib/libc.so.6(open+0x56) [0x117536]
 > /usr/lib/libpython3.14.so.1.0(os_open+0x4a7) [0x3b9b87]
 > load at /srv/app/settings.py:42
 > main at /srv/app/server.py:10
 > /usr/lib/libpython3.14.so.1.0(_PyEval_EvalFrameDefault+0x6955) [0x2b1b15]
```

A saved trace keeps them, and the browser groups calls by the Python line that
made them. A catchpoint's stop shows Python frames like any other stop.

The BPF engine reads each call's Python frames in the kernel as the call
returns. It finds the native frames by frame pointers, which glibc's system
call wrappers do not keep, so with `-k` the Python frames follow the first
native frame. Use `-k=dwarf` to place them among the native ones. The engine
learns a program's interpreter a moment after the program starts; calls before
then end with `interpreted frames unknown`.

### Not supported yet

A Python step starts only from where microscope's agent stopped the thread.
An uncaught exception's stop shows the frames it has not yet left, not those
of its traceback. CPU samples do not hold Python frames themselves. [Known limits](reference/limitations.md) lists them all.

In editors, Python frames appear in the call stack with their source file and
line, but no instruction reference. Their scopes are Arguments and Locals.

Native breakpoints, stepping, and everything else in the native code of a Python
process work as usual.

## Node.js

microscope reads the memory of Node's V8 engine and shows its JavaScript
frames in the native backtrace, with their functions, files, lines, and
columns, and their values written as `util.inspect` writes them. Stacks and
values run nothing inside the process. Breakpoints, stepping, and exceptions
go through Node's own inspector, which microscope opens and connects to
itself.

microscope supports Node 22, 24, and 26: the official builds, nixpkgs's,
and Debian's 24, whose shared `libnode` is stripped. A V8 microscope has
not checked is refused by name, and its native frames still appear.

Node works when you launch a program, attach to a process, or open a core
dump:

```sh
microscope node -- app.js
microscope -p PID
microscope --core core node
```

A core dump shows stacks and values; breakpoints and steps need a live
process.

### Variables

A JavaScript frame's variables are in JavaScript's scopes: Arguments, with
`this`; Locals; Closure; Module; and Global. V8 records the names only of
the variables that a closure uses or that live in a context. At a stop
microscope made through the inspector, V8's debugger names the rest, and
microscope keeps each name only where the value it reports is the one the
frame holds. Elsewhere, as at a native breakpoint, those variables appear
as unnamed registers with the reason. A `let` or `const` before its
declaration runs says it is not initialized yet. See
[JavaScript values](expressions.md#javascript-values) for expressions.

### Breakpoints

A breakpoint can be in JavaScript or TypeScript:

```text
(microscope) break app.js:12
(microscope) break src/routes/users.ts:30 if user.id == 7
(microscope) break js:handler
```

A path ending in `.js`, `.mjs`, `.cjs`, `.ts`, `.mts`, `.cts`, `.jsx`, or
`.tsx` names a line, matched as a native breakpoint's path is: a relative
path matches every script whose path ends with it, by whole directories.
An ES module's `file:` URL is matched by its path. `js:` names a function:
`js:handler` stops where each function V8 has compiled under that name
begins, a function declaration, an arrow function held in a `const`, or a
method alike. A dotted name such as `js:Shape.area` is refused; use
`js:area`. V8 compiles a nested function only once its parent first runs,
so microscope looks again as each script loads and each time the program
resumes. `js::run` is still a native function in a `js` namespace.

Conditions, hit counts, and log messages work as they do for native
breakpoints, and read the JavaScript frame. A `debugger` statement stops
too, unless its script is skipped (see [Stepping](#stepping-1)).

A JavaScript breakpoint stays pending until V8 sets it, and its pending
reason says what it waits for. A path that matches no loaded script names
the loaded script nearest it:

```text
no script by that name has loaded yet; did you mean 'routes/users.js'?
```

When microscope launches `node`, it adds a file of its own to
`NODE_OPTIONS` with `--require`, before any user preloads. In a node
microscope traces, that file opens the inspector and waits for microscope
to connect; anywhere else it opens no inspector. It puts `NODE_OPTIONS`
back before the program runs. With a follow mode other than `none`, it
leaves the variables, so that the nodes the program starts open theirs
too: `npm start`, `npx`, and `node --test`
are followed into the `node` that runs your code (see
[Debug the processes a program starts](debugging.md#debug-the-processes-a-program-starts)).
Followed launches keep the preload under `$XDG_CACHE_HOME/microscope/node`
(or `~/.cache/microscope/node`) so descendants can still start after the
debugger exits. An untraced descendant restores the original options.

When microscope attaches to a node, it uses an existing loopback inspector
first. Otherwise, it sends `SIGUSR1` only after proving that Node's native
inspector handler owns the signal. It never invokes an application's
`SIGUSR1` listener. JavaScript breakpoints stay pending, with the reason,
when the inspector cannot open:

- a node started with `--disable-sigusr1`, when attaching: start it with
  `--inspect=127.0.0.1:0` instead;
- a node with an application `SIGUSR1` handler, a blocked `SIGUSR1`, or
  active seccomp filtering;
- a build whose native inspector state microscope cannot prove;
- a node started with `--permission` and without `--allow-inspector`;
- a node whose parent replaced `NODE_OPTIONS`, so the file never loaded.

On detach, microscope closes an inspector it opened and leaves a pre-existing
server open. This also applies when a launched process is released on shutdown.
Closing a server does not evaluate JavaScript or call application getters.
If startup is already under way, detach finishes opening and closing the server
before releasing the process. Cleanup lets the program run to drain startup
callbacks. If it cannot verify closure within five seconds, for example because
the main thread never returns to its event loop, detach reports the failure;
the inspector may remain open.

The inspector listens on 127.0.0.1, on a port the system chooses, under a
random id. microscope reads the id from the process, so Node neither prints
it nor lists it at `/json/list`; another local user who finds the port
cannot connect without it.

### TypeScript and source maps

A script that names a source map, as tsx and esbuild write them, shows its
frames at their places in the original source, `.ts` frames as TypeScript.
A breakpoint on a line of the original source stops at each place the map
leads that line to, set before the script's first statement runs, and a
step moves by the original's lines. Tested with tsx and with esbuild's
CommonJS and ES module bundles.

### Stepping

From a JavaScript stop, `next`, `step`, and `finish` are V8's own steps:
over a statement, into the first call, and out to the caller. A step that
would stop in Node's own scripts (`node:` modules) goes on to the program's
next statement, so `step` into `emitter.emit(...)` stops in the listener.
`[step] skip` names more files to pass through, as globs:

```toml
[step]
skip = ["**/node_modules/**"]
```

`[step] runtime = "enter"` stops in Node's own scripts too. In an editor,
`skipFiles` in the launch configuration does what `[step] skip` does.

From a native stop, such as Ctrl-C on an idle server, `next` runs to the
next statement of JavaScript the program runs, as a timer's or a socket's
callback.

### Interrupts

Ctrl-C, or an editor's Pause, first asks V8 to pause the JavaScript each
thread is running. Running JavaScript pauses within about a millisecond, and
the stop has named locals and can step. A thread that runs no JavaScript,
as an idle event loop, a blocking system call, or a long call into C++,
stops natively, as any program does.

### Workers

Each `worker_threads` worker is debugged on its own thread: its
breakpoints stop that thread, its steps stay on it, and its exceptions stop
there. A new worker waits to run until microscope has set its breakpoints
in it.

### Exceptions

Two exception filters stop at JavaScript's exceptions. Both are off until
you choose them:

```text
(microscope) catch javascript-uncaught on
(microscope) catch javascript-thrown on
```

- `javascript-uncaught` stops where an exception nothing catches is thrown,
  and at a promise rejection nothing handles.
- `javascript-thrown` stops at every exception the program throws and
  every promise it rejects, caught or not.

The stop selects the throwing frame and names the exception as Node would
print it. An exception thrown and caught in a skipped script does not stop.

### Not supported yet

Assigning to a JavaScript variable, watchpoints on JavaScript variables,
and stepping into a Node-API addon's native code are not supported yet.
[Known limits](reference/limitations.md) lists them all.

## Zig, Odin, D, Nim, Ada, and Fortran

These languages use the shared native debugger and expression engine. microscope
normalizes their DWARF types and names where needed and includes built-in views
for common runtime or standard-library values where maintained.

Runtime-specific task scheduling is currently limited to Go, tokio, and
Python's asyncio. Other languages appear as native threads and frames.

## Debug information decides what is available

Language support does not mean every value survives optimization. Compilers
may omit a variable, reuse its storage, describe only part of its lifetime, or
emit no unwind rule for a frame. microscope shows the available part and the
reason for a gap.

See [Known limits](reference/limitations.md) for host and format support, and
[Troubleshooting](reference/troubleshooting.md) when source, symbols, or values
are missing.
