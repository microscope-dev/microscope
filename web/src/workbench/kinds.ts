import type { Component } from "solid-js";

import { assert } from "../assert";
import type { KindId, Pane, PaneId } from "./layout";

export interface PaneProps {
  readonly id: PaneId;
  readonly pane: Pane;
}

export interface PaneKind<Need extends string> {
  readonly id: KindId;

  title(pane: Pane): string;

  tooltip?(pane: Pane): string;
  readonly Body: Component<PaneProps>;

  readonly Badge?: Component<PaneProps>;

  readonly needs: Need | null;

  readonly keepAlive: boolean;

  closable(pane: Pane): boolean;

  chosen?(pane: Pane): void;

  close?(id: PaneId, pane: Pane): void;
}

export interface Kinds<Need extends string> {
  get(id: KindId): PaneKind<Need>;
  has(id: KindId): boolean;

  readonly all: readonly PaneKind<Need>[];
}

export function kinds<Need extends string>(all: readonly PaneKind<Need>[]): Kinds<Need> {
  const byId = new Map(all.map((kind) => [kind.id, kind]));
  assert(byId.size === all.length, "two pane kinds have the same id");
  return {
    all,
    has: (id) => byId.has(id),
    get: (id) => {
      const kind = byId.get(id);
      assert(kind, `no pane kind ${id}`);
      return kind;
    },
  };
}
