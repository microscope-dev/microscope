# Getting started

Compile a program with debug information, then launch it in microscope.

## Install microscope

> TODO: microscope does not have a supported installation method or
> stable release artifact yet. Add installation instructions after the project
> publishes one.

The repository contains a pinned environment for contributors. It is not an
end-user installation method.

## Build a program for debugging

Start with debug information and no optimization. Optimized builds also work,
but variables and control flow may no longer match the source:

```sh
cc -g -O0 -o hello hello.c
```

microscope reads DWARF from the executable or a separate debug file. Without
it, you can inspect addresses and instructions, but source lines, types, and
local variables may be missing.

## Start a terminal session

Launch a program by passing its executable and arguments:

```sh
microscope ./hello -- first second
```

A name without a directory, such as `python3`, is resolved in the current
directory first, then on `PATH`. microscope preserves that path, so `python`
from an activated virtual environment runs in that environment.

microscope loads the program and opens a prompt:

```text
(microscope)
```

Set a breakpoint and run:

```text
break main
run
```

At a stop, these commands cover the basic workflow:

```text
list                show source around the current line
bt                  show the stack
locals              show local variables
print name          evaluate an expression
next                step over a source line
step                step into a call
continue            run until the next stop
help                list commands
quit                end the session
```

See [Debugging](debugging.md) for launch, attach, core-dump, breakpoint, and
stepping workflows. The [command-line reference](reference/command-line.md)
lists exact invocation forms and REPL commands.

## Use the browser or an editor

Start the browser interface with:

```sh
microscope web ./hello
```

See [Browser](browser.md) for navigation and shared sessions.

For an editor, see [DAP client setup](editors.md).
