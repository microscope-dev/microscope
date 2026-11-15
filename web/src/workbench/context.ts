import { createContext, useContext } from "react";

import { assert } from "../assert";
import type { Kinds } from "./kinds";

export interface DockSetup {
  readonly kinds: Kinds<string>;

  useMissing(need: string | null): string | null;
}

export const DockContext = createContext<DockSetup | null>(null);

export function useDock(): DockSetup {
  const setup = useContext(DockContext);
  assert(setup, "a dock part outside the dock");
  return setup;
}

export const tabId = (pane: string) => `tab-${pane}`;
export const panelId = (pane: string) => `panel-${pane}`;
