# Editors

microscope can serve the Debug Adapter Protocol (DAP) over standard input and
output:

```sh
microscope dap
```

It uses the same debugger as the terminal and browser: launch, attach, core
dumps, breakpoints, watchpoints, stepping, expressions, variables, memory, and
disassembly.

## VS Code

The repository contains an extension under `editors/vscode`.

> TODO: publish the extension and replace the development instructions with an
> end-user installation path.

For development setup, see the
[extension README](https://github.com/microscope-dev/microscope/tree/main/editors/vscode).

A launch entry:

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
argument. Python breakpoints, stepping, and exception filters need CPython
3.14 or later, or a launched 3.12 or 3.13; see [Python](languages.md#python).

```json
{
  "type": "microscope",
  "request": "launch",
  "name": "Debug script",
  "program": "/usr/bin/python3",
  "args": ["${file}"]
}
```

To debug a Node.js program, launch `node` with the script as an argument.
JavaScript breakpoints, stepping, and exception filters use Node's inspector.
TypeScript source locations need source maps.

```json
{
  "type": "microscope",
  "request": "launch",
  "name": "Debug Node.js script",
  "program": "/usr/bin/node",
  "args": ["${file}"]
}
```

Use `"request": "attach"` with a process ID to attach. For core dumps, use the
extension's microscope-specific core setting.

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

To debug [child processes](#processes-the-program-starts), replace the adapter
definition with this function. It connects each child session to the original
adapter through `microscope dap --connect`:

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

Set `follow` in the launch or attach configuration to choose which child
processes to debug:

| `follow` | Debugs |
|---|---|
| `"auto"` (default) | Processes running the original program, Node.js, Python, or Go; others are watched until they execute one |
| `"all"` | Every process the program starts |
| `"none"` | Only the program; its children run on their own |
| `{"mode": …, "programs": [...], "stop": "process"}` | The mode's processes plus the named programs; `stop` is `"group"` (default, stop all processes together) or `"process"` (stop each independently) |

Each process has a session nested under its parent's, with the program's
breakpoints installed before its first instruction. With `stop: "group"`,
continuing one continues all. The VS Code extension connects child sessions
automatically; nvim-dap needs the function above. Other clients log a console
message for each debugged process they cannot display.

The adapter does not read `[follow]` from the settings file. Without `follow`,
it uses `"auto"`. Legacy `followForks: true` means `follow: "all"`, and
`false` means `follow: "none"`; do not set both options.

## Behavior to expect

microscope presents native threads through the standard DAP thread and stack
requests. Supported language runtimes can also appear as logical tasks. The
adapter reports optimized-out values and partial stacks as explicit failures or
unavailable values.

The System calls exception filter works like
[`catch syscall`](debugging.md#stop-at-system-calls). Its condition is a
comma-separated list such as `openat,connect`; leave it empty for every call.

DAP clients differ in which protocol features they expose. If a feature is
missing from the editor UI, use its debug console or the terminal client to
check whether the limitation belongs to the client or the adapter.
