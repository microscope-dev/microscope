// Connecting and signing in. A page served on its own is told which
// microscope to use by a join link; a page `microscope web` serves is
// connected to it already. Signing in to the services backend is optional:
// a join link works without an account.

import { type FormEvent, useEffect, useId, useState } from "react";
import { checkAccount, signIn, signOut, useAccount } from "../account";
import { isPagePath } from "../focus";
import { useModel } from "../store";
import { parseJoinLink, remote, setRemote, standalone } from "../target";

/** Why the microscope the page last connected to sent it back, if it did. */
function refused(): string | null {
  const last = remote();
  return last
    ? `The microscope at ${last.server} refused its link. It may have restarted with a new one.`
    : null;
}

export function ConnectBody() {
  const link = useModel((model) => model.link);
  if (!standalone && link === "unauthorized") {
    return (
      <div className="pane-body pane-text">
        <p>This browser has not joined this microscope session yet. Open a join link.</p>
        <p className="muted">
          <code>microscope web</code> prints one when it starts, and anyone in the session can make
          one with Share.
        </p>
      </div>
    );
  }
  if (!standalone) {
    return (
      <div className="pane-body pane-text">
        <p>
          This page came from <code>{window.location.origin}</code>, the <code>microscope web</code>{" "}
          it debugs with
          {link === "open" ? ", and is connected to it." : "."}
        </p>
      </div>
    );
  }
  const connected = remote();
  return (
    <div className="pane-body pane-text">
      {connected && link === "open" ? (
        <>
          <p>
            Connected to <code>{connected.server}</code>.
          </p>
          <div className="actions">
            <button
              type="button"
              className="button"
              onClick={() => {
                setRemote(null);
                window.location.assign("/");
              }}
            >
              Disconnect
            </button>
          </div>
          <h2>Connect to another</h2>
        </>
      ) : (
        link === "unauthorized" && connected && <p className="error">{refused()}</p>
      )}
      <ConnectForm />
    </div>
  );
}

function ConnectForm() {
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
    <form className="pane-form" onSubmit={connect}>
      <div className="field">
        <label htmlFor={id}>Join link</label>
        <input
          id={id}
          className="input"
          value={link}
          aria-invalid={error !== null}
          data-autofocus
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
    </form>
  );
}

export function AccountBody() {
  const now = useAccount();
  const [error, setError] = useState<string | null>(null);
  useEffect(() => {
    void checkAccount();
  }, []);

  return (
    <div className="pane-body pane-text">
      {now.kind === "checking" ? (
        <p className="muted">Checking whether you are signed in…</p>
      ) : now.kind === "unavailable" ? (
        <p className="muted">
          Sign-in is unavailable ({now.why}). You can still connect without an account.
        </p>
      ) : now.kind === "in" ? (
        <>
          <p>
            Signed in as <b>{now.user.username}</b> ({now.user.email}).
          </p>
          <p className="muted">Your programs, sessions, and profiles will show up here.</p>
          {error && <span className="error">{error}</span>}
          <div className="actions">
            <button
              type="button"
              className="button"
              onClick={() => {
                setError(null);
                signOut().catch((failure: Error) => setError(failure.message));
              }}
            >
              Sign out
            </button>
          </div>
        </>
      ) : (
        <SignIn />
      )}
    </div>
  );
}

function SignIn() {
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
      .catch((failure: Error) => setError(failure.message))
      .finally(() => setWorking(false));
  };
  return (
    <form className="pane-form" onSubmit={submit}>
      <p className="muted">Optional. A join link works without an account.</p>
      <div className="field">
        <label htmlFor={nameId}>Username or email</label>
        <input
          id={nameId}
          className="input"
          autoComplete="username"
          spellCheck={false}
          data-autofocus
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
