// What each address opens in the one workbench. An address names a place: a
// session's focus, a profile, or nowhere in particular, which starts from
// Account. Old addresses of pages that became panes open those panes.

import { Navigate, Outlet, useNavigate, useParams, useSearch } from "@tanstack/react-router";
import {
  Fragment,
  useCallback,
  useEffect,
  useLayoutEffect,
  useMemo,
  useRef,
  useState,
} from "react";

import { followEmailLink, followSignIn } from "../account";
import { documentsGroup, KIND } from "../documents";
import { type Note, targetName } from "../model";
import { validateProfileLook } from "../profile/look";
import { useModel } from "../store";
import { tab as tabState } from "../tab";
import { change, useLayout, workbench } from "../workbench/store";
import { SessionLink } from "./Explorer";
import { profileLayout, showPane, startLayout } from "./panes";
import { PICKED } from "./Picker";
import { lookOf, profileOf, showAddress, shownProfilePane, viewOpen } from "./profile/link";
import { orgLink } from "./SingleSignOn";
import { Workbench } from "./Workspace";

/** `/`: the session when there is one; else what to start from, which is
 * the profile a server started on one serves. */
export function Home() {
  // Until the server says what it has, unless it won't or can't.
  const loading = useModel(
    (model) => model.state === null && (model.link === "connecting" || model.link === "open"),
  );
  const session = useModel((model) => model.state?.session ?? null);
  const busy = useModel((model) => model.state?.busy ?? null);
  const profile = useModel((model) => (busy ? undefined : model.profiles.at(-1)));
  const start = useMemo(
    () => (profile ? () => profileLayout(profile.id, profile.name, {}) : startLayout),
    [profile],
  );
  if (session) {
    return <ToSession session={session} />;
  }
  if (loading) {
    return <div className="page muted">Connecting to microscope…</div>;
  }
  return (
    <Workbench focus={null} wants="start" start={start}>
      {busy && <div className="workbench-notice muted">{busy}…</div>}
    </Workbench>
  );
}

function ToSession({ session }: { session: string }) {
  const params = useMemo(() => ({ session }), [session]);
  return <Navigate to="/s/$session" params={params} replace />;
}

/** `/pick?tab=`, the address of the page that chose what to debug: opens
 * the pane that page's tab became. */
export function Pick() {
  const { tab } = useSearch({ from: "/pick" });
  const navigate = useNavigate();
  useLayoutEffect(() => {
    showPane(PICKED[tab ?? "launch"]);
    void navigate({ to: "/", replace: true });
  }, [tab, navigate]);
  return <Workbench focus={null} wants="start" />;
}

/** `/confirm-email#token=`, `/login-link#token=`, and
 * `/reset-password#token=`, where the links in services' emails lead: the
 * Account pane, saying how following it went or asking for a new password. */
export function EmailLink({ link }: { link: "confirm" | "login" | "reset" }) {
  const navigate = useNavigate();
  useLayoutEffect(() => {
    followEmailLink(link);
    showPane(KIND.account);
    void navigate({ to: "/", replace: true });
  }, [link, navigate]);
  return <Workbench focus={null} wants="start" />;
}

/** `/signed-in#…`, where services sends the browser back after single
 * sign-on: the Account pane, saying how it went. */
export function SignedIn() {
  const navigate = useNavigate();
  useLayoutEffect(() => {
    followSignIn();
    showPane(KIND.account);
    void navigate({ to: "/", replace: true });
  }, [navigate]);
  return <Workbench focus={null} wants="start" />;
}

/** `/sso#key=`, an organization's single sign-on link: the Account pane,
 * offering the way through it. */
export function OrgSignIn() {
  const navigate = useNavigate();
  useLayoutEffect(() => {
    orgLink.setState({ key: new URLSearchParams(window.location.hash.slice(1)).get("key") });
    showPane(KIND.account);
    void navigate({ to: "/", replace: true });
  }, [navigate]);
  return <Workbench focus={null} wants="start" />;
}

/** `/profile/$profile`: the profile's pane of the address's view, looking
 * as it says. Back and forward move through the looks it had. */
export function ProfileRoute() {
  const { profile } = useParams({ from: "/profile/$profile" });
  // The router keeps what the address says beside what it validated, such
  // as an old link's view, so the look is only what validates.
  const search = useSearch({ from: "/profile/$profile" });
  const look = useMemo(() => validateProfileLook(search), [search]);
  const name = useModel((model) => model.profiles.find((each) => each.id === profile)?.name ?? "");
  // After the workbench is arranged, before it paints.
  useLayoutEffect(() => {
    change((layout) =>
      showAddress(layout, profile, name, look, documentsGroup(layout, tabState.getState().files)),
    );
  }, [profile, name, look]);
  // Once the pane it names closes, the address names a profile's pane
  // still shown, or none.
  const navigate = useNavigate();
  const view = look.view ?? null;
  const open = useLayout((layout) => viewOpen(layout, profile, view));
  const was = useRef({ profile, view, open });
  useEffect(() => {
    const before = was.current;
    was.current = { profile, view, open };
    if (before.profile !== profile || before.view !== view || !before.open || open) {
      return;
    }
    const layout = workbench.getState().layout;
    const next = shownProfilePane(layout);
    const pane = next ? layout.panes[next] : undefined;
    const shown = next ? lookOf(layout, next) : null;
    const of = pane ? profileOf(layout, pane) : null;
    if (shown && of) {
      void navigate({
        to: "/profile/$profile",
        params: { profile: of },
        search: shown,
        replace: true,
      });
    } else {
      void navigate({ to: "/", replace: true });
    }
  }, [profile, view, open, navigate]);
  const start = useCallback(() => profileLayout(profile, name, look), [profile, name, look]);
  return <Workbench focus={null} wants="start" start={start} />;
}

/** The latest notice that ended the session of `program`, as the server
 * words it: `ended the kvstore session`, maybe with why it failed after. */
export function endedNotice(notices: readonly Note[], program: string | null): Note | undefined {
  if (!program) {
    return undefined;
  }
  const text = `ended the ${program} session`;
  return notices.findLast((notice) => notice.text === text || notice.text.startsWith(`${text}, `));
}

/** `/s/$session`: the session, or the panes with why it is not here. */
export function SessionRoute() {
  const { session } = useParams({ from: "/s/$session" });
  const state = useModel((model) => model.state);
  const notices = useModel((model) => model.notices);
  const navigate = useNavigate();
  // A tab that saw this session live follows it to the next one; a tab
  // opened on a link to an ended session says so instead. What it saw is
  // the program the session debugged, which names it in notices.
  const [seen, setSeen] = useState<{ program: string | null }>();
  const current = state?.session ?? null;
  if (current === session) {
    const program = targetName(state);
    if (seen === undefined || seen.program !== program) {
      setSeen({ program });
    }
  }
  const followed = seen !== undefined;

  useEffect(() => {
    if (followed && current && current !== session) {
      void navigate({ to: "/s/$session", params: { session: current }, replace: true });
    }
  }, [followed, current, session, navigate]);

  if (!state || current === session) {
    // Nothing a pane holds outlives its session.
    return (
      <Fragment key={session}>
        <Outlet />
      </Fragment>
    );
  }
  if (state.busy) {
    return (
      <Workbench focus={null} wants="session">
        <div className="workbench-notice muted">{state.busy}…</div>
      </Workbench>
    );
  }
  const ended = endedNotice(notices, seen?.program ?? null);
  return (
    <Workbench focus={null} wants="session">
      {/* oxlint-disable-next-line jsx-a11y/prefer-tag-over-role -- a styled notice, not a form output */}
      <div className="workbench-notice" role="status" data-testid="session-ended">
        {/* Only a tab that saw the session knows it ran here at all. */}
        <b>{followed ? "This session ended." : "This session is not here."}</b>{" "}
        {followed ? "" : "It ended, or the link is wrong. "}
        {ended ? `${ended.name} ${ended.text}. ` : ""}
        {current ? (
          <SessionLink session={current}>Go to {targetName(state)}</SessionLink>
        ) : (
          "Launch, attach, or open a core dump from the File menu."
        )}
      </div>
    </Workbench>
  );
}
