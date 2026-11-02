// What the dock's parts share: the pane kinds, and how to tell what a pane
// needs is missing.

import { createContext, useContext } from "react";
import { assert } from "../assert";
import type { Kinds } from "./kinds";

export interface DockSetup {
  readonly kinds: Kinds<string>;
  /** A hook: why a need is not met now, or null when it is. */
  useMissing(need: string | null): string | null;
}

export const DockContext = createContext<DockSetup | null>(null);

export function useDock(): DockSetup {
  const setup = useContext(DockContext);
  assert(setup, "a dock part outside the dock");
  return setup;
}

/** The ids a pane's tab and panel go by, so a command can focus a pane. */
export const tabId = (pane: string) => `tab-${pane}`;
export const panelId = (pane: string) => `panel-${pane}`;
