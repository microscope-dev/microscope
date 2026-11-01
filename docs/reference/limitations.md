# Known limits

These are the general limits. Commands report additional restrictions when
they apply.

## Host and target

- The debugger runs on Linux x86-64, version 3.8 or newer. Catchpoints and
  the ptrace syscall engine need 5.3, `--sysroot` 5.6, and the profiler 6.0.
- It debugs native x86-64 Linux processes and core dumps.
- Thread-local variables require musl or glibc 2.27 or newer. Before glibc
  2.34, the program must also have loaded libpthread's thread metadata.
- Cross-architecture and remote targets are not supported.
- Process control depends on Linux ptrace permissions.

## Debug information

- Source, types, variables, and unwinding depend on the information emitted by
  the compiler and linker.
- Optimized programs may have missing variables, non-linear source locations,
  inlined frames, and values available for only part of a function.
- Separate debug files and debuginfod are supported when they can be matched
  to a module.
- Missing values and incomplete stacks are reported, not filled in.

## Run control

- Public debugging is all-stop. One thread stopping causes every live thread
  to stop before state is published.
- Hardware watchpoint count, size, and alignment are limited by x86-64 debug
  registers.
- Source stepping is only as precise as the compiler's line and inline-call
  information.
- Exec preserves debugging and reinstalls breakpoints in the new program;
  address breakpoints stay pending. If the executable cannot be read (for
  example, a memfd or deleted file), the process stops with an error and
  commands that need the executable are rejected.
- The [follow policy](../debugging.md#debug-the-processes-a-program-starts)
  controls child processes. `--follow none` releases them after removing
  breakpoints and any Python agent. Other modes keep non-selected processes
  traced but resume their stops immediately; another debugger cannot attach.
- A child that execs a selected but unreadable program runs without debugging,
  reports an error, and causes `--batch` to fail.
- Attach follows only future children, not existing ones.
- Core dumps are read-only.
- A backtrace ends at any stack memory omitted from the dump, as can happen
  with `gcore`.

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
- BPF Python stacks require the initial PID namespace and interpreter metadata,
  which takes a few milliseconds to read after the interpreter is mapped.
  Otherwise they end with `interpreted frames unknown`.
- `record --syscalls` cannot be used with `--pid`.
- While a debugger catchpoint exists, every system call stops its thread
  briefly.

## Languages and runtimes

- C, C++, Rust, Go, Zig, Odin, Fortran, D, Nim, and Ada use the shared native
  debugger.
- Logical tasks are supported for Go goroutines, tokio tasks, and asyncio
  tasks on CPython 3.14 or newer.
- Runtime support follows known compiler and runtime layouts. Unknown versions
  can leave tasks or logical frames unavailable.
- Built-in views cover selected standard-library and runtime types, not every
  third-party container.

### Python

CPython 3.12 through 3.15 is supported, with or without the GIL. Unverified or
out-of-range releases are rejected with their version; native frames remain
available.

| Operation | Limits |
| --- | --- |
| Expressions | Read names, attributes, items, keys, and lengths; compute with numbers, text, and booleans. Never call properties, methods, `__getattr__`, `__getitem__`, `__len__`, or `__repr__`. Missing `defaultdict` keys are rejected. |
| C-extension objects | Show type and address, except supported standard-library types and NumPy arrays. |
| NumPy | Read `bool`, integer, `float32`, and `float64` arrays with native-endian elements. Other arrays show shape and dtype only; indexing is rejected. |
| Breakpoints | Require 3.14+ with remote debugging allowed, or a directly launched 3.12/3.13 without `-I`, `-E`, or `-S`. Unsupported breakpoints stay pending. Free-threaded 3.13 has none. |
| Stepping | Python line steps start at agent stops (Python breakpoints or completed steps). Native steps can return from C to Python on 3.14+, not 3.12/3.13. Stepping into C requires debug information. |
| Watchpoints | Watch function-frame variables, not attributes, items, closure cells, or generator/coroutine variables. |
| Assignment | Requires 3.14+, the GIL, an agent stop, and a single-threaded interpreter. Assign frame locals only, not globals, attributes, or items; object values must already be held by another frame local. |

Uncaught exceptions stop in the outermost frame they leave, after inner frames
have returned; the message includes the raise location. On the main thread,
only exceptions leaving the main module qualify, not `atexit` callbacks or
other C-invoked code. Exceptions raised inside microscope's agent do not stop.

### Node.js

Node 22, 24, and 26 are supported: official, nixpkgs, and Debian builds.
Breakpoints, stepping, and exception stops need the inspector. They stay pending
with a reason when attaching to Node with `--disable-sigusr1`, when
`--permission` lacks `--allow-inspector`, or when a parent replaces
`NODE_OPTIONS`.

| Operation | Limits |
| --- | --- |
| Function breakpoints | Use the function's own name, not a dotted name. |
| Variable names | Variables V8 leaves unnamed appear as registers, except at inspector stops. |
| Assignment | Requires an inspector stop and a variable V8 can assign. Excludes `this`, properties, globals, unnamed registers, and optimized variables outside a context. The paused statement may still use the old value. |
| Watchpoints | Watch interpreter/Sparkplug frame variables or captured context variables. Excludes properties, `this`, globals, script-level bindings, uncaptured block locals, generator/async variables outside a context, and optimized variables. Node 26 context cells are unsupported, including contexts with at most two variables. |
| Node-API stepping | Requires an inspector stop and addon debug information. Unavailable on Debian's Node, whose `libnode` lacks the V8 entry symbol. |
| Async callers | Chains start at async functions resumed after `await`, not `.then` handlers. |
| Core dumps | Read-only. |

A frame-slot watch ends when V8 starts replacing that frame with optimized
code, even if replacement fails. Debian's `libnode` lacks V8 collector symbols,
so JavaScript watchpoints are unavailable there.

### Python profiles and JIT code

`record --python` uses BPF (`CAP_BPF` and `CAP_PERFMON`) or snapshots taken
with the program stopped or running. BPF records at most 128 Python frames per
sample; early samples may report unknown frames. Live snapshots contain only
Python frames and omit threads whose frames change during the read. Only
CPython 3.15+ reports threads waiting for the GIL.

microscope reads GDB JIT registrations at each stop and when a profile sample
lands in registered code, without setting a registration breakpoint. Reads
are limited to 64 MiB of registered objects at a time. Code is unavailable
after process exit, so final JIT samples from short-lived programs may have
incomplete stacks.

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
