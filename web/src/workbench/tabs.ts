// Choosing, closing, and reopening a tab, the same from a click, a key, or a
// menu.

import { assert } from "../assert";
import { tabId } from "./context";
import type { Kinds } from "./kinds";
import {
  activate,
  close,
  groupOf,
  groups,
  linksOf,
  move,
  open,
  type PaneId,
  paneOf,
  restoreLinks,
} from "./layout";
import { change, REOPENABLE, workbench } from "./store";

export function chooseTab(kinds: Kinds<string>, pane: PaneId): void {
  const found = workbench.getState().layout.panes[pane];
  assert(found, `choosing ${pane}, which is no pane`);
  change((layout) => activate(layout, pane));
  kinds.get(found.kind).chosen?.(found);
}

export function closeTab(kinds: Kinds<string>, pane: PaneId): void {
  const { layout, closed } = workbench.getState();
  const found = layout.panes[pane];
  assert(found, `closing ${pane}, which is no pane`);
  const kind = kinds.get(found.kind);
  if (!kind.closable(found)) {
    return;
  }
  const group = groupOf(layout, pane);
  assert(group, `${pane} is in no group`);
  const remembered = {
    pane: found,
    links: linksOf(layout, [found]),
    group: group.id,
    index: group.tabs.indexOf(pane),
  };
  workbench.setState({ closed: [...closed, remembered].slice(-REOPENABLE) });
  if (kind.close) {
    kind.close(pane, found);
  } else {
    change((before) => close(before, pane));
  }
  requestAnimationFrame(() => {
    if (document.activeElement !== document.body) {
      return;
    }
    const current = workbench.getState().layout;
    const next = groups(current).find((candidate) => candidate.id === current.focused)?.active;
    if (next) {
      document.querySelector<HTMLElement>(`#${tabId(next)}`)?.focus();
    }
  });
}

/** Opens the most recently closed tab that is not open again, where it was
 * if its group is still there, and says whether there was one. */
export function reopenTab(kinds: Kinds<string>): boolean {
  const { layout, closed } = workbench.getState();
  const index = closed.findLastIndex((entry) => !paneOf(layout, entry.pane.kind, entry.pane.key));
  const entry = closed[index];
  workbench.setState({ closed: closed.slice(0, Math.max(index, 0)) });
  if (!entry) {
    return false;
  }
  change((before) => {
    const into = groups(before).some((group) => group.id === entry.group)
      ? entry.group
      : before.focused;
    const opened = open(restoreLinks(before, entry.links), entry.pane, into);
    // A tab index counts the moving tab where it is now.
    const at = groupOf(opened.layout, opened.pane)?.tabs.indexOf(opened.pane) ?? 0;
    const slot = at < entry.index ? entry.index + 1 : entry.index;
    return move(opened.layout, opened.pane, { type: "tab", group: into, index: slot });
  });
  kinds.get(entry.pane.kind).chosen?.(entry.pane);
  return true;
}
