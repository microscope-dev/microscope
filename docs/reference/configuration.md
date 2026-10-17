# Configuration

microscope reads TOML settings from up to three files:

1. `$XDG_CONFIG_HOME/microscope/config.toml`, or
   `~/.config/microscope/config.toml`
2. `.microscope/config.toml` in the project root
3. `.microscope/config.local.toml` in the project root

The local file is intended for settings that should not be committed. Add it
to the project's `.gitignore`.

The project root is the nearest parent containing `.microscope` or `.git`, or
else the current directory. A repository inside another project, such as a git
worktree, is a project of its own.

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

A command-line flag wins over environment settings, followed by the local
project file, project file, user file, and built-in default. Tables merge by
key; lists replace earlier lists.

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
| `[step]` | Whether source steps enter supported runtime code |
| `[backtrace]` | Whether backtraces fold runtime and interpreter frames |
| `[breakpoints]` | Saved-breakpoint behavior |
| `[debug-info]` | Separate debug-file directories and debuginfod |
| `[history]` | REPL history size |
| `[signals]` | Per-signal stop, print, and delivery policy |
| `[aliases]` | Additional command names |
| `[startup]` | Commands run at startup |
| `[projects]` | Project trust policy in the user file |
| `[[source-map]]` | Source-prefix remapping |
| `[[launch]]` | Named launch, attach, and core-dump sessions |

Run `microscope config init` to produce a file containing every setting, its
type, and its default. That generated file is the exact reference for the
installed version.

`[backtrace] runtime` can be `"fold"` (the default) or `"show"`. With `"fold"`,
each run of a language runtime's own frames, such as an interpreter's frames
between Python frames, shows as one line. With `"show"`, backtraces show every
frame. `bt -r` shows every frame for one command, and `set runtime-frames`
changes the setting for a session.

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
