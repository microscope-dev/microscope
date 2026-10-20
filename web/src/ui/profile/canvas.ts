// What the profile's charts share: the names column their lanes line up
// under, their width, a canvas at the screen's resolution, and selecting a
// time range by dragging across one.

import { useCallback, useMemo, useState } from "react";
import { formatRange, withLook } from "../../profile/look";
import { token, useProfile } from "./context";

/** The names column beside each chart's lanes, so their times line up. */
export const NAMES = 180;

/** A box's width in CSS pixels as it changes, and the ref that measures
 * it. A ref callback, so a box shown again after an error is measured. */
export function useWidth(): [(element: HTMLElement | null) => (() => void) | undefined, number] {
  const [width, setWidth] = useState(0);
  const measure = useCallback((element: HTMLElement | null) => {
    if (!element) {
      return undefined;
    }
    const observer = new ResizeObserver(() => setWidth(element.clientWidth));
    observer.observe(element);
    setWidth(element.clientWidth);
    return () => observer.disconnect();
  }, []);
  return [measure, width];
}

/** Prepares a canvas for drawing at the screen's resolution, cleared. */
export function prepare(
  element: HTMLCanvasElement | null,
  width: number,
  height: number,
): CanvasRenderingContext2D | null {
  const context = element?.getContext("2d");
  if (!element || !context || width <= 0) {
    return null;
  }
  const ratio = window.devicePixelRatio || 1;
  element.width = Math.round(width * ratio);
  element.height = Math.round(height * ratio);
  context.setTransform(ratio, 0, 0, ratio, 0, 0);
  context.clearRect(0, 0, width, height);
  return context;
}

/** Dragging across a chart of the whole profile selects a range; a click
 * clears it. Returns the handlers and the range being dragged. */
export function useRangeDrag(span: number) {
  const { setLook } = useProfile();
  const [dragging, setDragging] = useState<{ from: number; to: number } | null>(null);
  const time = (event: React.PointerEvent) => {
    const bounds = event.currentTarget.getBoundingClientRect();
    const fraction = (event.clientX - bounds.left) / Math.max(bounds.width, 1);
    return Math.min(Math.max(fraction, 0), 1) * span;
  };
  const handlers = {
    onPointerDown(event: React.PointerEvent<HTMLCanvasElement>) {
      event.currentTarget.setPointerCapture(event.pointerId);
      const at = time(event);
      setDragging({ from: at, to: at });
    },
    onPointerMove(event: React.PointerEvent<HTMLCanvasElement>) {
      if (dragging) {
        setDragging({ ...dragging, to: time(event) });
      }
    },
    onPointerUp(event: React.PointerEvent<HTMLCanvasElement>) {
      if (!dragging) {
        return;
      }
      const to = time(event);
      const [from, until] = [Math.min(dragging.from, to), Math.max(dragging.from, to)];
      const width = event.currentTarget.getBoundingClientRect().width;
      setDragging(null);
      // A click, not a drag, clears the selection.
      const chosen = (until - from) * (width / span) >= 3 ? formatRange([from, until]) : undefined;
      setLook((look) => withLook(look, "range", chosen), true);
    },
    onPointerCancel() {
      setDragging(null);
    },
  };
  const shown = useMemo(
    () =>
      dragging
        ? ([Math.min(dragging.from, dragging.to), Math.max(dragging.from, dragging.to)] as const)
        : null,
    [dragging],
  );
  return { handlers, shown };
}

/** Shades what lies outside the selected range. */
export function shade(
  context: CanvasRenderingContext2D,
  range: readonly [number, number] | null,
  span: number,
  width: number,
  height: number,
) {
  if (!range) {
    return;
  }
  const from = (width * range[0]) / span;
  const to = (width * range[1]) / span;
  context.fillStyle = token("--accent-soft");
  context.globalAlpha = 0.6;
  context.fillRect(0, 0, from, height);
  context.fillRect(to, 0, width - to, height);
  context.globalAlpha = 1;
}
