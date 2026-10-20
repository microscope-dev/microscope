// The code at the focus, in one of four views of the place. Source shows
// the file and lines the link names, or else the frame's own line, or,
// before the program runs, its main function (D2). A frame with no source
// shows its instructions instead.

import { useEffect, useLayoutEffect, useMemo, useRef } from "react";
import {
  askedAt,
  breakpointAt,
  knownBreakpointPlaces,
  namesFile,
  toggleBreakpoint,
} from "../breakpoints";
import { cache, useRequest } from "../data";
import { followedLook, formatPlace, inView, parsePlace, showingSource, type View } from "../focus";
import { stopNote } from "../follow";
import { controls } from "../model";
import type { Breakpoint, Row } from "../protocol";
import { frameAt } from "../stack";
import { useConnection, useModel } from "../store";
import { closeFile, openFile, shownAt, tab, useTab } from "../tab";
import { Disassembly } from "./Disassembly";
import { Drawings, useCards } from "./Drawings";
import { Memory } from "./Memory";
import { useLinkPaths, useLook } from "./navigation";
import { fileName } from "./paths";
import { useInlineValues } from "./source/inline";
import { type MarkKind, type Marks, SourceView } from "./source/SourceView";
import { useFocus } from "./Workspace";

/** The file a tab shows and where in it, before its text arrives. */
export interface Shown {
  path: string;
  /** The line to bring into view. */
  line: number | null;
  /** The lines the link selects. */
  selection: { line: number; end: number } | null;
}

export function useShown(): Shown | null {
  const { look } = useFocus();
  const paths = useLinkPaths();
  const own = useOwnSource();
  const named = look.src ? parsePlace(look.src) : null;
  if (named) {
    const path = paths.recorded(named.path);
    return path
      ? { path, line: named.line, selection: { line: named.line, end: named.end } }
      : null;
  }
  return own;
}

/** What the focus shows when no link names a file: the frame's line, or
 * before the program runs, its main function. */
function useOwnSource(): Shown | null {
  const { at, trace, state } = useFocus();
  const sources = useRequest("sources", state.session ? undefined : null).data;
  const frame = frameAt(trace, at);
  if (frame?.source) {
    return { path: frame.source.path, line: frame.source.line, selection: null };
  }
  if (!at && sources?.entry) {
    return { path: sources.entry.path, line: sources.entry.line, selection: null };
  }
  return null;
}

const VIEWS: readonly { view: View; label: string; key: string }[] = [
  { view: "source", label: "Source", key: "Alt+S" },
  { view: "disassembly", label: "Disassembly", key: "Alt+D" },
  { view: "memory", label: "Memory", key: "Alt+M" },
  { view: "drawings", label: "Drawings", key: "Alt+V" },
];

export function CodeArea() {
  const { at, trace, state, stale, look: current } = useFocus();
  const shown = useShown();
  const own = useOwnSource();
  const files = useTab((current) => current.files);
  const look = useLook();
  const paths = useLinkPaths();
  const drawings = useCards().length;

  const path = shown?.path;
  const line = shown?.line ?? null;
  // Before paint, so a file newly shown never appears without its tab. A
  // file closed opens again at every new stop, when its line moves, or when
  // something asks for it.
  const stop = at?.stop;
  const asked = useTab((current) => current.asked);
  // Closing the last file shows the focus's own place, which stays closed:
  // this names the showing it must not reopen.
  const unasked = useRef<string | null>(null);
  useLayoutEffect(() => {
    const skip = unasked.current === `${path}:${line}:${stop}:${asked}`;
    unasked.current = null;
    if (path && !skip) {
      openFile(path);
    }
  }, [path, line, stop, asked]);
  // A file closed stays closed until something shows it again.
  const open = path !== undefined && files.includes(path);
  const place =
    current.src ??
    (path && formatPlace({ path: paths.link(path), line: line ?? 1, end: line ?? 1 }));
  useEffect(() => {
    tab.setState({ shown: path && open ? { path, line } : null });
    if (path && open && place) {
      shownAt(path, place);
    }
  }, [path, line, open, place]);

  const frame = frameAt(trace, at);
  // A frame with no source, and no file named, shows its instructions.
  const sourceless = !shown && frame !== undefined && !frame.source;
  const view = current.view ?? (sourceless ? "disassembly" : "source");
  // A file's tab follows the focus when the focus shows that file by
  // itself, or else shows where the file was last shown.
  const show = (path: string) => {
    const place = tab.getState().places[path];
    look(
      (current) =>
        path === own?.path
          ? inView(followedLook(current), "source")
          : showingSource(
              current,
              place ?? formatPlace({ path: paths.link(path), line: 1, end: 1 }),
            ),
      { replace: false, ask: true },
    );
  };
  // Closing the file shown shows its neighbor, the next one or else the one
  // before it. Closing the last drops it from the address.
  const close = (closed: string) => {
    const index = files.indexOf(closed);
    const neighbor = files[index + 1] ?? files[index - 1];
    closeFile(closed);
    if (closed !== path) {
      return;
    }
    if (neighbor) {
      show(neighbor);
    } else if (current.src) {
      unasked.current = `${own?.path}:${own?.line ?? null}:${stop}:${asked}`;
      look(({ src: _, ...rest }) => rest, { replace: false });
    }
  };

  return (
    <section className="pane code grow" aria-label="Code">
      <nav className="file-tabs" aria-label="Open files">
        {files.map((path) => (
          <div key={path} className={`file-tab ${path === shown?.path ? "on" : ""}`} title={path}>
            <button
              type="button"
              className="file-name"
              aria-current={path === shown?.path ? "page" : undefined}
              onClick={() => show(path)}
              // A middle click closes the tab, without the browser's autoscroll.
              onMouseDown={(event) => event.button === 1 && event.preventDefault()}
              onAuxClick={(event) => event.button === 1 && close(path)}
            >
              {fileName(path)}
            </button>
            <button
              type="button"
              className="close"
              aria-label={`Close ${fileName(path)}`}
              onClick={() => close(path)}
            >
              ×
            </button>
          </div>
        ))}
        <span className="file-tabs-end">
          {stale === "running" && at && (
            <span className="showing">Showing stop #{at.stop} · program running</span>
          )}
          <fieldset className="segmented small" aria-label="View">
            {VIEWS.map((choice) => (
              <button
                key={choice.view}
                type="button"
                aria-pressed={view === choice.view}
                title={choice.key}
                onClick={() => look((look) => inView(look, choice.view), { replace: false })}
              >
                {choice.label}
                {choice.view === "drawings" && drawings > 0 && (
                  <span className="badge">{drawings}</span>
                )}
              </button>
            ))}
          </fieldset>
        </span>
      </nav>
      {view === "disassembly" ? (
        <>
          {sourceless && (
            <div className="view-note" data-testid="no-source">
              {frame.name} has no source: showing its instructions
            </div>
          )}
          <Disassembly />
        </>
      ) : view === "memory" ? (
        <Memory />
      ) : view === "drawings" ? (
        <Drawings />
      ) : shown && open ? (
        <SourceFile shown={shown} />
      ) : (
        <div className="empty center-message">
          {shown
            ? "No file open."
            : at && frame && !frame.source
              ? `${frame.name} has no source${frame.address ? `: it is at ${frame.address}` : ""}${frame.module ? ` in ${frame.module}` : ""}.`
              : at && !trace
                ? stale === "passed"
                  ? `Stop #${at.stop} has passed.`
                  : "Reading the stack…"
                : state.inferior.state === "notStarted"
                  ? "Press F5 to run the program."
                  : "No source to show."}
        </div>
      )}
    </section>
  );
}

/** Lines with breakpoints in a file, and what kind each is. */
export function breakpointLines(
  breakpoints: readonly Breakpoint[],
  path: string,
): Map<number, MarkKind> {
  const lines = new Map<number, MarkKind>();
  for (const breakpoint of breakpoints) {
    const kind: MarkKind = breakpoint.logMessage
      ? "log"
      : breakpoint.condition || breakpoint.hitCondition
        ? "conditional"
        : "plain";
    for (const pending of breakpoint.pending?.places ?? []) {
      const place = pending.place;
      if (place.path && namesFile(place.path, path) && place.line && !lines.has(place.line)) {
        lines.set(place.line, "pending");
      }
    }
    for (const place of breakpoint.places) {
      if (place.path && namesFile(place.path, path) && place.line) {
        lines.set(place.line, kind);
      }
    }
    // One set at a line no code has yet shows where it was asked for.
    const asked = askedAt(breakpoint, path);
    if (knownBreakpointPlaces(breakpoint).length === 0 && asked && !lines.has(asked)) {
      lines.set(asked, "pending");
    }
  }
  return lines;
}

function SourceFile({ shown }: { shown: Shown }) {
  const { at, trace, state, stale } = useFocus();
  const connection = useConnection();
  const control = useModel(controls);
  const paths = useLinkPaths();
  const look = useLook();
  const source = useRequest("source", { path: shown.path });

  // Values show for the frame shown, while its stop is the program's.
  const live = at && !stale ? at : null;
  const frame = frameAt(trace, at);
  const frameSource = frame?.source?.path === shown.path ? frame.source : null;
  const inline = useInlineValues(
    live,
    shown.path,
    source.data?.path === shown.path ? source.data.text : undefined,
    live ? (frameSource?.line ?? null) : null,
    frameSource?.first ?? null,
  );

  const marks = useMemo((): Marks => {
    const innermost = trace?.frames[0];
    return {
      breakpoints: breakpointLines(state.breakpoints, shown.path),
      breakable: new Set(source.data?.breakable ?? []),
      pc: innermost?.source?.path === shown.path ? innermost.source.line : null,
      frame:
        frame && frame.index > 0 && frame.source?.path === shown.path ? frame.source.line : null,
      selection: shown.selection,
      inline,
      note: (() => {
        const text = at ? stopNote(state, at) : null;
        return text && frame?.source?.path === shown.path
          ? { line: frame.source.line, text }
          : null;
      })(),
    };
  }, [trace, frame, at, state, shown, source.data, inline]);

  if (source.error) {
    return (
      <div className="empty center-message">
        Cannot show {shown.path}: {source.error.message}
      </div>
    );
  }
  if (!source.data || source.data.path !== shown.path) {
    return <div className="empty center-message">Reading {fileName(shown.path)}…</div>;
  }

  const toggle = (line: number) => {
    if (!control) {
      return;
    }
    const existing = breakpointAt(state.breakpoints, shown.path, line);
    void toggleBreakpoint(connection, existing, `${shown.path}:${line}`);
  };

  return (
    <SourceView
      path={shown.path}
      text={source.data.text}
      marks={marks}
      reveal={shown.line}
      onGutter={toggle}
      onLineNumber={(line, extend) => {
        const from = extend && shown.selection ? Math.min(shown.selection.line, line) : line;
        const to = extend && shown.selection ? Math.max(shown.selection.end, line) : line;
        look((current) =>
          showingSource(
            current,
            formatPlace({ path: paths.link(shown.path), line: from, end: to }),
          ),
        );
      }}
      onCursor={(line) => tab.setState({ cursor: { path: shown.path, line } })}
      onHover={
        live
          ? (expression) =>
              cache
                .get(connection, "evaluate", { ...live, expression })
                .promise.then((settled) =>
                  settled.ok
                    ? { text: (settled.value as Row).text, type: (settled.value as Row).type }
                    : null,
                )
          : undefined
      }
    />
  );
}
