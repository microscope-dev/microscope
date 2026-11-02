// Where a dragged splitter or floating window settles. Both snap to places a
// person is likely to want when the pointer comes near one, so layouts line
// up without care, and holding Alt turns snapping off.

import type { Rect } from "./layout";

/** The shares of a split its splitters snap to. */
export const FRACTIONS: readonly number[] = [1 / 4, 1 / 3, 1 / 2, 2 / 3, 3 / 4];

/** How near, in pixels, a splitter comes before it snaps. */
export const SPLITTER_SNAP = 6;

/** How near, in pixels, a window's edge comes to another before it snaps. */
export const WINDOW_SNAP = 8;

/** How much of a window always stays on screen, to grab it by. */
export const GRAB = 48;

export interface SplitterDrag {
  /** Where the split starts, and its length, along its axis. */
  readonly start: number;
  readonly length: number;
  /** Where the pointer puts the splitter. */
  readonly at: number;
  /** How far the splitter can go each way before a pane gets too small. */
  readonly least: number;
  readonly most: number;
  /** Splitters in neighboring splits along the same axis, to line up with. */
  readonly neighbors: readonly number[];
  /** Alt is held: no snapping. */
  readonly free: boolean;
}

/** Where a dragged splitter settles. When the panes can't all keep their
 * least size, the one before the splitter does. */
export function snapSplitter(drag: SplitterDrag): number {
  let settled = drag.at;
  if (!drag.free) {
    const places = [
      ...FRACTIONS.map((fraction) => drag.start + fraction * drag.length),
      ...drag.neighbors,
    ];
    settled = nearest(drag.at, places, SPLITTER_SNAP) ?? drag.at;
  }
  return Math.max(drag.least, Math.min(drag.most, settled));
}

/** Where a dragged window settles: its edges snap to the viewport's and to
 * other windows' within reach, and it keeps enough on screen to grab. */
export function snapWindow(
  rect: Rect,
  others: readonly Rect[],
  viewport: Rect,
  free: boolean,
): Rect {
  let { x, y } = rect;
  if (!free) {
    const edges = [viewport, ...others];
    const xs = edges.flatMap((edge) => [edge.x, edge.x + edge.width]);
    const ys = edges.flatMap((edge) => [edge.y, edge.y + edge.height]);
    x = snapEdges(x, rect.width, xs);
    y = snapEdges(y, rect.height, ys);
  }
  return keepOnScreen({ ...rect, x, y }, viewport);
}

/** A window moved so its title bar stays inside the viewport and at least
 * `GRAB` pixels of it across stay visible. */
export function keepOnScreen(rect: Rect, viewport: Rect): Rect {
  const x = Math.max(
    viewport.x + GRAB - rect.width,
    Math.min(viewport.x + viewport.width - GRAB, rect.x),
  );
  const y = Math.max(viewport.y, Math.min(viewport.y + viewport.height - GRAB, rect.y));
  return { ...rect, x, y };
}

/** Where a span starting at `start` settles so that one of its two edges
 * meets the nearest of `places` within reach. */
function snapEdges(start: number, size: number, places: readonly number[]): number {
  const before = nearest(start, places, WINDOW_SNAP);
  const after = nearest(start + size, places, WINDOW_SNAP);
  const moveBefore = before === null ? null : before - start;
  const moveAfter = after === null ? null : after - (start + size);
  if (moveBefore === null) {
    return moveAfter === null ? start : start + moveAfter;
  }
  if (moveAfter === null || Math.abs(moveBefore) <= Math.abs(moveAfter)) {
    return start + moveBefore;
  }
  return start + moveAfter;
}

/** The place nearest `value` within `reach`, the first on a tie. */
function nearest(value: number, places: readonly number[], reach: number): number | null {
  let found: number | null = null;
  for (const place of places) {
    const distance = Math.abs(place - value);
    if (distance <= reach && (found === null || distance < Math.abs(found - value))) {
      found = place;
    }
  }
  return found;
}
