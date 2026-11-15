import type { Kinds } from "./kinds";
import type { Layout } from "./layout";
import { type Loaded, loadLayout, saveLayout } from "./saved";
import { workbench } from "./store";

const CURRENT = "microscope-layout";
const ASIDE = "microscope-layout-session";
const NAMED = "microscope-layouts";

export function lastLayout(kinds: Kinds<string>): Loaded | null {
  const text = get(CURRENT);
  return text === null ? null : known(loadLayout(text), kinds);
}

export function keepSaving(): () => void {
  return workbench.subscribe((state, previous) => {
    if (state.layout !== previous.layout) {
      set(CURRENT, saveLayout(state.layout));
    }
  });
}

export function setAside(layout: Layout): void {
  set(ASIDE, saveLayout(layout));
}

export function setAsideLayout(kinds: Kinds<string>): Layout | null {
  const text = get(ASIDE);
  const loaded = text === null ? null : known(loadLayout(text), kinds);
  return loaded?.ok ? loaded.layout : null;
}

export function layoutNames(): string[] {
  return Object.keys(named());
}

export function saveNamed(name: string, layout: Layout): void {
  set(NAMED, JSON.stringify({ ...named(), [name]: saveLayout(layout) }));
}

export function loadNamed(name: string, kinds: Kinds<string>): Loaded {
  const text = named()[name];
  return text === undefined
    ? { ok: false, why: `no layout is named ${name}` }
    : known(loadLayout(text), kinds);
}

export function deleteNamed(name: string): void {
  const { [name]: _, ...rest } = named();
  set(NAMED, JSON.stringify(rest));
}

function known(loaded: Loaded, kinds: Kinds<string>): Loaded {
  if (!loaded.ok) {
    return loaded;
  }
  const unknown = Object.values(loaded.layout.panes).find((pane) => !kinds.has(pane.kind));
  return unknown ? { ok: false, why: `this page has no ${unknown.kind} pane` } : loaded;
}

function named(): Record<string, string> {
  try {
    const value: unknown = JSON.parse(get(NAMED) ?? "{}");
    if (typeof value !== "object" || value === null || Array.isArray(value)) {
      return {};
    }
    return Object.fromEntries(
      Object.entries(value).filter(
        (entry): entry is [string, string] => typeof entry[1] === "string",
      ),
    );
  } catch {
    return {};
  }
}

function get(key: string): string | null {
  try {
    return localStorage.getItem(key);
  } catch {
    return null;
  }
}

function set(key: string, value: string): void {
  try {
    localStorage.setItem(key, value);
  } catch {}
}
