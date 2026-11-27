export interface Gesture {
  readonly move: (event: PointerEvent) => void;

  readonly up: (event: PointerEvent) => void;

  readonly cancel: () => void;
}

interface Tracked {
  readonly pointer: number;
  readonly captor: Element;
  readonly gesture: Gesture;
}

let current: Tracked | null = null;

/**
 * Captures the primary pointer for a single active drag/resize gesture.
 * Listens for `Escape` in the capture phase with `stopImmediatePropagation`
 * so cancelling a gesture does not also dismiss surrounding UI.
 */
export function trackPointer(event: PointerEvent, gesture: Gesture): boolean {
  if (event.button !== 0 || current) {
    return false;
  }

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
