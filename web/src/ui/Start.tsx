// What a page served on its own shows to someone not signed in: a join link
// to paste, and an optional sign-in to the services backend. Someone signed
// in goes on to their landing page (Landing.tsx).

import { Navigate, useNavigate } from "@tanstack/react-router";
import { type FormEvent, useEffect, useId, useState } from "react";
import { currentUser, signIn, type User } from "../account";
import { isPagePath } from "../focus";
import { parseJoinLink, remote, setRemote } from "../target";

export function Start() {
  return (
    <div className="page" style={{ display: "grid", gap: 16, alignContent: "start" }}>
      <Connect />
      <Account />
    </div>
  );
}

/** Why the microscope the page last connected to sent it back here, if it did. */
function refused(): string | null {
  const last = remote();
  return last
    ? `The microscope at ${last.server} refused its link. It may have restarted with a new one.`
    : null;
}

export function Connect() {
  const reason = refused();
  const id = useId();
  const [link, setLink] = useState("");
  const [error, setError] = useState<string | null>(null);
  const connect = (event: FormEvent) => {
    event.preventDefault();
    const joined = parseJoinLink(link);
    if (!joined) {
      setError("That is not a join link. One looks like http://127.0.0.1:7341/join#c-…");
      return;
    }
    setRemote({ server: joined.server, token: joined.token });
    // A fresh page, so nothing from another server lingers.
    window.location.assign(isPagePath(joined.to) ? joined.to : "/");
  };
  return (
    <form className="card" style={{ width: "min(560px, 100%)" }} onSubmit={connect}>
      <div className="card-head">
        <h1>Connect to microscope</h1>
      </div>
      <div className="card-body">
        {reason && <p className="error">{reason}</p>}
        <div className="field">
          <label htmlFor={id}>Join link</label>
          <input
            id={id}
            className="input"
            value={link}
            aria-invalid={error !== null}
            onChange={(event) => {
              setLink(event.target.value);
              setError(null);
            }}
            placeholder="http://127.0.0.1:7341/join#c-…"
            spellCheck={false}
          />
          <span className="note">
            <code>microscope web</code> prints one when it starts, and Share makes more.
          </span>
        </div>
        {error && <span className="error">{error}</span>}
        <div className="actions">
          <button type="submit" className="button primary" disabled={!link.trim()}>
            Connect
          </button>
        </div>
      </div>
    </form>
  );
}

type AccountState =
  | { kind: "checking" }
  | { kind: "unavailable"; why: string }
  | { kind: "out" }
  | { kind: "in" };

function Account() {
  const navigate = useNavigate();
  const [state, setState] = useState<AccountState>({ kind: "checking" });
  useEffect(() => {
    currentUser()
      .then((user) => setState({ kind: user ? "in" : "out" }))
      .catch((error: Error) => setState({ kind: "unavailable", why: error.message }));
  }, []);

  if (state.kind === "in") {
    return <Navigate to="/home" replace />;
  }
  return (
    <div className="card" style={{ width: "min(560px, 100%)" }}>
      <div className="card-head">
        <h1>Account</h1>
      </div>
      {state.kind === "checking" ? (
        <div className="card-body muted">Checking whether you are signed in…</div>
      ) : state.kind === "unavailable" ? (
        <div className="card-body muted">
          Sign-in is unavailable ({state.why}). You can still connect without an account.
        </div>
      ) : (
        <SignIn onSignedIn={() => void navigate({ to: "/home" })} />
      )}
    </div>
  );
}

function SignIn({ onSignedIn }: { onSignedIn(user: User): void }) {
  const nameId = useId();
  const passwordId = useId();
  const [name, setName] = useState("");
  const [password, setPassword] = useState("");
  const [working, setWorking] = useState(false);
  const [error, setError] = useState<string | null>(null);
  const submit = (event: FormEvent) => {
    event.preventDefault();
    setWorking(true);
    setError(null);
    signIn(name, password)
      .then(onSignedIn)
      .catch((failure: Error) => setError(failure.message))
      .finally(() => setWorking(false));
  };
  return (
    <form className="card-body" onSubmit={submit}>
      <p className="muted" style={{ margin: 0 }}>
        Optional. A join link works without an account.
      </p>
      <div className="two">
        <div className="field">
          <label htmlFor={nameId}>Username or email</label>
          <input
            id={nameId}
            className="input"
            autoComplete="username"
            spellCheck={false}
            value={name}
            onChange={(event) => setName(event.target.value)}
          />
        </div>
        <div className="field">
          <label htmlFor={passwordId}>Password</label>
          <input
            id={passwordId}
            className="input"
            type="password"
            autoComplete="current-password"
            value={password}
            onChange={(event) => setPassword(event.target.value)}
          />
        </div>
      </div>
      {error && <span className="error">{error}</span>}
      <div className="actions">
        <button
          type="submit"
          className="button primary"
          disabled={working || !name.trim() || !password}
        >
          Sign in
        </button>
      </div>
    </form>
  );
}
