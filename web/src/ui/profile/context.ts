import { createContext, createEffect, createMemo, createSignal, useContext } from "solid-js";

import {
  captureCache,
  captureCacheKey,
  type CaptureSettled,
  isCaptureProfile,
  parseCaptureProfileId,
} from "../../captures";
import { type Requested, useRequest } from "../../data";
import type { ProfileLook } from "../../profile/look";
import { type CallTree, decodeTree } from "../../profile/tree";
import type {
  Method,
  ParamsOf,
  ProfileQuery,
  ProfileSummary,
  RequestError,
  Results,
} from "../../protocol";
import { useModel } from "../../store";
import { useVisible } from "../../workbench/visible";

export interface ProfileFocus {
  profile: string;
  summary: ProfileSummary;
  look: ProfileLook;

  query: ProfileQuery;

  unit: string;

  setLook(change: (look: ProfileLook) => ProfileLook, push?: boolean, carry?: boolean): void;
}

export const ProfileContext = createContext<() => ProfileFocus | null>(() => null);

export function useProfile(): () => ProfileFocus {
  const read = useContext(ProfileContext);
  return () => {
    const focus = read();
    if (!focus) {
      throw new Error("profile views need a ProfileContext");
    }
    return focus;
  };
}

function captureIdOfParams(params: unknown, fallbackProfile: string | null): number | null {
  if (typeof params === "object" && params !== null && "profile" in params) {
    const raw = (params as { profile?: unknown }).profile;
    if (typeof raw === "string") {
      return parseCaptureProfileId(raw);
    }
  }
  return parseCaptureProfileId(fallbackProfile);
}

/**
 * Routes profile RPC queries transparently between the live WebSocket (`useRequest`)
 * and the HTTP capture cache (`captureCache`) when viewing an uploaded capture.
 * Retains the previous settled data (`keptWs` / `keptCaptureData`) while a new
 * query is in flight so panes dim as stale instead of flickering to a loading state.
 */
export function useAnswer<M extends Method>(
  method: M,
  params: () => (ParamsOf<M> extends undefined ? undefined : ParamsOf<M>) | null,
): () => Requested<Results[M]> {
  const focus = useContext(ProfileContext);
  const open = useModel((model) => model.link === "open");
  const visible = useVisible();

  const resolved = createMemo(() => {
    const wanted = params();
    const captureId = wanted === null ? null : captureIdOfParams(wanted, focus()?.profile ?? null);
    return { wanted, captureId };
  });

  const wsAnswer = useRequest(method, () => {
    const cur = resolved();
    return cur.captureId === null && open() ? cur.wanted : null;
  });

  const captureKey = createMemo(() => {
    const cur = resolved();
    if (cur.captureId === null || cur.wanted === null) {
      return null;
    }
    return captureCacheKey(cur.captureId, method, cur.wanted);
  });

  const [arrived, setArrived] = createSignal<{ key: string; settled: CaptureSettled } | null>(null);

  createEffect(
    () => ({ key: captureKey(), isVisible: visible(), cur: resolved() }),
    ({ key, isVisible, cur }) => {
      if (!isVisible || key === null || cur.captureId === null || cur.wanted === null) {
        return;
      }
      let live = true;
      const entry = captureCache.get(cur.captureId, method, cur.wanted);
      if (entry.settled) {
        setArrived({ key, settled: entry.settled });
      } else {
        void (async () => {
          const settled = await entry.promise;
          if (live) {
            setArrived({ key, settled });
          }
        })();
      }
      return () => {
        live = false;
      };
    },
  );

  let keptWs: Results[M] | undefined;
  let keptCapture: { key: string; settled: CaptureSettled } | null = null;
  let keptCaptureData: Results[M] | undefined;

  return createMemo(() => {
    const cur = resolved();
    if (cur.captureId !== null) {
      const key = captureKey();
      if (key === null) {
        keptCapture = null;
        return { data: undefined, error: undefined, current: false };
      }
      const latest = arrived();
      if (latest && latest.key === key) {
        keptCapture = latest;
      } else if (visible() && keptCapture?.key !== key) {
        const cached = captureCache.peek(key)?.settled;
        if (cached) {
          keptCapture = { key, settled: cached };
        }
      }
      const settled = keptCapture?.settled;
      const current = keptCapture !== null && keptCapture.key === key;
      const data = settled?.ok ? (settled.value as Results[M]) : undefined;
      if (data !== undefined) {
        keptCaptureData = data;
      }
      const error = current && settled && !settled.ok ? settled.error : undefined;
      return data === undefined && error === undefined
        ? { data: keptCaptureData, error: undefined, current }
        : { data, error, current };
    }

    const current = wsAnswer();
    if (current.data !== undefined) {
      keptWs = current.data;
    }
    return current.data === undefined && current.error === undefined
      ? { ...current, data: keptWs }
      : current;
  });
}

export function useDisconnected(): () => boolean {
  const focus = useContext(ProfileContext);
  const reconnecting = useModel((model) => model.link === "reconnecting");
  return createMemo(() => (isCaptureProfile(focus()?.profile) ? false : reconnecting()));
}

export function token(name: string): string {
  return getComputedStyle(document.documentElement).getPropertyValue(name).trim();
}

export interface Fetched {
  tree: CallTree | null;
  error: RequestError | undefined;
  current: boolean;
}

export function useTree(query: () => ProfileQuery): () => Fetched {
  const answer = useAnswer("profileTree", query);
  const data = createMemo(() => answer().data);
  const tree = createMemo(() => {
    const got = data();
    return got ? decodeTree(got, got.payload) : null;
  });
  return createMemo(() => {
    const current = answer();
    return { tree: tree(), error: current.error, current: current.current };
  });
}
