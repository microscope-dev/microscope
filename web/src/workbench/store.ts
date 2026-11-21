import { createMemo } from "solid-js";

import { createState, useSelector } from "../state";
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

export const workbench = createState<Workbench>(() => ({ layout: emptyLayout(), closed: [] }));

export function resetWorkbench(layout: Layout = emptyLayout()): void {
  workbench.setState({ layout, closed: [] }, true);
}

export function useLayout<T>(
  select: (layout: Layout) => T,
  equals?: (a: T, b: T) => boolean,
): () => T {
  return useSelector(workbench, (state) => select(state.layout), equals);
}

export function useLink<T>(
  link: () => LinkId | null,
  select: (value: Json | undefined) => T,
): () => T {
  return createMemo(() => {
    const id = link();
    const layout = workbench.read().layout;
    return select(id === null ? undefined : layout.links[id]);
  });
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
