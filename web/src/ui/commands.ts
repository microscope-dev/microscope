// What each keyboard command does. Commands act on the tab's focus, which
// the tab store mirrors from the URL.

import { useNavigate } from "@tanstack/react-router";
import { useCallback } from "react";
import { ACTION_NAMES, type ActionName, action } from "../actions";
import { breakpointAt, toggleBreakpoint } from "../breakpoints";
import type { Connection } from "../connection";
import { type At, inView, stringifySearch, type View } from "../focus";
import type { Command } from "../keys";
import type { Model } from "../model";
import { useConnection, useStoreApi } from "../store";
import { flash, tab } from "../tab";
import { go } from "./navigation";

const ACTIONS = new Set<string>(ACTION_NAMES);

const VIEWS: Record<"viewSource" | "viewDisassembly" | "viewMemory" | "viewDrawings", View> = {
  viewSource: "source",
  viewDisassembly: "disassembly",
  viewMemory: "memory",
  viewDrawings: "drawings",
};

/** Runs commands, returning whether the command did anything here. */
export function useCommands(): (command: Command) => boolean {
  const store = useStoreApi();
  const connection = useConnection();
  const navigate = useNavigate();
  return useCallback(
    (command: Command) => {
      const model = store.getState();
      const current = tab.getState();
      if (ACTIONS.has(command)) {
        const cursor =
          current.cursor && current.cursor.path === current.shown?.path ? current.cursor : null;
        const chosen = action(command as ActionName, model, current.at, cursor);
        if (chosen.enabled) {
          void chosen.run(connection).catch((failure: Error) => flash(failure.message));
        } else if (!["continue", "pause"].includes(command) || model.state?.target) {
          flash(chosen.reason);
        }
        return true;
      }
      switch (command) {
        case "toggleBreakpoint":
        case "editBreakpoint":
          breakpoint(command, model, connection);
          return true;
        case "frameUp":
        case "frameDown": {
          const at = current.at && movedFrame(current.at, current.frames, command === "frameUp");
          if (at && current.session) {
            go(navigate, current.session, current.look, at);
          }
          return true;
        }
        case "copyLink":
          void navigator.clipboard
            .writeText(window.location.href)
            .then(() => flash("Copied the link to this view; it carries no access"))
            .catch(() => flash("The browser did not let the page copy"));
          return true;
        case "pin":
          tab.setState((state) => ({ pinned: !state.pinned }));
          flash(
            tab.getState().pinned
              ? "Pinned: this tab stays at its stop"
              : "Unpinned: this tab follows the program",
          );
          return true;
        case "watchSelection": {
          const expression = window.getSelection()?.toString().trim() ?? "";
          if (!expression || expression.includes("\n")) {
            flash("Select an expression in the source, then press w to watch it");
            return true;
          }
          const w = current.look.w ?? [];
          if (!w.includes(expression)) {
            void navigate({
              href:
                window.location.pathname +
                stringifySearch({ ...current.look, w: [...w, expression] }),
              replace: true,
            });
          }
          flash(`Watching ${expression}`);
          return true;
        }
        case "reloadViews":
          void connection.request("reloadViews").catch((failure: Error) => flash(failure.message));
          return true;
        case "viewSource":
        case "viewDisassembly":
        case "viewMemory":
        case "viewDrawings": {
          if (!current.session) {
            return false;
          }
          const view = VIEWS[command];
          void navigate({
            href: window.location.pathname + stringifySearch({ ...inView(current.look, view) }),
          });
          return true;
        }
        case "palette":
        case "files":
        case "line":
          // The palette searches a session's program.
          if (!current.session) {
            return false;
          }
          tab.setState({ palette: command === "palette" ? "all" : command });
          return true;
        case "help":
          tab.setState((state) => ({ help: !state.help }));
          return true;
        case "back": {
          if (current.help) {
            tab.setState({ help: false });
            return true;
          }
          if (current.look.view && current.session) {
            const look = inView(current.look, "source");
            void navigate({
              href: window.location.pathname + stringifySearch({ ...look }),
              replace: true,
            });
            return true;
          }
          return false;
        }
        default: {
          const pane = /^pane(\d)$/.exec(command);
          if (pane) {
            const target = document.querySelector<HTMLElement>(`[data-pane="${pane[1]}"]`);
            target?.focus();
            return target !== null;
          }
          return false;
        }
      }
    },
    [store, connection, navigate],
  );
}

/** The frame one up (toward callers) or down from `at`'s, if the stack of
 * `frames` has it. */
export function movedFrame(at: At, frames: number, up: boolean): At | null {
  const frame = at.frame + (up ? 1 : -1);
  return frame >= 0 && frame < frames ? { ...at, frame } : null;
}

/** Toggles the breakpoint at the cursor, or the line the frame is at. */
function breakpoint(
  command: "toggleBreakpoint" | "editBreakpoint",
  model: Model,
  connection: Connection,
) {
  const current = tab.getState();
  const state = model.state;
  const where =
    current.cursor && current.cursor.path === current.shown?.path
      ? current.cursor
      : current.shown?.line
        ? { path: current.shown.path, line: current.shown.line }
        : null;
  if (!state || !where) {
    flash("Open a file and put the cursor on a line first");
    return;
  }
  if (model.hello?.role !== "control") {
    flash("This link can only view the session");
    return;
  }
  const existing = breakpointAt(state.breakpoints, where.path, where.line);
  if (command === "editBreakpoint") {
    if (existing) {
      tab.setState({ editing: existing.id });
      return;
    }
    connection
      .request("addBreakpoint", { location: `${where.path}:${where.line}` })
      .then((added) => tab.setState({ editing: added.id }))
      .catch((failure: Error) => flash(failure.message));
    return;
  }
  void toggleBreakpoint(connection, existing, `${where.path}:${where.line}`);
}
