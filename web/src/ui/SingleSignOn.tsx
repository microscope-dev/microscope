import type { JSX } from "@solidjs/web";
import {
  children,
  createMemo,
  createSignal,
  createUniqueId,
  For,
  Match,
  Show,
  Switch,
} from "solid-js";

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
import { createState, useSelector } from "../state";
import { Confirm, Loading, useAction, useLoaded } from "./Orgs";

function useProviders(): () => Providers | null {
  const offered = useFeatures();
  return createMemo(() => offered()?.sso ?? null);
}

function usePasskeys(): () => boolean {
  const offered = useFeatures();
  return createMemo(() => offered()?.passkeys === true && supported());
}

export function ContinueWith(props: { children?: JSX.Element }) {
  const offered = useProviders();
  const resolved = children(() => props.children);
  const visible = createMemo(
    () => resolved.toArray().length > 0 || Boolean(offered()?.google || offered()?.github),
  );
  return (
    <Show when={visible()}>
      <div class="actions continue-with">
        {resolved()}
        <Show when={offered()?.google}>
          <a class="button" href={startUrl("google")} target="_self">
            Continue with Google
          </a>
        </Show>
        <Show when={offered()?.github}>
          <a class="button" href={startUrl("github")} target="_self">
            Continue with GitHub
          </a>
        </Show>
      </div>
    </Show>
  );
}

export const orgLink = createState<{ key: string | null }>(() => ({ key: null }));

function closeOrgLink() {
  orgLink.setState({ key: null });
}

export function OrgLink() {
  const key = useSelector(orgLink, (state) => state.key);
  const now = useAccount();
  const about = useLoaded(
    () => {
      const currentKey = key();
      return currentKey ? samlLink(currentKey) : Promise.resolve(null);
    },
    () => key() ?? "",
  );
  const { working, error, run } = useAction();
  const connectOrg = () => {
    const currentKey = key();
    assert(currentKey !== null, "connecting an organization with no sign-in link");
    run(() => proven("org-link", () => connectSaml(currentKey)));
  };
  const activeKey = createMemo(() => {
    const currentKey = key();
    const kind = now().kind;
    return currentKey && kind !== "checking" && kind !== "unavailable" ? currentKey : null;
  });
  return (
    <Show when={activeKey()}>
      {(currentKey) => (
        <Switch fallback={<p class="muted">Checking the sign-in link…</p>}>
          <Match when={about.error()}>
            {(message) => (
              <div class="banner error" role="alert">
                This single sign-on link does not work: {message()}.{" "}
                <button type="button" class="link-button" onClick={closeOrgLink}>
                  Dismiss
                </button>
              </div>
            )}
          </Match>
          <Match when={about.data()}>
            {(data) => (
              <section aria-label={`Sign in to ${data().org_name}`}>
                <h2>{data().org_name}</h2>
                <Show
                  when={now().kind === "in"}
                  fallback={
                    <p>{data().org_name} signs its members in with its own identity provider.</p>
                  }
                >
                  <p>
                    Connect {data().org_name}&apos;s single sign-on to your account to sign in with
                    it from now on.
                  </p>
                </Show>
                <div class="actions">
                  <Show
                    when={now().kind === "in"}
                    fallback={
                      <a class="button primary" href={samlStartUrl(currentKey())} target="_self">
                        Sign in with {data().org_name}
                      </a>
                    }
                  >
                    <button
                      type="button"
                      class="button primary"
                      disabled={working()}
                      onClick={connectOrg}
                    >
                      Connect {data().org_name}
                    </button>
                  </Show>
                  <button type="button" class="link-button" onClick={closeOrgLink}>
                    Not now
                  </button>
                </div>
                <Show when={error()}>
                  {(message) => (
                    <span class="error" role="alert">
                      {message()}
                    </span>
                  )}
                </Show>
                <ProveItsYou asked="org-link" />
              </section>
            )}
          </Match>
        </Switch>
      )}
    </Show>
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

export function SignInMethods() {
  const offered = useProviders();
  const passkeys = usePasskeys();
  const methods = useLoaded(identities, () => "");
  const { working, error, run } = useAction();
  const [naming, setNaming] = createSignal<string | null>(null);
  const nameId = createUniqueId();
  const known = methods.data;
  const ways = createMemo(() => {
    const current = known();
    return current
      ? current.identities.length + current.passkeys.length + (current.has_password ? 1 : 0)
      : 0;
  });
  const unconnected = createMemo(() => {
    const providers = offered();
    const current = known();
    return (["google", "github"] as const).filter(
      (provider) =>
        providers?.[provider] && !current?.identities.some((each) => each.provider === provider),
    );
  });
  const nothing = createMemo(() => {
    const current = known();
    const providers = offered();
    return (
      (!current && !methods.error()) ||
      (current?.identities.length === 0 &&
        current.passkeys.length === 0 &&
        !providers?.google &&
        !providers?.github &&
        !passkeys())
    );
  });
  const add = (event: SubmitEvent) => {
    event.preventDefault();
    const name = naming()?.trim() ?? "";
    run(() =>
      proven("methods", async () => {
        await addPasskey(name);
        setNaming(null);
        methods.again();
      }),
    );
  };
  const rename = (event: InputEvent & { currentTarget: HTMLInputElement }) =>
    setNaming(event.currentTarget.value);
  const stopNaming = () => setNaming(null);
  const startNaming = () => setNaming(deviceName());
  return (
    <Show when={!nothing()}>
      <h2>Sign-in methods</h2>
      <Show
        when={known()}
        fallback={<Loading error={methods.error()} what="your sign-in methods" />}
      >
        {(current) => (
          <ul class="roster" aria-label="Sign-in methods">
            <Show when={current().has_password}>
              <li>
                <span class="name">Password</span>
              </li>
            </Show>
            <For each={current().identities} keyed={(identity) => identity.id}>
              {(identity) => (
                <li>
                  <span class="name">{describe(identity())}</span>
                  <span class="muted">{identity().email}</span>
                  <Show when={ways() > 1}>
                    <span class="end">
                      <Forget
                        label="Disconnect"
                        question={`Stop signing in with ${describe(identity())}?`}
                        id={identity().id}
                        forget={disconnect}
                        again={methods.again}
                      />
                    </span>
                  </Show>
                </li>
              )}
            </For>
            <For each={current().passkeys} keyed={(passkey) => passkey.id}>
              {(passkey) => (
                <li>
                  <span class="name">Passkey: {passkey().name}</span>
                  <span class="muted">{used(passkey())}</span>
                  <Show when={ways() > 1}>
                    <span class="end">
                      <Forget
                        label="Remove"
                        question={`Stop signing in with ${passkey().name}?`}
                        id={passkey().id}
                        forget={removePasskey}
                        again={methods.again}
                      />
                    </span>
                  </Show>
                </li>
              )}
            </For>
          </ul>
        )}
      </Show>
      <Show when={naming() !== null}>
        <form class="pane-form" aria-label="Add a passkey" onSubmit={add}>
          <div class="field">
            <label for={nameId}>Passkey name</label>
            <input
              id={nameId}
              class="input"
              maxlength={64}
              data-autofocus=""
              value={naming() ?? ""}
              onInput={rename}
            />
          </div>
          <div class="actions">
            <button
              type="submit"
              class="button primary"
              disabled={working() || !(naming() ?? "").trim()}
            >
              Add passkey
            </button>
            <button type="button" class="link-button" onClick={stopNaming}>
              Cancel
            </button>
          </div>
        </form>
      </Show>
      <Show when={unconnected().length > 0 || (passkeys() && naming() === null)}>
        <div class="actions">
          <Show when={passkeys() && naming() === null}>
            <button type="button" class="button" onClick={startNaming}>
              Add a passkey
            </button>
          </Show>
          <For each={unconnected()}>
            {(provider) => <ConnectButton provider={provider} working={working()} run={run} />}
          </For>
        </div>
      </Show>
      <Show when={error()}>
        {(message) => (
          <span class="error" role="alert">
            {message()}
          </span>
        )}
      </Show>
      <ProveItsYou asked="methods" />
    </Show>
  );
}

function Forget(props: {
  label: string;
  question: string;
  id: number;
  forget: (id: number) => Promise<unknown>;
  again: () => void;
}) {
  const act = () =>
    proven("methods", async () => {
      await props.forget(props.id);
      props.again();
    });
  return <Confirm label={props.label} question={props.question} act={act} />;
}

function ConnectButton(props: {
  provider: "google" | "github";
  working: boolean;
  run: (send: () => Promise<unknown>) => void;
}) {
  const onClick = () => props.run(() => proven("methods", () => connect(props.provider)));
  return (
    <button type="button" class="button" disabled={props.working} onClick={onClick}>
      Connect {PROVIDER_LABEL[props.provider]}
    </button>
  );
}

function ProveItsYou(props: { asked: "methods" | "org-link" }) {
  const retry = useSelector(proving, (state) => (state.asked === props.asked ? state.retry : null));
  return <Show when={retry()}>{(fn) => <Prove retry={() => fn()()} />}</Show>;
}

function stopProving() {
  proving.setState({ retry: null, asked: null });
}

function selectAll(event: FocusEvent & { currentTarget: HTMLInputElement }) {
  event.currentTarget.select();
}

function Prove(props: { retry: () => Promise<void> }) {
  const now = useAccount();
  const methods = useLoaded(identities, () => "");
  const [password, setPassword] = createSignal("");
  const { working, error, run } = useAction();
  const id = createUniqueId();
  const known = methods.data;
  const again = createMemo(() => {
    const current = known();
    return (["google", "github"] as const).filter((provider) =>
      current?.identities.some((each) => each.provider === provider),
    );
  });
  const submit = (event: SubmitEvent) => {
    event.preventDefault();
    run(async () => {
      await verify(password()).catch((failure: Error) => {
        if (failure instanceof Refused && failure.status === 401) {
          void checkAccount();
        }
        throw failure;
      });
      stopProving();
      await props.retry();
    });
  };
  const passkey = () =>
    run(async () => {
      const user = await signInWithPasskey();
      const current = now();
      if (user && current.kind === "in" && user.id === current.user.id) {
        stopProving();
        await props.retry();
      } else if (user) {
        stopProving();
      }
    });
  const type = (event: InputEvent & { currentTarget: HTMLInputElement }) =>
    setPassword(event.currentTarget.value);
  const byPasskey = createMemo(() => (known()?.passkeys.length ?? 0) > 0 && supported());
  return (
    <section aria-label="Confirm it's you">
      <Show
        when={known()}
        fallback={
          <div class="pane-form">
            <h2>Confirm it&apos;s you</h2>
            <Loading error={methods.error()} what="your sign-in methods" />
          </div>
        }
      >
        {(current) => (
          <Show
            when={current().has_password}
            fallback={
              <div class="pane-form">
                <h2>Confirm it&apos;s you</h2>
                <p class="muted">
                  {again().length > 0 || byPasskey()
                    ? "Sign in again to change how you sign in."
                    : "Sign out, then sign in again with your organization's link."}
                </p>
                <Show when={error()}>
                  {(message) => (
                    <span class="error" role="alert">
                      {message()}
                    </span>
                  )}
                </Show>
                <div class="actions">
                  <Show when={byPasskey()}>
                    <button
                      type="button"
                      class="button primary"
                      disabled={working()}
                      onClick={passkey}
                    >
                      Sign in again with a passkey
                    </button>
                  </Show>
                  <For each={again()}>
                    {(provider) => (
                      <a class="button primary" href={startUrl(provider)} target="_self">
                        Sign in again with {PROVIDER_LABEL[provider]}
                      </a>
                    )}
                  </For>
                  <button type="button" class="link-button" onClick={stopProving}>
                    Cancel
                  </button>
                </div>
              </div>
            }
          >
            <form class="pane-form" onSubmit={submit}>
              <h2>Confirm it&apos;s you</h2>
              <p class="muted">Changing how you sign in takes your password.</p>
              <div class="field">
                <label for={id}>Password</label>
                <input
                  id={id}
                  class="input"
                  type="password"
                  autocomplete="current-password"
                  data-autofocus=""
                  value={password()}
                  onInput={type}
                />
              </div>
              <Show when={error()}>
                {(message) => (
                  <span class="error" role="alert">
                    {message()}
                  </span>
                )}
              </Show>
              <div class="actions">
                <button type="submit" class="button primary" disabled={working() || !password()}>
                  Confirm
                </button>
                <button type="button" class="link-button" onClick={stopProving}>
                  Cancel
                </button>
              </div>
            </form>
          </Show>
        )}
      </Show>
    </section>
  );
}

function Copyable(props: { label: string; value: string; note?: JSX.Element }) {
  const id = createUniqueId();
  const noteId = createUniqueId();
  const [copied, setCopied] = createSignal(false);
  let field: HTMLInputElement | undefined;
  const copy = () =>
    navigator.clipboard.writeText(props.value).then(
      () => setCopied(true),
      () => field?.focus(),
    );
  return (
    <div class="field">
      <label for={id}>{props.label}</label>
      <div class="link-field">
        <input
          ref={(el) => {
            field = el;
          }}
          id={id}
          readonly
          value={props.value}
          aria-describedby={props.note ? noteId : undefined}
          onFocus={selectAll}
        />
        <button type="button" onClick={copy}>
          {copied() ? "Copied" : "Copy"}
        </button>
      </div>
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

export function OrgSingleSignOn(props: { org: Org }) {
  const offered = useProviders();

  const loaded = useLoaded(
    () => loadOrgSso(props.org.id).then((settings) => ({ settings })),
    () => String(props.org.id),
  );
  const [editing, setEditing] = createSignal(false);
  const { working, error, run } = useAction();
  const setUp = () =>
    run(async () => {
      await createOrgSso(props.org.id);
      loaded.again();
    });
  return (
    <Show when={offered()?.saml}>
      <h2>Single sign-on</h2>
      <Show
        when={loaded.data()}
        fallback={<Loading error={loaded.error()} what="single sign-on" />}
      >
        {(result) => (
          <Show
            when={result().settings}
            fallback={
              <>
                <p class="muted">
                  Let {props.org.name}&apos;s members sign in with your identity provider, such as
                  Okta, Entra ID, or Google Workspace. Anyone who signs in through it joins{" "}
                  {props.org.name}.
                </p>
                <div class="actions">
                  <button type="button" class="button" disabled={working()} onClick={setUp}>
                    Set up single sign-on
                  </button>
                </div>
                <Show when={error()}>
                  {(message) => (
                    <span class="error" role="alert">
                      {message()}
                    </span>
                  )}
                </Show>
              </>
            }
          >
            {(settings) => (
              <Connection
                org={props.org}
                settings={settings()}
                editing={editing() || settings().idp === null}
                edit={setEditing}
                changed={loaded.again}
              />
            )}
          </Show>
        )}
      </Show>
    </Show>
  );
}

function Connection(props: {
  org: Org;
  settings: OrgSso;
  editing: boolean;
  edit: (editing: boolean) => void;
  changed: () => void;
}) {
  const saved = () => {
    props.edit(false);
    props.changed();
  };
  const stopEditing = () => props.edit(false);
  const startEditing = () => props.edit(true);
  const remove = async () => {
    await removeOrgSso(props.org.id);
    props.changed();
  };
  return (
    <>
      <Show
        when={props.settings.idp}
        fallback={
          <p class="muted">
            Add an application to your identity provider with these values, then enter what it gives
            you below.
          </p>
        }
      >
        <p>
          Members sign in at this link. Anyone your identity provider signs in joins{" "}
          {props.org.name} as a member.
        </p>
        <Copyable label="Sign-in link" value={props.settings.sign_in_url} />
      </Show>
      <details class="disclosure" open={!props.settings.idp}>
        <summary>For your identity provider</summary>
        <div class="pane-form">
          <Copyable label="Entity ID (audience)" value={props.settings.sp.entity_id} />
          <Copyable label="ACS URL (reply URL)" value={props.settings.sp.acs_url} />
          <Copyable label="Metadata URL" value={props.settings.sp.metadata_url} />
          <Copyable
            label="Name ID format"
            value={props.settings.sp.name_id_format}
            note="Send each person's email address as the Name ID. In Google Workspace, that is Primary email."
          />
          <p class="muted">
            Optional: map each person&apos;s first and last name to attributes named{" "}
            <code>firstName</code> and <code>lastName</code>, and Microscope will use them.
          </p>
        </div>
      </details>
      <Show
        when={props.editing}
        fallback={
          <Show when={props.settings.idp}>
            {(idp) => (
              <ul class="roster" aria-label="Identity provider">
                <li>
                  <span class="name">{idp().entity_id}</span>
                  <span class="muted">
                    {idp().certificates.length === 1
                      ? "1 certificate"
                      : `${idp().certificates.length} certificates`}
                  </span>
                  <span class="end">
                    <button type="button" class="button small" onClick={startEditing}>
                      Edit
                    </button>
                  </span>
                </li>
              </ul>
            )}
          </Show>
        }
      >
        <IdpForm
          org={props.org}
          settings={props.settings}
          done={saved}
          cancel={props.settings.idp ? stopEditing : null}
          abandon={props.settings.idp ? null : remove}
        />
      </Show>
      <Show when={props.settings.idp}>
        <div class="actions">
          <Confirm
            label="Turn off single sign-on"
            question={`Stop signing members in through ${props.org.name}'s identity provider? They keep their accounts.`}
            act={remove}
          />
        </div>
      </Show>
    </>
  );
}

function IdpForm(props: {
  org: Org;
  settings: OrgSso;
  done: () => void;
  cancel: (() => void) | null;
  abandon: (() => Promise<unknown>) | null;
}) {
  const [entityId, setEntityId] = createSignal(() => props.settings.idp?.entity_id ?? "");
  const [ssoUrl, setSsoUrl] = createSignal(() => props.settings.idp?.sso_url ?? "");
  const [certificate, setCertificate] = createSignal(
    () => props.settings.idp?.certificates[0] ?? "",
  );
  const [next, setNext] = createSignal(() => props.settings.idp?.certificates[1] ?? "");
  const { working, error, run } = useAction();
  const ids = {
    entity: createUniqueId(),
    url: createUniqueId(),
    cert: createUniqueId(),
    next: createUniqueId(),
    nextNote: createUniqueId(),
  };
  const submit = (event: SubmitEvent) => {
    event.preventDefault();
    run(async () => {
      await configureOrgSso(props.org.id, {
        idp_entity_id: entityId(),
        idp_sso_url: ssoUrl(),
        certificates: [certificate(), next()].filter((each) => each.trim()),
      });
      props.done();
    });
  };
  const typeUrl = (event: InputEvent & { currentTarget: HTMLInputElement }) =>
    setSsoUrl(event.currentTarget.value);
  const typeEntity = (event: InputEvent & { currentTarget: HTMLInputElement }) =>
    setEntityId(event.currentTarget.value);
  const typeCertificate = (event: InputEvent & { currentTarget: HTMLTextAreaElement }) =>
    setCertificate(event.currentTarget.value);
  const typeNext = (event: InputEvent & { currentTarget: HTMLTextAreaElement }) =>
    setNext(event.currentTarget.value);
  return (
    <form class="pane-form" onSubmit={submit} aria-label="Identity provider">
      <div class="field">
        <label for={ids.url}>Single sign-on URL</label>
        <input
          id={ids.url}
          class="input"
          type="url"
          spellcheck={false}
          placeholder="https://"
          value={ssoUrl()}
          onInput={typeUrl}
        />
      </div>
      <div class="field">
        <label for={ids.entity}>Identity provider entity ID (issuer)</label>
        <input
          id={ids.entity}
          class="input"
          spellcheck={false}
          value={entityId()}
          onInput={typeEntity}
        />
      </div>
      <div class="field">
        <label for={ids.cert}>Signing certificate</label>
        <textarea
          id={ids.cert}
          class="input"
          rows={4}
          spellcheck={false}
          placeholder="-----BEGIN CERTIFICATE-----"
          value={certificate()}
          onInput={typeCertificate}
        />
      </div>
      <div class="field">
        <label for={ids.next}>Next signing certificate</label>
        <textarea
          id={ids.next}
          class="input"
          rows={2}
          spellcheck={false}
          aria-describedby={ids.nextNote}
          value={next()}
          onInput={typeNext}
        />
        <span id={ids.nextNote} class="note">
          Optional. While your identity provider rotates its certificate, either one signs in.
        </span>
      </div>
      <Show when={error()}>
        {(message) => (
          <span class="error" role="alert">
            {message()}
          </span>
        )}
      </Show>
      <div class="actions">
        <button
          type="submit"
          class="button primary"
          disabled={working() || !entityId().trim() || !ssoUrl().trim() || !certificate().trim()}
        >
          Save
        </button>
        <Show when={props.cancel}>
          {(cancel) => (
            <button type="button" class="link-button" onClick={() => cancel()()}>
              Cancel
            </button>
          )}
        </Show>
        <Show when={props.abandon}>
          {(abandon) => (
            <Confirm
              label="Cancel setup"
              question="Cancel setting up single sign-on? The values you gave your identity provider stop working."
              act={() => abandon()()}
              keep="Keep setting up"
            />
          )}
        </Show>
      </div>
    </form>
  );
}
