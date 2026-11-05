// Docking as a person does it, with the pointer and keys: dragging a tab onto
// a side, a strip, or the dock's edge, floating one, moving and docking a
// window, reopening a closed tab, and Escape giving a drag up. A model-based
// test does long runs of these to the real dock and to the layout model
// alike, and the tabs on screen must match the model's after every one.

import { cleanup, render } from "@testing-library/react";
import * as fc from "fast-check";
import { act } from "react";
import { afterEach, expect, it } from "vitest";
import { page, userEvent } from "vitest/browser";

import "../src/styles.css";
import { Dock } from "../src/workbench/Dock";
import { kinds, type PaneKind } from "../src/workbench/kinds";
import {
  activate,
  build,
  close,
  dock,
  float,
  type Group,
  groupOf,
  groups,
  type KindId,
  type Layout,
  move,
  type Pane,
  type PaneId,
  type Side,
  type Target,
} from "../src/workbench/layout";
import { workbench } from "../src/workbench/store";
import { closeTab, reopenTab } from "../src/workbench/tabs";
import { readGroups } from "./driver/layout";

afterEach(cleanup);

// The model-based test acts synchronously, a step at a time, with act().
(globalThis as { IS_REACT_ACT_ENVIRONMENT?: boolean }).IS_REACT_ACT_ENVIRONMENT = true;

const KIND: PaneKind<string> = {
  id: "plain" as KindId,
  title: (pane) => pane.key,
  Body: ({ pane }) => <p>{pane.key}</p>,
  needs: null,
  keepAlive: true,
  closable: () => true,
};
const KINDS = kinds([KIND]);
const SETUP = { kinds: KINDS, useMissing: () => null };
const DOCK_BOX = { display: "flex", width: "900px", height: "500px" };

const pane = (key: string): Pane => ({ kind: KIND.id, key, params: null });

/** Three groups: two stacked on the left, one on the right. */
const START = () =>
  build({
    axis: "row",
    children: [
      [
        1,
        {
          axis: "column",
          children: [
            [1, [pane("Threads"), pane("Tasks")]],
            [1, [pane("Stack")]],
          ],
        },
      ],
      [2, [pane("Source"), pane("Memory"), pane("Registers")]],
    ],
  });

async function mount(layout: Layout = START()) {
  await page.viewport(1000, 600);
  workbench.setState({ layout, closed: [] });
  render(
    <div style={DOCK_BOX}>
      <Dock setup={SETUP} />
    </div>,
  );
}

const tabOf = (title: string) =>
  [...document.querySelectorAll<HTMLElement>('[role="tab"]')].find(
    (tab) => tab.querySelector("[data-part=tab-title]")?.textContent === title,
  ) as HTMLElement;

const middle = (element: Element) => {
  const box = element.getBoundingClientRect();
  return { x: box.x + box.width / 2, y: box.y + box.height / 2 };
};

/** Presses on `from`, moves past the drag threshold, and lets go at `to`;
 * or, `still`, lets go where it pressed. */
function drag(
  from: HTMLElement,
  to: { x: number; y: number },
  held: { shiftKey?: boolean; key?: string; still?: boolean } = {},
) {
  const start = middle(from);
  const at = (type: string, point: { x: number; y: number }) =>
    new PointerEvent(type, {
      bubbles: true,
      pointerId: 1,
      button: 0,
      buttons: type === "pointerup" ? 0 : 1,
      clientX: point.x,
      clientY: point.y,
      shiftKey: held.shiftKey ?? false,
    });
  act(() => {
    from.dispatchEvent(at("pointerdown", start));
    if (!held.still) {
      from.dispatchEvent(at("pointermove", { x: start.x + 10, y: start.y + 10 }));
      from.dispatchEvent(at("pointermove", to));
    }
    if (held.key) {
      window.dispatchEvent(new KeyboardEvent("keydown", { key: held.key, bubbles: true }));
    }
    from.dispatchEvent(at("pointerup", to));
    // As a browser does after a press and release on the same element.
    from.dispatchEvent(new MouseEvent("click", { bubbles: true, cancelable: true }));
  });
}

/** Each group's titles as the screen shows them: the dock's in reading
 * order, then the windows' in the order they were made. */
function expected(layout: Layout): string[][] {
  const titles = (group: Group) =>
    group.tabs.map((tab) => {
      const key = layout.panes[tab]?.key ?? "?";
      return tab === group.active ? `*${key}` : key;
    });
  const windows = layout.floating
    .map((window) => window.group)
    .toSorted((a, b) => (a.id < b.id ? -1 : 1));
  const floating = new Set(windows.map((group) => group.id));
  return [...groups(layout).filter((group) => !floating.has(group.id)), ...windows].map((group) =>
    titles(group),
  );
}

const groupElement = (id: string) => document.querySelector(`[data-group="${id}"]`) as HTMLElement;

/** A point in the middle of one side's band of a group's body. */
function sidePoint(id: string, side: Side) {
  const box = groupElement(id).getBoundingClientRect();
  const top = box.y + 28;
  const height = box.bottom - top;
  return {
    left: { x: box.x + box.width * 0.12, y: top + height / 2 },
    right: { x: box.right - box.width * 0.12, y: top + height / 2 },
    top: { x: box.x + box.width / 2, y: top + height * 0.12 },
    bottom: { x: box.x + box.width / 2, y: box.bottom - height * 0.12 },
  }[side];
}

const dockBox = () =>
  (document.querySelector("[data-part=dock]") as HTMLElement).getBoundingClientRect();

/** Whether a person can aim at `point` in `within`, as the screen shows
 * it: nothing else drawn over it, and, for a group's body, clear of its
 * strip and of the dock's edges, which take the drop first. */
function aims(point: { x: number; y: number }, within: string, body = false): boolean {
  const hit = document.elementFromPoint(point.x, point.y);
  if (!hit?.closest(within)) {
    return false;
  }
  if (!body) {
    return true;
  }
  const box = dockBox();
  const margin = 26;
  return (
    !hit.closest('[role="tablist"]') &&
    point.x > box.x + margin &&
    point.x < box.right - margin &&
    point.y > box.y + margin &&
    point.y < box.bottom - margin
  );
}

it("splits a group for a tab dropped on its side, and floats one dropped with Shift", async () => {
  await mount();
  drag(tabOf("Threads"), sidePoint("g7", "bottom"));
  expect(readGroups()).toEqual([
    ["*Tasks"],
    ["*Stack"],
    ["*Source", "Memory", "Registers"],
    ["*Threads"],
  ]);
  const halves = ["Source", "Threads"].map((title) =>
    (tabOf(title).closest("[data-group]") as HTMLElement).getBoundingClientRect(),
  );
  expect(halves[0]?.bottom).toBeCloseTo(halves[1]?.top ?? 0, -1);
  expect(halves[0]?.height).toBeCloseTo(halves[1]?.height ?? 0, -1);

  drag(tabOf("Memory"), { x: 600, y: 300 }, { shiftKey: true });
  const window = page.getByRole("dialog", { name: "Memory" });
  await expect.element(window).toBeVisible();
  expect(readGroups().at(-1)).toEqual(["*Memory"]);
});

it("drags a tab pressed on its close button, without closing it", async () => {
  await mount();
  const button = tabOf("Threads").parentElement?.querySelector(
    '[aria-label="Close Threads"]',
  ) as HTMLElement;
  drag(button, sidePoint("g7", "bottom"));
  expect(readGroups()).toEqual([
    ["*Tasks"],
    ["*Stack"],
    ["*Source", "Memory", "Registers"],
    ["*Threads"],
  ]);
});

const pointer = (type: string, point: { x: number; y: number }) =>
  new PointerEvent(type, { bubbles: true, pointerId: 1, clientX: point.x, clientY: point.y });

it("ends a drag whose tab closes under it, and drags again after", async () => {
  await mount();
  // Closed while pressed, and while dragging: the release lands on whatever
  // is under the pointer now that the tab is gone.
  for (const [title, moved] of [
    ["Threads", false],
    ["Tasks", true],
  ] as const) {
    const tab = tabOf(title);
    const start = middle(tab);
    act(() => {
      tab.dispatchEvent(pointer("pointerdown", start));
      if (moved) {
        tab.dispatchEvent(pointer("pointermove", { x: start.x + 40, y: start.y + 40 }));
      }
    });
    act(() => closeTab(KINDS, tab.id.slice("tab-".length) as PaneId));
    act(() => document.body.dispatchEvent(pointer("pointerup", sidePoint("g7", "bottom"))));
    expect(document.querySelector("[data-part=drag-ghost]")).toBeNull();
    expect(document.querySelector("[data-part=drop-highlight]")).toBeNull();
  }
  expect(readGroups()).toEqual([["*Stack"], ["*Source", "Memory", "Registers"]]);

  drag(tabOf("Memory"), sidePoint("g7", "bottom"));
  expect(readGroups()).toEqual([["*Stack"], ["*Source", "Registers"], ["*Memory"]]);
});

it("gives up a drag on Escape, leaving every tab where it was", async () => {
  await mount();
  const before = readGroups();
  drag(tabOf("Threads"), sidePoint("g7", "right"), { key: "Escape" });
  expect(readGroups()).toEqual(before);
  expect(document.querySelector("[data-part=drop-highlight]")).toBeNull();
  // A press that never moves far is a click, and chooses the tab.
  drag(tabOf("Tasks"), middle(tabOf("Tasks")), { still: true });
  expect(readGroups()[0]).toEqual(["Threads", "*Tasks"]);
});

it("moves a window by either header row, docks it on a strip, and reopens a closed tab there", async () => {
  await mount();
  drag(tabOf("Memory"), { x: 600, y: 300 }, { shiftKey: true });
  const bar = document.querySelector("[data-part=window-bar]") as HTMLElement;
  const window = document.querySelector("[data-part=window]") as HTMLElement;
  const before = window.getBoundingClientRect();
  // Onto a body, which a window only moves over.
  drag(bar, { x: middle(bar).x - 150, y: middle(bar).y + 40 });
  const after = (
    document.querySelector("[data-part=window]") as HTMLElement
  ).getBoundingClientRect();
  expect(after.x).toBeLessThan(before.x - 100);
  expect(readGroups()).toHaveLength(4);

  const windowStrip = window.querySelector('[role="tablist"]') as HTMLElement;
  const point = middle(windowStrip);
  drag(windowStrip, { x: point.x - 60, y: point.y - 30 });
  expect(window.getBoundingClientRect().x).toBeCloseTo(after.x - 60);
  expect(window.getBoundingClientRect().y).toBeCloseTo(after.y - 30);
  expect(readGroups()).toHaveLength(4);

  // Onto the Stack group's strip, past its tab.
  const strip = groupElement("g5").querySelector('[role="tablist"]') as HTMLElement;
  const box = strip.getBoundingClientRect();
  drag(document.querySelector("[data-part=window-bar]") as HTMLElement, {
    x: box.right - 4,
    y: box.y + 10,
  });
  expect(document.querySelector("[data-part=window]")).toBeNull();
  expect(readGroups()[1]).toEqual(["Stack", "*Memory"]);

  // A tab's id is its pane's, after "tab-".
  act(() => closeTab(KINDS, tabOf("Stack").id.slice("tab-".length) as PaneId));
  expect(readGroups()[1]).toEqual(["*Memory"]);
  act(() => void reopenTab(KINDS));
  expect(readGroups()[1]).toEqual(["*Stack", "Memory"]);
});

it("resizes a floating window by keyboard and cancels an interrupted pointer resize", async () => {
  await mount();
  drag(tabOf("Memory"), { x: 400, y: 180 }, { shiftKey: true });
  const window = document.querySelector("[data-part=window]") as HTMLElement;
  const grip = document.querySelector("[data-part=window-grip]") as HTMLElement;
  const before = window.getBoundingClientRect();
  act(() => grip.focus());
  await act(() => userEvent.keyboard("{ArrowLeft}"));
  await act(() => userEvent.keyboard("{ArrowUp}"));
  const resized = window.getBoundingClientRect();
  expect(resized.width).toBe(before.width - 16);
  expect(resized.height).toBe(before.height - 16);

  const layout = workbench.getState().layout;
  for (const end of ["pointercancel", "lostpointercapture", "Escape"]) {
    act(() => {
      grip.dispatchEvent(new PointerEvent("pointerdown", { bubbles: true, pointerId: 1 }));
      grip.dispatchEvent(
        new PointerEvent("pointermove", {
          bubbles: true,
          pointerId: 1,
          clientX: -80,
          clientY: -40,
        }),
      );
    });
    expect(window.getBoundingClientRect().width).toBe(resized.width - 80);
    act(() => {
      grip.dispatchEvent(
        end === "Escape"
          ? new KeyboardEvent("keydown", { bubbles: true, key: "Escape" })
          : new PointerEvent(end, { bubbles: true, pointerId: 1 }),
      );
    });
    expect(window.getBoundingClientRect().width).toBe(resized.width);
    expect(window.getBoundingClientRect().height).toBe(resized.height);
    expect(workbench.getState().layout).toBe(layout);
  }
});

it("keeps docked separators behind a floating window", async () => {
  const start = START();
  const memory = Object.keys(start.panes).find(
    (id) => start.panes[id as PaneId]?.key === "Memory",
  ) as PaneId;
  await mount(float(start, memory, { x: 200, y: 150, width: 400, height: 300 }));
  const window = document.querySelector("[data-part=window]") as HTMLElement;
  const box = window.getBoundingClientRect();
  const separators = [...document.querySelectorAll('[role="separator"]')];
  expect(separators).toHaveLength(2);
  for (const separator of separators) {
    const line = separator.getBoundingClientRect();
    const left = Math.max(box.left, line.left);
    const right = Math.min(box.right, line.right);
    const top = Math.max(box.top, line.top);
    const bottom = Math.min(box.bottom, line.bottom);
    expect(right).toBeGreaterThan(left);
    expect(bottom).toBeGreaterThan(top);
    expect(
      document
        .elementFromPoint((left + right) / 2, (top + bottom) / 2)
        ?.closest("[data-part=window]"),
    ).toBe(window);
  }
});

it("drops onto the raised floating window, not the one behind it", async () => {
  const start = START();
  const find = (name: string) =>
    Object.keys(start.panes).find((id) => start.panes[id as PaneId]?.key === name) as PaneId;
  const memory = find("Memory");
  const registers = find("Registers");
  const rect = { x: 350, y: 100, width: 400, height: 260 };
  const two = float(float(start, memory, rect), registers, rect);
  await mount(activate(two, memory));
  const strip = tabOf("Memory").closest('[role="tablist"]') as HTMLElement;
  drag(tabOf("Threads"), middle(strip));
  const after = workbench.getState().layout;
  expect(groupOf(after, find("Threads"))?.id).toBe(groupOf(after, memory)?.id);
  expect(groupOf(after, registers)?.tabs).toEqual([registers]);
});

/** What a person does, with indexes read against the layout as it is. */
type Action =
  | { type: "choose" | "close"; tab: number }
  | { type: "side"; tab: number; group: number; side: Side }
  | { type: "strip"; tab: number; group: number; along: number }
  | { type: "edge"; tab: number; side: Side }
  | { type: "float"; tab: number; x: number; y: number }
  | { type: "dock"; window: number };

const index = fc.nat({ max: 30 });
const side = fc.constantFrom<Side>("left", "right", "top", "bottom");
const action: fc.Arbitrary<Action> = fc.oneof(
  fc.record({ type: fc.constantFrom("choose" as const, "close" as const), tab: index }),
  fc.record({ type: fc.constant("side" as const), tab: index, group: index, side }),
  fc.record({
    type: fc.constant("strip" as const),
    tab: index,
    group: index,
    along: fc.double({ min: 0, max: 0.99, noNaN: true }),
  }),
  fc.record({ type: fc.constant("edge" as const), tab: index, side }),
  fc.record({
    type: fc.constant("float" as const),
    tab: index,
    x: fc.integer({ min: 100, max: 800 }),
    y: fc.integer({ min: 60, max: 440 }),
  }),
  fc.record({ type: fc.constant("dock" as const), window: index }),
);

const pick = <T,>(list: readonly T[], at: number): T | undefined =>
  list[at % Math.max(list.length, 1)];

/** Does `chosen` on screen, and to the model, or neither when a person
 * could not: a target under a window, or nothing to act on. */
function perform(model: Layout, chosen: Action): Layout {
  const tabs = groups(model).flatMap((group) => group.tabs);
  const docked = groups(model).filter(
    (group) => !model.floating.some((w) => w.group.id === group.id),
  );
  if (chosen.type === "dock") {
    const window = pick(model.floating, chosen.window);
    if (!window) {
      return model;
    }
    const button = groupElement(window.group.id)
      .closest("[data-part=window]")
      ?.querySelector("[data-part=window-dock]");
    act(() => (button as HTMLElement).click());
    return dock(model, window.group.id, { type: "edge", side: "right" });
  }
  const tab = pick(tabs, chosen.tab);
  if (!tab) {
    return model;
  }
  const element = tabOf(model.panes[tab]?.key as string);
  // A tab scrolled out of its strip, or under a window, is not there to take.
  if (!aims(middle(element), `#${element.id}`)) {
    return model;
  }
  switch (chosen.type) {
    case "choose":
      act(() => element.click());
      return activate(model, tab);
    case "close": {
      const button = element.parentElement?.querySelector('[aria-label^="Close "]') as HTMLElement;
      act(() => button.click());
      return close(model, tab);
    }
    case "side":
    case "strip": {
      const into = pick(docked, chosen.group);
      if (!into) {
        return model;
      }
      let point: { x: number; y: number };
      let target: Target;
      const within = `[data-group="${into.id}"]`;
      if (chosen.type === "side") {
        point = sidePoint(into.id, chosen.side);
        target = { type: "side", group: into.id, side: chosen.side };
        if (!aims(point, within, true)) {
          return model;
        }
      } else {
        const strip = groupElement(into.id).querySelector('[role="tablist"]') as HTMLElement;
        const box = strip.getBoundingClientRect();
        // Anywhere along the strip: between two tabs, or past them all.
        point = { x: box.x + 2 + chosen.along * (box.width - 5), y: box.y + 10 };
        // Tabs that overflow the strip lie past its end.
        const before = [...strip.querySelectorAll("[data-part=tab]")].filter(
          (t) => middle(t).x <= point.x,
        );
        target = { type: "tab", group: into.id, index: before.length };
        if (!aims(point, `${within} [role="tablist"]`)) {
          return model;
        }
      }
      drag(element, point);
      return move(model, tab, target);
    }
    case "edge": {
      const box = dockBox();
      const point = {
        left: { x: box.x + 6, y: box.y + box.height / 2 },
        right: { x: box.right - 6, y: box.y + box.height / 2 },
        top: { x: box.x + box.width / 2, y: box.y + 6 },
        bottom: { x: box.x + box.width / 2, y: box.bottom - 6 },
      }[chosen.side];
      // Strips take a drop on them before the edge they lie along; a point a
      // few pixels from one is too close to call by eye.
      const near = [-4, 0, 4].flatMap((dx) =>
        [-4, 0, 4].map((dy) => document.elementFromPoint(point.x + dx, point.y + dy)),
      );
      if (
        near.some((hit) => !hit?.closest("[data-part=tree]") || hit.closest('[role="tablist"]'))
      ) {
        return model;
      }
      drag(element, point);
      return move(model, tab, { type: "edge", side: chosen.side });
    }
    case "float": {
      const box = dockBox();
      drag(element, { x: box.x + chosen.x, y: box.y + chosen.y }, { shiftKey: true });
      return float(model, tab, { x: 0, y: 0, width: 1, height: 1 });
    }
  }
}

it("keeps the screen and the layout model alike through any run of drags and clicks", async () => {
  await mount();
  fc.assert(
    fc.property(fc.array(action, { maxLength: 12 }), (actions) => {
      act(() => workbench.setState({ layout: START(), closed: [] }));
      let model = workbench.getState().layout;
      for (const chosen of actions) {
        model = perform(model, chosen);
        expect(readGroups()).toEqual(expected(model));
      }
    }),
  );
});
