import { type RouteSectionProps, useLocation, useNavigate, useParams } from "@solidjs/router";
import { createEffect, createMemo, createSignal, Match, onSettled, Show, Switch } from "solid-js";

import { followEmailLink, followSignIn } from "../account";
import { documentsGroup, KIND } from "../documents";
import { parseSearch } from "../focus";
import { toHref } from "../history";
import { type Note, targetName } from "../model";
import { validateProfileLook } from "../profile/look";
import { useModel } from "../store";
import { tab as tabState } from "../tab";
import { standalone } from "../target";
import { change, useLayout, workbench } from "../workbench/store";
import { RecordedSessionRoute } from "./dashboards/DashboardRoutes";
import { SessionLink } from "./Explorer";
import { profileLayout, showPane, startLayout } from "./panes";
import { PICKED, pickerSearch } from "./Picker";
import { lookOf, profileOf, showAddress, shownProfilePane, viewOpen } from "./profile/link";
import { orgLink } from "./SingleSignOn";
import { Workbench } from "./Workspace";

export function Home() {
  const loading = useModel(
    (model) => model.state === null && (model.link === "connecting" || model.link === "open"),
  );
  const session = useModel((model) => model.state?.session ?? null);
  const busy = useModel((model) => model.state?.busy ?? null);
  const profile = useModel((model) => (model.state?.busy ? undefined : model.profiles.at(-1)));
  const start = createMemo(() => {
    const current = profile();
    return current ? () => profileLayout(current.id, current.name, {}) : startLayout;
  });

  return (
    <Switch
      fallback={
        <Workbench focus={null} wants="start" start={start()}>
          <Show when={busy()}>{(text) => <div class="workbench-notice muted">{text()}…</div>}</Show>
        </Workbench>
      }
    >
      <Match when={session()}>{(id) => <ToSession session={id()} />}</Match>
      <Match when={loading()}>
        <div class="page muted">Connecting to microscope…</div>
      </Match>
    </Switch>
  );
}

function ToSession(props: { session: string }) {
  const navigate = useNavigate();
  createEffect(
    () => props.session,
    (id) => {
      navigate(toHref(`/s/${id}`), { replace: true });
    },
  );
  return null;
}

export function Pick() {
  const location = useLocation();
  const tabName = createMemo(() => pickerSearch(parseSearch(location.search)).tab);
  const navigate = useNavigate();
  createEffect(
    () => tabName(),
    (chosen) => {
      showPane(PICKED[chosen ?? "launch"]);
      navigate(toHref("/"), { replace: true });
    },
  );
  return <Workbench focus={null} wants="start" />;
}

export function EmailLink(props: { link: "confirm" | "login" | "reset" }) {
  const navigate = useNavigate();
  createEffect(
    () => props.link,
    (kind) => {
      followEmailLink(kind);
      showPane(KIND.account);
      navigate(toHref("/"), { replace: true });
    },
  );
  return <Workbench focus={null} wants="start" />;
}

export function SignedIn() {
  const navigate = useNavigate();
  onSettled(() => {
    followSignIn();
    showPane(KIND.account);
    navigate(toHref("/"), { replace: true });
  });
  return <Workbench focus={null} wants="start" />;
}

export function OrgSignIn() {
  const navigate = useNavigate();
  onSettled(() => {
    orgLink.setState({ key: new URLSearchParams(window.location.hash.slice(1)).get("key") });
    showPane(KIND.account);
    navigate(toHref("/"), { replace: true });
  });
  return <Workbench focus={null} wants="start" />;
}

export function ProfileRoute() {
  const params = useParams();
  const location = useLocation();
  const profile = createMemo(() => params.profile ?? "");
  const look = createMemo(() => validateProfileLook(parseSearch(location.search)));
  const name = useModel(
    (model) => model.profiles.find((each) => each.id === profile())?.name ?? "",
  );

  createEffect(
    () => [profile(), name(), look()] as const,
    ([id, profileName, currentLook]) => {
      change((layout) =>
        showAddress(
          layout,
          id,
          profileName,
          currentLook,
          documentsGroup(layout, tabState.getState().files),
        ),
      );
    },
  );

  const navigate = useNavigate();
  const view = createMemo(() => look().view ?? null);
  const open = useLayout((layout) => viewOpen(layout, profile(), view()));
  // When the user closes the pane backing this URL (open -> closed), redirect to another
  // open profile pane or fall back to home.
  let was: { profile: string; view: string | null; open: boolean } | null = null;
  createEffect(
    () => ({ profile: profile(), view: view(), open: open() }),
    (next) => {
      const before = was;
      was = next;
      if (
        !before ||
        before.profile !== next.profile ||
        before.view !== next.view ||
        !before.open ||
        next.open
      ) {
        return;
      }
      const layout = workbench.getState().layout;
      const shownPane = shownProfilePane(layout);
      const pane = shownPane ? layout.panes[shownPane] : undefined;
      const shown = shownPane ? lookOf(layout, shownPane) : null;
      const of = pane ? profileOf(layout, pane) : null;
      if (shown && of) {
        navigate(toHref(`/profile/${of}`, shown as Record<string, unknown>), { replace: true });
      } else {
        navigate(toHref("/"), { replace: true });
      }
    },
  );
  const start = () => profileLayout(profile(), name(), look());
  return <Workbench focus={null} wants="start" start={start} />;
}

export function endedNotice(notices: readonly Note[], program: string | null): Note | undefined {
  if (!program) {
    return undefined;
  }
  const text = `ended the ${program} session`;
  return notices.findLast((notice) => notice.text === text || notice.text.startsWith(`${text}, `));
}

/**
 * Guards `/s/:session` routes: tracks whether the route ever matched the live session so an
 * ended session is distinguished from a bad link (and auto-follows replacement sessions), while
 * delegating numeric non-live IDs in standalone mode to recorded session dashboards.
 */
export function SessionRoute(props: RouteSectionProps) {
  const params = useParams();
  const session = createMemo(() => params.session ?? "");
  const state = useModel((model) => model.state);
  const notices = useModel((model) => model.notices);
  const navigate = useNavigate();

  const [seen, setSeen] = createSignal<{ program: string | null }>();
  const current = createMemo(() => state()?.session ?? null);

  createEffect(
    () => [current(), session(), state()] as const,
    ([currentId, routeSession, currentState]) => {
      if (currentId === routeSession && currentState) {
        const program = targetName(currentState);
        setSeen((previous) =>
          previous !== undefined && previous.program === program ? previous : { program },
        );
      }
    },
  );

  const followed = createMemo(() => seen() !== undefined || current() === session());

  createEffect(
    () => [seen() !== undefined, current(), session()] as const,
    ([wasSeen, currentId, routeSession]) => {
      if (wasSeen && currentId && currentId !== routeSession) {
        navigate(toHref(`/s/${currentId}`), { replace: true });
      }
    },
  );

  const ended = createMemo(() => endedNotice(notices(), seen()?.program ?? null));
  const recordedId = createMemo(() =>
    standalone && /^\d+$/u.test(session()) && current() !== session() ? Number(session()) : null,
  );

  return (
    <Show
      when={recordedId() === null}
      fallback={<RecordedSessionRoute sessionId={recordedId() ?? 0} />}
    >
      <Switch
        fallback={
          <Workbench focus={null} wants="session">
            {/* oxlint-disable-next-line jsx-a11y/prefer-tag-over-role -- a styled notice, not a form output */}
            <div class="workbench-notice" role="status" data-testid="session-ended">
              <b>{followed() ? "This session ended." : "This session is not here."}</b>{" "}
              {followed() ? "" : "It ended, or the link is wrong. "}
              <Show when={ended()}>{(note) => <>{`${note().name} ${note().text}. `}</>}</Show>
              <Show
                when={current()}
                fallback="Launch, attach, or open a core dump from the File menu."
              >
                {(id) => <SessionLink session={id()}>Go to {targetName(state())}</SessionLink>}
              </Show>
            </div>
          </Workbench>
        }
      >
        <Match when={!state() || current() === session()}>
          <Show when={session()} keyed>
            {props.children}
          </Show>
        </Match>
        <Match when={state()?.busy}>
          {(busy) => (
            <Workbench focus={null} wants="session">
              <div class="workbench-notice muted">{busy()}…</div>
            </Workbench>
          )}
        </Match>
      </Switch>
    </Show>
  );
}
