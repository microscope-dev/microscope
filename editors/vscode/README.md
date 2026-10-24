# microscope for VS Code

Debug native Linux programs written in C, C++, Rust, Go, and Zig, and
Python programs run by CPython and JavaScript and TypeScript programs run by
Node.js, with microscope. The extension contributes the `microscope` debugger type and runs
`microscope dap`; [the editor guide](../../docs/editors.md#vs-code) covers installing
it, its settings and configurations, and what the adapter supports.

`extension.js` is the whole extension, plain JavaScript that VS Code loads
directly. `test/` holds the acceptance test that `just uat-vscode` runs: it
drives the adapter from a real VS Code window and records each session's
traffic. The window opens on an Xvfb display of its own, so it has focus
and no desktop is needed. Name the VS Code executable, and optionally the
scenarios to run:

```sh
VSCODE_EXECUTABLE=/path/to/vscode/code just uat-vscode
MICROSCOPE_UAT_ONLY=nodeAttachedByProcessId,pauseAndThreads \
  VSCODE_EXECUTABLE=/path/to/vscode/code just uat-vscode
```
