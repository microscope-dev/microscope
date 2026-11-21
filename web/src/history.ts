import type { LocationChange, RouterHistory } from "@solidjs/router";
import { createContext, useContext } from "solid-js";

import { base } from "./base";
import { stringifySearch } from "./focus";

export type HistoryAction = "PUSH" | "REPLACE" | "BACK" | "FORWARD" | "GO";

export interface HistoryLocation {
  pathname: string;
  search: string;
  hash: string;
  href: string;
  state: unknown;
}

export interface AppHistory extends RouterHistory {
  readonly location: HistoryLocation;
  push(url: string, state?: unknown): void;
  replace(url: string, state?: unknown): void;
  back(): void;
  forward(): void;
  go(delta: number): void;
  canGoBack(): boolean;
  subscribe(listener: (event: { action: { type: HistoryAction } }) => void): () => void;
}

function parseLocation(url: string, state: unknown = undefined): HistoryLocation {
  const parsed = new URL(url, "http://microscope.local");
  return {
    pathname: parsed.pathname,
    search: parsed.search,
    hash: parsed.hash,
    href: `${parsed.pathname}${parsed.search}${parsed.hash}`,
    state,
  };
}

function currentHref(): string {
  return window.location.pathname + window.location.search + window.location.hash;
}

function stampDepth(state: unknown, nextDepth: number): Record<string, unknown> {
  return typeof state === "object" && state !== null
    ? { ...(state as Record<string, unknown>), depth: nextDepth }
    : { depth: nextDepth };
}

export function createBrowserHistory(): AppHistory {
  const subscribers = new Set<(event: { action: { type: HistoryAction } }) => void>();
  let routerNotify: ((value?: string | LocationChange) => void) | null = null;
  let depth =
    typeof window !== "undefined" && typeof window.history.state?.depth === "number"
      ? (window.history.state.depth as number)
      : 0;

  const emit = (type: HistoryAction) => {
    for (const listener of subscribers) {
      listener({ action: { type } });
    }
  };

  const write = (url: string, replace: boolean, state: unknown, notifyRouter: boolean) => {
    if (replace) {
      window.history.replaceState(stampDepth(state, depth), "", url);
    } else {
      depth += 1;
      window.history.pushState(stampDepth(state, depth), "", url);
    }
    if (notifyRouter) {
      routerNotify?.({ value: url, state });
    }
    emit(replace ? "REPLACE" : "PUSH");
  };

  const onPopState = () => {
    const raw = window.history.state as { depth?: unknown } | null;
    if (typeof raw?.depth === "number") {
      depth = raw.depth;
    }
    routerNotify?.();
    emit("BACK");
  };

  return {
    get location() {
      return parseLocation(currentHref(), window.history.state);
    },
    get: () => ({ value: currentHref(), state: window.history.state }),
    set: ({ value, replace, state }) => {
      write(value, Boolean(replace), state, false);
    },
    init: (notify) => {
      routerNotify = notify;
      window.addEventListener("popstate", onPopState);
      return () => {
        if (routerNotify === notify) {
          routerNotify = null;
        }
        window.removeEventListener("popstate", onPopState);
      };
    },
    push: (url, state) => write(url, false, state, true),
    replace: (url, state) => write(url, true, state, true),
    back: () => window.history.back(),
    forward: () => window.history.forward(),
    go: (delta) => window.history.go(delta),
    canGoBack: () => depth > 0 || window.history.length > 1,
    subscribe: (listener) => {
      subscribers.add(listener);
      return () => {
        subscribers.delete(listener);
      };
    },
    utils: {
      go: (delta) => window.history.go(delta),
    },
  };
}

export function createMemoryHistory(initial = "/"): AppHistory {
  const entries: { url: string; state: unknown }[] = [{ url: initial, state: undefined }];
  let index = 0;
  const subscribers = new Set<(event: { action: { type: HistoryAction } }) => void>();
  let routerNotify: ((value?: string | LocationChange) => void) | null = null;

  const emit = (type: HistoryAction) => {
    for (const listener of subscribers) {
      listener({ action: { type } });
    }
  };

  const write = (url: string, replace: boolean, state: unknown, notifyRouter: boolean) => {
    if (replace) {
      entries[index] = { url, state };
    } else {
      entries.splice(index + 1, entries.length - index - 1, { url, state });
      index += 1;
    }
    if (notifyRouter) {
      routerNotify?.({ value: url, state });
    }
    emit(replace ? "REPLACE" : "PUSH");
  };

  const go = (delta: number) => {
    const next = Math.max(0, Math.min(index + delta, entries.length - 1));
    if (next === index) {
      return;
    }
    index = next;
    const entry = entries[index];
    if (entry) {
      routerNotify?.({ value: entry.url, state: entry.state });
    }
    emit(delta < 0 ? "BACK" : delta > 0 ? "FORWARD" : "GO");
  };

  return {
    get location() {
      const entry = entries[index] ?? { url: "/", state: undefined };
      return parseLocation(entry.url, entry.state);
    },
    get: () => {
      const entry = entries[index] ?? { url: "/", state: undefined };
      return { value: entry.url, state: entry.state };
    },
    set: ({ value, replace, state }) => {
      write(value, Boolean(replace), state, false);
    },
    init: (notify) => {
      routerNotify = notify;
      return () => {
        if (routerNotify === notify) {
          routerNotify = null;
        }
      };
    },
    push: (url, state) => write(url, false, state, true),
    replace: (url, state) => write(url, true, state, true),
    back: () => go(-1),
    forward: () => go(1),
    go,
    canGoBack: () => index > 0,
    subscribe: (listener) => {
      subscribers.add(listener);
      return () => {
        subscribers.delete(listener);
      };
    },
    utils: { go },
  };
}

export const HistoryContext = createContext<AppHistory | null>(null);

export function useHistory(): AppHistory {
  const history = useContext(HistoryContext);
  if (!history) {
    throw new Error("components need a HistoryContext");
  }
  return history;
}

export const BASE_PREFIX = base === "/" ? "" : base.slice(0, -1);

export function toHref(path: string, search?: Record<string, unknown>, hash?: string): string {
  const prefixed = path.startsWith("/") ? `${BASE_PREFIX}${path}` : path;
  const query = search ? stringifySearch(search) : "";
  const fragment = hash ? (hash.startsWith("#") ? hash : `#${hash}`) : "";
  return `${prefixed}${query}${fragment}`;
}
