// Resizable splits: a handle between two panes that a pointer drags or the
// arrow keys move. Each split remembers its size in this browser.

import { type HTMLAttributes, useCallback, useState } from "react";
import { read, write } from "../storage";

interface Limits {
  /** What the handle resizes, for screen readers. */
  label: string;
  min: number;
  max: number;
  /** Rows split top from bottom; columns split left from right. */
  axis?: "columns" | "rows";
  /** The size measured from the far side: dragging toward it shrinks it. */
  reversed?: boolean;
}

const isNumber = (value: unknown): value is number => typeof value === "number";

export function useSplit(
  key: string,
  initial: number,
  { label, min, max, axis = "columns", reversed = false }: Limits,
): { size: number; handle: HTMLAttributes<HTMLDivElement> } {
  const [size, setSize] = useState(() => clamp(read(key, initial, isNumber), min, max));
  const resize = useCallback(
    (next: number) => {
      const clamped = clamp(next, min, max);
      setSize(clamped);
      write(key, clamped);
    },
    [key, min, max],
  );
  const sign = reversed ? -1 : 1;
  const handle: HTMLAttributes<HTMLDivElement> = {
    className: `gutter ${axis}`,
    role: "separator",
    "aria-label": label,
    tabIndex: 0,
    "aria-orientation": axis === "columns" ? "vertical" : "horizontal",
    "aria-valuenow": size,
    "aria-valuemin": min,
    "aria-valuemax": max,
    onPointerDown: (event) => {
      event.preventDefault();
      const target = event.currentTarget;
      target.setPointerCapture(event.pointerId);
      const start = axis === "columns" ? event.clientX : event.clientY;
      const from = size;
      const move = (moved: PointerEvent) => {
        const now = axis === "columns" ? moved.clientX : moved.clientY;
        resize(from + sign * (now - start));
      };
      // A cancelled drag, as when the browser takes the pointer, ends it too.
      const up = () => {
        target.removeEventListener("pointermove", move);
        target.removeEventListener("pointerup", up);
        target.removeEventListener("pointercancel", up);
      };
      target.addEventListener("pointermove", move);
      target.addEventListener("pointerup", up);
      target.addEventListener("pointercancel", up);
    },
    onKeyDown: (event) => {
      const step = event.shiftKey ? 48 : 12;
      const keys = axis === "columns" ? ["ArrowLeft", "ArrowRight"] : ["ArrowUp", "ArrowDown"];
      const index = keys.indexOf(event.key);
      if (index >= 0) {
        event.preventDefault();
        resize(size + sign * (index === 0 ? -step : step));
      }
    },
  };
  return { size, handle };
}

function clamp(value: number, min: number, max: number): number {
  return Math.min(max, Math.max(min, value));
}
