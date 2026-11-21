import { createMemo, createSignal, onSettled } from "solid-js";

import { createState, useSelector } from "./state";
import { read, write } from "./storage";

export type Theme = "system" | "light" | "dark";

export const THEMES: readonly Theme[] = ["system", "light", "dark"];

const isTheme = (value: unknown): value is Theme => THEMES.includes(value as Theme);

export function savedTheme(): Theme {
  return read("microscope-theme", "system", isTheme);
}

const chosen = createState(() => ({ theme: savedTheme() }));

export function useChosenTheme(): () => Theme {
  return useSelector(chosen, (state) => state.theme);
}

const systemDark = () => window.matchMedia("(prefers-color-scheme: dark)");

export function useShownScheme(): () => "light" | "dark" {
  const theme = useChosenTheme();
  const [dark, setDark] = createSignal(systemDark().matches);
  onSettled(() => {
    const query = systemDark();
    const changed = () => setDark(query.matches);
    setDark(query.matches);
    query.addEventListener("change", changed);
    return () => query.removeEventListener("change", changed);
  });
  return createMemo(() => {
    const selected = theme();
    return selected === "system" ? (dark() ? "dark" : "light") : selected;
  });
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
