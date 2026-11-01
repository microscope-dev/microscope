---
layout: home

hero:
  name: microscope
  text: Debug and profile native programs
  tagline: A Linux debugger for the terminal, browser, and your editor.
  actions:
    - theme: brand
      text: Get started
      link: /getting-started
    - theme: alt
      text: Debug a program
      link: /debugging

features:
  - title: See the program as it is
    details: Inspect stacks, values, memory, registers, and instructions. Missing information stays visible instead of becoming a guessed value.
  - title: Work with language runtimes
    details: Follow Go goroutines and async Rust tasks as logical stacks, including the places where they wait.
  - title: Record profiles
    details: Sample native programs from outside the process and explore CPU time, blocked time, tasks, and instrumented work.
  - title: Trace system calls
    details: See each call a program makes, as strace prints it, or count them in the kernel without stopping the program.
---

microscope runs on Linux x86-64 and understands programs written in C, C++,
Rust, Go, Zig, Odin, Fortran, D, Nim, and Ada.

## Start here

- [Install microscope and run a first session](getting-started.md)
- [Learn the debugging workflow](debugging.md)
- [Use the browser interface](browser.md)
- [Connect an editor](editors.md)
- [Record and inspect a profile](profiling.md)
- [Trace system calls](syscalls.md)

## Learn the data model

- [Write expressions](expressions.md)
- [Understand and write views](views.md)
- [Write visualizers](visualizers.md)
- [Check language support](languages.md)

Exact options and settings are in the [reference](reference/command-line.md).
