import { createContext, useContext } from "react";

export const VisibleContext = createContext(true);

export function useVisible(): boolean {
  return useContext(VisibleContext);
}
