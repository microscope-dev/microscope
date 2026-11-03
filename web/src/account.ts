// Accounts on the services backend, which serves a page served on its own
// under the page's own `/api/`: the development server proxies it there
// (vite.config.ts). The session is services' HttpOnly cookie.
//
// When services sends email, a new account confirms its address by a link
// to `/confirm-email`, and signing in takes the password and then the code
// or the link (`/login-link`) it emails. Without email, a password alone
// signs in, and signing up signs in too.
//
// An account with two-factor authentication finishes signing in with a code
// from its authenticator app, or one of its recovery codes, and never gets
// an emailed code.
//
// Signing in through Google, GitHub, or an organization's identity provider
// leaves the page and comes back to `/signed-in` (sso.ts).

import { createStore, useStore } from "zustand";
import { standalone } from "./target";

export interface User {
  id: number;
  username: string;
  email: string;
  totp_enabled: boolean;
}

/** What services answered when it refused, with its HTTP status. */
export class Refused extends Error {
  constructor(
    message: string,
    readonly status: number,
  ) {
    super(message);
  }
}

/** What services said went wrong, or why it could not be asked. */
export async function failure(response: Response): Promise<Refused> {
  const body: unknown = await response.json().catch(() => null);
  const error = (body as { error?: unknown } | null)?.error;
  return new Refused(
    typeof error === "string" ? error : `the services backend answered ${response.status}`,
    response.status,
  );
}

async function post(path: string, body: unknown): Promise<Response> {
  const response = await fetch(path, {
    method: "POST",
    credentials: "same-origin",
    headers: { "Content-Type": "application/json" },
    body: JSON.stringify(body),
  });
  if (!response.ok) {
    throw await failure(response);
  }
  return response;
}

/** The signed-in user, or null when nobody is. A session whose account has
 * not confirmed its email is no session yet. */
export async function currentUser(): Promise<User | null> {
  const response = await fetch("/api/users/me", { credentials: "same-origin" });
  if (response.status === 401 || response.status === 403) {
    return null;
  }
  if (!response.ok) {
    throw await failure(response);
  }
  return (await response.json()) as User;
}

/** Counts the page's own sign-ins and outs, which a check that set out
 * before one of them must not undo. */
let changes = 0;

const signedIn = (user: User): User => {
  changes += 1;
  account.setState({ now: { kind: "in", user }, notice: null });
  return user;
};

/** How a challenge is finished: the code services emailed, or a code from
 * an authenticator app. */
export type Method = "email" | "totp";

/** Signs in with a password: done, or a challenge to finish by `method`. */
export async function signIn(
  usernameOrEmail: string,
  password: string,
): Promise<{ user: User } | { challenge: string; method: Method }> {
  const response = await post("/api/login", { username_or_email: usernameOrEmail, password });
  if (response.status === 202) {
    return (await response.json()) as { challenge: string; method: Method };
  }
  return { user: signedIn((await response.json()) as User) };
}

export async function enterTotp(challenge: string, code: string): Promise<User> {
  const response = await post("/api/login/totp", { challenge, code });
  return signedIn((await response.json()) as User);
}

export async function enterRecoveryCode(challenge: string, recoveryCode: string): Promise<User> {
  const response = await post("/api/login/recovery", { challenge, recovery_code: recoveryCode });
  return signedIn((await response.json()) as User);
}

/** Asks services for the account again, after a change to it. */
async function refresh(): Promise<void> {
  const user = await currentUser();
  changes += 1;
  account.setState({ now: user ? { kind: "in", user } : { kind: "out" } });
}

/** A new secret for an authenticator app, which is not in use until a code
 * from it confirms it. `uri` is the otpauth:// address a QR code carries. */
export async function startTotpSetup(password: string): Promise<{ secret: string; uri: string }> {
  const response = await post("/api/users/me/totp", { password });
  return (await response.json()) as { secret: string; uri: string };
}

/** Turns two-factor authentication on, and returns the recovery codes,
 * which services shows this once. */
export async function confirmTotpSetup(code: string): Promise<string[]> {
  const response = await post("/api/users/me/totp/confirm", { code });
  const { recovery_codes } = (await response.json()) as { recovery_codes: string[] };
  return recovery_codes;
}

/** Shows the account with two-factor authentication on, once its recovery
 * codes are saved. */
export function totpConfirmed(): Promise<void> {
  return refresh();
}

/** `code` is from the authenticator app, or a recovery code. */
export async function disableTotp(password: string, code: string): Promise<void> {
  await post("/api/users/me/totp/disable", { password, code });
  await refresh();
}

export async function replaceRecoveryCodes(password: string, code: string): Promise<string[]> {
  const response = await post("/api/users/me/totp/recovery-codes", { password, code });
  const { recovery_codes } = (await response.json()) as { recovery_codes: string[] };
  return recovery_codes;
}

export async function enterCode(challenge: string, code: string): Promise<User> {
  const response = await post("/api/login/code", { challenge, code });
  return signedIn((await response.json()) as User);
}

/** Makes an account. Returns whether its email must be confirmed before it
 * can sign in; when it need not be, services signed it in already. */
export async function signUp(username: string, email: string, password: string): Promise<boolean> {
  await post("/api/users", { username, email, password });
  const user = await currentUser();
  if (user) {
    signedIn(user);
  }
  return user === null;
}

/** Asks for another confirmation link. Services answers the same whether
 * or not it sent one, so nobody learns which addresses have accounts. */
export async function resendConfirmation(email: string): Promise<void> {
  await post("/api/email-confirmations/resend", { email });
}

export async function signOut(): Promise<void> {
  const response = await fetch("/api/logout", { method: "POST", credentials: "same-origin" });
  if (!response.ok) {
    throw await failure(response);
  }
  changes += 1;
  account.setState({ now: { kind: "out" }, notice: null });
}

/** Who is signed in, as far as the page knows. */
export type Account =
  | { readonly kind: "checking" }
  | { readonly kind: "unavailable"; readonly why: string }
  | { readonly kind: "out" }
  | { readonly kind: "in"; readonly user: User };

/** What became of a link from an email, for the Account pane to say. */
export interface Notice {
  readonly ok: boolean;
  readonly text: string;
}

export const account = createStore<{
  now: Account;
  notice: Notice | null;
  /** A TOTP challenge single sign-on left for the Account pane to finish. */
  challenge: string | null;
}>(() => ({
  now: { kind: "checking" },
  notice: null,
  challenge: null,
}));

export function useAccount(): Account {
  return useStore(account, (state) => state.now);
}

export function useNotice(): Notice | null {
  return useStore(account, (state) => state.notice);
}

/** The question to services in flight, which panes opening together share. */
let asking: Promise<void> | null = null;

/** A login link's request in flight, until its answer is applied. */
let following: Promise<void> | null = null;

/** Asks services who is signed in. A page `microscope web` serves has no
 * services behind it. */
export function checkAccount(): Promise<void> {
  if (!standalone) {
    account.setState({
      now: { kind: "unavailable", why: "this page is served by microscope web, which has none" },
    });
    return Promise.resolve();
  }
  // A login link being followed decides who is signed in itself.
  if (following !== null) {
    return following;
  }
  if (asking === null) {
    const before = changes;
    const settle = (now: Account) => {
      if (changes === before) {
        account.setState({ now });
      }
    };
    asking = ask()
      .then(settle)
      .finally(() => {
        asking = null;
      });
  }
  return asking;
}

function ask(): Promise<Account> {
  return currentUser().then(
    (user) => (user ? { kind: "in", user } : { kind: "out" }),
    (error: Error) => ({ kind: "unavailable", why: error.message }),
  );
}

/** Takes the fragment out of the address bar, so nothing later can copy
 * what it carried. */
function takeFragment(): URLSearchParams {
  const fragment = new URLSearchParams(window.location.hash.slice(1));
  if (window.location.hash) {
    window.history.replaceState(null, "", window.location.pathname + window.location.search);
  }
  return fragment;
}

/** The token from an emailed link's `#token=…`, or null. */
function takeToken(): string | null {
  return takeFragment().get("token");
}

/** Follows services back from Google, GitHub, or an identity provider, to
 * `/signed-in`. Its fragment says what went wrong, what to know, or the
 * TOTP challenge left to finish; with none, services signed the person in
 * and set the session cookie. */
export function followSignIn(): void {
  const fragment = takeFragment();
  if (!standalone) {
    return;
  }
  const error = fragment.get("error");
  const notice = fragment.get("notice");
  const challenge = fragment.get("challenge");
  changes += 1;
  // Only what the fragment carried changes: once taken, the fragment is
  // gone, and following again, as React's development build does, must not
  // erase what the first time found.
  account.setState({
    now: { kind: "checking" },
    ...(error !== null
      ? { notice: { ok: false, text: error } }
      : notice !== null
        ? { notice: { ok: true, text: notice } }
        : {}),
    ...(challenge !== null && fragment.get("method") === "totp" ? { challenge } : {}),
  });
  following = ask()
    .then((now) => {
      changes += 1;
      account.setState({ now });
    })
    .finally(() => {
      following = null;
    });
}

/** Follows the link a confirmation or login email sent: `/confirm-email`
 * or `/login-link`. Says how it went in the Account pane. */
export function followEmailLink(link: "confirm" | "login"): void {
  const token = takeToken();
  if (token === null || !standalone) {
    return;
  }
  const done = (notice: Notice) => account.setState({ notice });
  if (link === "confirm") {
    post("/api/email-confirmations", { token })
      .then(() => done({ ok: true, text: "Your email address is confirmed. You can sign in now." }))
      .catch((error: Error) => done({ ok: false, text: `Could not confirm: ${error.message}.` }));
  } else {
    // Until the link is answered, the pane shows neither the sign-in form
    // nor an older check's answer.
    changes += 1;
    account.setState({ now: { kind: "checking" } });
    following = post("/api/login/link", { token })
      .then(async (response) => {
        signedIn((await response.json()) as User);
      })
      .catch(async (error: Error) => {
        done({ ok: false, text: `Could not sign in: ${error.message}.` });
        account.setState({ now: await ask() });
      })
      .finally(() => {
        following = null;
      });
  }
}
