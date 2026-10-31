# Assertions

Assertions catch bugs in microscope. They check facts about microscope's own
state that the type system can't express.

## Code map

| Path | What lives there |
| --- | --- |
| `Cargo.toml` | Profiles: where debug assertions and overflow checks are on |
| `src/backend/linux.rs` | `Controller::handle_message`, which every request and wait status passes through, and `Controller::run`, which catches a panic |
| `src/backend/linux/invariants.rs` | The controller's invariants, checked after its messages |
| `src/backend/linux/panic_release.rs` | Releasing an attached process after a panic |
| `src/backend/linux/lifecycle.rs` | `detach_inferior`, the correct way to release an attached process |

## Rules

1. **Types first.** If a type can prove a fact, use the type. Assert what
   types can't prove: facts that span structures, facts that span time, and
   facts given up for speed, such as an index in place of a reference. Assert
   more than feels necessary. If a comment says "always" or "never", assert it.

2. **Bugs, not input.** Never assert on DWARF, ELF, core files, tracee memory,
   ptrace or `/proc` results, or client requests. Return an error instead. This
   includes `slice[i]`, `.expect()`, and arithmetic on values from input: use
   `get` and `checked_*`. Fuzzers run with debug assertions on, so a fuzz crash
   on an assertion means an input check is missing.

   Production code never calls `unwrap`, `expect`, `panic!`, `unreachable!`,
   `todo!`, or `unimplemented!`; Clippy denies them outside tests, fuzz
   harnesses, and the simulator. A fact that should hold but didn't is
   `Error::Internal`, or the module's own internal error, not a panic.

3. **`debug_assert!` by default.** It runs in test, sim, and fuzz builds and
   costs nothing in release, so expensive checks are fine. Use `assert!` only
   when continuing would damage the tracee or a file microscope is writing. Use
   `const _: () = assert!(...)` for sizes and layouts. Release builds keep
   overflow checks on, so an overflow always panics.

4. **No side effects.** Debug assertions vanish in release. A condition must
   not mutate state, do I/O, or touch the tracee.

5. **Say what broke.** Check one fact per assertion. The message names the fact
   and the values involved: thread, `StopId`, IDs, and addresses with their
   address space. Never include tracee memory or strings, because messages end
   up in bug reports. Release builds have no flight recorder, so an `assert!`
   message must make sense on its own.

```rust
debug_assert!(
    site.installed,
    "lifting the trap at {address} for {owner:?}, but it is not installed",
);
```

## JavaScript, TypeScript, and Python

The same rules hold, but where an assertion may fire depends on where the
code runs.

| Code | How it asserts | When it's on |
| --- | --- | --- |
| `web/src` | `assert` from `web/src/assert.ts` | Always. A failure shows in the page and can't harm a tracee |
| `editors/vscode` | `node:assert/strict` | Always |
| `src/runtime_model/python/agent.py` | `assert` | Debug builds only: `agent::bootstrap` compiles it with `optimize=1` in release |
| `sdk/node`, `sdk/python` | the package's `assert`, or `assert not _CHECKED or FACT` | Only with `MICROSCOPE_SDK_CHECKS=1`, which the dev shell sets |

The agent and the packages run inside the program microscope observes, so in a
user's program they must never raise into its code. A test that clears a
fixture's environment sets `MICROSCOPE_SDK_CHECKS` itself. In the web UI,
server messages, the URL, storage, and anything a visualizer sends are input.
Validate them and assert only on the page's own state. Keep checks with
interpolated messages out of hot loops in painting code and the packages.

## Invariant checks

Give a stateful component a `check_invariants` method and call it under debug
assertions after each change. Prove that it fires with a unit test that
corrupts the state and uses `#[should_panic]`.

The controller's invariants run at the end of `Controller::handle_message`, so
the simulator checks them at every step of every seed. Their cost grows with
the state, so past a few hundred breakpoints, threads, and modules they run
every few messages instead. They must hold even when a ptrace request failed
partway through, as a tracee killed from outside makes one do. Record a
breakpoint site's owner and its bookkeeping one site at a time, so that a
failed edit leaves them in step.

## When an assertion fires

The controller thread dies. Pending requests fail with `RequestCancelled`, and
`shutdown` returns `BackendThreadPanicked`. Clients show both as an internal
error. The kernel then releases the tracees:

- A launched process is killed by `PTRACE_O_EXITKILL`. That's fine.
- An attached process is detached with its traps and debug registers still
  set, so it dies with SIGTRAP at the next trap. That's not acceptable.

So `Controller::run` catches the unwind and releases an attached process as
`detach_inferior` does. The cleanup lives at the thread boundary, not in an
assertion macro, because any panic can end the controller. It trusts the kernel
over the controller's state: a thread is stopped if it answers a register
read, and a byte is restored only if memory still holds microscope's trap and
the original byte is known. Aborts skip it: stack overflow, running out of
memory, and a panic during cleanup.

In development builds, `MICROSCOPE_TEST_PANIC_AFTER=<request>` makes the
controller panic after serving a request whose description begins with
`<request>`. `tests/cli.rs` uses it to check that an attached process survives.

The waiter thread asserts nothing. If it ends, the controller hears no more
wait statuses and stalls.
