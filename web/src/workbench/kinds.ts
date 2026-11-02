// Pane kinds: every view the workbench can show, and what each needs before
// it can show anything. The workbench draws a pane's tab and frame from its
// kind and knows nothing else of it, so a new view is a new kind, never a new
// page.

import type { ComponentType } from "react";
import { assert } from "../assert";
import type { KindId, Pane, PaneId } from "./layout";

export interface PaneProps {
  readonly id: PaneId;
  readonly pane: Pane;
}

export interface PaneKind<Need extends string> {
  readonly id: KindId;
  /** The tab's name. A kind with many panes names each from its key. */
  title(pane: Pane): string;
  readonly Body: ComponentType<PaneProps>;
  /** Something short the tab shows after its title, such as a count. */
  readonly Badge?: ComponentType<PaneProps>;
  /** What must be there before the body is shown, such as a stopped
   * program; the frame says why it is not instead. */
  readonly needs: Need | null;
  /** Hidden panes stay mounted, keeping their scroll and their answers,
   * unless they hold what is costly to keep, such as live drawings. */
  readonly keepAlive: boolean;
  /** Whether the tab offers to close. */
  closable(pane: Pane): boolean;
  /** Runs once the pane's tab is chosen, for a pane the address follows. */
  chosen?(pane: Pane): void;
  /** Closes the pane its own way, for a pane the address follows; without
   * it, closing takes the pane out of the layout. */
  close?(id: PaneId, pane: Pane): void;
}

export interface Kinds<Need extends string> {
  get(id: KindId): PaneKind<Need>;
  /** In the order they were defined, which is the order menus list them. */
  readonly all: readonly PaneKind<Need>[];
}

export function kinds<Need extends string>(all: readonly PaneKind<Need>[]): Kinds<Need> {
  const byId = new Map(all.map((kind) => [kind.id, kind]));
  assert(byId.size === all.length, "two pane kinds have the same id");
  return {
    all,
    get: (id) => {
      const kind = byId.get(id);
      assert(kind, `no pane kind ${id}`);
      return kind;
    },
  };
}
