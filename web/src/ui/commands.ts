// What each keyboard command does. Commands act on the tab's focus, which
// the tab store mirrors from the URL.

import { useNavigate } from "@tanstack/react-router";
import { useCallback } from "react";
import { type ActionName, action } from "../actions";
import { breakpointAt } from "../breakpoints";
import type { Connection } from "../connection";
import { followedLook, inView, stopPath, stringifySearch, type View } from "../focus";
import type { Command } from "../keys";
import type { Model } from "../model";
import { useConnection, useStoreApi } from "../store";
import { flash, tab } from "../tab";

const ACTIONS = new Set<string>([
  "continue",
  "pause",
  "kill",
  "restart",
  "over",
  "into",
  "out",
  "instruction",
  "overInstruction",
]);

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
        const chosen = action(command as ActionName, model, current.at);
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
        case "jumpToCursor":
          jump(model, connection);
          return true;
        case "stepIntoCall":
          stepIntoCall(model);
          return true;
        case "frameUp":
        case "frameDown": {
          const at = current.at;
          if (!at || !current.session) {
            return true;
          }
          const frame = at.frame + (command === "frameUp" ? 1 : -1);
          if (frame >= 0 && frame < current.frames) {
            void navigate({
              href:
                stopPath(current.session, { ...at, frame }) +
                stringifySearch({ ...followedLook(current.look) }),
            });
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

/** Opens the palette at the calls of the shown thread's line, which a
 * step into one of goes into. */
function stepIntoCall(model: Model) {
  const current = tab.getState();
  const inferior = model.state?.inferior;
  const at = current.at;
  if (model.hello?.role !== "control") {
    flash("This link can only view the session");
    return;
  }
  if (inferior?.state !== "stopped" || !at) {
    flash("The program is not stopped");
    return;
  }
  if (at.stop !== inferior.stop) {
    flash(`This tab shows stop #${at.stop}; go to stop #${inferior.stop} to step`);
    return;
  }
  tab.setState({ palette: "calls" });
}

/** Moves the shown thread, without running it, to the cursor's line. */
function jump(model: Model, connection: Connection) {
  const current = tab.getState();
  const inferior = model.state?.inferior;
  const at = current.at;
  if (!current.cursor || current.cursor.path !== current.shown?.path) {
    flash("Put the cursor on the line the thread should resume at");
    return;
  }
  if (model.hello?.role !== "control") {
    flash("This link can only view the session");
    return;
  }
  if (inferior?.state !== "stopped" || !at) {
    flash("The program is not stopped");
    return;
  }
  if (at.stop !== inferior.stop) {
    flash(`This tab shows stop #${at.stop}; go to stop #${inferior.stop} to jump`);
    return;
  }
  const { path, line } = current.cursor;
  connection
    .request("jump", {
      stop: at.stop,
      thread: at.thread,
      task: at.task ?? null,
      location: `${path}:${line}`,
    })
    .catch((failure: Error) => flash(failure.message));
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
  const request = existing
    ? connection.request("removeBreakpoint", { id: existing.id })
    : connection.request("addBreakpoint", { location: `${where.path}:${where.line}` });
  request.catch((failure: Error) => flash(failure.message));
}
