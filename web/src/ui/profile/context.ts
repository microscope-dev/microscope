import { createContext, createMemo, useContext } from "solid-js";

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

export function useAnswer<M extends Method>(
  method: M,
  params: () => (ParamsOf<M> extends undefined ? undefined : ParamsOf<M>) | null,
): () => Requested<Results[M]> {
  const open = useModel((model) => model.link === "open");
  const answer = useRequest(method, () => (open() ? params() : null));
  let kept: Results[M] | undefined;
  return createMemo(() => {
    const current = answer();
    if (current.data !== undefined) {
      kept = current.data;
    }
    return current.data === undefined && current.error === undefined
      ? { ...current, data: kept }
      : current;
  });
}

export function useDisconnected(): () => boolean {
  return useModel((model) => model.link === "reconnecting");
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
