// The debugger's commands, their keys, and what each does. Keys are VS
// Code's debugger keys, which a page can claim (F5 does not reload, F11 does
// not go fullscreen), and letters that work whenever no text box has focus,
// for keyboards without function keys. Commands act on the tab's focus,
// which the tab store mirrors from the URL.

import { useNavigate } from "@tanstack/react-router";
import { useCallback } from "react";
import { type ActionName, action } from "../actions";
import { breakpointAt, toggleBreakpoint } from "../breakpoints";
import type { Connection } from "../connection";
import { type At, inView, stringifySearch, type View } from "../focus";
import type { Model } from "../model";
import { useConnection, useStoreApi } from "../store";
import { flash, type TabState, tab } from "../tab";
import {
  ABSENT,
  type Availability,
  blocked,
  type Command,
  READY,
  registry,
} from "../workbench/commands";
import type { Chord } from "../workbench/keys";
import { go } from "./navigation";

interface Context {
  model: Model;
  /** The tab's state, as it is when the command runs. */
  current: TabState;
  connection: Connection;
  navigate: ReturnType<typeof useNavigate>;
}

type Spec = Command<Context>;

/** A run-control action, as the toolbar offers it too. */
function act(name: ActionName, label: string, menu: "Run" | "Debug", keys: Chord[]): Spec {
  return {
    label,
    menu,
    keys,
    available: ({ model, current }) => {
      const chosen = action(name, model, current.at, cursorShown(current));
      if (chosen.enabled) {
        return READY;
      }
      // With nothing to debug, run keys are not the debugger's to claim.
      const moot = (name === "continue" || name === "pause") && !model.state?.target;
      return moot ? ABSENT : blocked(chosen.reason);
    },
    run: ({ model, current, connection }) => {
      const chosen = action(name, model, current.at, cursorShown(current));
      if (chosen.enabled) {
        void chosen.run(connection).catch((failure: Error) => flash(failure.message));
      }
    },
  };
}

/** Shows one of the session's documents in place of the source. */
function show(view: View, label: string, letter: string): Spec {
  return {
    label,
    menu: "Go",
    keys: [{ key: letter, alt: true }],
    available: ({ current }) => (current.session ? READY : ABSENT),
    run: ({ current, navigate }) => {
      void navigate({
        href: window.location.pathname + stringifySearch({ ...inView(current.look, view) }),
      });
    },
  };
}

/** Focuses one of the panes by its number. */
function pane(number: number, label: string): Spec {
  const target = () => document.querySelector<HTMLElement>(`[data-pane="${number}"]`);
  return {
    label,
    menu: "View",
    keys: [{ key: String(number), alt: true }],
    available: () => (target() ? READY : ABSENT),
    run: () => target()?.focus(),
  };
}

/** Opens the palette at everything, a session's files, or a line. */
function palette(
  mode: "all" | "files" | "line",
  label: string,
  menu: "View" | "Go",
  letter: string,
): Spec {
  return {
    label,
    menu,
    keys: [{ key: letter, ctrl: true }],
    // The palette searches a session's program.
    available: ({ current }) => (current.session ? READY : ABSENT),
    run: () => tab.setState({ palette: mode }),
  };
}

function frame(up: boolean): Spec {
  return {
    label: up ? "Frame up" : "Frame down",
    menu: "Go",
    keys: [{ key: up ? "u" : "d" }],
    available: ({ current }) => {
      if (!current.at || !current.session) {
        return ABSENT;
      }
      return movedFrame(current.at, current.frames, up)
        ? READY
        : blocked(up ? "this is the outermost frame" : "this is the innermost frame");
    },
    run: ({ current, navigate }) => {
      const at = current.at && movedFrame(current.at, current.frames, up);
      if (at && current.session) {
        go(navigate, current.session, current.look, at);
      }
    },
  };
}

function breakpoint(edit: boolean): Spec {
  return {
    label: edit ? "Edit breakpoint condition" : "Toggle breakpoint",
    menu: "Debug",
    keys: edit ? [{ key: "F9", shift: true }] : [{ key: "F9" }, { key: "b" }],
    available: ({ model, current }) => {
      if (!model.state || !breakpointLine(current)) {
        return blocked("open a file and put the cursor on a line first");
      }
      return model.hello?.role === "control"
        ? READY
        : blocked("this link can only view the session");
    },
    run: ({ model, current, connection }) => {
      const where = breakpointLine(current);
      if (!model.state || !where) {
        return;
      }
      const existing = breakpointAt(model.state.breakpoints, where.path, where.line);
      const location = `${where.path}:${where.line}`;
      if (!edit) {
        void toggleBreakpoint(connection, existing, location);
      } else if (existing) {
        tab.setState({ editing: existing.id });
      } else {
        connection
          .request("addBreakpoint", { location })
          .then((added) => tab.setState({ editing: added.id }))
          .catch((failure: Error) => flash(failure.message));
      }
    },
  };
}

const always = (): Availability => READY;

export const COMMANDS = registry({
  continue: act("continue", "Continue", "Run", [{ key: "F5" }, { key: "c" }]),
  pause: act("pause", "Pause", "Run", [{ key: "F6" }]),
  kill: act("kill", "Kill", "Run", [{ key: "F5", shift: true }]),
  restart: act("restart", "Restart", "Run", [{ key: "F5", ctrl: true, shift: true }]),
  over: act("over", "Step over", "Run", [{ key: "F10" }, { key: "n" }]),
  into: act("into", "Step into", "Run", [{ key: "F11" }, { key: "s" }]),
  out: act("out", "Step out", "Run", [{ key: "F11", shift: true }, { key: "f" }]),
  instruction: act("instruction", "Step one instruction", "Run", [{ key: "S", shift: true }]),
  overInstruction: act("overInstruction", "Step over one instruction", "Run", [
    { key: "N", shift: true },
  ]),
  toggleBreakpoint: breakpoint(false),
  editBreakpoint: breakpoint(true),
  jumpToCursor: act("jumpToCursor", "Jump to cursor", "Run", [{ key: "J", shift: true }]),
  stepIntoCall: act("stepIntoCall", "Step into a call…", "Run", [
    { key: "F11", ctrl: true },
    { key: "i" },
  ]),
  frameUp: frame(true),
  frameDown: frame(false),
  copyLink: {
    label: "Copy link",
    menu: "Edit",
    keys: [{ key: "y" }],
    available: always,
    run: () => {
      void navigator.clipboard
        .writeText(window.location.href)
        .then(() => flash("Copied the link to this view; it carries no access"))
        .catch(() => flash("The browser did not let the page copy"));
    },
  },
  pin: {
    label: "Pin this tab",
    menu: "Debug",
    keys: [{ key: "p" }],
    available: always,
    run: () => {
      tab.setState((state) => ({ pinned: !state.pinned }));
      flash(
        tab.getState().pinned
          ? "Pinned: this tab stays at its stop"
          : "Unpinned: this tab follows the program",
      );
    },
  },
  watchSelection: {
    label: "Watch selection",
    menu: "Edit",
    keys: [{ key: "w" }],
    available: always,
    run: ({ current, navigate }) => {
      const expression = window.getSelection()?.toString().trim() ?? "";
      if (!expression || expression.includes("\n")) {
        flash("Select an expression in the source, then press w to watch it");
        return;
      }
      const w = current.look.w ?? [];
      if (!w.includes(expression)) {
        void navigate({
          href:
            window.location.pathname + stringifySearch({ ...current.look, w: [...w, expression] }),
          replace: true,
        });
      }
      flash(`Watching ${expression}`);
    },
  },
  palette: palette("all", "Command palette", "View", "k"),
  files: palette("files", "Open file", "Go", "p"),
  line: palette("line", "Go to line", "Go", "g"),
  help: {
    label: "Every key",
    menu: "Help",
    keys: [{ key: "?", shift: true }],
    available: always,
    run: () => tab.setState((state) => ({ help: !state.help })),
  },
  back: {
    label: "Back to source",
    menu: "Go",
    keys: [{ key: "Escape" }],
    available: ({ current }) =>
      current.help || (current.look.view && current.session) ? READY : ABSENT,
    run: ({ current, navigate }) => {
      if (current.help) {
        tab.setState({ help: false });
        return;
      }
      void navigate({
        href: window.location.pathname + stringifySearch({ ...inView(current.look, "source") }),
        replace: true,
      });
    },
  },
  pane1: pane(1, "Focus threads"),
  pane2: pane(2, "Focus the call stack"),
  pane3: pane(3, "Focus breakpoints"),
  pane4: pane(4, "Focus watches"),
  pane5: pane(5, "Focus variables"),
  pane6: pane(6, "Focus the console and output"),
  pane7: pane(7, "Focus registers"),
  viewSource: show("source", "Show source", "s"),
  viewDisassembly: show("disassembly", "Show disassembly", "d"),
  viewMemory: show("memory", "Show memory", "m"),
  viewDrawings: show("drawings", "Show drawings", "v"),
  reloadViews: {
    label: "Reload views",
    menu: "Debug",
    keys: [],
    available: always,
    run: ({ connection }) => {
      void connection.request("reloadViews").catch((failure: Error) => flash(failure.message));
    },
  },
});

export type CommandId = (typeof COMMANDS.ids)[number];

export interface Commands {
  /** Whether a command can run now, read when asked. */
  availability(id: CommandId): Availability;
  /** Runs a command if it can run now, and says whether it could. */
  run(id: CommandId): Availability;
}

/** Runs commands against the tab's focus as it is when they run. */
export function useCommands(): Commands {
  const store = useStoreApi();
  const connection = useConnection();
  const navigate = useNavigate();
  const context = useCallback(
    (): Context => ({ model: store.getState(), current: tab.getState(), connection, navigate }),
    [store, connection, navigate],
  );
  const availability = useCallback(
    (id: CommandId) => COMMANDS.get(id).available(context()),
    [context],
  );
  const run = useCallback(
    (id: CommandId) => {
      const now = context();
      const command = COMMANDS.get(id);
      const available = command.available(now);
      if (available.state === "ready") {
        command.run(now);
      }
      return available;
    },
    [context],
  );
  return { availability, run };
}

/** The frame one up (toward callers) or down from `at`'s, if the stack of
 * `frames` has it. */
export function movedFrame(at: At, frames: number, up: boolean): At | null {
  const frame = at.frame + (up ? 1 : -1);
  return frame >= 0 && frame < frames ? { ...at, frame } : null;
}

/** The cursor's line, when it is in the file shown. */
function cursorShown(current: TabState): { path: string; line: number } | null {
  return current.cursor && current.cursor.path === current.shown?.path ? current.cursor : null;
}

/** Where F9 acts: the cursor's line, or the line the frame is at. */
function breakpointLine(current: TabState): { path: string; line: number } | null {
  const cursor = cursorShown(current);
  if (cursor) {
    return cursor;
  }
  return current.shown?.line ? { path: current.shown.path, line: current.shown.line } : null;
}
