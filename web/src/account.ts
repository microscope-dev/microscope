// Accounts on the services backend, which serves a page served on its own
// under the page's own `/api/`: the development server proxies it there
// (vite.config.ts). The session is services' HttpOnly cookie.

import { createStore, useStore } from "zustand";
import { standalone } from "./target";

export interface User {
  id: number;
  username: string;
  email: string;
}

/** What services said went wrong, or why it could not be asked. */
async function failure(response: Response): Promise<Error> {
  const body: unknown = await response.json().catch(() => null);
  const error = (body as { error?: unknown } | null)?.error;
  return new Error(
    typeof error === "string" ? error : `the services backend answered ${response.status}`,
  );
}

/** The signed-in user, or null when nobody is. */
export async function currentUser(): Promise<User | null> {
  const response = await fetch("/api/users/me", { credentials: "same-origin" });
  if (response.status === 401) {
    return null;
  }
  if (!response.ok) {
    throw await failure(response);
  }
  return (await response.json()) as User;
}

export async function signIn(usernameOrEmail: string, password: string): Promise<User> {
  const user = await postLogin(usernameOrEmail, password);
  account.setState({ now: { kind: "in", user } });
  return user;
}

async function postLogin(usernameOrEmail: string, password: string): Promise<User> {
  const response = await fetch("/api/login", {
    method: "POST",
    credentials: "same-origin",
    headers: { "Content-Type": "application/json" },
    body: JSON.stringify({ username_or_email: usernameOrEmail, password }),
  });
  if (!response.ok) {
    throw await failure(response);
  }
  return (await response.json()) as User;
}

export async function signOut(): Promise<void> {
  const response = await fetch("/api/logout", { method: "POST", credentials: "same-origin" });
  if (!response.ok) {
    throw await failure(response);
  }
  account.setState({ now: { kind: "out" } });
}

/** Who is signed in, as far as the page knows. */
export type Account =
  | { readonly kind: "checking" }
  | { readonly kind: "unavailable"; readonly why: string }
  | { readonly kind: "out" }
  | { readonly kind: "in"; readonly user: User };

export const account = createStore<{ now: Account }>(() => ({ now: { kind: "checking" } }));

export function useAccount(): Account {
  return useStore(account, (state) => state.now);
}

/** The question to services in flight, which panes opening together share. */
let asking: Promise<void> | null = null;

/** Asks services who is signed in. A page `microscope web` serves has no
 * services behind it. */
export function checkAccount(): Promise<void> {
  if (!standalone) {
    account.setState({
      now: { kind: "unavailable", why: "this page is served by microscope web, which has none" },
    });
    return Promise.resolve();
  }
  asking ??= currentUser()
    .then((user) => account.setState({ now: user ? { kind: "in", user } : { kind: "out" } }))
    .catch((error: Error) => account.setState({ now: { kind: "unavailable", why: error.message } }))
    .finally(() => {
      asking = null;
    });
  return asking;
}
