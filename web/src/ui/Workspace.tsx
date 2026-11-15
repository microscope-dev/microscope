import { useLocation, useNavigate, useParams, useSearch } from "@tanstack/react-router";
import {
  createContext,
  type ReactNode,
  useContext,
  useEffect,
  useLayoutEffect,
  useMemo,
  useRef,
  useState,
} from "react";

import { assert } from "../assert";
import { useRequest } from "../data";
import { KIND } from "../documents";
import { type At, followedLook, type Look, parseAt } from "../focus";
import { follow, latestStop } from "../follow";
import type { Backtrace, State } from "../protocol";
import { frameAt } from "../stack";
import { useConnection, useModel } from "../store";
import { flash, tab, useTab } from "../tab";
import { Dock } from "../workbench/Dock";
import type { Layout } from "../workbench/layout";
import { keepSaving, setAside, setAsideLayout } from "../workbench/persist";
import { workbench } from "../workbench/store";
import { Documents } from "./Documents";
import { Palette } from "./Palette";
import {
  DOCK,
  debugLayout,
  KINDS,
  misplaced,
  restoreLayout,
  showPane,
  type Wants,
  startLayout,
} from "./panes";

export interface Focus {
  session: string;
  state: State;

  at: At | null;
  look: Look;

  trace: Backtrace | undefined;

  stale: "running" | "passed" | "disconnected" | null;
}

export const FocusContext = createContext<Focus | null>(null);

export function useFocus(): Focus {
  const focus = useContext(FocusContext);
  if (!focus) {
    throw new Error("panes need a focus");
  }
  return focus;
}

function useAt(): At | null {
  const params = useParams({ strict: false }) as {
    stop?: string;
    thread?: string;
    task?: string;
    frame?: string;
  };
  const { stop, thread, task, frame } = params;
  return useMemo(
    () =>
      stop !== undefined && frame !== undefined && (thread !== undefined || task !== undefined)
        ? parseAt({ stop, thread, task, frame })
        : null,
    [stop, thread, task, frame],
  );
}

export function Workspace() {
  const { session } = useParams({ from: "/s/$session" });
  const look = useSearch({ from: "/s/$session" });
  const state = useModel((model) => model.state);
  const link = useModel((model) => model.link);
  const at = useAt();
  useFollowing(session, at, look);
  const trace = useRequest(
    "backtrace",
    at && state?.session === session
      ? { stop: at.stop, thread: at.thread, task: at.task ?? null }
      : null,
  ).data;
  usePresence(at, trace);
  const frames = trace?.frames.length ?? 0;
  useEffect(() => {
    tab.setState({ frames });
  }, [frames]);

  const focus = useMemo((): Focus | null => {
    if (!state) {
      return null;
    }
    const inferior = state.inferior;
    const stale =
      link === "open"
        ? at && inferior.state === "running"
          ? "running"
          : at && (inferior.state !== "stopped" || inferior.stop !== at.stop)
            ? "passed"
            : null
        : "disconnected";
    return { session, state, at, look, trace, stale };
  }, [session, state, at, look, trace, link]);

  return <Workbench focus={focus} wants="session" />;
}

export function Workbench({
  focus,
  wants,
  start = wants === "session" ? debugLayout : startLayout,
  children,
}: {
  focus: Focus | null;
  wants: Wants;

  start?: () => Layout;
  children?: ReactNode;
}) {
  useArranged(wants, start);
  useMirror(focus);
  useConnectWhenRefused();
  return (
    <FocusContext value={focus}>
      {focus && <Documents />}
      <div className="workbench">
        {children}
        <Dock setup={DOCK} />
      </div>
      {focus && <Palette />}
    </FocusContext>
  );
}

function useArranged(wants: Wants, start: () => Layout) {
  const [refused] = useState(restoreLayout);
  const arranged = useRef<Wants | null>(null);
  // oxlint-disable-next-line react/refs -- arranged during render, before the panes render
  if (arranged.current !== wants) {
    // oxlint-disable-next-line react/refs -- as above
    arranged.current = wants;
    const current = workbench.getState().layout;
    if (misplaced(current, wants)) {
      if (wants === "start" && !misplaced(current, "session")) {
        setAside(current);
      }
      const back = wants === "session" ? setAsideLayout(KINDS) : null;
      workbench.setState({ layout: back ?? start() });
    }
  }
  useEffect(() => {
    if (refused) {
      flash(`The saved layout can't be used (${refused}), so the default layout is shown`);
    }
  }, [refused]);
  useEffect(keepSaving, []);
}

function useConnectWhenRefused() {
  const link = useModel((model) => model.link);
  useEffect(() => {
    const refused = link === "unauthorized" || link === "unreachable";
    const timer = refused
      ? setTimeout(() => showPane(KIND.connect), link === "unreachable" ? UNREACHABLE_FOR_MS : 0)
      : null;
    return () => {
      if (timer !== null) {
        clearTimeout(timer);
      }
    };
  }, [link]);
}

const UNREACHABLE_FOR_MS = 1000;

function useFollowing(session: string, at: At | null, look: Look) {
  const state = useModel((model) => model.state);
  const pinned = useTab((current) => current.pinned);
  const navigate = useNavigate();

  const followed = useRef<number | undefined>(state ? latestStop(state) : undefined);
  const lookRef = useRef(look);
  useLayoutEffect(() => {
    lookRef.current = look;
  });
  const stop = at?.stop;
  const thread = at?.thread;
  const frame = at?.frame;

  useEffect(() => {
    if (!state || state.session !== session) {
      return;
    }
    let where: At | null = null;
    if (stop !== undefined) {
      assert(thread !== undefined, `stop ${stop} shown without a thread`);
      assert(frame !== undefined, `stop ${stop} shown without a frame`);
      where = { stop, thread, frame };
    }
    const move = follow(state, where, followed.current, pinned);

    if (state.inferior.state !== "running") {
      followed.current = latestStop(state);
    }
    const search = followedLook(lookRef.current);
    if (move?.to === "stop") {
      void navigate({
        to: "/s/$session/stop/$stop/t/$thread/f/$frame",
        params: {
          session,
          stop: String(move.at.stop),
          thread: String(move.at.thread),
          frame: String(move.at.frame),
        },
        search,
        replace: true,
      });
    } else if (move?.to === "session") {
      void navigate({ to: "/s/$session", params: { session }, search, replace: true });
    }
  }, [state, session, stop, thread, frame, pinned, navigate]);
}

function useMirror(focus: Focus | null) {
  const session = focus?.session ?? null;
  const at = focus?.at ?? null;
  const look = focus?.look ?? NO_LOOK;
  useEffect(() => {
    tab.setState({ session, at, look });
  }, [session, at, look]);
}

const NO_LOOK: Look = {};

function usePresence(at: At | null, trace: Backtrace | undefined) {
  const connection = useConnection();
  const open = useModel((model) => model.link === "open");
  const location = useLocation();
  const url = location.href;
  const frame = frameAt(trace, at);
  const label = at
    ? `stop #${at.stop}, frame ${at.frame}${frame ? `, ${frame.name}` : ""}`
    : "the session";

  useEffect(() => {
    if (!open) {
      return;
    }
    const timer = setTimeout(() => {
      void connection.request("setFocus", { focus: { url, label } }).catch(() => {});
    }, 150);
    return () => clearTimeout(timer);
  }, [connection, open, url, label]);
}
