// Tabs follow the program unless pinned or opened on an earlier stop (D3),
// checked against the stops real servers reported.

import { describe, expect, it } from "vitest";
import { follow, latestStop, stopNote } from "../src/follow";
import type { State } from "../src/protocol";
import { received } from "./transcripts";

const states = received("steps").flatMap((message) =>
  message.type === "state" ? [message as State] : [],
);
const stopped = states.filter((state) => state.inferior.state === "stopped");
const at = (state: State) => {
  if (state.inferior.state !== "stopped") {
    throw new Error("not stopped");
  }
  return { stop: state.inferior.stop, thread: state.inferior.thread, frame: 0 };
};

describe("a tab at a stop", () => {
  const [first, second] = stopped as [State, State];

  it("follows the program to its next stop when it showed the latest", () => {
    expect(follow(second, { ...at(first), frame: 2 }, latestStop(first), false)).toEqual({
      to: "stop",
      at: at(second),
    });
  });

  it("opens on the frame the stop selects, as at an agent's Python stop", () => {
    const python = {
      ...second,
      inferior: { ...second.inferior, frame: 3 },
    } as State;
    expect(follow(python, at(first), latestStop(first), false)).toEqual({
      to: "stop",
      at: { ...at(second), frame: 3 },
    });
  });

  it("stays when pinned or opened on an earlier stop", () => {
    expect(follow(second, at(first), latestStop(first), true)).toBeNull();
    const older = { ...at(first), stop: at(first).stop - 1 };
    expect(follow(second, older, latestStop(first), false)).toBeNull();
  });

  it("stays put at the stop it shows, and while the program runs", () => {
    expect(follow(second, { ...at(second), frame: 1 }, latestStop(second), false)).toBeNull();
    const running = states.find(
      (state) => state.inferior.state === "running" && state.stops.length > 0,
    ) as State;
    expect(follow(running, at(first), latestStop(first), false)).toBeNull();
  });

  it("goes to the session once the program is gone", () => {
    const ended = { ...second, inferior: { state: "exited", description: "exited 0" } } as State;
    expect(follow(ended, at(second), latestStop(second), false)).toEqual({ to: "session" });
    expect(follow(ended, at(first), latestStop(second), false)).toBeNull();
  });
});

describe("a tab on the session itself", () => {
  it("goes to whatever stop the program is at", () => {
    const [first] = stopped as [State];
    expect(follow(first, null, undefined, false)).toEqual({ to: "stop", at: at(first) });
    expect(follow(states[0] as State, null, undefined, false)).toBeNull();
  });
});

describe("an exception's stop", () => {
  const [, second] = stopped as [State, State];
  const entry = second.stops.find((candidate) => candidate.stop === at(second).stop);
  const description = "stopped by an unhandled exception: OutOfStock: only 9 gear left";
  const raised = {
    ...second,
    stops: second.stops.map((candidate) =>
      candidate === entry
        ? { ...candidate, frame: 2, reason: { kind: "languageException", description } }
        : candidate,
    ),
  } as State;

  it("says why beside the frame it opened on, and only there", () => {
    expect(stopNote(raised, { ...at(second), frame: 2 })).toBe(description);
    expect(stopNote(raised, { ...at(second), frame: 3 })).toBeNull();
    expect(stopNote(raised, { ...at(second), frame: 2, thread: at(second).thread + 1 })).toBeNull();
  });

  it("says nothing more at a stop whose place says why", () => {
    expect(stopNote(second, at(second))).toBeNull();
  });
});
