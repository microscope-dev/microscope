// Moving the tab's focus. Every move is a navigation, so the address bar is
// always a working link to what the tab shows.

import { useNavigate } from "@tanstack/react-router";
import { useCallback, useMemo } from "react";
import { useRequest } from "../data";
import {
  type At,
  followedLook,
  formatPlace,
  type Look,
  linkPath,
  recordedPath,
  showingSource,
  taskSegment,
} from "../focus";
import { useModel } from "../store";
import { asked } from "../tab";
import { useFocus } from "./Workspace";

/** `replace` replaces browser history; `ask` reopens the displayed source file.
 * Frame navigation always reopens its source. */
export interface Moving {
  replace?: boolean;
  ask?: boolean;
}

export type Go = (at: At | null, look?: Look, options?: Moving) => void;

type Navigate = ReturnType<typeof useNavigate>;

/** Navigate within `session`, preserving `current` view settings unless overridden. */
export function go(
  navigate: Navigate,
  session: string,
  current: Look,
  at: At | null,
  look?: Look,
  options?: Moving,
): void {
  // A new frame shows its own code unless the caller says what to show.
  const search = look ?? followedLook(current);
  const replace = options?.replace ?? false;
  const going =
    at === null
      ? navigate({ to: "/s/$session", params: { session }, search, replace })
      : at.task
        ? navigate({
            to: "/s/$session/stop/$stop/task/$task/f/$frame",
            params: {
              session,
              stop: String(at.stop),
              task: taskSegment(at.task),
              frame: String(at.frame),
            },
            search,
            replace,
          })
        : navigate({
            to: "/s/$session/stop/$stop/t/$thread/f/$frame",
            params: {
              session,
              stop: String(at.stop),
              thread: String(at.thread),
              frame: String(at.frame),
            },
            search,
            replace,
          });
  // Reopen the source only after navigation completes.
  if (!look || options?.ask) {
    void going.then(asked);
  }
}

/** Goes to a stop, thread or task, and frame, or to the session itself. */
export function useGo(): Go {
  const navigate = useNavigate();
  const { session, look: current } = useFocus();
  return useCallback(
    (at, look, options) => go(navigate, session, current, at, look, options),
    [navigate, session, current],
  );
}

/** Changes how the tab looks at its focus, keeping the focus. */
export function useLook(): (change: (look: Look) => Look, options?: Moving) => void {
  const go = useGo();
  const { at, look } = useFocus();
  return useCallback(
    (change, options) => go(at, change(look), { ...options, replace: options?.replace ?? true }),
    [go, at, look],
  );
}

/** Shows a recorded source path's lines, opening its file even if closed. */
export function useShowSource(): (path: string, line: number, end?: number) => void {
  const look = useLook();
  const paths = useLinkPaths();
  return useCallback(
    (path, line, end) => {
      look(
        (current) =>
          showingSource(current, formatPlace({ path: paths.link(path), line, end: end ?? line })),
        { replace: false, ask: true },
      );
    },
    [look, paths],
  );
}

/**
 * Turns recorded source paths into the short ones links carry, and back,
 * against the files the debug information records.
 */
export function useLinkPaths() {
  const cwd = useModel((model) => model.hello?.cwd);
  const session = useModel((model) => model.state?.session);
  const files = useRequest("sources", session ? undefined : null).data?.files;
  return useMemo(
    () => ({
      link: (path: string) => linkPath(path, cwd, files),
      recorded: (path: string) => recordedPath(path, cwd, files),
    }),
    [cwd, files],
  );
}
