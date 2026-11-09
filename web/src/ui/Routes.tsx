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
import { KIND } from "../documents";
import { type Note, targetName } from "../model";
import { useModel } from "../store";
import { activate, groups, linkOf, paneOf, setLink, setParams } from "../workbench/layout";
import { change, useLayout, workbench } from "../workbench/store";
import { SessionLink } from "./Explorer";
import { profileLayout, showPane, showProfile, startLayout } from "./panes";
import { PICKED } from "./Picker";
import { profileLink, profileParams, readProfileLink } from "./profile/ProfilePage";
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

/** `/profile/$profile`: the profile's pane, as the address looks at it.
 * Back and forward move through the looks it had. */
export function ProfileRoute() {
  const { profile } = useParams({ from: "/profile/$profile" });
  const look = useSearch({ from: "/profile/$profile" });
  const name = useModel((model) => model.profiles.find((each) => each.id === profile)?.name ?? "");
  // After the workbench is arranged, before it paints.
  useLayoutEffect(() => {
    const id = paneOf(workbench.getState().layout, KIND.profile, profile);
    if (!id) {
      showProfile(profile, name, look);
      return;
    }
    change((layout) => {
      const now = layout.panes[id];
      const params = now ? profileParams(now) : { name, link: null };
      const named = setParams(layout, id, { name: name || params.name, link: params.link });
      const looked =
        params.link === null ? named : setLink(named, params.link, profileLink(profile, look));
      return activate(looked, id);
    });
  }, [profile, name, look]);
  // Once its pane closes, the address names a profile still shown, or none.
  const navigate = useNavigate();
  const open = useLayout((layout) => paneOf(layout, KIND.profile, profile) !== null);
  const was = useRef({ profile, open });
  useEffect(() => {
    const closed = was.current.profile === profile && was.current.open && !open;
    was.current = { profile, open };
    if (!closed) {
      return;
    }
    // The focused group's profile, else one shown in another group.
    const layout = workbench.getState().layout;
    const shown = groups(layout)
      .toSorted((a, b) => Number(b.id === layout.focused) - Number(a.id === layout.focused))
      .flatMap((group) => (group.active ? [layout.panes[group.active]] : []));
    const next = shown.find((pane) => pane?.kind === KIND.profile);
    if (next) {
      const link = linkOf(next);
      void navigate({
        to: "/profile/$profile",
        params: { profile: next.key },
        search: (link && readProfileLink(layout.links[link])?.look) ?? {},
        replace: true,
      });
    } else {
      void navigate({ to: "/", replace: true });
    }
  }, [profile, open, navigate]);
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
