import { useEffect, useRef, useState, useSyncExternalStore } from "react";

import { assert } from "./assert";
import type { Connection } from "./connection";
import { type Method, type ParamsOf, RequestError, type Results } from "./protocol";
import { useConnection } from "./store";
import { useVisible } from "./workbench/visible";

type Settled = { ok: true; value: unknown } | { ok: false; error: RequestError };

interface Entry {
  promise: Promise<Settled>;
  settled?: Settled;
}

const LIMIT = 400;

export class Cache {
  readonly #entries = new Map<string, Entry>();
  readonly #listeners = new Set<() => void>();

  #version = 0;

  readonly subscribe = (listener: () => void): (() => void) => {
    this.#listeners.add(listener);
    return () => this.#listeners.delete(listener);
  };

  readonly version = (): number => this.#version;

  #forgot(): void {
    this.#version += 1;
    for (const listener of this.#listeners) {
      listener();
    }
  }

  get(connection: Connection, method: Method, params: unknown): Entry {
    const key = cacheKey(method, params);
    const found = this.#entries.get(key);
    if (found) {
      this.#entries.delete(key);
      this.#entries.set(key, found);
      return found;
    }
    const send = connection.request.bind(connection) as (
      method: Method,
      params?: unknown,
    ) => Promise<unknown>;
    const asked = params === undefined ? send(method) : send(method, params);
    const entry: Entry = {
      promise: asked.then(
        (value): Settled => ({ ok: true, value }),
        (error: unknown): Settled => ({
          ok: false,
          error: error instanceof RequestError ? error : new RequestError("failed", String(error)),
        }),
      ),
    };
    void (async () => {
      const settled = await entry.promise;
      entry.settled = settled;
      if (
        !settled.ok &&
        settled.error.kind === "disconnected" &&
        this.#entries.get(key) === entry
      ) {
        this.#entries.delete(key);
      }
    })();
    this.#entries.set(key, entry);
    while (this.#entries.size > LIMIT) {
      const oldest = this.#entries.keys().next().value;
      assert(oldest !== undefined, `cache of ${this.#entries.size} answers has no oldest`);
      this.#entries.delete(oldest);
    }
    assert(this.#entries.get(key) === entry, "the cache evicted the answer it just asked for");
    return entry;
  }

  refresh(): void {
    this.#forgot();
  }

  clear(): void {
    this.#entries.clear();
    this.#forgot();
  }

  forget(prefix: string): void {
    assert(prefix.includes(" "), `forgetting by prefix "${prefix}", which names no whole method`);
    let forgot = false;
    for (const key of this.#entries.keys()) {
      if (key.startsWith(prefix)) {
        this.#entries.delete(key);
        forgot = true;
      }
    }
    if (forgot) {
      this.#forgot();
    }
  }
}

function cacheKey(method: Method, params: unknown): string {
  return `${method} ${JSON.stringify(params ?? null)}`;
}

export const cache = new Cache();

export interface Requested<T> {
  data: T | undefined;
  error: RequestError | undefined;

  current: boolean;
}

export function useRequest<M extends Method>(
  method: M,
  params: (ParamsOf<M> extends undefined ? undefined : ParamsOf<M>) | null,
  source: Cache = cache,
): Requested<Results[M]> {
  const connection = useConnection();
  const visible = useVisible();
  const key = params === null ? null : cacheKey(method, params);

  const version = useSyncExternalStore(source.subscribe, source.version);
  const [shown, setShown] = useState<{ key: string | null; settled?: Settled }>(() => {
    if (key === null || !visible) {
      return { key: null };
    }
    const settled = source.get(connection, method, params).settled;
    return settled ? { key, settled } : { key: null };
  });

  const asked = useRef(params);
  // oxlint-disable-next-line react/refs -- the effect reads the latest parameters but reruns only when the key changes
  asked.current = params;

  useEffect(() => {
    const wanted = asked.current;
    if (!visible) {
      return;
    }
    if (key === null || wanted === null) {
      setShown({ key: null });
      return;
    }
    let live = true;
    const entry = source.get(connection, method, wanted);
    if (entry.settled) {
      setShown({ key, settled: entry.settled });
    } else {
      void (async () => {
        const settled = await entry.promise;
        if (live) {
          setShown({ key, settled });
        }
      })();
    }
    return () => {
      live = false;
    };
    // oxlint-disable-next-line react/exhaustive-effect-dependencies -- a new version means answers were forgotten, so ask again
  }, [key, connection, method, source, version, visible]);

  const current = shown.key === key && key !== null;
  const settled = shown.settled;
  return {
    data: settled?.ok ? (settled.value as Results[M]) : undefined,
    error: current && settled && !settled.ok ? settled.error : undefined,
    current: current && settled !== undefined,
  };
}
