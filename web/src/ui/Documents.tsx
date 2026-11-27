import { createEffect, createMemo, Match, Show, Switch } from "solid-js";

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

export interface Shown {
  path: string;

  line: number | null;

  selection: { line: number; end: number } | null;
}

export function useShown(): () => Shown | null {
  const focus = useFocus();
  const paths = useLinkPaths();
  const own = useOwnSource();
  return createMemo(() => {
    const { look } = focus();
    const named = look.src ? parsePlace(look.src) : null;
    if (named) {
      assert(named.line >= 1, `link selects from line ${named.line}`);
      assert(named.line <= named.end, `link selects lines ${named.line}-${named.end}`);
      const path = paths.recorded(named.path);
      return path
        ? { path, line: named.line, selection: { line: named.line, end: named.end } }
        : null;
    }
    return own();
  });
}

function useOwnSource(): () => Shown | null {
  const focus = useFocus();
  const sources = useRequest("sources", () => (focus().state.session ? undefined : null));
  return createMemo(() => {
    const { at, trace } = focus();
    const frame = frameAt(trace, at);
    if (frame?.source) {
      return { path: frame.source.path, line: frame.source.line, selection: null };
    }
    const entry = sources().data?.entry;
    if (!at && entry) {
      return { path: entry.path, line: entry.line, selection: null };
    }
    return null;
  });
}

function useView(): () => { view: View; sourceless: boolean } {
  const focus = useFocus();
  const shown = useShown();
  return createMemo(() => {
    const { at, trace, look } = focus();
    const frame = frameAt(trace, at);
    const sourceless = !shown() && frame !== undefined && !frame.source;
    return { view: look.view ?? (sourceless ? "disassembly" : "source"), sourceless };
  });
}

const documents: {
  show(path: string): void;
  close(path: string): void;
  view(view: View): void;
} = {
  show: () => {},
  close: () => {},
  view: () => {},
};

export const showDocument = (path: string) => documents.show(path);
export const closeDocument = (path: string) => documents.close(path);
export const showView = (view: View) => documents.view(view);

/**
 * Headless bridge keeping URL focus (`at`, `look.src`, `look.view`), open file tabs (`tab.files`),
 * and the dock layout (`arrange`) in sync.
 */
export function Documents() {
  const focus = useFocus();
  const shown = useShown();
  const own = useOwnSource();
  const viewState = useView();
  const files = useTab((state) => state.files);
  const look = useLook();
  const paths = useLinkPaths();
  const asked = useTab((state) => state.asked);

  // Closing the last open tab clears `look.src`, causing `shown()` to fall back to `own()`;
  // `unasked` suppresses re-opening that frame's file on the resulting effect run.
  let unasked: string | null = null;

  const openTrigger = createMemo(
    () => ({
      path: shown()?.path,
      line: shown()?.line ?? null,
      stop: focus().at?.stop,
      asked: asked(),
    }),
    {
      equals: (a, b) =>
        a.path === b.path && a.line === b.line && a.stop === b.stop && a.asked === b.asked,
    },
  );

  createEffect(
    () => openTrigger(),
    ({ path, line, stop, asked: askedCount }) => {
      const skip = unasked === `${path}:${line}:${stop}:${askedCount}`;
      unasked = null;
      if (path && !skip) {
        openFile(path);
      }
    },
  );

  const shownInfo = createMemo(
    () => {
      const currentShown = shown();
      const path = currentShown?.path;
      const line = currentShown?.line ?? null;
      const open = path !== undefined && files().includes(path);
      const place =
        focus().look.src ??
        (path && formatPlace({ path: paths.link(path), line: line ?? 1, end: line ?? 1 }));
      return { path, line, open, place };
    },
    {
      equals: (a, b) =>
        a.path === b.path && a.line === b.line && a.open === b.open && a.place === b.place,
    },
  );

  createEffect(
    () => shownInfo(),
    ({ path, line, open, place }) => {
      tab.setState({ shown: path && open ? { path, line } : null });
      if (path && open && place) {
        shownAt(path, place);
      }
    },
  );

  const arranged = createMemo(
    () => {
      const path = shown()?.path;
      const currentFiles = files();
      const open = path !== undefined && currentFiles.includes(path);
      return { files: currentFiles, active: open && path ? path : null, view: viewState().view };
    },
    {
      equals: (a, b) => a.files === b.files && a.active === b.active && a.view === b.view,
    },
  );

  createEffect(
    () => arranged(),
    ({ files: currentFiles, active, view }) => {
      change((layout) => arrange(layout, currentFiles, active, view));
    },
  );

  documents.show = (chosen) => {
    const where = tab.getState().places[chosen];
    const currentOwn = own();
    look(
      (prev) =>
        chosen === currentOwn?.path
          ? inView(followedLook(prev), "source")
          : showingSource(
              prev,
              where ?? formatPlace({ path: paths.link(chosen), line: 1, end: 1 }),
            ),
      { replace: false, ask: true },
    );
  };

  documents.close = (closed) => {
    const currentFiles = files();
    const index = currentFiles.indexOf(closed);
    assert(index >= 0, `closing a file not among the ${currentFiles.length} open`);
    const neighbor = currentFiles[index + 1] ?? currentFiles[index - 1];
    closeFile(closed);
    if (closed !== shown()?.path) {
      return;
    }
    if (neighbor) {
      documents.show(neighbor);
    } else if (focus().look.src) {
      const currentOwn = own();
      unasked = `${currentOwn?.path}:${currentOwn?.line ?? null}:${focus().at?.stop}:${asked()}`;
      look(({ src: _src, ...rest }) => rest, { replace: false });
    }
  };

  documents.view = (chosen) => look((prev) => inView(prev, chosen), { replace: false });

  return null;
}

export function SourceBody(props: PaneProps) {
  const focus = useFocus();
  const shown = useShown();
  const path = createMemo(() => props.pane.key);
  const lastLine = useTab((state) => state.shown?.line ?? null);
  const fileShown = createMemo(() => {
    const currentShown = shown();
    const currentPath = path();
    return currentShown?.path === currentPath
      ? currentShown
      : { path: currentPath, line: lastLine(), selection: null };
  });

  return (
    <div class="document">
      <Banner />
      <Show when={focus().stale === "running" && focus().at ? focus().at : null}>
        {(at) => <div class="view-note">Showing stop #{at().stop} · program running</div>}
      </Show>
      <Show when={path() !== ""} fallback={<Nothing />}>
        <SourceFile shown={fileShown()} />
      </Show>
    </div>
  );
}

function Nothing() {
  const focus = useFocus();
  const shown = useShown();
  const message = createMemo(() => {
    const { at, trace, state, stale } = focus();
    const frame = frameAt(trace, at);
    if (shown()) {
      return "No file open.";
    }
    if (at && frame && !frame.source) {
      return `${frame.name} has no source${frame.address ? `: it is at ${frame.address}` : ""}${frame.module ? ` in ${frame.module}` : ""}.`;
    }
    if (at && !trace) {
      if (stale === "passed") {
        return unreached(state, at.stop)
          ? `This session has not reached stop #${at.stop}.`
          : `Stop #${at.stop} has passed.`;
      }
      return "Reading the stack…";
    }
    if (state.inferior.state === "notStarted") {
      return "Press F5 to run the program.";
    }
    return "No source to show.";
  });

  return <div class="empty center-message">{message()}</div>;
}

export function DisassemblyBody() {
  const focus = useFocus();
  const viewState = useView();
  const frame = createMemo(() => {
    const { trace, at } = focus();
    return frameAt(trace, at);
  });

  return (
    <div class="document">
      <Banner />
      <Show when={viewState().sourceless ? frame() : null}>
        {(currentFrame) => (
          <div class="view-note" data-testid="no-source">
            {currentFrame().name} has no source: showing its instructions
          </div>
        )}
      </Show>
      <Disassembly />
    </div>
  );
}

export function DrawingsBadge() {
  const cards = useCards();
  return (
    <Show when={cards().length > 0}>
      <span class="badge">{cards().length}</span>
    </Show>
  );
}

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

    const asked = askedAt(breakpoint, path);
    if (knownBreakpointPlaces(breakpoint).length === 0 && asked && !lines.has(asked)) {
      lines.set(asked, "pending");
    }
  }
  return lines;
}

function SourceFile(props: { shown: Shown }) {
  const focus = useFocus();
  const connection = useConnection();
  const control = useModel(controls);
  const paths = useLinkPaths();
  const look = useLook();
  const source = useRequest("source", () => ({ path: props.shown.path }));

  const live = createMemo(() => {
    const { at, stale } = focus();
    return at && !stale ? at : null;
  });
  const frame = createMemo(() => {
    const { trace, at } = focus();
    return frameAt(trace, at);
  });
  const frameSource = createMemo(() => {
    const currentFrame = frame();
    return currentFrame?.source?.path === props.shown.path ? currentFrame.source : null;
  });
  const inline = useInlineValues(
    live,
    () => props.shown.path,
    () => {
      const data = source().data;
      return data?.path === props.shown.path ? data.text : undefined;
    },
    () => (live() ? (frameSource()?.line ?? null) : null),
    () => frameSource()?.first ?? null,
  );

  const marks = createMemo((): Marks => {
    const { at, trace, state } = focus();
    const currentFrame = frame();
    const currentShown = props.shown;
    const innermost = trace?.frames[0];
    const text = at ? stopNote(state, at) : null;
    return {
      breakpoints: breakpointLines(state.breakpoints, currentShown.path),
      breakable: new Set(source().data?.breakable ?? []),
      pc: innermost?.source?.path === currentShown.path ? innermost.source.line : null,
      frame:
        currentFrame && currentFrame.index > 0 && currentFrame.source?.path === currentShown.path
          ? currentFrame.source.line
          : null,
      selection: currentShown.selection,
      inline: inline(),
      note:
        text && currentFrame?.source?.path === currentShown.path
          ? { line: currentFrame.source.line, text }
          : null,
    };
  });

  const toggle = (line: number) => {
    if (!control()) {
      return;
    }
    const path = props.shown.path;
    const existing = breakpointAt(focus().state.breakpoints, path, line);
    void toggleBreakpoint(connection, existing, `${path}:${line}`);
  };

  const select = (line: number, extend: boolean) => {
    const currentShown = props.shown;
    const from =
      extend && currentShown.selection ? Math.min(currentShown.selection.line, line) : line;
    const to = extend && currentShown.selection ? Math.max(currentShown.selection.end, line) : line;
    look((current) =>
      showingSource(
        current,
        formatPlace({ path: paths.link(currentShown.path), line: from, end: to }),
      ),
    );
  };

  const moveCursor = (line: number) => tab.setState({ cursor: { path: props.shown.path, line } });

  const hover = createMemo(() => {
    const currentLive = live();
    return currentLive
      ? (expression: string) =>
          cache
            .get(connection, "evaluate", { ...currentLive, expression })
            .promise.then((settled) =>
              settled.ok
                ? { text: (settled.value as Row).text, type: (settled.value as Row).type }
                : null,
            )
      : undefined;
  });

  return (
    <Switch
      fallback={<div class="empty center-message">Reading {fileName(props.shown.path)}…</div>}
    >
      <Match when={source().error}>
        {(err) => (
          <div class="empty center-message">
            Cannot show {props.shown.path}: {err().message}
          </div>
        )}
      </Match>
      <Match when={source().data?.path === props.shown.path ? source().data : null}>
        {(data) => (
          <SourceView
            path={props.shown.path}
            text={data().text}
            marks={marks()}
            reveal={props.shown.line}
            onGutter={toggle}
            onLineNumber={select}
            onCursor={moveCursor}
            onHover={hover()}
          />
        )}
      </Match>
    </Switch>
  );
}
