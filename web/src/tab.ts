import { assert } from "./assert";
import type { At, Look } from "./focus";
import { createState, useSelector } from "./state";

export interface TabState {
  session: string | null;

  at: At | null;
  look: Look;

  pinned: boolean;

  cursor: { path: string; line: number } | null;

  shown: { path: string; line: number | null } | null;

  frames: number;

  editing: number | null;

  flash: { text: string; at: number } | null;

  files: string[];

  places: Record<string, string>;

  asked: number;

  palette: "all" | "files" | "line" | "calls" | "layouts" | "save" | null;

  share: boolean;

  shareProfile: string | null;
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
  share: false,
  shareProfile: null,
};

export const tab = createState<TabState>(() => initialTab);

export function useTab<T>(select: (state: TabState) => T): () => T {
  return useSelector(tab, select);
}

const FILES = 8;

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

export function asked(): void {
  tab.setState((state) => ({ asked: state.asked + 1 }));
}

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

export function openShare(profile: string | null = null): void {
  tab.setState({ share: true, shareProfile: profile });
}

export function closeShare(): void {
  tab.setState({ share: false, shareProfile: null });
}
