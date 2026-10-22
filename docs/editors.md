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

The processes the program starts are debugged with it, each in a session of its
own that the program's adapter serves (see [`follow`](#processes-the-program-starts)).
nvim-dap starts that session through the adapter, and can start it only as a
program it runs. So define the adapter as a function that runs
`microscope dap --connect` with the socket a member's configuration names, which
relays the session to the program's adapter:

```lua
dap.adapters.microscope = function(callback, config)
  local member = config.__microscopeMember
  if member then
    callback({ type = "executable", command = "microscope", args = { "dap", "--connect", member.socket } })
  else
    callback({ type = "executable", command = "microscope", args = { "dap" } })
  end
end
```

## Other DAP clients

Helix, Zed, and Emacs dape can start an executable DAP adapter. Configure the
adapter command as `microscope dap` and use the standard launch or attach
request.

> TODO: add tested, versioned setup examples for Helix, Zed, and Emacs after
> they are part of the client acceptance suite.

## Processes the program starts

A launch or attach configuration's `follow` says which processes the program
starts are debugged too, as the command line's `--follow` and the settings
file's `[follow]` table do:

| `follow` | Debugs |
|---|---|
| `"auto"` (default) | Processes that run the program, or a runtime microscope models such as Node.js or Python; others are watched until they execute one |
| `"all"` | Every process the program starts |
| `"none"` | Only the program; its children run on their own |
| `{"mode": …, "programs": [...], "stop": "process"}` | As the mode says, and the named programs too; `stop` is `"group"` (default), which stops every process when one stops, or `"process"` |

Each debugged process appears as a session of its own, nested under the
session of the process that started it, and has the program's breakpoints
before it runs an instruction. Continuing any of them continues them all.
microscope's VS Code extension connects these sessions to the program's
adapter itself; nvim-dap needs the adapter function above. A client that cannot
start them is told which processes are debugged without a session to show
them. The older `followForks: true` means `follow: "all"`.

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
