// Shared profile state. View changes update the URL so the view can be linked.

import { createContext, useContext, useRef } from "react";
import { type Requested, useRequest } from "../../data";
import type { ProfileLook } from "../../profile/look";
import type { Method, ParamsOf, ProfileQuery, ProfileSummary, Results } from "../../protocol";
import { useModel } from "../../store";

export interface ProfileFocus {
  profile: string;
  summary: ProfileSummary;
  look: ProfileLook;
  /** The query the look asks. */
  query: ProfileQuery;
  /** What the query's weights are: waits are nanoseconds. */
  unit: string;
  /** Changes the look, replacing the address unless `push` is set. */
  setLook(change: (look: ProfileLook) => ProfileLook, push?: boolean): void;
}

export const ProfileContext = createContext<ProfileFocus | null>(null);

export function useProfile(): ProfileFocus {
  const focus = useContext(ProfileContext);
  if (!focus) {
    throw new Error("profile views need a ProfileContext");
  }
  return focus;
}

/** Retries on reconnection, which cancels in-flight requests.
 * Keeps the last result visible while disconnected or awaiting a new result. */
export function useAnswer<M extends Method>(
  method: M,
  params: (ParamsOf<M> extends undefined ? undefined : ParamsOf<M>) | null,
): Requested<Results[M]> {
  const open = useModel((model) => model.link === "open");
  const answer = useRequest(method, open ? params : null);
  const kept = useRef<Results[M] | undefined>(undefined);
  if (answer.data !== undefined) {
    kept.current = answer.data;
  }
  return answer.data === undefined && answer.error === undefined
    ? { ...answer, data: kept.current }
    : answer;
}

/** Whether the connection is down, so what shows may be out of date. */
export function useDisconnected(): boolean {
  return useModel((model) => model.link === "reconnecting");
}

/** Reads a CSS color token where the page is drawn. */
export function token(name: string): string {
  return getComputedStyle(document.documentElement).getPropertyValue(name).trim();
}
