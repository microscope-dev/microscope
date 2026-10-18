# microscope for VS Code

Debug native Linux programs written in C, C++, Rust, Go, and Zig, and
Python programs run by CPython, with microscope. The extension contributes the `microscope` debugger type and runs
`microscope dap`; [the editor guide](../../docs/editors.md#vs-code) covers installing
it, its settings and configurations, and what the adapter supports.

`extension.js` is the whole extension, plain JavaScript that VS Code loads
directly. `test/` holds the acceptance test that `just uat-vscode` runs: it
drives the adapter from a real VS Code window and records each session's
traffic.
