// Single sign-on in the Account and Organization panes (sso.ts): the
// buttons that sign in with Google or GitHub, the account's ways in, an
// organization's sign-in link, and the identity provider its owners set up.

import { type FormEvent, type ReactNode, useId, useState } from "react";
import { createStore, useStore } from "zustand";
import { useAccount } from "../account";
import { useFeatures } from "../features";
import type { Org } from "../orgs";
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
  removeOrgSso,
  samlLink,
  samlStartUrl,
  startUrl,
} from "../sso";
import { Confirm, Loading, useAction, useLoaded } from "./Orgs";

/** The providers services offers, or null until it says, or if it cannot. */
function useProviders(): Providers | null {
  return useFeatures()?.sso ?? null;
}

/** Signing in or up with Google or GitHub, for whichever services offers. */
export function ContinueWith() {
  const offered = useProviders();
  if (!offered || (!offered.google && !offered.github)) {
    return null;
  }
  return (
    <div className="actions">
      {offered.google && (
        <a className="button" href={startUrl("google")}>
          Continue with Google
        </a>
      )}
      {offered.github && (
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

/** `/sso#key=`: whose single sign-on it is, and the way through it. Signed
 * in, it connects the organization's identity provider to the account. */
export function OrgLink() {
  const key = useStore(orgLink, (state) => state.key);
  const now = useAccount();
  const about = useLoaded(() => (key ? samlLink(key) : Promise.resolve(null)), key ?? "");
  const { working, error, run } = useAction();
  if (!key || now.kind === "checking" || now.kind === "unavailable") {
    return null;
  }
  const close = () => orgLink.setState({ key: null });
  if (about.error) {
    return (
      <div className="banner error" role="alert">
        This single sign-on link does not work: {about.error}.{" "}
        <button type="button" className="link-button" onClick={close}>
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
        <p>Connect {org}'s single sign-on to your account to sign in with it from now on.</p>
      ) : (
        <p>{org} signs its members in with its own identity provider.</p>
      )}
      <div className="actions">
        {now.kind === "in" ? (
          <button
            type="button"
            className="button primary"
            disabled={working}
            onClick={() => run(() => connectSaml(key))}
          >
            Connect {org}
          </button>
        ) : (
          <a className="button primary" href={samlStartUrl(key)}>
            Sign in with {org}
          </a>
        )}
        <button type="button" className="link-button" onClick={close}>
          Not now
        </button>
      </div>
      {error && (
        <span className="error" role="alert">
          {error}
        </span>
      )}
    </section>
  );
}

function describe(identity: Identity): string {
  return identity.provider === "saml"
    ? `${identity.org_name ?? "An organization"}'s single sign-on`
    : PROVIDER_LABEL[identity.provider];
}

/** The ways into the signed-in account: its password, and each outside
 * account connected to it. The last one cannot go. Absent while there is
 * nothing to connect and nothing connected. */
export function SignInMethods() {
  const offered = useProviders();
  const methods = useLoaded(identities, "");
  const { working, error, run } = useAction();
  const known = methods.data;
  const ways = known ? known.identities.length + (known.has_password ? 1 : 0) : 0;
  const unconnected = (["google", "github"] as const).filter(
    (provider) =>
      offered?.[provider] && !known?.identities.some((each) => each.provider === provider),
  );
  const nothing =
    (!known && !methods.error) ||
    (known?.identities.length === 0 && !offered?.google && !offered?.github);
  if (nothing) {
    return null;
  }
  return (
    <>
      <h2>Sign-in methods</h2>
      {!known ? (
        <Loading error={methods.error} what="your sign-in methods" />
      ) : (
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
                  <Confirm
                    label="Disconnect"
                    question={`Stop signing in with ${describe(identity)}?`}
                    act={async () => {
                      await disconnect(identity.id);
                      methods.again();
                    }}
                  />
                </span>
              )}
            </li>
          ))}
        </ul>
      )}
      {unconnected.length > 0 && (
        <div className="actions">
          {unconnected.map((provider) => (
            <button
              key={provider}
              type="button"
              className="button"
              disabled={working}
              onClick={() => run(() => connect(provider))}
            >
              Connect {PROVIDER_LABEL[provider]}
            </button>
          ))}
        </div>
      )}
      {error && (
        <span className="error" role="alert">
          {error}
        </span>
      )}
    </>
  );
}

/** A value to copy into another system, as it must be pasted. */
function Copyable({ label, value, note }: { label: string; value: string; note?: ReactNode }) {
  const id = useId();
  const noteId = useId();
  const [copied, setCopied] = useState(false);
  return (
    <div className="field">
      <label htmlFor={id}>{label}</label>
      <div className="link-field">
        <input
          id={id}
          readOnly
          value={value}
          aria-describedby={note ? noteId : undefined}
          onFocus={(event) => event.target.select()}
        />
        <button
          type="button"
          onClick={() =>
            navigator.clipboard.writeText(value).then(
              () => setCopied(true),
              () => document.getElementById(id)?.focus(),
            )
          }
        >
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
  if (!offered?.saml) {
    return null;
  }
  return (
    <>
      <h2>Single sign-on</h2>
      {!loaded.data ? (
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
            Let {org.name}'s members sign in with your identity provider, such as Okta, Entra ID, or
            Google Workspace. Anyone who signs in through it joins {org.name}.
          </p>
          <div className="actions">
            <button
              type="button"
              className="button"
              disabled={working}
              onClick={() =>
                run(async () => {
                  await createOrgSso(org.id);
                  loaded.again();
                })
              }
            >
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
  const { org, settings } = props;
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
            Optional: map each person's first and last name to attributes named{" "}
            <code>firstName</code> and <code>lastName</code>, and Microscope will use them.
          </p>
        </div>
      </details>
      {props.editing ? (
        <IdpForm
          org={org}
          settings={settings}
          done={() => {
            props.edit(false);
            props.changed();
          }}
          cancel={settings.idp ? () => props.edit(false) : null}
          abandon={
            settings.idp
              ? null
              : async () => {
                  await removeOrgSso(org.id);
                  props.changed();
                }
          }
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
                <button type="button" className="button small" onClick={() => props.edit(true)}>
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
            act={async () => {
              await removeOrgSso(org.id);
              props.changed();
            }}
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
  const submit = (event: FormEvent) => {
    event.preventDefault();
    run(async () => {
      await configureOrgSso(props.org.id, {
        idp_entity_id: entityId,
        idp_sso_url: ssoUrl,
        certificates: [certificate, next].filter((each) => each.trim()),
      });
      props.done();
    });
  };
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
          onChange={(event) => setSsoUrl(event.target.value)}
        />
      </div>
      <div className="field">
        <label htmlFor={ids.entity}>Identity provider entity ID (issuer)</label>
        <input
          id={ids.entity}
          className="input"
          spellCheck={false}
          value={entityId}
          onChange={(event) => setEntityId(event.target.value)}
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
          onChange={(event) => setCertificate(event.target.value)}
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
          onChange={(event) => setNext(event.target.value)}
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
