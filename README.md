# microscope

microscope is a debugger and profiler for Linux x86-64. It supports C, C++, Rust
Go, Node, Python, Zig, Odin, Fortran, D, Nim, and Ada programs. You can use it in a
terminal, in a browser, or through an editor that supports the Debug Adapter
Protocol.

> microscope is under active development. It does not have a supported
> installation method or stable release yet.

## Documentation

- [Getting started](docs/getting-started.md)
- [Debugging programs](docs/debugging.md)
- [Using the browser](docs/browser.md)
- [Using an editor](docs/editors.md)
- [Recording and reading profiles](docs/profiling.md)
- [Tracing system calls](docs/syscalls.md)
- [Uploading recordings](docs/agent.md)
- [Expressions](docs/expressions.md)
- [Views](docs/views.md) and [writing views](docs/writing-views.md)
- [Supported languages](docs/languages.md)
- [Command-line reference](docs/reference/command-line.md)
- [Configuration reference](docs/reference/configuration.md)
- [Troubleshooting](docs/reference/troubleshooting.md)
- [Known limits](docs/reference/limitations.md)

## What it can do

microscope can launch a program, attach to a running process, or inspect a core
dump. It supports source and instruction stepping, conditional breakpoints,
hardware watchpoints, stack traces, expressions, memory, registers, and
disassembly.

It presents common containers as their contents instead of their internal
fields. It also understands Go goroutines and async Rust tasks. The profiler
can record native and logical stacks and open them in the browser interface.

`microscope syscalls` traces the system calls a program makes, as strace does.
By default it uses eBPF, so the traced program never stops.

When debug information or memory is missing, microscope reports what it could
not read. It does not fill gaps with guessed values.

## Platform support

microscope currently runs on Linux x86-64, version 3.8 or newer. Some features
need a newer kernel: catchpoints and the ptrace syscall engine need 5.3, the
BPF syscall engine 5.15, and the profiler 6.0.

To read thread-local variables in a program that uses glibc, microscope needs
glibc 2.27 or newer. Before glibc 2.34, the program must also have loaded
libpthread.

See [known limits](docs/reference/limitations.md) for details about targets,
debug information, and editor support.

## Development

Development uses the pinned Nix environment:

```sh
just dev
```

Read [AGENTS.md](AGENTS.md) before changing the project. It links to the
detailed [developer documentation](dev/index.md).

## License

Licensed under either [MIT](LICENSE-MIT) or [Apache-2.0](LICENSE-APACHE), at
your option.
