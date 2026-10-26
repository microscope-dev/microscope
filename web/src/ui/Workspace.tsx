// All panes share the session, stop, and frame selected by the URL.

import { useLocation, useNavigate, useParams, useSearch } from "@tanstack/react-router";
import { createContext, useContext, useEffect, useMemo, useRef } from "react";
import { useRequest } from "../data";
import { type At, followedLook, type Look, parseAt } from "../focus";
import { follow, latestStop } from "../follow";
import type { Backtrace, State } from "../protocol";
import { frameAt } from "../stack";
import { useConnection, useModel } from "../store";
import { tab, useTab } from "../tab";
import { Banner } from "./Banner";
import { BottomPanel } from "./BottomPanel";
import { Breakpoints } from "./Breakpoints";
import { CodeArea } from "./CodeArea";
import { Palette } from "./Palette";
import { Processes } from "./Processes";
import { Registers } from "./Registers";
import { useSplit } from "./Split";
import { Stack } from "./Stack";
import { Tasks } from "./Tasks";
import { Threads } from "./Threads";
import { Variables, Watch } from "./Values";

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

export function Workspace() {
  const { session } = useParams({ from: "/s/$session" });
  const look = useSearch({ from: "/s/$session" });
  const state = useModel((model) => model.state);
  const link = useModel((model) => model.link);
  const at = useAt();
  useFollowing(session, at, look);
  useMirror(session, at, look);
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
  const left = useSplit("microscope-split-left", 270, {
    label: "Resize the left column",
    min: 180,
    max: 520,
  });
  const right = useSplit("microscope-split-right", 340, {
    label: "Resize the right column",
    min: 220,
    max: 720,
    reversed: true,
  });

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

  if (!focus) {
    return null;
  }
  return (
    <FocusContext value={focus}>
      <main
        className="workspace"
        style={{
          gridTemplateColumns: `${left.size}px 5px minmax(0, 1fr) 5px ${right.size}px`,
        }}
      >
        <div className="column">
          <Threads />
          <Tasks />
          <Stack />
          <Breakpoints />
        </div>
        <div {...left.handle} />
        <div className="column center">
          <Processes />
          <Banner />
          <CodeArea />
          <BottomPanel />
        </div>
        <div {...right.handle} />
        <div className="column right">
          <Watch />
          <Variables />
          <Registers />
        </div>
      </main>
      <Palette />
    </FocusContext>
  );
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
    const where = stop === undefined ? null : { stop, thread: thread ?? 0, frame: frame ?? 0 };
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
function useMirror(session: string, at: At | null, look: Look) {
  useEffect(() => {
    tab.setState({ session, at, look });
  }, [session, at, look]);
}

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
