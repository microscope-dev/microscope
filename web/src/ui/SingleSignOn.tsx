// Single sign-on in the Account and Organization panes (sso.ts): the
// buttons that sign in with Google or GitHub, the account's ways in, an
// organization's sign-in link, and the identity provider its owners set up.

import {
  type ChangeEvent,
  type FocusEvent,
  type FormEvent,
  type ReactNode,
  useCallback,
  useId,
  useRef,
  useState,
} from "react";
import { createStore, useStore } from "zustand";

import { checkAccount, Refused, useAccount } from "../account";
import { assert } from "../assert";
import { useFeatures } from "../features";
import type { Org } from "../orgs";
import {
  addPasskey,
  deviceName,
  type Passkey,
  removePasskey,
  signInWithPasskey,
  supported,
} from "../passkeys";
import {
  configureOrgSso,
  connect,
  connectSaml,
  createOrgSso,
  disconnect,
  type Identity,
  identities,
  loadOrgSso,
  type OrgSso,
  PROVIDER_LABEL,
  type Providers,
  proven,
  proving,
  removeOrgSso,
  samlLink,
  samlStartUrl,
  startUrl,
  verify,
} from "../sso";
import { Confirm, Loading, useAction, useLoaded } from "./Orgs";

/** The providers services offers, or null until it says, or if it cannot. */
function useProviders(): Providers | null {
  return useFeatures()?.sso ?? null;
}

/** Whether services and this browser can both add passkeys. */
function usePasskeys(): boolean {
  return useFeatures()?.passkeys === true && supported();
}

/** Signing in or up with Google or GitHub, for whichever services offers,
 * after any other way in the form gives as `children`. */
export function ContinueWith({ children }: { children?: ReactNode }) {
  const offered = useProviders();
  if (!children && (!offered || (!offered.google && !offered.github))) {
    return null;
  }
  return (
    <div className="actions continue-with">
      {children}
      {offered?.google && (
        <a className="button" href={startUrl("google")}>
          Continue with Google
        </a>
      )}
      {offered?.github && (
        <a className="button" href={startUrl("github")}>
          Continue with GitHub
        </a>
      )}
    </div>
  );
}

/** The organization sign-in link the page was opened at, until it is used
 * or put away. */
export const orgLink = createStore<{ key: string | null }>(() => ({ key: null }));

function closeOrgLink() {
  orgLink.setState({ key: null });
}

/** `/sso#key=`: whose single sign-on it is, and the way through it. Signed
 * in, it connects the organization's identity provider to the account. */
export function OrgLink() {
  const key = useStore(orgLink, (state) => state.key);
  const now = useAccount();
  const about = useLoaded(() => (key ? samlLink(key) : Promise.resolve(null)), key ?? "");
  const { working, error, run } = useAction();
  const connectOrg = useCallback(() => {
    assert(key !== null, "connecting an organization with no sign-in link");
    run(() => proven("org-link", () => connectSaml(key)));
  }, [run, key]);
  if (!key || now.kind === "checking" || now.kind === "unavailable") {
    return null;
  }
  if (about.error) {
    return (
      <div className="banner error" role="alert">
        This single sign-on link does not work: {about.error}.{" "}
        <button type="button" className="link-button" onClick={closeOrgLink}>
          Dismiss
        </button>
      </div>
    );
  }
  if (!about.data) {
    return <p className="muted">Checking the sign-in link…</p>;
  }
  const org = about.data.org_name;
  return (
    <section aria-label={`Sign in to ${org}`}>
      <h2>{org}</h2>
      {now.kind === "in" ? (
        <p>Connect {org}&apos;s single sign-on to your account to sign in with it from now on.</p>
      ) : (
        <p>{org} signs its members in with its own identity provider.</p>
      )}
      <div className="actions">
        {now.kind === "in" ? (
          <button type="button" className="button primary" disabled={working} onClick={connectOrg}>
            Connect {org}
          </button>
        ) : (
          <a className="button primary" href={samlStartUrl(key)}>
            Sign in with {org}
          </a>
        )}
        <button type="button" className="link-button" onClick={closeOrgLink}>
          Not now
        </button>
      </div>
      {error && (
        <span className="error" role="alert">
          {error}
        </span>
      )}
      <ProveItsYou asked="org-link" />
    </section>
  );
}

function describe(identity: Identity): string {
  return identity.provider === "saml"
    ? `${identity.org_name ?? "An organization"}'s single sign-on`
    : PROVIDER_LABEL[identity.provider];
}

const day = (unix: number) =>
  new Date(unix * 1000).toLocaleDateString(undefined, { dateStyle: "medium" });

function used(passkey: Passkey): string {
  return passkey.last_used_at === null
    ? `Added ${day(passkey.created_at)}`
    : `Last used ${day(passkey.last_used_at)}`;
}

/** The ways into the signed-in account: its password, each outside account
 * connected to it, and its passkeys. The last one cannot go. Absent while
 * there is nothing to add and nothing added. */
export function SignInMethods() {
  const offered = useProviders();
  const passkeys = usePasskeys();
  const methods = useLoaded(identities, "");
  const { working, error, run } = useAction();
  const [naming, setNaming] = useState<string | null>(null);
  const nameId = useId();
  const known = methods.data;
  const ways = known
    ? known.identities.length + known.passkeys.length + (known.has_password ? 1 : 0)
    : 0;
  const unconnected = (["google", "github"] as const).filter(
    (provider) =>
      offered?.[provider] && !known?.identities.some((each) => each.provider === provider),
  );
  const nothing =
    (!known && !methods.error) ||
    (known?.identities.length === 0 &&
      known.passkeys.length === 0 &&
      !offered?.google &&
      !offered?.github &&
      !passkeys);
  const again = methods.again;
  const add = useCallback(
    (event: FormEvent) => {
      event.preventDefault();
      const name = naming?.trim() ?? "";
      run(() =>
        proven("methods", async () => {
          await addPasskey(name);
          setNaming(null);
          again();
        }),
      );
    },
    [naming, run, again],
  );
  const rename = useCallback(
    (event: ChangeEvent<HTMLInputElement>) => setNaming(event.target.value),
    [],
  );
  const stopNaming = useCallback(() => setNaming(null), []);
  const startNaming = useCallback(() => setNaming(deviceName()), []);
  if (nothing) {
    return null;
  }
  return (
    <>
      <h2>Sign-in methods</h2>
      {known ? (
        <ul className="roster" aria-label="Sign-in methods">
          {known.has_password && (
            <li>
              <span className="name">Password</span>
            </li>
          )}
          {known.identities.map((identity) => (
            <li key={identity.id}>
              <span className="name">{describe(identity)}</span>
              <span className="muted">{identity.email}</span>
              {/* The last way in cannot go, so it has no Disconnect. */}
              {ways > 1 && (
                <span className="end">
                  <Forget
                    label="Disconnect"
                    question={`Stop signing in with ${describe(identity)}?`}
                    id={identity.id}
                    forget={disconnect}
                    again={again}
                  />
                </span>
              )}
            </li>
          ))}
          {known.passkeys.map((passkey) => (
            <li key={`passkey-${passkey.id}`}>
              <span className="name">Passkey: {passkey.name}</span>
              <span className="muted">{used(passkey)}</span>
              {ways > 1 && (
                <span className="end">
                  <Forget
                    label="Remove"
                    question={`Stop signing in with ${passkey.name}?`}
                    id={passkey.id}
                    forget={removePasskey}
                    again={again}
                  />
                </span>
              )}
            </li>
          ))}
        </ul>
      ) : (
        <Loading error={methods.error} what="your sign-in methods" />
      )}
      {naming !== null && (
        <form className="pane-form" aria-label="Add a passkey" onSubmit={add}>
          <div className="field">
            <label htmlFor={nameId}>Passkey name</label>
            <input
              id={nameId}
              className="input"
              maxLength={64}
              data-autofocus
              value={naming}
              onChange={rename}
            />
          </div>
          <div className="actions">
            <button type="submit" className="button primary" disabled={working || !naming.trim()}>
              Add passkey
            </button>
            <button type="button" className="link-button" onClick={stopNaming}>
              Cancel
            </button>
          </div>
        </form>
      )}
      {(unconnected.length > 0 || (passkeys && naming === null)) && (
        <div className="actions">
          {passkeys && naming === null && (
            <button type="button" className="button" onClick={startNaming}>
              Add a passkey
            </button>
          )}
          {unconnected.map((provider) => (
            <ConnectButton key={provider} provider={provider} working={working} run={run} />
          ))}
        </div>
      )}
      {error && (
        <span className="error" role="alert">
          {error}
        </span>
      )}
      <ProveItsYou asked="methods" />
    </>
  );
}

/** A way in that can go, once the person confirms it. */
function Forget({
  label,
  question,
  id,
  forget,
  again,
}: {
  label: string;
  question: string;
  id: number;
  forget: (id: number) => Promise<unknown>;
  again: () => void;
}) {
  const act = useCallback(
    () =>
      proven("methods", async () => {
        await forget(id);
        again();
      }),
    [forget, id, again],
  );
  return <Confirm label={label} question={question} act={act} />;
}

function ConnectButton({
  provider,
  working,
  run,
}: {
  provider: "google" | "github";
  working: boolean;
  run: (send: () => Promise<unknown>) => void;
}) {
  const onClick = useCallback(
    () => run(() => proven("methods", () => connect(provider))),
    [run, provider],
  );
  return (
    <button type="button" className="button" disabled={working} onClick={onClick}>
      Connect {PROVIDER_LABEL[provider]}
    </button>
  );
}

/** Asks the person to prove who they are before a change to how they sign
 * in, below the control that asked, and then makes the change. */
function ProveItsYou({ asked }: { asked: "methods" | "org-link" }) {
  const retry = useStore(proving, (state) => (state.asked === asked ? state.retry : null));
  return retry && <Prove retry={retry} />;
}

function stopProving() {
  proving.setState({ retry: null, asked: null });
}

function selectAll(event: FocusEvent<HTMLInputElement>) {
  event.target.select();
}

function Prove({ retry }: { retry: () => Promise<void> }) {
  const now = useAccount();
  const methods = useLoaded(identities, "");
  const [password, setPassword] = useState("");
  const { working, error, run } = useAction();
  const id = useId();
  const known = methods.data;
  const again = (["google", "github"] as const).filter((provider) =>
    known?.identities.some((each) => each.provider === provider),
  );
  const submit = useCallback(
    (event: FormEvent) => {
      event.preventDefault();
      run(async () => {
        // Too many wrong passwords sign the session out, which the pane shows.
        await verify(password).catch((failure: Error) => {
          if (failure instanceof Refused && failure.status === 401) {
            void checkAccount();
          }
          throw failure;
        });
        stopProving();
        await retry();
      });
    },
    [run, password, retry],
  );
  // Signing in again with a passkey makes this session a recent one. The
  // change waits unless it signed in to the same account.
  const passkey = useCallback(
    () =>
      run(async () => {
        const user = await signInWithPasskey();
        if (user && now.kind === "in" && user.id === now.user.id) {
          stopProving();
          await retry();
        } else if (user) {
          stopProving();
        }
      }),
    [run, now, retry],
  );
  const type = useCallback(
    (event: ChangeEvent<HTMLInputElement>) => setPassword(event.target.value),
    [],
  );
  const byPasskey = (known?.passkeys.length ?? 0) > 0 && supported();
  const cancel = (
    <button type="button" className="link-button" onClick={stopProving}>
      Cancel
    </button>
  );
  const heading = <h2>Confirm it&apos;s you</h2>;
  return (
    <section aria-label="Confirm it's you">
      {known === null ? (
        <div className="pane-form">
          {heading}
          <Loading error={methods.error} what="your sign-in methods" />
        </div>
      ) : known.has_password ? (
        <form className="pane-form" onSubmit={submit}>
          {heading}
          <p className="muted">Changing how you sign in takes your password.</p>
          <div className="field">
            <label htmlFor={id}>Password</label>
            <input
              id={id}
              className="input"
              type="password"
              autoComplete="current-password"
              data-autofocus
              value={password}
              onChange={type}
            />
          </div>
          {error && (
            <span className="error" role="alert">
              {error}
            </span>
          )}
          <div className="actions">
            <button type="submit" className="button primary" disabled={working || !password}>
              Confirm
            </button>
            {cancel}
          </div>
        </form>
      ) : (
        <div className="pane-form">
          {heading}
          <p className="muted">
            {again.length > 0 || byPasskey
              ? "Sign in again to change how you sign in."
              : "Sign out, then sign in again with your organization's link."}
          </p>
          {error && (
            <span className="error" role="alert">
              {error}
            </span>
          )}
          <div className="actions">
            {byPasskey && (
              <button type="button" className="button primary" disabled={working} onClick={passkey}>
                Sign in again with a passkey
              </button>
            )}
            {again.map((provider) => (
              <a key={provider} className="button primary" href={startUrl(provider)}>
                Sign in again with {PROVIDER_LABEL[provider]}
              </a>
            ))}
            {cancel}
          </div>
        </div>
      )}
    </section>
  );
}

/** A value to copy into another system, as it must be pasted. */
function Copyable({ label, value, note }: { label: string; value: string; note?: ReactNode }) {
  const id = useId();
  const noteId = useId();
  const [copied, setCopied] = useState(false);
  const field = useRef<HTMLInputElement>(null);
  const copy = useCallback(
    () =>
      navigator.clipboard.writeText(value).then(
        () => setCopied(true),
        () => field.current?.focus(),
      ),
    [value],
  );
  return (
    <div className="field">
      <label htmlFor={id}>{label}</label>
      <div className="link-field">
        <input
          ref={field}
          id={id}
          readOnly
          value={value}
          aria-describedby={note ? noteId : undefined}
          onFocus={selectAll}
        />
        <button type="button" onClick={copy}>
          {copied ? "Copied" : "Copy"}
        </button>
      </div>
      {note && (
        <span id={noteId} className="note">
          {note}
        </span>
      )}
    </div>
  );
}

/** The organization's own identity provider, for its owners to set up. */
export function OrgSingleSignOn({ org }: { org: Org }) {
  const offered = useProviders();
  // Wrapped, since services answers null for an organization without one.
  const loaded = useLoaded(
    () => loadOrgSso(org.id).then((settings) => ({ settings })),
    String(org.id),
  );
  const [editing, setEditing] = useState(false);
  const { working, error, run } = useAction();
  const again = loaded.again;
  const setUp = useCallback(
    () =>
      run(async () => {
        await createOrgSso(org.id);
        again();
      }),
    [run, org.id, again],
  );
  if (!offered?.saml) {
    return null;
  }
  return (
    <>
      <h2>Single sign-on</h2>
      {loaded.data === null ? (
        <Loading error={loaded.error} what="single sign-on" />
      ) : loaded.data.settings ? (
        <Connection
          org={org}
          settings={loaded.data.settings}
          editing={editing || loaded.data.settings.idp === null}
          edit={setEditing}
          changed={loaded.again}
        />
      ) : (
        <>
          <p className="muted">
            Let {org.name}&apos;s members sign in with your identity provider, such as Okta, Entra
            ID, or Google Workspace. Anyone who signs in through it joins {org.name}.
          </p>
          <div className="actions">
            <button type="button" className="button" disabled={working} onClick={setUp}>
              Set up single sign-on
            </button>
          </div>
          {error && (
            <span className="error" role="alert">
              {error}
            </span>
          )}
        </>
      )}
    </>
  );
}

function Connection(props: {
  org: Org;
  settings: OrgSso;
  editing: boolean;
  edit: (editing: boolean) => void;
  changed: () => void;
}) {
  const { org, settings, edit, changed } = props;
  const saved = useCallback(() => {
    edit(false);
    changed();
  }, [edit, changed]);
  const stopEditing = useCallback(() => edit(false), [edit]);
  const startEditing = useCallback(() => edit(true), [edit]);
  const remove = useCallback(async () => {
    await removeOrgSso(org.id);
    changed();
  }, [org.id, changed]);
  return (
    <>
      {settings.idp ? (
        <>
          <p>
            Members sign in at this link. Anyone your identity provider signs in joins {org.name} as
            a member.
          </p>
          <Copyable label="Sign-in link" value={settings.sign_in_url} />
        </>
      ) : (
        <p className="muted">
          Add an application to your identity provider with these values, then enter what it gives
          you below.
        </p>
      )}
      <details className="disclosure" open={!settings.idp}>
        <summary>For your identity provider</summary>
        <div className="pane-form">
          <Copyable label="Entity ID (audience)" value={settings.sp.entity_id} />
          <Copyable label="ACS URL (reply URL)" value={settings.sp.acs_url} />
          <Copyable label="Metadata URL" value={settings.sp.metadata_url} />
          <Copyable
            label="Name ID format"
            value={settings.sp.name_id_format}
            note="Send each person's email address as the Name ID. In Google Workspace, that is Primary email."
          />
          <p className="muted">
            Optional: map each person&apos;s first and last name to attributes named{" "}
            <code>firstName</code> and <code>lastName</code>, and Microscope will use them.
          </p>
        </div>
      </details>
      {props.editing ? (
        <IdpForm
          org={org}
          settings={settings}
          done={saved}
          cancel={settings.idp ? stopEditing : null}
          abandon={settings.idp ? null : remove}
        />
      ) : (
        settings.idp && (
          <ul className="roster" aria-label="Identity provider">
            <li>
              <span className="name">{settings.idp.entity_id}</span>
              <span className="muted">
                {settings.idp.certificates.length === 1
                  ? "1 certificate"
                  : `${settings.idp.certificates.length} certificates`}
              </span>
              <span className="end">
                <button type="button" className="button small" onClick={startEditing}>
                  Edit
                </button>
              </span>
            </li>
          </ul>
        )
      )}
      {/* Until the identity provider is saved, nothing is on to turn off;
          the form's Cancel setup backs out instead. */}
      {settings.idp && (
        <div className="actions">
          <Confirm
            label="Turn off single sign-on"
            question={`Stop signing members in through ${org.name}'s identity provider? They keep their accounts.`}
            act={remove}
          />
        </div>
      )}
    </>
  );
}

function IdpForm(props: {
  org: Org;
  settings: OrgSso;
  done: () => void;
  /** Stops editing a saved identity provider. */
  cancel: (() => void) | null;
  /** Backs out of setting one up for the first time. */
  abandon: (() => Promise<unknown>) | null;
}) {
  const idp = props.settings.idp;
  const [entityId, setEntityId] = useState(idp?.entity_id ?? "");
  const [ssoUrl, setSsoUrl] = useState(idp?.sso_url ?? "");
  const [certificate, setCertificate] = useState(idp?.certificates[0] ?? "");
  const [next, setNext] = useState(idp?.certificates[1] ?? "");
  const { working, error, run } = useAction();
  const ids = { entity: useId(), url: useId(), cert: useId(), next: useId(), nextNote: useId() };
  const orgId = props.org.id;
  const done = props.done;
  const submit = useCallback(
    (event: FormEvent) => {
      event.preventDefault();
      run(async () => {
        await configureOrgSso(orgId, {
          idp_entity_id: entityId,
          idp_sso_url: ssoUrl,
          certificates: [certificate, next].filter((each) => each.trim()),
        });
        done();
      });
    },
    [run, orgId, entityId, ssoUrl, certificate, next, done],
  );
  const typeUrl = useCallback(
    (event: ChangeEvent<HTMLInputElement>) => setSsoUrl(event.target.value),
    [],
  );
  const typeEntity = useCallback(
    (event: ChangeEvent<HTMLInputElement>) => setEntityId(event.target.value),
    [],
  );
  const typeCertificate = useCallback(
    (event: ChangeEvent<HTMLTextAreaElement>) => setCertificate(event.target.value),
    [],
  );
  const typeNext = useCallback(
    (event: ChangeEvent<HTMLTextAreaElement>) => setNext(event.target.value),
    [],
  );
  return (
    <form className="pane-form" onSubmit={submit} aria-label="Identity provider">
      <div className="field">
        <label htmlFor={ids.url}>Single sign-on URL</label>
        <input
          id={ids.url}
          className="input"
          type="url"
          spellCheck={false}
          placeholder="https://"
          value={ssoUrl}
          onChange={typeUrl}
        />
      </div>
      <div className="field">
        <label htmlFor={ids.entity}>Identity provider entity ID (issuer)</label>
        <input
          id={ids.entity}
          className="input"
          spellCheck={false}
          value={entityId}
          onChange={typeEntity}
        />
      </div>
      <div className="field">
        <label htmlFor={ids.cert}>Signing certificate</label>
        <textarea
          id={ids.cert}
          className="input"
          rows={4}
          spellCheck={false}
          placeholder="-----BEGIN CERTIFICATE-----"
          value={certificate}
          onChange={typeCertificate}
        />
      </div>
      <div className="field">
        <label htmlFor={ids.next}>Next signing certificate</label>
        <textarea
          id={ids.next}
          className="input"
          rows={2}
          spellCheck={false}
          aria-describedby={ids.nextNote}
          value={next}
          onChange={typeNext}
        />
        <span id={ids.nextNote} className="note">
          Optional. While your identity provider rotates its certificate, either one signs in.
        </span>
      </div>
      {error && (
        <span className="error" role="alert">
          {error}
        </span>
      )}
      <div className="actions">
        <button
          type="submit"
          className="button primary"
          disabled={working || !entityId.trim() || !ssoUrl.trim() || !certificate.trim()}
        >
          Save
        </button>
        {props.cancel && (
          <button type="button" className="link-button" onClick={props.cancel}>
            Cancel
          </button>
        )}
        {props.abandon && (
          <Confirm
            label="Cancel setup"
            question="Cancel setting up single sign-on? The values you gave your identity provider stop working."
            act={props.abandon}
            keep="Keep setting up"
          />
        )}
      </div>
    </form>
  );
}
