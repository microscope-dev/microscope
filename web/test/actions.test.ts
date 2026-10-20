// Jumping and stepping into a call ask the same questions as the toolbar's
// actions, checked against a stop a real server reported.

import { afterEach, describe, expect, it, vi } from "vitest";
import { ACTION_NAMES, action } from "../src/actions";
import { toggleBreakpoint } from "../src/breakpoints";
import type { Connection } from "../src/connection";
import { initialModel, type Model, reduce } from "../src/model";
import type { Breakpoint, State } from "../src/protocol";
import { initialTab, tab } from "../src/tab";
import { movedFrame } from "../src/ui/commands";
import { received } from "./transcripts";

function stoppedModel(): Model {
  let model = initialModel;
  for (const [index, message] of received("steps").entries()) {
    model = reduce(model, { type: "message", message, at: index });
    if (model.state?.inferior.state === "stopped") {
      return model;
    }
  }
  throw new Error("the transcript never stopped");
}

const stopped = stoppedModel();
const state = stopped.state as State;
const inferior = state.inferior as Extract<State["inferior"], { state: "stopped" }>;
const at = { stop: inferior.stop, thread: inferior.thread, frame: 0 };
const cursor = { path: "/src/main.c", line: 7 };

function asking() {
  const asked: { method: string; params: unknown }[] = [];
  const connection = {
    request: (method: string, params?: unknown) => {
      asked.push({ method, params });
      return Promise.resolve({ id: 9 });
    },
  } as unknown as Connection;
  return { asked, connection };
}

afterEach(() => {
  tab.setState(initialTab, true);
});

describe("jumping to the cursor", () => {
  it("is refused on a core dump, as every action is", () => {
    const core = {
      ...stopped,
      state: { ...state, target: { ...state.target, kind: "core" } },
    } as Model;
    expect(action("jumpToCursor", core, at, cursor)).toEqual({
      enabled: false,
      reason: "a core dump cannot run",
    });
  });

  it("is refused while the connection is down", () => {
    expect(action("jumpToCursor", { ...stopped, link: "reconnecting" }, at, cursor)).toEqual({
      enabled: false,
      reason: "not connected to microscope",
    });
  });

  it("is refused at an earlier stop than the program's", () => {
    const earlier = { ...at, stop: at.stop - 1 };
    expect(action("jumpToCursor", stopped, earlier, cursor)).toMatchObject({
      enabled: false,
      reason: `this tab shows stop #${earlier.stop}; go to stop #${at.stop} to jump`,
    });
  });

  it("needs a cursor in the file shown", () => {
    expect(action("jumpToCursor", stopped, at, null)).toMatchObject({
      enabled: false,
      reason: "put the cursor on the line the thread should resume at",
    });
  });

  it("moves the shown thread to the cursor's line", async () => {
    const jump = action("jumpToCursor", stopped, at, cursor);
    if (!jump.enabled) {
      throw new Error(jump.reason);
    }
    const { asked, connection } = asking();
    await jump.run(connection);
    expect(asked).toEqual([
      {
        method: "jump",
        params: { stop: at.stop, thread: at.thread, task: null, location: "/src/main.c:7" },
      },
    ]);
  });
});

describe("stepping into a call", () => {
  it("is refused to a view-only link", () => {
    const viewer = { ...stopped, hello: { ...stopped.hello, role: "view" } } as Model;
    expect(action("stepIntoCall", viewer, at)).toMatchObject({
      enabled: false,
      reason: "this link can only view the session",
    });
  });

  it("opens the palette at the line's calls", async () => {
    const step = action("stepIntoCall", stopped, at);
    if (!step.enabled) {
      throw new Error(step.reason);
    }
    const { asked, connection } = asking();
    await step.run(connection);
    expect(asked).toEqual([]);
    expect(tab.getState().palette).toBe("calls");
  });
});

it("names every action once", () => {
  expect(new Set(ACTION_NAMES).size).toBe(ACTION_NAMES.length);
  expect(ACTION_NAMES).toContain("jumpToCursor");
});

describe("toggling a breakpoint", () => {
  const existing = { id: 4 } as Breakpoint;

  it("adds one where there is none, and removes one where there is", async () => {
    const { asked, connection } = asking();
    await toggleBreakpoint(connection, undefined, "/src/main.c:7");
    await toggleBreakpoint(connection, existing, "/src/main.c:7");
    expect(asked).toEqual([
      { method: "addBreakpoint", params: { location: "/src/main.c:7" } },
      { method: "removeBreakpoint", params: { id: 4 } },
    ]);
  });

  it("says why it failed", async () => {
    const connection = {
      request: vi.fn(() => Promise.reject(new Error("no code at /src/main.c:7"))),
    } as unknown as Connection;
    await toggleBreakpoint(connection, undefined, "/src/main.c:7");
    expect(tab.getState().flash?.text).toBe("no code at /src/main.c:7");
  });
});

describe("moving between frames", () => {
  const top = { stop: 3, thread: 1, frame: 0 };

  it("goes up to callers and back down", () => {
    expect(movedFrame(top, 3, true)).toEqual({ ...top, frame: 1 });
    expect(movedFrame({ ...top, frame: 1 }, 3, false)).toEqual(top);
  });

  it("stops at either end of the stack", () => {
    expect(movedFrame(top, 3, false)).toBeNull();
    expect(movedFrame({ ...top, frame: 2 }, 3, true)).toBeNull();
    expect(movedFrame(top, 0, true)).toBeNull();
  });
});
