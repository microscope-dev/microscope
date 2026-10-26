// Tabs follow the latest stop unless pinned with `p` or showing an older stop.

import type { At } from "./focus";
import type { State } from "./protocol";

export type Move = { to: "stop"; at: At } | { to: "session" } | null;

/** The newest stop the session has had, if any. */
export function latestStop(state: State): number | undefined {
  return state.stops.at(-1)?.stop;
}

/**
 * Use the stop's selected frame for its thread, otherwise the innermost frame.
 * Runtime-agent stops select the program's frame, not the agent's.
 */
export function openingFrame(state: State, stop: number, thread: number): number {
  const entry = state.stops.find((entry) => entry.stop === stop);
  return entry?.thread === thread ? entry.frame : 0;
}

/** Exceptions need an explanation beyond the source location. */
const EXCEPTIONS = new Set(["exception", "languageException"]);

/** Exception details, shown only on the stop's original thread and frame. */
export function stopNote(state: State, at: At): string | null {
  const entry = state.stops.find((candidate) => candidate.stop === at.stop);
  if (!entry || entry.thread !== at.thread || entry.frame !== at.frame || at.task) {
    return null;
  }
  return EXCEPTIONS.has(entry.reason.kind) ? entry.reason.description : null;
}

/**
 * `followed` is the previous state's latest stop, which following tabs showed.
 */
export function follow(
  state: State,
  at: At | null,
  followed: number | undefined,
  pinned: boolean,
): Move {
  const inferior = state.inferior;
  const following = at === null || (!pinned && at.stop === followed);
  if (inferior.state === "stopped") {
    if (at?.stop === inferior.stop || !following) {
      return null;
    }
    return {
      to: "stop",
      at: { stop: inferior.stop, thread: inferior.thread, frame: inferior.frame },
    };
  }
  if (inferior.state === "running" || at === null || !following) {
    return null;
  }
  // The program ended: its last stop is gone with it.
  return { to: "session" };
}
