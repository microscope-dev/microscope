// A layout as a browser keeps it. What comes back from storage may be from an
// older page, or edited, or broken, so it is read field by field and checked
// against every rule before the workbench trusts it.

import {
  type Axis,
  type Child,
  checkLayout,
  type Floating,
  type Group,
  type GroupId,
  type Json,
  type KindId,
  type Layout,
  type LinkId,
  type Node,
  type Pane,
  type PaneId,
  type Rect,
  type SplitId,
} from "./layout";

/** Raise this, and add a migration, whenever the saved shape changes. */
export const VERSION = 3;

/** Each turns a saved layout of one version into the next version's. */
const MIGRATIONS: Readonly<Record<number, (old: unknown) => unknown>> = {
  // Version 2 can hide docked nodes; nothing was hidden before.
  1: (old) => ({ ...record(old, "the layout"), hidden: [] }),
  // Version 3 keeps what panes share in links. A profile's pane kept how it
  // looked at its profile in its params; it reads that from a link now.
  2: (old) => {
    // Converts what it can read; reading the result says what is wrong.
    const layout = record(old, "the layout");
    const { panes } = layout;
    if (typeof panes !== "object" || panes === null || typeof layout.next !== "number") {
      return { ...layout, links: {} };
    }
    let { next } = layout;
    const links: Record<string, unknown> = {};
    const moved = Object.fromEntries(
      Object.entries(panes).map(([id, pane]: [string, unknown]) => {
        const found = pane as { kind?: unknown; key?: unknown; params?: unknown } | null;
        if (found?.kind !== "profile" || typeof found.params !== "object" || !found.params) {
          return [id, pane];
        }
        const params = found.params as { name?: unknown; look?: unknown };
        const link = `l${next++}`;
        links[link] = { profile: found.key, look: params.look ?? {} };
        return [id, { ...found, params: { name: params.name ?? "", link } }];
      }),
    );
    return { ...layout, panes: moved, links, next };
  },
};

export type Loaded = { ok: true; layout: Layout } | { ok: false; why: string };

export function saveLayout(layout: Layout): string {
  return JSON.stringify({ version: VERSION, layout });
}

export function loadLayout(text: string): Loaded {
  try {
    const saved = record(JSON.parse(text), "the saved layout");
    let version = saved.version;
    let data = saved.layout;
    if (typeof version !== "number" || !Number.isInteger(version) || version > VERSION) {
      return { ok: false, why: `it is from version ${String(version)}, not up to ${VERSION}` };
    }
    for (; version < VERSION; version++) {
      const migrate = MIGRATIONS[version];
      if (!migrate) {
        return { ok: false, why: `version ${version} layouts can no longer be read` };
      }
      data = migrate(data);
    }
    const layout = readLayout(data);
    const broken = checkLayout(layout);
    return broken.length === 0
      ? { ok: true, layout }
      : { ok: false, why: `it breaks the layout's rules: ${broken.join("; ")}` };
  } catch (error) {
    return { ok: false, why: error instanceof Error ? error.message : String(error) };
  }
}

class Malformed extends Error {}

function readLayout(value: unknown): Layout {
  const layout = record(value, "the layout");
  const panes: Record<PaneId, Pane> = {};
  for (const [id, pane] of Object.entries(record(layout.panes, "panes"))) {
    panes[ident<PaneId>(id, "p", "a pane")] = readPane(pane);
  }
  const maximized =
    layout.maximized === null ? null : ident<GroupId>(layout.maximized, "g", "maximized");
  return {
    root: readNode(layout.root),
    floating: list(layout.floating, "floating").map((item) => readFloating(item)),
    maximized,
    hidden: list(layout.hidden, "hidden").map((id) =>
      typeof id === "string" && id.startsWith("s")
        ? ident<SplitId>(id, "s", "a hidden node")
        : ident<GroupId>(id, "g", "a hidden node"),
    ),
    focused: ident<GroupId>(layout.focused, "g", "focused"),
    panes,
    links: Object.fromEntries(
      Object.entries(record(layout.links, "links")).map(([id, held]) => [
        ident<LinkId>(id, "l", "a link"),
        // JSON.parse makes nothing but JSON.
        held as Json,
      ]),
    ),
    next: count(layout.next, "next"),
  };
}

function readPane(value: unknown): Pane {
  const pane = record(value, "a pane");
  if (typeof pane.key !== "string") {
    throw new Malformed("a pane's key is not text");
  }
  // JSON.parse makes nothing but JSON.
  return { kind: kind(pane.kind), key: pane.key, params: (pane.params ?? null) as Json };
}

function readNode(value: unknown): Node {
  const node = record(value, "a node");
  if (node.type === "group") {
    return readGroup(node);
  }
  if (node.type !== "split") {
    throw new Malformed(`a node is a ${String(node.type)}, not a group or split`);
  }
  if (node.axis !== "row" && node.axis !== "column") {
    throw new Malformed(`a split's axis is ${String(node.axis)}`);
  }
  const axis: Axis = node.axis;
  const children = list(node.children, "a split's children").map((item): Child => {
    const child = record(item, "a split's child");
    if (typeof child.weight !== "number") {
      throw new Malformed("a child's weight is not a number");
    }
    return { node: readNode(child.node), weight: child.weight };
  });
  return { type: "split", id: ident<SplitId>(node.id, "s", "a split"), axis, children };
}

function readGroup(value: unknown): Group {
  const group = record(value, "a group");
  const tabs = list(group.tabs, "a group's tabs").map((tab) => ident<PaneId>(tab, "p", "a tab"));
  const active = group.active === null ? null : ident<PaneId>(group.active, "p", "an active tab");
  return { type: "group", id: ident<GroupId>(group.id, "g", "a group"), tabs, active };
}

function readFloating(value: unknown): Floating {
  const window = record(value, "a floating window");
  const rect = record(window.rect, "a window's rectangle");
  const number = (field: string) => {
    const read = rect[field];
    if (typeof read !== "number") {
      throw new Malformed(`a window's ${field} is not a number`);
    }
    return read;
  };
  const read: Rect = {
    x: number("x"),
    y: number("y"),
    width: number("width"),
    height: number("height"),
  };
  return { group: readGroup(window.group), rect: read };
}

function record(value: unknown, what: string): Record<string, unknown> {
  if (typeof value !== "object" || value === null || Array.isArray(value)) {
    throw new Malformed(`${what} is not an object`);
  }
  return value as Record<string, unknown>;
}

function list(value: unknown, what: string): unknown[] {
  if (!Array.isArray(value)) {
    throw new Malformed(`${what} is not a list`);
  }
  return value;
}

function count(value: unknown, what: string): number {
  if (typeof value !== "number" || !Number.isSafeInteger(value) || value < 0) {
    throw new Malformed(`${what} is not a count`);
  }
  return value;
}

/** An id: its letter, then its number. */
function ident<T extends PaneId | GroupId | SplitId | LinkId>(
  value: unknown,
  letter: string,
  what: string,
): T {
  if (typeof value !== "string" || !new RegExp(`^${letter}(0|[1-9][0-9]*)$`, "u").test(value)) {
    throw new Malformed(`${what}'s id is ${JSON.stringify(value)}`);
  }
  return value as T;
}

function kind(value: unknown): KindId {
  if (typeof value !== "string" || value === "") {
    throw new Malformed("a pane has no kind");
  }
  return value as KindId;
}
