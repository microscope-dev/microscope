import { useSyncExternalStore } from "react";
import { createStore, useStore } from "zustand";

import { read, write } from "./storage";

export type Theme = "system" | "light" | "dark";

export const THEMES: readonly Theme[] = ["system", "light", "dark"];

const isTheme = (value: unknown): value is Theme => THEMES.includes(value as Theme);

export function savedTheme(): Theme {
  return read("microscope-theme", "system", isTheme);
}

const chosen = createStore(() => ({ theme: savedTheme() }));

export function useChosenTheme(): Theme {
  return useStore(chosen, (state) => state.theme);
}

const systemDark = () => window.matchMedia("(prefers-color-scheme: dark)");

function watchSystem(changed: () => void): () => void {
  const query = systemDark();
  query.addEventListener("change", changed);
  return () => query.removeEventListener("change", changed);
}

export function useShownScheme(): "light" | "dark" {
  const theme = useChosenTheme();
  const dark = useSyncExternalStore(watchSystem, () => systemDark().matches);
  return theme === "system" ? (dark ? "dark" : "light") : theme;
}

export function chooseTheme(theme: Theme): void {
  apply(theme);
  write("microscope-theme", theme);
  chosen.setState({ theme });
}

export function apply(theme: Theme): void {
  if (theme === "system") {
    delete document.documentElement.dataset.theme;
  } else {
    document.documentElement.dataset.theme = theme;
  }
}
