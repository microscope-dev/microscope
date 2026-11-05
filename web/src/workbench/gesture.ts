// A press that follows the pointer until it lets go: a tab or window drag,
// a separator, a window's resize grip. One runs at a time. It listens on the
// window rather than the pressed element, so it still ends when a re-render
// removes that element mid-gesture, and Escape cancels it wherever the focus
// is.

import type { PointerEvent as ReactPointerEvent } from "react";

export interface Gesture {
  readonly move: (event: PointerEvent) => void;
  /** The pointer let go, ending the gesture. */
  readonly up: (event: PointerEvent) => void;
  /** Escape, or the browser took the pointer away. Undo what `move` showed. */
  readonly cancel: () => void;
}

interface Tracked {
  readonly pointer: number;
  readonly captor: Element;
  readonly gesture: Gesture;
}

let current: Tracked | null = null;

/** Follows the pointer pressed in `event` with `gesture`, and says whether it
 * does: only a primary press starts one, and not while another runs. */
export function trackPointer(event: ReactPointerEvent<Element>, gesture: Gesture): boolean {
  if (event.button !== 0 || current) {
    return false;
  }
  // The element pressed, not the one listening, holds the pointer, so the
  // click that ends a short press still lands on the button pressed.
  const captor = event.target as Element;
  current = { pointer: event.pointerId, captor, gesture };
  captor.setPointerCapture(event.pointerId);
  window.addEventListener("pointermove", moved);
  window.addEventListener("pointerup", released);
  window.addEventListener("pointercancel", lost);
  window.addEventListener("lostpointercapture", lost);
  window.addEventListener("keydown", escaped, { capture: true });
  return true;
}

function ours(event: PointerEvent): Tracked | null {
  return current?.pointer === event.pointerId ? current : null;
}

function moved(event: PointerEvent): void {
  ours(event)?.gesture.move(event);
}

function released(event: PointerEvent): void {
  const tracked = ours(event);
  if (tracked) {
    stop(tracked);
    tracked.gesture.up(event);
  }
}

function lost(event: PointerEvent): void {
  const tracked = ours(event);
  if (tracked) {
    stop(tracked);
    tracked.gesture.cancel();
  }
}

function escaped(event: KeyboardEvent): void {
  if (event.key === "Escape" && current) {
    event.preventDefault();
    event.stopImmediatePropagation();
    const tracked = current;
    stop(tracked);
    tracked.gesture.cancel();
  }
}

function stop(tracked: Tracked): void {
  current = null;
  window.removeEventListener("pointermove", moved);
  window.removeEventListener("pointerup", released);
  window.removeEventListener("pointercancel", lost);
  window.removeEventListener("lostpointercapture", lost);
  window.removeEventListener("keydown", escaped, { capture: true });
  if (tracked.captor.hasPointerCapture(tracked.pointer)) {
    tracked.captor.releasePointerCapture(tracked.pointer);
  }
}
