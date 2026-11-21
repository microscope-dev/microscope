import { createMemo, createSignal, createUniqueId, For, Show } from "solid-js";

import { assert } from "../assert";
import { firstBreakpointPlace } from "../breakpoints";
import { useRequest } from "../data";
import { openingFrame } from "../follow";
import { stopKind } from "../model";
import { best, score } from "../palette";
import { useConnection } from "../store";
import { flash, tab, useTab } from "../tab";
import { chooseTheme, THEMES } from "../theme";
import { deleteNamed, layoutNames, loadNamed, saveNamed } from "../workbench/persist";
import { workbench } from "../workbench/store";
import { placeText } from "./Banner";
import { placeLabel, useShowBreakpoint } from "./Breakpoints";
import { COMMANDS, useCommands } from "./commands";
import { useDialogFocus } from "./dialog";
import { threadLabel } from "./names";
import { useGo, useLinkPaths, useShowSource } from "./navigation";
import { applyLayout, debugLayout, KINDS } from "./panes";
import { fileName } from "./paths";
import { useFocus } from "./Workspace";

const EACH = 8;

interface Item {
  id: string;
  label: string;

  detail?: string | undefined;

  hint?: string | null;
  run(): void;
}

interface Group {
  name: string;
  items: Item[];

  rank: number;
}

type Mode = NonNullable<ReturnType<typeof tab.getState>["palette"]>;

export function Palette() {
  const mode = useTab((current) => current.palette);
  return (
    <Show when={mode()} keyed>
      {(current) => <Open mode={current} />}
    </Show>
  );
}

function close() {
  tab.setState({ palette: null });
}

function run(item: Item | undefined) {
  if (item) {
    close();
    item.run();
  }
}

function openLayout(name: string) {
  const loaded = loadNamed(name, KINDS);
  if (loaded.ok) {
    applyLayout(loaded.layout);
  } else {
    flash(`The layout ${name} can't be opened: ${loaded.why}`);
  }
}

function Open(props: { mode: Mode }) {
  const [query, setQuery] = createSignal("");
  const [selected, setSelected] = createSignal(0);
  const list = createUniqueId();
  let dialog: HTMLDivElement | undefined;
  let field: HTMLInputElement | undefined;
  useDialogFocus(
    () => dialog,
    () => field,
  );
  const trimmed = createMemo(() => query().trim());
  const found = useGroups(() => props.mode, trimmed);

  const sourcesReq = useRequest("sources", () => (props.mode === "files" ? undefined : null));
  const note = createMemo(() => sourcesReq().data?.note);
  const items = createMemo(() => found().groups.flatMap((group) => group.items));
  const chosen = createMemo(() => {
    const currentItems = items();
    const index = Math.min(selected(), Math.max(currentItems.length - 1, 0));
    assert(index >= 0, `palette chose ${index} of ${currentItems.length}`);
    assert(
      currentItems.length === 0 || index < currentItems.length,
      `palette chose ${index} of ${currentItems.length}`,
    );
    return index;
  });

  const activeDescendant = createMemo(() => {
    const item = items()[chosen()];
    return item ? `${list}-${item.id}` : undefined;
  });

  const edit = (event: InputEvent & { currentTarget: HTMLInputElement }) => {
    setQuery(event.currentTarget.value);
    setSelected(0);
  };

  const key = (event: KeyboardEvent) => {
    const currentItems = items();
    const currentChosen = chosen();
    if (event.key === "ArrowDown" || event.key === "ArrowUp") {
      event.preventDefault();
      const step = event.key === "ArrowDown" ? 1 : -1;
      setSelected((currentChosen + step + currentItems.length) % Math.max(currentItems.length, 1));
    } else if (event.key === "Enter") {
      event.preventDefault();
      run(currentItems[currentChosen]);
    } else if (event.key === "Escape") {
      event.preventDefault();
      close();
    }
  };

  return (
    <div class="overlay">
      <div
        class="palette"
        // oxlint-disable-next-line jsx-a11y/prefer-tag-over-role -- an overlay the page draws, not a <dialog>
        role="dialog"
        aria-modal="true"
        aria-label="Command palette"
        ref={(node) => {
          dialog = node;
        }}
      >
        <input
          ref={(node) => {
            field = node;
          }}
          class="palette-query"
          role="combobox"
          aria-expanded="true"
          aria-controls={list}
          aria-activedescendant={activeDescendant()}
          placeholder={
            props.mode === "line"
              ? "a line, or file:line"
              : props.mode === "layouts"
                ? "a layout"
                : props.mode === "save"
                  ? "a name for this layout"
                  : props.mode === "calls"
                    ? "a call of the line to step into"
                    : props.mode === "files"
                      ? "a file"
                      : "a command, function, file, thread, breakpoint, or stop"
          }
          value={query()}
          onInput={edit}
          onKeyDown={key}
          onBlur={close}
        />
        {/* oxlint-disable-next-line jsx-a11y/prefer-tag-over-role -- a combobox's results, not a <select> */}
        <div class="palette-results" id={list} role="listbox" aria-label="Results">
          <Show when={items().length === 0}>
            <div class="empty">{found().why ?? (query() ? "Nothing matches." : null)}</div>
          </Show>
          <For each={found().groups} keyed={(group) => group.name}>
            {(group) => (
              // oxlint-disable-next-line jsx-a11y/prefer-tag-over-role -- a listbox groups its options with role group
              <div role="group" aria-label={group().name}>
                <div class="palette-group">{group().name}</div>
                <For each={group().items} keyed={(item) => item.id}>
                  {(item) => (
                    <Result
                      id={`${list}-${item().id}`}
                      item={item()}
                      on={items()[chosen()] === item()}
                      query={trimmed()}
                    />
                  )}
                </For>
              </div>
            )}
          </For>
        </div>
        <Show when={note()}>
          {(text) => (
            <div class="palette-note" role="note">
              {text()}
            </div>
          )}
        </Show>
      </div>
    </div>
  );
}

function Result(props: { id: string; item: Item; on: boolean; query: string }) {
  const choose = (event: MouseEvent) => {
    event.preventDefault();
    run(props.item);
  };
  return (
    <div
      id={props.id}
      // oxlint-disable-next-line jsx-a11y/prefer-tag-over-role -- a combobox's result, not an <option>
      role="option"
      aria-selected={props.on ? "true" : "false"}
      tabindex={-1}
      class={["palette-item", { on: props.on }]}
      onMouseDown={choose}
    >
      <Marked text={props.item.label} query={props.query} />
      <Show when={props.item.detail}>{(detail) => <span class="dim">{detail()}</span>}</Show>
      <Show when={props.item.hint}>{(hint) => <span class="palette-hint">{hint()}</span>}</Show>
    </div>
  );
}

function Marked(props: { text: string; query: string }) {
  const parts = createMemo(() => {
    const chars = [...props.text];
    const owners = chars.flatMap((char, index) =>
      Array.from({ length: char.toLowerCase().length }, () => index),
    );
    const marks = new Set(score(props.text, props.query)?.marks.map((mark) => owners[mark]) ?? []);
    return { chars, marks };
  });

  return (
    <Show when={parts().marks.size > 0} fallback={<span class="palette-label">{props.text}</span>}>
      <span class="palette-label">
        <For each={parts().chars} keyed={false}>
          {(char, index) => (
            <Show when={parts().marks.has(index)} fallback={<span>{char()}</span>}>
              <mark>{char()}</mark>
            </Show>
          )}
        </For>
      </span>
    </Show>
  );
}

function ranked(name: string, items: Item[], query: string, limit = EACH): Group | null {
  const kept = best(items, query, (item) => item.label, limit);
  if (kept.length === 0) {
    return null;
  }
  assert(kept.length <= limit, `${name} kept ${kept.length} past its limit ${limit}`);
  const first = kept[0];
  assert(first !== undefined, `${name} kept nothing`);
  const found = query === "" ? null : score(first.label, query);
  assert(query === "" || found !== null, `${name} kept an item the query does not match`);
  const rank = found?.rank ?? 0;
  return { name, items: kept, rank };
}

interface Found {
  groups: Group[];
  why?: string;
}

function useGroups(mode: () => Mode, query: () => string): () => Found {
  const focus = useFocus();
  const go = useGo();
  const paths = useLinkPaths();
  const connection = useConnection();
  const { run: runCommand } = useCommands();
  const shown = useTab((current) => current.shown);
  // oxlint-disable-next-line unicorn/no-useless-undefined -- useRequest takes undefined for no parameters
  const sources = useRequest("sources", () => undefined);
  const functions = useRequest("functions", () => {
    const currentQuery = query();
    return mode() === "all" && currentQuery ? { query: currentQuery, limit: EACH } : null;
  });
  const calls = useRequest("stepTargets", () => {
    const { at, stale } = focus();
    return mode() === "calls" && at && !stale
      ? { stop: at.stop, thread: at.thread, task: at.task ?? null }
      : null;
  });

  const showSource = useShowSource();
  const showBreakpoint = useShowBreakpoint();

  return createMemo((): Found => {
    const currentMode = mode();
    const currentQuery = query();
    const { at, state, stale } = focus();
    const currentShown = shown();
    const currentSources = sources();
    const currentFunctions = functions().data;
    const currentCalls = calls();

    if (currentMode === "save") {
      if (currentQuery === "") {
        return { groups: [], why: "Type a name. A layout with the same name gets replaced." };
      }
      return {
        groups: [
          {
            name: "Save",
            rank: 0,
            items: [
              {
                id: "save",
                label: `Save this layout as ${currentQuery}`,
                run: () => {
                  saveNamed(currentQuery, workbench.getState().layout);
                  flash(`Saved the layout as ${currentQuery}`);
                },
              },
            ],
          },
        ],
      };
    }

    if (currentMode === "layouts") {
      const saved: Item[] = layoutNames().flatMap((name) => [
        { id: `layout:${name}`, label: name, run: () => openLayout(name) },
        {
          id: `delete:${name}`,
          label: `Delete ${name}`,
          run: () => {
            deleteNamed(name);
            flash(`Deleted the layout ${name}`);
          },
        },
      ]);
      const presets: Item[] = [
        {
          id: "preset:debug",
          label: "Debug",
          detail: "preset",
          run: () => applyLayout(debugLayout()),
        },
      ];
      const groups = [
        ranked("Saved", saved, currentQuery, 50),
        ranked("Presets", presets, currentQuery),
      ].filter((group): group is Group => group !== null);
      return { groups };
    }

    if (currentMode === "line") {
      const match = /^(?:(.+):)?(\d+)$/u.exec(currentQuery);
      const path = match?.[1] ? paths.recorded(match[1]) : currentShown?.path;
      const line = match ? Number(match[2]) : 0;
      if (currentQuery === "") {
        return {
          groups: [],
          why: currentShown
            ? `Type a line of ${fileName(currentShown.path)}, or file:line.`
            : "Type file:line.",
        };
      }
      if (!match || line < 1) {
        return { groups: [], why: "A line is a number, or file:number." };
      }
      if (!path) {
        return { groups: [], why: "No file is shown; type file:line." };
      }
      return {
        groups: [
          {
            name: "Line",
            rank: 0,
            items: [
              {
                id: "line",
                label: `Go to line ${line}`,
                detail: fileName(path),
                run: () => showSource(path, line),
              },
            ],
          },
        ],
      };
    }

    if (currentMode === "calls") {
      if (!at || stale) {
        return { groups: [], why: "Calls can be stepped into at the program's current stop." };
      }
      if (currentCalls.error) {
        return { groups: [], why: currentCalls.error.message };
      }
      if (!currentCalls.data) {
        return { groups: [], why: "Reading the calls on this line…" };
      }
      if (currentCalls.data.calls.length === 0) {
        return { groups: [], why: "This line makes no calls." };
      }
      const items: Item[] = currentCalls.data.calls.map((call) => ({
        id: `call:${call.call}`,
        label: call.callee ?? (call.target ? `call to ${call.target}` : "indirect call"),
        detail: call.call,
        run: () =>
          void connection
            .request("step", {
              stop: at.stop,
              thread: at.thread,
              task: at.task ?? null,
              kind: "into",
              call: call.call,
            })
            .catch((failure: Error) => flash(failure.message)),
      }));
      const group = ranked("Calls", items, currentQuery, 50);
      return { groups: group ? [group] : [] };
    }

    const files: Item[] = (currentSources.data?.files ?? []).map((path) => ({
      id: `file:${path}`,
      label: paths.link(path),
      run: () => showSource(path, 1),
    }));
    if (currentMode === "files") {
      const group = ranked("Files", files, currentQuery, 50);
      if (currentSources.error) {
        return { groups: [], why: currentSources.error.message };
      }
      if (!currentSources.data) {
        return { groups: [], why: "Reading the program's files…" };
      }
      if (files.length === 0) {
        return { groups: [], why: "The debug information lists no source files." };
      }
      return { groups: group ? [group] : [] };
    }

    const commands: Item[] = COMMANDS.ids
      .filter(
        (command) =>
          !["palette", "files", "line", "stepIntoCall", "saveLayout", "openLayout"].includes(
            command,
          ),
      )
      .map((command) => ({
        id: `command:${command}`,
        label: COMMANDS.get(command).label,
        hint: COMMANDS.describe(command) ?? COMMANDS.describe(command, true),
        run: () => {
          const ran = runCommand(command);
          if (ran.state === "blocked") {
            flash(ran.why);
          } else if (ran.state === "absent") {
            flash(`${COMMANDS.get(command).label} does nothing here`);
          }
        },
      }));
    for (const theme of THEMES) {
      commands.push({
        id: `theme:${theme}`,
        label: `Theme: ${theme}`,
        run: () => chooseTheme(theme),
      });
    }
    if (currentQuery === "") {
      return { groups: [{ name: "Commands", items: commands, rank: 0 }] };
    }

    const matches = (currentFunctions?.functions ?? []).filter((entry) =>
      score(entry.name, currentQuery),
    );
    const functionItems: Item[] = matches.map((entry) => ({
      id: `function:${entry.name}:${entry.path}:${entry.line}`,
      label: entry.name,
      detail: entry.path && entry.line ? `${fileName(entry.path)}:${entry.line}` : undefined,
      run: () => {
        if (entry.path && entry.line) {
          showSource(entry.path, entry.line);
        } else {
          flash(`${entry.name} has no source`);
        }
      },
    }));

    const breakAt: Item[] = matches.slice(0, 3).map((entry) => ({
      id: `break:${entry.name}`,
      label: `Break at ${entry.name}`,
      hint: COMMANDS.describe("toggleBreakpoint"),
      run: () =>
        void connection
          .request("addBreakpoint", { location: entry.name })
          .catch((failure: Error) => flash(failure.message)),
    }));
    let breakGroup: Group | null = null;
    const first = matches[0];
    if (first) {
      const matched = score(first.name, currentQuery);
      assert(matched !== null, "a function the query does not match");
      breakGroup = { name: "Commands", items: breakAt, rank: matched.rank };
    }
    const commandGroup = ranked("Commands", commands, currentQuery);

    const threads: Item[] = state.threads.map((thread) => ({
      id: `thread:${thread.id}`,
      label: threadLabel(thread.id, thread.name),
      run: () => {
        const inferior = state.inferior;
        if (inferior.state === "stopped") {
          go({
            stop: inferior.stop,
            thread: thread.id,
            frame: openingFrame(state, inferior.stop, thread.id),
          });
        } else {
          flash("Threads are shown while the program is stopped");
        }
      },
    }));
    const breakpoints: Item[] = state.breakpoints.map((breakpoint) => ({
      id: `breakpoint:${breakpoint.id}`,
      label: `${placeLabel(breakpoint)} ${firstBreakpointPlace(breakpoint)?.function ?? ""}`.trim(),
      detail: `breakpoint ${breakpoint.id}`,
      run: () => {
        const why = showBreakpoint(breakpoint);
        if (why) {
          flash(why);
        }
      },
    }));
    const stops: Item[] = state.stops.toReversed().map((entry) => ({
      id: `stop:${entry.stop}`,
      label: `#${entry.stop} ${stopKind(entry.reason.kind)} · ${placeText(entry.place)}`,
      detail: entry.stop === at?.stop ? "shown" : undefined,
      run: () => go({ stop: entry.stop, thread: entry.thread, frame: entry.frame }),
    }));

    const groups = [
      ranked("Functions", functionItems, currentQuery),
      commandGroup && breakGroup
        ? {
            ...commandGroup,
            items: [...commandGroup.items, ...breakGroup.items],
            rank: Math.min(commandGroup.rank, breakGroup.rank + 0.5),
          }
        : (commandGroup ?? (breakGroup && { ...breakGroup, rank: breakGroup.rank + 0.5 })),
      ranked("Files", files, currentQuery),
      ranked("Threads", threads, currentQuery),
      ranked("Breakpoints", breakpoints, currentQuery),
      ranked("Stops", stops, currentQuery),
    ].filter((group): group is Group => group !== null);
    return { groups: groups.toSorted((a, b) => a.rank - b.rank) };
  });
}
