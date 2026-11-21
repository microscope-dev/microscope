import { createContext, useContext } from "solid-js";

export const VisibleContext = createContext<() => boolean>(() => true);

export function useVisible(): () => boolean {
  return useContext(VisibleContext);
}
