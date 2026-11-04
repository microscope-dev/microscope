// Where every pane sits: a tree of splits whose leaves are groups of tabs,
// and floating windows above it. A layout is plain data, changed only by the
// operations below. Each returns a new layout that keeps every rule
// `checkLayout` lists, or the same layout when the operation makes no sense,
// such as splitting a group off itself. Pixels are not its business: weights
// say how a split shares its space, and the frame keeps floating windows on
// screen where it draws them.

import { assert } from "../assert";

declare const brand: unique symbol;
type Id<Name extends string> = string & { readonly [brand]: Name };

export type PaneId = Id<"pane">;
export type GroupId = Id<"group">;
export type SplitId = Id<"split">;
export type KindId = Id<"kind">;

export type Json =
  | null
  | boolean
  | number
  | string
  | readonly Json[]
  | { readonly [key: string]: Json };

export interface Rect {
  readonly x: number;
  readonly y: number;
  readonly width: number;
  readonly height: number;
}

export type Side = "left" | "right" | "top" | "bottom";

/** A row's children sit side by side; a column's are stacked. */
export type Axis = "row" | "column";

export interface Group {
  readonly type: "group";
  readonly id: GroupId;
  readonly tabs: readonly PaneId[];
  /** Null only when there are no tabs. */
  readonly active: PaneId | null;
}

export interface Split {
  readonly type: "split";
  readonly id: SplitId;
  readonly axis: Axis;
  readonly children: readonly Child[];
}

export interface Child {
  readonly node: Node;
  /** The share of the split's space, all of them summing to 1. */
  readonly weight: number;
}

export type Node = Group | Split;

export interface Floating {
  readonly group: Group;
  readonly rect: Rect;
}

export interface Pane {
  readonly kind: KindId;
  /** Tells panes of a kind apart: a file's path, or "" for a kind with one
   * pane. Opening a kind and key that is open already shows that pane. */
  readonly key: string;
  readonly params: Json;
}

export interface Layout {
  readonly root: Node;
  /** Bottom to top. */
  readonly floating: readonly Floating[];
  readonly maximized: GroupId | null;
  /** Docked nodes put out of sight, such as a side dock hidden to give the
   * code room. Never one holding the focused group. */
  readonly hidden: readonly (GroupId | SplitId)[];
  /** The group keys and new panes go to. */
  readonly focused: GroupId;
  readonly panes: Readonly<Record<PaneId, Pane>>;
  /** The number the next new id gets, so ids are never reused. */
  readonly next: number;
}

/** Where a pane or a floating window lands. A tab index counts the tabs as
 * the strip shows them, the moving pane among them. */
export type Target =
  | { readonly type: "tab"; readonly group: GroupId; readonly index: number }
  | { readonly type: "side"; readonly group: GroupId; readonly side: Side }
  | { readonly type: "edge"; readonly side: Side };

/** The share of the dock a pane docked on its edge takes. */
const EDGE_WEIGHT = 0.25;

export function emptyLayout(): Layout {
  const root: Group = { type: "group", id: "g0" as GroupId, tabs: [], active: null };
  return {
    root,
    floating: [],
    maximized: null,
    hidden: [],
    focused: root.id,
    panes: {},
    next: 1,
  };
}

/** Every group: the dock's in reading order, then the floating ones. */
export function groups(layout: Layout): Group[] {
  return [...docked(layout.root), ...layout.floating.map((window) => window.group)];
}

export function groupOf(layout: Layout, pane: PaneId): Group | null {
  return groups(layout).find((group) => group.tabs.includes(pane)) ?? null;
}

export function splits(node: Node): Split[] {
  return node.type === "group"
    ? []
    : [node, ...node.children.flatMap((child) => splits(child.node))];
}

function docked(node: Node): Group[] {
  return node.type === "group" ? [node] : node.children.flatMap((child) => docked(child.node));
}

/** Opens a pane in a group, the focused one unless another is named, after
 * its active tab, or shows the pane of the same kind and key when there is
 * one. */
export function open(
  layout: Layout,
  pane: Pane,
  group: GroupId = layout.focused,
): { layout: Layout; pane: PaneId } {
  const existing = paneOf(layout, pane.kind, pane.key);
  if (existing) {
    return { layout: activate(layout, existing), pane: existing };
  }
  const id = `p${layout.next}` as PaneId;
  const into = find(layout, group);
  const at = into.active === null ? into.tabs.length : into.tabs.indexOf(into.active) + 1;
  const opened = settle({
    ...replaceGroup(layout, { ...into, tabs: insert(into.tabs, at, [id]), active: id }),
    panes: { ...layout.panes, [id]: pane },
    next: layout.next + 1,
  });
  return { layout: opened, pane: id };
}

/** The open pane of a kind and key. */
export function paneOf(layout: Layout, kind: KindId, key: string): PaneId | null {
  const found = (Object.entries(layout.panes) as [PaneId, Pane][]).find(
    ([, pane]) => pane.kind === kind && pane.key === key,
  );
  return found?.[0] ?? null;
}

/** A layout to build: a split's children with their shares, or a group's
 * panes, the first of them shown. */
export type Shape =
  | { readonly axis: Axis; readonly children: readonly (readonly [number, Shape])[] }
  | readonly Pane[];

/** Builds a layout from a shape, its first group focused. */
export function build(shape: Shape): Layout {
  let next = 0;
  const panes: Record<PaneId, Pane> = {};
  const node = (part: Shape): Node => {
    if ("axis" in part) {
      const id = `s${next++}` as SplitId;
      const total = part.children.reduce((sum, [weight]) => sum + weight, 0);
      const children = part.children.map(([weight, child]) => ({
        node: node(child),
        weight: weight / total,
      }));
      return { type: "split", id, axis: part.axis, children };
    }
    assert(part.length > 0, "a group to build has no panes");
    const id = `g${next++}` as GroupId;
    const tabs = part.map((pane) => {
      const tab = `p${next++}` as PaneId;
      panes[tab] = pane;
      return tab;
    });
    return { type: "group", id, tabs, active: tabs[0] as PaneId };
  };
  const root = node(shape);
  const first = docked(root)[0] as Group;
  return settle({
    root,
    floating: [],
    maximized: null,
    hidden: [],
    focused: first.id,
    panes,
    next,
  });
}

/** Closes a pane. Its group shows the tab beside it, or goes when empty. */
export function close(layout: Layout, pane: PaneId): Layout {
  const group = groupOf(layout, pane);
  if (!group) {
    return layout;
  }
  const { [pane]: _, ...panes } = layout.panes;
  return settle({ ...replaceGroup(layout, without(group, pane)), panes });
}

/** Shows a pane in its group, focuses the group, and raises its window. */
export function activate(layout: Layout, pane: PaneId): Layout {
  const group = groupOf(layout, pane);
  if (!group) {
    return layout;
  }
  const shown = replaceGroup(layout, { ...group, active: pane });
  const index = shown.floating.findIndex((window) => window.group.id === group.id);
  const floating =
    index < 0
      ? shown.floating
      : [...shown.floating.filter((_, at) => at !== index), shown.floating[index] as Floating];
  return settle({ ...shown, floating, focused: group.id });
}

/** Moves a pane to a target, focusing the group it lands in. */
export function move(layout: Layout, pane: PaneId, target: Target): Layout {
  const from = groupOf(layout, pane);
  if (!from || !lands(layout, target, from, from.tabs.length === 1)) {
    return layout;
  }
  const lifted = replaceGroup(layout, without(from, pane));
  if (target.type === "tab") {
    const into = find(lifted, target.group);
    const shift = into.id === from.id && from.tabs.indexOf(pane) < target.index ? 1 : 0;
    const index = Math.min(target.index - shift, into.tabs.length);
    const placed = { ...into, tabs: insert(into.tabs, index, [pane]), active: pane };
    return settle({ ...replaceGroup(lifted, placed), focused: into.id });
  }
  const group: Group = {
    type: "group",
    id: `g${lifted.next}` as GroupId,
    tabs: [pane],
    active: pane,
  };
  return settle({ ...place(lifted, group, target), focused: group.id });
}

/** Moves a group's active tab into a new group on one side of it. */
export function split(layout: Layout, group: GroupId, side: Side): Layout {
  const found = groups(layout).find((candidate) => candidate.id === group);
  if (!found?.active || found.tabs.length < 2) {
    return layout;
  }
  return move(layout, found.active, { type: "side", group, side });
}

/** Shares a split's space anew. */
export function resize(layout: Layout, id: SplitId, weights: readonly number[]): Layout {
  const found = splits(layout.root).find((candidate) => candidate.id === id);
  if (
    !found ||
    weights.length !== found.children.length ||
    !weights.every((weight) => Number.isFinite(weight) && weight > 0)
  ) {
    return layout;
  }
  const total = weights.reduce((sum, weight) => sum + weight, 0);
  const children = found.children.map((child, index) => ({
    node: child.node,
    weight: (weights[index] as number) / total,
  }));
  return settle({ ...layout, root: replaceNode(layout.root, id, { ...found, children }) });
}

/** Floats a pane in a window of its own at `rect`, on top. A pane alone in
 * its window already moves the window instead. */
export function float(layout: Layout, pane: PaneId, rect: Rect): Layout {
  const from = groupOf(layout, pane);
  if (!from || !validRect(rect)) {
    return layout;
  }
  const own = layout.floating.find((window) => window.group.id === from.id);
  if (own && from.tabs.length === 1) {
    const others = layout.floating.filter((window) => window !== own);
    return settle({
      ...layout,
      floating: [...others, { group: own.group, rect }],
      focused: from.id,
    });
  }
  const lifted = replaceGroup(layout, without(from, pane));
  const group: Group = {
    type: "group",
    id: `g${lifted.next}` as GroupId,
    tabs: [pane],
    active: pane,
  };
  return settle({
    ...lifted,
    floating: [...lifted.floating, { group, rect }],
    focused: group.id,
    next: lifted.next + 1,
  });
}

/** Gives a pane new params, such as how a profile pane looks at its
 * profile; the same layout when they are the same. */
export function setParams(layout: Layout, pane: PaneId, params: Json): Layout {
  const found = layout.panes[pane];
  if (!found || JSON.stringify(found.params) === JSON.stringify(params)) {
    return layout;
  }
  return { ...layout, panes: { ...layout.panes, [pane]: { ...found, params } } };
}

/** Moves or resizes a floating window, and raises it. */
export function reframe(layout: Layout, window: GroupId, rect: Rect): Layout {
  const found = layout.floating.find((candidate) => candidate.group.id === window);
  if (!found || !validRect(rect)) {
    return layout;
  }
  const others = layout.floating.filter((candidate) => candidate !== found);
  return settle({
    ...layout,
    floating: [...others, { group: found.group, rect }],
    focused: window,
  });
}

/** Docks a floating window's group at a target: its tabs join a group, or
 * the whole group takes a side. */
export function dock(layout: Layout, window: GroupId, target: Target): Layout {
  const found = layout.floating.find((candidate) => candidate.group.id === window);
  const onto = target.type === "edge" ? null : target.group;
  if (!found || onto === window || !lands(layout, target, found.group, true)) {
    return layout;
  }
  const lifted = {
    ...layout,
    floating: layout.floating.filter((candidate) => candidate !== found),
  };
  if (target.type === "tab") {
    const into = find(lifted, target.group);
    const index = Math.min(target.index, into.tabs.length);
    const joined = {
      ...into,
      tabs: insert(into.tabs, index, found.group.tabs),
      active: found.group.active,
    };
    return settle({ ...replaceGroup(lifted, joined), focused: into.id });
  }
  return settle({ ...place(lifted, found.group, target), focused: found.group.id });
}

/** Fills the dock with one docked group until `restore`. */
export function maximize(layout: Layout, group: GroupId): Layout {
  if (!docked(layout.root).some((candidate) => candidate.id === group) || isHidden(layout, group)) {
    return layout;
  }
  return settle({ ...layout, maximized: group, focused: group });
}

export function restore(layout: Layout): Layout {
  return layout.maximized === null ? layout : { ...layout, maximized: null };
}

/** The dock beside a group's branch of the tree: the node before it in the
 * nearest row that has one (`left`), or after it in the nearest column that
 * has one (`bottom`). Null when there is none. */
export function dockBeside(layout: Layout, group: GroupId, side: "left" | "bottom"): Node | null {
  const path = pathTo(layout.root, group);
  for (let at = path.length - 2; at >= 0; at--) {
    const node = path[at] as Node;
    if (node.type !== "split" || node.axis !== (side === "left" ? "row" : "column")) {
      continue;
    }
    const index = node.children.findIndex((child) => child.node === path[at + 1]);
    const beside = node.children[side === "left" ? index - 1 : index + 1];
    if (beside) {
      return beside.node;
    }
  }
  return null;
}

/** Hides a docked node, or shows it again. Hiding the one holding the
 * focused group moves the focus to `from`, which must stay in sight. */
export function toggleHidden(layout: Layout, node: GroupId | SplitId, from: GroupId): Layout {
  if (layout.hidden.includes(node)) {
    return settle({ ...layout, hidden: layout.hidden.filter((id) => id !== node) });
  }
  const found = nodeIn(layout.root, node);
  if (!found || found === layout.root || docked(found).some((group) => group.id === from)) {
    return layout;
  }
  const inside = docked(found).some((group) => group.id === layout.focused);
  return settle({
    ...layout,
    hidden: [...layout.hidden, node],
    focused: inside ? from : layout.focused,
    maximized: docked(found).some((group) => group.id === layout.maximized)
      ? null
      : layout.maximized,
  });
}

/** Whether a docked group is out of sight. */
export function isHidden(layout: Layout, group: GroupId): boolean {
  return layout.hidden.some((id) => {
    const found = nodeIn(layout.root, id);
    return found !== null && docked(found).some((candidate) => candidate.id === group);
  });
}

/** The nodes from the root down to a group, or none when it is not docked. */
function pathTo(node: Node, group: GroupId): Node[] {
  if (node.type === "group") {
    return node.id === group ? [node] : [];
  }
  for (const child of node.children) {
    const below = pathTo(child.node, group);
    if (below.length > 0) {
      return [node, ...below];
    }
  }
  return [];
}

function nodeIn(node: Node, id: GroupId | SplitId): Node | null {
  if (node.id === id) {
    return node;
  }
  if (node.type === "group") {
    return null;
  }
  for (const child of node.children) {
    const found = nodeIn(child.node, id);
    if (found) {
      return found;
    }
  }
  return null;
}

/** Whether a pane or window from `from` can land at `target`. A group can't
 * split off itself when that would leave it empty, and floating groups don't
 * split. */
function lands(layout: Layout, target: Target, from: Group, leavesEmpty: boolean): boolean {
  if (target.type === "edge") {
    return true;
  }
  const into = groups(layout).find((group) => group.id === target.group);
  if (!into) {
    return false;
  }
  if (target.type === "tab") {
    return true;
  }
  const floats = layout.floating.some((window) => window.group.id === into.id);
  return !floats && !(into.id === from.id && leavesEmpty);
}

/** Puts a new group on a side of a group, or along an edge of the dock. */
function place(layout: Layout, group: Group, target: Target & { type: "side" | "edge" }): Layout {
  const axis: Axis = target.side === "left" || target.side === "right" ? "row" : "column";
  const first = target.side === "left" || target.side === "top";
  let next = Math.max(layout.next, Number(group.id.slice(1)) + 1);
  const wrap = (node: Node, weight: number): Split => {
    const added = { node: group, weight: 1 - weight };
    const kept = { node, weight };
    return {
      type: "split",
      id: `s${next++}` as SplitId,
      axis,
      children: first ? [added, kept] : [kept, added],
    };
  };
  if (target.type === "edge") {
    const root = wrap(layout.root, 1 - EDGE_WEIGHT);
    return { ...layout, root, next };
  }
  const beside = find(layout, target.group);
  const root = replaceNode(layout.root, beside.id, wrap(beside, 0.5));
  return { ...layout, root, next };
}

function without(group: Group, pane: PaneId): Group {
  const index = group.tabs.indexOf(pane);
  const tabs = group.tabs.filter((tab) => tab !== pane);
  const active =
    group.active === pane ? (tabs[Math.min(index, tabs.length - 1)] ?? null) : group.active;
  return { ...group, tabs, active };
}

function insert<T>(list: readonly T[], index: number, items: readonly T[]): T[] {
  return [...list.slice(0, index), ...items, ...list.slice(index)];
}

function find(layout: Layout, id: GroupId): Group {
  const group = groups(layout).find((candidate) => candidate.id === id);
  assert(group, `no group ${id}`);
  return group;
}

function replaceGroup(layout: Layout, group: Group): Layout {
  return {
    ...layout,
    root: replaceNode(layout.root, group.id, group),
    floating: layout.floating.map((window) =>
      window.group.id === group.id ? { ...window, group } : window,
    ),
  };
}

function replaceNode(node: Node, id: GroupId | SplitId, by: Node): Node {
  if (node.id === id) {
    return by;
  }
  if (node.type === "group") {
    return node;
  }
  return {
    ...node,
    children: node.children.map((child) => ({ ...child, node: replaceNode(child.node, id, by) })),
  };
}

/** Removes empty groups, collapses splits, and restores lost focus to the
 * maximized group or the nearest remaining group in reading order. */
function settle(layout: Layout): Layout {
  assert(Number.isSafeInteger(layout.next), "layout ids exhausted");
  const before = groups(layout);
  const firstDocked = before.find((group) => !layout.floating.some((w) => w.group === group));
  const root = tidy(layout.root) ?? {
    type: "group",
    id: (firstDocked as Group).id,
    tabs: [],
    active: null,
  };
  const floating = layout.floating.filter((window) => window.group.tabs.length > 0);
  const after = new Set(
    [...docked(root), ...floating.map((window) => window.group)].map((g) => g.id),
  );
  let focused = layout.focused;
  if (!after.has(focused)) {
    const index = before.findIndex((group) => group.id === focused);
    const nearest = [...before.slice(0, index).toReversed(), ...before.slice(index + 1)].find(
      (group) => after.has(group.id),
    );
    focused =
      docked(root).find((group) => group.id === layout.maximized)?.id ??
      nearest?.id ??
      docked(root)[0]?.id ??
      focused;
  }
  const maximized =
    docked(root).some((group) => group.id === layout.maximized) &&
    (focused === layout.maximized || floating.some((window) => window.group.id === focused))
      ? layout.maximized
      : null;
  // A node a change took apart is gone, and one holding the focus shows
  // again, as when a pane in a hidden dock is opened.
  const hidden = layout.hidden.filter((id) => {
    const found = nodeIn(root, id);
    return found !== null && found !== root && !docked(found).some((group) => group.id === focused);
  });
  const settled = { ...layout, root, floating, focused, maximized, hidden };
  if (import.meta.env.DEV) {
    const broken = checkLayout(settled);
    assert(broken.length === 0, `the layout breaks its rules: ${broken.join("; ")}`);
  }
  return settled;
}

function tidy(node: Node): Node | null {
  if (node.type === "group") {
    return node.tabs.length > 0 ? node : null;
  }
  const children = node.children.flatMap((child): Child[] => {
    const kept = tidy(child.node);
    if (!kept) {
      return [];
    }
    if (kept.type === "split" && kept.axis === node.axis) {
      return kept.children.map((inner) => ({
        node: inner.node,
        weight: inner.weight * child.weight,
      }));
    }
    return [{ node: kept, weight: child.weight }];
  });
  if (children.length <= 1) {
    return children[0]?.node ?? null;
  }
  const total = children.reduce((sum, child) => sum + child.weight, 0);
  return {
    ...node,
    children: children.map((child) => ({ node: child.node, weight: child.weight / total })),
  };
}

function validRect(rect: Rect): boolean {
  return (
    [rect.x, rect.y, rect.width, rect.height].every((value) => Number.isFinite(value)) &&
    rect.width > 0 &&
    rect.height > 0
  );
}

/** Every rule the layout breaks; none, after any operation. */
export function checkLayout(layout: Layout): string[] {
  const broken: string[] = [];
  // Splitting can allocate both a group and a split.
  if (!Number.isSafeInteger(layout.next + 2) || layout.next < 0) {
    broken.push("the next id leaves no room for another split");
  }
  const all = groups(layout);
  const seen = new Set<PaneId>();
  const ids = new Set<string>();
  const counted = (id: string) => {
    if (ids.has(id)) {
      broken.push(`${id} appears twice`);
    }
    ids.add(id);
    if (!(Number(id.slice(1)) < layout.next)) {
      broken.push(`${id} is not below the next id, ${layout.next}`);
    }
  };

  for (const group of all) {
    counted(group.id);
    if (group.tabs.length === 0 && !(group === layout.root && layout.root.type === "group")) {
      broken.push(`${group.id} is empty`);
    }
    if (
      group.tabs.length === 0 ? group.active !== null : !group.tabs.includes(group.active as PaneId)
    ) {
      broken.push(`${group.id} shows ${group.active}, not one of its tabs`);
    }
    for (const pane of group.tabs) {
      if (seen.has(pane)) {
        broken.push(`${pane} is in two places`);
      }
      seen.add(pane);
      counted(pane);
      if (!layout.panes[pane]) {
        broken.push(`${pane} has no pane`);
      }
    }
  }
  if (Object.keys(layout.panes).length !== seen.size) {
    broken.push("a pane is in no group");
  }
  const kinds = Object.values(layout.panes).map((pane) => `${pane.kind}\u0000${pane.key}`);
  if (new Set(kinds).size !== kinds.length) {
    broken.push("two panes have the same kind and key");
  }

  for (const each of splits(layout.root)) {
    counted(each.id);
    if (each.children.length < 2) {
      broken.push(`${each.id} has fewer than two children`);
    }
    const total = each.children.reduce((sum, child) => sum + child.weight, 0);
    if (!each.children.every((child) => child.weight > 0) || Math.abs(total - 1) > 1e-9) {
      broken.push(`${each.id}'s weights are not positive and summing to 1`);
    }
    if (
      each.children.some((child) => child.node.type === "split" && child.node.axis === each.axis)
    ) {
      broken.push(`${each.id} holds a split along its own axis`);
    }
  }

  if (!all.some((group) => group.id === layout.focused)) {
    broken.push(`the focused ${layout.focused} is no group`);
  }
  if (layout.maximized !== null && !docked(layout.root).some((g) => g.id === layout.maximized)) {
    broken.push(`the maximized ${layout.maximized} is no docked group`);
  }
  if (
    layout.maximized !== null &&
    layout.focused !== layout.maximized &&
    !layout.floating.some((window) => window.group.id === layout.focused)
  ) {
    broken.push(`the focused ${layout.focused} is behind the maximized group`);
  }
  for (const id of layout.hidden) {
    const found = nodeIn(layout.root, id);
    if (!found || found === layout.root) {
      broken.push(`the hidden ${id} is no docked node below the root`);
    } else if (docked(found).some((g) => g.id === layout.focused || g.id === layout.maximized)) {
      broken.push(`the hidden ${id} holds the focused or maximized group`);
    }
  }
  if (new Set(layout.hidden).size !== layout.hidden.length) {
    broken.push("a node is hidden twice");
  }
  for (const window of layout.floating) {
    if (!validRect(window.rect)) {
      broken.push(`${window.group.id}'s window has no size`);
    }
  }
  return broken;
}
