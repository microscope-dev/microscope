// What each address opens in the one workbench. An address names a place: a
// session's focus, a profile, or nowhere in particular, which starts from
// Welcome. Old addresses of pages that became panes open those panes.

import { Link, Navigate, Outlet, useNavigate, useParams, useSearch } from "@tanstack/react-router";
import { Fragment, useEffect, useLayoutEffect, useRef } from "react";
import { followEmailLink } from "../account";
import { KIND } from "../documents";
import { type Note, targetName } from "../model";
import { useModel } from "../store";
import { activate, type Json, paneOf, setParams } from "../workbench/layout";
import { change, workbench } from "../workbench/store";
import { PICKED } from "./Picker";
import { profileLayout, showPane, showProfile, welcomeLayout } from "./panes";
import { profileParams } from "./profile/ProfilePage";
import { Workbench } from "./Workspace";

/** `/`: the session when there is one; else what to start from, which is
 * the profile a server started on one serves. */
export function Home() {
  // Until the server says what it has, unless it won't.
  const loading = useModel(
    (model) =>
      model.state === null && model.link !== "unauthorized" && model.link !== "incompatible",
  );
  const session = useModel((model) => model.state?.session ?? null);
  const busy = useModel((model) => model.state?.busy ?? null);
  const profile = useModel((model) => (busy ? undefined : model.profiles.at(-1)));
  if (session) {
    return <Navigate to="/s/$session" params={{ session }} replace />;
  }
  if (loading) {
    return <div className="page muted">Connecting to microscope…</div>;
  }
  return (
    <Workbench
      focus={null}
      wants="start"
      start={profile ? () => profileLayout(profile.id, profile.name, {}) : welcomeLayout}
      notice={busy && <div className="workbench-notice muted">{busy}…</div>}
    />
  );
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

/** `/confirm-email#token=` and `/login-link#token=`, where the links in
 * services' emails lead: the Account pane, saying how following it went. */
export function EmailLink({ link }: { link: "confirm" | "login" }) {
  const navigate = useNavigate();
  useLayoutEffect(() => {
    followEmailLink(link);
    showPane(KIND.account);
    void navigate({ to: "/", replace: true });
  }, [link, navigate]);
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
      const params = now ? profileParams(now) : { name, look };
      const next = { name: name || params.name, look } as unknown as Json;
      return activate(setParams(layout, id, next), id);
    });
  }, [profile, name, look]);
  return <Workbench focus={null} wants="start" start={() => profileLayout(profile, name, look)} />;
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
  // opened on a link to an ended session says so instead.
  const followed = useRef(false);
  // The program this tab saw the session debug, which names it in notices.
  const program = useRef<string | null>(null);
  const current = state?.session ?? null;
  if (current === session) {
    followed.current = true;
    program.current = targetName(state);
  }

  useEffect(() => {
    if (followed.current && current && current !== session) {
      void navigate({ to: "/s/$session", params: { session: current }, replace: true });
    }
  }, [current, session, navigate]);

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
      <Workbench
        focus={null}
        wants="session"
        notice={<div className="workbench-notice muted">{state.busy}…</div>}
      />
    );
  }
  const ended = endedNotice(notices, program.current);
  return (
    <Workbench
      focus={null}
      wants="session"
      notice={
        <div className="workbench-notice" role="status" data-testid="session-ended">
          {/* Only a tab that saw the session knows it ran here at all. */}
          <b>{followed.current ? "This session ended." : "This session is not here."}</b>{" "}
          {followed.current ? "" : "It ended, or the link is wrong. "}
          {ended ? `${ended.name} ${ended.text}. ` : ""}
          {current ? (
            <Link to="/s/$session" params={{ session: current }}>
              Go to {targetName(state)}
            </Link>
          ) : (
            "Launch, attach, or open a core dump from the File menu."
          )}
        </div>
      }
    />
  );
}
