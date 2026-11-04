// Arranging panes by keyboard: what the Window menu does to the focused group
// and its shown tab. Each says why it cannot act, as its menu item does,
// so none of it needs a pointer.

import { panelId, tabId } from "./context";
import { FLOAT_SIZE, floatingRect, rememberSize } from "./drag";
import { type Direction, neighbor } from "./drop";
import {
  activate,
  dock,
  float,
  type Group,
  type GroupId,
  groups,
  type Layout,
  move,
  type PaneId,
  type Rect,
  type Side,
  split,
} from "./layout";
import { change, workbench } from "./store";

/** Null when the arrangement can be made, or why not. */
export type Why = string | null;

/** The focused group, and the tab it shows. */
export function focusedGroup(layout: Layout = workbench.getState().layout): Group | null {
  return groups(layout).find((group) => group.id === layout.focused) ?? null;
}

const floats = (layout: Layout, group: GroupId) =>
  layout.floating.some((window) => window.group.id === group);

/** Puts the keyboard in a pane: in the box it is for, if it has one. */
export function focusPane(pane: PaneId): void {
  const panel = document.querySelector<HTMLElement>(`#${panelId(pane)}`);
  (panel?.querySelector<HTMLElement>("[data-autofocus]") ?? panel)?.focus();
}

/** Puts the keyboard on a pane's tab. */
export function focusTab(pane: PaneId): void {
  document.querySelector<HTMLElement>(`#${tabId(pane)}`)?.focus();
}

/** The groups on screen, where they are. */
function onScreen(): Map<GroupId, Rect> {
  const rects = new Map<GroupId, Rect>();
  for (const element of document.querySelectorAll<HTMLElement>("[data-part=dock] [data-group]")) {
    const box = element.getBoundingClientRect();
    if (box.width > 0 && box.height > 0) {
      rects.set(element.dataset.group as GroupId, box);
    }
  }
  return rects;
}

export function besideWhy(direction: Direction): Why {
  const layout = workbench.getState().layout;
  if (layout.maximized !== null) {
    return "a group is maximized";
  }
  return neighbor(onScreen(), layout.focused, direction)
    ? null
    : `no group is ${toward(direction)}`;
}

/** Focuses the group beside the focused one. */
export function focusBeside(direction: Direction): void {
  const layout = workbench.getState().layout;
  const id = neighbor(onScreen(), layout.focused, direction);
  const group = groups(layout).find((candidate) => candidate.id === id);
  if (group?.active) {
    const pane = group.active;
    change((now) => activate(now, pane));
    focusPane(pane);
  }
}

export function sendWhy(direction: "left" | "right"): Why {
  const group = focusedGroup();
  return group?.active ? besideWhy(direction) : "no tab is shown";
}

/** Moves the shown tab to the end of the group beside it. */
export function sendBeside(direction: "left" | "right"): void {
  const layout = workbench.getState().layout;
  const pane = focusedGroup(layout)?.active;
  const id = neighbor(onScreen(), layout.focused, direction);
  const into = groups(layout).find((candidate) => candidate.id === id);
  if (pane && into) {
    change((now) => move(now, pane, { type: "tab", group: into.id, index: into.tabs.length }));
    focusTab(pane);
  }
}

export function shiftWhy(by: 1 | -1): Why {
  const group = focusedGroup();
  if (!group?.active) {
    return "no tab is shown";
  }
  const index = group.tabs.indexOf(group.active);
  if (by === -1 && index === 0) {
    return "the tab is first already";
  }
  return by === 1 && index === group.tabs.length - 1 ? "the tab is last already" : null;
}

/** Moves the shown tab one place along its strip. */
export function shiftTab(by: 1 | -1): void {
  const group = focusedGroup();
  const pane = group?.active;
  if (!group || !pane) {
    return;
  }
  const index = group.tabs.indexOf(pane);
  // A tab index counts the moving tab where it is now.
  const to = by === 1 ? index + 2 : index - 1;
  change((layout) => move(layout, pane, { type: "tab", group: group.id, index: to }));
  focusTab(pane);
}

export function splitWhy(layout: Layout = workbench.getState().layout): Why {
  const group = focusedGroup(layout);
  if (!group || group.tabs.length < 2) {
    return "a group needs two tabs to split";
  }
  return floats(layout, group.id) ? "a floating window does not split" : null;
}

/** Moves the shown tab into a new group beside its own. */
export function splitFocused(side: Side): void {
  const layout = workbench.getState().layout;
  const pane = focusedGroup(layout)?.active;
  change((now) => split(now, now.focused, side));
  if (pane) {
    focusPane(pane);
  }
}

export function floatWhy(narrow: boolean): Why {
  if (narrow) {
    return "the window is too narrow to float panes";
  }
  return focusedGroup()?.active ? null : "no tab is shown";
}

/** Floats the shown tab in the middle of the dock, or docks the focused
 * window along the dock's right edge. */
export function floatOrDock(): void {
  const layout = workbench.getState().layout;
  const group = focusedGroup(layout);
  const pane = group?.active;
  if (!group || !pane) {
    return;
  }
  if (floats(layout, group.id)) {
    rememberSize(layout, group.id);
    change((now) => dock(now, group.id, { type: "edge", side: "right" }));
  } else {
    const box = document.querySelector("[data-part=dock]")?.getBoundingClientRect();
    const bounds = box
      ? { x: box.x, y: box.y, width: box.width, height: box.height }
      : { x: 0, y: 0, width: window.innerWidth, height: window.innerHeight };
    // Each window a little below and right of the last, so none hides one.
    const step = 24 * layout.floating.length;
    const middle = {
      x: bounds.x + (bounds.width - FLOAT_SIZE.width) / 2 + 60 + step,
      y: bounds.y + (bounds.height - FLOAT_SIZE.height) / 2 + 14 + step,
    };
    change((now) => float(now, pane, floatingRect(pane, middle, bounds)));
  }
  focusPane(pane);
}

function toward(direction: Direction): string {
  return { left: "to the left", right: "to the right", up: "above", down: "below" }[direction];
}
