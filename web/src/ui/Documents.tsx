// The code at the focus, as panes: a Source pane for each open file, and
// Disassembly, Memory, and Drawings. Source shows the file and lines the link
// names, or else the frame's own line, or, before the program runs, its main
// function (D2). A frame with no source shows its instructions instead. The
// address says which document is shown, and the documents group follows it.

import { useEffect, useLayoutEffect, useMemo, useRef } from "react";

import { assert } from "../assert";
import {
  askedAt,
  breakpointAt,
  knownBreakpointPlaces,
  namesFile,
  toggleBreakpoint,
} from "../breakpoints";
import { cache, useRequest } from "../data";
import { arrange } from "../documents";
import { followedLook, formatPlace, inView, parsePlace, showingSource, type View } from "../focus";
import { stopNote, unreached } from "../follow";
import { controls } from "../model";
import type { Breakpoint, Row } from "../protocol";
import { frameAt } from "../stack";
import { useConnection, useModel } from "../store";
import { closeFile, openFile, shownAt, tab, useTab } from "../tab";
import type { PaneProps } from "../workbench/kinds";
import { change } from "../workbench/store";
import { Banner } from "./Banner";
import { Disassembly } from "./Disassembly";
import { useCards } from "./Drawings";
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
    assert(named.line >= 1, `link selects from line ${named.line}`);
    assert(named.line <= named.end, `link selects lines ${named.line}-${named.end}`);
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

/** The view shown: the address's, or else source, or a sourceless frame's
 * instructions. */
function useView(): { view: View; sourceless: boolean } {
  const { at, trace, look } = useFocus();
  const shown = useShown();
  const frame = frameAt(trace, at);
  const sourceless = !shown && frame !== undefined && !frame.source;
  return { view: look.view ?? (sourceless ? "disassembly" : "source"), sourceless };
}

/** What the document panes' tabs do, given by the session's workspace. */
const documents: {
  show(path: string): void;
  close(path: string): void;
  view(view: View): void;
} = {
  show: () => undefined,
  close: () => undefined,
  view: () => undefined,
};

export const showDocument = (path: string) => documents.show(path);
export const closeDocument = (path: string) => documents.close(path);
export const showView = (view: View) => documents.view(view);

/** Keeps the tab's open files and the documents group in step with the
 * address. Renders nothing. */
export function Documents() {
  const { at, look: current } = useFocus();
  const shown = useShown();
  const own = useOwnSource();
  const { view } = useView();
  const files = useTab((state) => state.files);
  const look = useLook();
  const paths = useLinkPaths();

  const path = shown?.path;
  const line = shown?.line ?? null;
  // Before paint, so a file newly shown never appears without its tab. A
  // file closed opens again at every new stop, when its line moves, or when
  // something asks for it.
  const stop = at?.stop;
  const asked = useTab((state) => state.asked);
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

  useLayoutEffect(() => {
    change((layout) => arrange(layout, files, open && path ? path : null, view));
  }, [files, path, open, view]);

  // The tabs act through these, which close over this render's focus.
  useLayoutEffect(() => {
    // A file's tab follows the focus when the focus shows that file by
    // itself, or else shows where the file was last shown.
    documents.show = (chosen) => {
      const where = tab.getState().places[chosen];
      look(
        (current) =>
          chosen === own?.path
            ? inView(followedLook(current), "source")
            : showingSource(
                current,
                where ?? formatPlace({ path: paths.link(chosen), line: 1, end: 1 }),
              ),
        { replace: false, ask: true },
      );
    };
    // Closing the file shown shows its neighbor, the next one or else the one
    // before it. Closing the last drops it from the address.
    documents.close = (closed) => {
      const index = files.indexOf(closed);
      assert(index >= 0, `closing a file not among the ${files.length} open`);
      const neighbor = files[index + 1] ?? files[index - 1];
      closeFile(closed);
      if (closed !== path) {
        return;
      }
      if (neighbor) {
        documents.show(neighbor);
      } else if (current.src) {
        unasked.current = `${own?.path}:${own?.line ?? null}:${stop}:${asked}`;
        look(({ src: _, ...rest }) => rest, { replace: false });
      }
    };
    documents.view = (chosen) => look((look) => inView(look, chosen), { replace: false });
  });
  return null;
}

/** One file's source, or while none is open, why there is nothing to show. */
export function SourceBody({ pane }: PaneProps) {
  const { at, stale } = useFocus();
  const shown = useShown();
  const path = pane.key;
  return (
    <div className="document">
      <Banner />
      {stale === "running" && at && (
        <div className="view-note">Showing stop #{at.stop} · program running</div>
      )}
      {path === "" ? (
        <Nothing />
      ) : (
        <SourceFile
          shown={
            shown?.path === path
              ? shown
              : { path, line: tab.getState().shown?.line ?? null, selection: null }
          }
        />
      )}
    </div>
  );
}

function Nothing() {
  const { at, trace, state, stale } = useFocus();
  const shown = useShown();
  const frame = frameAt(trace, at);
  return (
    <div className="empty center-message">
      {shown
        ? "No file open."
        : at && frame && !frame.source
          ? `${frame.name} has no source${frame.address ? `: it is at ${frame.address}` : ""}${frame.module ? ` in ${frame.module}` : ""}.`
          : at && !trace
            ? stale !== "passed"
              ? "Reading the stack…"
              : unreached(state, at.stop)
                ? `This session has not reached stop #${at.stop}.`
                : `Stop #${at.stop} has passed.`
            : state.inferior.state === "notStarted"
              ? "Press F5 to run the program."
              : "No source to show."}
    </div>
  );
}

export function DisassemblyBody() {
  const { at, trace } = useFocus();
  const { sourceless } = useView();
  const frame = frameAt(trace, at);
  return (
    <div className="document">
      <Banner />
      {sourceless && frame && (
        <div className="view-note" data-testid="no-source">
          {frame.name} has no source: showing its instructions
        </div>
      )}
      <Disassembly />
    </div>
  );
}

/** How many drawings the stop shows. */
export function DrawingsBadge() {
  const drawings = useCards().length;
  return drawings > 0 ? <span className="badge">{drawings}</span> : null;
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
