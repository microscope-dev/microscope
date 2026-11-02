// The workbench's layout, one per page, changed only through the layout's
// operations. Groups keep their identity across a change that leaves them be,
// so a group's selector re-renders it only when it changed.

import { createStore, useStore } from "zustand";
import { emptyLayout, type GroupId, type Layout, type Node } from "./layout";

export const workbench = createStore<{ layout: Layout }>(() => ({ layout: emptyLayout() }));

export function useLayout<T>(select: (layout: Layout) => T): T {
  return useStore(workbench, (state) => select(state.layout));
}

/** Applies an operation; one that changes nothing changes no state. */
export function change(by: (layout: Layout) => Layout): void {
  workbench.setState((state) => {
    const layout = by(state.layout);
    return layout === state.layout ? state : { layout };
  });
}

export function findNode(node: Node, id: GroupId | string): Node | null {
  if (node.id === id) {
    return node;
  }
  if (node.type === "group") {
    return null;
  }
  for (const child of node.children) {
    const found = findNode(child.node, id);
    if (found) {
      return found;
    }
  }
  return null;
}
