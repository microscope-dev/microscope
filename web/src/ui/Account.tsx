// Connecting and signing in. A page served on its own is told which
// microscope to use by a join link; a page `microscope web` serves is
// connected to it already. Signing in to the services backend is optional:
// a join link works without an account.

import { type FormEvent, type ReactNode, useEffect, useId, useRef, useState } from "react";
import { useStore } from "zustand";
import {
  account,
  checkAccount,
  confirmTotpSetup,
  disableTotp,
  enterCode,
  enterRecoveryCode,
  enterTotp,
  Refused,
  replaceRecoveryCodes,
  requestPasswordReset,
  resendConfirmation,
  resetPassword,
  signIn,
  signOut,
  signOutEverywhere,
  signUp,
  startTotpSetup,
  totpConfirmed,
  type User,
  useAccount,
  useNotice,
} from "../account";
import { useFeatures } from "../features";
import { isPagePath } from "../focus";
import { autofillAvailable, signInWithPasskey, supported } from "../passkeys";
import { useModel } from "../store";
import { parseJoinLink, remote, setRemote, standalone } from "../target";
import { qrcodegen } from "../vendor/qrcodegen";
import { Organizations } from "./Orgs";
import { ContinueWith, OrgLink, SignInMethods } from "./SingleSignOn";

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
  | { kind: "totp"; challenge: string }
  | { kind: "recovery"; challenge: string }
  | { kind: "sent"; email: string }
  | { kind: "forgot"; email: string };

export function AccountBody() {
  const now = useAccount();
  const notice = useNotice();
  const [error, setError] = useState<string | null>(null);
  const [everywhere, setEverywhere] = useState(false);
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
  // Single sign-on came back for a code from the account's authenticator app.
  const challenge = useStore(account, (state) => state.challenge);
  useEffect(() => {
    if (challenge !== null && now.kind === "out") {
      account.setState({ challenge: null });
      setStep({ kind: "totp", challenge });
    }
  }, [challenge, now.kind]);
  // A password reset link asks for the new password first, signed in or not.
  const reset = useStore(account, (state) => state.reset);

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
      <OrgLink />
      {reset !== null ? (
        <NewPassword token={reset} />
      ) : now.kind === "checking" ? (
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
            {everywhere ? (
              <span className="confirm">
                <span>Sign out on every device?</span>
                <button
                  type="button"
                  className="button danger"
                  onClick={() => {
                    setError(null);
                    signOutEverywhere()
                      .catch((failure: Error) => setError(failure.message))
                      .finally(() => setEverywhere(false));
                  }}
                >
                  Sign out everywhere
                </button>
                <button type="button" className="link-button" onClick={() => setEverywhere(false)}>
                  Cancel
                </button>
              </span>
            ) : (
              <>
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
                <button type="button" className="button" onClick={() => setEverywhere(true)}>
                  Sign out everywhere
                </button>
              </>
            )}
          </div>
          <Organizations />
          <SignInMethods />
          <TwoFactor user={now.user} />
        </>
      ) : step.kind === "sign-up" ? (
        <SignUp go={go} />
      ) : step.kind === "sent" ? (
        <Sent email={step.email} go={go} />
      ) : step.kind === "code" ? (
        <Code challenge={step.challenge} go={go} />
      ) : step.kind === "totp" ? (
        <Totp challenge={step.challenge} go={go} />
      ) : step.kind === "recovery" ? (
        <Recovery challenge={step.challenge} go={go} />
      ) : step.kind === "forgot" ? (
        <Forgot email={step.email} go={go} />
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

/** Signing in with a passkey: the button's, and the username field's
 * autofill while the form is open, which the button stops first. */
function usePasskeySignIn(on: boolean) {
  const [working, setWorking] = useState(false);
  const [error, setError] = useState<string | null>(null);
  const autofill = useRef<AbortController | null>(null);

  useEffect(() => {
    if (!on) {
      return;
    }
    const controller = new AbortController();
    autofill.current = controller;
    autofillAvailable()
      .then((available) =>
        available ? signInWithPasskey({ autofill: true, signal: controller.signal }) : null,
      )
      .catch((failure: Error) => {
        if (!controller.signal.aborted) {
          setError(failure.message);
        }
      });
    return () => controller.abort();
  }, [on]);

  const start = () => {
    autofill.current?.abort();
    setWorking(true);
    setError(null);
    signInWithPasskey()
      .catch((failure: Error) => setError(failure.message))
      .finally(() => setWorking(false));
  };
  return { working, error, start };
}

function SignIn({ go }: { go: Go }) {
  const offered = useFeatures();
  const passkeys = offered?.passkeys === true && supported();
  const passkey = usePasskeySignIn(passkeys);
  const [name, setName] = useState("");
  const [password, setPassword] = useState("");
  const { working, error, submit } = useSubmit(async () => {
    const signed = await signIn(name, password);
    if ("challenge" in signed) {
      go({ kind: signed.method === "totp" ? "totp" : "code", challenge: signed.challenge });
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
          autoComplete={passkeys ? "username webauthn" : "username"}
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
          {offered?.email && (
            <button
              type="button"
              className="link-button"
              onClick={() => go({ kind: "forgot", email: name.includes("@") ? name.trim() : "" })}
            >
              Forgot password?
            </button>
          )}
          <span className="muted">
            No account?
            <button type="button" className="link-button" onClick={() => go({ kind: "sign-up" })}>
              Create one
            </button>
          </span>
        </div>
      </form>
      <ContinueWith>
        {passkeys && (
          <button
            type="button"
            className="button"
            disabled={passkey.working}
            onClick={passkey.start}
          >
            Sign in with a passkey
          </button>
        )}
      </ContinueWith>
      {passkey.error && (
        <span className="error" role="alert">
          {passkey.error}
        </span>
      )}
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
  const offered = useFeatures();
  const [username, setUsername] = useState("");
  const [email, setEmail] = useState("");
  const [password, setPassword] = useState("");
  const { working, error, submit } = useSubmit(async () => {
    // The button waits for the features, so they are here.
    const confirming = offered?.email === true;
    await signUp(username, email, password, confirming);
    if (confirming) {
      go({ kind: "sent", email });
    }
  });
  return (
    <>
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
            disabled={working || !offered || !username.trim() || !email.trim() || !password}
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
      <ContinueWith />
    </>
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

/** Asks for a password reset link. */
function Forgot(props: { email: string; go: Go }) {
  const [email, setEmail] = useState(props.email);
  const [sent, setSent] = useState<string | null>(null);
  const { working, error, submit } = useSubmit(async () => {
    setSent(null);
    await requestPasswordReset(email.trim());
    setSent(email.trim());
  });
  return (
    <form className="pane-form" onSubmit={submit}>
      <h2>Reset your password</h2>
      <p className="muted">We will email you a link to choose a new one.</p>
      <Field
        label="Email"
        type="email"
        value={email}
        set={setEmail}
        autoComplete="email"
        autoFocus
      />
      {sent && (
        <p role="status" className="muted">
          If an account uses {sent}, a reset link is on its way.
        </p>
      )}
      {error && <span className="error">{error.message}</span>}
      <div className="actions">
        <button type="submit" className="button primary" disabled={working || !email.trim()}>
          {sent ? "Send another link" : "Send reset link"}
        </button>
        <button type="button" className="link-button" onClick={() => props.go({ kind: "sign-in" })}>
          Back to sign in
        </button>
      </div>
    </form>
  );
}

/** After following a password reset link. */
function NewPassword({ token }: { token: string }) {
  const [password, setPassword] = useState("");
  const { working, error, submit } = useSubmit(() => resetPassword(token, password));
  return (
    <form className="pane-form" onSubmit={submit}>
      <h2>Choose a new password</h2>
      <p className="muted">Changing it signs your account out on every device.</p>
      <Field
        label="New password"
        type="password"
        value={password}
        set={setPassword}
        autoComplete="new-password"
        note="At least 8 characters."
        autoFocus
      />
      {error && <span className="error">{error.message}</span>}
      <div className="actions">
        <button type="submit" className="button primary" disabled={working || !password}>
          Change password
        </button>
        <button type="button" className="button" onClick={() => account.setState({ reset: null })}>
          Cancel
        </button>
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

/** After a right password, for an account with two-factor authentication. */
function Totp({ challenge, go }: { challenge: string; go: Go }) {
  const [code, setCode] = useState("");
  const { working, error, submit } = useSubmit(async () => {
    await enterTotp(challenge, code.trim());
  });
  const recover = () => go({ kind: "recovery", challenge });
  // Too many wrong codes today: only a recovery code gets in now.
  const locked = error instanceof Refused && error.status === 429;
  return (
    <form className="pane-form" onSubmit={submit}>
      <h2>Enter the code from your authenticator app</h2>
      <Field label="Code" value={code} set={setCode} autoComplete="one-time-code" autoFocus code />
      {error && <span className="error">{error.message}</span>}
      <div className="actions">
        <button
          type="submit"
          className={locked ? "button" : "button primary"}
          disabled={working || !/^\d{6}$/.test(code.trim())}
        >
          Sign in
        </button>
        {locked && (
          <button type="button" className="button primary" onClick={recover}>
            Use a recovery code
          </button>
        )}
        <button type="button" className="link-button" onClick={() => go({ kind: "sign-in" })}>
          Start over
        </button>
      </div>
      {!locked && (
        <p className="muted">
          Lost your device?
          <button type="button" className="link-button" onClick={recover}>
            Use a recovery code
          </button>
        </p>
      )}
    </form>
  );
}

function Recovery({ challenge, go }: { challenge: string; go: Go }) {
  const [code, setCode] = useState("");
  const { working, error, submit } = useSubmit(async () => {
    await enterRecoveryCode(challenge, code.trim());
  });
  return (
    <form className="pane-form" onSubmit={submit}>
      <h2>Enter a recovery code</h2>
      <Field
        label="Recovery code"
        value={code}
        set={setCode}
        autoComplete="off"
        note="Each code works once. We email you when one is used."
        autoFocus
      />
      {error && <span className="error">{error.message}</span>}
      <div className="actions">
        <button type="submit" className="button primary" disabled={working || !code.trim()}>
          Sign in
        </button>
        <button
          type="button"
          className="link-button"
          onClick={() => go({ kind: "totp", challenge })}
        >
          Use the app instead
        </button>
      </div>
    </form>
  );
}

/** Where turning two-factor authentication on or off has got to. */
type Setup =
  | { kind: "idle" }
  | { kind: "password" }
  | { kind: "scan"; secret: string; uri: string }
  | { kind: "codes"; codes: string[]; enabling: boolean }
  | { kind: "reauth"; action: "disable" | "codes" };

/** Two-factor authentication for the signed-in account: off, being set up,
 * or on, with the forms that change it. */
function TwoFactor({ user }: { user: User }) {
  const offered = useFeatures();
  const [setup, setSetup] = useState<Setup>({ kind: "idle" });
  const idle = () => setSetup({ kind: "idle" });
  // A server without TOTP has nothing to offer here, unless the account
  // turned it on before, which it can still turn off with a recovery code.
  if (!offered || (!offered.totp && !user.totp_enabled)) {
    return null;
  }
  return (
    <>
      <h2>Two-factor authentication</h2>
      {setup.kind === "password" ? (
        <SetupPassword next={setSetup} cancel={idle} />
      ) : setup.kind === "scan" ? (
        <Scan secret={setup.secret} uri={setup.uri} next={setSetup} cancel={idle} />
      ) : setup.kind === "codes" ? (
        <RecoveryCodes
          codes={setup.codes}
          done={() => {
            idle();
            if (setup.enabling) {
              void totpConfirmed();
            }
          }}
        />
      ) : setup.kind === "reauth" ? (
        <Reauthenticate action={setup.action} next={setSetup} cancel={idle} />
      ) : user.totp_enabled ? (
        <>
          <p className="muted">On. Signing in takes your password and a code from your app.</p>
          <div className="actions">
            <button
              type="button"
              className="button"
              onClick={() => setSetup({ kind: "reauth", action: "codes" })}
            >
              New recovery codes
            </button>
            <button
              type="button"
              className="button"
              onClick={() => setSetup({ kind: "reauth", action: "disable" })}
            >
              Turn off
            </button>
          </div>
        </>
      ) : (
        <>
          <p className="muted">
            Off. Turn it on, and signing in will take a code from an authenticator app as well as
            your password.
          </p>
          <div className="actions">
            <button type="button" className="button" onClick={() => setSetup({ kind: "password" })}>
              Set up
            </button>
          </div>
        </>
      )}
    </>
  );
}

type Next = (setup: Setup) => void;

function SetupPassword({ next, cancel }: { next: Next; cancel: () => void }) {
  const [password, setPassword] = useState("");
  const { working, error, submit } = useSubmit(async () => {
    const { secret, uri } = await startTotpSetup(password);
    next({ kind: "scan", secret, uri });
  });
  return (
    <form className="pane-form" onSubmit={submit}>
      <Field
        label="Password"
        type="password"
        value={password}
        set={setPassword}
        autoComplete="current-password"
        note="Enter your password to start."
        autoFocus
      />
      {error && <span className="error">{error.message}</span>}
      <div className="actions">
        <button type="submit" className="button primary" disabled={working || !password}>
          Continue
        </button>
        <button type="button" className="link-button" onClick={cancel}>
          Cancel
        </button>
      </div>
    </form>
  );
}

function Scan(props: { secret: string; uri: string; next: Next; cancel: () => void }) {
  const [code, setCode] = useState("");
  const { working, error, submit } = useSubmit(async () => {
    const codes = await confirmTotpSetup(code.trim());
    props.next({ kind: "codes", codes, enabling: true });
  });
  return (
    <form className="pane-form" onSubmit={submit}>
      <div className="totp-scan">
        <Qr text={props.uri} />
        <div className="totp-key">
          <span>
            Scan this with an authenticator app, such as 1Password or Google Authenticator.
          </span>
          <span className="muted">Can't scan it? Enter this key:</span>
          <code data-testid="totp-secret">{props.secret.match(/.{1,4}/g)?.join(" ")}</code>
          <div className="actions">
            <Copy text={props.secret} label="Copy key" />
          </div>
        </div>
      </div>
      <Field
        label="Code from the app"
        value={code}
        set={setCode}
        autoComplete="one-time-code"
        note="Turning it on signs you out everywhere else."
        autoFocus
        code
      />
      {error && <span className="error">{error.message}</span>}
      <div className="actions">
        <button
          type="submit"
          className="button primary"
          disabled={working || !/^\d{6}$/.test(code.trim())}
        >
          Turn on
        </button>
        <button type="button" className="link-button" onClick={props.cancel}>
          Cancel
        </button>
      </div>
    </form>
  );
}

/** The otpauth:// address as a QR code, drawn here so the secret never
 * leaves the page. Black on white in either theme, which every camera
 * reads. */
function Qr({ text }: { text: string }) {
  const qr = qrcodegen.QrCode.encodeText(text, qrcodegen.QrCode.Ecc.MEDIUM);
  const quiet = 4;
  let path = "";
  for (let y = 0; y < qr.size; y++) {
    for (let x = 0; x < qr.size; x++) {
      if (qr.getModule(x, y)) {
        path += `M${x + quiet} ${y + quiet}h1v1h-1z`;
      }
    }
  }
  const side = qr.size + quiet * 2;
  return (
    <svg
      className="totp-qr"
      viewBox={`0 0 ${side} ${side}`}
      role="img"
      aria-label="QR code for your authenticator app"
      shapeRendering="crispEdges"
    >
      <rect width={side} height={side} fill="#fff" />
      <path d={path} fill="#000" />
    </svg>
  );
}

/** A button that copies `text`, where the browser allows it: only pages
 * served over HTTPS or from localhost can write to the clipboard. */
function Copy({ text, label }: { text: string; label: string }) {
  const [copied, setCopied] = useState(false);
  if (!navigator.clipboard) {
    return null;
  }
  return (
    <button
      type="button"
      className="button"
      onClick={() => {
        void navigator.clipboard.writeText(text).then(() => setCopied(true));
      }}
    >
      {copied ? "Copied" : label}
    </button>
  );
}

function RecoveryCodes({ codes, done }: { codes: string[]; done: () => void }) {
  const [saved, setSaved] = useState(false);
  const text = `Microscope recovery codes. Each signs you in once.\n\n${codes.join("\n")}\n`;
  const download = () => {
    const url = URL.createObjectURL(new Blob([text], { type: "text/plain" }));
    const link = document.createElement("a");
    link.href = url;
    link.download = "microscope-recovery-codes.txt";
    link.click();
    URL.revokeObjectURL(url);
  };
  return (
    <div className="pane-form">
      <p>
        <b>Save your recovery codes.</b> If you lose your authenticator, each of these signs you in
        once. This is the only time they are shown.
      </p>
      <ul className="totp-codes" aria-label="Recovery codes">
        {codes.map((code) => (
          <li key={code}>{code}</li>
        ))}
      </ul>
      <div className="actions">
        <Copy text={text} label="Copy" />
        <button type="button" className="button" onClick={download}>
          Download .txt
        </button>
      </div>
      <label className="check">
        <input
          type="checkbox"
          checked={saved}
          onChange={(event) => setSaved(event.target.checked)}
        />
        I saved these codes somewhere safe
      </label>
      <div className="actions">
        <button type="button" className="button primary" disabled={!saved} onClick={done}>
          Done
        </button>
      </div>
    </div>
  );
}

/** Turning it off or replacing the recovery codes takes the password and a
 * second factor, so a stolen session alone can do neither. */
function Reauthenticate(props: { action: "disable" | "codes"; next: Next; cancel: () => void }) {
  const [password, setPassword] = useState("");
  const [code, setCode] = useState("");
  const { working, error, submit } = useSubmit(async () => {
    if (props.action === "disable") {
      await disableTotp(password, code.trim());
      props.cancel();
    } else {
      const codes = await replaceRecoveryCodes(password, code.trim());
      props.next({ kind: "codes", codes, enabling: false });
    }
  });
  const verb = props.action === "disable" ? "Turn off" : "Make new codes";
  return (
    <form className="pane-form" onSubmit={submit}>
      <p className="muted">
        {props.action === "disable"
          ? "Turning it off takes your password and a code."
          : "New recovery codes replace the old ones, which stop working."}
      </p>
      <Field
        label="Password"
        type="password"
        value={password}
        set={setPassword}
        autoComplete="current-password"
        autoFocus
      />
      <Field
        label="Code from your app, or a recovery code"
        value={code}
        set={setCode}
        autoComplete="one-time-code"
      />
      {error && <span className="error">{error.message}</span>}
      <div className="actions">
        <button
          type="submit"
          className="button primary"
          disabled={working || !password || !code.trim()}
        >
          {verb}
        </button>
        <button type="button" className="link-button" onClick={props.cancel}>
          Cancel
        </button>
      </div>
    </form>
  );
}
