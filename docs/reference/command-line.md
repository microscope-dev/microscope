# Command-line reference

## Invocation

```text
microscope [OPTIONS] EXECUTABLE [-- ARGS...]
microscope [OPTIONS] --attach PID|NAME [EXECUTABLE]
microscope [OPTIONS] --core CORE [EXECUTABLE]
microscope [OPTIONS] --launch NAME [-- ARGS...]
microscope TOOL
```

Use `microscope --help` for the complete current option list and `microscope
TOOL --help` for one tool.

## Tools

| Tool | Purpose |
|---|---|
| `dap` | Serve the Debug Adapter Protocol |
| `web` | Serve a debugging or profiling session to browsers |
| `views` | Check views, explain binding, or replay recorded kernels |
| `config` | Inspect, check, create, and trust configuration |
| `record` | Record a profile |
| `syscalls` | Trace system calls, as strace does; see [System calls](../syscalls.md) |
| `report` | Print a profile summary or call tree |
| `convert` | Convert microscope, pprof, folded-stack, and Chrome-trace formats |
| `symbolize` | Add source lines to a profile recorded for later symbolization |
| `agent` | Upload recordings to a microscope service; see [Upload recordings](../agent.md) |

## Target options

| Option | Meaning |
|---|---|
| `-p, --attach PID\|NAME` | Attach by process ID or exact process name |
| `--core FILE` | Open a core dump |
| `-l, --launch NAME` | Use a project launch configuration |
| `--cwd DIR` | Working directory for a launched program |
| `--env NAME=VALUE` | Add an environment value; repeatable |
| `--sysroot DIR` | Root used to find files named by a core dump |
| `--module-path DIR` | Search for missing core-dump modules; repeatable |
| `--allow-module-mismatch` | Accept a module that cannot be proven to match the dump |

## Debug-information options

| Option | Meaning |
|---|---|
| `--source-map FROM TO` | Remap a recorded source prefix; repeatable |
| `--debug-directory DIR` | Search for separate debug files; repeatable |
| `--debuginfod` | Use servers listed in `DEBUGINFOD_URLS` |
| `--no-cache` | Read debug information without the image cache |
| `--views FILE` | Load a view file ahead of discovered and built-in views |

## Startup and output options

| Option | Meaning |
|---|---|
| `-c, --command FILE` | Run commands from a file; repeatable |
| `-e, --eval COMMAND` | Run one command; repeatable |
| `--batch` | Exit after commands instead of opening the REPL |
| `--config FILE` | Use this user configuration file |
| `--no-config` | Read no configuration files |
| `--trust-project` | Use active project commands for this session |
| `--color auto\|always\|never` | Control terminal color |
| `--disassembly-syntax intel\|att` | Choose assembly syntax |
| `--timings FILE` | Write a JSON timing and allocation report |

## Web server options

These options apply to `microscope web`.

| Option | Meaning |
|---|---|
| `--port PORT` | Listen on `127.0.0.1:PORT` instead of the default port |
| `--listen ADDRESS` | Listen on `ADDRESS`; anyone who can reach it can use the session |
| `--listen-unix PATH` | Listen on a Unix socket at `PATH`, which must not exist; requires `--public-url` |
| `--public-url URL` | Serve pages through a reverse proxy at `URL`; the proxy controls access |
| `--trust-proxy` | Give every request the proxy sends read-only access to a profile; requires `--public-url` and serves only profiles |

## Agent options

These options apply to `microscope agent`. `microscope agent login --server
URL` saves a device token, read from standard input, for the service at
`URL`.

| Option | Meaning |
|---|---|
| `--server URL` | Connect to this service instead of the one `agent.toml` names |
| `--watch DIR` | Also upload each segment `record --continuous DIR` finishes; repeatable |
| `--allow-remote` | Run the commands the service sends, such as recording one of your processes |

`microscope record --upload` also queues the profile for the agent, and
prints its link once the agent has uploaded it.

## REPL commands

Commands accept unique prefixes. `help COMMAND` prints the current syntax for
one command.

### Run control

| Command | Purpose |
|---|---|
| `run` | Start or restart a launched program |
| `continue` | Resume until the next stop |
| `step` | Step into the next source line or named call |
| `next` | Step over calls on the next source line |
| `finish` | Run until the selected frame returns |
| `stepi`, `nexti` | Step one machine instruction |
| `advance LOCATION` | Run to a location in the selected frame |
| `jump LOCATION` | Move the selected thread's next instruction without running |
| `pause` | Stop a running program |
| `kill` | Kill the process |
| `detach` | Detach from a process and leave it running |
| `quit` | End the session |

### Breakpoints and watchpoints

| Command | Purpose |
|---|---|
| `break LOCATION [if EXPR]` | Create a breakpoint |
| `tbreak LOCATION` | Create a breakpoint removed after one hit |
| `rbreak REGEX` | Break on functions whose names match |
| `watch EXPR [if EXPR]` | Stop when the addressed bytes change |
| `breakpoints` | List breakpoints and watchpoints |
| `condition ID EXPR` | Set or clear a condition |
| `ignore ID COUNT` | Ignore the next hits |
| `enable ID...`, `disable ID...` | Change whether sites stop |
| `delete ID...` | Remove sites |
| `catch syscall [entry\|exit] [CALL...] [if EXPR]` | Stop at system calls |
| `info catchpoints` | List catchpoints |

IDs can include breakpoint ranges, watchpoint IDs such as `w2`, catchpoint
IDs such as `c1`, or `all` where the command permits it.

### Stack, threads, and tasks

| Command | Purpose |
|---|---|
| `bt [-r]` | Show the selected stack; `-r` shows the runtime and interpreter frames it folds |
| `frame N`, `up`, `down` | Select a frame |
| `threads`, `thread ID` | List or select native threads |
| `goroutines`, `goroutine ID` | List or select Go goroutines |
| `tasks`, `task ID` | List or select runtime tasks |
| `print`, `pp` | With no expression, show the selected frame's variables |

### Values and machine state

| Command | Purpose |
|---|---|
| `print EXPR`, `p EXPR` | Evaluate and print a value |
| `pp EXPR` | Print a value in expanded form |
| `display EXPR` | Print an expression at every stop |
| `x ...` | Examine memory |
| `registers` | Show registers for the selected frame |
| `disassemble` | Show instructions around a location or function |
| `info symbol ADDRESS` | Name the symbol containing an address |
| `list` | Show source around the selected location |

`print` and `pp` accept format suffixes such as `/x` for hexadecimal. Use
`help print` for the complete set.

### Session settings

| Command | Purpose |
|---|---|
| `set NAME VALUE` | Change a session setting |
| `show NAME` | Show a session setting |
| `set runtime-frames on\|off` | Show a runtime's and an interpreter's own frames in backtraces, or fold them |
| `handle SIGNAL ...` | Set stop, print, and delivery policy |
| `signals` | List signal policy |
| `source FILE` | Run commands from a file |
| `help [COMMAND]` | Show help |

## Location forms

Commands that accept a location can use a function name, `file:line`, a bare
line in the selected file where supported, or `*0xADDRESS`. Function lookup
uses demangled names and reports ambiguity instead of choosing silently.

## Command files

A command file contains one REPL command per line. Blank lines and lines whose
first non-space character is `#` are ignored. Startup order is user, project,
and local configuration commands, then `-c` files, then `-e` commands.
