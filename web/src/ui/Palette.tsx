// The command palette: Ctrl+K searches commands, functions, files, threads,
// breakpoints, and stops; Ctrl+P starts at files, and Ctrl+G at a line of
// the file shown. Groups come in the order of their best match.

import { useId, useState } from "react";
import { firstBreakpointPlace } from "../breakpoints";
import { useRequest } from "../data";
import { openingFrame } from "../follow";
import { type Command, describe, LABELS } from "../keys";
import { stopKind } from "../model";
import { best, score } from "../palette";
import { useConnection } from "../store";
import { flash, tab, useTab } from "../tab";
import { chooseTheme, THEMES } from "../theme";
import { placeText } from "./Banner";
import { placeLabel } from "./Breakpoints";
import { useCommands } from "./commands";
import { useGo, useLinkPaths, useShowSource } from "./navigation";
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

type Mode = "all" | "files" | "line" | "calls";

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

function Open({ mode }: { mode: Mode }) {
  const [query, setQuery] = useState("");
  const [selected, setSelected] = useState(0);
  const list = useId();
  const groups = useGroups(mode, query.trim());
  const items = groups.flatMap((group) => group.items);
  const chosen = Math.min(selected, Math.max(items.length - 1, 0));
  const run = (item: Item | undefined) => {
    if (item) {
      close();
      item.run();
    }
  };

  return (
    <div className="overlay">
      <div className="palette" role="dialog" aria-modal="true" aria-label="Command palette">
        <input
          className="palette-query"
          role="combobox"
          aria-expanded="true"
          aria-controls={list}
          aria-activedescendant={items[chosen] ? `${list}-${items[chosen].id}` : undefined}
          placeholder={
            mode === "line"
              ? "a line, or file:line"
              : mode === "calls"
                ? "a call of the line to step into"
                : mode === "files"
                  ? "a file"
                  : "a command, function, file, thread, breakpoint, or stop"
          }
          value={query}
          // biome-ignore lint/a11y/noAutofocus: the palette opens to be typed in
          autoFocus
          onChange={(event) => {
            setQuery(event.target.value);
            setSelected(0);
          }}
          onKeyDown={(event) => {
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
          }}
          onBlur={close}
        />
        <div className="palette-results" id={list} role="listbox" aria-label="Results">
          {items.length === 0 && <div className="empty">{query ? "Nothing matches." : ""}</div>}
          {groups.map((group) => (
            // biome-ignore lint/a11y/useSemanticElements: a listbox groups its options with role group
            <div key={group.name} role="group" aria-label={group.name}>
              <div className="palette-group">{group.name}</div>
              {group.items.map((item) => {
                const on = items[chosen] === item;
                return (
                  <div
                    key={item.id}
                    id={`${list}-${item.id}`}
                    role="option"
                    aria-selected={on}
                    tabIndex={-1}
                    className={`palette-item ${on ? "on" : ""}`}
                    // Before the input's blur closes the palette.
                    onMouseDown={(event) => {
                      event.preventDefault();
                      run(item);
                    }}
                    onKeyDown={() => undefined}
                  >
                    <Marked text={item.label} query={query.trim()} />
                    {item.detail && <span className="dim">{item.detail}</span>}
                    {item.hint && <span className="palette-hint">{item.hint}</span>}
                  </div>
                );
              })}
            </div>
          ))}
        </div>
      </div>
    </div>
  );
}

/** The label, with the characters the query matched marked. */
function Marked({ text, query }: { text: string; query: string }) {
  const marks = new Set(score(text, query)?.marks ?? []);
  if (marks.size === 0) {
    return <span className="palette-label">{text}</span>;
  }
  return (
    <span className="palette-label">
      {[...text].map((char, index) =>
        // biome-ignore lint/suspicious/noArrayIndexKey: characters of one fixed label
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
  const rank = query === "" ? 0 : (score(kept[0]?.label ?? "", query)?.rank ?? 3);
  return { name, items: kept, rank };
}

function useGroups(mode: Mode, query: string): Group[] {
  const { at, state } = useFocus();
  const go = useGo();
  const paths = useLinkPaths();
  const connection = useConnection();
  const runCommand = useCommands();
  const shown = useTab((current) => current.shown);
  const sources = useRequest("sources", undefined).data;
  const functions = useRequest(
    "functions",
    mode === "all" && query ? { query, limit: EACH } : null,
  ).data;
  const calls = useRequest(
    "stepTargets",
    mode === "calls" && at ? { stop: at.stop, thread: at.thread, task: at.task ?? null } : null,
  ).data;

  const showSource = useShowSource();

  if (mode === "line") {
    const match = /^(?:(.+):)?(\d+)$/.exec(query);
    const path = match?.[1] ? paths.recorded(match[1]) : shown?.path;
    const line = match ? Number(match[2]) : 0;
    if (!path || line < 1) {
      return [];
    }
    return [
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
    ];
  }

  if (mode === "calls") {
    if (!at) {
      return [];
    }
    const items: Item[] = (calls?.calls ?? []).map((call) => ({
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
    return group ? [group] : [];
  }

  const files: Item[] = (sources?.files ?? []).map((path) => ({
    id: `file:${path}`,
    label: paths.link(path),
    run: () => showSource(path, 1),
  }));
  if (mode === "files") {
    const group = ranked("Files", files, query, 50);
    return group ? [group] : [];
  }

  const commands: Item[] = (Object.keys(LABELS) as Command[])
    .filter((command) => !["palette", "files", "line", "stepIntoCall"].includes(command))
    .map((command) => ({
      id: `command:${command}`,
      label: LABELS[command],
      hint: describe(command) ?? describe(command, true),
      run: () => {
        if (!runCommand(command)) {
          flash(`${LABELS[command]} does nothing here`);
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
    return [{ name: "Commands", items: commands, rank: 0 }];
  }

  // Only the functions that match this query, not a slower earlier one's.
  const found = (functions?.functions ?? []).filter((found) => score(found.name, query));
  const functionItems: Item[] = found.map((found) => ({
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
  const breakAt: Item[] = found.slice(0, 3).map((found) => ({
    id: `break:${found.name}`,
    label: `Break at ${found.name}`,
    hint: describe("toggleBreakpoint"),
    run: () =>
      void connection
        .request("addBreakpoint", { location: found.name })
        .catch((failure: Error) => flash(failure.message)),
  }));
  const breakGroup: Group | null =
    breakAt.length > 0
      ? { name: "Commands", items: breakAt, rank: score(found[0]?.name ?? "", query)?.rank ?? 3 }
      : null;
  const commandGroup = ranked("Commands", commands, query);

  const threads: Item[] = state.threads.map((thread) => ({
    id: `thread:${thread.id}`,
    label: `thread ${thread.id}${thread.name ? ` ${thread.name}` : ""}`,
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
      const place = firstBreakpointPlace(breakpoint);
      if (place?.path && place.line) {
        showSource(place.path, place.line);
      }
    },
  }));
  const stops: Item[] = [...state.stops].reverse().map((entry) => ({
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
  return groups.sort((a, b) => a.rank - b.rank);
}
