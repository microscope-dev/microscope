// Passkeys through services (account.ts): adding one to the signed-in
// account, and signing in with one. Services makes each challenge and checks
// the answer; the browser and the person's device or password manager make
// and keep the key.
//
// Services sends WebAuthn's options as JSON, with bytes as base64url, and
// takes the answer the same way. The browser's own JSON helpers are newer
// than some browsers people still use, so the bytes are converted here.
//
// Adding one changes how the account signs in, so it goes through `proven`
// like connecting Google does.

import { passkeySignedIn, type User } from "./account";
import { request } from "./orgs";

/** A passkey on the account, as its sign-in methods list it. */
export interface Passkey {
  id: number;
  name: string;
  /** Whether it is synced, such as by iCloud Keychain or a password manager. */
  backed_up: boolean;
  /** Unix seconds. */
  created_at: number;
  last_used_at: number | null;
}

/** Whether this browser can make and use passkeys at all. */
export const supported = () => typeof window.PublicKeyCredential === "function";

const fromB64 = (text: string): ArrayBuffer => {
  const binary = atob(text.replaceAll("-", "+").replaceAll("_", "/"));
  return Uint8Array.from(binary, (char) => char.codePointAt(0) as number).buffer;
};

const toB64 = (bytes: ArrayBuffer): string =>
  btoa(String.fromCodePoint(...new Uint8Array(bytes)))
    .replaceAll("+", "-")
    .replaceAll("/", "_")
    .replace(/=+$/u, "");

interface Descriptor {
  type: "public-key";
  id: string;
}

interface CreationJson {
  challenge: string;
  user: { id: string; name: string; displayName: string };
  excludeCredentials: Descriptor[];
  [rest: string]: unknown;
}

interface RequestJson {
  challenge: string;
  [rest: string]: unknown;
}

/** What the browser threw, in words for the person. */
function refusal(error: unknown): unknown {
  if (error instanceof DOMException) {
    if (error.name === "NotAllowedError") {
      return new Error("no passkey was used");
    }
    if (error.name === "InvalidStateError") {
      return new Error("this device already has a passkey for your account");
    }
  }
  return error;
}

const descriptor = (each: Descriptor): PublicKeyCredentialDescriptor => ({
  type: each.type,
  id: fromB64(each.id),
});

/** A guess at what to call a passkey made on this device. */
export function deviceName(): string {
  const agent = navigator.userAgent;
  for (const [pattern, name] of [
    [/iPhone/u, "iPhone"],
    [/iPad/u, "iPad"],
    [/Android/u, "Android"],
    [/Macintosh|Mac OS X/u, "Mac"],
    [/Windows/u, "Windows"],
    [/CrOS/u, "Chromebook"],
    [/Linux/u, "Linux"],
  ] as const) {
    if (pattern.test(agent)) {
      return name;
    }
  }
  return "Passkey";
}

/** Makes a passkey on this device and adds it to the signed-in account. */
export async function addPasskey(name: string): Promise<void> {
  const options = await request<CreationJson>("POST", "/api/passkeys/register/start", {});
  const made = (await navigator.credentials
    .create({
      publicKey: {
        ...(options as unknown as PublicKeyCredentialCreationOptions),
        challenge: fromB64(options.challenge),
        user: { ...options.user, id: fromB64(options.user.id) },
        excludeCredentials: options.excludeCredentials.map(descriptor),
      },
    })
    .catch((error: unknown) => {
      throw refusal(error);
    })) as PublicKeyCredential | null;
  if (!made) {
    throw new Error("no passkey was made");
  }
  const response = made.response as AuthenticatorAttestationResponse;
  await request("POST", "/api/passkeys/register/finish", {
    credential_id: toB64(made.rawId),
    client_data_json: toB64(response.clientDataJSON),
    attestation_object: toB64(response.attestationObject),
    name,
  });
}

export const removePasskey = (id: number) =>
  request<void>("DELETE", `/api/users/me/passkeys/${id}`);

/** Whether the browser can offer passkeys in the username field's
 * autofill, alongside saved passwords. */
export async function autofillAvailable(): Promise<boolean> {
  return (
    supported() &&
    typeof PublicKeyCredential.isConditionalMediationAvailable === "function" &&
    (await PublicKeyCredential.isConditionalMediationAvailable())
  );
}

/** Signs in with a passkey the person picks. With `autofill`, the browser
 * offers them in the username field and waits until one is picked or
 * `signal` stops it, which answers null. */
export async function signInWithPasskey(
  options: { autofill?: boolean; signal?: AbortSignal } = {},
): Promise<User | null> {
  const asked = await request<RequestJson>("POST", "/api/passkeys/sign-in/start", {});
  let picked: PublicKeyCredential | null;
  try {
    picked = (await navigator.credentials.get({
      publicKey: {
        ...(asked as unknown as PublicKeyCredentialRequestOptions),
        challenge: fromB64(asked.challenge),
      },
      ...(options.autofill ? { mediation: "conditional" as CredentialMediationRequirement } : {}),
      ...(options.signal ? { signal: options.signal } : {}),
    })) as PublicKeyCredential | null;
  } catch (error) {
    if (options.signal?.aborted) {
      return null;
    }
    throw refusal(error);
  }
  if (!picked) {
    return null;
  }
  const response = picked.response as AuthenticatorAssertionResponse;
  return passkeySignedIn({
    credential_id: toB64(picked.rawId),
    client_data_json: toB64(response.clientDataJSON),
    authenticator_data: toB64(response.authenticatorData),
    signature: toB64(response.signature),
    user_handle: response.userHandle ? toB64(response.userHandle) : null,
  });
}
