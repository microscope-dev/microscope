// Where drags land and settle: dropping a tab, and snapping splitters and
// floating windows.

import fc from "fast-check";
import { describe, expect, it } from "vitest";
import { type Measured, type Point, resolveDrop } from "../src/workbench/drop";
import type { GroupId, Rect } from "../src/workbench/layout";
import { GRAB, keepOnScreen, snapSplitter, snapWindow } from "../src/workbench/snap";

const A = "g1" as GroupId;
const B = "g2" as GroupId;
const W = "g3" as GroupId;

/** Two docked groups side by side, and a window floating over the right. */
const measured: Measured = {
  dock: { x: 0, y: 0, width: 1000, height: 600 },
  docked: [
    {
      id: A,
      strip: { x: 0, y: 0, width: 300, height: 30 },
      body: { x: 0, y: 30, width: 300, height: 570 },
      tabs: [
        { x: 0, y: 0, width: 100, height: 30 },
        { x: 100, y: 0, width: 100, height: 30 },
      ],
    },
    {
      id: B,
      strip: { x: 300, y: 0, width: 700, height: 30 },
      body: { x: 300, y: 30, width: 700, height: 570 },
      tabs: [{ x: 300, y: 0, width: 120, height: 30 }],
    },
  ],
  floating: [
    {
      id: W,
      strip: { x: 500, y: 200, width: 200, height: 24 },
      body: { x: 500, y: 224, width: 200, height: 150 },
      tabs: [{ x: 500, y: 200, width: 80, height: 24 }],
    },
  ],
};

describe("dropping a tab", () => {
  it.each<[string, Point, unknown]>([
    ["a strip, left of a tab's middle", { x: 40, y: 15 }, { type: "tab", group: A, index: 0 }],
    ["a strip, right of a tab's middle", { x: 60, y: 15 }, { type: "tab", group: A, index: 1 }],
    ["a strip, past its tabs", { x: 250, y: 15 }, { type: "tab", group: A, index: 2 }],
    ["a strip, on the dock's left edge", { x: 5, y: 15 }, { type: "tab", group: A, index: 0 }],
    ["the first pixel of the right group", { x: 300, y: 15 }, { type: "tab", group: B, index: 0 }],
    ["a body's middle", { x: 150, y: 300 }, { type: "tab", group: A, index: 2 }],
    ["a body's left quarter", { x: 60, y: 300 }, { type: "side", group: A, side: "left" }],
    ["a body's right quarter", { x: 240, y: 300 }, { type: "side", group: A, side: "right" }],
    ["a body's bottom quarter", { x: 150, y: 500 }, { type: "side", group: A, side: "bottom" }],
    ["a corner, nearer the left", { x: 320, y: 50 }, { type: "side", group: B, side: "left" }],
    ["a corner, nearer the top", { x: 360, y: 40 }, { type: "side", group: B, side: "top" }],
    ["the dock's left edge", { x: 5, y: 300 }, { type: "edge", side: "left" }],
    ["the dock's bottom edge", { x: 800, y: 590 }, { type: "edge", side: "bottom" }],
    ["a window's strip", { x: 550, y: 210 }, { type: "tab", group: W, index: 1 }],
    ["a window's body, over a docked one", { x: 600, y: 300 }, { type: "tab", group: W, index: 1 }],
  ])("onto %s", (_, point, target) => {
    expect(resolveDrop(measured, point)?.target).toEqual(target);
  });

  it("floats what lands outside every target", () => {
    expect(resolveDrop(measured, { x: 1000, y: 300 })).toBeNull();
    expect(resolveDrop(measured, { x: -1, y: 300 })).toBeNull();
  });

  it("highlights where the pane will go", () => {
    const highlight = (point: Point) => resolveDrop(measured, point)?.highlight;
    expect(highlight({ x: 60, y: 15 })).toEqual({ x: 99, y: 0, width: 2, height: 30 });
    expect(highlight({ x: 60, y: 300 })).toEqual({ x: 0, y: 0, width: 150, height: 600 });
    expect(highlight({ x: 150, y: 300 })).toEqual({ x: 0, y: 30, width: 300, height: 570 });
    expect(highlight({ x: 800, y: 590 })).toEqual({ x: 0, y: 450, width: 1000, height: 150 });
  });

  it("lands every point of the dock somewhere, inside the dock", () => {
    const point = fc.record({
      x: fc.double({ min: 0, max: 999.99, noNaN: true }),
      y: fc.double({ min: 0, max: 599.99, noNaN: true }),
    });
    fc.assert(
      fc.property(point, (at) => {
        const drop = resolveDrop(measured, at);
        expect(drop).not.toBeNull();
        const { x, y, width, height } = drop?.highlight as Rect;
        expect(x).toBeGreaterThanOrEqual(-1);
        expect(x + width).toBeLessThanOrEqual(1000);
        expect(y).toBeGreaterThanOrEqual(0);
        expect(y + height).toBeLessThanOrEqual(600);
      }),
    );
  });
});

describe("a dragged splitter", () => {
  const drag = { start: 100, length: 1000, least: 220, most: 980, neighbors: [517], free: false };

  it.each([
    ["snaps to a half within 6px", 594, 600],
    ["snaps to a third", 438, 100 + 1000 / 3],
    ["stays where it is beyond reach", 590, 590],
    ["lines up with a neighboring splitter", 520, 517],
    ["stops short of making a pane too small", 150, 220],
  ])("%s", (_, at, expected) => {
    expect(snapSplitter({ ...drag, at })).toBeCloseTo(expected, 9);
  });

  it("goes exactly where the pointer is while Alt is held", () => {
    expect(snapSplitter({ ...drag, at: 597, free: true })).toBe(597);
  });

  it("settles within its bounds, and within reach of the pointer when not stopped", () => {
    const bounded = fc
      .record({
        at: fc.double({ min: -100, max: 1300, noNaN: true }),
        least: fc.double({ min: 0, max: 600, noNaN: true }),
        span: fc.double({ min: 0, max: 600, noNaN: true }),
        neighbors: fc.array(fc.double({ min: 0, max: 1200, noNaN: true }), { maxLength: 3 }),
        free: fc.boolean(),
      })
      .map(({ least, span, ...rest }) => ({
        ...rest,
        least,
        most: least + span,
        start: 0,
        length: 1200,
      }));
    fc.assert(
      fc.property(bounded, (dragged) => {
        const settled = snapSplitter(dragged);
        expect(settled).toBeGreaterThanOrEqual(dragged.least);
        expect(settled).toBeLessThanOrEqual(dragged.most);
        if (settled > dragged.least && settled < dragged.most) {
          expect(Math.abs(settled - dragged.at)).toBeLessThanOrEqual(dragged.free ? 0 : 6);
        }
      }),
    );
  });
});

describe("a dragged floating window", () => {
  const viewport = { x: 0, y: 0, width: 1200, height: 800 };
  const other = { x: 500, y: 100, width: 200, height: 200 };

  it.each([
    ["snaps to the viewport's left", { x: 5, y: 400 }, { x: 0, y: 400 }],
    ["snaps its right edge to the viewport's", { x: 895, y: 400 }, { x: 900, y: 400 }],
    ["snaps to another window's edge", { x: 706, y: 400 }, { x: 700, y: 400 }],
    ["snaps its right edge to another window's left", { x: 197, y: 400 }, { x: 200, y: 400 }],
    ["stays beyond reach", { x: 750, y: 400 }, { x: 750, y: 400 }],
  ])("%s", (_, at, expected) => {
    const settled = snapWindow({ ...at, width: 300, height: 200 }, [other], viewport, false);
    expect({ x: settled.x, y: settled.y }).toEqual(expected);
  });

  it("keeps its title bar on screen and enough of it to grab", () => {
    expect(keepOnScreen({ x: -1000, y: -50, width: 300, height: 200 }, viewport)).toEqual({
      x: GRAB - 300,
      y: 0,
      width: 300,
      height: 200,
    });
    expect(keepOnScreen({ x: 5000, y: 5000, width: 300, height: 200 }, viewport)).toEqual({
      x: 1200 - GRAB,
      y: 800 - GRAB,
      width: 300,
      height: 200,
    });
  });

  it("can always be grabbed, wherever it is dropped", () => {
    const rect = fc.record({
      x: fc.double({ min: -5000, max: 5000, noNaN: true }),
      y: fc.double({ min: -5000, max: 5000, noNaN: true }),
      width: fc.double({ min: 1, max: 2000, noNaN: true }),
      height: fc.double({ min: 1, max: 2000, noNaN: true }),
    });
    fc.assert(
      fc.property(rect, fc.array(rect, { maxLength: 3 }), fc.boolean(), (window, others, free) => {
        const settled = snapWindow(window, others, viewport, free);
        const visible = Math.min(settled.x + settled.width, 1200) - Math.max(settled.x, 0);
        expect(visible).toBeGreaterThanOrEqual(Math.min(GRAB, settled.width) - 1e-9);
        expect(settled.y).toBeGreaterThanOrEqual(0);
        expect(settled.y).toBeLessThanOrEqual(800 - GRAB);
      }),
    );
  });
});
