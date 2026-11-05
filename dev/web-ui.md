# Web UI

`microscope web` serves the React application in `web/` and connects every tab
to one debugger session over WebSocket. The server is a `DebuggerHandle` client,
like the CLI and DAP server.

The page is a workbench, as in an IDE: a menu bar, a run toolbar, and panes in
tabbed groups that split, float, and dock. The menu bar ends with who is here
and Share. The run toolbar shows only while a program is loaded; without one,
such as for a profile, the run state speaks only to warn, from the menu bar.
Every view is a pane, including Account, Settings, and the forms that launch
or attach. Only the join form sits outside the workbench.

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
| `web/src/router.tsx`, `web/src/ui/Routes.tsx`, `web/src/tab.ts` | Addresses, what each opens in the workbench, and per-tab state |
| `web/src/workbench/` | Generic layout model, frame components, drag geometry, keys, and command registry; depends only on React, Zustand, and the shared assertion helper |
| `web/src/ui/panes.tsx`, `web/src/ui/commands.ts` | Every pane kind, the layout presets, and every command |
| `web/src/ui/` | Pane bodies and the frame around the workbench |
| `web/src/target.ts`, `web/src/account.ts`, `web/src/ui/Account.tsx` | The page served on its own: which microscope it talks to, and services sign-in |
| `web/src/orgs.ts`, `web/src/ui/Orgs.tsx` | Services' organizations, teams, and projects, and the panes that manage them |
| `web/src/protocol.gen.ts` | Generated TypeScript protocol; do not edit by hand |
| `web/src/visualize/`, `sdk/web/` | Sandboxed visualizers and their public type contract |
| `web/test/` | Fast TypeScript, React, replay, and sandbox tests |
| `web/e2e/`, `tests/web/` | Browser end-to-end tests and Rust server tests |
| `web/vite.config.ts`, `web/playwright.config.ts` | Browser build and end-to-end configuration |

## The workbench

`web/src/workbench/` is generic: it knows panes only through their kinds, and
imports nothing from the debugger. `web/src/ui/panes.tsx` registers the
debugger's kinds and `web/src/ui/commands.ts` its commands.

- **Layout** (`layout.ts`): plain data, a tree of splits whose leaves are
  groups of tabs, plus floating windows, a maximized group, and hidden docks.
  Only its operations change it (`open`, `close`, `move`, `split`, `float`,
  `dock`, `toggleHidden` and the rest). Each returns a layout that keeps every
  rule `checkLayout` lists; development builds assert it after every change.
- **Pane kinds** (`kinds.ts`): a kind gives a pane its title, body, optional
  badge, what it `needs` (a session, a connection, or control), and whether it
  stays mounted when hidden. A pane is a kind and a key, so there is one pane
  per kind and key: one Registers, one Source per file, one Profile per
  profile. A pane's own state goes in its params, which the layout saves.
- **Frame** (`Dock.tsx`, `Group.tsx`, `SplitView.tsx`, `PaneHost.tsx`,
  `MenuBar.tsx`): draws the layout. `PaneHost` shows why a pane is empty when
  its need is missing, and catches a pane that throws so the rest keeps
  working. Each has a CSS module beside it; `styles.css` keeps the tokens and
  what pane bodies share. Code and tests find frame parts by role or
  `data-part`, never by a module's class names.
- **Commands** (`commands.ts` here, and `web/src/ui/commands.ts`): see
  [actions and focus](#actions-and-focus).
- **Persistence** (`persist.ts`, `saved.ts`): the layout is saved per browser.
  `saved.ts` has a version and a migration per version, and
  `web/test/layouts/` keeps a fixture of every version ever written, each of
  which must still load.

### Addresses

Every address renders the same workbench and says what it wants from it
(`web/src/ui/Routes.tsx`). `/s/SESSION/...` wants a session's panes, and gets
the Debug layout when no open pane shows one. `/` starts at Account when every open
pane needs a session; a server started on a profile starts at that profile
instead. `/profile/ID` opens that profile's pane, and the address follows the
pane while it is the one shown. `/pick?tab=` opens the Start pane its tab
became. `/join` alone shows without the frame.

### Adding a pane

1. Add its id to `KIND` in `web/src/documents.ts`.
2. Write its body as a component. Take `PaneProps` when it needs the pane's
   key or params. Read the session's focus with `useFocus()` only if its kind
   needs a session.
3. Register the kind in `KINDS` in `web/src/ui/panes.tsx`: title, body, needs,
   `keepAlive`, and a badge if the tab should count something.
4. Give it a command in `web/src/ui/commands.ts` (`debugPane` for a session's
   view, `pane` for one that needs none), so the menus, palette, and Keyboard
   shortcuts pane offer it. Most panes need no shortcut. Before adding one,
   test it in real Chrome and Firefox on each supported platform, including
   while typing. Synthetic key events cannot establish whether browser or
   operating-system shortcuts intercept a key.
5. Decide whether a preset (`debugLayout`, `startLayout`) should hold it. A
   saved layout without it still loads.
6. Test what a person does with it through the driver, and add it to a journey
   if it changes one. The swarm finds its menu item on its own.

## Protocol and state

The server sends tab state and notices. A tab sends requests that include the
`StopId` of the state it read. Cache read answers by stop and request. Forget
them after writes, setting changes, and session changes.

The address bar contains the tab's complete view: stop, thread or task, frame,
source, document, and memory location. A shared link must open the same view.

Formatting shared with other clients belongs in `src/present`.

`web/src/protocol.gen.ts` is generated from `src/web/protocol.rs` with ts-rs.
A test detects stale output. Regenerate it with:

```sh
MICROSCOPE_UPDATE_PROTOCOL=1 just test protocol
```

## Building and testing

Journeys are written once against the `User` interface in
`web/test/driver/user.ts`: `press`, `menu`, `chooseTab`, `expectGroups`, and
the debugger's own steps. `fast.ts` runs them in Vitest's Chromium against a
scripted debugger in milliseconds; `real.ts` runs them in Playwright against a
real `microscope web`. When the page changes how something is done, change the
backends, not the journeys.

`web/e2e/swarm.spec.ts` drives random menu commands, keys, tab clicks, and
drags against a live session, checking after each that every group shows one
tab and no pane failed. `just web-e2e` runs 60 steps.
`MICROSCOPE_SWARM_STEPS=1000 just web-e2e swarm` runs it at depth, and a
failure's `MICROSCOPE_SWARM_SEED` replays it.


Debug builds read the page from `build/web` at runtime. Release builds embed
it.

```sh
just web-build
just web-test
just web-watch
just web-props
just web-e2e
just web-e2e --project=chromium -g NAME
```

`just web-test` runs oxlint, oxfmt, TypeScript, and Vitest; `just web-fmt`
formats. End-to-end tests run real servers in Chromium and Firefox. The test budgets, journeys, and property tests are
described in [testing](testing.md#web-tests).

Replay tests use recorded server transcripts. Update them with `just
web-transcripts` when the protocol change is intentional.

To inspect the page, run:

```sh
just web-probe PROGRAM STEPS...
```

The probe saves a screenshot after each step and prints browser-console
messages. `web/e2e/probe.ts` lists the available steps.

## Serving the page on its own

`just web` serves the page from Vite on `0.0.0.0:3000`, reloading on every
save; `just web 127.0.0.1` keeps it local. A page that `microscope web` did
not serve has no `microscope-base` meta tag, so `web/src/target.ts` treats it
as standalone: it asks for a join link, keeps the link's server and token in
localStorage, and presents the token itself. The socket offers it as the
subprotocol `microscope-token.TOKEN` beside `microscope`; `/api/check` and
`/api/profile/{id}` take it as the POST body. A presented token is good from
any origin, since only a join link could have given it; the cookie is still
good only from the server's own pages.

The standalone page also offers an optional sign-in to the services backend,
which it reaches under its own `/api/`. Vite proxies that to
`MICROSCOPE_SERVICES_URL`, by default `http://127.0.0.1:8080`, and renames
services' `__Host-` cookies to `dev-`, since a browser keeps those only over
HTTPS. Start
services from its own repository with `just up` and `just run`. Without it,
the page says sign-in is unavailable and connecting still works.

The Account pane signs up, signs in, and signs out (`web/src/account.ts`).
When services sends email, a new account must confirm its address first,
and signing in takes the password and then the six-digit code services
emails. The links in those emails lead to the page's `/confirm-email` and
`/login-link`, with the token in the fragment. Those addresses open the
Account pane, which says how the link went. Without email, a password alone
signs in.

When services sends email (`GET /api/features` reports `email`), the sign-in
form also offers "Forgot password?". Services emails a link to the page's
`/reset-password`, token in the fragment, and the Account pane asks for a
new password, signed in or not. Changing it ends every session of that
account, and the pane asks services again who this browser is.

Connecting or disconnecting Google, GitHub, or an organization's single
sign-on takes a session that proved who holds it in the last ten minutes,
by signing in or by the password (`POST /api/users/me/verify`). When
services refuses with "confirm it is you first", `proven` in
`web/src/sso.ts` keeps the change, and the pane asks for the password, or
for an account without one, to sign in again. Then it makes the change.

An account can turn on two-factor authentication from the same pane: its
password, then a QR code of services' `otpauth://` address (drawn in the
page by the vendored `web/src/vendor/qrcodegen.ts`, so the secret never
leaves it), then a code from the app, then ten recovery codes shown once.
Such an account signs in with its password and then a code from the app or
a recovery code, never an emailed code. Turning it off or replacing the
recovery codes takes the password and a code.

Single sign-on (`web/src/sso.ts`, `web/src/ui/SingleSignOn.tsx`) signs in
with Google, GitHub, or an organization's own SAML identity provider,
whichever services' operator turned on (`--sso-providers`).
`GET /api/features` says which are on, whether services sends email, and
whether accounts can turn on two-factor authentication;
`web/src/features.ts` reads it, and the page hides what the server does not
offer. Signing in
follows a plain link to services' `/api/sso/PROVIDER/start`; connecting
another way in to a signed-in account POSTs to `.../connect` first, which
ties the sign-in to that account. Either way the browser comes back to the
page's `/signed-in`, whose fragment carries an error, a notice, or a TOTP
challenge for the Account pane, and nothing when services set the session.
The Account pane lists the account's sign-in methods and will not
disconnect the last one. An organization's owners set up SAML in its
Settings: services' values to paste into the identity provider, the
identity provider's entity ID, SSO URL, and certificates, and a sign-in
link, `/sso#key=KEY`, to hand to members. Anyone it signs in joins the
organization. Locally, run services' `just mock-idp` and pass `just run`
the flags it prints; its sign-in pages list made-up people.

Passkeys (`web/src/passkeys.ts`) are on when `GET /api/features` reports
`passkeys`, which services does once it has a `--public-url` whose host is a
domain: a passkey belongs to a domain, never an IP address. A signed-in
account adds one from its sign-in methods, through `proven` like connecting
Google; there is no signing up with one. The sign-in form has a "Sign in
with a passkey" button and offers passkeys in the username field's autofill
(`autocomplete="username webauthn"`), and the button stops the autofill's
request before making its own. A passkey checks who is there itself, so it
signs in without an emailed code or TOTP. Services sends WebAuthn's options
as JSON with base64url bytes; the page converts them by hand, since the
browser's own JSON helpers are newer than some browsers people use.

A signed-in account also has organizations and projects
(`web/src/orgs.ts`, `web/src/ui/Orgs.tsx`). The Account pane lists the
person's shared organizations and pending invitations, and makes new
organizations; services never returns the personal organization each user
has, so the page never shows it. The Projects pane lists every project the
person can read, their own first and then each organization's by name. An
Organization pane (kind `org`, keyed by the organization's id) has Members,
Projects, Teams, and Settings. The Projects, New project, and New
organization commands are absent until someone signs in. Every answer
carries the person's role or permission, which decides only which controls
show; services checks each request, and its refusal shows where the control
was. `useDirectory` shares one load of organizations, projects, and
invitations between the panes, and `reload` asks again after a change.

`web/e2e/accounts.spec.ts` runs this against the real services server, with
a fake Postmark that keeps each email so the test can follow its link or
type its code. `web/e2e/totp.spec.ts` does the same for two-factor
authentication, with a fake Cloud KMS behind services' real KMS client
(`--totp-kms-api-url`, `--totp-metadata-url`); it computes codes from the
key the page shows. `TOTP_SHOTS=<dir>` saves a screenshot of each step.
`web/e2e/sso.spec.ts` runs single sign-on against services and its
`mock-idp` binary, built beside it, including a forged SAML assertion and a
tampered Google token that must sign nobody in. `SSO_SHOTS=<dir>` saves
screenshots. `web/e2e/passkeys.spec.ts` adds, uses, and removes a passkey
with Chromium's virtual authenticator, which makes real keys; it serves the
page from `localhost` (`startServices({ localhost: true })`) and skips in
Firefox, which has no such authenticator. `PASSKEY_SHOTS=<dir>` saves
screenshots. Services hashes passwords at argon2's minimum costs there
(`--insecure-fast-password-hashing`, debug builds only). It runs services
from `../services/target/debug`, or from `MICROSCOPE_SERVICES_BINARY`, and
skips when that is not built. Build it and start its database from the
services repository with `just build` and `just up`.

Use binaries in `web/node_modules/.bin`. Do not use
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

`web/src/ui/Processes.tsx` lists the members in the Processes pane, with a
Detach button for a stopped member other than the root. A single-process
session shows an empty-state explanation instead.

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
- tab: selected context, open files, expanded values, and pinned drawings;
- browser: where panes sit (`workbench/persist.ts`), kept in localStorage and
  never part of a URL, since a link shares where to look, not how a page is
  arranged;
- component: transient focus, hover, and drag state.

Put data in the narrowest lifetime that owns it. On a new stop, forget every
cached answer tied to the previous `StopId`. On session replacement, also
forget module, view, and target state. A settings or memory write can require
more selective invalidation within one stop.

## URL state

`router.tsx`, `tab.ts`, and UI path helpers encode a tab's complete navigable
view. A copied URL should restore the same stop where retained, execution
context, frame, source, document, memory address, and pinned values.

URL parsing is an untrusted boundary. Reject or normalize invalid IDs, paths,
indexes, and duplicate fields. Do not issue debugger writes merely by opening a
URL.

A drag changes the layout once, when it drops. Until then `workbench/drag.ts`
moves the ghost, the highlight, and a dragged window by hand, at most once a
frame, against rectangles measured as the drag began, so a drag renders
nothing. If something else changes the layout mid-drag, the drop does
nothing, since what it measured may be gone.

Every press that follows the pointer, whether a tab or window drag, a
separator, or a window's resize grip, goes through `trackPointer` in
`workbench/gesture.ts`. It runs one gesture at a time, listens on the window
so a gesture still ends when a re-render removes the element pressed, and
cancels on Escape or when the browser takes the pointer away. A new gesture
should use it rather than its own listeners.

When adding durable view state, update parsing, serialization, back/forward
behavior, sharing tests, and unknown-value fallback. Ephemeral state such as a
hover or open menu should stay out of the URL.

## Actions and focus

Every command is defined once, in `web/src/ui/commands.ts`, in a registry
from `web/src/workbench/commands.ts`: its label, menu, keys, when it can run,
and what it does. The menus, buttons, keys, and the command palette read
that registry rather than duplicating control logic, and so does the
Keyboard shortcuts pane, so no key is listed by hand. Run control asks
`actions.ts`.

A command says whether it can run now: ready, blocked with the reason, or
absent where it means nothing, such as stepping with no program. A blocked
command's key shows the reason; an absent one's key is left to the browser,
except function keys, which the page always claims. Disabled controls should
explain the missing condition where the UI has room. Keyboard handling
must respect text inputs, dialogs, renderer canvases, and platform modifier
conventions.

Focus is part of accessibility and debugger speed. After a stop or pane
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

Profile panes use typed server queries over validated captures. Flame trees,
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
Use it after layout, theme, pane, focus, or responsive changes even when DOM
tests pass.

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
- Is a new view a pane kind with a command, rather than a page or a fixed
  part of the frame?
- Are keyboard and focus behavior accessible?
- Are large results paged or virtualized?
- Does untrusted renderer or profile data stay bounded?
- Are generated protocol files and transcripts current?
- Do unit, browser component, Chromium, and Firefox tests cover the change at
  the appropriate level?
