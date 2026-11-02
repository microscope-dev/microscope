// The workbench every address shows. A session's panes share the session,
// stop, and frame its address selects.

import { useLocation, useNavigate, useParams, useSearch } from "@tanstack/react-router";
import {
  createContext,
  type ReactNode,
  useContext,
  useEffect,
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
import { type Layout, paneOf } from "../workbench/layout";
import { keepSaving } from "../workbench/persist";
import { workbench } from "../workbench/store";
import { Documents } from "./Documents";
import { Palette } from "./Palette";
import {
  DOCK,
  debugLayout,
  misplaced,
  restoreLayout,
  showPane,
  type Wants,
  welcomeLayout,
} from "./panes";

export interface Focus {
  session: string;
  state: State;
  /** The stop, thread or task, and frame shown; none before the program
   * stops. */
  at: At | null;
  look: Look;
  /** The stack of the thread or task shown, once it has arrived. */
  trace: Backtrace | undefined;
  /** Why displayed data may be stale; null when current. */
  stale: "running" | "passed" | "disconnected" | null;
}

/** Exported so tests can render a pane at a focus they choose. */
export const FocusContext = createContext<Focus | null>(null);

export function useFocus(): Focus {
  const focus = useContext(FocusContext);
  if (!focus) {
    throw new Error("panes need a focus");
  }
  return focus;
}

/** The URL's stop, thread or task, and frame, or null on the session's
 * own page. */
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

/** A session's panes, at the focus the address names. */
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
      link !== "open"
        ? "disconnected"
        : at && inferior.state === "running"
          ? "running"
          : at && (inferior.state !== "stopped" || inferior.stop !== at.stop)
            ? "passed"
            : null;
    return { session, state, at, look, trace, stale };
  }, [session, state, at, look, trace, link]);

  return <Workbench focus={focus} wants="session" />;
}

/** The panes, as every address shows them: with a session's focus when the
 * address names one, and with `notice` above them when it says something
 * the panes can't. */
export function Workbench({
  focus,
  wants,
  start = wants === "session" ? debugLayout : welcomeLayout,
  notice,
}: {
  focus: Focus | null;
  wants: Wants;
  /** The layout to start from when the one in use holds nothing for what
   * the address wants. */
  start?: () => Layout;
  notice?: ReactNode;
}) {
  useArranged(wants, start);
  useMirror(focus);
  useConnectWhenRefused();
  return (
    <FocusContext value={focus}>
      {focus && <Documents />}
      <div className="workbench">
        {notice}
        <Dock setup={DOCK} />
      </div>
      {focus && <Palette />}
    </FocusContext>
  );
}

/** Arranges the panes as this browser last had them, then as the address
 * wants them, during the first render so the panes render within it; then
 * keeps the arrangement as it changes. */
function useArranged(wants: Wants, start: () => Layout) {
  // Read once, so StrictMode's second render agrees.
  const [refused] = useState(restoreLayout);
  const arranged = useRef<Wants | null>(null);
  if (arranged.current !== wants) {
    arranged.current = wants;
    if (misplaced(workbench.getState().layout, wants)) {
      workbench.setState({ layout: start() });
    }
  }
  useEffect(() => {
    if (refused) {
      flash(`The layout this browser kept can't be used (${refused}), so it starts over`);
    }
  }, [refused]);
  useEffect(keepSaving, []);
}

/** Shows Connect when microscope refuses the page's link, which is the
 * pane that can fix it. */
function useConnectWhenRefused() {
  const refused = useModel((model) => model.link === "unauthorized");
  useEffect(() => {
    if (refused && !paneOf(workbench.getState().layout, KIND.connect, "")) {
      showPane(KIND.connect);
    }
  }, [refused]);
}

/** Moves a following tab to each new stop, replacing the address (D3). */
function useFollowing(session: string, at: At | null, look: Look) {
  const state = useModel((model) => model.state);
  const pinned = useTab((current) => current.pinned);
  const navigate = useNavigate();
  // The latest stop as of the previous state: what a following tab showed.
  const followed = useRef<number | undefined>(state ? latestStop(state) : undefined);
  const lookRef = useRef(look);
  lookRef.current = look;
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
    // A running state shows no stop, so it leaves the one a following tab
    // showed. Its stops can be another member's, once the member that
    // stopped exits and the group selects another while it runs.
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

/** Mirrors the focus into the tab store, for the keyboard. */
function useMirror(focus: Focus | null) {
  const session = focus?.session ?? null;
  const at = focus?.at ?? null;
  const look = focus?.look ?? NO_LOOK;
  useEffect(() => {
    tab.setState({ session, at, look });
  }, [session, at, look]);
}

const NO_LOOK: Look = {};

/** Tells everyone where this tab looks. */
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
      void connection.request("setFocus", { focus: { url, label } }).catch(() => undefined);
    }, 150);
    return () => clearTimeout(timer);
  }, [connection, open, url, label]);
}
