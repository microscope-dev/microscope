// Every pane kind by id, and how the debugger's documents group follows the
// address: a Source pane for each file the tab has open, and the pane of the
// view the address names shown.

import type { View } from "./focus";
import {
  activate,
  close,
  type GroupId,
  groupOf,
  groups,
  type KindId,
  type Layout,
  move,
  open,
  type Pane,
  type PaneId,
  paneOf,
} from "./workbench/layout";

const kind = (id: string) => id as KindId;

export const KIND = {
  threads: kind("threads"),
  tasks: kind("tasks"),
  processes: kind("processes"),
  stack: kind("stack"),
  breakpoints: kind("breakpoints"),
  stops: kind("stops"),
  watch: kind("watch"),
  variables: kind("variables"),
  registers: kind("registers"),
  console: kind("console"),
  output: kind("output"),
  signals: kind("signals"),
  modules: kind("modules"),
  source: kind("source"),
  disassembly: kind("disassembly"),
  memory: kind("memory"),
  drawings: kind("drawings"),
  explorer: kind("explorer"),
  connect: kind("connect"),
  account: kind("account"),
  projects: kind("projects"),
  org: kind("org"),
  launch: kind("launch"),
  attach: kind("attach"),
  core: kind("core"),
  record: kind("record"),
  profile: kind("profile"),
  profileFlame: kind("profile-flame"),
  profileTree: kind("profile-tree"),
  profileFunctions: kind("profile-functions"),
  profileSyscalls: kind("profile-syscalls"),
  profileZones: kind("profile-zones"),
  profileDetails: kind("profile-details"),
  settings: kind("settings"),
  keys: kind("keys"),
} as const;

/** A pane of a kind there is one of. */
export const single = (id: KindId): Pane => ({ kind: id, key: "", params: null });

/** A file's source. The one with no path stands in while no file is open. */
export const sourcePane = (path: string): Pane => ({ kind: KIND.source, key: path, params: path });

const VIEWS: Record<View, KindId> = {
  source: KIND.source,
  disassembly: KIND.disassembly,
  memory: KIND.memory,
  drawings: KIND.drawings,
};

export const DOCUMENT_KINDS: ReadonlySet<KindId> = new Set(Object.values(VIEWS));

/** The group documents open in: the first open file's, or else the one
 * holding a Source pane or any document, or else the focused one. Not by
 * where groups sit, which raising a floating window changes. */
export function documentsGroup(layout: Layout, files: readonly string[] = []): GroupId {
  for (const file of files) {
    const pane = paneOf(layout, KIND.source, file);
    const group = pane && groupOf(layout, pane);
    if (group) {
      return group.id;
    }
  }
  const all = groups(layout).toSorted((a, b) => a.id.localeCompare(b.id));
  const holding = (wanted: (pane: Pane) => boolean) =>
    all.find((group) => group.tabs.some((tab) => wanted(layout.panes[tab] as Pane)));
  const found =
    holding((pane) => pane.kind === KIND.source) ??
    holding((pane) => DOCUMENT_KINDS.has(pane.kind));
  return found?.id ?? layout.focused;
}

/**
 * Brings the layout in step with the address: a Source pane for each of
 * `files` in their order, the stand-in while there are none, and the pane of
 * `view` shown, which for source is `shown`'s file. Focus stays where it was,
 * and a layout already in step comes back as it is.
 */
export function arrange(
  layout: Layout,
  files: readonly string[],
  shown: string | null,
  view: View,
): Layout {
  let next = layout;
  for (const [id, pane] of Object.entries(next.panes) as [PaneId, Pane][]) {
    if (pane.kind === KIND.source && pane.key !== "" && !files.includes(pane.key)) {
      next = close(next, id);
    }
  }
  for (const file of files) {
    if (!paneOf(next, KIND.source, file)) {
      next = open(next, sourcePane(file), documentsGroup(next, files)).layout;
    }
  }
  const standIn = paneOf(next, KIND.source, "");
  if (files.length === 0 && !standIn) {
    next = open(next, sourcePane(""), documentsGroup(next, files)).layout;
  } else if (files.length > 0 && standIn) {
    next = close(next, standIn);
  }

  let want: PaneId | null;
  if (view === "source") {
    want = paneOf(next, KIND.source, files.length > 0 ? (shown ?? "\u0000") : "");
  } else {
    want = paneOf(next, VIEWS[view], "");
    if (!want) {
      const opened = open(next, single(VIEWS[view]), documentsGroup(next, files));
      next = opened.layout;
      want = opened.pane;
    }
  }

  // Files keep the order they were opened in, ahead of the other documents.
  const into = documentsGroup(next, files);
  const sources = (files.length > 0 ? files : [""])
    .map((file) => paneOf(next, KIND.source, file))
    .filter((pane): pane is PaneId => pane !== null && groupOf(next, pane)?.id === into);
  for (const [index, pane] of sources.entries()) {
    if (groupOf(next, pane)?.tabs.indexOf(pane) !== index) {
      next = move(next, pane, { type: "tab", group: into, index });
    }
  }

  if (want && groupOf(next, want)?.active !== want) {
    next = activate(next, want);
  }
  if (next === layout) {
    return layout;
  }
  const remaining = groups(next);
  const focused = remaining.some((group) => group.id === layout.focused)
    ? layout.focused
    : next.focused;
  const maximized =
    focused === layout.focused && remaining.some((group) => group.id === layout.maximized)
      ? layout.maximized
      : next.maximized;
  return { ...next, focused, maximized };
}
