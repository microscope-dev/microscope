// A profile's views against a server the test plays: what each says while
// its answer is on its way or after it failed, how its rows answer the
// keyboard, and what stays on screen while the connection is down.

import { act, cleanup, render } from "@testing-library/react";
import { type ReactNode, useMemo, useState } from "react";
import { afterEach, expect, it, vi } from "vitest";
import { page, userEvent } from "vitest/browser";

import "../src/styles.css";
import { assert } from "../src/assert";
import type { TransportHandlers } from "../src/connection";
import { type ProfileLook, type ProfileView, queryOf } from "../src/profile/look";
import {
  type ErrorKind,
  PROTOCOL_VERSION,
  type ProfileInspection,
  type ProfileKey,
  type ProfileSummary,
  type ProfileThread,
  type ServerMessage,
} from "../src/protocol";
import { createSession, type Session, SessionContext } from "../src/store";
import { ProfileContext, type ProfileFocus } from "../src/ui/profile/context";
import { FlameView } from "../src/ui/profile/FlameView";
import { FunctionsView } from "../src/ui/profile/FunctionsView";
import { Lanes } from "../src/ui/profile/Lanes";
import { Options } from "../src/ui/profile/Options";
import { LaneChip } from "../src/ui/profile/parts";
import { Profile } from "../src/ui/profile/ProfilePage";
import { SyscallsView } from "../src/ui/profile/SyscallsView";
import { TreeView } from "../src/ui/profile/TreeView";
import { ZonesPane } from "../src/ui/profile/ZonesPane";
import { POLL } from "./fake-server";

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
          close: () => {},
        };
      },
      authorized: () => Promise.resolve(true),
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
    }, POLL);
    handlers.open();
    this.send({
      type: "hello",
      version: PROTOCOL_VERSION,
      connection: 1,
      role: "control",
      name: "tester",
      cwd: "/work",
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
    }, POLL);
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
    command: null,
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
    behind: [],
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

// Sized as a dock pane is, so long lists scroll as they do in the page.
const HARNESS_SIZE = { width: 900, height: 800, display: "grid" };
const WIDE = { width: 5000 };
/** 180 pixels of lane names leave 1000 for the lanes. */
const SECOND_WIDE = { width: 1180 };
/** The lanes fill their pane, scrolling past its height. */
const SECOND_WIDE_SHORT = { width: 1180, height: 400, display: "flex" };
// Narrower than the lanes' names, leaving no room to draw.
const NAMES_WIDE = { width: 150 };
const NO_LOOK: ProfileLook = {};

/** Changes the look from outside, as the address bar would. */
let changeLook: (change: (look: ProfileLook) => ProfileLook) => void = () => {};

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
  // oxlint-disable-next-line react/globals -- lets a test change the look from outside
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
      <div style={HARNESS_SIZE}>{children}</div>
    </ProfileContext>
  );
}

async function mount(
  view: ReactNode,
  { summary = summaryOf(), look = NO_LOOK }: { summary?: ProfileSummary; look?: ProfileLook } = {},
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
  const row = id ? document.querySelector(`#${CSS.escape(id)}`) : null;
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
  await vi.waitFor(() => expect(lookShown().fn).toBe("thread"), POLL);
  await expect
    .element(tree.getByRole("treeitem", { selected: true }))
    .toHaveAccessibleName(/^thread,/u);
});

it("names each row's shares for those who cannot see the columns", async () => {
  const server = await mount(<TreeView />);
  server.answer(await server.next("profileTree"), ...TREE);
  await expect
    .element(page.getByRole("treeitem", { name: "main, 0.00% own, 83.3% total" }))
    .toBeVisible();
});

/** A call tree's row by its accessible name. */
const treeRow = (name: RegExp) => page.getByRole("treeitem", { name });

/** A row's share cells, own then total. */
function shares(name: RegExp): HTMLElement[] {
  return [...treeRow(name).element().querySelectorAll<HTMLElement>(".share")];
}

/** A share cell's bar. */
function fillOf(cell: Element | null | undefined): HTMLElement {
  const fill = cell?.querySelector<HTMLElement>(".share-fill");
  assert(fill, "a share cell has a bar");
  return fill;
}

/** How wide a share cell's bar is drawn, in percent of the cell. */
function barWidth(cell: Element | undefined): number {
  return (
    (fillOf(cell).getBoundingClientRect().width / (cell?.getBoundingClientRect().width ?? 1)) * 100
  );
}

/** A bar's drawn color as red, green, blue, and alpha. */
function barColor(cell: Element | undefined): number[] {
  return (getComputedStyle(fillOf(cell)).backgroundColor.match(/[\d.]+/gu) ?? []).map(Number);
}

/** The names a search marks. */
function markedNames(): (string | null)[] {
  return [...document.querySelectorAll(".tree-row .name.matched")].map((name) => name.textContent);
}

it("draws each share as a bar, bolds large own time, and searches as the flame graph does", async () => {
  const server = await mount(<TreeView />);
  server.answer(await server.next("profileTree"), ...TREE);
  await expect.element(treeRow(/^main,/u)).toBeVisible();

  // main is 10 of 12 total and spends none of it itself.
  const [mainOwn, mainTotal] = shares(/^main,/u);
  expect(barWidth(mainTotal)).toBeCloseTo(83.33, 1);
  expect(barWidth(mainOwn)).toBe(0);
  expect(mainOwn?.classList.contains("strong")).toBe(false);
  // The larger share is drawn redder: as much red, less green.
  const [hotRed, hotGreen] = barColor(mainTotal);
  const [coolRed, coolGreen, , coolAlpha] = barColor(shares(/^idle,/u)[1]);
  expect(coolAlpha).toBeGreaterThan(0);
  expect(hotRed).toBeGreaterThanOrEqual(coolRed ?? 0);
  expect(hotGreen).toBeLessThan(coolGreen ?? 0);
  // work spends 2 of 12 itself, over the 5% that marks own time hot.
  expect(shares(/^work,/u)[0]?.classList.contains("strong")).toBe(true);

  // The search marks each leaf, and counts their samples once: 4 + 2 of 12.
  const search = page.getByRole("searchbox", { name: "Search frames" });
  await userEvent.fill(search, "lea");
  await expect.element(page.getByTestId("matched")).toHaveTextContent("matched 50.0% of samples");
  expect(markedNames()).toEqual(["leaf", "leaf"]);

  // A click selects into the search, and Clear lets go of it.
  await treeRow(/^idle,/u).click();
  await expect.element(search).toHaveValue("idle");
  await page.getByRole("button", { name: "Clear" }).click();
  await expect.element(search).toHaveValue("");
  expect(lookShown().fn).toBeUndefined();
  expect(markedNames()).toEqual([]);
});

/** A tree whose one deep match its opening leaves closed: main opens down
 * to a, whose child b holds too little of it to open. */
const HIDDEN = treeOf([
  ["main", 0, 10, 0],
  ["a", 4, 6, 1],
  ["b", 0, 2, 2],
  ["deep", 2, 2, 3],
  ["c", 4, 4, 1],
]);

it("opens the callers of what a search matches, and leaves them as they are after", async () => {
  const server = await mount(<TreeView />);
  server.answer(await server.next("profileTree"), ...HIDDEN);
  const tree = page.getByRole("tree", { name: "Call tree" });
  const deep = treeRow(/^deep,/u);
  await expect.element(tree.getByRole("treeitem")).toHaveLength(4);
  await expect.element(deep).not.toBeInTheDocument();

  const search = page.getByRole("searchbox", { name: "Search frames" });
  await userEvent.fill(search, "dee");
  await expect.element(deep).toBeVisible();
  expect(markedNames()).toEqual(["deep"]);

  // Closing a caller the search opened keeps it closed while the search
  // stands, and a new search opens it again.
  treeRow(/^b,/u).element().querySelector<HTMLElement>(".twist")?.click();
  await expect.element(deep).not.toBeInTheDocument();
  await userEvent.fill(search, "deep");
  await expect.element(deep).toBeVisible();

  // What a search opened stays open once it is cleared.
  await page.getByRole("button", { name: "Clear" }).click();
  await expect.element(search).toHaveValue("");
  await expect.element(deep).toBeVisible();
  expect(markedNames()).toEqual([]);
});

/** A tall tree whose only match is far below, under callers its opening
 * leaves closed: main's 200 callees come first, then a, b, and target. */
const TALL = treeOf([
  ["main", 0, 201, 0],
  ...Array.from({ length: 200 }, (_, at): [string, number, number, number] => [`f${at}`, 1, 1, 1]),
  ["a", 0, 1, 1],
  ["b", 0, 1, 2],
  ["target", 1, 1, 3],
]);
const SHORT = { display: "flex", flexDirection: "column", height: 300 } as const;

/** Whether a row lies wholly inside what its scrolled container shows. */
function scrolledInto(container: Element, row: Element): boolean {
  const outer = container.getBoundingClientRect();
  const inner = row.getBoundingClientRect();
  return inner.top >= outer.top && inner.bottom <= outer.bottom;
}

it("scrolls to the first match of each search, but not as rows open and close after", async () => {
  const server = await mount(
    <div style={SHORT}>
      <TreeView />
    </div>,
  );
  server.answer(await server.next("profileTree"), ...TALL);
  const tree = page.getByRole("tree", { name: "Call tree" });
  await expect.element(treeRow(/^f0,/u)).toBeVisible();
  const body = tree.element();
  expect(body.scrollTop).toBe(0);

  const search = page.getByRole("searchbox", { name: "Search frames" });
  await userEvent.fill(search, "target");
  const target = treeRow(/^target,/u);
  await vi.waitFor(() => expect(scrolledInto(body, target.element())).toBe(true), POLL);
  expect(body.getAttribute("aria-activedescendant")).toBe(target.element().id);
  // The search keeps the focus as the tree moves.
  await expect.element(search).toHaveFocus();

  // Scrolled back up, closing a row does not bring the match back.
  body.scrollTop = 0;
  await expect.element(treeRow(/^main,/u)).toBeVisible();
  treeRow(/^main,/u)
    .element()
    .querySelector<HTMLElement>(".twist")
    ?.click();
  await expect.element(treeRow(/^f0,/u)).not.toBeInTheDocument();
  treeRow(/^main,/u)
    .element()
    .querySelector<HTMLElement>(".twist")
    ?.click();
  await expect.element(treeRow(/^f0,/u)).toBeVisible();
  expect(body.scrollTop).toBe(0);

  // A new search moves again.
  await userEvent.fill(search, "targe");
  await vi.waitFor(() => expect(scrolledInto(body, target.element())).toBe(true), POLL);
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
  expect(errors.mock.calls.flat().join(" ")).not.toMatch(/same key/u);
  (list.element() as HTMLElement).focus();
  await userEvent.keyboard("{ArrowDown}{ArrowDown}");
  expect(activeName(list.element())).toBe("leaf");
  expect(list.element().getAttribute("aria-activedescendant")).toBe(
    list.getByRole("option").nth(2).element().id,
  );
  await userEvent.keyboard("{Enter}");
  await vi.waitFor(() => expect(lookShown().fn).toBe("leaf"), POLL);
});

it("finds functions by name, and bolds the ones whose own time is hot", async () => {
  const server = await mount(<FunctionsView />);
  server.answer(await server.next("profileFunctions"), {
    weight: 10,
    rows: [
      { key: 0, own: 5, total: 10, baseOwn: 0, baseTotal: 0 },
      { key: 1, own: 3, total: 3, baseOwn: 0, baseTotal: 0 },
      { key: 2, own: 1, total: 1, baseOwn: 0, baseTotal: 0 },
      // Just at the 5% that makes own time hot, and just under it.
      { key: 3, own: 0.5, total: 0.5, baseOwn: 0, baseTotal: 0 },
      { key: 4, own: 0.49, total: 0.49, baseOwn: 0, baseTotal: 0 },
    ],
    keys: [key("main"), key("leaf", "liba.so"), key("leaf", "libb.so"), key("edge"), key("under")],
    base: null,
  });
  const list = page.getByRole("listbox", { name: "Functions" });
  const options = list.getByRole("option");
  await expect.element(options).toHaveLength(5);
  const hot = () =>
    [...list.element().querySelectorAll(".tree-row")]
      .filter((row) => row.querySelector(".share")?.classList.contains("strong"))
      .map((row) => row.querySelector(".name")?.textContent);
  expect(hot()).toEqual(["main", "leaf", "leaf", "edge"]);
  // The own bar is half main's cell, as main's own is half the samples.
  expect(barWidth(list.element().querySelector(".tree-row .share") ?? undefined)).toBeCloseTo(
    50,
    1,
  );

  // A search keeps only the functions whose names hold it, in any case.
  const search = page.getByRole("searchbox", { name: "Search frames" });
  await userEvent.fill(search, "LEA");
  await expect.element(options).toHaveLength(2);
  await expect.element(page.getByTestId("matched")).toHaveTextContent("2 of 5 functions match");
  await userEvent.fill(search, "zzz");
  await expect.element(page.getByText("No function matches.")).toBeVisible();

  // Selecting one shows its name in the search, and every function again.
  await userEvent.fill(search, "ma");
  await options.first().click();
  await expect.element(search).toHaveValue("main");
  await expect.element(options).toHaveLength(5);
  await page.getByRole("button", { name: "Clear" }).click();
  await expect.element(search).toHaveValue("");
  expect(lookShown().fn).toBeUndefined();
  await expect.element(options).toHaveLength(5);
});

it("orders functions by own until asked to order them by total", async () => {
  const server = await mount(<FunctionsView />);
  server.answer(await server.next("profileFunctions"), {
    weight: 10,
    rows: [
      { key: 0, own: 5, total: 5, baseOwn: 0, baseTotal: 0 },
      { key: 1, own: 1, total: 9, baseOwn: 0, baseTotal: 0 },
    ],
    keys: [key("busy"), key("caller")],
    base: null,
  });
  const list = page.getByRole("listbox", { name: "Functions" });
  const names = () =>
    [...list.element().querySelectorAll(".tree-row .name")].map((name) => name.textContent);
  await expect.element(list.getByRole("option")).toHaveLength(2);
  expect(names()).toEqual(["busy", "caller"]);
  await expect
    .element(page.getByRole("button", { name: "by own" }))
    .toHaveAttribute("aria-pressed", "true");

  await page.getByRole("button", { name: "by total" }).click();
  await vi.waitFor(() => expect(names()).toEqual(["caller", "busy"]), POLL);
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

const file = (hottest: number) => ({
  path: "/src/app.c",
  first: 1,
  text: Array.from({ length: 300 }, (_, at) => `line ${at + 1}`),
  unread: null,
  lines: [[hottest, 5, 5]],
});

it("brings each function's own hottest line into view once its lines arrive", async () => {
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
  }, POLL);
});

/** The share cells of the row that shows `text`, own then total. */
function annotatedShares(text: string): HTMLElement[] {
  const row = page.getByText(text, { exact: true }).element().closest(".annotated-row, .code-row");
  return [...(row?.querySelectorAll<HTMLElement>(".share") ?? [])];
}

/** The text of the profile source spans colored as `token`. */
const colored = (token: string) =>
  [...document.querySelectorAll(`[data-testid="source-lines"] .${token}`)].map(
    (span) => span.textContent,
  );

it("colors each file's source lines in that file's language, as the debugger does", async () => {
  const server = await mount(<FunctionsView />, { look: { fn: "spin", ann: "lines" } });
  server.answer(await server.next("profileLines"), {
    weight: 10,
    files: [
      { ...file(1), text: ["return 0; /* a", "b */"], lines: [[1, 1, 1]] },
      { ...file(1), path: "/src/spin.py", text: ["def spin(): pass  # c"], lines: [[1, 1, 1]] },
      { ...file(1), path: "/src/NOTES", text: ["return 0;"], lines: [[1, 1, 1]] },
    ],
  });
  await expect.element(page.getByText("def")).toBeVisible();
  expect(colored("tk-k")).toEqual(["return", "def", "pass"]);
  expect(colored("tk-c")).toEqual(["/* a", "b */", "# c"]);
  expect(colored("tk-f")).toEqual(["spin"]);
});

it("draws a line's and an instruction's shares as bars, bold where own time is hot", async () => {
  const server = await mount(<FunctionsView />, { look: { fn: "spin", ann: "lines" } });
  server.answer(await server.next("profileLines"), {
    weight: 10,
    files: [
      {
        ...file(3),
        text: ["one", "two", "three", "four", "five"],
        lines: [
          [3, 1, 4],
          [5, 0.2, 0.2],
        ],
      },
    ],
  });
  await expect.element(page.getByText("three", { exact: true })).toBeVisible();
  // Line 3 spends 1 of 10 itself, and 4 of 10 under it.
  const [threeOwn, threeTotal] = annotatedShares("three");
  expect(threeOwn).toHaveTextContent("10.0%");
  expect(barWidth(threeOwn)).toBeCloseTo(10, 1);
  expect(barWidth(threeTotal)).toBeCloseTo(40, 1);
  expect(threeOwn?.classList.contains("strong")).toBe(true);
  expect(annotatedShares("five")[0]?.classList.contains("strong")).toBe(false);
  // A line nothing ran on has no number and no bar.
  const [oneOwn] = annotatedShares("one");
  expect(oneOwn).toHaveTextContent("");
  expect(barWidth(oneOwn)).toBe(0);

  changeLook((look) => ({ ...look, ann: "code" }));
  server.answer(await server.next("profileCode"), {
    weight: 10,
    module: "app",
    blocks: [
      {
        symbol: "spin",
        stopped: null,
        instructions: [
          {
            address: 0x10,
            tokens: [["mnemonic", "cmp"]],
            target: null,
            own: 0,
            total: 9,
            lines: [],
          },
          {
            address: 0x14,
            tokens: [["mnemonic", "nop"]],
            target: null,
            own: 0,
            total: 0,
            lines: [],
          },
        ],
      },
    ],
    unplaced: [],
    unavailable: null,
    functions: [],
    files: [],
    unlined: "app's debug information has no source lines for this code",
    inlined_into: null,
  });
  await expect.element(page.getByText("cmp", { exact: true })).toBeVisible();
  // A call's own time is none of it, so it has a total bar alone.
  const [cmpOwn, cmpTotal] = annotatedShares("cmp");
  expect(cmpOwn).toHaveTextContent("");
  expect(barWidth(cmpTotal)).toBeCloseTo(90, 1);
  expect(barWidth(annotatedShares("nop")[1])).toBe(0);
});

/** scan, whose loop runs step inlined at line 3, which runs get from
 * h.h inlined at its line 4. */
const SPLIT = {
  weight: 20,
  module: "app",
  blocks: [
    {
      symbol: "scan",
      stopped: null,
      instructions: [
        {
          address: 0x10,
          tokens: [
            ["mnemonic", "mov"],
            ["text", " "],
            ["register", "eax"],
            ["punctuation", ","],
            ["text", " "],
            ["number", "1"],
          ],
          target: null,
          own: 1,
          total: 1,
          lines: [[0, 0, 3]],
        },
        {
          address: 0x14,
          tokens: [["mnemonic", "add"]],
          target: null,
          own: 5,
          total: 5,
          lines: [
            [2, 1, 9],
            [1, 0, 4],
            [0, 0, 3],
          ],
        },
        {
          address: 0x18,
          tokens: [
            ["mnemonic", "jne"],
            ["text", " "],
            ["address", "0x10"],
          ],
          target: "scan+0x0",
          own: 0,
          total: 0,
          lines: [[0, 0, 3]],
        },
        {
          address: 0x1c,
          tokens: [["mnemonic", "imul"]],
          target: null,
          own: 2,
          total: 2,
          lines: [
            [1, 0, 4],
            [0, 0, 3],
          ],
        },
        {
          address: 0x20,
          tokens: [["mnemonic", "ret"]],
          target: null,
          own: 0,
          total: 0,
          lines: [[0, 0, 5]],
        },
      ],
    },
  ],
  unplaced: [],
  unavailable: null,
  functions: ["scan", "step", "get"],
  files: [
    {
      path: "/src/app.c",
      first: 1,
      text: ["int sink;", "void scan() {", "  for (;;) sum = step(sum);", "  sum += i * i;", "}"],
      lines: [
        [3, 1, 8],
        [4, 7, 7],
        [5, 0, 0],
      ],
      unread: null,
    },
    {
      path: "/src/h.h",
      first: 8,
      text: ["static int get(int i) {", "  return table[i];", "}"],
      lines: [[9, 5, 5]],
      unread: null,
    },
  ],
  unlined: null,
  inlined_into: null,
} as const;

/** The rows of the listing the chosen line holds, by index. */
const chosenRows = () =>
  [...document.querySelectorAll<HTMLElement>('[aria-label="Assembly"] [aria-selected="true"]')].map(
    (row) => Number(row.dataset.row),
  );
/** The chosen source line's number. */
const chosenLine = () =>
  document.querySelector('[aria-label="Source"] [aria-selected="true"] .line-number')?.textContent;
/** The source row of the line numbered `number` that holds code. */
const lineRow = (number: number) =>
  [...document.querySelectorAll<HTMLElement>('[aria-label="Source"] [data-line]')].find(
    (row) => row.querySelector(".line-number")?.textContent === String(number),
  );
const background = (selector: string) =>
  getComputedStyle(document.querySelector(selector) as Element).backgroundColor;
const tintOf = (selector: string) => document.querySelector<HTMLElement>(selector)?.dataset.tint;
/** An instruction's bracket: its name and part, and whether it holds a
 * chosen instruction. */
const bracketOf = (row: number) => {
  const bracket = document.querySelector<HTMLElement>(`[data-row="${row}"] .inlined-from`);
  return [bracket?.textContent, bracket?.dataset.bracket, bracket?.classList.contains("holds")];
};

/** An instruction row's pieces, each as its class and text. */
const pieces = (row: number) =>
  [...document.querySelectorAll(`[data-row="${row}"] .text > *`)].map(
    (piece) => `${piece.className}:${piece.textContent}`,
  );

it("nests inlined code under the line it was inlined at, brackets it, and steps a line's runs", async () => {
  const server = await mount(<FunctionsView />, { look: { fn: "scan", ann: "code" } });
  server.answer(await server.next("profileCode"), SPLIT);
  // It opens on the hottest line of the program's own code, get's line 9,
  // opening scan's line 3 and step's line 4 above it.
  await expect.element(page.getByText("return table[i];")).toBeVisible();
  expect(chosenLine()).toBe("9");
  expect(chosenRows()).toEqual([1]);
  await expect.element(page.getByTestId("run")).toHaveTextContent("run 1 of 1");
  // No file to choose: every file's code is in the tree.
  expect(document.querySelector('[aria-label="Source file"]')).toBeNull();
  await expect
    .element(page.getByRole("button", { name: "Hide the code inlined at line 3" }))
    .toHaveAttribute("aria-expanded", "true");
  await expect
    .element(page.getByRole("button", { name: "Hide the code inlined at line 4" }))
    .toBeVisible();

  // The source is colored as the debugger colors it.
  expect(lineRow(3)?.querySelector(".tk-k")).toHaveTextContent("for");
  // So are the instructions, by what the decoder says each piece is; a
  // jump's address gives way to a button to its offset.
  expect(pieces(0)).toEqual([
    "tk-mnemonic:mov",
    "tk-text: ",
    "tk-register:eax",
    "tk-punctuation:,",
    "tk-text: ",
    "tk-number:1",
  ]);
  expect(pieces(2)).toEqual(["tk-mnemonic:jne", "tk-text: ", "asm-target:+0"]);
  expect(getComputedStyle(document.querySelector(".tk-register") as Element).color).not.toBe(
    getComputedStyle(document.querySelector(".tk-mnemonic") as Element).color,
  );

  // Code inlined from another function is bracketed with its innermost
  // name of the program's own; scan's own code is not.
  expect(bracketOf(1)).toEqual(["get", "one", true]);
  expect(bracketOf(3)).toEqual(["step", "one", false]);
  expect(bracketOf(0)).toEqual(["", undefined, false]);

  // Lines and their instructions share a tint; a line nothing ran on has none.
  expect(tintOf('[data-row="1"]')).toBe(lineRow(9)?.dataset.tint);
  expect(tintOf('[data-row="0"]')).toBe(lineRow(3)?.dataset.tint);
  expect(tintOf('[data-row="4"]')).toBeUndefined();
  expect(lineRow(5)?.dataset.tint).toBeUndefined();
  expect(background('[data-row="1"]')).not.toBe(background('[data-row="0"]'));

  // A line chooses every instruction under it, inlined or not, in runs
  // the arrows step through and wrap around.
  await userEvent.click(lineRow(4) as HTMLElement);
  expect(chosenLine()).toBe("4");
  expect(chosenRows()).toEqual([1, 3]);
  expect(bracketOf(3)).toEqual(["step", "one", true]);
  await expect.element(page.getByTestId("run")).toHaveTextContent("run 1 of 2");
  await userEvent.click(page.getByRole("button", { name: "Next run" }));
  await expect.element(page.getByTestId("run")).toHaveTextContent("run 2 of 2");
  await userEvent.click(page.getByRole("button", { name: "Next run" }));
  await expect.element(page.getByTestId("run")).toHaveTextContent("run 1 of 2");

  // A line with no code chooses nothing.
  await userEvent.click(page.getByText("int sink;"));
  expect(chosenLine()).toBe("4");

  // Keys move between lines that have code, in the order they are shown.
  await userEvent.keyboard("{ArrowDown}");
  expect(chosenLine()).toBe("9");
  await userEvent.keyboard("{ArrowDown}");
  expect(chosenLine()).toBe("5");
  await userEvent.keyboard("{ArrowDown}");
  expect(chosenLine()).toBe("5");
  await userEvent.keyboard("{ArrowUp}{ArrowUp}");
  expect(chosenLine()).toBe("4");
  await userEvent.keyboard("]");
  await expect.element(page.getByTestId("run")).toHaveTextContent("run 2 of 2");

  // Enter closes the chosen line, and the code inlined there goes.
  await userEvent.keyboard("{ArrowUp}{Enter}");
  expect(chosenLine()).toBe("3");
  await expect.element(page.getByText("return table[i];")).not.toBeInTheDocument();
  await expect
    .element(page.getByRole("button", { name: "Show the code inlined at line 3" }))
    .toHaveAttribute("aria-expanded", "false");

  // An instruction chooses its line, opening the lines above it, and says
  // what it was inlined through.
  await userEvent.click(page.getByText("add", { exact: true }));
  expect(chosenLine()).toBe("9");
  await expect.element(page.getByText("return table[i];")).toBeVisible();
  await expect.element(page.getByText("+4 get h.h:9 ← step app.c:4 ← scan app.c:3")).toBeVisible();

  // A jump goes to the instruction it targets.
  await userEvent.click(page.getByRole("button", { name: "+0", exact: true }));
  expect(chosenLine()).toBe("3");
  await expect.element(page.getByText("+0 scan app.c:3")).toBeVisible();
});

it("marks code with no associated source line as not code, in both panes, and gives it no tint", async () => {
  const server = await mount(<FunctionsView />, { look: { fn: "scan", ann: "code" } });
  const [first, ...rest] = SPLIT.blocks[0].instructions;
  server.answer(await server.next("profileCode"), {
    ...SPLIT,
    blocks: [
      {
        ...SPLIT.blocks[0],
        instructions: [{ ...first, own: 9, total: 9, lines: [[0, 0, 0]] }, ...rest],
      },
    ],
  });
  const marker = await vi.waitFor(() => {
    const row = [
      ...document.querySelectorAll<HTMLElement>('[aria-label="Source"] [data-line]'),
    ].find((each) => each.textContent?.includes("no associated source line"));
    assert(row, "the row of no source line is not drawn");
    return row;
  }, POLL);
  expect(marker.querySelector(".unsourced-note")).toHaveTextContent(
    "no associated source line · 1 instruction",
  );
  expect(marker.classList.contains("unsourced")).toBe(true);
  // The hottest code, yet no hue: its stripes are its mark.
  expect(marker.dataset.tint).toBeUndefined();
  expect(marker.querySelector(".line-number")).toHaveTextContent("");
  expect(document.querySelector('[data-row="0"]')?.classList.contains("unsourced")).toBe(true);
  expect(tintOf('[data-row="0"]')).toBeUndefined();
  expect(document.querySelector('[data-row="1"]')?.classList.contains("unsourced")).toBe(false);

  // It chooses its instructions like any line.
  await userEvent.click(marker);
  expect(chosenRows()).toEqual([0]);
  await expect.element(page.getByText("no line · 1 instruction in 1 run")).toBeVisible();
});

it("draws only the rows near view of a long file and listing, opening on the far line", async () => {
  const count = 3000;
  const server = await mount(<FunctionsView />, { look: { fn: "big", ann: "code" } });
  server.answer(await server.next("profileCode"), {
    ...SPLIT,
    blocks: [
      {
        symbol: "big",
        stopped: null,
        instructions: Array.from({ length: count }, (_, at) => ({
          address: 0x1000 + at * 4,
          tokens: [["mnemonic", "nop"]],
          target: null,
          own: at === count - 1 ? 9 : 0,
          total: at === count - 1 ? 9 : 0,
          // Line 6, far above the line it opens on, has none.
          lines: [[0, 0, at === 5 ? 7 : at + 1]],
        })),
      },
    ],
    functions: ["big"],
    files: [
      {
        path: "/src/big.c",
        first: 1,
        // Line 6 is the widest.
        text: Array.from({ length: count }, (_, at) =>
          at === 5 ? `int wide = ${"x".repeat(400)};` : `line ${at + 1}`,
        ),
        unread: null,
        lines: [[count, 9, 9]],
      },
    ],
  });
  await expect.element(page.getByText(`line ${count}`, { exact: true })).toBeVisible();
  expect(chosenLine()).toBe(String(count));
  expect(chosenRows()).toEqual([count - 1]);

  const source = document.querySelector<HTMLElement>('[aria-label="Source"]');
  const listing = document.querySelector<HTMLElement>('[aria-label="Assembly"]');
  expect(source?.querySelectorAll(".code-row").length).toBeLessThan(200);
  expect(listing?.querySelectorAll("[data-row]").length).toBeLessThan(200);
  // The widest line is not drawn, yet the pane scrolls sideways to it.
  expect(source?.textContent).not.toContain("int wide");
  expect((source?.scrollWidth ?? 0) - (source?.clientWidth ?? 0)).toBeGreaterThan(1000);

  // The first instruction, once scrolled to, chooses its far line, which
  // the source draws and brings into view.
  expect(lineRow(1)).toBeUndefined();
  listing?.scrollTo({ top: 0 });
  const first = await vi.waitFor(() => {
    const row = listing?.querySelector('[data-row="0"]');
    assert(row, "the first instruction is not drawn");
    return row;
  }, POLL);
  await userEvent.click(first);
  expect(chosenLine()).toBe("1");
  await expect.element(page.getByText("line 1", { exact: true })).toBeVisible();
});

it("shows an inlined function's code where it was inlined, opening on its own line", async () => {
  const server = await mount(<FunctionsView />, { look: { fn: "step", ann: "code" } });
  server.answer(await server.next("profileCode"), { ...SPLIT, inlined_into: "scan" });
  await expect
    .element(
      page.getByText(
        "step has no code of its own. Every sample ran it inlined into scan, shown here.",
      ),
    )
    .toBeVisible();
  // Not get's line 9, though it is hotter: step's own.
  expect(chosenLine()).toBe("4");
  expect(chosenRows()).toEqual([1, 3]);
});

it("forgets the line it chose once another function's code arrives", async () => {
  const server = await mount(<FunctionsView />, { look: { fn: "scan", ann: "code" } });
  server.answer(await server.next("profileCode"), SPLIT);
  await expect.element(page.getByText("return table[i];")).toBeVisible();
  await userEvent.click(lineRow(3) as HTMLElement);
  expect(chosenLine()).toBe("3");

  changeLook((look) => ({ ...look, fn: "walk" }));
  server.answer(await server.next("profileCode"), {
    ...SPLIT,
    blocks: [{ ...SPLIT.blocks[0], symbol: "walk" }],
    functions: ["walk", "step", "get"],
  });
  await vi.waitFor(() => expect(chosenLine()).toBe("9"), POLL);
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
    .element(page.getByTestId("details").getByText(/80.0% total, 70.0% own/u))
    .toBeVisible();
});

/** A mouse event at `x`, `y` within `target`'s box. */
function mouseAt(target: Element, type: string, x: number, y: number): void {
  const bounds = target.getBoundingClientRect();
  act(() => {
    target.dispatchEvent(
      new MouseEvent(type, {
        bubbles: true,
        clientX: bounds.left + x,
        clientY: bounds.top + y,
      }),
    );
  });
}

const tip = () => document.querySelector("[data-testid=flame-tip] .mono")?.textContent;

it("forgets the frame it hovered once another tree arrives", async () => {
  const server = await mount(<FlameView />);
  server.answer(await server.next("profileTree"), ...TREE);
  const canvas = page.getByRole("img", { name: "Flame graph" });
  await expect.element(canvas).toBeVisible();
  mouseAt(canvas.element(), "mousemove", 10, 5);
  await vi.waitFor(() => expect(tip()).toBe("main"), POLL);
  changeLook((look) => ({ ...look, invert: "1" }));
  server.answer(await server.next("profileTree"), ...treeOf([["other", 1, 1, 0]]));
  await expect.element(page.getByTestId("flame-tip")).not.toBeInTheDocument();
});

it("puts a clicked frame in the search, and Clear or typing lets it go", async () => {
  const server = await mount(<FlameView />);
  server.answer(await server.next("profileTree"), ...TREE);
  const canvas = page.getByRole("img", { name: "Flame graph" });
  const search = page.getByRole("searchbox", { name: "Search frames" });
  await expect.element(search).toHaveAttribute("placeholder", "Click or search for a frame");
  await expect.element(canvas).toBeVisible();
  mouseAt(canvas.element(), "click", 10, 23);
  await expect.element(search).toHaveValue("work");
  await expect.element(page.getByTestId("details").getByText("Focus")).toBeVisible();
  expect(lookShown().fn).toBe("work");

  await page.getByRole("button", { name: "Clear" }).click();
  await expect.element(search).toHaveValue("");
  await expect.element(search).toHaveFocus();
  await expect.element(page.getByTestId("details")).not.toBeInTheDocument();
  expect(lookShown().z).toBeUndefined();
  expect(lookShown().fn).toBeUndefined();

  // Typing over a selected frame searches instead, keeping the zoom.
  mouseAt(canvas.element(), "click", 10, 23);
  await expect.element(search).toHaveValue("work");
  await userEvent.fill(search, "lea");
  await expect.element(page.getByTestId("matched")).toHaveTextContent("matched 50.0% of samples");
  await expect.element(search).toHaveFocus();
  expect(lookShown().fn).toBeUndefined();
  expect(lookShown().z).toBeDefined();

  // Emptying the search, as after selecting all and deleting, clears.
  mouseAt(canvas.element(), "click", 10, 23);
  await expect.element(search).toHaveValue("work");
  await userEvent.clear(search);
  await expect.element(page.getByRole("button", { name: "Clear" })).not.toBeInTheDocument();
  expect(lookShown().fn).toBeUndefined();
  expect(lookShown().z).toBeUndefined();

  // A search typed with nothing zoomed or selected has a Clear of its own.
  await userEvent.fill(search, "lea");
  await page.getByRole("button", { name: "Clear" }).click();
  await expect.element(search).toHaveValue("");
  await expect.element(page.getByTestId("matched")).not.toBeInTheDocument();
});

it("lets go of the selection once Focus, Merge, or Drop reshapes the graph", async () => {
  for (const action of ["Focus", "Merge", "Drop"]) {
    const server = await mount(<FlameView />, { look: { fn: "work" } });
    server.answer(await server.next("profileTree"), ...TREE);
    const search = page.getByRole("searchbox", { name: "Search frames" });
    await expect.element(search).toHaveValue("work");
    await page.getByTestId("details").getByRole("button", { name: action }).click();
    expect(lookShown().tf).toHaveLength(1);
    expect(lookShown().fn).toBeUndefined();
    await expect.element(search).toHaveValue("");
    cleanup();
  }
});

/** Resolves a frame after now, once the effects of a change have run. */
function settled(): Promise<void> {
  return new Promise((done) => {
    requestAnimationFrame(() => {
      setTimeout(done, 0);
    });
  });
}

it("paints the lanes again when they change, not when a function is chosen", async () => {
  const server = await mount(
    <div style={WIDE}>
      <Lanes />
    </div>,
    { summary: summaryOf({ timed: true }) },
  );
  const asked = await server.next("profileTimeline");
  server.answer(asked, { range: [0, 1e9], lanes: [] });
  const painted = vi.spyOn(HTMLCanvasElement.prototype, "getContext");
  // Once its answer and its width have arrived, the lanes are painted and
  // stay so for a frame.
  await vi.waitFor(async () => {
    expect(painted).toHaveBeenCalled();
    painted.mockClear();
    await settled();
    expect(painted).not.toHaveBeenCalled();
  }, POLL);

  changeLook((look) => ({ ...look, fn: "leaf", ann: "code" }));
  await settled();
  expect(painted).not.toHaveBeenCalled();

  changeLook((look) => ({ ...look, lanes: "tasks" }));
  await vi.waitFor(() => expect(painted).toHaveBeenCalled(), POLL);
  painted.mockRestore();
});

it("draws a lane per thread for hundreds of threads", async () => {
  const threads: ProfileThread[] = Array.from({ length: 300 }, (_, row) => ({
    row,
    pid: 1,
    tid: row + 1,
    name: `worker ${row}`,
    samples: 1,
    weight: 1,
    zones: 0,
    blocked: 0,
    preempted: 0,
  }));
  const summary = summaryOf({ timed: true, threads });
  const server = await mount(
    <div style={WIDE}>
      <Lanes />
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
      weights: Array.from({ length: buckets }, () => 1),
      blocked: [],
      preempted: [],
    })),
  });
  const canvas = page.getByRole("img", { name: /^CPU over/u });
  await vi.waitFor(() => {
    const element = canvas.element() as HTMLCanvasElement;
    const pixels = element.getContext("2d")?.getImageData(0, 0, 4, 18).data ?? [];
    expect([...pixels].some((value) => value > 0)).toBe(true);
  }, POLL);
});

/** The ruler's labels: round times, [pills], and a range's <length>. */
const ruler = () =>
  [...document.querySelectorAll("[data-testid=ruler] .ruler-marks > *")].map((mark) =>
    mark.classList.contains("marker")
      ? `[${mark.textContent}]`
      : mark.classList.contains("ruler-measure")
        ? `<${mark.textContent}>`
        : mark.textContent,
  );

function pointerAt(target: Element, type: string, x: number): void {
  const bounds = target.getBoundingClientRect();
  act(() => {
    target.dispatchEvent(
      new PointerEvent(type, { bubbles: true, clientX: bounds.left + x, clientY: bounds.top + 4 }),
    );
  });
}

it("marks round times, the pointer's time, and a range's ends and length", async () => {
  const server = await mount(
    <div style={SECOND_WIDE}>
      <Lanes />
    </div>,
    { summary: summaryOf({ timed: true }) },
  );
  await server.next("profileTimeline");
  const all = [
    "0",
    "0.1 s",
    "0.2 s",
    "0.3 s",
    "0.4 s",
    "0.5 s",
    "0.6 s",
    "0.7 s",
    "0.8 s",
    "0.9 s",
    "1.00 s",
  ];
  await vi.waitFor(() => expect(ruler()).toEqual(all), POLL);

  // The pointer's time hides the round times either side of it.
  const canvas = page.getByRole("img", { name: /^CPU over/u }).element();
  pointerAt(canvas, "pointermove", 250);
  expect(ruler()).toEqual([
    ...all.filter((label) => !["0.2 s", "0.3 s"].includes(label)),
    "[0.250 s]",
  ]);
  expect(document.querySelector(".timeline-cursor")).not.toBeNull();
  pointerAt(canvas, "pointerout", 250);
  expect(ruler()).toEqual(all);
  expect(document.querySelector(".timeline-cursor")).toBeNull();

  // A range's ends sit outside it, with its length measured between them,
  // and no round times inside it.
  changeLook((look) => ({ ...look, range: "200000000-700000000" }));
  expect(ruler()).toEqual([
    "0",
    "0.1 s",
    "0.8 s",
    "0.9 s",
    "1.00 s",
    "<0.500 s>",
    "[0.200 s]",
    "[0.700 s]",
  ]);

  // The pointer's time stands in for the length it would cover.
  pointerAt(canvas, "pointermove", 450);
  expect(ruler()).toEqual([
    "0",
    "0.1 s",
    "0.8 s",
    "0.9 s",
    "1.00 s",
    "<>",
    "[0.200 s]",
    "[0.700 s]",
    "[0.450 s]",
  ]);
  pointerAt(canvas, "pointerout", 450);

  // A range too narrow to hold its length still names its ends.
  changeLook((look) => ({ ...look, range: "200000000-250000000" }));
  expect(ruler()).toEqual([
    "0",
    "0.1 s",
    "0.4 s",
    "0.5 s",
    "0.6 s",
    "0.7 s",
    "0.8 s",
    "0.9 s",
    "1.00 s",
    "[0.200 s]",
    "[0.250 s]",
  ]);

  // The pointer at an end, as after a drag across the lanes, stands in for
  // that end's pill, which it would cover.
  pointerAt(canvas, "pointermove", 252);
  expect(ruler().filter((mark) => mark.startsWith("["))).toEqual(["[0.200 s]", "[0.252 s]"]);
});

it("names the lanes it cuts samples to with the other chips, not over the lanes", async () => {
  const thread = { pid: 1, samples: 1, weight: 1, zones: 0, blocked: 0, preempted: 0 };
  const threads: ProfileThread[] = [
    { ...thread, row: 0, tid: 10, name: "main" },
    { ...thread, row: 1, tid: 11, name: "worker" },
  ];
  await mount(
    <>
      <Lanes />
      <LaneChip />
    </>,
    { summary: summaryOf({ timed: true, threads }) },
  );

  await page.getByRole("button", { name: "main 10" }).click();
  await page.getByRole("button", { name: "worker 11" }).click();
  await expect.element(page.getByText("2 threads")).toBeVisible();
  expect(page.getByTestId("ruler").element().textContent).not.toContain("2 threads");
  expect(lookShown().th).toEqual(["0", "1"]);

  await page.getByRole("button", { name: "Remove 2 threads" }).click();
  await expect.element(page.getByText("2 threads")).not.toBeInTheDocument();
  expect(lookShown().th).toBeUndefined();
});

it("sets how frames are drawn from Options, and puts it away on Escape or a press outside", async () => {
  await mount(<Options />, { look: { tf: ["focus:main"] } });
  const button = page.getByRole("button", { name: "Options", exact: true });
  await button.click();
  const options = page.getByRole("dialog", { name: "Options" });
  await expect.element(options).toBeVisible();

  // Each view is a pane of its own, chosen by its tab, not here.
  expect(options.getByRole("combobox", { name: "View" }).query()).toBeNull();

  await options.getByRole("combobox", { name: "Frames" }).selectOptions("lines");
  await options.getByRole("checkbox", { name: "Async stacks" }).click();
  expect(lookShown()).toMatchObject({ by: "line", native: "1" });

  // Collapsing recursion is a checkbox. Unchecking it takes out only that
  // transform, keeping one added after it.
  const collapse = options.getByRole("checkbox", { name: "Collapse recursion" });
  await collapse.click();
  expect(lookShown().tf).toEqual(["focus:main", "collapseCycles"]);
  changeLook((look) => ({ ...look, tf: [...(look.tf ?? []), "drop:noise"] }));
  await expect.element(collapse).toBeChecked();
  await collapse.click();
  expect(lookShown().tf).toEqual(["focus:main", "drop:noise"]);

  // Escape puts it away and gives focus back to its button.
  await userEvent.keyboard("{Escape}");
  await expect.element(options).not.toBeInTheDocument();
  expect(document.activeElement).toBe(button.element());

  // So does a press anywhere else.
  await button.click();
  await expect.element(options).toBeVisible();
  act(() => {
    document.body.dispatchEvent(new PointerEvent("pointerdown", { bubbles: true }));
  });
  await expect.element(options).not.toBeInTheDocument();
});

it("offers no Options for a trace of system calls alone", async () => {
  await mount(<Options />, { summary: summaryOf({ samples: 0, syscalls: 10 }) });
  await expect.element(page.getByText("Reading…")).not.toBeInTheDocument();
  expect(document.body.textContent).not.toContain("Options");
});

it("says why the timeline could not be read", async () => {
  const server = await mount(<Lanes />, { summary: summaryOf({ timed: true }) });
  server.fail(await server.next("profileTimeline"), "the timeline is cut short");
  await expect.element(page.getByText("the timeline is cut short")).toBeVisible();
});

/** A zones answer and its columns, each block [start, end, thread, depth,
 * name, zones, flags, blocked, preempted], covering all of its span. */
function zonesOf(names: string[], blocks: number[][], offCpu = false): [unknown, Uint8Array] {
  const count = blocks.length;
  const bytes = new Uint8Array(count * 60);
  const view = new DataView(bytes.buffer);
  for (const [at, block] of blocks.entries()) {
    const [start = 0, end = 0, blocked = 0, preempted = 0] = [
      block[0],
      block[1],
      block[7],
      block[8],
    ];
    for (const [column, value] of [start, end, end - start, blocked, preempted].entries()) {
      view.setFloat64((column * count + at) * 8, value, true);
    }
    for (const [column, value] of block.slice(2, 7).entries()) {
      view.setUint32(count * 40 + (column * count + at) * 4, value, true);
    }
  }
  return [{ range: [0, 1e9], blocks: count, names, marks: [], plots: [], offCpu }, bytes];
}

const ZONED_THREADS: ProfileThread[] = [
  {
    row: 0,
    pid: 1,
    tid: 10,
    name: "main",
    samples: 2,
    weight: 2,
    zones: 0,
    blocked: 0,
    preempted: 0,
  },
  {
    row: 1,
    pid: 1,
    tid: 11,
    name: "worker",
    samples: 1,
    weight: 1,
    zones: 3,
    blocked: 0,
    preempted: 0,
  },
];

const DRAIN = { name: "drain", label: false, count: 2, total: 220e6, longest: 200e6, flagged: 0 };

/** Lanes of two threads, the worker's zones a drain holding a parse and a
 * short drain after; 1000 pixels for a second, so a pixel is a millisecond.
 * The worker comes first, open to both its rows, at 18 and 34 pixels, and
 * main is folded into a lane of threads without zones under it. With
 * `offCpu`, the drain was blocked for 120 ms and preempted for 20. */
async function zonedLanes(look: ProfileLook = NO_LOOK, offCpu = false): Promise<Server> {
  const summary = summaryOf({
    timed: true,
    threads: ZONED_THREADS,
    zoneSites: [DRAIN, { ...DRAIN, name: "parse", count: 1, total: 50e6, longest: 50e6 }],
  });
  const server = await mount(
    <div style={SECOND_WIDE}>
      <Lanes />
    </div>,
    { summary, look },
  );
  server.answer(await server.next("profileTimeline"), { range: [0, 1e9], lanes: [] });
  const [answer, columns] = zonesOf(
    ["drain", "parse"],
    [
      [100e6, 300e6, 1, 0, 0, 1, 0, 120e6, 20e6],
      [150e6, 200e6, 1, 1, 1, 1, 0],
      [500e6, 520e6, 1, 0, 0, 1, 0],
    ],
    offCpu,
  );
  server.answer(await server.next("profileZones"), answer, columns);
  return server;
}

const lanesCanvas = () => page.getByRole("img", { name: /^CPU and zones over/u });

it("says why the zones could not be read, and still draws the samples", async () => {
  const summary = summaryOf({ timed: true, threads: ZONED_THREADS, zoneSites: [DRAIN] });
  const server = await mount(<Lanes />, { summary });
  server.answer(await server.next("profileTimeline"), { range: [0, 1e9], lanes: [] });
  server.fail(await server.next("profileZones"), "the zones are cut short");
  await expect.element(page.getByText("the zones are cut short")).toBeVisible();
  await expect.element(lanesCanvas()).toBeVisible();
});

it("waits on no zones in a box too narrow to draw them", async () => {
  const summary = summaryOf({ timed: true, threads: ZONED_THREADS, zoneSites: [DRAIN] });
  const server = await mount(
    <div style={NAMES_WIDE}>
      <Lanes />
    </div>,
    { summary },
  );
  await settled();
  expect(server.requests.some((request) => request.method === "profileZones")).toBe(false);
  expect(document.querySelector("canvas.lanes")?.classList.contains("stale")).toBe(false);
});

it("puts a range's ends side by side where it is too near an edge for both", async () => {
  await zonedLanes({ range: "995000000-998000000" });
  await vi.waitFor(() => expect(ruler()).toContain("[0.998 s]"), POLL);
  const [from, to] = [...document.querySelectorAll(".ruler-marks .marker")].map((pill) =>
    pill.getBoundingClientRect(),
  );
  assert(from && to, "a range has two ends");
  expect(from.right).toBeLessThanOrEqual(to.left);
  expect(covered()).toEqual([]);
});

it("asks for no zones in a recording without any, nor of task lanes", async () => {
  const server = await mount(<Lanes />, {
    summary: summaryOf({ timed: true, threads: ZONED_THREADS }),
  });
  await server.next("profileTimeline");
  await settled();
  expect(server.requests.some((request) => request.method === "profileZones")).toBe(false);
});

it("shows a thread's zones to every depth, and folds them to one from its chevron", async () => {
  await zonedLanes();
  const canvas = lanesCanvas();
  // The worker's lane and both its rows of zones, and the folded lane.
  await vi.waitFor(() => expect(canvas.element().clientHeight).toBe(18 * 2 + 16 * 2), POLL);
  const chevron = page.getByRole("button", { name: "Hide worker 11's nested zones" });
  await expect.element(chevron).toHaveTextContent("▾");
  await chevron.click();
  await vi.waitFor(() => expect(canvas.element().clientHeight).toBe(18 * 2 + 16), POLL);
  const shown = page.getByRole("button", { name: "Show worker 11's nested zones" });
  await expect.element(shown).toHaveTextContent("▸ +1");
  await shown.click();
  await vi.waitFor(() => expect(canvas.element().clientHeight).toBe(18 * 2 + 16 * 2), POLL);
});

const laneNames = () =>
  [...document.querySelectorAll(".lane-names .lane-name")].map((name) => name.textContent);

it("puts threads with zones first, and folds the rest into a lane that opens to them", async () => {
  await zonedLanes();
  const canvas = lanesCanvas();
  // The worker has less weight than main, and still comes first.
  await vi.waitFor(() => expect(laneNames()).toEqual(["worker 11", "▸ 1 without zones"]), POLL);
  const others = page.getByRole("button", { name: "1 without zones" });
  await expect.element(others).toHaveAttribute("aria-expanded", "false");

  await others.click();
  await vi.waitFor(
    () => expect(laneNames()).toEqual(["worker 11", "▾ 1 without zones", "main 10"]),
    POLL,
  );
  await vi.waitFor(() => expect(canvas.element().clientHeight).toBe(18 * 3 + 16 * 2), POLL);
  // Opening the lane selects no thread; a thread in it selects as any does.
  expect(lookShown().th).toBeUndefined();
  await page.getByRole("button", { name: "main 10" }).click();
  expect(lookShown().th).toEqual(["0"]);

  await others.click();
  await vi.waitFor(() => expect(laneNames()).toEqual(["worker 11", "▸ 1 without zones"]), POLL);
  await vi.waitFor(() => expect(canvas.element().clientHeight).toBe(18 * 2 + 16 * 2), POLL);
  // The thread it hides stays selected.
  expect(lookShown().th).toEqual(["0"]);
});

it("folds no lane when every thread has zones, or none does", async () => {
  const both = ZONED_THREADS.map((thread) => ({ ...thread, zones: 1 }));
  await mount(<Lanes />, {
    summary: summaryOf({ timed: true, threads: both, zoneSites: [DRAIN] }),
  });
  await expect.element(page.getByRole("button", { name: "main 10" })).toBeVisible();
  await expect.element(page.getByRole("button", { name: "worker 11" })).toBeVisible();
  expect(document.body.textContent).not.toContain("without zones");
  cleanup();

  const neither = ZONED_THREADS.map((thread) => ({ ...thread, zones: 0 }));
  await mount(<Lanes />, { summary: summaryOf({ timed: true, threads: neither }) });
  await expect.element(page.getByRole("button", { name: "main 10" })).toBeVisible();
  await expect.element(page.getByRole("button", { name: "worker 11" })).toBeVisible();
  expect(document.body.textContent).not.toContain("without zones");
});

it("says how much of a zone ran on its CPU, in a label that fits it", async () => {
  const drawn: { text: string; width: number }[] = [];
  const fillText = CanvasRenderingContext2D.prototype.fillText;
  vi.spyOn(CanvasRenderingContext2D.prototype, "fillText").mockImplementation(function (
    this: CanvasRenderingContext2D,
    text,
    ...rest
  ) {
    drawn.push({ text, width: this.measureText(text).width });
    fillText.call(this, text, ...rest);
  });
  await zonedLanes(NO_LOOK, true);
  // The drain ran for 60 of its 200 ms.
  await vi.waitFor(
    () => expect(drawn.map(({ text }) => text)).toContain("drain 200 ms, 30% CPU"),
    POLL,
  );
  // A pixel is a millisecond: the parse has 44 pixels for its label, the
  // short drain too few for any.
  const parse = drawn.filter(({ text }) => text.endsWith("%") || text.includes("CPU"));
  expect(parse.every(({ text }) => !text.startsWith("parse"))).toBe(true);
  expect(drawn.find(({ text }) => text.includes("100%"))?.width).toBeLessThanOrEqual(44);
  expect(drawn.some(({ text }) => text.includes("20.0 ms") || text.includes("20 ms"))).toBe(false);

  await userEvent.hover(lanesCanvas(), { position: { x: 200, y: 26 } });
  await vi.waitFor(
    () =>
      expect(page.getByTestId("zone-tip").element().textContent).toContain(
        "30% on CPU, 120 ms blocked, 20.0 ms preempted",
      ),
    POLL,
  );
  vi.restoreAllMocks();
});

it("draws a zone without its CPU when the recording did not trace it", async () => {
  const drawn: string[] = [];
  const fillText = CanvasRenderingContext2D.prototype.fillText;
  vi.spyOn(CanvasRenderingContext2D.prototype, "fillText").mockImplementation(function (
    this: CanvasRenderingContext2D,
    text,
    ...rest
  ) {
    drawn.push(text);
    fillText.call(this, text, ...rest);
  });
  await zonedLanes();
  await vi.waitFor(() => expect(drawn).toContain("drain 200 ms"), POLL);
  expect(drawn.some((text) => text.includes("%"))).toBe(false);
  await userEvent.hover(lanesCanvas(), { position: { x: 200, y: 26 } });
  await vi.waitFor(
    () => expect(page.getByTestId("zone-tip").element().textContent).toContain("drain: 200 ms"),
    POLL,
  );
  expect(page.getByTestId("zone-tip").element().textContent).not.toContain("on CPU");
  vi.restoreAllMocks();
});

it("selects a zone and its site from a click, leaving the threads alone, and lets it go again", async () => {
  await zonedLanes({ th: ["0"] });
  const canvas = lanesCanvas();
  await vi.waitFor(() => expect(canvas.element().clientHeight).toBe(68), POLL);
  // The drain, 200 ms in, on the worker's first row of zones.
  await userEvent.click(canvas, { position: { x: 200, y: 26 } });
  await vi.waitFor(
    () => expect(lookShown()).toEqual({ th: ["0"], zi: "1:100000000-300000000", zs: "drain" }),
    POLL,
  );
  await userEvent.click(canvas, { position: { x: 200, y: 26 } });
  await vi.waitFor(() => expect(lookShown()).toEqual({ th: ["0"], zs: "drain" }), POLL);

  // Another site's zone takes its site, and the old site's band goes.
  changeLook((look) => ({ ...look, zb: "1-2", zf: "band" }));
  await userEvent.click(canvas, { position: { x: 175, y: 42 } });
  await vi.waitFor(
    () => expect(lookShown()).toEqual({ th: ["0"], zi: "1:150000000-200000000", zs: "parse" }),
    POLL,
  );
  // Nothing under the pointer selects nothing.
  await userEvent.click(canvas, { position: { x: 900, y: 26 } });
  expect(lookShown().zi).toBe("1:150000000-200000000");
});

it("brings a zone chosen elsewhere into view, asking for it in whole nanoseconds", async () => {
  const server = await zonedLanes();
  // Under a pixel wide, so the lanes zoom to it, a third of its length
  // either side: not a whole number of nanoseconds.
  changeLook((look) => ({ ...look, zs: "drain", zi: "1:500000000-500300001" }));
  const asked = await server.next("profileZones");
  const range = (asked.params as { range: [number, number] }).range;
  expect(range.every((time) => Number.isInteger(time))).toBe(true);
  expect(range[0]).toBeLessThan(500_000_000);
  expect(range[1]).toBeGreaterThan(500_300_001);
  expect(range[1] - range[0]).toBeLessThan(1_000_000);
  const timeline = await server.next("profileTimeline");
  expect(
    (timeline.params as { range: number[] }).range.every((time) => Number.isInteger(time)),
  ).toBe(true);
  // The views read the zone's span, and the threads stay as they were.
  expect(lookShown().th).toBeUndefined();
});

it("scrolls a zone chosen elsewhere into view, once, so the lanes scroll away from it again", async () => {
  // Forty threads with zones, the last of them the one the zone is on.
  const threads: ProfileThread[] = Array.from({ length: 40 }, (_, row) => ({
    row,
    pid: 1,
    tid: row + 1,
    name: `worker ${row}`,
    samples: 40 - row,
    weight: 40 - row,
    zones: 1,
    blocked: 0,
    preempted: 0,
  }));
  const server = await mount(
    <div style={SECOND_WIDE_SHORT}>
      <Lanes />
    </div>,
    { summary: summaryOf({ timed: true, threads, zoneSites: [DRAIN] }) },
  );
  server.answer(await server.next("profileTimeline"), { range: [0, 1e9], lanes: [] });
  const [answer, columns] = zonesOf(
    ["drain"],
    threads.map(({ row }) => [100e6, 300e6, row, 0, 0, 1, 0]),
  );
  server.answer(await server.next("profileZones"), answer, columns);
  const box = page.getByTestId("timeline").element();
  await vi.waitFor(() => expect(box.scrollHeight).toBeGreaterThan(box.clientHeight * 2), POLL);
  expect(box.scrollTop).toBe(0);

  const last = page.getByRole("button", { name: "worker 39 40" }).element();
  const shown = () => {
    const [outer, name] = [box.getBoundingClientRect(), last.getBoundingClientRect()];
    return name.top >= outer.top && name.bottom <= outer.bottom;
  };
  expect(shown()).toBe(false);
  changeLook((look) => ({ ...look, zs: "drain", zi: "39:100000000-300000000" }));
  await vi.waitFor(() => expect(shown()).toBe(true), POLL);

  // Scrolled back, it stays where the reader put it.
  box.scrollTop = 0;
  changeLook((look) => ({ ...look, range: "0-500000000" }));
  await settled();
  expect(box.scrollTop).toBe(0);
});

it("names a zone, how long it took, and what holds it", async () => {
  await zonedLanes();
  const canvas = lanesCanvas();
  await vi.waitFor(() => expect(canvas.element().clientHeight).toBe(68), POLL);
  await userEvent.hover(canvas, { position: { x: 175, y: 42 } });
  const zoneTip = page.getByTestId("zone-tip");
  await vi.waitFor(
    () =>
      expect(zoneTip.element().textContent).toMatch(
        /^parse: 50(\.0)? msworker 11, from 150 msin drain$/u,
      ),
    POLL,
  );
});

it("zooms with ctrl and the wheel anywhere in the lanes, the names included", async () => {
  await zonedLanes();
  await vi.waitFor(() => expect(ruler().at(-1)).toBe("1.00 s"), POLL);
  const names = document.querySelector(".lane-names");
  assert(names, "the lanes have names");
  const bounds = names.getBoundingClientRect();
  act(() => {
    names.dispatchEvent(
      new WheelEvent("wheel", {
        bubbles: true,
        cancelable: true,
        ctrlKey: true,
        deltaY: -400,
        clientX: bounds.right + 500,
        clientY: bounds.top + 4,
      }),
    );
  });
  await vi.waitFor(() => expect(ruler().at(-1)).not.toBe("1.00 s"), POLL);
  // Zooming shows less, so it asks for that time's zones, but the range
  // the views read stays as it was.
  expect(lookShown().range).toBeUndefined();
});

/** The ruler's labels that a pill covers, which it should hide. */
function covered(): string[] {
  const marks = [...document.querySelectorAll("[data-testid=ruler] .ruler-marks > span")];
  const pills = marks.filter((mark) => mark.classList.contains("marker"));
  return marks
    .filter((mark) => !mark.classList.contains("marker"))
    .filter((mark) => {
      const at = mark.getBoundingClientRect();
      return pills.some((pill) => {
        const over = pill.getBoundingClientRect();
        return at.left < over.right && over.left < at.right;
      });
    })
    .map((mark) => mark.textContent ?? "");
}

/** The ruler's last round time, the end of what the lanes show. */
const lastMark = () => ruler().findLast((mark) => !/^[[<]/u.test(mark));

/** Presses the lanes `from` pixels in and lets go `to` pixels in, `y`
 * pixels down, with ctrl held or not. */
function dragLanes(
  from: number,
  to: number,
  ctrlKey = false,
  y = 60,
  beforeRelease = NOTHING,
): void {
  const canvas = lanesCanvas().element();
  const bounds = canvas.getBoundingClientRect();
  const at = (x: number) => ({
    bubbles: true,
    button: 0,
    ctrlKey,
    clientX: bounds.left + x,
    clientY: bounds.top + y,
  });
  for (const [type, x] of [
    ["pointerdown", from],
    ["pointermove", (from + to) / 2],
    ["pointermove", to],
    ["pointerup", to],
  ] as const) {
    if (type === "pointerup") {
      beforeRelease();
    }
    act(() => {
      canvas.dispatchEvent(new PointerEvent(type, at(x)));
    });
  }
}

it("selects a range from a drag across the lanes, letting go of the zone, and a click lets it go", async () => {
  vi.spyOn(Element.prototype, "setPointerCapture").mockImplementation(() => {});
  await zonedLanes();
  const canvas = lanesCanvas();
  await vi.waitFor(() => expect(canvas.element().clientHeight).toBe(68), POLL);
  await userEvent.click(canvas, { position: { x: 200, y: 26 } });
  await vi.waitFor(
    () => expect(lookShown()).toEqual({ zs: "drain", zi: "1:100000000-300000000" }),
    POLL,
  );
  // Right to left, starting over a zone: a pixel is a millisecond.
  dragLanes(250, 120, false, 26);
  await vi.waitFor(
    () => expect(lookShown()).toEqual({ zs: "drain", range: "120000000-250000000" }),
    POLL,
  );
  expect(ruler()).toContain("[0.120 s]");
  // The view stayed put. A press that hardly moves is a click, which off
  // the zones lets go of the range.
  expect(lastMark()).toBe("1.00 s");
  dragLanes(500, 503);
  await vi.waitFor(() => expect(lookShown()).toEqual({ zs: "drain" }), POLL);
  // A drag past the lanes' ends stops at them.
  dragLanes(900, 5000);
  await vi.waitFor(() => expect(lookShown().range).toBe("900000000-1000000000"), POLL);
  act(() => {
    lanesCanvas()
      .element()
      .dispatchEvent(new PointerEvent("pointerout", { bubbles: true }));
  });
  expect(covered()).toEqual([]);
  // Escape lets go of a drag under way, and of nothing else.
  dragLanes(100, 300, false, 60, () => {
    act(() => {
      window.dispatchEvent(new KeyboardEvent("keydown", { key: "Escape", bubbles: true }));
    });
  });
  await settled();
  expect(lookShown().range).toBe("900000000-1000000000");
  expect(ruler()).not.toContain("<0.200 s>");
  vi.restoreAllMocks();
});

it("keeps a range's ends on the ruler however long their times grow", async () => {
  vi.spyOn(Element.prototype, "setPointerCapture").mockImplementation(() => {});
  await zonedLanes();
  const canvas = lanesCanvas().element();
  const bounds = canvas.getBoundingClientRect();
  // Zoomed in about two thirds of the way, to a few microseconds, whose
  // times take many digits.
  for (let step = 0; step < 6; step += 1) {
    act(() => {
      canvas.dispatchEvent(
        new WheelEvent("wheel", {
          bubbles: true,
          cancelable: true,
          ctrlKey: true,
          deltaY: -1000,
          clientX: bounds.left + 666,
          clientY: bounds.top + 4,
        }),
      );
    });
    await settled();
  }
  // Across the whole of the lanes and past both ends.
  dragLanes(-50, 1100);
  await vi.waitFor(() => expect(lookShown().range).toBeDefined(), POLL);
  act(() => {
    canvas.dispatchEvent(new PointerEvent("pointerout", { bubbles: true }));
  });
  const marks = document.querySelector("[data-testid=ruler] .ruler-marks");
  assert(marks, "the ruler has marks");
  const pills = [...marks.querySelectorAll(".marker")];
  expect(pills.map((pill) => pill.textContent?.length ?? 0).every((length) => length > 9)).toBe(
    true,
  );
  expect(covered()).toEqual([]);
  const outer = marks.getBoundingClientRect();
  for (const pill of pills) {
    const at = pill.getBoundingClientRect();
    expect(at.left).toBeGreaterThanOrEqual(outer.left);
    expect(at.right).toBeLessThanOrEqual(outer.right);
  }
  vi.restoreAllMocks();
});

it("pans with ctrl held while dragging, selecting nothing, and keeps the menu away", async () => {
  vi.spyOn(Element.prototype, "setPointerCapture").mockImplementation(() => {});
  await zonedLanes();
  await vi.waitFor(() => expect(lastMark()).toBe("1.00 s"), POLL);
  // The whole profile shows, so there is nowhere to pan to.
  dragLanes(600, 300, true);
  await settled();
  expect(lastMark()).toBe("1.00 s");
  expect(lookShown().range).toBeUndefined();

  const canvas = lanesCanvas().element();
  const bounds = canvas.getBoundingClientRect();
  act(() => {
    canvas.dispatchEvent(
      new WheelEvent("wheel", {
        bubbles: true,
        cancelable: true,
        ctrlKey: true,
        deltaY: -400,
        clientX: bounds.left,
        clientY: bounds.top + 4,
      }),
    );
  });
  await vi.waitFor(() => expect(lastMark()).not.toBe("1.00 s"), POLL);
  const zoomedEnd = lastMark();
  dragLanes(600, 300, true);
  await vi.waitFor(() => expect(lastMark()).not.toBe(zoomedEnd), POLL);
  expect(lookShown().range).toBeUndefined();

  const menu = new MouseEvent("contextmenu", { bubbles: true, cancelable: true, ctrlKey: true });
  act(() => {
    canvas.dispatchEvent(menu);
  });
  expect(menu.defaultPrevented).toBe(true);
  vi.restoreAllMocks();
});

/** A site's stats: 8 zones in four buckets of lengths, the longest one. */
const STATS = {
  count: 8,
  total: 2000,
  flagged: 0,
  min: 100,
  median: 150,
  p90: 800,
  p99: 900,
  max: 900,
  histogram: [
    [100, 5],
    [200, 2],
    [400, 0],
    [800, 1],
  ],
  longest: [{ thread: 1, start: 1000, end: 1900, value: null }],
  banded: [8, 2000],
  shorter: 0,
  values: [],
};

const DRAIN_SITE = { ...DRAIN, count: 8, total: 2000, longest: 900 };

const NOTHING = () => {};

/** The sites the Zones pane lists, in order. */
const listedSites = () =>
  [...document.querySelectorAll(".zone-sites tbody td:first-child button")].map(
    (button) => button.textContent,
  );

/** Presses at `from` across the histogram and lets go at `to`, each a
 * share of its width. */
function dragHistogram(from: number, to: number): void {
  const bars = page.getByRole("img", { name: /^Lengths of drain/u }).element();
  const bounds = bars.getBoundingClientRect();
  const at = (share: number) => ({
    bubbles: true,
    pointerId: 1,
    clientX: bounds.left + bounds.width * share,
    clientY: bounds.top + 4,
  });
  act(() => {
    bars.dispatchEvent(new PointerEvent("pointerdown", { ...at(from), button: 0 }));
    bars.dispatchEvent(new PointerEvent("pointermove", at(to)));
    bars.dispatchEvent(new PointerEvent("pointerup", at(to)));
  });
}

it("lists sites by total, and sorts them by another column", async () => {
  const summary = summaryOf({
    zoneSites: [
      { ...DRAIN, name: "few", count: 1, total: 900, longest: 900 },
      { ...DRAIN, name: "many", count: 50, total: 100, longest: 10 },
    ],
  });
  await mount(<ZonesPane />, { summary });
  expect(listedSites()).toEqual(["few", "many"]);
  await page.getByRole("button", { name: "count", exact: true }).click();
  expect(listedSites()).toEqual(["many", "few"]);
  await page.getByRole("button", { name: "many", exact: true }).click();
  expect(lookShown().zs).toBe("many");
  await page.getByRole("button", { name: "many", exact: true }).click();
  expect(lookShown().zs).toBeUndefined();
});

it("says a recording has no zones", async () => {
  await mount(<ZonesPane />);
  await expect.element(page.getByText("This recording has no zones.")).toBeVisible();
});

it("picks a band from the histogram, scopes the views to it, compares it, and lets it go", async () => {
  vi.spyOn(Element.prototype, "setPointerCapture").mockImplementation(() => {});
  const server = await mount(<ZonesPane />, {
    summary: summaryOf({ zoneSites: [DRAIN_SITE] }),
    look: { zs: "drain", th: ["0"] },
  });
  const asked = await server.next("profileZoneStats");
  expect(asked.params).toMatchObject({ name: "drain", threads: [0] });
  server.answer(asked, STATS);
  await vi.waitFor(
    () => expect(page.getByTestId("zone-stats").element().textContent).toContain("count8total"),
    POLL,
  );
  // Nothing to scope to until there is a band.
  await expect.element(page.getByRole("button", { name: "Inside the band" })).toBeDisabled();
  await expect
    .element(page.getByRole("button", { name: "Compare the band with the rest" }))
    .toBeDisabled();

  // From the second bucket to the last: 200 ns to the longest, 900.
  dragHistogram(0.3, 0.95);
  expect(lookShown()).toEqual({ zs: "drain", th: ["0"], zb: "200-900" });
  const banded = await server.next("profileZoneStats");
  expect(banded.params).toMatchObject({ band: [200, 900] });
  server.answer(banded, { ...STATS, banded: [3, 1300] });
  await vi.waitFor(
    () => expect(page.getByTestId("zone-band").element().textContent).toContain("3 of 8"),
    POLL,
  );
  await expect.element(page.getByText("Longest in the band")).toBeVisible();

  await page.getByRole("button", { name: "Inside the band" }).click();
  expect(lookShown().zf).toBe("band");
  await page.getByRole("button", { name: "Compare the band with the rest" }).click();
  expect(lookShown().bz).toBe("rest");

  // A click on one bucket picks it alone; on the band again, lets it go,
  // and what read the band with it.
  dragHistogram(0.1, 0.1);
  expect(lookShown().zb).toBe("100-199");
  dragHistogram(0.1, 0.1);
  expect(lookShown()).toEqual({ zs: "drain", th: ["0"] });
  vi.restoreAllMocks();
});

it("selects one of the longest, ranks it, and never touches the threads", async () => {
  const server = await mount(<ZonesPane />, {
    summary: summaryOf({
      zoneSites: [DRAIN_SITE, { ...DRAIN_SITE, name: "parse" }],
      threads: ZONED_THREADS,
    }),
    look: { zs: "drain", th: ["0"] },
  });
  server.answer(await server.next("profileZoneStats"), STATS);
  const longest = page.getByRole("button", { name: "900 ns" });
  await longest.click();
  expect(lookShown()).toEqual({ zs: "drain", th: ["0"], zi: "1:1000-1900" });
  const ranked = await server.next("profileZoneStats");
  expect(ranked.params).toMatchObject({ rankOf: 900 });
  server.answer(ranked, { ...STATS, shorter: 7 });
  await expect
    .element(page.getByTestId("zone-rank"))
    .toHaveTextContent("The zone selected is p88 of 8: 900 ns");
  await expect.element(longest).toHaveAttribute("aria-pressed", "true");
  await longest.click();
  expect(lookShown()).toEqual({ zs: "drain", th: ["0"] });

  // Another site lets go of the zone, which is not one of its own.
  await longest.click();
  expect(lookShown().zi).toBe("1:1000-1900");
  await page.getByRole("button", { name: "parse", exact: true }).click();
  expect(lookShown()).toEqual({ zs: "parse", th: ["0"] });
});

it("says why a site could not be measured, and when none of its zones are in the range", async () => {
  const server = await mount(<ZonesPane />, {
    summary: summaryOf({ zoneSites: [DRAIN_SITE] }),
    look: { zs: "drain" },
  });
  await expect.element(page.getByText("Measuring drain…")).toBeVisible();
  server.fail(await server.next("profileZoneStats"), "no zones are named drain");
  await expect.element(page.getByText("no zones are named drain")).toBeVisible();

  changeLook((look) => ({ ...look, range: "5-9" }));
  server.answer(await server.next("profileZoneStats"), {
    ...STATS,
    count: 0,
    total: 0,
    histogram: [],
    longest: [],
    banded: [0, 0],
  });
  await expect.element(page.getByText("None of its zones are in the range.")).toBeVisible();

  // Threads chosen may leave them out too, wherever they are.
  const empty = { ...STATS, count: 0, total: 0, histogram: [], longest: [], banded: [0, 0] };
  changeLook((look) => ({ ...look, th: ["1"] }));
  server.answer(await server.next("profileZoneStats"), empty);
  await expect
    .element(page.getByText("None of its zones are in the range or on the thread selected."))
    .toBeVisible();
  changeLook(({ range: _range, ...look }) => ({ ...look, th: ["1", "2"] }));
  server.answer(await server.next("profileZoneStats"), empty);
  await expect
    .element(page.getByText("None of its zones are on the threads selected."))
    .toBeVisible();
});

it("offers no zone scope of waits, which zones never hold", async () => {
  const server = await mount(<ZonesPane />, {
    summary: summaryOf({ zoneSites: [DRAIN_SITE] }),
    look: { zs: "drain", zb: "100-199", src: "waits" },
  });
  server.answer(await server.next("profileZoneStats"), STATS);
  for (const name of ["Range only", "Inside every one", "Inside the band"]) {
    await expect.element(page.getByRole("button", { name })).toBeDisabled();
  }
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

const sorting = (column: string) =>
  page.getByRole("button", { name: column, exact: true }).element().closest("th")?.ariaSort;

it("selects a call from its name's button, and sorts by a column it names", async () => {
  const server = await mount(<SyscallsView />, { summary: traced() });
  server.answer(await server.next("syscallSummary"), SUMMARY);
  const call = page.getByRole("button", { name: "read", exact: true });
  await expect.element(call).toHaveAttribute("aria-pressed", "false");
  (call.element() as HTMLElement).focus();
  await userEvent.keyboard("{Enter}");
  await expect.element(call).toHaveAttribute("aria-pressed", "true");
  expect(lookShown().sc).toBe("read");
  expect(sorting("total")).toBe("descending");
  expect(sorting("calls")).toBeNull();
  await page.getByRole("button", { name: "name", exact: true }).click();
  await vi.waitFor(() => expect(sorting("name")).toBe("ascending"), POLL);
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

it("reads a click on a heat map's top edge as its highest row, in the slice clicked", async () => {
  const server = await mount(<SyscallsView />, { summary: traced(), look: { sc: "read" } });
  server.answer(await server.next("syscallSummary"), SUMMARY);
  // Two slices of three rows: the first's calls are slow, the second's fast.
  server.answer(await server.next("syscallHeatmap"), {
    start: 1000,
    width: 500,
    rows: 3,
    counts: [0, 0, 5, 4, 0, 0],
    most: 5,
  });
  const map = page.getByRole("img", { name: "Latency heat map" });
  await expect.element(page.getByText(/0 to/u)).toBeVisible();
  const bounds = map.element().getBoundingClientRect();
  // A mouse event's coordinates are whole pixels, so on a canvas whose top
  // falls between two, a click on its top row reads as just above it.
  expect(bounds.top % 1, "the heat map's top is a whole pixel").not.toBe(0);
  mouseAt(map.element(), "click", bounds.width / 4, 0);
  await vi.waitFor(() => expect(lookShown().sl).toBeDefined(), POLL);
  expect(lookShown().sl).toBe("2");
  expect(lookShown().range).toBe("1000-1500");
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
  await page.getByRole("button", { name: /read/u }).click();
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

/** Mounts a profile's pane of `view`, or its overview. */
async function mountPage(
  summary: ProfileSummary,
  known: boolean,
  view: ProfileView | null = null,
): Promise<Server> {
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
    return <Profile view={view} profile={summary.id} look={look} setLook={setLook} />;
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
  await expect.element(page.getByText(/its columns are cut short/u)).toBeVisible();
  await expect.element(page.getByText("microscope web PROFILE.usprof")).not.toBeInTheDocument();
});

it("says nothing the tab says, and shows a bar only for a recording", async () => {
  const summary = summaryOf({ program: "/work/searcher" });
  const server = await mountPage(summary, true);
  server.answer(await server.next("profile"), summary);
  // Its tab names it, and File details says what it holds, so the head says neither.
  const details = page.getByRole("button", { name: "File details" });
  await expect.element(details).toBeVisible();
  const head = document.querySelector(".profile-head") as HTMLElement;
  expect(head.firstElementChild).toBe(details.element());
  expect(head.textContent).not.toMatch(/test\.usprof|searcher|10 samples|Download/u);
  expect(page.getByTestId("recording").elements()).toHaveLength(0);

  server.send({
    type: "profiles",
    profiles: [{ id: summary.id, name: summary.name, recording: true, revision: 0, ended: null }],
  });
  await expect
    .element(page.getByTestId("recording").getByText(`Recording ${summary.name}`))
    .toBeVisible();
});

/** What File details reads of a recording of a program with an argument a
 * shell would split. */
function inspectionOf(change: Partial<ProfileInspection> = {}): ProfileInspection {
  return {
    processes: [{ pid: 7, name: "rg", command: null }],
    modules: [
      { path: "/bin/rg", size: 2048, buildId: null, frames: 9 },
      { path: "/lib/libc.so.6", size: 1024, buildId: "5a70", frames: 2 },
    ],
    // Ten samples: 2 at depth 1, 5 at depth 3, 3 at depth 4.
    depths: [0, 2, 0, 5, 3],
    overTime: [4, 0, 6],
    distinctStacks: 4,
    frames: 11,
    framesWithLines: 8,
    counted: 20,
    stackBytes: 4096,
    shortened: 0,
    recorderSetup: 1e6,
    recorderFinishing: 5e6,
    tables: [
      { name: "samples", bytes: 300 },
      { name: "strings", bytes: 100 },
    ],
    file: { bytes: 5000, chunks: 1, truncated: false },
    ...change,
  };
}

/** A recording of a program with an argument a shell would split. */
const RG = { arguments: ["/bin/rg", "-c", "a b", "it's"], text: "/bin/rg -c a b it's" };

it("shows the file's details in a dialog: its whole command, then its samples", async () => {
  const summary = summaryOf({
    command: RG,
    timed: true,
    truncations: [{ what: "Stack copy ran out", count: 3 }],
    recorderCpu: 20e6,
    recorderLoading: 8e6,
    settings: [
      { name: "source", value: "CPU time at 999 Hz" },
      { name: "program", value: "/bin/rg" },
      { name: "command 0", value: "/bin/rg -c a b it's" },
    ],
  });
  const server = await mountPage(summary, true);
  server.answer(await server.next("profile"), summary);
  const inspect = page.getByRole("button", { name: "File details" });
  await userEvent.click(inspect);
  const dialog = page.getByRole("dialog", { name: "test.usprof" });
  await expect.element(dialog).toBeVisible();
  const asked = await server.next("profileInspect");
  expect(asked.params).toEqual({ profile: summary.id, revision: 0 });
  await expect.element(dialog.getByText("Reading the profile…")).toBeVisible();
  server.answer(asked, inspectionOf());

  // Metadata comes first: every argument whole, on its own line, quoted
  // as a shell needs, and Copy copies the line a shell runs.
  await expect
    .element(page.getByRole("tab", { name: "Metadata" }))
    .toHaveAttribute("aria-selected", "true");
  await expect.element(page.getByTestId("command")).toBeVisible();
  const lines = [...page.getByTestId("command").element().children].map((line) => line.textContent);
  expect(lines).toEqual(["/bin/rg \\", "-c \\", "'a b' \\", String.raw`'it'\''s'`]);
  await expect.element(dialog.getByRole("heading", { name: "Command", exact: true })).toBeVisible();
  const written = vi.spyOn(navigator.clipboard, "writeText").mockResolvedValue();
  await userEvent.click(dialog.getByRole("button", { name: "Copy" }));
  expect(written).toHaveBeenCalledWith(String.raw`/bin/rg -c 'a b' 'it'\''s'`);
  await expect.element(dialog.getByRole("button", { name: "Copied" })).toBeVisible();
  written.mockRestore();

  // Settings shown elsewhere are left out of Recording.
  await expect
    .element(dialog.getByRole("row", { name: "Source CPU time at 999 Hz" }))
    .toBeVisible();
  expect(dialog.getByRole("row", { name: /^Program/u }).elements()).toHaveLength(0);
  await expect
    .element(dialog.getByRole("row", { name: /^\/lib\/libc\.so\.6 1\.00 KiB 5a70 2$/u }))
    .toBeVisible();
  await expect
    .element(dialog.getByRole("row", { name: "Debug info after exit 5.00 ms" }))
    .toBeVisible();
  await expect.element(dialog.getByRole("row", { name: "samples 300 B 75.0%" })).toBeVisible();

  // The arrow keys move to Samples.
  await userEvent.click(page.getByRole("tab", { name: "Metadata" }));
  await userEvent.keyboard("{ArrowRight}");
  const samples = page.getByRole("tab", { name: "Samples" });
  await expect.element(samples).toHaveAttribute("aria-selected", "true");
  await expect.element(samples).toHaveFocus();
  await expect
    .element(dialog.getByRole("row", { name: "Stacks cut short 3, 30.0%" }))
    .toBeVisible();
  await expect
    .element(dialog.getByRole("row", { name: "Reached the outermost frame 7 70.0%" }))
    .toBeVisible();

  // Depth starts at one frame, as no sample has none; the median is 3.
  const bars = dialog.getByRole("button", { name: /frames?: /u });
  expect(bars.elements().map((bar) => bar.getAttribute("aria-label"))).toEqual([
    "1 frame: 2 samples",
    "2 frames: 0 samples",
    "3 frames: 5 samples",
    "4 frames: 3 samples",
  ]);
  expect(bars.elements()[2]?.classList.contains("marked")).toBe(true);
  await expect.element(dialog.getByText("median 3")).toBeVisible();
  await userEvent.hover(bars.nth(3));
  const depthRegion = dialog.getByRole("region", { name: /stack depth/u });
  await expect
    .element(depthRegion.getByRole("status"))
    .toHaveTextContent("4 frames: 3 samples, 30.0%");

  // Each slice of time reads out its span and count too, under round ticks.
  const time = dialog.getByRole("region", { name: /over time/u });
  const slices = time.getByRole("button");
  expect(slices.elements()).toHaveLength(3);
  await userEvent.hover(slices.nth(2));
  await expect
    .element(time.getByRole("status"))
    .toHaveTextContent("0.67 s – 1.00 s: 6 samples, 60.0%");
  const ticks = [...time.element().querySelectorAll(".details-ticks span")];
  expect(ticks.map((tick) => tick.textContent)).toEqual([
    "0",
    "0.2 s",
    "0.4 s",
    "0.6 s",
    "0.8 s",
    "1.00 s",
  ]);

  // Escape closes it and gives focus back to File details.
  await userEvent.keyboard("{Escape}");
  await expect.element(dialog).not.toBeInTheDocument();
  await expect.element(inspect).toHaveFocus();
});

it("closes File details on a click outside, and says when a recording kept only the joined command", async () => {
  const summary = summaryOf({
    samples: 0,
    syscalls: 4,
    command: { arguments: null, text: "/bin/sh -c 'a b'" },
  });
  const server = await mountPage(summary, true);
  server.answer(await server.next("profile"), summary);
  await userEvent.click(page.getByRole("button", { name: "File details" }));
  server.answer(await server.next("profileInspect"), inspectionOf({ file: null }));
  const dialog = page.getByRole("dialog");
  // A trace with no samples has nothing for a Samples tab.
  await expect.element(dialog.getByText(/kept the command as one line/u)).toBeVisible();
  expect(dialog.getByRole("tab").elements()).toHaveLength(0);
  expect(page.getByTestId("command").element().textContent).toBe("/bin/sh -c 'a b'");
  await expect.element(dialog.getByRole("row", { name: "Source a live recording" })).toBeVisible();

  // A click in the dialog keeps it; one on the scrim around it closes it.
  await userEvent.click(dialog.getByText(/kept the command/u));
  await expect.element(dialog).toBeVisible();
  const scrim = document.querySelector(".details-overlay") as HTMLElement;
  // This harness has no styles, so the scrim has no margin to click.
  act(() => {
    scrim.dispatchEvent(new PointerEvent("pointerdown", { bubbles: true }));
  });
  await expect.element(dialog).not.toBeInTheDocument();
});

/** Takes the clipboard API away, as from a page served over plain HTTP to
 * another machine, and copies through a selection, as such a page must. */
function withoutClipboard() {
  Object.defineProperty(navigator, "clipboard", { value: undefined, configurable: true });
  const selected: string[] = [];
  const copy = vi.spyOn(document, "execCommand").mockImplementation((command) => {
    const area = document.activeElement;
    if (command === "copy" && area instanceof HTMLTextAreaElement) {
      selected.push(area.value.slice(area.selectionStart, area.selectionEnd));
    }
    return command === "copy";
  });
  return {
    selected,
    restore() {
      copy.mockRestore();
      // The page's own value goes, and Navigator's getter answers again.
      delete (navigator as unknown as Record<string, unknown>).clipboard;
    },
  };
}

it("copies the command from File details where the page has no clipboard API", async () => {
  const summary = summaryOf({ command: RG });
  const server = await mountPage(summary, true);
  server.answer(await server.next("profile"), summary);
  await userEvent.click(page.getByRole("button", { name: "File details" }));
  server.answer(await server.next("profileInspect"), inspectionOf());
  const dialog = page.getByRole("dialog", { name: "test.usprof" });
  const without = withoutClipboard();
  try {
    await userEvent.click(dialog.getByRole("button", { name: "Copy" }));
    await expect.element(dialog.getByRole("button", { name: "Copied" })).toBeVisible();
    expect(without.selected).toEqual([String.raw`/bin/rg -c 'a b' 'it'\''s'`]);
  } finally {
    without.restore();
  }
});

/** The accounting's facts, each its label and value. */
const stats = () =>
  [...document.querySelectorAll("[data-testid=accounting] .stat")].map(
    (stat) =>
      `${stat.querySelector(".stat-label")?.textContent}: ${stat.querySelector(".stat-value")?.textContent}`,
  );

it("warns of instrumentation that went unread though no event was read", async () => {
  const problem = "/bin/app: pid 7 exited before microscope read its instrumentation";
  const summary = summaryOf({ probeProblems: [{ time: 0, text: problem }] });
  const server = await mountPage(summary, true);
  server.answer(await server.next("profile"), summary);
  await expect.element(page.getByText("Probe events")).toBeVisible();
  expect(stats()).toContain("Probe events: 0, lost 0");
  const stat = document.querySelector("[data-testid=accounting] .stat:has(.warn)");
  expect(stat?.querySelector(".stat-label")?.textContent).toBe("Probe events");
  expect(stat?.getAttribute("title") ?? "").toContain(problem);
});

it("leads with what was recorded, and shows only the losses there were", async () => {
  const thread = { pid: 1, samples: 1, weight: 1, zones: 0, blocked: 0, preempted: 0 };
  const summary = summaryOf({
    command: RG,
    timed: true,
    duration: 2e9,
    weight: 3e9,
    samples: 3000,
    lost: 1000,
    throttles: 7,
    threads: [
      { ...thread, row: 0, tid: 10, name: "main", blocked: 1e9 },
      { ...thread, row: 1, tid: 11, name: "worker", preempted: 5e8 },
    ],
    settings: [
      { name: "source", value: "CPU time at 999 Hz" },
      { name: "kernel", value: "on" },
    ],
  });
  const server = await mountPage(summary, true);
  server.answer(await server.next("profile"), summary);
  const program = page.getByRole("button", { name: "rg", exact: true });
  await expect.element(program).toBeVisible();
  // Two threads at most keep two CPUs busy; 3 s of CPU in 2 s is 1.5. Lost
  // samples stay in File details.
  expect(stats()).toEqual([
    "Program: rg",
    "Sampling: 999 Hz + kernel",
    "Ran for: 2.00 s",
    "Threads: 2",
    "Cores busy: ≈1.5",
    "On CPU: 3.00 s",
    "Off CPU: 1.50 s",
    "Throttled: 7",
  ]);
  expect(
    [...document.querySelectorAll("[data-testid=accounting] .warn")].map(
      (each) => each.textContent,
    ),
  ).toEqual(["7"]);

  // The program shows its whole command, which copies where the page has
  // no clipboard API, and Escape puts it away.
  await userEvent.click(program);
  const dialog = page.getByRole("dialog", { name: "Command" });
  await expect
    .element(dialog.getByTestId("command"))
    .toHaveTextContent(String.raw`/bin/rg \-c \'a b' \'it'\''s'`);
  const without = withoutClipboard();
  try {
    await userEvent.click(dialog.getByRole("button", { name: "Copy" }));
    await expect.element(dialog.getByRole("button", { name: "Copied" })).toBeVisible();
    expect(without.selected).toEqual([String.raw`/bin/rg -c 'a b' 'it'\''s'`]);
  } finally {
    without.restore();
  }
  await userEvent.keyboard("{Escape}");
  await expect.element(dialog).not.toBeInTheDocument();
  await expect.element(program).toHaveFocus();
});

it("says why File details failed", async () => {
  const summary = summaryOf();
  const server = await mountPage(summary, true);
  server.answer(await server.next("profile"), summary);
  await userEvent.click(page.getByRole("button", { name: "File details" }));
  server.fail(await server.next("profileInspect"), "the file went away", "failed");
  await expect
    .element(page.getByText("Could not read its details: the file went away"))
    .toBeVisible();

  // Download sits between the tabs and Close. This test's page serves no
  // profiles, so the download fails, and says why.
  const dialog = page.getByRole("dialog");
  const buttons = [...dialog.element().querySelectorAll(".details-head > button")];
  expect(buttons.map((button) => button.textContent)).toEqual(["Download", "Close"]);
  const download = dialog.getByRole("button", { name: "Download" });
  await expect.element(download).toHaveAttribute("title", expect.stringContaining(".usprof"));
  await userEvent.click(download);
  await expect
    .element(dialog.getByText(/Could not download it: the server answered/u))
    .toBeVisible();
});

it("keeps what it shows while disconnected, and says so", async () => {
  const summary = summaryOf();
  const server = await mountPage(summary, true, "flame");
  server.answer(await server.next("profile"), summary);
  server.answer(await server.next("profileTree"), ...TREE);
  const search = page.getByRole("searchbox", { name: "Search frames" });
  await userEvent.fill(search, "lea");
  await expect.element(page.getByTestId("matched")).toBeVisible();

  server.drop();
  await expect.element(page.getByText(/^Disconnected/u)).toBeVisible();
  await expect.element(search).toHaveValue("lea");
  await expect.element(page.getByRole("img", { name: "Flame graph" })).toBeVisible();

  await server.open();
  await expect.element(page.getByText(/^Disconnected/u)).not.toBeInTheDocument();
  await expect.element(search).toHaveValue("lea");
  await expect.element(page.getByTestId("matched")).toBeVisible();
});
