import type { Rect } from "./layout";

export const FRACTIONS: readonly number[] = [1 / 4, 1 / 3, 1 / 2, 2 / 3, 3 / 4];

export const SPLITTER_SNAP = 6;

export const WINDOW_SNAP = 8;

export const GRAB = 48;

export interface SplitterDrag {
  readonly start: number;
  readonly length: number;

  readonly at: number;

  readonly least: number;
  readonly most: number;

  readonly neighbors: readonly number[];

  readonly free: boolean;
}

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

export function keepOnScreen(rect: Rect, viewport: Rect): Rect {
  const x = Math.max(
    viewport.x + GRAB - rect.width,
    Math.min(viewport.x + viewport.width - GRAB, rect.x),
  );
  const y = Math.max(viewport.y, Math.min(viewport.y + viewport.height - GRAB, rect.y));
  return { ...rect, x, y };
}

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
