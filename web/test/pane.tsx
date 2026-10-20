// Renders a pane as the workspace would: in a session whose server is the
// test, under a router, at a focus the test chooses.

import {
  createMemoryHistory,
  createRootRoute,
  createRouter,
  RouterProvider,
} from "@tanstack/react-router";
import { cleanup, render } from "@testing-library/react";
import type { ReactNode } from "react";
import { cache } from "../src/data";
import type { State } from "../src/protocol";
import { SessionContext } from "../src/store";
import { initialTab, tab } from "../src/tab";
import { type Focus, FocusContext } from "../src/ui/Workspace";
import { type FakeServer, fakeServer } from "./fake-server";

/** A launched program stopped at stop #1 in thread 1. */
export function stoppedState(changes: Partial<State> = {}): State {
  return {
    session: "s1",
    target: { kind: "launch", program: "/work/basic", arguments: [], pid: 100 },
    busy: null,
    revision: 1,
    inferior: {
      state: "stopped",
      pid: 100,
      stop: 1,
      thread: 1,
      reason: { kind: "breakpoint", description: "breakpoint 1" },
      frame: 0,
      place: { address: "0x401000", function: "main", path: "/work/basic.c", line: 3 },
    },
    threads: [{ id: 1, name: "basic", stopped: true }],
    breakpoints: [],
    stops: [],
    writes: 0,
    watchpoints: [],
    settings: 0,
    ...changes,
  };
}

export async function mountPane(
  pane: ReactNode,
  focus: Partial<Focus> = {},
  state: State = stoppedState(),
): Promise<FakeServer> {
  const server = await fakeServer();
  server.send({ type: "state", ...state });
  const shown: Focus = {
    session: "s1",
    state,
    at: { stop: 1, thread: 1, frame: 0 },
    look: {},
    trace: undefined,
    stale: null,
    ...focus,
  };
  const root = createRootRoute({
    component: () => <FocusContext value={shown}>{pane}</FocusContext>,
  });
  const router = createRouter({
    routeTree: root,
    history: createMemoryHistory({ initialEntries: ["/"] }),
  });
  render(
    <SessionContext value={server.session}>
      <RouterProvider router={router as never} />
    </SessionContext>,
  );
  return server;
}

/** Forgets what one test left: its page, its answers, and its tab. */
export function unmountPane(): void {
  cleanup();
  cache.clear();
  tab.setState(initialTab, true);
}
