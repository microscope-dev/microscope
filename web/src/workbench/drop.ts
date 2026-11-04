// Where a dragged tab or window lands, from where the pointer is. It reads
// rectangles measured once when the drag starts, so a drag never makes the
// browser lay the page out again. Rectangles hold their left and top edges
// and not their right and bottom ones, so neighbors never both claim a point.

import type { GroupId, Rect, Side, Target } from "./layout";

/** The strip along each edge of the dock that docks across all of it. */
export const EDGE = 24;

/** The share of a group's body, from each side, that splits that side. */
export const BAND = 0.25;

/** How much of the dock a pane docked on an edge takes, as `move` gives it. */
const EDGE_SHARE = 0.25;

export interface MeasuredGroup {
  readonly id: GroupId;
  readonly strip: Rect;
  readonly body: Rect;
  /** Its tabs, left to right. */
  readonly tabs: readonly Rect[];
}

export interface Measured {
  readonly dock: Rect;
  /** The docked groups, which never overlap. */
  readonly docked: readonly MeasuredGroup[];
  /** Floating windows, bottom to top, less the one being dragged. */
  readonly floating: readonly MeasuredGroup[];
}

export interface Point {
  readonly x: number;
  readonly y: number;
}

/** A target, and the rectangle to highlight before the drop. */
export interface Drop {
  readonly target: Target;
  readonly highlight: Rect;
}

/** Where a drop at `point` lands, or null to float it there. A floating
 * window dragged by its title bar lands only on strips and the dock's
 * edges (`bodies` false), so dropping it anywhere else just moves it. */
export function resolveDrop(measured: Measured, point: Point, bodies = true): Drop | null {
  // Windows sit above the dock, the top one first.
  for (const window of measured.floating.toReversed()) {
    if (inside(window.strip, point)) {
      return gap(window, point);
    }
    if (inside(window.body, point)) {
      return bodies ? whole(window) : null;
    }
  }
  // Strips come before the dock's edges, whose top one they would cover.
  for (const group of measured.docked) {
    if (inside(group.strip, point)) {
      return gap(group, point);
    }
  }
  const edge = nearestSide(measured.dock, point, EDGE, EDGE);
  if (edge) {
    return {
      target: { type: "edge", side: edge },
      highlight: part(measured.dock, edge, EDGE_SHARE),
    };
  }
  if (!bodies) {
    return null;
  }
  for (const group of measured.docked) {
    if (inside(group.body, point)) {
      const { body } = group;
      const side = nearestSide(body, point, body.width * BAND, body.height * BAND);
      if (!side) {
        return whole(group);
      }
      return {
        target: { type: "side", group: group.id, side },
        highlight: part(around(group), side, 0.5),
      };
    }
  }
  return null;
}

/** Into a strip, at the gap nearest the pointer. */
function gap(group: MeasuredGroup, point: Point): Drop {
  const index = group.tabs.filter((tab) => tab.x + tab.width / 2 <= point.x).length;
  const before = group.tabs[index - 1];
  const x =
    index === 0 ? (group.tabs[0]?.x ?? group.strip.x) : (before as Rect).x + (before as Rect).width;
  return {
    target: { type: "tab", group: group.id, index },
    highlight: { x: x - 1, y: group.strip.y, width: 2, height: group.strip.height },
  };
}

/** Onto a group's body: a tab at the end of its strip. */
function whole(group: MeasuredGroup): Drop {
  return {
    target: { type: "tab", group: group.id, index: group.tabs.length },
    highlight: group.body,
  };
}

function inside(rect: Rect, point: Point): boolean {
  return (
    point.x >= rect.x &&
    point.x < rect.x + rect.width &&
    point.y >= rect.y &&
    point.y < rect.y + rect.height
  );
}

/** The side of `rect` whose band `point` is in. Bands are `across` wide on
 * the left and right and `down` deep on the top and bottom. Where two
 * overlap, at a corner, the side the point is nearer, for the size of its
 * band, wins; on a tie, the first of left, right, top, bottom. */
function nearestSide(rect: Rect, point: Point, across: number, down: number): Side | null {
  if (!inside(rect, point)) {
    return null;
  }
  const depths: [Side, number][] = [
    ["left", (point.x - rect.x) / across],
    ["right", (rect.x + rect.width - point.x) / across],
    ["top", (point.y - rect.y) / down],
    ["bottom", (rect.y + rect.height - point.y) / down],
  ];
  let nearest: [Side, number] | null = null;
  for (const depth of depths) {
    if (depth[1] < 1 && (!nearest || depth[1] < nearest[1])) {
      nearest = depth;
    }
  }
  return nearest?.[0] ?? null;
}

/** The share of `rect` along one side. */
function part(rect: Rect, side: Side, share: number): Rect {
  const width = rect.width * share;
  const height = rect.height * share;
  switch (side) {
    case "left":
      return { ...rect, width };
    case "right":
      return { ...rect, x: rect.x + rect.width - width, width };
    case "top":
      return { ...rect, height };
    case "bottom":
      return { ...rect, y: rect.y + rect.height - height, height };
  }
}

/** A group's whole rectangle, its strip and body together. */
function around(group: MeasuredGroup): Rect {
  const x = Math.min(group.strip.x, group.body.x);
  const y = Math.min(group.strip.y, group.body.y);
  const right = Math.max(group.strip.x + group.strip.width, group.body.x + group.body.width);
  const bottom = Math.max(group.strip.y + group.strip.height, group.body.y + group.body.height);
  return { x, y, width: right - x, height: bottom - y };
}

export type Direction = "left" | "right" | "up" | "down";

/** The group beside `from` in a direction: of those wholly that way and
 * overlapping it across, the nearest, then the most overlapping. */
export function neighbor(
  rects: ReadonlyMap<GroupId, Rect>,
  from: GroupId,
  direction: Direction,
): GroupId | null {
  const origin = rects.get(from);
  if (!origin) {
    return null;
  }
  const across = direction === "left" || direction === "right";
  let best: { id: GroupId; gap: number; overlap: number } | null = null;
  for (const [id, rect] of rects) {
    if (id === from) {
      continue;
    }
    const apart = {
      left: origin.x - (rect.x + rect.width),
      right: rect.x - (origin.x + origin.width),
      up: origin.y - (rect.y + rect.height),
      down: rect.y - (origin.y + origin.height),
    }[direction];
    const overlap = across
      ? Math.min(origin.y + origin.height, rect.y + rect.height) - Math.max(origin.y, rect.y)
      : Math.min(origin.x + origin.width, rect.x + rect.width) - Math.max(origin.x, rect.x);
    // A pixel either way, for borders that round apart.
    if (apart < -1 || overlap <= 0) {
      continue;
    }
    if (
      !best ||
      apart < best.gap - 1 ||
      (Math.abs(apart - best.gap) <= 1 && overlap > best.overlap)
    ) {
      best = { id, gap: apart, overlap };
    }
  }
  return best?.id ?? null;
}
