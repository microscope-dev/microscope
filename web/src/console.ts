// The console's scrollback and line history, which belong to this tab.
// History lasts across reloads, in this browser.

import { createStore } from "zustand";

import { assert } from "./assert";
import type { ConsoleResult } from "./protocol";
import { read, write } from "./storage";

export interface Entry {
  id: number;
  line: string;
  /** The stop it ran at, when it ran at one. */
  stop: number | null;
  /** The answer, once it arrives. */
  result?: ConsoleResult;
  error?: string;
}

export interface ConsoleState {
  entries: Entry[];
  /** Lines run, oldest first. */
  history: string[];
}

/** The most entries kept on screen and lines kept in history. */
const ENTRIES = 200;
const HISTORY = 100;
const KEY = "microscope-console-history";

const isLines = (value: unknown): value is string[] =>
  Array.isArray(value) && value.every((line) => typeof line === "string");

export const consoleStore = createStore<ConsoleState>(() => ({
  entries: [],
  history: read(KEY, [], isLines),
}));

let next = 1;

/** Adds a line being run, returning its entry's id. */
export function started(line: string, stop: number | null): number {
  const id = next++;
  consoleStore.setState((state) => {
    const history = [...state.history.filter((previous) => previous !== line), line].slice(
      -HISTORY,
    );
    write(KEY, history);
    return { entries: [...state.entries, { id, line, stop }].slice(-ENTRIES), history };
  });
  return id;
}

export function finished(id: number, outcome: { result?: ConsoleResult; error?: string }): void {
  assert(Number.isInteger(id) && id >= 1 && id < next, `console entry ${id} was never started`);
  consoleStore.setState((state) => ({
    entries: state.entries.map((entry) => (entry.id === id ? { ...entry, ...outcome } : entry)),
  }));
}

export function clearConsole(): void {
  consoleStore.setState({ entries: [] });
}

/** The longest text every candidate starts with. */
export function commonPrefix(labels: readonly string[]): string {
  if (labels.length === 0) {
    return "";
  }
  let prefix = labels[0];
  assert(prefix !== undefined, `${labels.length} candidates have no first`);
  for (const label of labels) {
    while (!label.startsWith(prefix)) {
      prefix = prefix.slice(0, -1);
    }
  }
  return prefix;
}
