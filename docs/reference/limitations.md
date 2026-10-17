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
- Logical runtime tasks are implemented for Go goroutines and tokio tasks.
- Python frames are read from CPython 3.12 through 3.15, with and without the
  GIL. A CPython release microscope has not checked, or a version outside that
  range, is refused by name; its native frames still appear.
- Python expressions read names, attributes, items, keys, and lengths, and
  compute with numbers, text, and truth values. They never run code in the
  program, so properties, methods, and a class's own `__getattr__`,
  `__getitem__`, `__len__`, and `__repr__` are refused or not used. Python is
  not supported yet for breakpoints by file and line, stepping, Python
  exceptions as catchpoints, asyncio tasks, and Python stacks in profiles.
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
