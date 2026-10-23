# Known limits

This page states the broad support boundary. Individual commands can have
narrower limits and report them when used.

## Host and target

- The debugger runs on Linux x86-64, version 3.8 or newer. Catchpoints and
  the ptrace syscall engine need 5.3, `--sysroot` 5.6, and the profiler 6.0.
- It debugs native x86-64 Linux processes and core dumps.
- Thread-local variables are read in programs that use musl, or glibc 2.27
  or newer. glibc before 2.34 describes its thread structures only in
  libpthread, so a program that has not loaded it shows them as unavailable.
- Cross-architecture and remote targets are not supported.
- Process control depends on Linux ptrace permissions.

## Debug information

- Source, types, variables, and unwinding depend on the information emitted by
  the compiler and linker.
- Optimized programs may have missing variables, non-linear source locations,
  inlined frames, and values available for only part of a function.
- Separate debug files and debuginfod are supported when they can be matched
  to a module.
- A partial value or stack stays partial; microscope does not fill missing state
  with assumed values.

## Run control

- Public debugging is all-stop. One thread stopping causes every live thread
  to stop before state is published.
- Hardware watchpoint count, size, and alignment are limited by x86-64 debug
  registers.
- Source stepping is only as precise as the compiler's line and inline-call
  information.
- A program that execs another, as `env`, a shell wrapper, or a version
  manager's shim does, is followed into it, and breakpoints set before the
  run are placed in the new program. Breakpoints set by address stay
  pending there. A program microscope cannot read, such as one executed
  from a memfd or deleted since, stops the process with the reason, and
  later commands that read it are refused.
- The processes a program starts are debugged as the follow policy says
  (see [Debug the processes a program starts](../debugging.md#debug-the-processes-a-program-starts)).
  With `--follow none`, each runs on without microscope, which removes its
  breakpoints, and a Python child's agent, from it first. With the other
  modes, a process the policy does not debug stays traced, so another
  debugger cannot attach to it, but each of its stops is resumed at once.
- A started process that executes a program the policy would debug but
  microscope cannot load runs on without being debugged, with an error
  line, and fails a `--batch` run.
- In a Node.js process, breakpoints are native only; microscope does not set
  breakpoints in JavaScript.
- An attach does not take in the processes the program started before it.
- Core dumps are read-only.
- A core dump that left out a thread's stack, as `gcore` can, shows that
  thread's backtrace as stopped where the stack is missing.

## System calls

- `microscope syscalls` traces x86-64 programs, including their 32-bit and x32
  calls.
- The BPF engine needs Linux 5.15 or newer with BTF, `CAP_BPF`, and
  `CAP_PERFMON`. It cannot use a BPF token.
- The ptrace engine stops the program at each traced call, at a cost of
  several microseconds a call. Attaching it stops every call.
- Calls answered in the vDSO, such as `clock_gettime`, are not traced.
- The BPF engine can drop calls under load and reports how many it lost.
- A cgroup or container trace cannot show stacks.
- The BPF engine's stacks show a Python program's frames only once it has
  read the program's interpreter, a moment after the program starts, and
  only when microscope runs in the initial PID namespace. Until then, and
  otherwise, they end with `interpreted frames unknown`.
- `record --syscalls` cannot be used with `--pid`.
- While a debugger catchpoint exists, every system call stops its thread
  briefly.

## Languages and runtimes

- C, C++, Rust, Go, Zig, Odin, Fortran, D, Nim, and Ada use the shared native
  debugger.
- Logical runtime tasks are implemented for Go goroutines, tokio tasks, and
  asyncio tasks from CPython 3.14, which is the first to list them.
- Python frames are read from CPython 3.12 through 3.15, with and without the
  GIL. A CPython release microscope has not checked, or a version outside that
  range, is refused by name; its native frames still appear.
- Python expressions read names, attributes, items, keys, and lengths, and
  compute with numbers, text, and truth values. They never run code in the
  program, so properties, methods, and a class's own `__getattr__`,
  `__getitem__`, `__len__`, and `__repr__` are refused or not used, and so is a
  key a `defaultdict` holds no value for. A type an extension defines in C,
  other than the standard library's and numpy's arrays that microscope reads,
  shows its type and address. A numpy array of a dtype other than `bool`, an
  integer, `float32`, or `float64`, or of big-endian elements, shows only its
  shape and dtype, and indexing an array is refused. Python breakpoints need
  CPython 3.14 or later with remote debugging allowed, or a 3.12 or 3.13 that
  microscope launched itself, without `-I`, `-E`, or `-S`. Otherwise they stay
  pending. 3.13's free-threaded build takes none. On 3.12 and 3.13, a native
  step does not become a Python step where its frame returns to the
  interpreter. A Python step starts only where microscope's agent stopped
  the thread, at a Python breakpoint or a step's end, and steps by lines;
  a native step leaves C for Python only where its frame returns to the
  interpreter. A step into C stops only in a function debug information
  describes. A Python watchpoint watches a variable of a function's frame,
  not an object an expression reaches, a closure's cell, or a generator's or
  coroutine's variable. An
  uncaught Python exception stops in the outermost frame it leaves, whose
  inner frames have returned; its message says where it was raised. On the
  main thread only the main module's exceptions are uncaught: one leaving an
  `atexit` callback or other code C calls there does not stop.
  Exceptions raised while the agent itself runs, as while it compares a
  breakpoint's keys, do not stop. A Python variable is given a value only
  on CPython 3.14 or later with the GIL, at a stop the agent made, while its
  thread is the interpreter's only one. Only a variable of the frame takes
  one, not a global, attribute, or item, and an object only as another name
  of the frame holds it.
- A profile's Python stacks (`record --python`) come from the BPF engine,
  which needs `CAP_BPF` and `CAP_PERFMON`, or from snapshots that stop the
  program or read it as it runs. The BPF engine's first samples of a program
  or thread may have unknown Python frames, and say so, and it holds at most
  128 Python frames a sample. A live read holds Python frames only, and leaves
  out a thread whose frames change as it reads them. Only CPython 3.15 and
  later say which threads wait for the GIL.
- Code a JIT compiler registers through the GDB JIT interface is read at each
  stop, and by a profile when a sample lands in it, without a breakpoint on
  the compiler's registration. A profile cannot read the code of a program
  that has already exited, so a short program's last samples in it end
  short. At most 64 MiB of registered objects are read at a time.
- Runtime support follows known compiler and runtime layouts. Unknown versions
  can leave tasks or logical frames unavailable.
- Built-in views cover selected standard-library and runtime types, not every
  third-party container.

## Clients

- The terminal, browser, and DAP clients share the same debugger state.
- The repository's VS Code and Neovim paths are tested during development.
- Helix, Zed, and Emacs configurations are documented as DAP clients but do
  not yet have the same acceptance coverage.
- A browser session has no built-in multi-user authorization. Access is
  controlled by its listener, tunnel, or reverse proxy.

## Distribution

microscope does not yet publish a supported package or stable release artifact.
The repository development environment is not an end-user installation path.
