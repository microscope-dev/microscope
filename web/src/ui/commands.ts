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
import { DOCUMENT_KINDS, documentsGroup, KIND, single } from "../documents";
import { type At, inView, type View, watching } from "../focus";
import type { Model } from "../model";
import { useConnection, useStoreApi } from "../store";
import { flash, type TabState, tab } from "../tab";
import { setRemote, standalone } from "../target";
import { chooseTheme, type Theme } from "../theme";
import {
  besideWhy,
  floatOrDock,
  floatWhy,
  focusBeside,
  focusPane,
  focusTab,
  sendBeside,
  sendWhy,
  shiftTab,
  shiftWhy,
  splitFocused,
  splitWhy,
  type Why,
} from "../workbench/arrange";
import {
  ABSENT,
  type Availability,
  blocked,
  type Command,
  READY,
  registry,
} from "../workbench/commands";
import { NARROW } from "../workbench/Dock";
import type { Chord } from "../workbench/keys";
import {
  activate,
  type Group,
  groups,
  type KindId,
  maximize,
  open,
  type PaneId,
  paneOf,
  restore,
} from "../workbench/layout";
import { MENU_BAR } from "../workbench/MenuBar";
import { change, workbench } from "../workbench/store";
import { chooseTab, closeTab, reopenTab } from "../workbench/tabs";
import { go } from "./navigation";
import type { PickerTab } from "./Picker";
import { applyLayout, DEBUG_KINDS, debugLayout } from "./panes";

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
      void navigate({ to: ".", search: inView(current.look, view) });
    },
  };
}

/** Shows a pane, opening it where it belongs when it is not open, and
 * moves the keyboard into it. */
function showPane(kind: KindId, label: string, number?: number): Spec {
  return {
    label,
    menu: "View",
    keys: number === undefined ? [] : [{ key: String(number), alt: true }],
    available: ({ current }) => (current.session ? READY : ABSENT),
    run: () => {
      let shown: PaneId | null = null;
      change((layout) => {
        const found = paneOf(layout, kind, "");
        if (found) {
          shown = found;
          return activate(layout, found);
        }
        const into = DOCUMENT_KINDS.has(kind)
          ? documentsGroup(layout, tab.getState().files)
          : layout.focused;
        const opened = open(layout, single(kind), into);
        shown = opened.pane;
        return activate(opened.layout, opened.pane);
      });
      // Once the pane is on screen.
      requestAnimationFrame(() => shown && focusPane(shown));
    },
  };
}

/** The active tab of the focused group. */
function focusedPane(): { id: PaneId; group: Group } | null {
  const layout = workbench.getState().layout;
  const group = groups(layout).find((candidate) => candidate.id === layout.focused);
  return group?.active ? { id: group.active, group } : null;
}

/** Shows the tab beside the focused one. */
function nextTab(by: 1 | -1): Spec {
  return {
    label: by === 1 ? "Next tab" : "Previous tab",
    menu: "Window",
    keys: [{ key: by === 1 ? "]" : "[", alt: true }],
    available: () => ((focusedPane()?.group.tabs.length ?? 0) > 1 ? READY : ABSENT),
    run: () => {
      const focused = focusedPane();
      if (!focused) {
        return;
      }
      const { tabs } = focused.group;
      const next = tabs[(tabs.indexOf(focused.id) + by + tabs.length) % tabs.length] as PaneId;
      chooseTab(DEBUG_KINDS, next);
      focusTab(next);
    },
  };
}

/** Starts something new, on the picker's tab for it. */
function start(tab: PickerTab, label: string): Spec {
  return {
    label,
    menu: "File",
    keys: [],
    available: ({ model }) =>
      model.hello?.role === "control"
        ? READY
        : model.hello
          ? blocked("this link can only view the session")
          : ABSENT,
    run: ({ navigate }) => void navigate({ to: "/pick", search: tab === "launch" ? {} : { tab } }),
  };
}

function theme(chosen: Theme, label: string): Spec {
  return {
    label,
    menu: "View",
    keys: [],
    available: always,
    run: () => chooseTheme(chosen),
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

/** A Window menu arrangement, blocked for as long as it says why. */
function arrange(label: string, keys: Chord[], why: () => Why, run: () => void): Spec {
  return {
    label,
    menu: "Window",
    keys,
    available: ({ current }) => {
      if (!current.session) {
        return ABSENT;
      }
      const reason = why();
      return reason === null ? READY : blocked(reason);
    },
    run,
  };
}

const narrow = () => window.innerWidth < NARROW;

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
      const look = watching(current.look, expression);
      if (look !== current.look) {
        void navigate({ to: ".", search: look, replace: true });
      }
      flash(`Watching ${expression}`);
    },
  },
  palette: palette("all", "Command palette", "View", "k"),
  files: palette("files", "Open file", "Go", "p"),
  line: palette("line", "Go to line", "Go", "g"),
  help: {
    label: "Keyboard shortcuts",
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
      void navigate({ to: ".", search: inView(current.look, "source"), replace: true });
    },
  },
  threads: showPane(KIND.threads, "Threads", 1),
  stack: showPane(KIND.stack, "Call Stack", 2),
  breakpoints: showPane(KIND.breakpoints, "Breakpoints", 3),
  watch: showPane(KIND.watch, "Watch", 4),
  variables: showPane(KIND.variables, "Variables", 5),
  console: showPane(KIND.console, "Console", 6),
  registers: showPane(KIND.registers, "Registers", 7),
  tasks: showPane(KIND.tasks, "Tasks"),
  processes: showPane(KIND.processes, "Processes"),
  stops: showPane(KIND.stops, "Stops"),
  output: showPane(KIND.output, "Output"),
  signals: showPane(KIND.signals, "Signals"),
  modules: showPane(KIND.modules, "Modules"),
  viewSource: show("source", "Show source", "s"),
  viewDisassembly: show("disassembly", "Show disassembly", "d"),
  viewMemory: show("memory", "Show memory", "m"),
  viewDrawings: show("drawings", "Show drawings", "v"),
  launch: start("launch", "Launch a program…"),
  attach: start("attach", "Attach to a process…"),
  core: start("core", "Open a core dump…"),
  record: start("profile", "Record a profile…"),
  disconnect: {
    label: "Disconnect",
    menu: "File",
    keys: [],
    available: () => (standalone ? READY : ABSENT),
    run: () => {
      setRemote(null);
      window.location.assign("/");
    },
  },
  themeSystem: theme("system", "Theme: the system's"),
  themeLight: theme("light", "Theme: light"),
  themeDark: theme("dark", "Theme: dark"),
  closeTab: {
    label: "Close tab",
    menu: "Window",
    keys: [{ key: "w", alt: true }],
    available: () => {
      const focused = focusedPane();
      if (!focused) {
        return ABSENT;
      }
      const pane = workbench.getState().layout.panes[focused.id];
      return pane && DEBUG_KINDS.get(pane.kind).closable(pane)
        ? READY
        : blocked("this tab stays open");
    },
    run: () => {
      const focused = focusedPane();
      if (focused) {
        closeTab(DEBUG_KINDS, focused.id);
      }
    },
  },
  nextTab: nextTab(1),
  previousTab: nextTab(-1),
  reopenTab: arrange(
    "Reopen closed tab",
    [{ key: "t", alt: true, shift: true }],
    () => (workbench.getState().closed.length > 0 ? null : "no tab was closed"),
    () => void reopenTab(DEBUG_KINDS),
  ),
  moveTabLeft: arrange(
    "Move tab left",
    [{ key: "[", alt: true, shift: true }],
    () => shiftWhy(-1),
    () => shiftTab(-1),
  ),
  moveTabRight: arrange(
    "Move tab right",
    [{ key: "]", alt: true, shift: true }],
    () => shiftWhy(1),
    () => shiftTab(1),
  ),
  moveToGroupLeft: arrange(
    "Move tab to the group on the left",
    [{ key: "ArrowLeft", alt: true, shift: true }],
    () => (narrow() ? "the window is too narrow" : sendWhy("left")),
    () => sendBeside("left"),
  ),
  moveToGroupRight: arrange(
    "Move tab to the group on the right",
    [{ key: "ArrowRight", alt: true, shift: true }],
    () => (narrow() ? "the window is too narrow" : sendWhy("right")),
    () => sendBeside("right"),
  ),
  focusLeft: arrange(
    "Focus the group on the left",
    [{ key: "ArrowLeft", alt: true }],
    () => besideWhy("left"),
    () => focusBeside("left"),
  ),
  focusRight: arrange(
    "Focus the group on the right",
    [{ key: "ArrowRight", alt: true }],
    () => besideWhy("right"),
    () => focusBeside("right"),
  ),
  focusUp: arrange(
    "Focus the group above",
    [{ key: "ArrowUp", alt: true }],
    () => besideWhy("up"),
    () => focusBeside("up"),
  ),
  focusDown: arrange(
    "Focus the group below",
    [{ key: "ArrowDown", alt: true }],
    () => besideWhy("down"),
    () => focusBeside("down"),
  ),
  splitRight: arrange(
    "Split right",
    [{ key: "\\", ctrl: true }],
    () => (narrow() ? "the window is too narrow" : splitWhy()),
    () => splitFocused("right"),
  ),
  splitDown: arrange(
    "Split down",
    [{ key: "\\", ctrl: true, shift: true }],
    () => (narrow() ? "the window is too narrow" : splitWhy()),
    () => splitFocused("bottom"),
  ),
  floatOrDock: arrange(
    "Float or dock",
    [{ key: "f", alt: true, shift: true }],
    () => floatWhy(narrow()),
    floatOrDock,
  ),
  maximize: {
    label: "Maximize or restore",
    menu: "Window",
    keys: [{ key: "Enter", alt: true }],
    available: ({ current }) => (current.session ? READY : ABSENT),
    run: () =>
      change((layout) =>
        layout.maximized === null ? maximize(layout, layout.focused) : restore(layout),
      ),
  },
  resetLayout: {
    label: "Reset layout",
    menu: "Window",
    keys: [],
    available: ({ current }) => (current.session ? READY : ABSENT),
    run: () => applyLayout(debugLayout()),
  },
  saveLayout: {
    label: "Save layout as…",
    menu: "Window",
    keys: [],
    available: ({ current }) => (current.session ? READY : ABSENT),
    run: () => tab.setState({ palette: "save" }),
  },
  openLayout: {
    label: "Open layout…",
    menu: "Window",
    keys: [],
    available: ({ current }) => (current.session ? READY : ABSENT),
    run: () => tab.setState({ palette: "layouts" }),
  },
  focusMenuBar: {
    label: "Focus the menu bar",
    menu: "View",
    keys: [{ key: "F10", alt: true }],
    available: always,
    run: () => document.getElementById(MENU_BAR)?.focus(),
  },
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
        // What an earlier command said no longer holds.
        tab.setState({ flash: null });
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
