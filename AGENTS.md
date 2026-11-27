# Agent Instructions

@README.md

We practice Kaizen in our development processes and are always on the lookout to improve. When you find a potential way to improve how we work, flag it to the user for review. Our goal is to maximize total system quality as well as efficiency in our work process.

## Engineering Practices

Always work in the repository’s Nix development environment. Enter it with `./scripts/dev.sh` from the repo root. All tools are pinned via the nix flake.

Use the `justfile` for project tasks. Run `just --list` to see available recipes.

- Keep our systems and code as simple and straightforward as possible, avoiding fancy tricks unless we have specific reason (performance, security, etc.).
- Always ask yourself "what is the best possible architecture for the system and code". We are very open to refactoring our architecture to find global quality maximums.
    - When you find a bug, ask yourself "what is the best possible fix, even if it's a large change" rather than simply putting a local band-aid in place. Present options to the user to choose from if unsure.
- Use all common rust best practices to maintain a high code quality (handle errors rigorously, newtype, make invalid states unrepresentable, etc.).
- Prefer state machines instead of long, complicated logic chains when managing complex logic flows.
- Never add unsafe code without checking with the user. Keep unsafe blocks as small as possible and always test them with miri.
- Never add dependencies without checking with the user.
- When exploring the code, use `rg` to scope down your searches to save tokens.
- We practice poka-yoke wherever possible to ensure our systems are mistake proof.

## Testing

- Always ensure our feedback loops are as fast as possible.
- Use a variety of testing techniques as appropriate for the task at hand and test in-depth (integration tests, e2e tests, user acceptance tests, fuzz tests, property tests, swarm tests, miri, loom, benchmarks, etc.).
- Use unit tests sparingly
- Use `just check` and scope test runs down for fast linter/test runs as you iterate. Then when you reach critical points and want to ensure no regressions, run `just all`, which takes a long time.

## Style Guide

- Keep code comments minimal, high-level, and written in simple, human-like language.
    - Explain *why* things are the way they are (invariants, subtle ordering, kernel/ABI/runtime quirks), and explain *how* only for genuinely complicated logic.
    - Prefer doc comments (`//!`, `///`, `/** ... */`) over inline comments where possible. Avoid trivial comments that restate the code.
    - Never leave load-bearing assumptions solely to comments or documentation. Systems and code must be correct by construction.
- Keep files under 1500 lines (excluding tests) and ensure the code is logically organized so it's easy to consume later.

## Code Map

Rust workspace (`src/`, `crates/microscope-data/`, `crates/microscope-test-strategies/`), plus a TypeScript web UI (`web/`).

- `src/backend/linux/`: ptrace tracer, run control, stepping, breakpoints/watchpoints, frames, memory, core dumps, BPF (`bpf/`), perf (`perf/`), CDP inspector, syscalls
- `src/debug_info/`: ELF, DWARF, gopclntab, CPython, V8, GDB JIT, perf maps, JS source maps
- `src/image/`: indexed, cached per-binary facts (symbols, lines, types, variables, unwind)
- `src/runtime_model/`: language runtimes (`go/`, `python/`, `node/`, `rust/`, `tokio/`, futures)
- `src/eval/`: expression language (parse in `syntax/`, bind, IR, interpret)
- `src/view/` + `views/*.views`: type visualizer DSL and the built-in visualizers
- `src/unwind/`: stack unwinding
- `src/profiler/`: sampling profiler
- `crates/microscope-data/src/`: capture files (`capture/`), columnar object storage (`object/`), profile queries/diffs (`analysis/`), syscall tables (`syscalls/`, `*.gen.rs` are generated), sanitization (`sanitize/`), symbols (`symbols/`), shared view queries (`view/`)
- Frontends: `src/cli/` (REPL), `src/dap/` (Debug Adapter Protocol), `src/web/` (server for `web/`), `src/present/` (shared formatting)
- `src/agent/`: remote machine agent, cloud, inventory, wire protocol
- `src/group.rs`: multi-process groups; `src/model.rs`: core newtypes (`Pid`, `Tid`, ...); `src/protocol.rs`: shared query types
- `src/sim/`: deterministic simulation (`just sim`); `src/tools/` + `src/bin/`: internal tools
- `web/src/` UI, `web/test/` unit tests, `web/e2e/` Playwright
- `tests/`: integration tests by area (`debugger/`, `dap/`, `go/`, `python/`, `node/`, `tokio/`, `web/`, `golden/`); `tests/fixtures/<lang>/` debuggee programs; `tests/support/` harness helpers
- `fuzz/`, `loom/`, `proptest-regressions/`, `bench/`: fuzz targets, loom models, saved proptest cases, benchmarks
- `sdk/`: in-process probe SDKs per language; `editors/`: VS Code and Neovim integrations; `docs/`: user docs (VitePress)
