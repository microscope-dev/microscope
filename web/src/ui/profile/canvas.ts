import { createEffect, createSignal } from "solid-js";

import { assert } from "../../assert";

export const NAMES = 180;
export const NAMES_STYLE = { width: `${NAMES}px` };

export function useWidth(): [(element: HTMLElement | null) => void, () => number] {
  const [element, setElement] = createSignal<HTMLElement | null>(null);
  const [width, setWidth] = createSignal(0);
  createEffect(
    () => element(),
    (current) => {
      if (!current) {
        return;
      }
      const observer = new ResizeObserver(() => setWidth(current.clientWidth));
      observer.observe(current);
      setWidth(current.clientWidth);
      return () => observer.disconnect();
    },
  );
  return [(node) => setElement(node), width];
}

export function prepare(
  element: HTMLCanvasElement | null | undefined,
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
