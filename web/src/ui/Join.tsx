// `/join#TOKEN`, the address a join link opens: it trades the token for a
// cookie, then opens the page the link names.

import { useNavigate, useSearch } from "@tanstack/react-router";
import { useCallback, useEffect, useState } from "react";

import { tradeOnce } from "../connection";
import { isPagePath } from "../focus";
import { useConnection } from "../store";

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

const CARD = { width: "min(560px, 100%)" };
const NO_MARGIN = { margin: 0 };

/** `/join#TOKEN`: trades the token for a cookie, then opens the link. */
export function Join() {
  const connection = useConnection();
  const navigate = useNavigate();
  const { to } = useSearch({ from: "/join" });
  const [failed, setFailed] = useState<"refused" | "unreachable" | null>(null);
  const [tries, setTries] = useState(0);

  useEffect(() => {
    const attempt = async () => {
      if (!(await join())) {
        setFailed("refused");
        return;
      }
      connection.start();
      // `href`, since the link holds a query of its own.
      void navigate({ href: isPagePath(to) ? to : "/", replace: true });
    };
    attempt().catch(() => setFailed("unreachable"));
    // oxlint-disable-next-line react/exhaustive-effect-dependencies -- each try trades again
  }, [connection, navigate, to, tries]);
  const retry = useCallback(() => {
    setFailed(null);
    setTries((count) => count + 1);
  }, []);

  if (failed) {
    return (
      <div className="page">
        <div className="card" style={CARD}>
          <div className="card-head">
            <h1>{failed === "refused" ? "Open a join link" : "Could not join"}</h1>
          </div>
          <div className="card-body">
            <p style={NO_MARGIN}>
              {failed === "refused"
                ? "This link is not for this microscope, or it has expired. microscope web prints one when it starts, and Share makes more."
                : "Could not reach microscope to use this link. Check that it is running, then try again."}
            </p>
            {failed === "unreachable" && (
              <div className="actions">
                <button type="button" className="button primary" onClick={retry}>
                  Try again
                </button>
              </div>
            )}
          </div>
        </div>
      </div>
    );
  }
  return <div className="page muted">Joining…</div>;
}
