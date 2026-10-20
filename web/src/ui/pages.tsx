import { Link, Navigate, Outlet, useNavigate, useParams, useSearch } from "@tanstack/react-router";
import { Fragment, useEffect, useRef, useState } from "react";
import { tradeOnce } from "../connection";
import { isPagePath } from "../focus";
import { type Note, targetName } from "../model";
import { useConnection, useModel } from "../store";

/** `/`: wherever the session is. */
export function Home() {
  const state = useModel((model) => model.state);
  const profile = useModel((model) => model.profiles.at(-1)?.id);
  if (!state) {
    return <div className="page muted">Connecting to microscope…</div>;
  }
  if (state.session) {
    return <Navigate to="/s/$session" params={{ session: state.session }} replace />;
  }
  // A server started on a profile, and debugging nothing, shows it.
  if (profile && !state.busy) {
    return <Navigate to="/profile/$profile" params={{ profile }} replace />;
  }
  // The program microscope started with is loading; its session comes next.
  if (state.busy) {
    return <div className="page muted">{state.busy}…</div>;
  }
  return <Navigate to="/pick" replace />;
}

// The token leaves the address bar before anything else can copy it, so it
// is kept here for a second render, or another try if the server was away.
let token: string | null = null;
const trade = tradeOnce();

function join(): Promise<boolean> {
  if (token === null) {
    token = window.location.hash.slice(1);
    window.history.replaceState(null, "", window.location.pathname + window.location.search);
  }
  return trade(token);
}

/** `/join#TOKEN`: trades the token for a cookie, then opens the link. */
export function Join() {
  const connection = useConnection();
  const navigate = useNavigate();
  const { to } = useSearch({ from: "/join" });
  const [failed, setFailed] = useState<"refused" | "unreachable" | null>(null);
  const [tries, setTries] = useState(0);

  // biome-ignore lint/correctness/useExhaustiveDependencies: each try trades again
  useEffect(() => {
    join()
      .then((accepted) => {
        if (!accepted) {
          setFailed("refused");
          return;
        }
        connection.start();
        // `href`, since the link holds a query of its own.
        void navigate({ href: isPagePath(to) ? to : "/", replace: true });
      })
      .catch(() => setFailed("unreachable"));
  }, [connection, navigate, to, tries]);

  if (failed === "refused") {
    return <NeedsLink reason="This link is not for this microscope, or it has expired." />;
  }
  if (failed === "unreachable") {
    return (
      <div className="page">
        <div className="card" style={{ width: "min(560px, 100%)" }}>
          <div className="card-head">
            <h1>Could not join</h1>
          </div>
          <div className="card-body">
            <p style={{ margin: 0 }}>
              Could not reach microscope to use this link. Check that it is running, then try again.
            </p>
            <div className="actions">
              <button
                type="button"
                className="button primary"
                onClick={() => {
                  setFailed(null);
                  setTries((tries) => tries + 1);
                }}
              >
                Try again
              </button>
            </div>
          </div>
        </div>
      </div>
    );
  }
  return <div className="page muted">Joining…</div>;
}

export function NeedsLink({ reason }: { reason: string }) {
  return (
    <div className="page">
      <div className="card" style={{ width: "min(560px, 100%)" }}>
        <div className="card-head">
          <h1>Open a join link</h1>
        </div>
        <div className="card-body">
          <p style={{ margin: 0 }}>{reason}</p>
          <p className="muted" style={{ margin: 0 }}>
            <code>microscope web</code> prints a link when it starts, and anyone in the session can
            make one with Share.
          </p>
        </div>
      </div>
    </div>
  );
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

/** `/s/$session`: the session, or why it is not here. */
export function SessionPage() {
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

  if (!state) {
    return <div className="page muted">Connecting to microscope…</div>;
  }
  if (current === session) {
    // Nothing a pane holds outlives its session.
    return (
      <Fragment key={session}>
        <Outlet />
      </Fragment>
    );
  }
  if (state.busy) {
    return <div className="page muted">{state.busy}…</div>;
  }
  const ended = endedNotice(notices, program.current);
  return (
    <div className="page">
      <div className="card" style={{ width: "min(560px, 100%)" }} data-testid="session-ended">
        <div className="card-head">
          <h1>This session ended</h1>
        </div>
        <div className="card-body">
          <p style={{ margin: 0 }}>
            {ended ? `${ended.name} ${ended.text}. ` : ""}
            {current
              ? `microscope is now debugging ${targetName(state) ?? "something else"}.`
              : "microscope is not debugging anything now."}
          </p>
          <div className="actions">
            {current ? (
              <Link className="button primary" to="/s/$session" params={{ session: current }}>
                Go to {targetName(state)}
              </Link>
            ) : (
              <Link className="button primary" to="/pick">
                Debug something
              </Link>
            )}
          </div>
        </div>
      </div>
    </div>
  );
}
