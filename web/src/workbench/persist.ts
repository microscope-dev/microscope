// Layouts this browser keeps: the one in use, saved shortly after each change
// and as the page goes, and any a person saved by name. Layouts are not part
// of a link; a link shares where to look, not how the page is arranged.

import type { Kinds } from "./kinds";
import type { Layout } from "./layout";
import { type Loaded, loadLayout, saveLayout } from "./saved";
import { workbench } from "./store";

const CURRENT = "microscope-layout";
const NAMED = "microscope-layouts";

/** How long the layout is left to settle before it is saved. */
export const SAVE_AFTER = 300;

/** The layout this browser used last, or why it can't be used. Null when it
 * has none. */
export function lastLayout(kinds: Kinds<string>): Loaded | null {
  const text = get(CURRENT);
  return text === null ? null : known(loadLayout(text), kinds);
}

/** Saves the layout a moment after each change, and as the page goes. */
export function keepSaving(): () => void {
  let timer: ReturnType<typeof setTimeout> | null = null;
  const save = () => {
    if (timer !== null) {
      clearTimeout(timer);
      timer = null;
    }
    set(CURRENT, saveLayout(workbench.getState().layout));
  };
  const unsubscribe = workbench.subscribe((state, previous) => {
    if (state.layout === previous.layout) {
      return;
    }
    if (timer !== null) {
      clearTimeout(timer);
    }
    timer = setTimeout(save, SAVE_AFTER);
  });
  window.addEventListener("pagehide", save);
  return () => {
    unsubscribe();
    window.removeEventListener("pagehide", save);
    if (timer !== null) {
      save();
    }
  };
}

/** The names of the layouts saved by name, in the order saved. */
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

/** Refuses a layout with panes this page does not have, as one from
 * another version may. */
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

// Storage may be unavailable, as in some private windows; layouts are then
// kept for as long as the page is open.
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
  } catch {
    // Kept until the page goes.
  }
}
