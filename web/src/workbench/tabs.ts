// Choosing and closing a tab, the same from a click, a key, or a menu.

import { assert } from "../assert";
import type { Kinds } from "./kinds";
import { activate, close, type PaneId } from "./layout";
import { change, workbench } from "./store";

export function chooseTab(kinds: Kinds<string>, pane: PaneId): void {
  const found = workbench.getState().layout.panes[pane];
  assert(found, `choosing ${pane}, which is no pane`);
  change((layout) => activate(layout, pane));
  kinds.get(found.kind).chosen?.(found);
}

export function closeTab(kinds: Kinds<string>, pane: PaneId): void {
  const found = workbench.getState().layout.panes[pane];
  assert(found, `closing ${pane}, which is no pane`);
  const kind = kinds.get(found.kind);
  if (!kind.closable(found)) {
    return;
  }
  if (kind.close) {
    kind.close(pane, found);
  } else {
    change((layout) => close(layout, pane));
  }
}
