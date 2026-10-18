// Where a tab goes when the session moves on (D3). A tab that showed the
// latest stop follows the program to the next one, at the thread that
// stopped it. A tab pinned with `p`, or opened on an earlier stop, stays
// where it is and says the stop has passed.

import type { At } from "./focus";
import type { State } from "./protocol";

export type Move = { to: "stop"; at: At } | { to: "session" } | null;

/** The newest stop the session has had, if any. */
export function latestStop(state: State): number | undefined {
  return state.stops.at(-1)?.stop;
}

/**
 * The frame a tab opens on for `thread` at `stop`: on the thread that
 * stopped, the frame the stop selects, which at a runtime agent's stop is
 * the program's frame rather than the agent's; elsewhere the innermost.
 */
export function openingFrame(state: State, stop: number, thread: number): number {
  const entry = state.stops.find((entry) => entry.stop === stop);
  return entry?.thread === thread ? entry.frame : 0;
}

/** Stops whose reason the place they show does not say. */
const EXCEPTIONS = new Set(["exception", "languageException"]);

/**
 * Why the program stopped, to say beside the line of the frame `at` shows,
 * when that is the frame an exception's stop opened on: there the program
 * did not choose to stop, and an uncaught exception's frames that raised it
 * are gone.
 */
export function stopNote(state: State, at: At): string | null {
  const entry = state.stops.find((candidate) => candidate.stop === at.stop);
  if (!entry || entry.thread !== at.thread || entry.frame !== at.frame || at.task) {
    return null;
  }
  return EXCEPTIONS.has(entry.reason.kind) ? entry.reason.description : null;
}

/**
 * Where to go now. `followed` is the latest stop as of the state before
 * this one, which a tab that was following was showing.
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
