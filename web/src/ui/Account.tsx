import type { JSX } from "@solidjs/web";
import {
  createEffect,
  createMemo,
  createSignal,
  createUniqueId,
  For,
  Match,
  onSettled,
  Show,
  Switch,
} from "solid-js";

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
import { useSelector } from "../state";
import { useModel } from "../store";
import { parseJoinLink, remote, setRemote, standalone } from "../target";
import { qrcodegen } from "../vendor/qrcodegen";
import type { Availability } from "../workbench/commands";
import { COMMANDS, type CommandId, useCommands } from "./commands";
import { Organizations, useRun } from "./Orgs";
import { ContinueWith, OrgLink, SignInMethods } from "./SingleSignOn";

function refused(): string | null {
  const last = remote();
  return last
    ? `The microscope at ${last.server} refused its link. It may have restarted with a new one.`
    : null;
}

function disconnect() {
  setRemote(null);
  window.location.assign("/");
}

export function ConnectBody() {
  const link = useModel((model) => model.link);
  if (!standalone) {
    return (
      <Show
        when={link() === "unauthorized"}
        fallback={
          <div class="pane-body pane-text">
            <p>
              This page came from <code>{window.location.origin}</code>, the{" "}
              <code>microscope web</code> it debugs with
              {link() === "open" ? ", and is connected to it." : "."}
            </p>
          </div>
        }
      >
        <div class="pane-body pane-text">
          <p>This browser has not joined this microscope session yet. Open a join link.</p>
          <p class="muted">
            <code>microscope web</code> prints one when it starts, and anyone in the session can
            make one with Share.
          </p>
        </div>
      </Show>
    );
  }
  const connected = remote();
  const status = createMemo(() => {
    const current = link();
    return current === "open" ||
      current === "connecting" ||
      current === "reconnecting" ||
      current === "incompatible"
      ? current
      : null;
  });
  return (
    <Show
      when={connected && link() !== "unauthorized" ? connected : null}
      fallback={
        <div class="pane-body pane-text">
          <Show when={connected}>
            <p class="error">{refused()}</p>
          </Show>
          <ConnectForm />
        </div>
      }
    >
      {(target) => (
        <div class="pane-body pane-text">
          <p class={link() === "unreachable" ? "error" : undefined}>
            <Show
              when={link() === "unreachable"}
              fallback={
                <Show when={status()}>
                  {(state) => (
                    <>
                      {CONNECTION[state()]} <code>{target().server}</code>
                      {state() === "connecting" || state() === "reconnecting" ? "…" : "."}
                    </>
                  )}
                </Show>
              }
            >
              Can&apos;t reach the microscope at <code>{target().server}</code>. It may have
              stopped; this page keeps trying.
            </Show>
          </p>
          <div class="actions">
            <button type="button" class="button" onClick={disconnect}>
              Disconnect
            </button>
          </div>
          <h2>Connect to another</h2>
          <ConnectForm />
        </div>
      )}
    </Show>
  );
}

const CONNECTION: Record<"open" | "connecting" | "reconnecting" | "incompatible", string> = {
  open: "Connected to",
  connecting: "Connecting to",
  reconnecting: "Lost the connection; reconnecting to",
  incompatible: "This page is from another version of microscope than the one at",
};

function ConnectForm() {
  const id = createUniqueId();
  const [link, setLink] = createSignal("");
  const [error, setError] = createSignal<string | null>(null);
  const connect = (event: SubmitEvent) => {
    event.preventDefault();
    const joined = parseJoinLink(link());
    if (!joined) {
      setError("That is not a join link. One looks like http://127.0.0.1:7341/join#c-…");
      return;
    }
    setRemote({ server: joined.server, token: joined.token });

    window.location.assign(isPagePath(joined.to) ? joined.to : "/");
  };
  const edit = (event: InputEvent & { currentTarget: HTMLInputElement }) => {
    setLink(event.currentTarget.value);
    setError(null);
  };
  return (
    <form class="pane-form" onSubmit={connect}>
      <div class="field">
        <label for={id}>Join link</label>
        <input
          id={id}
          class="input"
          value={link()}
          aria-invalid={error() === null ? "false" : "true"}
          data-autofocus=""
          onInput={edit}
          placeholder="http://127.0.0.1:7341/join#c-…"
          spellcheck={false}
        />
        <span class="note">
          <code>microscope web</code> prints one when it starts, and Share makes more.
        </span>
      </div>
      <Show when={error()}>{(message) => <span class="error">{message()}</span>}</Show>
      <div class="actions">
        <button type="submit" class="button primary" disabled={!link().trim()}>
          Connect
        </button>
      </div>
    </form>
  );
}

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
  const [error, setError] = createSignal<string | null>(null);
  const [everywhere, setEverywhere] = createSignal(false);
  const [step, setStep] = createSignal<Step>({ kind: "sign-in" });

  const go = (next: Step) => {
    account.setState({ notice: null });
    setStep(next);
  };
  onSettled(() => {
    void checkAccount();
  });

  const accountKind = createMemo(() => now().kind);
  createEffect(
    () => accountKind(),
    () => {
      setStep({ kind: "sign-in" });
    },
    { defer: true },
  );

  const challenge = useSelector(account, (state) => state.challenge);
  createEffect(
    () => [challenge(), accountKind()] as const,
    ([pending, kind]) => {
      if (pending !== null && kind === "out") {
        account.setState({ challenge: null });
        setStep({ kind: "totp", challenge: pending });
      }
    },
  );
  const outEverywhere = () => {
    setError(null);
    signOutEverywhere()
      .catch((failure: Error) => setError(failure.message))
      .finally(() => setEverywhere(false));
  };
  const out = () => {
    setError(null);
    signOut().catch((failure: Error) => setError(failure.message));
  };
  const askEverywhere = () => setEverywhere(true);
  const cancelEverywhere = () => setEverywhere(false);

  const reset = useSelector(account, (state) => state.reset);
  const heading = createMemo(() => {
    const current = now();
    return current.kind === "in" ? `Welcome, ${current.user.username}` : "Welcome to microscope";
  });
  const unavailable = createMemo(() => {
    const current = now();
    return current.kind === "unavailable" ? current : null;
  });
  const signedIn = createMemo(() => {
    const current = now();
    return current.kind === "in" ? current : null;
  });
  const sentStep = createMemo(() => {
    const current = step();
    return current.kind === "sent" ? current : null;
  });
  const codeStep = createMemo(() => {
    const current = step();
    return current.kind === "code" ? current : null;
  });
  const totpStep = createMemo(() => {
    const current = step();
    return current.kind === "totp" ? current : null;
  });
  const recoveryStep = createMemo(() => {
    const current = step();
    return current.kind === "recovery" ? current : null;
  });
  const forgotStep = createMemo(() => {
    const current = step();
    return current.kind === "forgot" ? current : null;
  });

  return (
    <div class="pane-body pane-text">
      <Show when={notice()}>
        {(current) => (
          <Show
            when={current().ok}
            fallback={
              <div class="banner error" role="alert">
                {current().text}
              </div>
            }
          >
            {/* oxlint-disable-next-line jsx-a11y/prefer-tag-over-role -- a notice, not a form's result */}
            <p role="status">{current().text}</p>
          </Show>
        )}
      </Show>
      <h1>{heading()}</h1>
      <Actions />
      <OrgLink />
      <Show
        when={reset()}
        fallback={
          <Switch
            fallback={
              <Switch fallback={<SignIn go={go} />}>
                <Match when={step().kind === "sign-up"}>
                  <SignUp go={go} />
                </Match>
                <Match when={sentStep()}>{(s) => <Sent email={s().email} go={go} />}</Match>
                <Match when={codeStep()}>{(s) => <Code challenge={s().challenge} go={go} />}</Match>
                <Match when={totpStep()}>{(s) => <Totp challenge={s().challenge} go={go} />}</Match>
                <Match when={recoveryStep()}>
                  {(s) => <Recovery challenge={s().challenge} go={go} />}
                </Match>
                <Match when={forgotStep()}>{(s) => <Forgot email={s().email} go={go} />}</Match>
              </Switch>
            }
          >
            <Match when={accountKind() === "checking"}>
              <p class="muted">Checking whether you are signed in…</p>
            </Match>
            <Match when={unavailable()}>
              {(current) => (
                <p class="muted">
                  Sign-in is unavailable ({current().why}). You can still connect without an
                  account.
                </p>
              )}
            </Match>
            <Match when={signedIn()}>
              {(current) => (
                <>
                  <p>
                    Signed in as <b>{current().user.username}</b> ({current().user.email}).
                  </p>
                  <p class="muted">Your programs, sessions, and profiles will show up here.</p>
                  <Show when={error()}>{(message) => <span class="error">{message()}</span>}</Show>
                  <div class="actions">
                    <Show
                      when={everywhere()}
                      fallback={
                        <>
                          <button type="button" class="button" onClick={out}>
                            Sign out
                          </button>
                          <button type="button" class="button" onClick={askEverywhere}>
                            Sign out everywhere
                          </button>
                        </>
                      }
                    >
                      <span class="confirm">
                        <span>Sign out on every device?</span>
                        <button type="button" class="button danger" onClick={outEverywhere}>
                          Sign out everywhere
                        </button>
                        <button type="button" class="link-button" onClick={cancelEverywhere}>
                          Cancel
                        </button>
                      </span>
                    </Show>
                  </div>
                  <Organizations />
                  <SignInMethods />
                  <TwoFactor user={current().user} />
                </>
              )}
            </Match>
          </Switch>
        }
      >
        {(token) => <NewPassword token={token()} />}
      </Show>
    </div>
  );
}

type Go = (step: Step) => void;

const ACTIONS: readonly CommandId[] = ["launch", "attach", "core", "record"];

function Actions() {
  const commands = useCommands();
  const signature = useModel((model) =>
    [model.hello?.role, model.link, model.state?.session].join(),
  );
  const offered = createMemo(() => {
    signature();
    return ACTIONS.flatMap((id) => {
      const availability = commands.availability(id);
      return availability.state === "absent" ? [] : [{ id, availability }];
    });
  });
  return (
    <Show when={offered().length > 0}>
      <ul class="start-actions">
        <For each={offered()} keyed={(item) => item.id}>
          {(item) => (
            <Action id={item().id} availability={item().availability} onRun={commands.run} />
          )}
        </For>
      </ul>
    </Show>
  );
}

function Action(props: {
  id: CommandId;
  availability: Availability;
  onRun: (id: CommandId) => unknown;
}) {
  const onClick = () => props.onRun(props.id);
  const key = createMemo(() => COMMANDS.describe(props.id) ?? COMMANDS.describe(props.id, true));
  const blocked = createMemo(() =>
    props.availability.state === "blocked" ? props.availability.why : undefined,
  );
  return (
    <li>
      <button
        type="button"
        class="button"
        disabled={props.availability.state !== "ready"}
        title={blocked()}
        onClick={onClick}
      >
        {COMMANDS.get(props.id).label}
      </button>
      <Show when={key()}>{(shortcut) => <kbd>{shortcut()}</kbd>}</Show>
      <Show when={blocked()}>{(why) => <span class="muted">{why()}</span>}</Show>
    </li>
  );
}

function useSubmit(send: () => Promise<unknown>) {
  const { working, error, run } = useRun();
  const submit = (event: SubmitEvent) => {
    event.preventDefault();
    run(send);
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
  first?: boolean;
  code?: boolean;
}) {
  const id = createUniqueId();
  const noteId = createUniqueId();
  const edit = (event: InputEvent & { currentTarget: HTMLInputElement }) =>
    props.set(event.currentTarget.value);
  return (
    <div class="field">
      <label for={id}>{props.label}</label>
      <input
        id={id}
        class="input"
        type={props.type}
        autocomplete={props.autoComplete}
        spellcheck={false}
        data-autofocus={props.first ? "" : undefined}
        aria-describedby={props.note ? noteId : undefined}
        inputmode={props.code ? "numeric" : undefined}
        maxlength={props.code ? 6 : undefined}
        pattern={props.code ? "[0-9]{6}" : undefined}
        value={props.value}
        onInput={edit}
      />
      <Show when={props.note}>
        {(note) => (
          <span id={noteId} class="note">
            {note()}
          </span>
        )}
      </Show>
    </div>
  );
}

function usePasskeySignIn(on: () => boolean) {
  const { working, error, run, fail } = useRun();
  let autofill: AbortController | null = null;

  createEffect(
    () => on(),
    (enabled) => {
      if (!enabled) {
        return;
      }
      const controller = new AbortController();
      autofill = controller;
      autofillAvailable()
        .then((available) =>
          available ? signInWithPasskey({ autofill: true, signal: controller.signal }) : null,
        )
        .catch((failure: Error) => {
          if (!controller.signal.aborted) {
            fail(failure);
          }
        });
      return () => {
        controller.abort();
      };
    },
  );

  const start = () => {
    autofill?.abort();
    run(signInWithPasskey);
  };
  return {
    working,
    error: createMemo(() => error()?.message ?? null),
    start,
  };
}

function SignIn(props: { go: Go }) {
  const offered = useFeatures();
  const passkeys = createMemo(() => offered()?.passkeys === true && supported());
  const passkey = usePasskeySignIn(passkeys);
  const [name, setName] = createSignal("");
  const [password, setPassword] = createSignal("");
  const { working, error, submit } = useSubmit(async () => {
    const signed = await signIn(name(), password());
    if ("challenge" in signed) {
      props.go({ kind: signed.method === "totp" ? "totp" : "code", challenge: signed.challenge });
    }
  });

  const unconfirmed = createMemo(() => {
    const current = error();
    return current instanceof Refused && current.status === 403;
  });
  const forgot = () => {
    const current = name();
    props.go({ kind: "forgot", email: current.includes("@") ? current.trim() : "" });
  };
  const toSignUp = () => props.go({ kind: "sign-up" });
  return (
    <>
      <form class="pane-form" onSubmit={submit}>
        <p class="muted">Optional. A join link works without an account.</p>
        <Field
          label="Username or email"
          value={name()}
          set={setName}
          autoComplete={passkeys() ? "username webauthn" : "username"}
          first
        />
        <Field
          label="Password"
          type="password"
          value={password()}
          set={setPassword}
          autoComplete="current-password"
        />
        <Show when={error()}>{(err) => <span class="error">{err().message}</span>}</Show>
        <div class="actions">
          <button
            type="submit"
            class="button primary"
            disabled={working() || !name().trim() || !password()}
          >
            Sign in
          </button>
          <Show when={offered()?.email}>
            <button type="button" class="link-button" onClick={forgot}>
              Forgot password?
            </button>
          </Show>
          <span class="muted">
            No account?
            <button type="button" class="link-button" onClick={toSignUp}>
              Create one
            </button>
          </span>
        </div>
      </form>
      <ContinueWith>
        <Show when={passkeys()}>
          <button type="button" class="button" disabled={passkey.working()} onClick={passkey.start}>
            Sign in with a passkey
          </button>
        </Show>
      </ContinueWith>
      <Show when={passkey.error()}>
        {(message) => (
          <span class="error" role="alert">
            {message()}
          </span>
        )}
      </Show>
      <Show when={unconfirmed()}>
        <Resend email={name().includes("@") ? name() : ""} go={props.go}>
          <h2>Confirm your email address</h2>
          <p class="muted">
            Follow the link in the email sent when you signed up, or get a new one.
          </p>
        </Resend>
      </Show>
    </>
  );
}

function SignUp(props: { go: Go }) {
  const offered = useFeatures();
  const [username, setUsername] = createSignal("");
  const [email, setEmail] = createSignal("");
  const [password, setPassword] = createSignal("");
  const { working, error, submit } = useSubmit(async () => {
    const confirming = offered()?.email === true;
    const address = email();
    await signUp(username(), address, password(), confirming);
    if (confirming) {
      props.go({ kind: "sent", email: address });
    }
  });
  const toSignIn = () => props.go({ kind: "sign-in" });
  return (
    <>
      <form class="pane-form" onSubmit={submit}>
        <h2>Create an account</h2>
        <Field
          label="Username"
          value={username()}
          set={setUsername}
          autoComplete="username"
          note="3 to 32 letters, digits, _ or -."
          first
        />
        <Field label="Email" type="email" value={email()} set={setEmail} autoComplete="email" />
        <Field
          label="Password"
          type="password"
          value={password()}
          set={setPassword}
          autoComplete="new-password"
          note="At least 8 characters."
        />
        <Show when={error()}>{(err) => <span class="error">{err().message}</span>}</Show>
        <div class="actions">
          <button
            type="submit"
            class="button primary"
            disabled={
              working() || !offered() || !username().trim() || !email().trim() || !password()
            }
          >
            Create account
          </button>
          <span class="muted">
            Have one?
            <button type="button" class="link-button" onClick={toSignIn}>
              Sign in
            </button>
          </span>
        </div>
      </form>
      <ContinueWith />
    </>
  );
}

function Sent(props: { email: string; go: Go }) {
  return (
    <Resend email={props.email} go={props.go} fixed>
      <h2>Check your email</h2>
      <p>
        A confirmation link is on its way to <b>{props.email}</b>. Follow it, then sign in here.
      </p>
    </Resend>
  );
}

function Resend(props: { email: string; go: Go; fixed?: boolean; children: JSX.Element }) {
  const [email, setEmail] = createSignal(() => props.email);
  const [sent, setSent] = createSignal(false);
  const { working, error, submit } = useSubmit(async () => {
    setSent(false);
    await resendConfirmation(email());
    setSent(true);
  });
  const toSignIn = () => props.go({ kind: "sign-in" });
  return (
    <form class="pane-form" onSubmit={submit}>
      {props.children}
      <Show when={!props.fixed}>
        <Field label="Email" type="email" value={email()} set={setEmail} autoComplete="email" />
      </Show>
      <Show when={sent()}>
        {/* oxlint-disable-next-line jsx-a11y/prefer-tag-over-role -- a notice, not a form's result */}
        <p role="status" class="muted">
          If {email()} still needs confirming, a new link is on its way.
        </p>
      </Show>
      <Show when={error()}>{(err) => <span class="error">{err().message}</span>}</Show>
      <div class="actions">
        <button type="submit" class="button" disabled={working() || !email().trim()}>
          Send another link
        </button>
        <Show when={props.fixed}>
          <button type="button" class="button primary" onClick={toSignIn}>
            Sign in
          </button>
        </Show>
      </div>
    </form>
  );
}

function Forgot(props: { email: string; go: Go }) {
  const [email, setEmail] = createSignal(() => props.email);
  const [sent, setSent] = createSignal<string | null>(null);
  const { working, error, submit } = useSubmit(async () => {
    setSent(null);
    const address = email().trim();
    await requestPasswordReset(address);
    setSent(address);
  });
  const toSignIn = () => props.go({ kind: "sign-in" });
  return (
    <form class="pane-form" onSubmit={submit}>
      <h2>Reset your password</h2>
      <p class="muted">We will email you a link to choose a new one.</p>
      <Field label="Email" type="email" value={email()} set={setEmail} autoComplete="email" first />
      <Show when={sent()}>
        {(address) => (
          <>
            {/* oxlint-disable-next-line jsx-a11y/prefer-tag-over-role -- a notice, not a form's result */}
            <p role="status" class="muted">
              If an account uses {address()}, a reset link is on its way.
            </p>
          </>
        )}
      </Show>
      <Show when={error()}>{(err) => <span class="error">{err().message}</span>}</Show>
      <div class="actions">
        <button type="submit" class="button primary" disabled={working() || !email().trim()}>
          {sent() ? "Send another link" : "Send reset link"}
        </button>
        <button type="button" class="link-button" onClick={toSignIn}>
          Back to sign in
        </button>
      </div>
    </form>
  );
}

const cancelReset = () => account.setState({ reset: null });

function NewPassword(props: { token: string }) {
  const [password, setPassword] = createSignal("");
  const { working, error, submit } = useSubmit(() => resetPassword(props.token, password()));
  return (
    <form class="pane-form" onSubmit={submit}>
      <h2>Choose a new password</h2>
      <p class="muted">Changing it signs your account out on every device.</p>
      <Field
        label="New password"
        type="password"
        value={password()}
        set={setPassword}
        autoComplete="new-password"
        note="At least 8 characters."
        first
      />
      <Show when={error()}>{(err) => <span class="error">{err().message}</span>}</Show>
      <div class="actions">
        <button type="submit" class="button primary" disabled={working() || !password()}>
          Change password
        </button>
        <button type="button" class="button" onClick={cancelReset}>
          Cancel
        </button>
      </div>
    </form>
  );
}

function Code(props: { challenge: string; go: Go }) {
  const [code, setCode] = createSignal("");
  const { working, error, submit } = useSubmit(async () => {
    await enterCode(props.challenge, code().trim());
  });
  const startOver = () => props.go({ kind: "sign-in" });
  return (
    <form class="pane-form" onSubmit={submit}>
      <h2>Enter the code from your email</h2>
      <p class="muted">The email also has a link, which signs in whichever device opens it.</p>
      <Field label="Code" value={code()} set={setCode} autoComplete="one-time-code" first code />
      <Show when={error()}>{(err) => <span class="error">{err().message}</span>}</Show>
      <div class="actions">
        <button
          type="submit"
          class="button primary"
          disabled={working() || !/^\d{6}$/u.test(code().trim())}
        >
          Sign in
        </button>
        <button type="button" class="link-button" onClick={startOver}>
          Start over
        </button>
      </div>
    </form>
  );
}

function Totp(props: { challenge: string; go: Go }) {
  const [code, setCode] = createSignal("");
  const { working, error, submit } = useSubmit(async () => {
    await enterTotp(props.challenge, code().trim());
  });
  const recover = () => props.go({ kind: "recovery", challenge: props.challenge });
  const startOver = () => props.go({ kind: "sign-in" });

  const locked = createMemo(() => {
    const current = error();
    return current instanceof Refused && current.status === 429;
  });
  return (
    <form class="pane-form" onSubmit={submit}>
      <h2>Enter the code from your authenticator app</h2>
      <Field label="Code" value={code()} set={setCode} autoComplete="one-time-code" first code />
      <Show when={error()}>{(err) => <span class="error">{err().message}</span>}</Show>
      <div class="actions">
        <button
          type="submit"
          class={["button", { primary: !locked() }]}
          disabled={working() || !/^\d{6}$/u.test(code().trim())}
        >
          Sign in
        </button>
        <Show when={locked()}>
          <button type="button" class="button primary" onClick={recover}>
            Use a recovery code
          </button>
        </Show>
        <button type="button" class="link-button" onClick={startOver}>
          Start over
        </button>
      </div>
      <Show when={!locked()}>
        <p class="muted">
          Lost your device?
          <button type="button" class="link-button" onClick={recover}>
            Use a recovery code
          </button>
        </p>
      </Show>
    </form>
  );
}

function Recovery(props: { challenge: string; go: Go }) {
  const [code, setCode] = createSignal("");
  const { working, error, submit } = useSubmit(async () => {
    await enterRecoveryCode(props.challenge, code().trim());
  });
  const toApp = () => props.go({ kind: "totp", challenge: props.challenge });
  return (
    <form class="pane-form" onSubmit={submit}>
      <h2>Enter a recovery code</h2>
      <Field
        label="Recovery code"
        value={code()}
        set={setCode}
        autoComplete="off"
        note="Each code works once. We email you when one is used."
        first
      />
      <Show when={error()}>{(err) => <span class="error">{err().message}</span>}</Show>
      <div class="actions">
        <button type="submit" class="button primary" disabled={working() || !code().trim()}>
          Sign in
        </button>
        <button type="button" class="link-button" onClick={toApp}>
          Use the app instead
        </button>
      </div>
    </form>
  );
}

type Setup =
  | { kind: "idle" }
  | { kind: "password" }
  | { kind: "scan"; secret: string; uri: string }
  | { kind: "codes"; codes: string[]; enabling: boolean }
  | { kind: "reauth"; action: "disable" | "codes" };

function TwoFactor(props: { user: User }) {
  const offered = useFeatures();
  const [setup, setSetup] = createSignal<Setup>({ kind: "idle" });
  const idle = () => setSetup({ kind: "idle" });
  const saved = () => {
    const current = setup();
    const enabling = current.kind === "codes" && current.enabling;
    idle();
    if (enabling) {
      void totpConfirmed();
    }
  };
  const newCodes = () => setSetup({ kind: "reauth", action: "codes" });
  const turnOff = () => setSetup({ kind: "reauth", action: "disable" });
  const start = () => setSetup({ kind: "password" });

  const visible = createMemo(() => {
    const current = offered();
    return Boolean(current && (current.totp || props.user.totp_enabled));
  });
  const scanSetup = createMemo(() => {
    const current = setup();
    return current.kind === "scan" ? current : null;
  });
  const codesSetup = createMemo(() => {
    const current = setup();
    return current.kind === "codes" ? current : null;
  });
  const reauthSetup = createMemo(() => {
    const current = setup();
    return current.kind === "reauth" ? current : null;
  });

  return (
    <Show when={visible()}>
      <h2>Two-factor authentication</h2>
      <Switch
        fallback={
          <Show
            when={props.user.totp_enabled}
            fallback={
              <>
                <p class="muted">
                  Off. Turn it on, and signing in will take a code from an authenticator app as well
                  as your password.
                </p>
                <div class="actions">
                  <button type="button" class="button" onClick={start}>
                    Set up
                  </button>
                </div>
              </>
            }
          >
            <p class="muted">On. Signing in takes your password and a code from your app.</p>
            <div class="actions">
              <button type="button" class="button" onClick={newCodes}>
                New recovery codes
              </button>
              <button type="button" class="button" onClick={turnOff}>
                Turn off
              </button>
            </div>
          </Show>
        }
      >
        <Match when={setup().kind === "password"}>
          <SetupPassword next={setSetup} cancel={idle} />
        </Match>
        <Match when={scanSetup()}>
          {(current) => (
            <Scan secret={current().secret} uri={current().uri} next={setSetup} cancel={idle} />
          )}
        </Match>
        <Match when={codesSetup()}>
          {(current) => <RecoveryCodes codes={current().codes} done={saved} />}
        </Match>
        <Match when={reauthSetup()}>
          {(current) => <Reauthenticate action={current().action} next={setSetup} cancel={idle} />}
        </Match>
      </Switch>
    </Show>
  );
}

type Next = (setup: Setup) => void;

function SetupPassword(props: { next: Next; cancel: () => void }) {
  const [password, setPassword] = createSignal("");
  const { working, error, submit } = useSubmit(async () => {
    const { secret, uri } = await startTotpSetup(password());
    props.next({ kind: "scan", secret, uri });
  });
  return (
    <form class="pane-form" onSubmit={submit}>
      <Field
        label="Password"
        type="password"
        value={password()}
        set={setPassword}
        autoComplete="current-password"
        note="Enter your password to start."
        first
      />
      <Show when={error()}>{(err) => <span class="error">{err().message}</span>}</Show>
      <div class="actions">
        <button type="submit" class="button primary" disabled={working() || !password()}>
          Continue
        </button>
        <button type="button" class="link-button" onClick={() => props.cancel()}>
          Cancel
        </button>
      </div>
    </form>
  );
}

function Scan(props: { secret: string; uri: string; next: Next; cancel: () => void }) {
  const [code, setCode] = createSignal("");
  const { working, error, submit } = useSubmit(async () => {
    const codes = await confirmTotpSetup(code().trim());
    props.next({ kind: "codes", codes, enabling: true });
  });
  return (
    <form class="pane-form" onSubmit={submit}>
      <div class="totp-scan">
        <Qr text={props.uri} />
        <div class="totp-key">
          <span>
            Scan this with an authenticator app, such as 1Password or Google Authenticator.
          </span>
          <span class="muted">Can&apos;t scan it? Enter this key:</span>
          <code data-testid="totp-secret">{props.secret.match(/.{1,4}/gu)?.join(" ")}</code>
          <div class="actions">
            <Copy text={props.secret} label="Copy key" />
          </div>
        </div>
      </div>
      <Field
        label="Code from the app"
        value={code()}
        set={setCode}
        autoComplete="one-time-code"
        note="Turning it on signs you out everywhere else."
        first
        code
      />
      <Show when={error()}>{(err) => <span class="error">{err().message}</span>}</Show>
      <div class="actions">
        <button
          type="submit"
          class="button primary"
          disabled={working() || !/^\d{6}$/u.test(code().trim())}
        >
          Turn on
        </button>
        <button type="button" class="link-button" onClick={() => props.cancel()}>
          Cancel
        </button>
      </div>
    </form>
  );
}

function Qr(props: { text: string }) {
  const drawing = createMemo(() => {
    const qr = qrcodegen.QrCode.encodeText(props.text, qrcodegen.QrCode.Ecc.MEDIUM);
    const quiet = 4;
    let path = "";
    for (let y = 0; y < qr.size; y++) {
      for (let x = 0; x < qr.size; x++) {
        if (qr.getModule(x, y)) {
          path += `M${x + quiet} ${y + quiet}h1v1h-1z`;
        }
      }
    }
    return { side: qr.size + quiet * 2, path };
  });
  return (
    <svg
      class="totp-qr"
      viewBox={`0 0 ${drawing().side} ${drawing().side}`}
      // oxlint-disable-next-line jsx-a11y/prefer-tag-over-role -- an inline drawing, not an image file
      role="img"
      aria-label="QR code for your authenticator app"
      shape-rendering="crispEdges"
    >
      <rect width={drawing().side} height={drawing().side} fill="#fff" />
      <path d={drawing().path} fill="#000" />
    </svg>
  );
}

function Copy(props: { text: string; label: string }) {
  const [copied, setCopied] = createSignal(false);
  const copy = () => {
    void navigator.clipboard.writeText(props.text).then(() => setCopied(true));
  };
  if (!navigator.clipboard) {
    return null;
  }
  return (
    <button type="button" class="button" onClick={copy}>
      {copied() ? "Copied" : props.label}
    </button>
  );
}

function RecoveryCodes(props: { codes: string[]; done: () => void }) {
  const [saved, setSaved] = createSignal(false);
  const text = createMemo(
    () => `Microscope recovery codes. Each code works once.\n\n${props.codes.join("\n")}\n`,
  );
  const download = () => {
    const url = URL.createObjectURL(new Blob([text()], { type: "text/plain" }));
    const link = document.createElement("a");
    link.href = url;
    link.download = "microscope-recovery-codes.txt";
    link.click();
    URL.revokeObjectURL(url);
  };
  const check = (event: Event & { currentTarget: HTMLInputElement }) =>
    setSaved(event.currentTarget.checked);
  return (
    <div class="pane-form">
      <p>
        <b>Save your recovery codes.</b> If you lose your authenticator, each of these signs you in
        once. This is the only time they are shown.
      </p>
      <ul class="totp-codes" aria-label="Recovery codes">
        <For each={props.codes}>{(code) => <li>{code}</li>}</For>
      </ul>
      <div class="actions">
        <Copy text={text()} label="Copy" />
        <button type="button" class="button" onClick={download}>
          Download .txt
        </button>
      </div>
      <label class="check">
        <input type="checkbox" checked={saved()} onChange={check} />I saved these codes somewhere
        safe
      </label>
      <div class="actions">
        <button
          type="button"
          class="button primary"
          disabled={!saved()}
          onClick={() => props.done()}
        >
          Done
        </button>
      </div>
    </div>
  );
}

function Reauthenticate(props: { action: "disable" | "codes"; next: Next; cancel: () => void }) {
  const [password, setPassword] = createSignal("");
  const [code, setCode] = createSignal("");
  const { working, error, submit } = useSubmit(async () => {
    if (props.action === "disable") {
      await disableTotp(password(), code().trim());
      props.cancel();
    } else {
      const codes = await replaceRecoveryCodes(password(), code().trim());
      props.next({ kind: "codes", codes, enabling: false });
    }
  });
  return (
    <form class="pane-form" onSubmit={submit}>
      <p class="muted">
        {props.action === "disable"
          ? "Turning it off takes your password and a code."
          : "New recovery codes replace the old ones, which stop working."}
      </p>
      <Field
        label="Password"
        type="password"
        value={password()}
        set={setPassword}
        autoComplete="current-password"
        first
      />
      <Field
        label="Code from your app, or a recovery code"
        value={code()}
        set={setCode}
        autoComplete="one-time-code"
      />
      <Show when={error()}>{(err) => <span class="error">{err().message}</span>}</Show>
      <div class="actions">
        <button
          type="submit"
          class="button primary"
          disabled={working() || !password() || !code().trim()}
        >
          {props.action === "disable" ? "Turn off" : "Make new codes"}
        </button>
        <button type="button" class="link-button" onClick={() => props.cancel()}>
          Cancel
        </button>
      </div>
    </form>
  );
}
