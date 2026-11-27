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
  untrack,
} from "solid-js";

import { type Account, checkAccount, useAccount } from "../account";
import { KIND } from "../documents";
import {
  acceptInvitation,
  addToTeam,
  cancelInvitation,
  createOrg,
  createProject,
  createTeam,
  declineInvitation,
  deleteOrg,
  deleteProject,
  deleteTeam,
  directory as directoryStore,
  GRANTS,
  type Grant,
  type Invitation,
  grantTeam,
  groupProjects,
  invite,
  listInvitations,
  listMembers,
  listTeams,
  type Member,
  type Org,
  orgLabel,
  PERMISSION_LABEL,
  PERMISSIONS,
  type Permission,
  type Project,
  type ProjectTeam,
  projectTeams,
  ROLE_LABEL,
  ROLES,
  type Role,
  reload,
  removeFromTeam,
  removeMember,
  renameProject,
  renameTeam,
  revokeTeam,
  setRole,
  type Team,
  type TeamMember,
  teamMembers,
  updateOrg,
  useDirectory,
} from "../orgs";
import { createState, useSelector } from "../state";
import type { PaneProps } from "../workbench/kinds";
import { close, type Json, type Pane, type PaneId, setParams } from "../workbench/layout";
import { change } from "../workbench/store";
import { showOrg, showPane } from "./panes";
import { OrgSingleSignOn } from "./SingleSignOn";

export const asked = createState<{ form: "project" | "org" | null }>(() => ({ form: null }));

function useAsked(form: "project" | "org", open: () => void): void {
  const wanted = useSelector(asked, (state) => state.form === form);
  createEffect(
    () => wanted(),
    (isWanted) => {
      if (isWanted) {
        asked.setState({ form: null });
        open();
      }
    },
  );
}

function orgParams(pane: Pane): { name: string; section: Section } {
  const params = (pane.params ?? {}) as { name?: unknown; section?: unknown };
  return {
    name: typeof params.name === "string" ? params.name : "",
    section: SECTIONS.find((each) => each === params.section) ?? "members",
  };
}

export const orgName = (pane: Pane): string => orgParams(pane).name;

type Run = { kind: "idle" } | { kind: "working"; id: number } | { kind: "failed"; error: Error };

export function useRun(): {
  working: () => boolean;
  error: () => Error | null;
  run: (send: () => Promise<unknown>) => void;
  fail: (error: Error) => void;
} {
  const [now, setNow] = createSignal<Run>({ kind: "idle" });
  let newest = 0;
  const run = (send: () => Promise<unknown>) => {
    newest += 1;
    const id = newest;
    setNow({ kind: "working", id });
    send().then(
      () => newest === id && setNow({ kind: "idle" }),
      (error: Error) => newest === id && setNow({ kind: "failed", error }),
    );
  };
  const fail = (error: Error) => setNow({ kind: "failed", error });
  return {
    working: createMemo(() => now().kind === "working"),
    error: createMemo(() => {
      const current = now();
      return current.kind === "failed" ? current.error : null;
    }),
    run,
    fail,
  };
}

export function useAction(): {
  working: () => boolean;
  error: () => string | null;
  run: (send: () => Promise<unknown>) => void;
} {
  const { working, error, run } = useRun();
  return {
    working,
    error: createMemo(() => error()?.message ?? null),
    run,
  };
}

/**
 * Fetches async data keyed by `key()` with stale-response cancellation. Passing `keepFor()`
 * preserves the previous data across sub-key changes sharing the same `keepFor` value to avoid
 * flashing a loading state.
 */
export function useLoaded<T>(
  load: () => Promise<T>,
  key: () => string,
  keepFor?: () => string | undefined,
): { data: () => T | null; error: () => string | null; again: () => void } {
  const [loaded, setLoaded] = createSignal<{
    key: string | undefined;
    keepFor: string | undefined;
    data: T | null;
    error: string | null;
  }>({
    key: undefined,
    keepFor: undefined,
    data: null,
    error: null,
  });
  const [round, setRound] = createSignal(0);
  const currentKey = createMemo(() => key());
  const currentKeepFor = createMemo(() => keepFor?.());
  createEffect(
    () => [currentKey(), currentKeepFor(), round()] as const,
    ([nextKey, nextKeepFor]) => {
      let live = true;
      untrack(load).then(
        (data) => live && setLoaded({ key: nextKey, keepFor: nextKeepFor, data, error: null }),
        (failure: Error) =>
          live &&
          setLoaded((before) => ({
            key: nextKey,
            keepFor: nextKeepFor,
            data: before.key === nextKey ? before.data : null,
            error: failure.message,
          })),
      );
      return () => {
        live = false;
      };
    },
  );
  const again = () => setRound((before) => before + 1);
  const data = createMemo(() => {
    const state = loaded();
    const k = currentKey();
    const keep = currentKeepFor();
    const current = state.key === k;
    const kept = keep !== undefined && state.keepFor === keep;
    return current || kept ? state.data : null;
  });
  const error = createMemo(() => {
    const state = loaded();
    return state.key === currentKey() ? state.error : null;
  });
  return { data, error, again };
}

function showAccount() {
  showPane(KIND.account);
}

const NO_ORGS: readonly Org[] = [];
const NO_PROJECTS: readonly Project[] = [];
const NO_MEMBERS: readonly Member[] = [];

function SignedOut(props: { now: Account; what: string }) {
  const unavailable = createMemo(() => (props.now.kind === "unavailable" ? props.now : null));
  return (
    <Show when={props.now.kind !== "in"}>
      <div class="pane-body pane-text">
        <Switch
          fallback={
            <>
              <p>
                Sign in to see {props.what.toLowerCase()}. Connecting to a microscope needs no
                account.
              </p>
              <div class="actions">
                <button type="button" class="button" onClick={showAccount}>
                  Sign in
                </button>
              </div>
            </>
          }
        >
          <Match when={props.now.kind === "checking"}>
            <p class="muted">Checking whether you are signed in…</p>
          </Match>
          <Match when={unavailable()}>
            {(current) => (
              <p class="muted">
                {props.what} need the services backend, which is unavailable ({current().why}).
              </p>
            )}
          </Match>
        </Switch>
      </div>
    </Show>
  );
}

function NameForm(props: {
  label: string;
  initial?: string;
  verb: string;
  note?: string;
  send: (name: string) => Promise<unknown>;
  done: () => void;
  children?: JSX.Element;
}) {
  const id = createUniqueId();
  let input: HTMLInputElement | undefined;
  const [name, setName] = createSignal(() => props.initial ?? "");
  const { working, error, run } = useAction();
  onSettled(() => {
    input?.focus();
  });
  const submit = (event: SubmitEvent) => {
    event.preventDefault();
    run(async () => {
      await props.send(name().trim());
      props.done();
    });
  };
  const type = (event: InputEvent & { currentTarget: HTMLInputElement }) =>
    setName(event.currentTarget.value);
  const escape = (event: KeyboardEvent) => {
    if (event.key === "Escape") {
      event.stopPropagation();
      props.done();
    }
  };
  const unchanged = createMemo(() => name().trim() === "" || name().trim() === props.initial);
  return (
    <form class="pane-form wide" onSubmit={submit}>
      <div class="field">
        <label for={id}>{props.label}</label>
        <input
          ref={(el) => {
            input = el;
          }}
          id={id}
          class="input"
          value={name()}
          maxlength={64}
          spellcheck={false}
          autocomplete="off"
          data-autofocus=""
          aria-invalid={error() === null ? "false" : "true"}
          onInput={type}
          onKeyDown={escape}
        />
        <Show when={props.note}>{(note) => <span class="note">{note()}</span>}</Show>
      </div>
      {props.children}
      <Show when={error()}>
        {(message) => (
          <span class="error" role="alert">
            {message()}
          </span>
        )}
      </Show>
      <div class="actions">
        <button type="submit" class="button primary" disabled={working() || unchanged()}>
          {props.verb}
        </button>
        <button type="button" class="link-button" onClick={() => props.done()}>
          Cancel
        </button>
      </div>
    </form>
  );
}

export function Confirm(props: {
  label: string;
  question: string;
  act: () => Promise<unknown>;
  keep?: string;
}) {
  const [asking, setAsking] = createSignal(false);
  const { working, error, run } = useAction();
  const ask = () => setAsking(true);
  const confirm = () => run(() => props.act());
  const keep = () => setAsking(false);
  return (
    <Show
      when={asking()}
      fallback={
        <button type="button" class="button small" onClick={ask}>
          {props.label}
        </button>
      }
    >
      <span class="confirm">
        <span>{props.question}</span>
        <button type="button" class="button small danger" disabled={working()} onClick={confirm}>
          {props.label}
        </button>
        <button type="button" class="link-button" onClick={keep}>
          {props.keep ?? "Cancel"}
        </button>
        <Show when={error()}>
          {(message) => (
            <span class="error" role="alert">
              {message()}
            </span>
          )}
        </Show>
      </span>
    </Show>
  );
}

function Choice<T extends string>(props: {
  label: string;
  value: T;
  options: readonly T[];
  names: Record<T, string>;
  send: (value: T) => Promise<unknown>;
}) {
  const { working, error, run } = useAction();
  const choose = (event: Event & { currentTarget: HTMLSelectElement }) => {
    const value = event.currentTarget.value as T;
    run(() => props.send(value));
  };
  return (
    <>
      <select
        class="input compact"
        aria-label={props.label}
        value={props.value}
        disabled={working()}
        onChange={choose}
      >
        <For each={props.options}>
          {(option) => (
            <option value={option} selected={option === props.value}>
              {props.names[option]}
            </option>
          )}
        </For>
      </select>
      <Show when={error()}>
        {(message) => (
          <span class="error" role="alert">
            {message()}
          </span>
        )}
      </Show>
    </>
  );
}

export function Loading(props: { error: string | null; what: string }) {
  return (
    <Show when={props.error} fallback={<p class="muted">Loading {props.what}…</p>}>
      {(message) => (
        <p class="error" role="alert">
          Could not load {props.what}: {message()}.
        </p>
      )}
    </Show>
  );
}

function Stale(props: { error: string | null; what: string }) {
  return (
    <Show when={props.error}>
      {(message) => (
        <p class="error" role="alert">
          Could not refresh {props.what}: {message()}.
        </p>
      )}
    </Show>
  );
}

export function ProjectsBody() {
  const now = useAccount();
  const directory = useDirectory();
  const [creating, setCreating] = createSignal(false);
  onSettled(() => {
    void checkAccount();
  });
  const startCreating = () => setCreating(true);
  const stopCreating = () => setCreating(false);
  useAsked("project", startCreating);
  const owned = createMemo(() => (directory().orgs ?? []).filter((org) => org.role === "owner"));
  const grouped = createMemo(() => {
    const current = directory();
    return current.orgs !== null && current.projects !== null
      ? { orgs: current.orgs, groups: groupProjects(current.projects, current.orgs) }
      : null;
  });
  return (
    <Show when={now().kind === "in"} fallback={<SignedOut now={now()} what="Projects" />}>
      <div class="pane-body pane-text projects">
        <Show
          when={creating()}
          fallback={
            <div class="actions">
              <button type="button" class="button" onClick={startCreating}>
                New project
              </button>
            </div>
          }
        >
          <NewProject owners={owned()} done={stopCreating} />
        </Show>
        <Show
          when={grouped()}
          fallback={<Loading error={directory().error} what="your projects" />}
        >
          {(loaded) => (
            <>
              <Stale error={directory().error} what="your projects" />
              <For each={loaded().groups} keyed={(group) => group.org?.id ?? "own"}>
                {(group) => (
                  <section class="project-group">
                    <div class="group-head">
                      <h3>
                        {(() => {
                          const org = group().org;
                          return org ? orgLabel(org, loaded().orgs) : "Yours";
                        })()}
                      </h3>
                      <Show when={group().org}>
                        {(org) => (
                          <ShowOrgButton
                            org={org()}
                            class="link-button"
                            label={`Open ${orgLabel(org(), loaded().orgs)}`}
                          >
                            Open organization
                          </ShowOrgButton>
                        )}
                      </Show>
                    </div>
                    <Show
                      when={group().projects.length > 0}
                      fallback={
                        <Show when={!directory().error}>
                          <p class="muted">
                            {(() => {
                              const org = group().org;
                              return org === null
                                ? "None of your own yet."
                                : org.role === "owner"
                                  ? "No projects yet."
                                  : "No projects here that you can see.";
                            })()}
                          </p>
                        </Show>
                      }
                    >
                      <ul
                        class="roster"
                        aria-label={`${(() => {
                          const org = group().org;
                          return org ? orgLabel(org, loaded().orgs) : "Your";
                        })()} projects`}
                      >
                        <For each={group().projects} keyed={(project) => project.id}>
                          {(project) => <ProjectRow project={project()} />}
                        </For>
                      </ul>
                    </Show>
                  </section>
                )}
              </For>
            </>
          )}
        </Show>
      </div>
    </Show>
  );
}

function ShowOrgButton(props: {
  org: { id: number; name: string };
  class: string;
  label?: string;
  children: JSX.Element;
}) {
  const onClick = () => showOrg(props.org.id, props.org.name);
  return (
    <button type="button" class={props.class} aria-label={props.label} onClick={onClick}>
      {props.children}
    </button>
  );
}

function NewProject(props: { owners: readonly Org[]; fixed?: Org; done: () => void }) {
  const ownerId = createUniqueId();
  const orgs = useSelector(directoryStore, (state) => state.orgs);
  const [owner, setOwner] = createSignal(() => (props.fixed ? String(props.fixed.id) : ""));
  const send = async (name: string) => {
    const selected = owner();
    await createProject(name, selected === "" ? null : Number(selected));
    await reload();
  };
  const choose = (event: Event & { currentTarget: HTMLSelectElement }) =>
    setOwner(event.currentTarget.value);
  return (
    <NameForm label="Project name" verb="Create project" send={send} done={props.done}>
      <Show when={!props.fixed && props.owners.length > 0}>
        <div class="field">
          <label for={ownerId}>Owner</label>
          <select id={ownerId} class="input" value={owner()} onChange={choose}>
            <option value="" selected={owner() === ""}>
              You
            </option>
            <For each={props.owners} keyed={(org) => org.id}>
              {(org) => (
                <option value={org().id} selected={String(org().id) === owner()}>
                  {orgLabel(org(), orgs())}
                </option>
              )}
            </For>
          </select>
        </div>
      </Show>
    </NameForm>
  );
}

function ProjectRow(props: { project: Project; org?: Org }) {
  const [renaming, setRenaming] = createSignal(false);
  const [teams, setTeams] = createSignal(false);
  const admin = createMemo(() => props.project.permission === "admin");
  const rename = async (name: string) => {
    await renameProject(props.project.id, name);
    await reload();
  };
  const remove = async () => {
    await deleteProject(props.project.id);
    await reload();
  };
  const startRenaming = () => setRenaming(true);
  const stopRenaming = () => setRenaming(false);
  const toggleTeams = () => setTeams((open) => !open);
  return (
    <li>
      <Show
        when={renaming()}
        fallback={
          <>
            <span class="name">{props.project.name}</span>
            <span class="muted" title="Your access">
              {PERMISSION_LABEL[props.project.permission]}
            </span>
            <span class="end">
              <Show when={props.org}>
                <button
                  type="button"
                  class="button small"
                  aria-expanded={teams() ? "true" : "false"}
                  onClick={toggleTeams}
                >
                  Team access
                </button>
              </Show>
              <Show when={admin()}>
                <button type="button" class="button small" onClick={startRenaming}>
                  Rename
                </button>
                <Confirm
                  label="Delete"
                  question={`Delete ${props.project.name}? This cannot be undone.`}
                  act={remove}
                />
              </Show>
            </span>
          </>
        }
      >
        <NameForm
          label={`New name for ${props.project.name}`}
          initial={props.project.name}
          verb="Rename"
          send={rename}
          done={stopRenaming}
        />
      </Show>
      <Show when={props.org && teams() ? props.org : null}>
        {(org) => <ProjectTeams project={props.project} org={org()} />}
      </Show>
    </li>
  );
}

function ProjectTeams(props: { project: Project; org: Org }) {
  const admin = createMemo(() => props.project.permission === "admin");
  const granted = useLoaded(
    () => projectTeams(props.project.id),
    () => String(props.project.id),
  );
  const teams = useLoaded(
    () => (admin() ? listTeams(props.org.id) : Promise.resolve([])),
    () => `${props.org.id}:${admin()}`,
  );
  const teamId = createUniqueId();
  const grantId = createUniqueId();
  const [team, setTeam] = createSignal("");
  const [grant, setGrant] = createSignal<Grant>("read");
  const { working, error, run } = useAction();
  const ungranted = createMemo(() =>
    (teams.data() ?? []).filter(
      (each) => !granted.data()?.some((given) => given.team.id === each.id),
    ),
  );
  const chosen = createMemo(() =>
    ungranted().some((each) => String(each.id) === team()) ? team() : "",
  );
  const give = (event: SubmitEvent) => {
    event.preventDefault();
    const selectedTeam = chosen();
    const selectedGrant = grant();
    run(async () => {
      await grantTeam(props.project.id, Number(selectedTeam), selectedGrant);
      setTeam("");
      granted.again();
    });
  };
  const chooseTeam = (event: Event & { currentTarget: HTMLSelectElement }) =>
    setTeam(event.currentTarget.value);
  const chooseGrant = (event: Event & { currentTarget: HTMLSelectElement }) =>
    setGrant(event.currentTarget.value as Grant);
  return (
    <Show
      when={granted.data()}
      fallback={
        <div class="nested wide">
          <Loading error={granted.error()} what="team access" />
        </div>
      }
    >
      {(list) => (
        <div class="nested wide">
          <Stale error={granted.error()} what="team access" />
          <Show
            when={list().length > 0}
            fallback={
              <p class="muted">
                No team has access of its own. Members get{" "}
                {PERMISSION_LABEL[props.org.base_permission].toLowerCase()} from the organization.
              </p>
            }
          >
            <ul class="roster" aria-label={`Teams with access to ${props.project.name}`}>
              <For each={list()} keyed={(given) => given.team.id}>
                {(given) => (
                  <GrantRow
                    project={props.project.id}
                    given={given()}
                    admin={admin()}
                    changed={granted.again}
                  />
                )}
              </For>
            </ul>
          </Show>
          <Show when={admin() ? teams.error() : null}>
            {(message) => <p class="error">Could not load teams: {message()}.</p>}
          </Show>
          <Show when={admin() && ungranted().length > 0}>
            <form class="inline-form" onSubmit={give}>
              <label for={teamId}>Give</label>
              <select id={teamId} class="input compact" value={chosen()} onChange={chooseTeam}>
                <option value="" selected={chosen() === ""}>
                  a team…
                </option>
                <For each={ungranted()} keyed={(each) => each.id}>
                  {(each) => (
                    <option value={each().id} selected={String(each().id) === chosen()}>
                      {each().name}
                    </option>
                  )}
                </For>
              </select>
              <select id={grantId} class="input compact" value={grant()} onChange={chooseGrant}>
                <For each={GRANTS}>
                  {(each) => (
                    <option value={each} selected={each === grant()}>
                      {PERMISSION_LABEL[each]}
                    </option>
                  )}
                </For>
              </select>
              <label for={grantId}>access</label>
              <button type="submit" class="button small" disabled={working() || chosen() === ""}>
                Give access
              </button>
              <Show when={error()}>
                {(message) => (
                  <span class="error" role="alert">
                    {message()}
                  </span>
                )}
              </Show>
            </form>
          </Show>
        </div>
      )}
    </Show>
  );
}

function GrantRow(props: {
  project: number;
  given: ProjectTeam;
  admin: boolean;
  changed: () => void;
}) {
  const send = async (permission: Grant) => {
    await grantTeam(props.project, props.given.team.id, permission);
    props.changed();
  };
  const revoke = async () => {
    await revokeTeam(props.project, props.given.team.id);
    props.changed();
  };
  return (
    <li>
      <span class="name">{props.given.team.name}</span>
      <span class="end">
        <Show
          when={props.admin}
          fallback={<span class="muted">{PERMISSION_LABEL[props.given.permission]}</span>}
        >
          <Choice
            label={`Access for ${props.given.team.name}`}
            value={props.given.permission}
            options={GRANTS}
            names={PERMISSION_LABEL}
            send={send}
          />
          <Confirm
            label="Remove"
            question={`Take ${props.given.team.name}'s access away?`}
            act={revoke}
          />
        </Show>
      </span>
    </li>
  );
}

export function Organizations() {
  const directory = useDirectory();
  const [creating, setCreating] = createSignal(false);
  const startCreating = () => setCreating(true);
  const stopCreating = () => setCreating(false);
  useAsked("org", startCreating);
  const invitations = createMemo(() => {
    const list = directory().invitations;
    return list && list.length > 0 ? list : null;
  });
  return (
    <>
      <Show when={invitations()}>
        {(list) => (
          <>
            <h2>Invitations</h2>
            <ul class="roster" aria-label="Invitations">
              <For each={list()} keyed={(each) => each.org.id}>
                {(each) => <InvitationRow invitation={each()} />}
              </For>
            </ul>
          </>
        )}
      </Show>
      <h2>Organizations</h2>
      <Show when={directory().orgs !== null}>
        <Stale error={directory().error} what="your organizations" />
      </Show>
      <Show
        when={directory().orgs}
        fallback={<Loading error={directory().error} what="your organizations" />}
      >
        {(orgs) => (
          <Show
            when={orgs().length > 0}
            fallback={
              <Show when={!directory().error}>
                <p class="muted">
                  You are not in any organization. Make one to share projects with other people.
                </p>
              </Show>
            }
          >
            <ul class="roster" aria-label="Organizations">
              <For each={orgs()} keyed={(org) => org.id}>
                {(org) => (
                  <li>
                    <ShowOrgButton org={org()} class="link-button name">
                      {orgLabel(org(), orgs())}
                    </ShowOrgButton>
                    <span class="muted">{ROLE_LABEL[org().role]}</span>
                  </li>
                )}
              </For>
            </ul>
          </Show>
        )}
      </Show>
      <Show
        when={creating()}
        fallback={
          <div class="actions">
            <button type="button" class="button" onClick={startCreating}>
              New organization
            </button>
          </div>
        }
      >
        <NameForm
          label="Organization name"
          verb="Create organization"
          note="You will be its owner. Only people you invite can see it."
          send={makeOrg}
          done={stopCreating}
        />
      </Show>
    </>
  );
}

async function makeOrg(name: string) {
  const made = await createOrg(name);
  await reload();
  showOrg(made.id, made.name);
}

function InvitationRow(props: {
  invitation: { org: { id: number; name: string }; role: Role; invited_by: string | null };
}) {
  const { working, error, run } = useAction();
  const orgs = useSelector(directoryStore, (state) => state.orgs);
  const accept = () =>
    run(async () => {
      await acceptInvitation(props.invitation.org.id);
      await reload();
    });
  const decline = () =>
    run(async () => {
      await declineInvitation(props.invitation.org.id);
      await reload();
    });
  return (
    <li>
      <span>
        <b>{orgLabel(props.invitation.org, orgs())}</b> invites you as{" "}
        {props.invitation.role === "owner" ? "an owner" : "a member"}
        <Show when={props.invitation.invited_by}>
          {(by) => <span class="muted"> (from {by()})</span>}
        </Show>
      </span>
      <span class="end">
        <button type="button" class="button small primary" disabled={working()} onClick={accept}>
          Accept
        </button>
        <button type="button" class="button small" disabled={working()} onClick={decline}>
          Decline
        </button>
      </span>
      <Show when={error()}>
        {(message) => (
          <span class="error wide" role="alert">
            {message()}
          </span>
        )}
      </Show>
    </li>
  );
}

const SECTIONS = ["members", "projects", "teams", "settings"] as const;
type Section = (typeof SECTIONS)[number];
const SECTION_LABEL: Record<Section, string> = {
  members: "Members",
  projects: "Projects",
  teams: "Teams",
  settings: "Settings",
};

export function OrgBody(props: PaneProps) {
  const orgId = createMemo(() => Number(props.pane.key));
  const now = useAccount();
  const directory = useDirectory();
  const params = createMemo(() => orgParams(props.pane));

  const [section, setSection] = createSignal(() => params().section);
  const keep = (changes: Partial<ReturnType<typeof orgParams>>) =>
    change((layout) => setParams(layout, props.id, { ...untrack(params), ...changes } as Json));
  const pick = (each: Section) => {
    setSection(each);
    keep({ section: each });
  };
  const org = createMemo(() => directory().orgs?.find((each) => each.id === orgId()));
  onSettled(() => {
    void checkAccount();
  });
  const hasOrg = createMemo(() => org() !== undefined);
  const label = createMemo(() => {
    const current = org();
    return current ? orgLabel(current, directory().orgs) : "";
  });

  createEffect(
    () => [hasOrg(), label(), params().name] as const,
    ([present, nextLabel, currentName]) => {
      if (present && nextLabel !== currentName) {
        keep({ name: nextLabel });
      }
    },
  );

  const signedIn = createMemo(() => {
    const current = now();
    return current.kind === "in" ? current : null;
  });

  return (
    <Show when={signedIn()} fallback={<SignedOut now={now()} what="Organizations" />}>
      {(currentUser) => (
        <Show
          when={org()}
          fallback={
            <div class="pane-body pane-text">
              <Switch
                fallback={
                  <p class="muted">
                    {orgName(props.pane) || "This organization"} is gone, or you are no longer in
                    it.
                  </p>
                }
              >
                <Match when={directory().error}>
                  <Stale error={directory().error} what="your organizations" />
                </Match>
                <Match when={directory().orgs === null}>
                  <Loading error={null} what="this organization" />
                </Match>
              </Switch>
            </div>
          }
        >
          {(currentOrg) => (
            <div class="pane-body pane-text org">
              <div class="org-head">
                <h1>{label()}</h1>
                <span class="muted">
                  You are {currentOrg().role === "owner" ? "an owner" : "a member"}.
                </span>
              </div>
              <Stale error={directory().error} what="your organizations" />
              <fieldset class="segmented" aria-label="Sections">
                <For each={SECTIONS}>
                  {(each) => (
                    <SectionButton section={each} pressed={section() === each} onPick={pick} />
                  )}
                </For>
              </fieldset>
              <Switch fallback={<Settings org={currentOrg()} pane={props.id} />}>
                <Match when={section() === "members"}>
                  <Members org={currentOrg()} me={currentUser().user.id} pane={props.id} />
                </Match>
                <Match when={section() === "projects"}>
                  <OrgProjects
                    org={currentOrg()}
                    projects={directory().projects ?? NO_PROJECTS}
                    stale={directory().error !== null}
                  />
                </Match>
                <Match when={section() === "teams"}>
                  <Teams org={currentOrg()} />
                </Match>
              </Switch>
            </div>
          )}
        </Show>
      )}
    </Show>
  );
}

function SectionButton(props: {
  section: Section;
  pressed: boolean;
  onPick: (section: Section) => void;
}) {
  const onClick = () => props.onPick(props.section);
  return (
    <button type="button" aria-pressed={props.pressed ? "true" : "false"} onClick={onClick}>
      {SECTION_LABEL[props.section]}
    </button>
  );
}

function leave(pane: PaneId): void {
  change((layout) => (layout.panes[pane] ? close(layout, pane) : layout));
}

function Members(props: { org: Org; me: number; pane: PaneId }) {
  const owner = createMemo(() => props.org.role === "owner");
  const members = useLoaded(
    () => listMembers(props.org.id),
    () => String(props.org.id),
  );
  const invitations = useLoaded(
    () => (owner() ? listInvitations(props.org.id) : Promise.resolve([])),
    () => `${props.org.id}:${owner()}`,
  );
  const changed = async (member: Member, left: boolean) => {
    if (left) {
      await reload();
      leave(props.pane);
      return;
    }
    members.again();

    if (member.id === props.me) {
      await reload();
    }
  };
  const pendingInvitations = createMemo(() => {
    const list = invitations.data();
    return owner() && list && list.length > 0 ? list : null;
  });
  return (
    <>
      <Show when={owner()}>
        <InviteForm org={props.org} invited={invitations.again} />
      </Show>
      <h2>Members</h2>
      <Show when={members.data() !== null}>
        <Stale error={members.error()} what="members" />
      </Show>
      <Show when={members.data()} fallback={<Loading error={members.error()} what="members" />}>
        {(list) => (
          <ul class="roster" aria-label="Members">
            <For each={list()} keyed={(member) => member.id}>
              {(member) => (
                <MemberRow
                  org={props.org}
                  member={member()}
                  me={member().id === props.me}
                  changed={changed}
                />
              )}
            </For>
          </ul>
        )}
      </Show>
      <Show when={pendingInvitations()}>
        {(list) => (
          <>
            <h2>Invited</h2>
            <ul class="roster" aria-label="Invited">
              <For each={list()} keyed={(each) => each.user_id}>
                {(each) => (
                  <InvitedRow org={props.org.id} invitation={each()} changed={invitations.again} />
                )}
              </For>
            </ul>
          </>
        )}
      </Show>
      <Show when={owner() ? invitations.error() : null}>
        {(message) => <p class="error">Could not load invitations: {message()}.</p>}
      </Show>
    </>
  );
}

function InvitedRow(props: { org: number; invitation: Invitation; changed: () => void }) {
  const cancel = async () => {
    await cancelInvitation(props.org, props.invitation.user_id);
    props.changed();
  };
  return (
    <li>
      <span class="name">{props.invitation.username}</span>
      <span class="muted">{ROLE_LABEL[props.invitation.role]}</span>
      <span class="end">
        <Confirm
          label="Cancel invitation"
          question={`Cancel ${props.invitation.username}'s invitation?`}
          act={cancel}
        />
      </span>
    </li>
  );
}

function InviteForm(props: { org: Org; invited: () => void }) {
  const nameId = createUniqueId();
  const roleId = createUniqueId();
  const [username, setUsername] = createSignal("");
  const [role, setRoleChoice] = createSignal<Role>("member");
  const [sent, setSent] = createSignal<string | null>(null);
  const { working, error, run } = useAction();
  const submit = (event: SubmitEvent) => {
    event.preventDefault();
    const name = username().trim();
    const selectedRole = role();
    setSent(null);
    run(async () => {
      await invite(props.org.id, name, selectedRole);
      setUsername("");
      setSent(name);
      props.invited();
    });
  };
  const type = (event: InputEvent & { currentTarget: HTMLInputElement }) =>
    setUsername(event.currentTarget.value);
  const choose = (event: Event & { currentTarget: HTMLSelectElement }) =>
    setRoleChoice(event.currentTarget.value as Role);
  return (
    <form class="pane-form" onSubmit={submit}>
      <h2>Invite someone</h2>
      <div class="inline-form">
        <div class="field grow">
          <label for={nameId}>Username</label>
          <input
            id={nameId}
            class="input"
            value={username()}
            spellcheck={false}
            autocomplete="off"
            aria-invalid={error() === null ? "false" : "true"}
            onInput={type}
          />
        </div>
        <div class="field">
          <label for={roleId}>Role</label>
          <select id={roleId} class="input" value={role()} onChange={choose}>
            <For each={ROLES}>
              {(each) => (
                <option value={each} selected={each === role()}>
                  {ROLE_LABEL[each]}
                </option>
              )}
            </For>
          </select>
        </div>
        <button type="submit" class="button primary" disabled={working() || !username().trim()}>
          Invite
        </button>
      </div>
      <span class="note muted">They accept from their Account pane.</span>
      <Show when={sent()}>
        {(name) => (
          <>
            {/* oxlint-disable-next-line jsx-a11y/prefer-tag-over-role -- a styled span, not a form output */}
            <span role="status" class="muted">
              Invited {name()}.
            </span>
          </>
        )}
      </Show>
      <Show when={error()}>
        {(message) => (
          <span class="error" role="alert">
            {message()}
          </span>
        )}
      </Show>
    </form>
  );
}

function MemberRow(props: {
  org: Org;
  member: Member;
  me: boolean;
  changed: (member: Member, left: boolean) => Promise<void>;
}) {
  const owner = createMemo(() => props.org.role === "owner");
  const send = async (role: Role) => {
    await setRole(props.org.id, props.member.id, role);
    await props.changed(props.member, false);
  };
  const leaveOrg = async () => {
    await removeMember(props.org.id, props.member.id);
    await props.changed(props.member, true);
  };
  const remove = async () => {
    await removeMember(props.org.id, props.member.id);
    await props.changed(props.member, false);
  };
  return (
    <li>
      <span class="name">
        {props.member.username}
        <Show when={props.me}>
          <span class="muted"> (you)</span>
        </Show>
      </span>
      <span class="end">
        <Show when={owner()} fallback={<span class="muted">{ROLE_LABEL[props.member.role]}</span>}>
          <Choice
            label={`Role of ${props.member.username}`}
            value={props.member.role}
            options={ROLES}
            names={ROLE_LABEL}
            send={send}
          />
        </Show>
        <Show
          when={props.me}
          fallback={
            <Show when={owner()}>
              <Confirm
                label="Remove"
                question={`Remove ${props.member.username} from ${props.org.name}?`}
                act={remove}
              />
            </Show>
          }
        >
          <Confirm
            label="Leave"
            question={`Leave ${props.org.name}? Only an owner can invite you back.`}
            act={leaveOrg}
          />
        </Show>
      </span>
    </li>
  );
}

function OrgProjects(props: { org: Org; projects: readonly Project[]; stale: boolean }) {
  const [creating, setCreating] = createSignal(false);
  const owner = createMemo(() => props.org.role === "owner");
  const mine = createMemo(() =>
    props.projects.filter((project) => project.org?.id === props.org.id),
  );
  const startCreating = () => setCreating(true);
  const stopCreating = () => setCreating(false);
  return (
    <>
      <Show when={owner()}>
        <Show
          when={creating()}
          fallback={
            <div class="actions">
              <button type="button" class="button" onClick={startCreating}>
                New project
              </button>
            </div>
          }
        >
          <NewProject owners={NO_ORGS} fixed={props.org} done={stopCreating} />
        </Show>
      </Show>
      <Show
        when={mine().length > 0}
        fallback={
          <Show when={!props.stale}>
            <p class="muted">
              {owner() ? "No projects yet." : "No projects here that you can see."}
            </p>
          </Show>
        }
      >
        <ul class="roster" aria-label={`${props.org.name} projects`}>
          <For each={mine()} keyed={(project) => project.id}>
            {(project) => <ProjectRow project={project()} org={props.org} />}
          </For>
        </ul>
      </Show>
    </>
  );
}

function Teams(props: { org: Org }) {
  const owner = createMemo(() => props.org.role === "owner");
  const teams = useLoaded(
    () => listTeams(props.org.id),
    () => String(props.org.id),
  );
  const members = useLoaded(
    () => (owner() ? listMembers(props.org.id) : Promise.resolve([])),
    () => `${props.org.id}:${owner()}`,
  );
  const [creating, setCreating] = createSignal(false);
  const create = async (name: string) => {
    await createTeam(props.org.id, name);
    teams.again();
  };
  const startCreating = () => setCreating(true);
  const stopCreating = () => setCreating(false);
  return (
    <>
      <Show when={owner()}>
        <Show
          when={creating()}
          fallback={
            <div class="actions">
              <button type="button" class="button" onClick={startCreating}>
                New team
              </button>
            </div>
          }
        >
          <NameForm label="Team name" verb="Create team" send={create} done={stopCreating} />
        </Show>
      </Show>
      <Show when={teams.data() !== null}>
        <Stale error={teams.error()} what="teams" />
      </Show>
      <Show when={owner()}>
        <Show
          when={members.data() === null}
          fallback={<Stale error={members.error()} what="the members to add to teams" />}
        >
          <Show when={members.error()}>
            {(message) => <Loading error={message()} what="the members to add to teams" />}
          </Show>
        </Show>
      </Show>
      <Show when={teams.data()} fallback={<Loading error={teams.error()} what="teams" />}>
        {(list) => (
          <Show
            when={list().length > 0}
            fallback={
              <p class="muted">
                {owner()
                  ? "No teams yet. Make one, then give it access to projects."
                  : "No teams yet. Owners make them."}
              </p>
            }
          >
            <ul class="roster" aria-label="Teams">
              <For each={list()} keyed={(team) => team.id}>
                {(team) => (
                  <TeamRow
                    org={props.org}
                    team={team()}
                    members={members.data() ?? NO_MEMBERS}
                    changed={teams.again}
                  />
                )}
              </For>
            </ul>
          </Show>
        )}
      </Show>
    </>
  );
}

function TeamRow(props: { org: Org; team: Team; members: readonly Member[]; changed: () => void }) {
  const owner = createMemo(() => props.org.role === "owner");
  const [renaming, setRenaming] = createSignal(false);
  const [open, setOpen] = createSignal(false);
  const rename = async (name: string) => {
    await renameTeam(props.org.id, props.team.id, name);
    props.changed();
  };
  const remove = async () => {
    await deleteTeam(props.org.id, props.team.id);
    props.changed();
  };
  const startRenaming = () => setRenaming(true);
  const stopRenaming = () => setRenaming(false);
  const toggle = () => setOpen((current) => !current);
  return (
    <li>
      <Show
        when={renaming()}
        fallback={
          <>
            <span class="name">{props.team.name}</span>
            <span class="end">
              <button
                type="button"
                class="button small"
                aria-expanded={open() ? "true" : "false"}
                onClick={toggle}
              >
                Members
              </button>
              <Show when={owner()}>
                <button type="button" class="button small" onClick={startRenaming}>
                  Rename
                </button>
                <Confirm
                  label="Delete"
                  question={`Delete ${props.team.name}? The team loses access to all its projects.`}
                  act={remove}
                />
              </Show>
            </span>
          </>
        }
      >
        <NameForm
          label={`New name for ${props.team.name}`}
          initial={props.team.name}
          verb="Rename"
          send={rename}
          done={stopRenaming}
        />
      </Show>
      <Show when={open()}>
        <TeamMembers org={props.org} team={props.team} members={props.members} />
      </Show>
    </li>
  );
}

function TeamMembers(props: { org: Org; team: Team; members: readonly Member[] }) {
  const owner = createMemo(() => props.org.role === "owner");
  const people = useLoaded(
    () => teamMembers(props.org.id, props.team.id),
    () => `${props.org.id}:${props.team.id}`,
  );
  const pickId = createUniqueId();
  const [pick, setPick] = createSignal("");
  const { working, error, run } = useAction();
  const outside = createMemo(() =>
    props.members.filter((member) => !people.data()?.some((each) => each.id === member.id)),
  );
  const chosen = createMemo(() =>
    outside().some((member) => String(member.id) === pick()) ? pick() : "",
  );
  const add = (event: SubmitEvent) => {
    event.preventDefault();
    const selected = chosen();
    run(async () => {
      await addToTeam(props.org.id, props.team.id, Number(selected));
      setPick("");
      people.again();
    });
  };
  const choose = (event: Event & { currentTarget: HTMLSelectElement }) =>
    setPick(event.currentTarget.value);
  return (
    <Show
      when={people.data()}
      fallback={
        <div class="nested wide">
          <Loading error={people.error()} what="the team's members" />
        </div>
      }
    >
      {(list) => (
        <div class="nested wide">
          <Stale error={people.error()} what="the team's members" />
          <Show
            when={list().length > 0}
            fallback={<p class="muted">Nobody is in {props.team.name} yet.</p>}
          >
            <ul class="roster" aria-label={`Members of ${props.team.name}`}>
              <For each={list()} keyed={(person) => person.id}>
                {(person) => (
                  <TeamMemberRow
                    org={props.org.id}
                    team={props.team}
                    person={person()}
                    owner={owner()}
                    changed={people.again}
                  />
                )}
              </For>
            </ul>
          </Show>
          <Show when={owner() && outside().length > 0}>
            <form class="inline-form" onSubmit={add}>
              <label for={pickId}>Add</label>
              <select id={pickId} class="input compact" value={chosen()} onChange={choose}>
                <option value="" selected={chosen() === ""}>
                  a member…
                </option>
                <For each={outside()} keyed={(member) => member.id}>
                  {(member) => (
                    <option value={member().id} selected={String(member().id) === chosen()}>
                      {member().username}
                    </option>
                  )}
                </For>
              </select>
              <button type="submit" class="button small" disabled={working() || chosen() === ""}>
                Add to team
              </button>
              <Show when={error()}>
                {(message) => (
                  <span class="error" role="alert">
                    {message()}
                  </span>
                )}
              </Show>
            </form>
          </Show>
        </div>
      )}
    </Show>
  );
}

function TeamMemberRow(props: {
  org: number;
  team: Team;
  person: TeamMember;
  owner: boolean;
  changed: () => void;
}) {
  const remove = async () => {
    await removeFromTeam(props.org, props.team.id, props.person.id);
    props.changed();
  };
  return (
    <li>
      <span class="name">{props.person.username}</span>
      <Show when={props.owner}>
        <span class="end">
          <Confirm
            label="Remove"
            question={`Take ${props.person.username} out of ${props.team.name}?`}
            act={remove}
          />
        </span>
      </Show>
    </li>
  );
}

function Settings(props: { org: Org; pane: PaneId }) {
  const [renaming, setRenaming] = createSignal(false);
  const rename = async (name: string) => {
    await updateOrg(props.org.id, { name });
    await reload();
  };
  const remove = async () => {
    await deleteOrg(props.org.id);
    await reload();
    leave(props.pane);
  };
  const startRenaming = () => setRenaming(true);
  const stopRenaming = () => setRenaming(false);
  return (
    <Show
      when={props.org.role === "owner"}
      fallback={
        <p>
          Every member gets {PERMISSION_LABEL[props.org.base_permission].toLowerCase()} on every
          project. Teams can give more. Only owners change this.
        </p>
      }
    >
      <h2>Name</h2>
      <Show
        when={renaming()}
        fallback={
          <div class="actions">
            <span class="name">{props.org.name}</span>
            <button type="button" class="button small" onClick={startRenaming}>
              Rename
            </button>
          </div>
        }
      >
        <NameForm
          label="Organization name"
          initial={props.org.name}
          verb="Rename"
          send={rename}
          done={stopRenaming}
        />
      </Show>
      <h2>Base permission</h2>
      <BasePermission org={props.org} />
      <OrgSingleSignOn org={props.org} />
      <h2>Delete</h2>
      <p class="muted">
        Deleting {props.org.name} deletes its projects and teams, and removes everyone from it. This
        cannot be undone.
      </p>
      <div class="actions">
        <Confirm label="Delete organization" question={`Delete ${props.org.name}?`} act={remove} />
      </div>
    </Show>
  );
}

function BasePermission(props: { org: Org }) {
  const id = createUniqueId();
  const [chosen, setChosen] = createSignal<Permission>(() => props.org.base_permission);
  const { working, error, run } = useAction();
  const save = (event: SubmitEvent) => {
    event.preventDefault();
    const next = chosen();
    run(async () => {
      await updateOrg(props.org.id, { base_permission: next });
      await reload();
    });
  };
  const choose = (event: Event & { currentTarget: HTMLSelectElement }) =>
    setChosen(event.currentTarget.value as Permission);
  return (
    <form class="pane-form" onSubmit={save}>
      <div class="inline-form">
        <div class="field">
          <label for={id}>Every member gets</label>
          <select id={id} class="input" value={chosen()} onChange={choose}>
            <For each={PERMISSIONS}>
              {(each) => (
                <option value={each} selected={each === chosen()}>
                  {PERMISSION_LABEL[each]}
                </option>
              )}
            </For>
          </select>
        </div>
        <button
          type="submit"
          class="button"
          disabled={working() || chosen() === props.org.base_permission}
        >
          Save
        </button>
      </div>
      <span class="note muted">
        This applies to every project in {props.org.name}. Teams can give more, never less. Owners
        are admins everywhere.
      </span>
      <Show when={error()}>
        {(message) => (
          <span class="error" role="alert">
            {message()}
          </span>
        )}
      </Show>
    </form>
  );
}
