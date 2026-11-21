import { isServer } from "@solidjs/web";
import { createMemo, createSignal } from "solid-js";

export interface Store<T> {
  getState(): T;
  setState(next: Partial<T> | T | ((state: T) => Partial<T> | T), replace?: boolean): void;
  subscribe(listener: (state: T, prev: T) => void): () => void;
  read(): T;
}

export function createState<T>(initial: T | (() => T)): Store<T> {
  let current = typeof initial === "function" ? (initial as () => T)() : initial;
  const anySignal = isServer ? null : createSignal(0, { ownedWrite: true });
  const keySignals = new Map<PropertyKey, ReturnType<typeof createSignal<number>>>();
  const listeners = new Set<(state: T, prev: T) => void>();

  const trackKey = (key: PropertyKey): void => {
    if (isServer) {
      return;
    }
    let entry = keySignals.get(key);
    if (!entry) {
      entry = createSignal(0, { ownedWrite: true });
      keySignals.set(key, entry);
    }
    entry[0]();
  };

  const getState = (): T => current;

  const read = (): T => {
    if (isServer || typeof current !== "object" || current === null) {
      anySignal?.[0]();
      return current;
    }
    return new Proxy(current as object, {
      get(target, prop, receiver) {
        trackKey(prop);
        return Reflect.get(target, prop, receiver);
      },
      has(target, prop) {
        trackKey(prop);
        return Reflect.has(target, prop);
      },
      ownKeys(target) {
        anySignal?.[0]();
        return Reflect.ownKeys(target);
      },
    }) as T;
  };

  const setState = (
    next: Partial<T> | T | ((state: T) => Partial<T> | T),
    replace = false,
  ): void => {
    const value =
      typeof next === "function" ? (next as (state: T) => Partial<T> | T)(current) : next;
    if (Object.is(value, current)) {
      return;
    }
    const prev = current;
    current =
      replace || typeof value !== "object" || value === null
        ? (value as T)
        : Object.assign({}, prev, value);
    if (Object.is(current, prev)) {
      return;
    }
    if (!isServer) {
      if (
        typeof current === "object" &&
        current !== null &&
        typeof prev === "object" &&
        prev !== null
      ) {
        let changed = false;
        for (const [key, entry] of keySignals) {
          if (
            !Object.is(
              (current as Record<PropertyKey, unknown>)[key],
              (prev as Record<PropertyKey, unknown>)[key],
            )
          ) {
            entry[1]((n) => n + 1);
            changed = true;
          }
        }
        if (changed || keySignals.size === 0) {
          anySignal?.[1]((n) => n + 1);
        }
      } else {
        for (const entry of keySignals.values()) {
          entry[1]((n) => n + 1);
        }
        anySignal?.[1]((n) => n + 1);
      }
    }
    for (const listener of listeners) {
      listener(current, prev);
    }
  };

  const subscribe = (listener: (state: T, prev: T) => void): (() => void) => {
    listeners.add(listener);
    return () => {
      listeners.delete(listener);
    };
  };

  return { getState, setState, subscribe, read };
}

export function useSelector<T, R>(
  store: Store<T>,
  select: (state: T) => R,
  equals?: (a: R, b: R) => boolean,
): () => R {
  return createMemo(
    () => {
      const tracked = store.read();
      const selected = select(tracked);
      if ((selected as unknown) === (tracked as unknown)) {
        if (typeof tracked === "object" && tracked !== null) {
          Reflect.ownKeys(tracked);
        }
        return store.getState() as unknown as R;
      }
      return selected;
    },
    equals ? { equals } : undefined,
  );
}

export function shallowEqual<T>(a: T, b: T): boolean {
  if (Object.is(a, b)) {
    return true;
  }
  if (typeof a !== "object" || a === null || typeof b !== "object" || b === null) {
    return false;
  }
  const keysA = Object.keys(a);
  const keysB = Object.keys(b);
  if (keysA.length !== keysB.length) {
    return false;
  }
  for (const key of keysA) {
    if (
      !Object.hasOwn(b, key) ||
      !Object.is((a as Record<string, unknown>)[key], (b as Record<string, unknown>)[key])
    ) {
      return false;
    }
  }
  return true;
}
