// A profile's views against a server the test plays: what each says while
// its answer is on its way or after it failed, how its rows answer the
// keyboard, and what stays on screen while the connection is down.

import { act, cleanup, render } from "@testing-library/react";
import { type ReactNode, useMemo, useState } from "react";
import { afterEach, expect, it, vi } from "vitest";
import { page, userEvent } from "vitest/browser";
import type { TransportHandlers } from "../src/connection";
import { type ProfileLook, queryOf } from "../src/profile/look";
import {
  type ErrorKind,
  PROTOCOL_VERSION,
  type ProfileKey,
  type ProfileSummary,
  type ProfileThread,
  type ServerMessage,
} from "../src/protocol";
import { createSession, type Session, SessionContext } from "../src/store";
import { ProfileContext, type ProfileFocus } from "../src/ui/profile/context";
import { FlameView } from "../src/ui/profile/FlameView";
import { FunctionsView } from "../src/ui/profile/FunctionsView";
import { Profile } from "../src/ui/profile/ProfilePage";
import { SyscallsView } from "../src/ui/profile/SyscallsView";
import { Timeline } from "../src/ui/profile/Timeline";
import { TreeView } from "../src/ui/profile/TreeView";
import { ZonesView } from "../src/ui/profile/ZonesView";

interface Asked {
  id: number;
  method: string;
  params?: unknown;
}

/** A server the test answers, which can also drop the connection and take
 * it back. */
class Server {
  readonly requests: Asked[] = [];
  readonly #answered = new Set<number>();
  #handlers: TransportHandlers | null = null;
  #started = false;
  readonly session: Session;

  constructor() {
    this.session = createSession({
      connect: (handlers) => {
        this.#handlers = handlers;
        return {
          send: (text) => this.requests.push(JSON.parse(text) as Asked),
          close: () => undefined,
        };
      },
      authorized: async () => true,
      backoff: [0],
    });
  }

  /** Connects, or after a drop, takes the page's next attempt. */
  async open(): Promise<void> {
    if (!this.#started) {
      this.#started = true;
      this.session.connection.start();
    }
    const handlers = await vi.waitFor(() => {
      if (!this.#handlers) {
        throw new Error("the page never connected");
      }
      return this.#handlers;
    });
    handlers.open();
    this.send({
      type: "hello",
      version: PROTOCOL_VERSION,
      connection: 1,
      role: "control",
      name: "tester",
      cwd: "/work",
      share: true,
    });
  }

  drop(): void {
    const handlers = this.#handlers;
    this.#handlers = null;
    act(() => handlers?.close());
  }

  send(message: ServerMessage): void {
    act(() => this.#handlers?.message(JSON.stringify(message)));
  }

  next(method: string): Promise<Asked> {
    return vi.waitFor(() => {
      const found = this.requests.find(
        (request) => request.method === method && !this.#answered.has(request.id),
      );
      if (!found) {
        throw new Error(`nothing asked ${method}`);
      }
      this.#answered.add(found.id);
      return found;
    });
  }

  /** Answers, with the columns sent first as a binary frame. */
  answer(request: Asked, result: unknown, columns?: Uint8Array): void {
    if (columns) {
      const frame = new Uint8Array(8 + columns.byteLength);
      new DataView(frame.buffer).setBigUint64(0, BigInt(request.id), true);
      frame.set(columns, 8);
      act(() => this.#handlers?.binary?.(frame.buffer));
    }
    this.send({ type: "result", id: request.id, result });
  }

  fail(request: Asked, message: string, kind: ErrorKind = "failed"): void {
    this.send({ type: "error", id: request.id, error: { kind, message } });
  }
}

let profiles = 0;

/** A profile no other test has asked about, since answers are cached by
 * what asked for them. */
function summaryOf(change: Partial<ProfileSummary> = {}): ProfileSummary {
  profiles += 1;
  return {
    id: `p${profiles}-${Date.now()}`,
    name: "test.usprof",
    program: null,
    unit: "nanoseconds",
    source: null,
    timed: false,
    samples: 10,
    weight: 10,
    syscalls: 0,
    syscallsLost: 0,
    duration: 1e9,
    threads: [],
    lost: 0,
    throttles: 0,
    truncations: [],
    settings: [],
    recorderCpu: 0,
    recorderLoading: 0,
    gaps: [],
    budgetChanges: [],
    tasks: [],
    interpreted: false,
    snapshots: 0,
    snapshotPause: 0,
    taskSamples: 0,
    snapshotProblems: [],
    waited: 0,
    probes: { events: 0, overwritten: 0, noRing: 0, interrupted: 0, malformed: 0, plotted: 0 },
    zoneSites: [],
    labelKeys: [],
    probeProblems: [],
    ...change,
  };
}

/** Changes the look from outside, as the address bar would. */
let changeLook: (change: (look: ProfileLook) => ProfileLook) => void = () => undefined;

function Harness({
  summary,
  initial,
  children,
}: {
  summary: ProfileSummary;
  initial: ProfileLook;
  children: ReactNode;
}) {
  const [look, setLook] = useState(initial);
  changeLook = (change) => act(() => setLook(change));
  const focus = useMemo<ProfileFocus>(
    () => ({
      profile: summary.id,
      summary,
      look,
      query: { ...queryOf(summary.id, look), revision: 0 },
      unit: summary.unit,
      setLook: (change) => setLook(change),
    }),
    [summary, look],
  );
  return (
    <ProfileContext value={focus}>
      <output data-testid="look">{JSON.stringify(look)}</output>
      <div style={{ width: 900 }}>{children}</div>
    </ProfileContext>
  );
}

async function mount(
  view: ReactNode,
  { summary = summaryOf(), look = {} }: { summary?: ProfileSummary; look?: ProfileLook } = {},
): Promise<Server> {
  const server = new Server();
  await server.open();
  render(
    <SessionContext value={server.session}>
      <Harness summary={summary} initial={look}>
        {view}
      </Harness>
    </SessionContext>,
  );
  return server;
}

const lookShown = (): ProfileLook =>
  JSON.parse(page.getByTestId("look").element().textContent ?? "{}") as ProfileLook;

const key = (name: string, module: string | null = null): ProfileKey => ({
  name,
  kind: "function",
  module,
  file: null,
  line: null,
});

/** A call tree's answer and columns, each node [name, own, total, depth]. */
function treeOf(nodes: [string, number, number, number][]): [unknown, Uint8Array] {
  const names = [...new Set(nodes.map(([name]) => name))];
  const count = nodes.length;
  const bytes = new Uint8Array(count * 24);
  const view = new DataView(bytes.buffer);
  for (const [node, [name, own, total, depth]] of nodes.entries()) {
    view.setFloat64(node * 8, own, true);
    view.setFloat64((count + node) * 8, total, true);
    view.setUint32(count * 16 + node * 4, names.indexOf(name), true);
    view.setUint32(count * 20 + node * 4, depth, true);
  }
  const weight = nodes.filter(([, , , depth]) => depth === 0).reduce((sum, [, , t]) => sum + t, 0);
  return [
    { nodes: count, weight, samples: weight, keys: names.map((name) => key(name)), base: null },
    bytes,
  ];
}

const TREE = treeOf([
  ["main", 0, 10, 0],
  ["work", 2, 6, 1],
  ["leaf", 4, 4, 2],
  ["idle", 4, 4, 1],
  ["thread", 0, 2, 0],
  ["leaf", 2, 2, 1],
]);

/** The name of the row a tree or listbox's keyboard is on. */
function activeName(container: Element): string | undefined {
  const id = container.getAttribute("aria-activedescendant");
  const row = id ? document.getElementById(id) : null;
  return row?.querySelector(".name")?.textContent ?? undefined;
}

afterEach(() => {
  cleanup();
  window.scrollTo(0, 0);
  vi.restoreAllMocks();
});

it("walks a call tree with the arrow keys and selects with Enter", async () => {
  const server = await mount(<TreeView />);
  server.answer(await server.next("profileTree"), ...TREE);
  const tree = page.getByRole("tree", { name: "Call tree" });
  await expect.element(tree.getByRole("treeitem").first()).toBeVisible();
  (tree.element() as HTMLElement).focus();
  const element = tree.element();
  expect(activeName(element)).toBe("main");

  await userEvent.keyboard("{ArrowDown}");
  expect(activeName(element)).toBe("work");
  const work = () => tree.getByRole("treeitem").nth(1);
  await expect.element(work()).toHaveAttribute("aria-expanded", "true");
  // Left closes an open node, then moves to its caller.
  await userEvent.keyboard("{ArrowLeft}");
  await expect.element(work()).toHaveAttribute("aria-expanded", "false");
  await expect.element(tree.getByRole("treeitem")).toHaveLength(5);
  await userEvent.keyboard("{ArrowRight}");
  await expect.element(work()).toHaveAttribute("aria-expanded", "true");
  // Right on an open node moves to its first callee.
  await userEvent.keyboard("{ArrowRight}");
  expect(activeName(element)).toBe("leaf");
  // Left on a leaf moves to its caller, which it then closes.
  await userEvent.keyboard("{ArrowLeft}");
  expect(activeName(element)).toBe("work");
  await userEvent.keyboard("{ArrowLeft}");
  await expect.element(work()).toHaveAttribute("aria-expanded", "false");
  await userEvent.keyboard("{ArrowLeft}");
  expect(activeName(element)).toBe("main");
  await userEvent.keyboard("{End}");
  expect(activeName(element)).toBe("leaf");
  await userEvent.keyboard("{ArrowUp}{Enter}");
  await vi.waitFor(() => expect(lookShown().fn).toBe("thread"));
  await expect
    .element(tree.getByRole("treeitem", { selected: true }))
    .toHaveAccessibleName(/^thread,/);
});

it("names each row's shares for those who cannot see the columns", async () => {
  const server = await mount(<TreeView />);
  server.answer(await server.next("profileTree"), ...TREE);
  await expect
    .element(page.getByRole("treeitem", { name: "main, 83.3% total, 0.00% own" }))
    .toBeVisible();
});

it("says it is reading a call tree, and why it could not", async () => {
  const server = await mount(<TreeView />);
  await expect.element(page.getByText("Reading the call tree…")).toBeVisible();
  server.fail(await server.next("profileTree"), "the profile is cut short");
  await expect.element(page.getByText("the profile is cut short")).toBeVisible();
});

it("walks the functions with the arrow keys, same-named functions included", async () => {
  const errors = vi.spyOn(console, "error");
  const server = await mount(<FunctionsView />);
  await expect.element(page.getByText("Reading functions…")).toBeVisible();
  server.answer(await server.next("profileFunctions"), {
    weight: 10,
    rows: [
      { key: 0, own: 5, total: 10, baseOwn: 0, baseTotal: 0 },
      { key: 1, own: 3, total: 3, baseOwn: 0, baseTotal: 0 },
      { key: 2, own: 2, total: 2, baseOwn: 0, baseTotal: 0 },
    ],
    keys: [key("main"), key("leaf", "liba.so"), key("leaf", "libb.so")],
    base: null,
  });
  const list = page.getByRole("listbox", { name: "Functions" });
  await expect.element(list.getByRole("option")).toHaveLength(3);
  expect(errors.mock.calls.flat().join(" ")).not.toMatch(/same key/);
  (list.element() as HTMLElement).focus();
  await userEvent.keyboard("{ArrowDown}{ArrowDown}");
  expect(activeName(list.element())).toBe("leaf");
  expect(list.element().getAttribute("aria-activedescendant")).toBe(
    list.getByRole("option").nth(2).element().id,
  );
  await userEvent.keyboard("{Enter}");
  await vi.waitFor(() => expect(lookShown().fn).toBe("leaf"));
});

it("says when no function was sampled", async () => {
  const server = await mount(<FunctionsView />);
  server.answer(await server.next("profileFunctions"), {
    weight: 0,
    rows: [],
    keys: [],
    base: null,
  });
  await expect.element(page.getByText("No samples are selected.")).toBeVisible();
});

it("brings each function's own hottest line into view once its lines arrive", async () => {
  const file = (hottest: number) => ({
    path: "/src/app.c",
    first: 1,
    text: Array.from({ length: 300 }, (_, at) => `line ${at + 1}`),
    unread: null,
    lines: [[hottest, 5, 5]],
  });
  const server = await mount(<FunctionsView />, { look: { fn: "first", ann: "lines" } });
  await expect.element(page.getByText("Reading the source lines of first…")).toBeVisible();
  server.answer(await server.next("profileLines"), { weight: 10, files: [file(1)] });
  await expect.element(page.getByText("line 300")).toBeInTheDocument();

  changeLook((look) => ({ ...look, fn: "second" }));
  const asked = await server.next("profileLines");
  server.answer(asked, { weight: 10, files: [file(280)] });
  const hottest = page.getByText("line 280", { exact: true });
  await expect.element(hottest).toBeInTheDocument();
  await vi.waitFor(() => {
    const bounds = hottest.element().getBoundingClientRect();
    expect(bounds.top).toBeGreaterThanOrEqual(0);
    expect(bounds.bottom).toBeLessThanOrEqual(window.innerHeight);
  });
});

it("says it is reading a flame graph, and reads a function's cost from the server", async () => {
  const server = await mount(<FlameView />, { look: { fn: "leaf" } });
  await expect.element(page.getByText("Reading the flame graph…")).toBeVisible();
  server.answer(await server.next("profileTree"), ...TREE);
  server.answer(await server.next("profileFunctions"), {
    weight: 10,
    rows: [{ key: 0, own: 7, total: 8, baseOwn: 0, baseTotal: 0 }],
    keys: [key("leaf")],
    base: null,
  });
  await expect
    .element(page.getByTestId("details").getByText(/80.0% total, 70.0% own/))
    .toBeVisible();
});

it("draws a lane per thread for hundreds of threads", async () => {
  const threads: ProfileThread[] = Array.from({ length: 300 }, (_, row) => ({
    row,
    pid: 1,
    tid: row + 1,
    name: `worker ${row}`,
    samples: 1,
    weight: 1,
    blocked: 0,
    preempted: 0,
  }));
  const summary = summaryOf({ timed: true, threads });
  const server = await mount(
    <div style={{ width: 5000 }}>
      <Timeline />
    </div>,
    { summary },
  );
  const asked = await server.next("profileTimeline");
  const buckets = (asked.params as { buckets: number }).buckets;
  expect(buckets).toBe(2048);
  server.answer(asked, {
    range: [0, 1e9],
    lanes: threads.map((thread) => ({
      thread: thread.row,
      task: null,
      weights: new Array(buckets).fill(1),
      blocked: [],
      preempted: [],
    })),
  });
  const canvas = page.getByRole("img", { name: /Samples and time off CPU/ });
  await vi.waitFor(() => {
    const element = canvas.element() as HTMLCanvasElement;
    const pixels = element.getContext("2d")?.getImageData(0, 0, 4, 18).data ?? [];
    expect([...pixels].some((value) => value > 0)).toBe(true);
  });
});

it("says why the timeline could not be read", async () => {
  const server = await mount(<Timeline />, { summary: summaryOf({ timed: true }) });
  server.fail(await server.next("profileTimeline"), "the timeline is cut short");
  await expect.element(page.getByText("the timeline is cut short")).toBeVisible();
});

it("says it is reading zones, and why it could not", async () => {
  const server = await mount(<ZonesView />);
  await expect.element(page.getByText("Reading zones…")).toBeVisible();
  server.fail(await server.next("profileZones"), "the zones are cut short");
  await expect.element(page.getByText("the zones are cut short")).toBeVisible();
});

const traced = () => summaryOf({ samples: 0, syscalls: 10 });

const ROW = {
  key: "read",
  name: "read",
  calls: 1200,
  errors: 0,
  timed: 1200,
  totalNs: 1_234_567,
  p50Ns: 100,
  p90Ns: 200,
  p99Ns: 300,
  maxNs: 400,
  bytes: 0,
  errnos: [],
};

const SUMMARY = {
  total: 1200,
  calls: 1200,
  errors: 0,
  lost: 0,
  counted: false,
  engine: null,
  processes: 1,
  rows: [ROW],
};

it("selects a call from its name's button, and sorts by a column it names", async () => {
  const server = await mount(<SyscallsView />, { summary: traced() });
  server.answer(await server.next("syscallSummary"), SUMMARY);
  const call = page.getByRole("button", { name: "read", exact: true });
  await expect.element(call).toHaveAttribute("aria-pressed", "false");
  (call.element() as HTMLElement).focus();
  await userEvent.keyboard("{Enter}");
  await expect.element(call).toHaveAttribute("aria-pressed", "true");
  expect(lookShown().sc).toBe("read");
  const sorting = (column: string) =>
    page.getByRole("button", { name: column, exact: true }).element().closest("th")?.ariaSort;
  expect(sorting("total")).toBe("descending");
  expect(sorting("calls")).toBeNull();
  await page.getByRole("button", { name: "name", exact: true }).click();
  await vi.waitFor(() => expect(sorting("name")).toBe("ascending"));
  expect(sorting("total")).toBeNull();
  // Counts are grouped, and a rounded time holds its exact nanoseconds.
  await expect.element(page.getByRole("cell", { name: "1,200" }).first()).toBeVisible();
  await expect.element(page.getByTitle("1,234,567 ns")).toHaveTextContent("1.23 ms");
});

it("says it is reading a call's detail, and why it could not", async () => {
  const server = await mount(<SyscallsView />, { summary: traced(), look: { sc: "read" } });
  server.answer(await server.next("syscallSummary"), SUMMARY);
  const detail = page.getByTestId("syscall-detail");
  await expect.element(detail.getByText("Reading read's calls…")).toBeVisible();
  await expect.element(detail.getByText("No call was timed.")).not.toBeInTheDocument();
  server.fail(await server.next("syscallDetail"), "the trace is cut short");
  await expect.element(detail.getByText("the trace is cut short")).toBeVisible();
  server.fail(await server.next("syscallHeatmap"), "the heat map is cut short");
  await expect.element(detail.getByText("the heat map is cut short")).toBeVisible();
});

it("says why the lanes and the rate could not be read", async () => {
  const server = await mount(<SyscallsView />, { summary: traced() });
  server.fail(await server.next("syscallLanes"), "the lanes are cut short");
  server.fail(await server.next("syscallRate"), "the rate is cut short");
  await expect.element(page.getByText("the lanes are cut short")).toBeVisible();
  await expect.element(page.getByText("the rate is cut short")).toBeVisible();
});

it("says it is reading the log, and why a call could not be read", async () => {
  const server = await mount(<SyscallsView />, { summary: traced(), look: { st: "log" } });
  await expect.element(page.getByText("Reading calls…")).toBeVisible();
  server.answer(await server.next("syscallLog"), {
    total: 1,
    lines: [
      {
        event: 7,
        time: 0,
        tid: 1,
        pid: 1,
        tokens: [{ kind: "name", text: "read" }],
        durationNs: null,
        failed: false,
        call: true,
      },
    ],
  });
  await page.getByRole("button", { name: /read/ }).click();
  server.fail(await server.next("syscallCall"), "the call is cut short");
  await expect.element(page.getByText("the call is cut short")).toBeVisible();
});

it("says why the log could not be read", async () => {
  const server = await mount(<SyscallsView />, { summary: traced(), look: { st: "log" } });
  server.fail(await server.next("syscallLog"), "the log is cut short");
  await expect.element(page.getByText("the log is cut short")).toBeVisible();
});

for (const [tab, method, reading, empty] of [
  ["files", "syscallFiles", "Reading files…", "No file was opened or used."],
  ["network", "syscallNetwork", "Reading sockets…", "No socket was used."],
  ["processes", "syscallProcesses", "Reading processes…", "No process was seen."],
] as const) {
  it(`says it is reading ${tab}, and why it could not, rather than that there are none`, async () => {
    const server = await mount(<SyscallsView />, { summary: traced(), look: { st: tab } });
    await expect.element(page.getByText(reading)).toBeVisible();
    await expect.element(page.getByText(empty)).not.toBeInTheDocument();
    server.fail(await server.next(method), `the ${tab} are cut short`);
    await expect.element(page.getByText(`the ${tab} are cut short`)).toBeVisible();
    await expect.element(page.getByText(empty)).not.toBeInTheDocument();
  });
}

it("says it is reading blocked threads, and why it could not", async () => {
  const summary = traced();
  const server = await mount(<SyscallsView />, { summary, look: { st: "processes" } });
  server.send({
    type: "profiles",
    profiles: [{ id: summary.id, name: "live", recording: true, revision: 0, ended: null }],
  });
  await expect.element(page.getByText("Reading blocked threads…")).toBeVisible();
  server.fail(await server.next("syscallInFlight"), "the threads are gone");
  await expect.element(page.getByText("the threads are gone")).toBeVisible();
});

/** Mounts the whole page of a profile. */
async function mountPage(summary: ProfileSummary, known: boolean): Promise<Server> {
  const server = new Server();
  await server.open();
  if (known) {
    server.send({
      type: "profiles",
      profiles: [
        { id: summary.id, name: summary.name, recording: false, revision: 0, ended: null },
      ],
    });
  }
  function Page() {
    const [look, setLook] = useState<ProfileLook>({});
    return <Profile profile={summary.id} look={look} setLook={(change) => setLook(change)} />;
  }
  render(
    <SessionContext value={server.session}>
      <Page />
    </SessionContext>,
  );
  return server;
}

it("says a profile it does not serve is missing, and any other failure as it is", async () => {
  const missing = await mountPage(summaryOf(), false);
  missing.fail(await missing.next("profile"), "no profile x", "invalid");
  await expect.element(page.getByRole("heading", { name: "No such profile" })).toBeVisible();
  await expect.element(page.getByText("microscope web PROFILE.usprof")).toBeVisible();
  cleanup();

  const broken = await mountPage(summaryOf(), true);
  broken.fail(await broken.next("profile"), "its columns are cut short");
  await expect
    .element(page.getByRole("heading", { name: "Could not read the profile" }))
    .toBeVisible();
  await expect.element(page.getByText(/its columns are cut short/)).toBeVisible();
  await expect.element(page.getByText("microscope web PROFILE.usprof")).not.toBeInTheDocument();
});

it("keeps what it shows while disconnected, and says so", async () => {
  const summary = summaryOf();
  const server = await mountPage(summary, true);
  server.answer(await server.next("profile"), summary);
  server.answer(await server.next("profileTree"), ...TREE);
  const search = page.getByRole("textbox", { name: "Search frames" });
  await userEvent.fill(search, "lea");
  await expect.element(page.getByTestId("matched")).toBeVisible();

  server.drop();
  await expect.element(page.getByText(/^Disconnected/)).toBeVisible();
  await expect.element(search).toHaveValue("lea");
  await expect.element(page.getByRole("img", { name: "Flame graph" })).toBeVisible();

  await server.open();
  await expect.element(page.getByText(/^Disconnected/)).not.toBeInTheDocument();
  await expect.element(search).toHaveValue("lea");
  await expect.element(page.getByTestId("matched")).toBeVisible();
});
