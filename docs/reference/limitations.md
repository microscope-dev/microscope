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
- A program that runs itself again with exec is followed. One that execs a
  different program, as a shell wrapper or a version manager's shim runs
  Python, stops when it does, and microscope does not debug the new program:
  launch that program itself.
- A child the program forks runs on without microscope, which removes its
  breakpoints, and a Python child's agent, from it first.
- Core dumps are read-only.

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
  CPython 3.14 or later with remote debugging allowed. Otherwise they stay
  pending. A Python step starts only where microscope's agent stopped
  the thread, at a Python breakpoint or a step's end, and steps by lines. An
  uncaught Python exception stops in the outermost frame it leaves, whose
  inner frames have returned; its message says where it was raised. On the
  main thread only the main module's exceptions are uncaught: one leaving an
  `atexit` callback or other code C calls there does not stop.
  Exceptions raised while the agent itself runs, as while it compares a
  breakpoint's keys, do not stop.
- A profile's Python stacks (`record --python`) come from the BPF engine,
  which needs `CAP_BPF` and `CAP_PERFMON`, or from snapshots that stop the
  program or read it as it runs. The BPF engine's first samples of a program
  or thread may have unknown Python frames, and say so, and it holds at most
  128 Python frames a sample. A live read holds Python frames only, and leaves
  out a thread whose frames change as it reads them. Only CPython 3.15 and
  later say which threads wait for the GIL.
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
