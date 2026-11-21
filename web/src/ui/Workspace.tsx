import { useLocation, useNavigate, useParams } from "@solidjs/router";
import type { JSX } from "@solidjs/web";
import {
  createContext,
  createEffect,
  createMemo,
  onSettled,
  Show,
  untrack,
  useContext,
} from "solid-js";

import { assert } from "../assert";
import { useRequest } from "../data";
import { KIND } from "../documents";
import {
  type At,
  followedLook,
  type Look,
  parseAt,
  parseSearch,
  stopPath,
  validateLook,
} from "../focus";
import { follow, latestStop } from "../follow";
import { toHref } from "../history";
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
  startLayout,
  type Wants,
} from "./panes";

export interface Focus {
  session: string;
  state: State;

  at: At | null;
  look: Look;

  trace: Backtrace | undefined;

  stale: "running" | "passed" | "disconnected" | null;
}

export const FocusContext = createContext<() => Focus | null>(() => null);

export function useFocus(): () => Focus {
  const read = useContext(FocusContext);
  return () => {
    const focus = read();
    if (!focus) {
      throw new Error("panes need a focus");
    }
    return focus;
  };
}

function useAt(): () => At | null {
  const params = useParams();
  return createMemo(() => {
    const { stop, thread, task, frame } = params;
    return stop !== undefined && frame !== undefined && (thread !== undefined || task !== undefined)
      ? parseAt({ stop, thread, task, frame })
      : null;
  });
}

export function Workspace() {
  const params = useParams();
  const location = useLocation();
  const session = createMemo(() => params.session ?? "");
  const look = createMemo(() => validateLook(parseSearch(location.search)));
  const state = useModel((model) => model.state);
  const link = useModel((model) => model.link);
  const at = useAt();
  useFollowing(session, at, look);
  const traceRequest = useRequest("backtrace", () => {
    const currentAt = at();
    const currentState = state();
    return currentAt && currentState?.session === session()
      ? { stop: currentAt.stop, thread: currentAt.thread, task: currentAt.task ?? null }
      : null;
  });
  const trace = createMemo(() => traceRequest().data);
  usePresence(at, trace);
  const frames = createMemo(() => trace()?.frames.length ?? 0);
  createEffect(
    () => frames(),
    (count) => {
      tab.setState({ frames: count });
    },
  );

  const focus = createMemo((): Focus | null => {
    const currentState = state();
    if (!currentState) {
      return null;
    }
    const currentAt = at();
    const inferior = currentState.inferior;
    const stale =
      link() === "open"
        ? currentAt && inferior.state === "running"
          ? "running"
          : currentAt && (inferior.state !== "stopped" || inferior.stop !== currentAt.stop)
            ? "passed"
            : null
        : "disconnected";
    return {
      session: session(),
      state: currentState,
      at: currentAt,
      look: look(),
      trace: trace(),
      stale,
    };
  });

  return <Workbench focus={focus()} wants="session" />;
}

export function Workbench(props: {
  focus: Focus | null;
  wants: Wants;
  start?: () => Layout;
  children?: JSX.Element;
}) {
  useArranged(
    () => props.wants,
    () => props.start ?? (props.wants === "session" ? debugLayout : startLayout),
  );
  useMirror(() => props.focus);
  useConnectWhenRefused(() => props.wants);
  const focus = createMemo(() => props.focus);
  return (
    <FocusContext value={focus}>
      <Show when={focus()}>
        <Documents />
      </Show>
      <div class="workbench">
        {props.children}
        <Dock setup={DOCK} />
      </div>
      <Show when={focus()}>
        <Palette />
      </Show>
    </FocusContext>
  );
}

function useArranged(wants: () => Wants, start: () => () => Layout) {
  const refused = restoreLayout();
  let arranged: Wants | null = null;

  const ensure = (wanted: Wants, makeStart: () => Layout) => {
    if (arranged === wanted) {
      return;
    }
    arranged = wanted;
    const current = workbench.getState().layout;
    if (misplaced(current, wanted)) {
      if (wanted === "start" && !misplaced(current, "session")) {
        setAside(current);
      }
      const back = wanted === "session" ? setAsideLayout(KINDS) : null;
      workbench.setState({ layout: back ?? makeStart() });
    }
  };

  ensure(untrack(wants), untrack(start));

  createEffect(
    () => [wants(), start()] as const,
    ([wanted, makeStart]) => {
      ensure(wanted, makeStart);
    },
  );

  onSettled(() => {
    if (refused) {
      flash(`The saved layout can't be used (${refused}), so the default layout is shown`);
    }
    return keepSaving();
  });
}

function useConnectWhenRefused(wants: () => Wants) {
  const link = useModel((model) => model.link);
  createEffect(
    () => [link(), wants()] as const,
    ([current, wanted]) => {
      if (wanted === "fleet") {
        return;
      }
      const refused = current === "unauthorized" || current === "unreachable";
      const timer = refused
        ? setTimeout(
            () => showPane(KIND.connect),
            current === "unreachable" ? UNREACHABLE_FOR_MS : 0,
          )
        : null;
      return () => {
        if (timer !== null) {
          clearTimeout(timer);
        }
      };
    },
  );
}

const UNREACHABLE_FOR_MS = 1000;

function useFollowing(session: () => string, at: () => At | null, look: () => Look) {
  const state = useModel((model) => model.state);
  const pinned = useTab((current) => current.pinned);
  const navigate = useNavigate();

  let followed: number | undefined;
  let initialized = false;
  let latestLook: Look = {};

  createEffect(
    () => look(),
    (next) => {
      latestLook = next;
    },
  );

  createEffect(
    () => {
      const currentAt = at();
      return {
        state: state(),
        session: session(),
        stop: currentAt?.stop,
        thread: currentAt?.thread,
        frame: currentAt?.frame,
        pinned: pinned(),
      };
    },
    ({ state: currentState, session: currentSession, stop, thread, frame, pinned: isPinned }) => {
      if (!initialized) {
        initialized = true;
        followed = currentState ? latestStop(currentState) : undefined;
      }
      if (!currentState || currentState.session !== currentSession) {
        return;
      }
      let where: At | null = null;
      if (stop !== undefined) {
        assert(thread !== undefined, `stop ${stop} shown without a thread`);
        assert(frame !== undefined, `stop ${stop} shown without a frame`);
        where = { stop, thread, frame };
      }
      const move = follow(currentState, where, followed, isPinned);

      if (currentState.inferior.state !== "running") {
        followed = latestStop(currentState);
      }
      const search = followedLook(latestLook) as Record<string, unknown>;
      if (move?.to === "stop") {
        navigate(toHref(stopPath(currentSession, move.at), search), { replace: true });
      } else if (move?.to === "session") {
        navigate(toHref(`/s/${currentSession}`, search), { replace: true });
      }
    },
  );
}

function useMirror(focus: () => Focus | null) {
  createEffect(
    () => {
      const current = focus();
      return {
        session: current?.session ?? null,
        at: current?.at ?? null,
        look: current?.look ?? NO_LOOK,
      };
    },
    ({ session, at, look }) => {
      tab.setState({ session, at, look });
    },
  );
}

const NO_LOOK: Look = {};

function usePresence(at: () => At | null, trace: () => Backtrace | undefined) {
  const connection = useConnection();
  const open = useModel((model) => model.link === "open");
  const location = useLocation();

  createEffect(
    () => {
      const currentAt = at();
      const frame = frameAt(trace(), currentAt);
      const label = currentAt
        ? `stop #${currentAt.stop}, frame ${currentAt.frame}${frame ? `, ${frame.name}` : ""}`
        : "the session";
      return {
        open: open(),
        url: `${location.pathname}${location.search}${location.hash}`,
        label,
      };
    },
    ({ open: isOpen, url, label }) => {
      if (!isOpen) {
        return;
      }
      const timer = setTimeout(() => {
        void connection.request("setFocus", { focus: { url, label } }).catch(() => {});
      }, 150);
      return () => clearTimeout(timer);
    },
  );
}
