# Editors

microscope can serve the Debug Adapter Protocol (DAP) over standard input and
output:

```sh
microscope dap
```

The adapter uses the same debugger as the terminal and browser clients. It can
launch, attach, inspect core dumps, set breakpoints and watchpoints, step,
evaluate expressions, inspect variables and memory, and show disassembly.

## VS Code

The repository contains an extension under `editors/vscode`.

> TODO: publish the extension and replace the development instructions with an
> end-user installation path.

For repository development, follow the extension's
[README on
GitHub](https://github.com/microscope-dev/microscope/tree/main/editors/vscode).

A launch entry has this shape:

```json
{
  "type": "microscope",
  "request": "launch",
  "name": "Debug program",
  "program": "${workspaceFolder}/build/program",
  "args": ["--verbose"]
}
```

To debug a Python program, launch its interpreter with the script as an
argument; breakpoints in `.py` files, steps, and the Python exception filters
need CPython 3.14 or later, or a launched 3.12 or 3.13 (see
[Python](languages.md#python)):

```json
{
  "type": "microscope",
  "request": "launch",
  "name": "Debug script",
  "program": "/usr/bin/python3",
  "args": ["${file}"]
}
```

Use `"request": "attach"` with a process ID to attach. Core-dump sessions use
the microscope-specific core setting described by the extension when installed.

## Neovim

The repository contains a tested `nvim-dap` setup in `editors/nvim/uat.lua`.
A minimal adapter definition starts `microscope dap` as an executable adapter:

```lua
local dap = require("dap")

dap.adapters.microscope = {
  type = "executable",
  command = "microscope",
  args = { "dap" },
}
```

Create launch and attach configurations with `type = "microscope"` and the usual
DAP `request`, `program`, `args`, and `cwd` fields.

## Other DAP clients

Helix, Zed, and Emacs dape can start an executable DAP adapter. Configure the
adapter command as `microscope dap` and use the standard launch or attach
request.

> TODO: add tested, versioned setup examples for Helix, Zed, and Emacs after
> they are part of the client acceptance suite.

## Behavior to expect

microscope presents native threads through the standard DAP thread and stack
requests. Supported language runtimes can also appear as logical tasks. The
adapter reports optimized-out values and partial stacks as explicit failures or
unavailable values.

The adapter's exception filters include **System calls**, which stops at
system calls as [`catch syscall`](debugging.md#stop-at-system-calls) does. Its
condition is a comma-separated list of calls, such as `openat,connect`; leave
it empty to stop at every call.

DAP clients differ in which protocol features they expose. If a feature is
missing from the editor UI, use its debug console or the terminal client to
check whether the limitation belongs to the client or the adapter.
