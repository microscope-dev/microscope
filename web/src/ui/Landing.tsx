// `/home`: where a signed-in user lands on a page served on its own. Little
// here yet: who they are, the microscope they use, and a way out.

import { Link, Navigate, useNavigate } from "@tanstack/react-router";
import { useEffect, useState } from "react";
import { currentUser, signOut, type User } from "../account";
import { base } from "../base";
import { remote, setRemote } from "../target";
import { Connect } from "./Start";

export function Landing() {
  const navigate = useNavigate();
  const [user, setUser] = useState<User | null | undefined>(undefined);
  const [error, setError] = useState<string | null>(null);
  useEffect(() => {
    currentUser()
      .then(setUser)
      .catch(() => setUser(null));
  }, []);

  if (user === undefined) {
    return <div className="page muted">Loading…</div>;
  }
  if (user === null) {
    return <Navigate to="/" replace />;
  }

  const connected = remote();
  return (
    <div className="landing">
      <header className="toolbar">
        <Link to="/home" className="brand" aria-label="microscope home">
          <img src={`${base}favicon.svg`} alt="" />
          microscope
        </Link>
        <span className="spacer" />
        <span className="muted">{user.username}</span>
        <button
          type="button"
          className="tb ghost"
          onClick={() => {
            signOut()
              .then(() => navigate({ to: "/" }))
              .catch((failure: Error) => setError(failure.message));
          }}
        >
          Sign out
        </button>
      </header>
      <main className="page" style={{ display: "grid", gap: 16, alignContent: "start" }}>
        <section className="card" style={{ width: "min(560px, 100%)" }}>
          <div className="card-head">
            <h1>Welcome, {user.username}</h1>
          </div>
          <div className="card-body">
            <p className="muted" style={{ margin: 0 }}>
              Your programs, sessions, and profiles will show up here.
            </p>
            {error && <span className="error">{error}</span>}
          </div>
        </section>
        {connected ? (
          <section className="card" style={{ width: "min(560px, 100%)" }}>
            <div className="card-head">
              <h1>Debugger</h1>
            </div>
            <div className="card-body">
              <p style={{ margin: 0 }}>
                Connected to <code>{connected.server}</code>.
              </p>
              <div className="actions">
                <a className="button primary" href="/">
                  Open
                </a>
                <button
                  type="button"
                  className="button"
                  onClick={() => {
                    setRemote(null);
                    window.location.reload();
                  }}
                >
                  Disconnect
                </button>
              </div>
            </div>
          </section>
        ) : (
          <Connect />
        )}
      </main>
    </div>
  );
}
