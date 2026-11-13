// What the profile's charts share: the names column their lanes line up
// under, their width, and a canvas at the screen's resolution.

import { useCallback, useState } from "react";

import { assert } from "../../assert";

/** The names column beside each chart's lanes, so their times line up. */
export const NAMES = 180;
export const NAMES_STYLE = { width: NAMES };

/** A box's width in CSS pixels as it changes, and the ref that measures
 * it. A ref callback, so a box shown again after an error is measured. */
export function useWidth(): [(element: HTMLElement | null) => (() => void) | undefined, number] {
  const [width, setWidth] = useState(0);
  const measure = useCallback((element: HTMLElement | null) => {
    if (!element) {
      return;
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
  assert(height >= 0, `canvas height ${height} is negative`);
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
