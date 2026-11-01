---
layout: home

hero:
  name: microscope
  text: Debug and profile native programs
  tagline: A Linux debugger for the terminal, browser, and your editor.
  actions:
    - theme: brand
      text: Run a first session
      link: /getting-started
    - theme: alt
      text: Debug a program
      link: /debugging

features:
  - title: Inspect program state
    details: Read stacks, values, memory, registers, and instructions. Missing values and incomplete stacks are reported.
  - title: Follow runtime tasks
    details: Inspect Go goroutines and async Rust tasks, including suspended stacks and wait locations.
  - title: Record profiles
    details: Sample native programs externally. Measure CPU time, blocked time, tasks, and instrumented work.
  - title: Trace system calls
    details: Trace calls in strace format, or count them in the kernel without stopping the program.
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
