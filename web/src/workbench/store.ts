import { createStore, useStore } from "zustand";

import {
  emptyLayout,
  type GroupId,
  type Json,
  type Layout,
  type LinkId,
  type Node,
  type Pane,
} from "./layout";

export interface Closed {
  readonly pane: Pane;

  readonly links: Readonly<Record<LinkId, Json>>;
  readonly group: GroupId;
  readonly index: number;
}

export const REOPENABLE = 20;

export interface Workbench {
  readonly layout: Layout;

  readonly closed: readonly Closed[];
}

export const workbench = createStore<Workbench>(() => ({ layout: emptyLayout(), closed: [] }));

export function resetWorkbench(layout: Layout = emptyLayout()): void {
  workbench.setState({ layout, closed: [] }, true);
}

export function useLayout<T>(select: (layout: Layout) => T): T {
  return useStore(workbench, (state) => select(state.layout));
}

export function useLink<T>(link: LinkId | null, select: (value: Json | undefined) => T): T {
  return useStore(workbench, (state) =>
    select(link === null ? undefined : state.layout.links[link]),
  );
}

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
