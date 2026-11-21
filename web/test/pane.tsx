import { createRouter } from "@solidjs/router";
import type { JSX } from "@solidjs/web";

import { cache } from "../src/data";
import { createMemoryHistory, HistoryContext } from "../src/history";
import type { State } from "../src/protocol";
import { SessionContext } from "../src/store";
import { initialTab, tab } from "../src/tab";
import { type Focus, FocusContext } from "../src/ui/Workspace";
import { type FakeServer, fakeServer } from "./fake-server";
import { cleanup, render } from "./render";

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
    threads: [{ id: 1, name: "basic", stopped: true, labels: [] }],
    breakpoints: [],
    stops: [],
    writes: 0,
    watchpoints: [],
    settings: 0,
    group: null,
    ...changes,
  };
}

export async function mountPane(
  pane: () => JSX.Element,
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
  const history = createMemoryHistory("/");
  const Router = createRouter({
    routes: [
      {
        path: "/",
        component: () => <FocusContext value={() => shown}>{pane()}</FocusContext>,
        children: [{ path: "*all", component: () => null }],
      },
    ],
    history,
  });
  render(() => (
    <SessionContext value={server.session}>
      <HistoryContext value={history}>
        <Router />
      </HistoryContext>
    </SessionContext>
  ));
  return server;
}

export function unmountPane(): void {
  cleanup();
  cache.clear();
  tab.setState(initialTab, true);
}
