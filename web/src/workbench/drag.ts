// Dragging a tab, or a floating window by its title bar. The page is measured
// once as the drag begins, the ghost and the highlight are moved by hand at
// most once a frame, and the layout changes once, at the drop, so a drag
// renders nothing until it ends.

import type { PointerEvent as ReactPointerEvent } from "react";

import { classOf } from "./classes";
import { type Drop, type Measured, type MeasuredGroup, type Point, resolveDrop } from "./drop";
import { trackPointer } from "./gesture";
import {
  dock,
  float,
  type GroupId,
  type Layout,
  move,
  type PaneId,
  type Rect,
  reframe,
} from "./layout";
import { keepOnScreen, snapWindow } from "./snap";
import { change, workbench } from "./store";

import styles from "./Dock.module.css";

const css = classOf(styles);

/** How far, in pixels, the pointer moves before a press becomes a drag, so
 * a click still chooses a tab. */
export const THRESHOLD = 4;

/** The size a pane floats at the first time. */
export const FLOAT_SIZE = { width: 480, height: 320 } as const;

/** The size each pane last floated at, to float it at again. */
const sizes = new Map<PaneId, { width: number; height: number }>();

type Source =
  | { readonly type: "tab"; readonly pane: PaneId; readonly title: string }
  | {
      readonly type: "window";
      readonly group: GroupId;
      readonly element: HTMLElement;
      /** Where the window was, in the dock's coordinates. */
      readonly rect: Rect;
    };

interface Drag {
  readonly source: Source;
  readonly dock: HTMLElement;
  readonly start: Point;
  point: Point;
  shift: boolean;
  alt: boolean;
  /** Set once the pointer has gone past the threshold. */
  measured: Measured | null;
  /** The layout measured, which a drop lands in only if it still holds. */
  layout: Layout | null;
  bounds: Rect;
  drop: Drop | null;
  frame: number | null;
  ghost: HTMLElement | null;
  highlight: HTMLElement | null;
}

/** Starts what may become a drag of a tab, from a press on it. */
export function pressTab(event: ReactPointerEvent<HTMLElement>, pane: PaneId, title: string): void {
  press(event, { type: "tab", pane, title });
}

/** Starts what may become a drag of a floating window, from a press on its
 * title bar. */
export function pressWindow(
  event: ReactPointerEvent<HTMLElement>,
  group: GroupId,
  element: HTMLElement,
  rect: Rect,
): void {
  press(event, { type: "window", group, element, rect });
}

/** Where a pane floats when dropped at `point`, in the dock's coordinates:
 * its title under the pointer, at the size it last floated at. */
export function floatingRect(pane: PaneId, point: Point, bounds: Rect): Rect {
  const size = sizes.get(pane) ?? FLOAT_SIZE;
  const width = Math.min(size.width, bounds.width);
  const height = Math.min(size.height, bounds.height);
  return keepOnScreen(
    { x: point.x - bounds.x - 60, y: point.y - bounds.y - 14, width, height },
    { x: 0, y: 0, width: bounds.width, height: bounds.height },
  );
}

/** Remembers a window's size for each of its panes, as it is docked. */
export function rememberSize(layout: Layout, window: GroupId): void {
  const found = layout.floating.find((candidate) => candidate.group.id === window);
  for (const pane of found?.group.tabs ?? []) {
    sizes.set(pane, { width: found?.rect.width ?? 0, height: found?.rect.height ?? 0 });
  }
}

function press(event: ReactPointerEvent<HTMLElement>, source: Source): void {
  const dockElement = event.currentTarget.closest<HTMLElement>("[data-part=dock]");
  if (!dockElement) {
    return;
  }
  const point = { x: event.clientX, y: event.clientY };
  const drag: Drag = {
    source,
    dock: dockElement,
    start: point,
    point,
    shift: event.shiftKey,
    alt: event.altKey,
    measured: null,
    layout: null,
    bounds: { x: 0, y: 0, width: 0, height: 0 },
    drop: null,
    frame: null,
    ghost: null,
    highlight: null,
  };
  trackPointer(event, {
    move: (moved) => follow(drag, moved),
    up: (released) => {
      aim(drag, released);
      if (drag.measured) {
        land(drag);
        // The click that ends a drag is not a click on the tab.
        window.addEventListener("click", swallow, { capture: true, once: true });
        setTimeout(() => window.removeEventListener("click", swallow, { capture: true }), 0);
      }
      end(drag);
    },
    cancel: () => end(drag),
  });
}

/** Takes in where the pointer is, starting the drag once it has gone far
 * enough, and shows the drop at most once a frame. */
function follow(drag: Drag, event: PointerEvent): void {
  aim(drag, event);
  if (!drag.measured) {
    const distance = Math.hypot(drag.point.x - drag.start.x, drag.point.y - drag.start.y);
    if (distance < THRESHOLD) {
      return;
    }
    begin(drag);
  }
  drag.frame ??= requestAnimationFrame(() => {
    drag.frame = null;
    show(drag);
  });
}

function aim(drag: Drag, event: PointerEvent): void {
  drag.point = { x: event.clientX, y: event.clientY };
  drag.shift = event.shiftKey;
  drag.alt = event.altKey;
}

function swallow(event: Event): void {
  event.preventDefault();
  event.stopPropagation();
}

function begin(drag: Drag): void {
  const dockRect = drag.dock.getBoundingClientRect();
  drag.bounds = { x: dockRect.x, y: dockRect.y, width: dockRect.width, height: dockRect.height };
  drag.measured = measure(drag);
  drag.layout = workbench.getState().layout;
  drag.dock.classList.add(css("dragging"));
  const highlight = document.createElement("div");
  highlight.className = css("drop-highlight");
  highlight.dataset.part = "drop-highlight";
  highlight.hidden = true;
  drag.highlight = highlight;
  drag.dock.append(highlight);
  if (drag.source.type === "tab") {
    const ghost = document.createElement("div");
    ghost.className = css("drag-ghost");
    ghost.dataset.part = "drag-ghost";
    ghost.textContent = drag.source.title;
    drag.ghost = ghost;
    drag.dock.append(ghost);
  }
}

/** Shows where the drop would land now. */
function show(drag: Drag): void {
  const { source, point } = drag;
  if (drag.ghost) {
    drag.ghost.style.transform = `translate(${point.x + 12}px, ${point.y + 8}px)`;
  }
  if (source.type === "window") {
    const rect = windowRect(drag);
    source.element.style.transform = `translate(${rect.x - source.rect.x}px, ${rect.y - source.rect.y}px)`;
  }
  const drop = target(drag);
  if (sameDrop(drop, drag.drop)) {
    return;
  }
  drag.drop = drop;
  const highlight = drag.highlight;
  if (!highlight) {
    return;
  }
  highlight.hidden = drop === null;
  if (drop) {
    // Measured in the page; drawn in the dock.
    const { x, y, width, height } = drop.highlight;
    Object.assign(highlight.style, {
      left: `${x - drag.bounds.x}px`,
      top: `${y - drag.bounds.y}px`,
      width: `${width}px`,
      height: `${height}px`,
    });
  }
}

/** Where the pointer is aimed: none when Shift floats a tab, and only strips
 * and edges for a window. */
function target(drag: Drag): Drop | null {
  const measured = drag.measured;
  if (!measured || (drag.source.type === "tab" && drag.shift)) {
    return null;
  }
  return resolveDrop(measured, drag.point, drag.source.type === "tab");
}

/** A dragged window's rectangle now, in the dock's coordinates, snapped. */
function windowRect(drag: Drag): Rect {
  const source = drag.source;
  if (source.type !== "window") {
    throw new Error("a tab has no window");
  }
  const shifted = {
    ...source.rect,
    x: source.rect.x + drag.point.x - drag.start.x,
    y: source.rect.y + drag.point.y - drag.start.y,
  };
  const others = workbench
    .getState()
    .layout.floating.filter((window) => window.group.id !== source.group)
    .map((window) => window.rect);
  const viewport = { x: 0, y: 0, width: drag.bounds.width, height: drag.bounds.height };
  return snapWindow(shifted, others, viewport, drag.alt);
}

function land(drag: Drag): void {
  // Something else changed the layout mid-drag, such as closing the dragged
  // tab, so the drop measured may name panes and groups that are gone.
  if (workbench.getState().layout !== drag.layout) {
    return;
  }
  const source = drag.source;
  const drop = target(drag);
  if (source.type === "tab") {
    change((layout) =>
      drop
        ? move(layout, source.pane, drop.target)
        : float(layout, source.pane, floatingRect(source.pane, drag.point, drag.bounds)),
    );
    return;
  }
  if (drop) {
    rememberSize(workbench.getState().layout, source.group);
    change((layout) => dock(layout, source.group, drop.target));
  } else {
    const rect = windowRect(drag);
    change((layout) => reframe(layout, source.group, rect));
  }
}

function end(drag: Drag): void {
  if (drag.frame !== null) {
    cancelAnimationFrame(drag.frame);
  }
  drag.ghost?.remove();
  drag.highlight?.remove();
  drag.dock.classList.remove(css("dragging"));
  if (drag.source.type === "window") {
    drag.source.element.style.transform = "";
  }
}

/** The dock's groups and windows as they are on screen, less what is
 * dragged. */
function measure(drag: Drag): Measured {
  const tree = [...drag.dock.querySelectorAll<HTMLElement>("[data-part=tree] [data-group]")];
  const dragged = drag.source.type === "window" ? drag.source.group : null;
  const windows = [
    ...drag.dock.querySelectorAll<HTMLElement>("[data-part=window] [data-group]"),
  ].filter((element) => element.dataset.group !== dragged);
  const stacked = workbench.getState().layout.floating.map((window) => window.group.id);
  windows.sort(
    (a, b) =>
      stacked.indexOf(a.dataset.group as GroupId) - stacked.indexOf(b.dataset.group as GroupId),
  );
  return {
    dock: drag.bounds,
    docked: tree.map((element) => measureGroup(element)),
    floating: windows.map((element) => measureGroup(element)),
  };
}

function measureGroup(element: HTMLElement): MeasuredGroup {
  const whole = element.getBoundingClientRect();
  const head = element.querySelector('[role="tablist"]')?.getBoundingClientRect() ?? whole;
  const tabs = [...element.querySelectorAll("[data-part=tab]")].map((tab) => toRect(tab));
  return {
    id: element.dataset.group as GroupId,
    strip: toRect(head),
    body: { x: whole.x, y: head.bottom, width: whole.width, height: whole.bottom - head.bottom },
    tabs,
  };
}

function toRect(of: Element | DOMRect): Rect {
  const box = of instanceof Element ? of.getBoundingClientRect() : of;
  return { x: box.x, y: box.y, width: box.width, height: box.height };
}

function sameDrop(a: Drop | null, b: Drop | null): boolean {
  return JSON.stringify(a) === JSON.stringify(b);
}
