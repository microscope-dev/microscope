// What each run-control action does in the current state, or why it is
// unavailable. Toolbar buttons and keys both ask here, so they always agree.
// Steps act on the tab's focus: the stop, thread, and frame it shows.

import type { Connection } from "./connection";
import type { At } from "./focus";
import type { Model } from "./model";
import { controls } from "./model";
import type { Inferior, StepKind } from "./protocol";
import { tab } from "./tab";

export const ACTION_NAMES = [
  "continue",
  "pause",
  "kill",
  "restart",
  "over",
  "into",
  "out",
  "instruction",
  "overInstruction",
  "jumpToCursor",
  "stepIntoCall",
] as const;

export type ActionName = (typeof ACTION_NAMES)[number];

type StepName = "over" | "into" | "out" | "instruction" | "overInstruction";

const STEPS: Record<StepName, StepKind> = {
  over: "over",
  into: "into",
  out: "out",
  instruction: "instruction",
  overInstruction: "overInstruction",
};

/** The cursor's line, when it is in the file shown. */
export interface Cursor {
  path: string;
  line: number;
}

export interface Available {
  enabled: true;
  run(connection: Connection): Promise<unknown>;
}

export interface Unavailable {
  enabled: false;
  /** Why, as a tooltip says it. */
  reason: string;
}

export type Action = Available | Unavailable;

const unavailable = (reason: string): Unavailable => ({ enabled: false, reason });

/** `at` is where the tab looks, when it looks at a stop. */
export function action(
  name: ActionName,
  model: Model,
  at: At | null = null,
  cursor: Cursor | null = null,
): Action {
  const state = model.state;
  if (model.link !== "open") {
    return unavailable("not connected to microscope");
  }
  if (!controls(model)) {
    return unavailable("this link can only view the session");
  }
  if (!state?.target) {
    return unavailable("nothing is being debugged");
  }
  if (state.target.kind === "core") {
    return unavailable("a core dump cannot run");
  }
  const inferior = state.inferior;
  const launched = state.target.kind === "launch";
  switch (name) {
    case "over":
    case "into":
    case "out":
    case "instruction":
    case "overInstruction":
    case "jumpToCursor":
    case "stepIntoCall":
      return atStop(name, inferior, at, cursor);
    case "continue":
      if (inferior.state === "stopped") {
        const stop = inferior.stop;
        return { enabled: true, run: (connection) => connection.request("continue", { stop }) };
      }
      if (launched && (inferior.state === "notStarted" || inferior.state === "exited")) {
        return {
          enabled: true,
          run: (connection) => connection.request("continue", { stop: null }),
        };
      }
      return unavailable(inferior.state === "running" ? "already running" : "the program is gone");
    case "pause":
      return inferior.state === "running"
        ? { enabled: true, run: (connection) => connection.request("pause") }
        : unavailable("the program is not running");
    case "kill":
      return inferior.state === "running" || inferior.state === "stopped"
        ? { enabled: true, run: (connection) => connection.request("kill") }
        : unavailable("the program is not running");
    case "restart":
      return launched
        ? { enabled: true, run: (connection) => connection.request("restart") }
        : unavailable("only a launched program can restart");
  }
}

/** An action that moves the stopped thread the tab shows. */
function atStop(
  name: StepName | "jumpToCursor" | "stepIntoCall",
  inferior: Inferior,
  at: At | null,
  cursor: Cursor | null,
): Action {
  if (inferior.state !== "stopped") {
    return unavailable("the program is not stopped");
  }
  // The thread and frame shown must be this stop's.
  if (at && at.stop !== inferior.stop) {
    const verb = name === "jumpToCursor" ? "jump" : "step";
    return unavailable(`this tab shows stop #${at.stop}; go to stop #${inferior.stop} to ${verb}`);
  }
  const stop = inferior.stop;
  const thread = at?.thread ?? inferior.thread;
  const task = at?.task ?? null;
  switch (name) {
    case "jumpToCursor": {
      if (!cursor) {
        return unavailable("put the cursor on the line the thread should resume at");
      }
      const location = `${cursor.path}:${cursor.line}`;
      return {
        enabled: true,
        run: (connection) => connection.request("jump", { stop, thread, task, location }),
      };
    }
    case "stepIntoCall":
      if (!at) {
        return unavailable("this tab shows no stop");
      }
      // The palette lists the line's calls; choosing one steps into it.
      return { enabled: true, run: async () => tab.setState({ palette: "calls" }) };
    default: {
      const kind = STEPS[name];
      const frame = kind === "out" ? (at?.frame ?? 0) : 0;
      return {
        enabled: true,
        run: (connection) => connection.request("step", { stop, thread, task, frame, kind }),
      };
    }
  }
}

/** The label of the continue button: starting and continuing differ. */
export function continueLabel(model: Model): string {
  const inferior = model.state?.inferior;
  return inferior && inferior.state !== "stopped" && inferior.state !== "running"
    ? "Run"
    : "Continue";
}
