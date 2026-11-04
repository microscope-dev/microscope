// The command palette: Ctrl+K searches commands, functions, files, threads,
// breakpoints, and stops; Ctrl+P starts at files, and Ctrl+G at a line of
// the file shown. Groups come in the order of their best match.

import {
  type ChangeEvent,
  type KeyboardEvent,
  type MouseEvent,
  useCallback,
  useId,
  useRef,
  useState,
} from "react";

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

/** How many results each group shows. */
const EACH = 8;

interface Item {
  id: string;
  label: string;
  /** Text after the label, such as where a function is. */
  detail?: string | undefined;
  /** The key that does the same. */
  hint?: string | null;
  run(): void;
}

interface Group {
  name: string;
  items: Item[];
  /** Its best match's rank, which orders the groups. */
  rank: number;
}

type Mode = NonNullable<ReturnType<typeof tab.getState>["palette"]>;

export function Palette() {
  const mode = useTab((current) => current.palette);
  if (!mode) {
    return null;
  }
  // A new mode starts over with nothing typed.
  return <Open key={mode} mode={mode} />;
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

function Open({ mode }: { mode: Mode }) {
  const [query, setQuery] = useState("");
  const [selected, setSelected] = useState(0);
  const list = useId();
  const dialog = useRef<HTMLDivElement>(null);
  const field = useRef<HTMLInputElement>(null);
  useDialogFocus(dialog, field);
  const { groups, why } = useGroups(mode, query.trim());
  // Why the files listed are not all of them, when they are not.
  const note = useRequest("sources", mode === "files" ? undefined : null).data?.note;
  const items = groups.flatMap((group) => group.items);
  const chosen = Math.min(selected, Math.max(items.length - 1, 0));
  assert(chosen >= 0, `palette chose ${chosen} of ${items.length}`);
  assert(items.length === 0 || chosen < items.length, `palette chose ${chosen} of ${items.length}`);
  const edit = useCallback((event: ChangeEvent<HTMLInputElement>) => {
    setQuery(event.target.value);
    setSelected(0);
  }, []);
  const key = useCallback(
    (event: KeyboardEvent) => {
      if (event.key === "ArrowDown" || event.key === "ArrowUp") {
        event.preventDefault();
        const step = event.key === "ArrowDown" ? 1 : -1;
        setSelected((chosen + step + items.length) % Math.max(items.length, 1));
      } else if (event.key === "Enter") {
        event.preventDefault();
        run(items[chosen]);
      } else if (event.key === "Escape") {
        event.preventDefault();
        close();
      }
    },
    [chosen, items],
  );

  return (
    <div className="overlay">
      <div
        className="palette"
        // oxlint-disable-next-line jsx-a11y/prefer-tag-over-role -- an overlay the page draws, not a <dialog>
        role="dialog"
        aria-modal="true"
        aria-label="Command palette"
        ref={dialog}
      >
        <input
          ref={field}
          className="palette-query"
          role="combobox"
          aria-expanded="true"
          aria-controls={list}
          aria-activedescendant={items[chosen] ? `${list}-${items[chosen].id}` : undefined}
          placeholder={
            mode === "line"
              ? "a line, or file:line"
              : mode === "layouts"
                ? "a layout"
                : mode === "save"
                  ? "a name for this layout"
                  : mode === "calls"
                    ? "a call of the line to step into"
                    : mode === "files"
                      ? "a file"
                      : "a command, function, file, thread, breakpoint, or stop"
          }
          value={query}
          onChange={edit}
          onKeyDown={key}
          onBlur={close}
        />
        {/* oxlint-disable-next-line jsx-a11y/prefer-tag-over-role -- a combobox's results, not a <select> */}
        <div className="palette-results" id={list} role="listbox" aria-label="Results">
          {items.length === 0 && (
            <div className="empty">{why ?? (query ? "Nothing matches." : null)}</div>
          )}
          {groups.map((group) => (
            // oxlint-disable-next-line jsx-a11y/prefer-tag-over-role -- a listbox groups its options with role group
            <div key={group.name} role="group" aria-label={group.name}>
              <div className="palette-group">{group.name}</div>
              {group.items.map((item) => (
                <Result
                  key={item.id}
                  id={`${list}-${item.id}`}
                  item={item}
                  on={items[chosen] === item}
                  query={query.trim()}
                />
              ))}
            </div>
          ))}
        </div>
        {note && (
          <div className="palette-note" role="note">
            {note}
          </div>
        )}
      </div>
    </div>
  );
}

function Result({ id, item, on, query }: { id: string; item: Item; on: boolean; query: string }) {
  // Before the input's blur closes the palette.
  const choose = useCallback(
    (event: MouseEvent) => {
      event.preventDefault();
      run(item);
    },
    [item],
  );
  return (
    <div
      id={id}
      // oxlint-disable-next-line jsx-a11y/prefer-tag-over-role -- a combobox's result, not an <option>
      role="option"
      aria-selected={on}
      tabIndex={-1}
      className={`palette-item ${on ? "on" : ""}`}
      onMouseDown={choose}
    >
      <Marked text={item.label} query={query} />
      {item.detail && <span className="dim">{item.detail}</span>}
      {item.hint && <span className="palette-hint">{item.hint}</span>}
    </div>
  );
}

/** The label, with the characters the query matched marked. */
function Marked({ text, query }: { text: string; query: string }) {
  const chars = [...text];
  // Score marks code units of the lowercased text; each belongs to the
  // character it was lowercased from.
  const owners = chars.flatMap((char, index) =>
    Array.from({ length: char.toLowerCase().length }, () => index),
  );
  const marks = new Set(score(text, query)?.marks.map((mark) => owners[mark]) ?? []);
  if (marks.size === 0) {
    return <span className="palette-label">{text}</span>;
  }
  return (
    <span className="palette-label">
      {chars.map((char, index) =>
        // oxlint-disable-next-line react/no-array-index-key -- characters of one fixed label
        marks.has(index) ? <mark key={index}>{char}</mark> : <span key={index}>{char}</span>,
      )}
    </span>
  );
}

/** Ranks `items` by their labels, keeping those that match. */
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

/** The groups to show, and why there are none when that is not because
 * nothing matches the query. */
interface Found {
  groups: Group[];
  why?: string;
}

function useGroups(mode: Mode, query: string): Found {
  const { at, state, stale } = useFocus();
  const go = useGo();
  const paths = useLinkPaths();
  const connection = useConnection();
  const { run: runCommand } = useCommands();
  const shown = useTab((current) => current.shown);
  // oxlint-disable-next-line unicorn/no-useless-undefined -- useRequest takes undefined for no parameters
  const sources = useRequest("sources", undefined);
  const functions = useRequest(
    "functions",
    mode === "all" && query ? { query, limit: EACH } : null,
  ).data;
  const calls = useRequest(
    "stepTargets",
    mode === "calls" && at && !stale
      ? { stop: at.stop, thread: at.thread, task: at.task ?? null }
      : null,
  );

  const showSource = useShowSource();
  const showBreakpoint = useShowBreakpoint();

  if (mode === "save") {
    if (query === "") {
      return { groups: [], why: "Type a name; a layout of that name is replaced." };
    }
    return {
      groups: [
        {
          name: "Save",
          rank: 0,
          items: [
            {
              id: "save",
              label: `Save this layout as ${query}`,
              run: () => {
                saveNamed(query, workbench.getState().layout);
                flash(`Saved the layout as ${query}`);
              },
            },
          ],
        },
      ],
    };
  }

  if (mode === "layouts") {
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
    const groups = [ranked("Saved", saved, query, 50), ranked("Presets", presets, query)].filter(
      (group): group is Group => group !== null,
    );
    return { groups };
  }

  if (mode === "line") {
    const match = /^(?:(.+):)?(\d+)$/u.exec(query);
    const path = match?.[1] ? paths.recorded(match[1]) : shown?.path;
    const line = match ? Number(match[2]) : 0;
    if (query === "") {
      return {
        groups: [],
        why: shown ? `Type a line of ${fileName(shown.path)}, or file:line.` : "Type file:line.",
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

  if (mode === "calls") {
    if (!at || stale) {
      return { groups: [], why: "Calls can be stepped into at the program's current stop." };
    }
    if (calls.error) {
      return { groups: [], why: calls.error.message };
    }
    if (!calls.data) {
      return { groups: [], why: "Reading the calls on this line…" };
    }
    if (calls.data.calls.length === 0) {
      return { groups: [], why: "This line makes no calls." };
    }
    const items: Item[] = calls.data.calls.map((call) => ({
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
    const group = ranked("Calls", items, query, 50);
    return { groups: group ? [group] : [] };
  }

  const files: Item[] = (sources.data?.files ?? []).map((path) => ({
    id: `file:${path}`,
    label: paths.link(path),
    run: () => showSource(path, 1),
  }));
  if (mode === "files") {
    const group = ranked("Files", files, query, 50);
    if (sources.error) {
      return { groups: [], why: sources.error.message };
    }
    if (!sources.data) {
      return { groups: [], why: "Reading the program's files…" };
    }
    if (files.length === 0) {
      return { groups: [], why: "The debug information names no source files." };
    }
    return { groups: group ? [group] : [] };
  }

  const commands: Item[] = COMMANDS.ids
    .filter(
      (command) =>
        !["palette", "files", "line", "stepIntoCall", "saveLayout", "openLayout"].includes(command),
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
  if (query === "") {
    return { groups: [{ name: "Commands", items: commands, rank: 0 }] };
  }

  // Only the functions that match this query, not a slower earlier one's.
  const matches = (functions?.functions ?? []).filter((found) => score(found.name, query));
  const functionItems: Item[] = matches.map((found) => ({
    id: `function:${found.name}:${found.path}:${found.line}`,
    label: found.name,
    detail: found.path && found.line ? `${fileName(found.path)}:${found.line}` : undefined,
    run: () => {
      if (found.path && found.line) {
        showSource(found.path, found.line);
      } else {
        flash(`${found.name} has no source`);
      }
    },
  }));
  // Each function offers a breakpoint, as a command.
  const breakAt: Item[] = matches.slice(0, 3).map((found) => ({
    id: `break:${found.name}`,
    label: `Break at ${found.name}`,
    hint: COMMANDS.describe("toggleBreakpoint"),
    run: () =>
      void connection
        .request("addBreakpoint", { location: found.name })
        .catch((failure: Error) => flash(failure.message)),
  }));
  let breakGroup: Group | null = null;
  const first = matches[0];
  if (first) {
    const matched = score(first.name, query);
    assert(matched !== null, "a function the query does not match");
    breakGroup = { name: "Commands", items: breakAt, rank: matched.rank };
  }
  const commandGroup = ranked("Commands", commands, query);

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

  // Functions come before the commands made from them, at equal rank.
  const groups = [
    ranked("Functions", functionItems, query),
    commandGroup && breakGroup
      ? {
          ...commandGroup,
          items: [...commandGroup.items, ...breakGroup.items],
          rank: Math.min(commandGroup.rank, breakGroup.rank + 0.5),
        }
      : (commandGroup ?? (breakGroup && { ...breakGroup, rank: breakGroup.rank + 0.5 })),
    ranked("Files", files, query),
    ranked("Threads", threads, query),
    ranked("Breakpoints", breakpoints, query),
    ranked("Stops", stops, query),
  ].filter((group): group is Group => group !== null);
  return { groups: groups.toSorted((a, b) => a.rank - b.rank) };
}
