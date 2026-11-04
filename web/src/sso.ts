// Single sign-on through services (account.ts): Google, GitHub, and each
// organization's own SAML identity provider. Every sign-in leaves the page
// for the provider and comes back to `/signed-in` (followSignIn).
//
// Signing in or up goes straight to services' `/api/sso/…/start`, a plain
// link. Connecting another way in to a signed-in account asks services
// first, which binds the sign-in to that account, and then leaves.
//
// Services' server operator chooses which providers are on.
//
// Connecting or disconnecting one takes a session that proved who holds it
// in the last ten minutes, by signing in or by the password, so a stolen
// session cookie alone cannot add a way in. Without that, services refuses,
// and `proven` asks the person to prove it and then tries again.

import { createStore } from "zustand";
import { Refused } from "./account";
import { request } from "./orgs";
import type { Passkey } from "./passkeys";

export type Provider = "google" | "github" | "saml";

export const PROVIDER_LABEL: Record<Provider, string> = {
  google: "Google",
  github: "GitHub",
  saml: "Single sign-on",
};

export type Providers = Record<Provider, boolean>;

/** One outside account that signs in to the person's. */
export interface Identity {
  id: number;
  provider: Provider;
  email: string;
  /** The organization whose identity provider it is, for SAML. */
  org_id: number | null;
  org_name: string | null;
}

/** The ways into the signed-in account. */
export interface Identities {
  has_password: boolean;
  identities: Identity[];
  passkeys: Passkey[];
}

/** What an organization's owners paste into their identity provider, and
 * what they took from it. */
export interface OrgSso {
  sign_in_url: string;
  sp: { entity_id: string; acs_url: string; metadata_url: string; name_id_format: string };
  idp: { entity_id: string; sso_url: string; certificates: string[] } | null;
}

/** Where a sign-in through `provider` starts, for a link. */
export const startUrl = (provider: "google" | "github") => `/api/sso/${provider}/start`;

export const samlStartUrl = (key: string) => `/api/sso/saml/${key}/start`;

export const identities = () => request<Identities>("GET", "/api/users/me/identities");

export const disconnect = (id: number) => request<void>("DELETE", `/api/users/me/identities/${id}`);

/** Leaves for the provider to connect it to the signed-in account. */
async function connectAt(path: string): Promise<void> {
  const { url } = await request<{ url: string }>("POST", path);
  window.location.assign(url);
}

export const connect = (provider: "google" | "github") => connectAt(`/api/sso/${provider}/connect`);

export const connectSaml = (key: string) => connectAt(`/api/sso/saml/${key}/connect`);

/** What services answers a change that needs a recent proof. */
const NOT_VERIFIED = "confirm it is you first";

/** Proves who holds this session by the account's password. */
export const verify = (password: string) =>
  request<void>("POST", "/api/users/me/verify", { password });

/** Where the person asked for a change that is waiting on a proof. */
export type Asked = "methods" | "org-link";

/** The change waiting for the person to prove who they are, if any. */
export const proving = createStore<{ retry: (() => Promise<void>) | null; asked: Asked | null }>(
  () => ({ retry: null, asked: null }),
);

/** Does `act`, or, when services wants a recent proof first, keeps it to
 * try again once the person gives one. */
export async function proven(asked: Asked, act: () => Promise<void>): Promise<void> {
  try {
    await act();
  } catch (error) {
    if (error instanceof Refused && error.status === 403 && error.message === NOT_VERIFIED) {
      proving.setState({ retry: act, asked });
      return;
    }
    throw error;
  }
}

/** Which organization an `/sso#key=` link signs in to. */
export const samlLink = (key: string) =>
  request<{ org_name: string }>("GET", `/api/sso/saml/${encodeURIComponent(key)}`);

const orgSso = (id: number) => `/api/orgs/${id}/sso`;

/** The organization's connection, or null before an owner sets it up. */
export const loadOrgSso = (id: number) => request<OrgSso | null>("GET", orgSso(id));

export const createOrgSso = (id: number) => request<OrgSso>("POST", orgSso(id));

export const configureOrgSso = (
  id: number,
  idp: { idp_entity_id: string; idp_sso_url: string; certificates: string[] },
) => request<OrgSso>("PUT", orgSso(id), idp);

export const removeOrgSso = (id: number) => request<void>("DELETE", orgSso(id));
