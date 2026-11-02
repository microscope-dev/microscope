// What belongs to this tab alone: where it looks, whether it is pinned to a
// stop, and the files it has open. The URL holds the focus; this store
// mirrors it for code outside the router, such as the keyboard.

import { createStore, useStore } from "zustand";
import { assert } from "./assert";
import type { At, Look } from "./focus";

export interface TabState {
  session: string | null;
  /** The stop, thread, and frame shown, when a stop is. */
  at: At | null;
  look: Look;
  /** A pinned tab stays on its stop when the program stops elsewhere. */
  pinned: boolean;
  /** The source line the keyboard acts on, such as F9's. */
  cursor: { path: string; line: number } | null;
  /** The file shown and the line the frame is at in it, when it is. */
  shown: { path: string; line: number | null } | null;
  /** How many frames the stack shown has. */
  frames: number;
  /** The breakpoint whose editor is open. */
  editing: number | null;
  /** A short message, such as that a link was copied. */
  flash: { text: string; at: number } | null;
  /** Files opened in this tab, in their tabs' order. */
  files: string[];
  /** Where each open file was last shown, as a `src` place, the file shown
   * longest ago first. */
  places: Record<string, string>;
  /** Counts requests to show code, so a closed file opens again when asked
   * for even at the place it was closed. */
  asked: number;
  /** The palette, open at everything, at files, at a line, at the calls
   * of the shown thread's line to step into, at the saved layouts, or
   * naming the layout to save. */
  palette: "all" | "files" | "line" | "calls" | "layouts" | "save" | null;
  /** Whether the key help is open. */
  help: boolean;
}

export const initialTab: TabState = {
  session: null,
  at: null,
  look: {},
  pinned: false,
  cursor: null,
  shown: null,
  frames: 0,
  editing: null,
  flash: null,
  files: [],
  places: {},
  asked: 0,
  palette: null,
  help: false,
};

export const tab = createStore<TabState>(() => initialTab);

export function useTab<T>(select: (state: TabState) => T): T {
  return useStore(tab, select);
}

/** The most files a tab keeps open. */
const FILES = 8;

/** Opens a file, closing the one shown longest ago to make room. */
export function openFile(path: string): void {
  tab.setState((state) => {
    if (state.files.includes(path)) {
      return state;
    }
    const files = [...state.files, path];
    if (files.length <= FILES) {
      return { files };
    }
    const oldest =
      Object.keys(state.places).find((file) => file !== path && files.includes(file)) ?? files[0];
    assert(oldest !== undefined, `${files.length} open files have no oldest`);
    assert(oldest !== path, "opening a file closed it to make room");
    const { [oldest]: _, ...places } = state.places;
    return { files: files.filter((file) => file !== oldest), places };
  });
}

/** Remembers where an open file is shown, as the file shown most recently. */
export function shownAt(path: string, place: string): void {
  tab.setState((state) => {
    const { [path]: _, ...places } = state.places;
    return { places: { ...places, [path]: place } };
  });
}

export function closeFile(path: string): void {
  tab.setState((state) => {
    const { [path]: _, ...places } = state.places;
    return { files: state.files.filter((file) => file !== path), places };
  });
}

/** Notes a request to show code, which opens its file even if closed. */
export function asked(): void {
  tab.setState((state) => ({ asked: state.asked + 1 }));
}

/** What a tab keeps of its files across a reload. */
export interface KeptFiles {
  files: string[];
  places: Record<string, string>;
}

export const isKeptFiles = (value: unknown): value is KeptFiles => {
  if (typeof value !== "object" || value === null) {
    return false;
  }
  const { files, places } = value as Record<string, unknown>;
  return (
    Array.isArray(files) &&
    files.every((file) => typeof file === "string") &&
    typeof places === "object" &&
    places !== null &&
    Object.values(places).every((place) => typeof place === "string")
  );
};

export function flash(text: string): void {
  tab.setState({ flash: { text, at: Date.now() } });
}
