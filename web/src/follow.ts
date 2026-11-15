import type { At } from "./focus";
import type { State } from "./protocol";

export type Move = { to: "stop"; at: At } | { to: "session" } | null;

export function latestStop(state: State): number | undefined {
  return state.stops.at(-1)?.stop;
}

export function unreached(state: State, stop: number): boolean {
  return stop > (latestStop(state) ?? 0);
}

export function openingFrame(state: State, stop: number, thread: number): number {
  const entry = state.stops.find((each) => each.stop === stop);
  return entry?.thread === thread ? entry.frame : 0;
}

const EXCEPTIONS = new Set(["exception", "languageException"]);

export function stopNote(state: State, at: At): string | null {
  const entry = state.stops.find((candidate) => candidate.stop === at.stop);
  if (!entry || entry.thread !== at.thread || entry.frame !== at.frame || at.task) {
    return null;
  }
  return EXCEPTIONS.has(entry.reason.kind) ? entry.reason.description : null;
}

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

  return { to: "session" };
}
