// Whether a pane is on screen. A pane in a tab not shown stays mounted, and
// reads this to hold its requests until it is shown again.

import { createContext, useContext } from "react";

export const VisibleContext = createContext(true);

export function useVisible(): boolean {
  return useContext(VisibleContext);
}
