# Configuration

microscope reads TOML settings from up to three files:

1. `$XDG_CONFIG_HOME/microscope/config.toml`, or
   `~/.config/microscope/config.toml`
2. `.microscope/config.toml` in the project root
3. `.microscope/config.local.toml` in the project root

Keep uncommitted settings in the local file and add it to `.gitignore`.

The project root is the nearest ancestor containing `.microscope` or `.git`,
or the current directory if neither exists. Nested repositories and git
worktrees are separate projects.

## Inspect configuration

```sh
microscope config path
microscope config show
microscope config check
microscope config init
```

`path` prints the project root and files a session would read. `show` prints
effective values and their sources. `check` validates files without starting a
debugger. `init` writes a commented user file and refuses to overwrite one.

Unknown keys, wrong types, and out-of-range values stop startup with the file
and location of the error.

## Precedence

Highest priority first: command-line flags, environment settings, local project
file, project file, user file, built-in defaults. Tables merge by key; lists
replace earlier lists.

Startup command lists run in order instead: user, project, local, selected
launch configuration, `-c` files, then `-e` commands.

## Main tables

| Table | Controls |
|---|---|
| `[ui]` | Color, theme, Unicode, links, paths, pager, editor, prompt, quit confirmation |
| `[theme]` | Styles for names, values, changes, keywords, and other output roles |
| `[source]` | Context lines, highlighting, and tab width |
| `[stop]` | Sections printed at stops and their sizes |
| `[print]` | Compact or pretty output, radix, width, depth, and element limits |
| `[disassembly]` | Intel or AT&T syntax and instruction bytes |
| `[step]` | Runtime stepping and file skip patterns |
| `[backtrace]` | Whether backtraces fold runtime and interpreter frames |
| `[breakpoints]` | Saved-breakpoint behavior |
| `[debug-info]` | Separate debug-file directories and debuginfod |
| `[history]` | REPL history size |
| `[follow]` | Child-process selection and whether processes stop together |
| `[signals]` | Per-signal stop, print, and delivery policy |
| `[aliases]` | Additional command names |
| `[startup]` | Commands run at startup |
| `[projects]` | Project trust policy in the user file |
| `[[source-map]]` | Source-prefix remapping |
| `[[launch]]` | Named launch, attach, and core-dump sessions |

`microscope config init` generates the full reference for your installed
version: every setting, type, and default.

`[backtrace] runtime = "fold"` (the default) collapses consecutive runtime and
interpreter frames into one line. `"show"` displays every frame. Use `bt -r`
for one backtrace or `set runtime-frames` to change the session setting.

## Processes the program starts

`[follow]` selects which child processes to debug after launch or attach. See
[Debug the processes a program starts](../debugging.md#debug-the-processes-a-program-starts).

```toml
[follow]
mode = "auto"
programs = ["worker*"]
stop = "group"
```

| Key | Default | Meaning |
|---|---|---|
| `mode` | `"auto"` | `"auto"`: the original program, Node.js, Python, or Go; `"all"`: every child; `"none"`: only the original process |
| `programs` | `[]` | Additional executable filenames; `*` matches any number of characters, `?` one |
| `stop` | `"group"` | `"group"`: a stop in one process stops every debugged process; `"process"`: only the one that stopped |

`--follow` overrides `mode` when it names a mode, and replaces `programs` when
it names programs. Project `[follow]` settings do not require trust.
`microscope web` reads the same settings but has no `--follow` flag. The editor
adapter uses [`follow`](../editors.md#processes-the-program-starts) in its launch
or attach configuration instead.

## Launch configurations

```toml
[[launch]]
name = "server"
program = "build/server"
args = ["--port", "8080"]
env = { RUST_LOG = "debug" }
cwd = "."
startup = ["break handle_request", "run"]
```

Paths are relative to the project root. Start it with `microscope --launch
server`. When a project has exactly one launch configuration, `microscope` with
no target uses it. Arguments after `--` replace its arguments; command-line
environment and working-directory options override its values.

A launch entry may use `attach` or `core` instead of `program` to describe
another kind of target.

## Trust project actions

Project startup commands, aliases, and launch configurations can execute
actions. Under the default `ask` policy, an interactive session asks before
using them and remembers approval until the files change. A non-interactive
session must use `--trust-project` or previously recorded trust.

```sh
microscope config trust
microscope config untrust
microscope config trusted
```

The user setting `[projects] trust` can be `ask`, `always`, or `never`.
