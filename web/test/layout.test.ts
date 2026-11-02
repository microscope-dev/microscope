// The workbench layout: what each operation does to where panes sit, and the
// rules every layout keeps whatever a person does to it.

import { readFileSync } from "node:fs";
import { join } from "node:path";
import fc from "fast-check";
import { describe, expect, it } from "vitest";
import {
  activate,
  checkLayout,
  close,
  dock,
  emptyLayout,
  float,
  type GroupId,
  groupOf,
  groups,
  type KindId,
  type Layout,
  maximize,
  move,
  type Node,
  open,
  type PaneId,
  reframe,
  split,
} from "../src/workbench/layout";
import { loadLayout, saveLayout } from "../src/workbench/saved";
import { apply, step, steps } from "./arbitraries/layout";

/** Opens panes named by kind into the focused group. */
function opened(layout: Layout, ...kinds: string[]): [Layout, ...PaneId[]] {
  const ids: PaneId[] = [];
  for (const kind of kinds) {
    const result = open(layout, { kind: kind as KindId, key: "", params: null });
    layout = result.layout;
    ids.push(result.pane);
  }
  return [layout, ...ids];
}

/** A layout's dock as nested lists of the kinds in each group, with weights
 * to two places: easy to read in a failure. */
function shape(layout: Layout, node: Node = layout.root): unknown {
  if (node.type === "group") {
    return node.tabs.map((tab) => {
      const kind = layout.panes[tab]?.kind ?? "?";
      return tab === node.active ? `*${kind}` : kind;
    });
  }
  return {
    [node.axis]: node.children.map((child) => [
      Math.round(child.weight * 100) / 100,
      shape(layout, child.node),
    ]),
  };
}

const focusedKinds = (layout: Layout) =>
  groups(layout)
    .find((group) => group.id === layout.focused)
    ?.tabs.map((tab) => layout.panes[tab]?.kind);

describe("opening and closing", () => {
  it("opens beside the active tab, and shows a kind and key that is open already", () => {
    let [layout, a] = opened(emptyLayout(), "source", "memory");
    layout = activate(layout, a as PaneId);
    [layout] = opened(layout, "registers");
    expect(shape(layout)).toEqual(["source", "*registers", "memory"]);
    const again = open(layout, { kind: "source" as KindId, key: "", params: null });
    expect(again.pane).toBe(a);
    expect(shape(again.layout)).toEqual(["*source", "registers", "memory"]);
  });

  it("shows the tab to the right of a closed one, or else the left", () => {
    const [layout, a, b, c] = opened(emptyLayout(), "a", "b", "c");
    const middle = close(activate(layout, b as PaneId), b as PaneId);
    expect(shape(middle)).toEqual(["a", "*c"]);
    const last = close(activate(layout, c as PaneId), c as PaneId);
    expect(shape(last)).toEqual(["a", "*b"]);
    expect(Object.keys(close(last, a as PaneId).panes)).toEqual([b]);
  });

  it("closes a group with its last tab, and focuses the group beside it", () => {
    const [layout, , b] = opened(emptyLayout(), "a", "b");
    const both = split(layout, layout.focused, "right");
    expect(shape(both)).toEqual({
      row: [
        [0.5, ["*a"]],
        [0.5, ["*b"]],
      ],
    });
    const closed = close(both, b as PaneId);
    expect(shape(closed)).toEqual(["*a"]);
    expect(focusedKinds(closed)).toEqual(["a"]);
  });

  it("keeps one empty group when the last pane closes", () => {
    const [layout, a] = opened(emptyLayout(), "a");
    const empty = close(layout, a as PaneId);
    expect(shape(empty)).toEqual([]);
    expect(checkLayout(empty)).toEqual([]);
  });
});

describe("moving panes", () => {
  it("reorders within a group by the gap the pane is dropped in", () => {
    const [layout, a] = opened(emptyLayout(), "a", "b", "c");
    // The gap between b and c, counting a where it still is.
    expect(
      shape(move(layout, a as PaneId, { type: "tab", group: layout.focused, index: 2 })),
    ).toEqual(["b", "*a", "c"]);
    expect(
      shape(move(layout, a as PaneId, { type: "tab", group: layout.focused, index: 3 })),
    ).toEqual(["b", "c", "*a"]);
  });

  it("splits a group in half for a pane dropped on its side", () => {
    const [layout, , b] = opened(emptyLayout(), "a", "b", "c");
    const below = move(layout, b as PaneId, { type: "side", group: layout.focused, side: "top" });
    expect(shape(below)).toEqual({
      column: [
        [0.5, ["*b"]],
        [0.5, ["a", "*c"]],
      ],
    });
    expect(focusedKinds(below)).toEqual(["b"]);
  });

  it("gives a pane docked on an edge a quarter of the dock, across all of it", () => {
    const [layout, a, b, c] = opened(emptyLayout(), "a", "b", "c");
    const right = move(layout, a as PaneId, { type: "edge", side: "right" });
    const bottom = move(right, b as PaneId, { type: "edge", side: "bottom" });
    expect(shape(bottom)).toEqual({
      column: [
        [
          0.75,
          {
            row: [
              [0.75, ["*c"]],
              [0.25, ["*a"]],
            ],
          },
        ],
        [0.25, ["*b"]],
      ],
    });
    // Along the axis the dock is split on already, the others share the rest.
    const left = move(right, c as PaneId, { type: "edge", side: "left" });
    expect(shape(left)).toEqual({
      row: [
        [0.25, ["*c"]],
        [0.56, ["*b"]],
        [0.19, ["*a"]],
      ],
    });
  });

  it("refuses what makes no sense, returning the layout it was given", () => {
    const [layout, a] = opened(emptyLayout(), "a");
    expect(split(layout, layout.focused, "right")).toBe(layout);
    expect(move(layout, a as PaneId, { type: "side", group: layout.focused, side: "left" })).toBe(
      layout,
    );
    const floated = float(opened(layout, "b")[0], a as PaneId, {
      x: 10,
      y: 10,
      width: 300,
      height: 200,
    });
    const window = floated.floating[0]?.group.id as GroupId;
    expect(dock(floated, window, { type: "tab", group: window, index: 0 })).toBe(floated);
    const [, b] = Object.keys(floated.panes) as PaneId[];
    expect(move(floated, b as PaneId, { type: "side", group: window, side: "left" })).toBe(floated);
  });
});

describe("floating windows", () => {
  it("float on top, move as a whole when alone, and dock back as a group", () => {
    const [layout, a, b] = opened(emptyLayout(), "a", "b");
    const floated = float(layout, a as PaneId, { x: 10, y: 20, width: 300, height: 200 });
    expect(shape(floated)).toEqual(["*b"]);
    expect(floated.floating.map((window) => window.rect.x)).toEqual([10]);
    const moved = float(floated, a as PaneId, { x: 50, y: 20, width: 300, height: 200 });
    expect(moved.floating.map((window) => window.rect.x)).toEqual([50]);
    const window = moved.floating[0]?.group.id as GroupId;
    const below = groupOf(moved, b as PaneId)?.id as GroupId;
    const docked = dock(moved, window, { type: "side", group: below, side: "bottom" });
    expect(shape(docked)).toEqual({
      column: [
        [0.5, ["*b"]],
        [0.5, ["*a"]],
      ],
    });
    expect(docked.floating).toEqual([]);
    expect(groupOf(docked, a as PaneId)?.id).toBe(window);
  });

  it("raises a window when one of its panes is shown", () => {
    const [layout, a, b] = opened(emptyLayout(), "a", "b", "c");
    const rect = { x: 0, y: 0, width: 100, height: 100 };
    const two = float(float(layout, a as PaneId, rect), b as PaneId, rect);
    const raised = activate(two, a as PaneId);
    expect(
      raised.floating.map((window) => raised.panes[window.group.tabs[0] as PaneId]?.kind),
    ).toEqual(["b", "a"]);
  });

  it("move and resize as a whole, tabs and all, and rise and take focus", () => {
    const [layout, a, b, c] = opened(emptyLayout(), "a", "b", "c");
    const rect = { x: 0, y: 0, width: 100, height: 100 };
    const first = float(layout, a as PaneId, rect);
    const window = groupOf(first, a as PaneId)?.id as GroupId;
    const joined = move(first, b as PaneId, { type: "tab", group: window, index: 1 });
    const two = float(joined, c as PaneId, rect);
    const moved = reframe(two, window, { x: 40, y: 50, width: 300, height: 200 });
    expect(moved.floating.at(-1)).toEqual({
      group: groupOf(two, a as PaneId),
      rect: { x: 40, y: 50, width: 300, height: 200 },
    });
    expect(moved.focused).toBe(window);
    expect(reframe(two, window, { ...rect, width: 0 })).toBe(two);
    expect(reframe(two, layout.focused, rect)).toBe(two);
  });
});

it("forgets a maximized group once it is gone", () => {
  const [layout, , b] = opened(emptyLayout(), "a", "b");
  const both = split(layout, layout.focused, "right");
  const big = maximize(both, both.focused);
  expect(big.maximized).toBe(both.focused);
  expect(close(big, b as PaneId).maximized).toBeNull();
});

describe("any run of operations", () => {
  it("keeps every rule after every step", () => {
    fc.assert(
      fc.property(steps, (run) => {
        let layout = emptyLayout();
        for (const chosen of run) {
          layout = apply(layout, chosen);
          expect(checkLayout(layout)).toEqual([]);
        }
      }),
    );
  });

  it("only gains a pane by opening one and only loses one by closing it", () => {
    fc.assert(
      fc.property(steps, step, (run, chosen) => {
        const before = run.reduce(apply, emptyLayout());
        const after = apply(before, chosen);
        const lost = Object.keys(before.panes).filter((id) => !(id in after.panes));
        const gained = Object.keys(after.panes).filter((id) => !(id in before.panes));
        expect(lost.length).toBe(chosen.op === "close" && after !== before ? 1 : 0);
        expect(gained.length).toBeLessThanOrEqual(chosen.op === "open" ? 1 : 0);
        for (const id of Object.keys(after.panes) as PaneId[]) {
          expect(after.panes[id]).toBe(before.panes[id] ?? after.panes[id]);
        }
      }),
    );
  });

  it("shows a moved pane, in the group it lands in, which gets focus", () => {
    fc.assert(
      fc.property(steps, fc.nat(), fc.nat(), fc.nat({ max: 6 }), (run, pane, group, index) => {
        const before = run.reduce(apply, emptyLayout());
        const after = apply(before, { op: "move", pane, target: { type: "tab", group, index } });
        const moved = (Object.keys(before.panes).sort() as PaneId[])[
          pane % Object.keys(before.panes).length
        ];
        fc.pre(moved !== undefined);
        const into = groupOf(after, moved as PaneId);
        expect(into?.active).toBe(moved);
        expect(after.focused).toBe(into?.id);
      }),
    );
  });
});

describe("a saved layout", () => {
  it("loads as the layout that was saved", () => {
    fc.assert(
      fc.property(steps, (run) => {
        const layout = run.reduce(apply, emptyLayout());
        expect(loadLayout(saveLayout(layout))).toEqual({ ok: true, layout });
      }),
    );
  });

  it("is refused, never thrown on or half read, when it is damaged", () => {
    fc.assert(
      fc.property(steps, fc.nat(), fc.jsonValue(), (run, at, value) => {
        const saved = JSON.parse(saveLayout(run.reduce(apply, emptyLayout())));
        const places = paths(saved);
        const place = places[at % places.length] as (string | number)[];
        const damaged = place.length === 0 ? value : setAt(saved, place, value);
        const loaded = loadLayout(JSON.stringify(damaged));
        if (loaded.ok) {
          expect(checkLayout(loaded.layout)).toEqual([]);
        } else {
          expect(loaded.why).not.toBe("");
        }
      }),
    );
  });

  it("refuses text that is no layout, and layouts from a newer page", () => {
    expect(loadLayout("{").ok).toBe(false);
    expect(loadLayout("null").ok).toBe(false);
    const newer = JSON.stringify({ version: 2, layout: emptyLayout() });
    expect(loadLayout(newer)).toEqual({ ok: false, why: "it is from version 2, not up to 1" });
  });

  // Every version of the saved shape ever written stays loadable.
  it.each(["v1"])("of version %s loads", (version) => {
    const text = readFileSync(join(import.meta.dirname, "layouts", `${version}.json`), "utf8");
    const loaded = loadLayout(text);
    expect(loaded.ok && shape(loaded.layout)).toEqual({
      row: [
        [0.25, ["*threads", "tasks"]],
        [
          0.75,
          {
            column: [
              [0.7, ["*source"]],
              [0.3, ["*console"]],
            ],
          },
        ],
      ],
    });
  });
});

/** Every path to a value in parsed JSON. */
function paths(value: unknown, at: (string | number)[] = []): (string | number)[][] {
  const below =
    typeof value === "object" && value !== null
      ? Object.entries(value).flatMap(([key, inner]) =>
          paths(inner, [...at, Array.isArray(value) ? Number(key) : key]),
        )
      : [];
  return [at, ...below];
}

function setAt(value: unknown, place: (string | number)[], to: unknown): unknown {
  const [first, ...rest] = place;
  if (first === undefined) {
    return to;
  }
  const copy = (Array.isArray(value) ? [...value] : { ...(value as object) }) as Record<
    string | number,
    unknown
  >;
  copy[first] = setAt(copy[first], rest, to);
  return copy;
}
