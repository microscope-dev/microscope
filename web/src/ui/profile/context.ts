import { createContext, useContext, useMemo, useRef } from "react";

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

export const ProfileContext = createContext<ProfileFocus | null>(null);

export function useProfile(): ProfileFocus {
  const focus = useContext(ProfileContext);
  if (!focus) {
    throw new Error("profile views need a ProfileContext");
  }
  return focus;
}

export function useAnswer<M extends Method>(
  method: M,
  params: (ParamsOf<M> extends undefined ? undefined : ParamsOf<M>) | null,
): Requested<Results[M]> {
  const open = useModel((model) => model.link === "open");
  const answer = useRequest(method, open ? params : null);
  const kept = useRef<Results[M] | undefined>(undefined);
  if (answer.data !== undefined) {
    // oxlint-disable-next-line react/refs -- keeps the last answer without a second render
    kept.current = answer.data;
  }
  return answer.data === undefined && answer.error === undefined
    ? // oxlint-disable-next-line react/refs -- keeps the last answer without a second render
      { ...answer, data: kept.current }
    : answer;
}

export function useDisconnected(): boolean {
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

export function useTree(query: ProfileQuery): Fetched {
  const answer = useAnswer("profileTree", query);
  const tree = useMemo(
    () => (answer.data ? decodeTree(answer.data, answer.data.payload) : null),
    [answer.data],
  );
  return { tree, error: answer.error, current: answer.current };
}
