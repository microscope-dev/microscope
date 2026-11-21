import { Refused } from "./account";
import { request } from "./orgs";
import type { Passkey } from "./passkeys";
import { createState } from "./state";

export type Provider = "google" | "github" | "saml";

export const PROVIDER_LABEL: Record<Provider, string> = {
  google: "Google",
  github: "GitHub",
  saml: "Single sign-on",
};

export type Providers = Record<Provider, boolean>;

export interface Identity {
  id: number;
  provider: Provider;
  email: string;

  org_id: number | null;
  org_name: string | null;
}

export interface Identities {
  has_password: boolean;
  identities: Identity[];
  passkeys: Passkey[];
}

export interface OrgSso {
  sign_in_url: string;
  sp: { entity_id: string; acs_url: string; metadata_url: string; name_id_format: string };
  idp: { entity_id: string; sso_url: string; certificates: string[] } | null;
}

export const startUrl = (provider: "google" | "github") => `/api/sso/${provider}/start`;

export const samlStartUrl = (key: string) => `/api/sso/saml/${key}/start`;

export const identities = () => request<Identities>("GET", "/api/users/me/identities");

export const disconnect = (id: number) => request<void>("DELETE", `/api/users/me/identities/${id}`);

async function connectAt(path: string): Promise<void> {
  const { url } = await request<{ url: string }>("POST", path);
  window.location.assign(url);
}

export const connect = (provider: "google" | "github") => connectAt(`/api/sso/${provider}/connect`);

export const connectSaml = (key: string) => connectAt(`/api/sso/saml/${key}/connect`);

const NOT_VERIFIED = "confirm it is you first";

export const verify = (password: string) =>
  request<void>("POST", "/api/users/me/verify", { password });

export type Asked = "methods" | "org-link";

export const proving = createState<{ retry: (() => Promise<void>) | null; asked: Asked | null }>(
  () => ({ retry: null, asked: null }),
);

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

export const samlLink = (key: string) =>
  request<{ org_name: string }>("GET", `/api/sso/saml/${encodeURIComponent(key)}`);

const orgSso = (id: number) => `/api/orgs/${id}/sso`;

export const loadOrgSso = (id: number) => request<OrgSso | null>("GET", orgSso(id));

export const createOrgSso = (id: number) => request<OrgSso>("POST", orgSso(id));

export const configureOrgSso = (
  id: number,
  idp: { idp_entity_id: string; idp_sso_url: string; certificates: string[] },
) => request<OrgSso>("PUT", orgSso(id), idp);

export const removeOrgSso = (id: number) => request<void>("DELETE", orgSso(id));
