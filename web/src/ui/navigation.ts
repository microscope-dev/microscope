import { type Navigator, useNavigate } from "@solidjs/router";

import { useRequest } from "../data";
import {
  type At,
  followedLook,
  formatPlace,
  type Look,
  linkPath,
  recordedPath,
  showingSource,
  stopPath,
} from "../focus";
import { toHref } from "../history";
import { useModel } from "../store";
import { asked } from "../tab";
import { useFocus } from "./Workspace";

export interface Moving {
  replace?: boolean;
  ask?: boolean;
}

export type Go = (at: At | null, look?: Look, options?: Moving) => void;

export function go(
  navigate: Navigator,
  session: string,
  current: Look,
  at: At | null,
  look?: Look,
  options?: Moving,
): void {
  const search = look ?? followedLook(current);
  const replace = options?.replace ?? false;
  const path = at === null ? `/s/${session}` : stopPath(session, at);
  navigate(toHref(path, search as Record<string, unknown>), { replace });

  // Signal an explicit navigation after the router settles so Documents re-opens/reveals the target.
  if (!look || options?.ask) {
    queueMicrotask(asked);
  }
}

export function useGo(): Go {
  const navigate = useNavigate();
  const focus = useFocus();
  return (at, look, options) => {
    const current = focus();
    go(navigate, current.session, current.look, at, look, options);
  };
}

export function useLook(): (change: (look: Look) => Look, options?: Moving) => void {
  const goTo = useGo();
  const focus = useFocus();
  return (change, options) => {
    const current = focus();
    goTo(current.at, change(current.look), { ...options, replace: options?.replace ?? true });
  };
}

export function useShowSource(): (path: string, line: number, end?: number) => void {
  const look = useLook();
  const paths = useLinkPaths();
  return (path, line, end) => {
    look(
      (current) =>
        showingSource(current, formatPlace({ path: paths.link(path), line, end: end ?? line })),
      { replace: false, ask: true },
    );
  };
}

export function useLinkPaths() {
  const cwd = useModel((model) => model.hello?.cwd);
  const session = useModel((model) => model.state?.session);
  const sources = useRequest("sources", () => (session() ? undefined : null));
  return {
    link: (path: string) => linkPath(path, cwd(), sources().data?.files),
    recorded: (path: string) => recordedPath(path, cwd(), sources().data?.files),
  };
}
