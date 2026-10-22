# Web UI

`microscope web` serves the React application in `web/` and connects every tab
to one debugger session over WebSocket. The server is a `DebuggerHandle` client,
like the CLI and DAP server.

## Code map

| Path | What lives there |
| --- | --- |
| `src/web/mod.rs`, `src/web/session.rs`, `src/web/connection.rs` | HTTP/WebSocket server and debugger-session ownership |
| `src/web/protocol.rs` | Rust protocol types and TypeScript generation source |
| `src/web/group.rs`, `web/src/ui/Processes.tsx` | The selected member of a process group and the process switcher |
| `src/web/assets.rs`, `src/web/auth.rs` | Built-page serving and access control |
| `src/web/inspect.rs`, `src/web/values.rs`, `src/web/draw.rs`, `src/web/profiles.rs` | Server adapters for major page features |
| `src/web/syscalls.rs`, `web/src/profile/syscalls.ts`, `web/src/ui/profile/SyscallsView.tsx` | A profile's Syscalls view; see [system call tracing](syscalls.md) |
| `web/src/store.ts`, `web/src/connection.ts`, `web/src/actions.ts` | Browser state, transport, and user actions |
| `web/src/router.tsx`, `web/src/tab.ts`, `web/src/ui/` | URL state, per-tab view, and React interface |
| `web/src/protocol.gen.ts` | Generated TypeScript protocol; do not edit by hand |
| `web/src/visualize/`, `sdk/web/` | Sandboxed visualizers and their public type contract |
| `web/test/` | Fast TypeScript, React, replay, and sandbox tests |
| `web/e2e/`, `tests/web/` | Browser end-to-end tests and Rust server tests |
| `web/vite.config.ts`, `web/playwright.config.ts` | Browser build and end-to-end configuration |

## Protocol and state

The server sends tab state and notices. A tab sends requests that include the
`StopId` of the state it read. Cache read answers by stop and request. Forget
them after writes, setting changes, and session changes.

The address bar contains the tab's complete view: stop, thread or task, frame,
source, panel, and memory location. A shared link must open the same view.

Formatting shared with other clients belongs in `src/present`.

`web/src/protocol.gen.ts` is generated from `src/web/protocol.rs` with ts-rs.
A test detects stale output. Regenerate it with:

```sh
MICROSCOPE_UPDATE_PROTOCOL=1 just test protocol
```

## Building and testing

Debug builds read the page from `build/web` at runtime. Release builds embed
it.

```sh
just web
just web-test
just web-e2e
just web-e2e --project=chromium -g NAME
```

`just web-test` runs TypeScript, Biome, and Vitest. End-to-end tests run real
servers in Chromium and Firefox.

Replay tests use recorded server transcripts. Update them with `just
web-transcripts` when the protocol change is intentional.

To inspect the page, run:

```sh
just web-probe PROGRAM STEPS...
```

The probe saves a screenshot after each step and prints browser-console
messages. `web/e2e/probe.ts` lists the available steps. `just web-dev` starts
the live-reload server.

Use binaries in `web/node_modules/.bin` or the Nix shell's `biome`. Do not use
`npx`, which can download a different package when a command is missing.

## Server boundary

`src/web` adapts `DebuggerHandle` to WebSocket messages. It owns authentication
tokens, session joining, per-connection state, protocol conversion, source and
profile responses, and static assets. It does not own debugger semantics.

A connection receives session events and sends requests tagged with the state
the tab observed. The server validates inputs before calling the handle and
returns notices or typed answers the page can render.

Keep blocking file work and profile loading away from the controller thread.
Backpressure every stream that can be driven by a browser. A disconnected tab
must release its subscriptions, pending replies, and drawing work.

## Process groups

A launched or attached program is a `ProcessGroup` that follows as the
settings' `[follow]` says (`src/web/mod.rs`, `src/web/session.rs`).
`src/web/group.rs` keeps the session's selected member: the state every tab
receives describes it, and every request about the program acts on its
handle, except breakpoints, which are the group's and go to the root. The
selection belongs to the session, not to a tab. A group stop selects the
member that stopped. `State::group` carries the members, the selected one,
and the one whose stop stopped the group. `selectMember` and `detachMember`
change them.

`web/src/ui/Processes.tsx` draws the switcher above the stop banner once the
group has a second member, with a Detach button for a stopped member other
than the root.

## Generated protocol

Rust records in `src/web/protocol.rs` are the protocol source. ts-rs generates
`web/src/protocol.gen.ts`. Handwritten TypeScript wrappers in
`web/src/protocol.ts` may add client helpers but must not duplicate wire
records.

After a Rust protocol change, run the generator through the test path and
inspect both sides:

```sh
MICROSCOPE_UPDATE_PROTOCOL=1 just test protocol
git diff -- web/src/protocol.gen.ts
```

Then run the ordinary test without the update variable. A stale generated file
must fail CI.

Protocol changes need an explicit compatibility decision. Development server
and page builds normally move together, but replay transcripts and an old open
tab can expose ordering and missing-field problems. Prefer additive fields
with clear defaults when that costs little.

## Client state

`web/src/store.ts` owns shared application state. `connection.ts` owns the
transport lifecycle. `model.ts` and `data.ts` normalize wire data. UI
components select state and issue actions; they should not maintain private
copies of debugger truth.

State has several lifetimes:

- session: target, modules, settings, loaded views;
- execution: running operation and pending control state;
- stop: threads, tasks, frames, variables, memory, and disassembly answers;
- tab: selected context, panel, paths, expanded values, and pinned drawings;
- component: transient focus, hover, and layout state.

Put data in the narrowest lifetime that owns it. On a new stop, forget every
cached answer tied to the previous `StopId`. On session replacement, also
forget module, view, and target state. A settings or memory write can require
more selective invalidation within one stop.

## URL state

`router.tsx`, `tab.ts`, and UI path helpers encode a tab's complete navigable
view. A copied URL should restore the same stop where retained, execution
context, frame, source, panel, memory address, and pinned values.

URL parsing is an untrusted boundary. Reject or normalize invalid IDs, paths,
indexes, and duplicate fields. Do not issue debugger writes merely by opening a
URL.

When adding durable view state, update parsing, serialization, back/forward
behavior, sharing tests, and unknown-value fallback. Ephemeral state such as a
hover or open menu should stay out of the URL.

## Actions and focus

`actions.ts`, `keys.ts`, `palette.ts`, and `focus.ts` form one command system.
Buttons, keyboard shortcuts, and the command palette should invoke the same
action rather than duplicate control logic.

An action declares when it is available from current state. Disabled actions
should explain the missing condition where the UI has room. Keyboard handling
must respect text inputs, dialogs, renderer canvases, and platform modifier
conventions.

Focus is part of accessibility and debugger speed. After a stop or panel
change, move focus only when the user's current focus is no longer meaningful.
Do not steal it from the console or an expression editor on every event.

## Values and large data

Value trees request children by page and retain stable expansion paths. Never
load an entire large sequence to render its first screen. Virtualize long
tables and lists, and keep exact integer text separate from rounded chart
labels.

Every request uses the value reference and stop that produced the row. A stale
reply must not replace data from a newer stop. Cancellation saves browser work
but does not replace server-side stop validation.

Memory, source, and disassembly views follow the same rule: cache by complete
request key and stop, then discard on invalidation.

## Visualizer sandbox

Renderer JavaScript is program-provided input. It runs in a worker sandbox
with a narrow `microscope` drawing API. It receives only inputs evaluated by the
debugger and cannot fetch arbitrary memory or access page state.

The host enforces time, concurrency, shape, byte, image, and message limits.
It validates every returned picture before drawing. A renderer that throws,
times out, loses its WebGL context, or returns invalid data fails its card and
does not break the workspace.

Live renderers receive bounded pointer and key events. The debugger's function
keys remain reserved, and Escape returns focus. Stop worker and graphics
resources when a card leaves its lifetime.

`sdk/web/microscope-visualizer.d.ts` is the public renderer API. Type-check
built-in renderers against it and update public visualizer docs with any API
change.

## Profiles in the browser

Profile pages use typed server queries over validated captures. Flame trees,
functions, timelines, zones, source costs, and diffs should consume
`src/analysis` results. Do not reimplement aggregation in TypeScript.

Large profile views use virtual rows, bounded plot bins, and workers where
decoding or drawing would block input. Keep query state in the URL when it is
part of a shareable investigation.

## Replay transcripts

Replay tests feed recorded server traffic to the real client. They are useful
for stop sequences, multiple tabs, values, profiles, and low-level state
without a native process.

Transcripts are generated artifacts from Rust web tests. Do not edit JSONL by
hand. Rerecord with `just web-transcripts`, inspect the semantic changes, then
run `just web-test`.

A transcript should be small enough to explain its scenario. Add a new one
only when existing traffic cannot represent the behavior.

## End-to-end tests and probes

Playwright tests own user-visible browser/server workflows. Use semantic
locators and observable state, not sleeps. Each server and inferior is owned by
the test harness and must end after failure as well as success.

`just web-probe` is for human inspection. It drives named steps, saves
screenshots under the target output directory, and prints console messages.
Use it after layout, theme, panel, focus, or responsive changes even when DOM
tests pass.

`just web-dev` serves Vite on its development port and microscope on a separate
port with the allowed origin configured. It drives a host browser and is one
of the recipes that deliberately leaves the pure build boundary where needed.

## Security review

For a web change, identify which inputs are untrusted:

- WebSocket requests and URL fields;
- source text and program strings;
- renderer JavaScript and pictures;
- profile and capture contents;
- file paths and reverse-proxy headers.

Escape text at rendering boundaries, validate sizes before allocation, and do
not turn program strings into HTML. The listen address and reverse proxy own
network access; a session token is not a replacement for authenticating a
public deployment.

## Web change checklist

- Is debugger behavior still implemented below the web adapter?
- Does every stopped request carry the observed `StopId`?
- Are caches invalidated at the right lifetime boundary?
- Does URL state round-trip and handle old or malformed values?
- Are keyboard and focus behavior accessible?
- Are large results paged or virtualized?
- Does untrusted renderer or profile data stay bounded?
- Are generated protocol files and transcripts current?
- Do unit, browser component, Chromium, and Firefox tests cover the change at
  the appropriate level?
