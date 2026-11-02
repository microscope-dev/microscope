// Connecting and signing in. A page served on its own is told which
// microscope to use by a join link; a page `microscope web` serves is
// connected to it already. Signing in to the services backend is optional:
// a join link works without an account.

import { type FormEvent, type ReactNode, useEffect, useId, useState } from "react";
import {
  account,
  checkAccount,
  enterCode,
  Refused,
  resendConfirmation,
  signIn,
  signOut,
  signUp,
  useAccount,
  useNotice,
} from "../account";
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

/** Where signing in or up has got to. */
type Step =
  | { kind: "sign-in" }
  | { kind: "sign-up" }
  | { kind: "code"; challenge: string }
  | { kind: "sent"; email: string };

export function AccountBody() {
  const now = useAccount();
  const notice = useNotice();
  const [error, setError] = useState<string | null>(null);
  const [step, setStep] = useState<Step>({ kind: "sign-in" });
  // What an emailed link did stops mattering once the person moves on.
  const go = (next: Step) => {
    account.setState({ notice: null });
    setStep(next);
  };
  useEffect(() => {
    void checkAccount();
  }, []);
  // Whoever signs in or out starts over next time.
  // biome-ignore lint/correctness/useExhaustiveDependencies: only the kind matters
  useEffect(() => setStep({ kind: "sign-in" }), [now.kind]);

  return (
    <div className="pane-body pane-text">
      {notice &&
        (notice.ok ? (
          <p role="status">{notice.text}</p>
        ) : (
          <div className="banner error" role="alert">
            {notice.text}
          </div>
        ))}
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
      ) : step.kind === "sign-up" ? (
        <SignUp go={go} />
      ) : step.kind === "sent" ? (
        <Sent email={step.email} go={go} />
      ) : step.kind === "code" ? (
        <Code challenge={step.challenge} go={go} />
      ) : (
        <SignIn go={go} />
      )}
    </div>
  );
}

type Go = (step: Step) => void;

/** A form's fields and its one request: whether it is in flight, and what
 * went wrong with it. */
function useSubmit(send: () => Promise<void>) {
  const [working, setWorking] = useState(false);
  const [error, setError] = useState<Refused | Error | null>(null);
  const submit = (event: FormEvent) => {
    event.preventDefault();
    setWorking(true);
    setError(null);
    send()
      .catch((failure: Error) => setError(failure))
      .finally(() => setWorking(false));
  };
  return { working, error, submit };
}

function Field(props: {
  label: string;
  value: string;
  set: (value: string) => void;
  note?: string;
  type?: "email" | "password";
  autoComplete: string;
  autoFocus?: boolean;
  code?: boolean;
}) {
  const id = useId();
  const noteId = useId();
  return (
    <div className="field">
      <label htmlFor={id}>{props.label}</label>
      <input
        id={id}
        className="input"
        type={props.type}
        autoComplete={props.autoComplete}
        spellCheck={false}
        data-autofocus={props.autoFocus || undefined}
        aria-describedby={props.note ? noteId : undefined}
        {...(props.code ? { inputMode: "numeric", maxLength: 6, pattern: "[0-9]{6}" } : {})}
        value={props.value}
        onChange={(event) => props.set(event.target.value)}
      />
      {props.note && (
        <span id={noteId} className="note">
          {props.note}
        </span>
      )}
    </div>
  );
}

function SignIn({ go }: { go: Go }) {
  const [name, setName] = useState("");
  const [password, setPassword] = useState("");
  const { working, error, submit } = useSubmit(async () => {
    const signed = await signIn(name, password);
    if ("challenge" in signed) {
      go({ kind: "code", challenge: signed.challenge });
    }
  });
  // Services says so only once the password is right.
  const unconfirmed = error instanceof Refused && error.status === 403;
  return (
    <>
      <form className="pane-form" onSubmit={submit}>
        <p className="muted">Optional. A join link works without an account.</p>
        <Field
          label="Username or email"
          value={name}
          set={setName}
          autoComplete="username"
          autoFocus
        />
        <Field
          label="Password"
          type="password"
          value={password}
          set={setPassword}
          autoComplete="current-password"
        />
        {error && <span className="error">{error.message}</span>}
        <div className="actions">
          <button
            type="submit"
            className="button primary"
            disabled={working || !name.trim() || !password}
          >
            Sign in
          </button>
          <span className="muted">
            No account?
            <button type="button" className="link-button" onClick={() => go({ kind: "sign-up" })}>
              Create one
            </button>
          </span>
        </div>
      </form>
      {unconfirmed && (
        <Resend email={name.includes("@") ? name : ""} go={go}>
          <h2>Confirm your email address</h2>
          <p className="muted">
            Follow the link in the email sent when you signed up, or get a new one.
          </p>
        </Resend>
      )}
    </>
  );
}

function SignUp({ go }: { go: Go }) {
  const [username, setUsername] = useState("");
  const [email, setEmail] = useState("");
  const [password, setPassword] = useState("");
  const { working, error, submit } = useSubmit(async () => {
    if (await signUp(username, email, password)) {
      go({ kind: "sent", email });
    }
  });
  return (
    <form className="pane-form" onSubmit={submit}>
      <h2>Create an account</h2>
      <Field
        label="Username"
        value={username}
        set={setUsername}
        autoComplete="username"
        note="3 to 32 letters, digits, _ or -."
        autoFocus
      />
      <Field label="Email" type="email" value={email} set={setEmail} autoComplete="email" />
      <Field
        label="Password"
        type="password"
        value={password}
        set={setPassword}
        autoComplete="new-password"
        note="At least 8 characters."
      />
      {error && <span className="error">{error.message}</span>}
      <div className="actions">
        <button
          type="submit"
          className="button primary"
          disabled={working || !username.trim() || !email.trim() || !password}
        >
          Create account
        </button>
        <span className="muted">
          Have one?
          <button type="button" className="link-button" onClick={() => go({ kind: "sign-in" })}>
            Sign in
          </button>
        </span>
      </div>
    </form>
  );
}

/** After signing up: the account waits for its email to be confirmed. */
function Sent({ email, go }: { email: string; go: Go }) {
  return (
    <Resend email={email} go={go} fixed>
      <h2>Check your email</h2>
      <p>
        A confirmation link is on its way to <b>{email}</b>. Follow it, then sign in here.
      </p>
    </Resend>
  );
}

/** Asks for another confirmation link, to an address given or to type. */
function Resend(props: { email: string; go: Go; fixed?: boolean; children: ReactNode }) {
  const [email, setEmail] = useState(props.email);
  const [sent, setSent] = useState(false);
  const { working, error, submit } = useSubmit(async () => {
    setSent(false);
    await resendConfirmation(email);
    setSent(true);
  });
  return (
    <form className="pane-form" onSubmit={submit}>
      {props.children}
      {!props.fixed && (
        <Field label="Email" type="email" value={email} set={setEmail} autoComplete="email" />
      )}
      {sent && (
        <p role="status" className="muted">
          If {email} still needs confirming, a new link is on its way.
        </p>
      )}
      {error && <span className="error">{error.message}</span>}
      <div className="actions">
        <button type="submit" className="button" disabled={working || !email.trim()}>
          Send another link
        </button>
        {props.fixed && (
          <button
            type="button"
            className="button primary"
            onClick={() => props.go({ kind: "sign-in" })}
          >
            Sign in
          </button>
        )}
      </div>
    </form>
  );
}

/** After a right password, when services emails a code to finish with. */
function Code({ challenge, go }: { challenge: string; go: Go }) {
  const [code, setCode] = useState("");
  const { working, error, submit } = useSubmit(async () => {
    await enterCode(challenge, code.trim());
  });
  return (
    <form className="pane-form" onSubmit={submit}>
      <h2>Enter the code from your email</h2>
      <p className="muted">The email also has a link, which signs in whichever device opens it.</p>
      <Field label="Code" value={code} set={setCode} autoComplete="one-time-code" autoFocus code />
      {error && <span className="error">{error.message}</span>}
      <div className="actions">
        <button
          type="submit"
          className="button primary"
          disabled={working || !/^\d{6}$/.test(code.trim())}
        >
          Sign in
        </button>
        <button type="button" className="link-button" onClick={() => go({ kind: "sign-in" })}>
          Start over
        </button>
      </div>
    </form>
  );
}
