# Documentation

Public documentation lives in `docs/` and is published with VitePress.
Developer documentation lives in `dev/`.

## Code map

| Path | What lives there |
| --- | --- |
| `README.md` | Short human-facing project entrance and public-doc links |
| `AGENTS.md` | Short agent-facing rules and developer-doc router |
| `docs/` | Public, end-user documentation published by VitePress |
| `docs/.vitepress/` | Site navigation, theme, metadata, and build configuration |
| `dev/` | Detailed developer documentation kept in the repository |
| `docs/package.json`, `docs/pnpm-lock.yaml` | Pinned documentation-site tools |
| `justfile` | Documentation install, development, and build recipes |
| `web/test/docs.test.tsx`, `src/eval/tests.rs` | Executable documentation examples and contracts |

## Audience

Public pages explain how to use microscope. They may explain a design choice
when it affects a user's result, but they do not describe internal module
structure, test machinery, or implementation plans.

Developer pages explain how to change and verify the project. `AGENTS.md` is a
short router and a list of rules that apply to almost every change. Put
area-specific details here instead of growing the top-level file.

## Writing

Every developer guide should place a compact `Code map` after its opening
paragraph. Name the few files or directories that provide the best entry
points for that subject, including its main tests. Prefer an exact file when
one owns the behavior. Use a directory when the reader needs the whole
subsystem. Keep the map current when ownership moves.

- Open with what the reader can do or what the component does.
- Use common words and short sentences.
- Define a technical term the first time it is needed.
- Use one name for one concept.
- Prefer a command and its output over a broad feature claim.
- State support and limits exactly.
- Link to the owner of reference material instead of copying it.
- Keep examples complete enough to run.
- Avoid filler, sales language, and claims such as “seamless,” “powerful,” or
  “comprehensive.”
- Use descriptive headings such as “Attach to a running process.”

If the product has no answer yet, say so and record what must exist before the
page can be completed:

```markdown
> TODO: microscope does not have a supported installation method yet. Document
> installation here after release artifacts or packages are available.
```

Do not invent a workflow to make a page appear complete.

## Tested examples

Examples in `docs/expressions.md`, `docs/views.md`,
`docs/writing-views.md`, and `docs/visualizers.md` are test inputs. Keep their
fence names and expected-output syntax intact. Change the reference and the
implementation in the same commit.

## Site

VitePress configuration lives in `docs/.vitepress`. Keep source pages as plain
Markdown with relative links so they remain readable in GitHub and by agents.
Use VitePress components only when ordinary Markdown cannot express the page.

Run the documentation check with:

```sh
just docs
```

It builds the static site and catches broken internal links.

## Public information architecture

The public site has four kinds of page:

- **Start and workflow pages** explain how to complete a task with microscope.
- **Concept pages** explain expressions, views, languages, and other models a
  user must understand.
- **Reference pages** list exact options, settings, commands, and limits.
- **The home page** helps a reader choose the right path.

Keep a workflow in one page when its steps share one goal. Split a reference
only when readers can find an item more easily in the smaller page. File count
is not a goal by itself; clear ownership is.

The public site must not depend on `dev/` links. A person using an
installed microscope may not have the repository. Repository links are
acceptable only when the repository artifact itself is the subject, such as an
unpublished editor extension, and should use a full GitHub URL.

## Developer information architecture

`AGENTS.md` contains universal constraints and a routing table. `dev/` owns
current subsystem design and working practice.

When information changes:

- update `AGENTS.md` only if nearly every change must know it;
- update the owning `dev/` page if it describes current implementation or
  required workflow;
- update public docs when user behavior, setup, output, or limits change.

Avoid copying the same rule into several developer pages. Link to its owner and
repeat only the short consequence needed in local context.

## Page openings

The first paragraph should answer two questions: what is this page about, and
when should the reader use it? Do not begin with project history, a broad claim
about software complexity, or a list of what the page will “explore.”

For a workflow, show the shortest ordinary path before alternatives. For a
reference, state the scope and notation. For an architecture page, name the
boundary and the rule it protects.

## Current behavior and future work

Write current behavior in the present tense. A feature in a plan is not
current behavior. Verify uncertain claims against code, command help, tests,
and fixtures.

If no supported user workflow exists, add a precise TODO rather than inventing
one. Name the prerequisite that will make the TODO actionable. Keep internal
workarounds in developer docs.

Do not use “currently” for facts that are stable constraints unless a known
future change is relevant. Dates and versions belong on support boundaries and
active proposals, not every paragraph.

## Commands and output

Run command help from the pinned build before documenting flags:

```sh
just dev --command cargo run -- --help
just dev --command cargo run -- record --help
```

Prefer a short exact example followed by the rule it demonstrates. Use
`text` fences for terminal output and `sh` for commands. Do not include a shell
prompt in copyable commands.

When output is a public contract, cover it with a CLI or presentation test.
When output is illustrative and contains addresses, IDs, or paths, make that
clear and avoid implying those values are fixed.

## Links and paths

Use relative Markdown links between repository files. Public pages use links
within `docs/` so VitePress can validate them. Developer pages link to source,
tests, and other developer pages relative to `dev/`.

Use descriptive link text. “See the process-control guide” is useful out of
context; “click here” is not.

When moving a public page before the site has stable releases, update all
repository references and remove the old path as one change. Once published
URLs become a compatibility promise, add redirects through site configuration
instead of stub Markdown pages.

## Code fences used by tests

Custom fences beginning with `microscope-` are parsed by repository tests.
VitePress's Markdown configuration renders them as plain text without changing
their source marker.

The important forms are:

- `microscope-example` for expression rows;
- `microscope-view-example` for a view definition and expected rows;
- `microscope-renderer-example` and `microscope-live-example` for sandboxed
  JavaScript;
- language-plus-path fences in the writing-views tutorial, which must quote a
  real fixture fragment.

Do not reformat the body of a path fence unless the fixture changes in the
same patch. The comparison intentionally prevents tutorial code from drifting.

## VitePress

The site root is `docs/`. Navigation and Markdown behavior are in
`docs/.vitepress/config.ts`; small visual changes are in
`docs/.vitepress/theme`. Dependencies are locked with pnpm.

Keep the theme restrained. Readability, code blocks, tables, navigation, dark
mode, mobile layout, and search matter. Avoid page-specific HTML when a normal
heading, table, list, or image works.

The production build uses clean URLs and fails on dead internal links:

```sh
just docs
```

Use `just docs-dev` for local live reload. Inspect desktop and narrow layouts
after theme, table, navigation, or home-page changes.

## Images and diagrams

Add an image only when it explains an interface or relationship more clearly
than text. Store public assets under `docs/public` or an established asset
directory and give them useful alt text.

Use Mermaid sparingly for small architecture or sequence diagrams that render
well in both themes. Prefer prose for one-way flows with only a few steps.

Screenshots should show a stable user task, be cropped to the relevant area,
and avoid personal paths or machine details. Record how to reproduce a
screenshot when exact appearance matters.

## Terminology

Use the model's names consistently:

- **program** for the executable and its code;
- **process** for a running or dumped instance;
- **tracee** in Linux controller internals;
- **native thread** for an OS thread;
- **task** for a neutral runtime unit;
- **goroutine** or **tokio task** when runtime specificity matters;
- **image address** and **virtual address** for their distinct address spaces;
- **view** for logical value presentation;
- **visualizer** or **renderer** for browser drawings as defined by the public
  reference;
- **recording** for the act or session and **profile** for the saved data a
  user opens.

Do not alternate between synonyms merely to vary prose. Repetition is clearer
when it preserves a technical distinction.

## Reviewing prose

Read the page once as a new user and once as a maintainer. Check:

- Can the reader tell whether the page is public, developer, or historical?
- Does the first ordinary task appear before edge cases?
- Does each claim say what microscope actually does?
- Are unsupported cases and partial results explicit?
- Are terms defined before their detailed use?
- Can a paragraph be shortened without losing a rule or reason?
- Are examples runnable and links owned by the right page?
- Does a large table help lookup, or would prose be easier?

Remove filler transitions, self-congratulation, and claims about being simple,
fast, safe, or complete unless the next sentence supplies measurable evidence.

## Documentation checks

For an ordinary public prose change, run `just docs`. Also run the owning test
when changing executable examples. For a developer-only page, run a link check
through the normal review and `git diff --check`; VitePress does not publish or
validate `dev/`.

Search the repository before deleting or moving a page because source comments,
SDK headers, tests, and editor packages can name it:

```sh
rg 'docs/old-page\.md|dev/old-page\.md'
```
