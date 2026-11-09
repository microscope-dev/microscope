// Layouts as a person makes them: by a run of operations from an empty one.
// An operation names panes, groups, and splits by position, read against the
// layout it is applied to, so every generated run is a plausible one and
// shrinks to a shorter run.

import * as fc from "fast-check";

import {
  activate,
  close,
  dock,
  dockBeside,
  emptyLayout,
  float,
  groups,
  type KindId,
  type Layout,
  type LinkId,
  maximize,
  move,
  open,
  openLinked,
  type PaneId,
  type Rect,
  reframe,
  resize,
  restore,
  setLink,
  type Side,
  split,
  splits,
  type Target,
  toggleHidden,
} from "../../src/workbench/layout";

export type Step =
  | { op: "open"; kind: string; key: string }
  | { op: "openLinked"; key: string; value: number }
  | { op: "openOnLink"; link: number; key: string }
  | { op: "setLink"; link: number; value: number }
  | { op: "close" | "activate"; pane: number }
  | { op: "move"; pane: number; target: TargetChoice }
  | { op: "split"; group: number; side: Side }
  | { op: "resize"; split: number; weights: number[] }
  | { op: "float"; pane: number; rect: Rect }
  | { op: "dock"; window: number; target: TargetChoice }
  | { op: "reframe"; window: number; rect: Rect }
  | { op: "maximize"; group: number }
  | { op: "hide"; group: number; side: "left" | "bottom" }
  | { op: "restore" };

type TargetChoice =
  | { type: "tab"; group: number; index: number }
  | { type: "side"; group: number; side: Side }
  | { type: "edge"; side: Side };

const side = fc.constantFrom<Side>("left", "right", "top", "bottom");
const choice = fc.nat({ max: 50 });

const rect: fc.Arbitrary<Rect> = fc.record({
  x: fc.integer({ min: -200, max: 2000 }),
  y: fc.integer({ min: -200, max: 1200 }),
  width: fc.integer({ min: 1, max: 1200 }),
  height: fc.integer({ min: 1, max: 900 }),
});

const target: fc.Arbitrary<TargetChoice> = fc.oneof(
  fc.record({ type: fc.constant("tab" as const), group: choice, index: fc.nat({ max: 8 }) }),
  fc.record({ type: fc.constant("side" as const), group: choice, side }),
  fc.record({ type: fc.constant("edge" as const), side }),
);

export const step: fc.Arbitrary<Step> = fc.oneof(
  {
    weight: 6,
    arbitrary: fc.record({
      op: fc.constant("open" as const),
      kind: fc.constantFrom("source", "registers", "memory"),
      key: fc.constantFrom("", "a", "b", "c"),
    }),
  },
  {
    weight: 2,
    arbitrary: fc.record({
      op: fc.constant("openLinked" as const),
      key: fc.constantFrom("a", "b", "c"),
      value: fc.nat({ max: 3 }),
    }),
  },
  {
    weight: 2,
    arbitrary: fc.record({
      op: fc.constant("openOnLink" as const),
      link: choice,
      key: fc.constantFrom("a", "b", "c"),
    }),
  },
  {
    weight: 1,
    arbitrary: fc.record({
      op: fc.constant("setLink" as const),
      link: choice,
      value: fc.nat({ max: 3 }),
    }),
  },
  {
    weight: 1,
    arbitrary: fc.record({
      op: fc.constantFrom("close" as const, "activate" as const),
      pane: choice,
    }),
  },
  { weight: 4, arbitrary: fc.record({ op: fc.constant("move" as const), pane: choice, target }) },
  { weight: 2, arbitrary: fc.record({ op: fc.constant("split" as const), group: choice, side }) },
  {
    weight: 1,
    arbitrary: fc.record({
      op: fc.constant("resize" as const),
      split: choice,
      weights: fc.array(fc.double({ min: 0.01, max: 10, noNaN: true }), {
        minLength: 2,
        maxLength: 5,
      }),
    }),
  },
  { weight: 2, arbitrary: fc.record({ op: fc.constant("float" as const), pane: choice, rect }) },
  { weight: 2, arbitrary: fc.record({ op: fc.constant("dock" as const), window: choice, target }) },
  {
    weight: 1,
    arbitrary: fc.record({ op: fc.constant("reframe" as const), window: choice, rect }),
  },
  { weight: 1, arbitrary: fc.record({ op: fc.constant("maximize" as const), group: choice }) },
  { weight: 1, arbitrary: fc.record({ op: fc.constant("restore" as const) }) },
  {
    weight: 2,
    arbitrary: fc.record({
      op: fc.constant("hide" as const),
      group: choice,
      side: fc.constantFrom("left" as const, "bottom" as const),
    }),
  },
);

// Long runs, which build layouts several splits deep; short ones are still
// reached by shrinking.
export const steps = fc.array(step, { maxLength: 40, size: "max" });

function pick<T>(list: readonly T[], index: number): T | undefined {
  return list.length === 0 ? undefined : list[index % list.length];
}

function paneAt(layout: Layout, index: number): PaneId | undefined {
  return pick(Object.keys(layout.panes).toSorted() as PaneId[], index);
}

function linkAt(layout: Layout, index: number): LinkId | undefined {
  return pick(Object.keys(layout.links).toSorted() as LinkId[], index);
}

function targetAt(layout: Layout, chosen: TargetChoice): Target | undefined {
  if (chosen.type === "edge") {
    return chosen;
  }
  const group = pick(groups(layout), chosen.group);
  return group && { ...chosen, group: group.id };
}

/** Applies one step, or leaves the layout be when it names nothing there. */
export function apply(layout: Layout, chosen: Step): Layout {
  switch (chosen.op) {
    case "open":
      return open(layout, { kind: chosen.kind as KindId, key: chosen.key, params: chosen.key })
        .layout;
    case "openLinked":
      return openLinked(
        layout,
        (link) => ({ kind: "linked" as KindId, key: chosen.key, params: { link } }),
        chosen.value,
      ).layout;
    case "openOnLink": {
      const link = linkAt(layout, chosen.link);
      return link
        ? open(layout, { kind: "reader" as KindId, key: chosen.key, params: { link } }).layout
        : layout;
    }
    case "setLink": {
      const link = linkAt(layout, chosen.link);
      return link ? setLink(layout, link, chosen.value) : layout;
    }
    case "close":
    case "activate": {
      const pane = paneAt(layout, chosen.pane);
      return pane ? (chosen.op === "close" ? close : activate)(layout, pane) : layout;
    }
    case "move": {
      const pane = paneAt(layout, chosen.pane);
      const where = targetAt(layout, chosen.target);
      return pane && where ? move(layout, pane, where) : layout;
    }
    case "split": {
      const group = pick(groups(layout), chosen.group);
      return group ? split(layout, group.id, chosen.side) : layout;
    }
    case "resize": {
      const found = pick(splits(layout.root), chosen.split);
      return found
        ? resize(layout, found.id, chosen.weights.slice(0, found.children.length))
        : layout;
    }
    case "float": {
      const pane = paneAt(layout, chosen.pane);
      return pane ? float(layout, pane, chosen.rect) : layout;
    }
    case "dock": {
      const window = pick(layout.floating, chosen.window);
      const where = targetAt(layout, chosen.target);
      return window && where ? dock(layout, window.group.id, where) : layout;
    }
    case "reframe": {
      const window = pick(layout.floating, chosen.window);
      return window ? reframe(layout, window.group.id, chosen.rect) : layout;
    }
    case "maximize": {
      const group = pick(groups(layout), chosen.group);
      return group ? maximize(layout, group.id) : layout;
    }
    case "restore":
      return restore(layout);
    case "hide": {
      const group = pick(groups(layout), chosen.group);
      const beside = group && dockBeside(layout, group.id, chosen.side);
      return group && beside ? toggleHidden(layout, beside.id, group.id) : layout;
    }
  }
}

/** The layout a run of operations makes from an empty one. */
export function runLayout(run: readonly Step[]): Layout {
  return run.reduce((made, chosen) => apply(made, chosen), emptyLayout());
}

/** A layout some run of operations made. */
export const layout: fc.Arbitrary<Layout> = steps.map((run) => runLayout(run));
