import { isServer } from "@solidjs/web";
import { createEffect, createMemo, createSignal } from "solid-js";

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

/**
 * Bounded LRU cache (max `LIMIT` entries) for RPC responses, with a reactive
 * `#version` counter that triggers `useRequest` subscribers on invalidation.
 *
 * `disconnected` errors evict themselves once settled so reconnects retry
 * instead of pinning a transient offline failure.
 */
export class Cache {
  readonly #entries = new Map<string, Entry>();
  readonly #listeners = new Set<() => void>();
  readonly #versionSignal = isServer ? null : createSignal(0, { ownedWrite: true });

  #version = 0;

  readonly subscribe = (listener: () => void): (() => void) => {
    this.#listeners.add(listener);
    return () => this.#listeners.delete(listener);
  };

  readonly version = (): number => {
    this.#versionSignal?.[0]();
    return this.#version;
  };

  #forgot(): void {
    this.#version += 1;
    this.#versionSignal?.[1](this.#version);
    for (const listener of this.#listeners) {
      listener();
    }
  }

  peek(method: Method, params: unknown): Entry | undefined {
    return this.#entries.get(cacheKey(method, params));
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

/**
 * Subscribes a component to a cached RPC call.
 *
 * Fetches pause while the enclosing workbench pane is hidden (`useVisible`)
 * and the last settled answer (`kept`) stays visible across key or cache-version
 * changes so panes do not flash empty while refetching.
 */
export function useRequest<M extends Method>(
  method: M,
  params: () => (ParamsOf<M> extends undefined ? undefined : ParamsOf<M>) | null,
  source: Cache = cache,
): () => Requested<Results[M]> {
  const connection = useConnection();
  const visible = useVisible();

  const request = createMemo(
    () => {
      const wanted = params();
      const key = wanted === null ? null : cacheKey(method, wanted);
      return { wanted, key, visible: visible(), version: source.version() };
    },
    {
      equals: (a, b) => a.key === b.key && a.visible === b.visible && a.version === b.version,
    },
  );

  const [arrived, setArrived] = createSignal<{
    key: string;
    version: number;
    settled: Settled;
  } | null>(null);

  createEffect(
    () => request(),
    ({ wanted, key, visible: isVisible, version }) => {
      if (!isVisible || key === null || wanted === null) {
        return;
      }
      let live = true;
      const entry = source.get(connection, method, wanted);
      if (!entry.settled) {
        void (async () => {
          const settled = await entry.promise;
          if (live) {
            setArrived({ key, version, settled });
          }
        })();
      }
      return () => {
        live = false;
      };
    },
  );

  let kept: { key: string; version: number; settled: Settled } | null = null;

  return createMemo(() => {
    const { key, wanted, visible: isVisible, version } = request();
    const latest = arrived();
    if (key === null || wanted === null) {
      kept = null;
      return { data: undefined, error: undefined, current: false };
    }
    if (latest && latest.key === key && latest.version === version) {
      kept = latest;
    } else if (isVisible && (kept?.key !== key || kept.version !== version)) {
      const cached = source.peek(method, wanted)?.settled;
      if (cached) {
        kept = { key, version, settled: cached };
      }
    }
    const settled = kept?.settled;
    const current = kept !== null && kept.key === key && kept.version === version;
    return {
      data: settled?.ok ? (settled.value as Results[M]) : undefined,
      error: current && settled && !settled.ok ? settled.error : undefined,
      current,
    };
  });
}
