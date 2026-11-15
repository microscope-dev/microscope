import { createStore, useStore } from "zustand";

import { standalone } from "./target";

export interface User {
  id: number;
  username: string;
  email: string;
  totp_enabled: boolean;
}

export class Refused extends Error {
  readonly status: number;

  constructor(message: string, status: number) {
    super(message);
    this.status = status;
  }
}

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

let epoch = 0;

function decided(state: Partial<AccountState>): void {
  epoch += 1;
  account.setState(state);
}

async function askSince(): Promise<{ now: Account; apply(state: Partial<AccountState>): void }> {
  const asked = epoch;
  const now = await ask();
  return {
    now,
    apply: (state) => {
      if (epoch === asked) {
        decided(state);
      }
    },
  };
}

const signedIn = (user: User): User => {
  decided({ now: { kind: "in", user }, notice: null });
  return user;
};

export type Method = "email" | "totp";

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

export async function passkeySignedIn(answer: unknown): Promise<User> {
  const response = await post("/api/passkeys/sign-in/finish", answer);
  return signedIn((await response.json()) as User);
}

export async function enterTotp(challenge: string, code: string): Promise<User> {
  const response = await post("/api/login/totp", { challenge, code });
  return signedIn((await response.json()) as User);
}

export async function enterRecoveryCode(challenge: string, recoveryCode: string): Promise<User> {
  const response = await post("/api/login/recovery", { challenge, recovery_code: recoveryCode });
  return signedIn((await response.json()) as User);
}

async function refresh(): Promise<void> {
  const { now, apply } = await askSince();
  apply({ now });
}

export async function startTotpSetup(password: string): Promise<{ secret: string; uri: string }> {
  const response = await post("/api/users/me/totp", { password });
  return (await response.json()) as { secret: string; uri: string };
}

export async function confirmTotpSetup(code: string): Promise<string[]> {
  const response = await post("/api/users/me/totp/confirm", { code });
  const { recovery_codes } = (await response.json()) as { recovery_codes: string[] };
  return recovery_codes;
}

export function totpConfirmed(): Promise<void> {
  return refresh();
}

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

export async function signUp(
  username: string,
  email: string,
  password: string,
  confirming: boolean,
): Promise<void> {
  const response = await post("/api/users", { username, email, password });
  if (!confirming) {
    signedIn((await response.json()) as User);
  }
}

export async function resendConfirmation(email: string): Promise<void> {
  await post("/api/email-confirmations/resend", { email });
}

export async function requestPasswordReset(email: string): Promise<void> {
  await post("/api/password-resets", { email });
}

export async function resetPassword(token: string, password: string): Promise<void> {
  await post("/api/password-resets/complete", { token, password });
  const { now, apply } = await askSince();
  const user = now.kind === "in" ? now.user : null;
  apply({
    now,
    reset: null,
    notice: {
      ok: true,
      text: user
        ? `The password is changed, and that account is signed out everywhere. You are still signed in as ${user.username}.`
        : "Your password is changed, and every device is signed out. Sign in with the new password.",
    },
  });
}

export async function signOut(): Promise<void> {
  const response = await fetch("/api/logout", { method: "POST", credentials: "same-origin" });
  if (!response.ok) {
    throw await failure(response);
  }
  decided({ now: { kind: "out" }, notice: null });
}

export async function signOutEverywhere(): Promise<void> {
  await post("/api/logout/everywhere", null);
  decided({
    now: { kind: "out" },
    notice: { ok: true, text: "You are signed out on every device." },
  });
}

export type Account =
  | { readonly kind: "checking" }
  | { readonly kind: "unavailable"; readonly why: string }
  | { readonly kind: "out" }
  | { readonly kind: "in"; readonly user: User };

export interface Notice {
  readonly ok: boolean;
  readonly text: string;
}

interface AccountState {
  now: Account;
  notice: Notice | null;

  challenge: string | null;

  reset: string | null;
}

export const account = createStore<AccountState>(() => ({
  now: { kind: "checking" },
  notice: null,
  challenge: null,
  reset: null,
}));

export function useAccount(): Account {
  return useStore(account, (state) => state.now);
}

export function useNotice(): Notice | null {
  return useStore(account, (state) => state.notice);
}

let asking: Promise<void> | null = null;

let following: Promise<void> | null = null;

export function checkAccount(): Promise<void> {
  if (!standalone) {
    account.setState({
      now: { kind: "unavailable", why: "this page is served by microscope web, which has none" },
    });
    return Promise.resolve();
  }

  if (following !== null) {
    return following;
  }
  if (asking === null) {
    asking = askSince()
      .then(({ now, apply }) => apply({ now }))
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

function takeFragment(): URLSearchParams {
  const fragment = new URLSearchParams(window.location.hash.slice(1));
  if (window.location.hash) {
    window.history.replaceState(null, "", window.location.pathname + window.location.search);
  }
  return fragment;
}

function takeToken(): string | null {
  return takeFragment().get("token");
}

export function followSignIn(): void {
  const fragment = takeFragment();
  if (!standalone) {
    return;
  }
  const error = fragment.get("error");
  const notice = fragment.get("notice");
  const challenge = fragment.get("challenge");

  decided({
    now: { kind: "checking" },
    ...(error === null
      ? notice === null
        ? {}
        : { notice: { ok: true, text: notice } }
      : { notice: { ok: false, text: error } }),
    ...(challenge !== null && fragment.get("method") === "totp" ? { challenge } : {}),
  });
  following = (async () => {
    decided({ now: await ask() });
  })().finally(() => {
    following = null;
  });
}

function tell(notice: Notice): void {
  account.setState({ notice });
}

export function followEmailLink(link: "confirm" | "login" | "reset"): void {
  const token = takeToken();
  if (token === null || !standalone) {
    return;
  }
  if (link === "reset") {
    account.setState({ reset: token, notice: null });
  } else if (link === "confirm") {
    post("/api/email-confirmations", { token })
      .then(() => tell({ ok: true, text: "Your email address is confirmed. You can sign in now." }))
      .catch((error: Error) => tell({ ok: false, text: `Could not confirm: ${error.message}.` }));
  } else {
    decided({ now: { kind: "checking" } });
    following = (async () => {
      const response = await post("/api/login/link", { token });
      signedIn((await response.json()) as User);
    })()
      .catch(async (error: Error) => {
        tell({ ok: false, text: `Could not sign in: ${error.message}.` });
        decided({ now: await ask() });
      })
      .finally(() => {
        following = null;
      });
  }
}
