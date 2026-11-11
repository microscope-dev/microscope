// Organizations and projects on the services backend, for a signed-in
// person on a page served on its own: the Projects pane, the Organization
// pane, and the Account pane's organizations and invitations. Each control
// shows only for someone services would let use it, and when services
// refuses anyway, its reason shows where the control was.

import {
  type ChangeEvent,
  type FormEvent,
  type KeyboardEvent,
  type ReactNode,
  useCallback,
  useEffect,
  useId,
  useMemo,
  useRef,
  useState,
} from "react";
import { createStore, useStore } from "zustand";

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
import type { PaneProps } from "../workbench/kinds";
import { close, type Json, type Pane, type PaneId, setParams } from "../workbench/layout";
import { change } from "../workbench/store";
import { showOrg, showPane } from "./panes";
import { OrgSingleSignOn } from "./SingleSignOn";

/** A form a command asked a pane to open: New project or New organization. */
export const asked = createStore<{ form: "project" | "org" | null }>(() => ({ form: null }));

/** Opens a form when a command asks for it, now or once the pane mounts. */
function useAsked(form: "project" | "org", open: () => void): void {
  const wanted = useStore(asked, (state) => state.form === form);
  useEffect(() => {
    if (wanted) {
      asked.setState({ form: null });
      open();
    }
    // oxlint-disable-next-line react-hooks/exhaustive-deps -- only the ask matters
  }, [wanted]);
}

/** What an Organization pane keeps in the layout: the name its tab shows,
 * and the section it was left at. */
function orgParams(pane: Pane): { name: string; section: Section } {
  const params = (pane.params ?? {}) as { name?: unknown; section?: unknown };
  return {
    name: typeof params.name === "string" ? params.name : "",
    section: SECTIONS.find((each) => each === params.section) ?? "members",
  };
}

export const orgName = (pane: Pane): string => orgParams(pane).name;

/** Where a control's request is: none, or answered; on its way; or
 * refused. Only the newest request's answer counts. */
type Run = { kind: "idle" } | { kind: "working"; id: number } | { kind: "failed"; error: Error };

/** One request a control makes at a time, with `fail` for a failure that
 * comes some other way. */
export function useRun() {
  const [now, setNow] = useState<Run>({ kind: "idle" });
  const newest = useRef(0);
  const run = useCallback((send: () => Promise<unknown>) => {
    newest.current += 1;
    const id = newest.current;
    setNow({ kind: "working", id });
    send().then(
      () => newest.current === id && setNow({ kind: "idle" }),
      (error: Error) => newest.current === id && setNow({ kind: "failed", error }),
    );
  }, []);
  const fail = useCallback((error: Error) => setNow({ kind: "failed", error }), []);
  return {
    working: now.kind === "working",
    error: now.kind === "failed" ? now.error : null,
    run,
    fail,
  };
}

/** One request a control makes: whether it is in flight, and why services
 * refused it. */
export function useAction() {
  const { working, error, run } = useRun();
  return { working, error: error?.message ?? null, run };
}

/** An answer a pane asks for itself, again whenever `key` changes or it
 * calls `again`. An answer for another key is never shown; one for this
 * key stays while it is asked again. */
export function useLoaded<T>(load: () => Promise<T>, key: string) {
  const [loaded, setLoaded] = useState<{ key: string; data: T | null; error: string | null }>({
    key,
    data: null,
    error: null,
  });
  const [round, setRound] = useState(0);
  useEffect(() => {
    let live = true;
    load().then(
      (data) => live && setLoaded({ key, data, error: null }),
      (failure: Error) =>
        live &&
        setLoaded((before) => ({
          key,
          data: before.key === key ? before.data : null,
          error: failure.message,
        })),
    );
    return () => {
      live = false;
    };
    // oxlint-disable-next-line react-hooks/exhaustive-deps -- `key` names what `load` asks
  }, [key, round]);
  const again = useCallback(() => setRound((before) => before + 1), []);
  const current = loaded.key === key;
  return { data: current ? loaded.data : null, error: current ? loaded.error : null, again };
}

function showAccount() {
  showPane(KIND.account);
}

const NO_ORGS: readonly Org[] = [];
const NO_PROJECTS: readonly Project[] = [];
const NO_MEMBERS: readonly Member[] = [];

/** Why a pane about the person's projects has nothing to show, or null. */
function SignedOut({ now, what }: { now: Account; what: string }) {
  if (now.kind === "in") {
    return null;
  }
  return (
    <div className="pane-body pane-text">
      {now.kind === "checking" ? (
        <p className="muted">Checking whether you are signed in…</p>
      ) : now.kind === "unavailable" ? (
        <p className="muted">
          {what} need the services backend, which is unavailable ({now.why}).
        </p>
      ) : (
        <>
          <p>Sign in to see {what.toLowerCase()}. Connecting to a microscope needs no account.</p>
          <div className="actions">
            <button type="button" className="button" onClick={showAccount}>
              Sign in
            </button>
          </div>
        </>
      )}
    </div>
  );
}

/** A name to give something new or change, and Escape to give up. */
function NameForm(props: {
  label: string;
  initial?: string;
  verb: string;
  note?: string;
  send: (name: string) => Promise<unknown>;
  done: () => void;
  children?: ReactNode;
}) {
  const id = useId();
  const input = useRef<HTMLInputElement>(null);
  const [name, setName] = useState(props.initial ?? "");
  const { working, error, run } = useAction();
  useEffect(() => input.current?.focus(), []);
  const { send, done } = props;
  const submit = useCallback(
    (event: FormEvent) => {
      event.preventDefault();
      run(async () => {
        await send(name.trim());
        done();
      });
    },
    [run, send, name, done],
  );
  const type = useCallback(
    (event: ChangeEvent<HTMLInputElement>) => setName(event.target.value),
    [],
  );
  const escape = useCallback(
    (event: KeyboardEvent) => {
      if (event.key === "Escape") {
        event.stopPropagation();
        done();
      }
    },
    [done],
  );
  const unchanged = name.trim() === "" || name.trim() === props.initial;
  return (
    <form className="pane-form wide" onSubmit={submit}>
      <div className="field">
        <label htmlFor={id}>{props.label}</label>
        <input
          ref={input}
          id={id}
          className="input"
          value={name}
          maxLength={64}
          spellCheck={false}
          autoComplete="off"
          data-autofocus
          aria-invalid={error !== null}
          onChange={type}
          onKeyDown={escape}
        />
        {props.note && <span className="note">{props.note}</span>}
      </div>
      {props.children}
      {error && (
        <span className="error" role="alert">
          {error}
        </span>
      )}
      <div className="actions">
        <button type="submit" className="button primary" disabled={working || unchanged}>
          {props.verb}
        </button>
        <button type="button" className="link-button" onClick={done}>
          Cancel
        </button>
      </div>
    </form>
  );
}

/** A button for what cannot be undone, which asks first. */
export function Confirm(props: {
  label: string;
  question: string;
  act: () => Promise<unknown>;
  /** What backing out is called, when "Cancel" would mean the act itself. */
  keep?: string;
}) {
  const [asking, setAsking] = useState(false);
  const { working, error, run } = useAction();
  const { act } = props;
  const ask = useCallback(() => setAsking(true), []);
  const confirm = useCallback(() => run(act), [run, act]);
  const keep = useCallback(() => setAsking(false), []);
  if (!asking) {
    return (
      <button type="button" className="button small" onClick={ask}>
        {props.label}
      </button>
    );
  }
  return (
    <span className="confirm">
      <span>{props.question}</span>
      <button type="button" className="button small danger" disabled={working} onClick={confirm}>
        {props.label}
      </button>
      <button type="button" className="link-button" onClick={keep}>
        {props.keep ?? "Cancel"}
      </button>
      {error && (
        <span className="error" role="alert">
          {error}
        </span>
      )}
    </span>
  );
}

/** A choice from a short list, saved as soon as it changes. */
function Choice<T extends string>(props: {
  label: string;
  value: T;
  options: readonly T[];
  names: Record<T, string>;
  send: (value: T) => Promise<unknown>;
}) {
  const { working, error, run } = useAction();
  const { send } = props;
  const choose = useCallback(
    (event: ChangeEvent<HTMLSelectElement>) => {
      const value = event.target.value as T;
      run(() => send(value));
    },
    [run, send],
  );
  return (
    <>
      <select
        className="input compact"
        aria-label={props.label}
        value={props.value}
        disabled={working}
        onChange={choose}
      >
        {props.options.map((option) => (
          <option key={option} value={option}>
            {props.names[option]}
          </option>
        ))}
      </select>
      {error && (
        <span className="error" role="alert">
          {error}
        </span>
      )}
    </>
  );
}

/** Loading, or why loading failed, for a list a pane asked for. */
export function Loading({ error, what }: { error: string | null; what: string }) {
  return error ? (
    <p className="error" role="alert">
      Could not load {what}: {error}.
    </p>
  ) : (
    <p className="muted">Loading {what}…</p>
  );
}

/** Why a list on screen may be out of date: asking again failed. */
function Stale({ error, what }: { error: string | null; what: string }) {
  return error ? (
    <p className="error" role="alert">
      Could not refresh {what}: {error}.
    </p>
  ) : null;
}

// Projects

export function ProjectsBody() {
  const now = useAccount();
  const directory = useDirectory();
  const [creating, setCreating] = useState(false);
  useEffect(() => {
    void checkAccount();
  }, []);
  const startCreating = useCallback(() => setCreating(true), []);
  const stopCreating = useCallback(() => setCreating(false), []);
  useAsked("project", startCreating);
  const { orgs, projects } = directory;
  const owned = useMemo(() => (orgs ?? []).filter((org) => org.role === "owner"), [orgs]);
  if (now.kind !== "in") {
    return <SignedOut now={now} what="Projects" />;
  }
  return (
    <div className="pane-body pane-text projects">
      {creating ? (
        <NewProject owners={owned} done={stopCreating} />
      ) : (
        <div className="actions">
          <button type="button" className="button" onClick={startCreating}>
            New project
          </button>
        </div>
      )}
      {orgs === null || projects === null ? (
        <Loading error={directory.error} what="your projects" />
      ) : (
        <>
          <Stale error={directory.error} what="your projects" />
          {groupProjects(projects, orgs).map((group) => (
            <section key={group.org?.id ?? "own"} className="project-group">
              <div className="group-head">
                <h3>{group.org ? orgLabel(group.org, orgs) : "Yours"}</h3>
                {group.org && (
                  <ShowOrgButton
                    org={group.org}
                    className="link-button"
                    label={`Open ${orgLabel(group.org, orgs)}`}
                  >
                    Open organization
                  </ShowOrgButton>
                )}
              </div>
              {group.projects.length === 0 ? (
                directory.error ? null : (
                  <p className="muted">
                    {group.org === null
                      ? "None of your own yet."
                      : group.org.role === "owner"
                        ? "No projects yet."
                        : "No projects here that you can see."}
                  </p>
                )
              ) : (
                <ul
                  className="roster"
                  aria-label={`${group.org ? orgLabel(group.org, orgs) : "Your"} projects`}
                >
                  {group.projects.map((project) => (
                    <ProjectRow key={project.id} project={project} />
                  ))}
                </ul>
              )}
            </section>
          ))}
        </>
      )}
    </div>
  );
}

function ShowOrgButton({
  org,
  className,
  label,
  children,
}: {
  org: { id: number; name: string };
  className: string;
  label?: string;
  children: ReactNode;
}) {
  const onClick = useCallback(() => showOrg(org.id, org.name), [org.id, org.name]);
  return (
    <button type="button" className={className} aria-label={label} onClick={onClick}>
      {children}
    </button>
  );
}

/** Asks for a new project's name and, for an owner of any organization,
 * whose it is: the person's own by default. */
function NewProject(props: { owners: readonly Org[]; fixed?: Org; done: () => void }) {
  const ownerId = useId();
  const orgs = useStore(directoryStore, (state) => state.orgs);
  const [owner, setOwner] = useState(props.fixed ? String(props.fixed.id) : "");
  const send = useCallback(
    async (name: string) => {
      await createProject(name, owner === "" ? null : Number(owner));
      await reload();
    },
    [owner],
  );
  const choose = useCallback(
    (event: ChangeEvent<HTMLSelectElement>) => setOwner(event.target.value),
    [],
  );
  return (
    <NameForm label="Project name" verb="Create project" send={send} done={props.done}>
      {!props.fixed && props.owners.length > 0 && (
        <div className="field">
          <label htmlFor={ownerId}>Owner</label>
          <select id={ownerId} className="input" value={owner} onChange={choose}>
            <option value="">You</option>
            {props.owners.map((org) => (
              <option key={org.id} value={org.id}>
                {orgLabel(org, orgs)}
              </option>
            ))}
          </select>
        </div>
      )}
    </NameForm>
  );
}

/** A project, the person's access to it, and for an admin, renaming and
 * deleting it. In an Organization pane, also which teams can reach it. */
function ProjectRow({ project, org }: { project: Project; org?: Org }) {
  const [renaming, setRenaming] = useState(false);
  const [teams, setTeams] = useState(false);
  const admin = project.permission === "admin";
  const rename = useCallback(
    async (name: string) => {
      await renameProject(project.id, name);
      await reload();
    },
    [project.id],
  );
  const remove = useCallback(async () => {
    await deleteProject(project.id);
    await reload();
  }, [project.id]);
  const startRenaming = useCallback(() => setRenaming(true), []);
  const stopRenaming = useCallback(() => setRenaming(false), []);
  const toggleTeams = useCallback(() => setTeams(!teams), [teams]);
  return (
    <li>
      {renaming ? (
        <NameForm
          label={`New name for ${project.name}`}
          initial={project.name}
          verb="Rename"
          send={rename}
          done={stopRenaming}
        />
      ) : (
        <>
          <span className="name">{project.name}</span>
          <span className="muted" title="Your access">
            {PERMISSION_LABEL[project.permission]}
          </span>
          <span className="end">
            {org && (
              <button
                type="button"
                className="button small"
                aria-expanded={teams}
                onClick={toggleTeams}
              >
                Team access
              </button>
            )}
            {admin && (
              <>
                <button type="button" className="button small" onClick={startRenaming}>
                  Rename
                </button>
                <Confirm
                  label="Delete"
                  question={`Delete ${project.name}? This cannot be undone.`}
                  act={remove}
                />
              </>
            )}
          </span>
        </>
      )}
      {org && teams && <ProjectTeams project={project} org={org} />}
    </li>
  );
}

/** Which of an organization's teams can reach a project, and how far. An
 * admin changes them. */
function ProjectTeams({ project, org }: { project: Project; org: Org }) {
  const admin = project.permission === "admin";
  const granted = useLoaded(() => projectTeams(project.id), String(project.id));
  const teams = useLoaded(
    () => (admin ? listTeams(org.id) : Promise.resolve([])),
    `${org.id}:${admin}`,
  );
  const teamId = useId();
  const grantId = useId();
  const [team, setTeam] = useState("");
  const [grant, setGrant] = useState<Grant>("read");
  const { working, error, run } = useAction();
  const ungranted = (teams.data ?? []).filter(
    (each) => !granted.data?.some((given) => given.team.id === each.id),
  );
  const chosen = ungranted.some((each) => String(each.id) === team) ? team : "";
  const again = granted.again;
  const give = useCallback(
    (event: FormEvent) => {
      event.preventDefault();
      run(async () => {
        await grantTeam(project.id, Number(chosen), grant);
        setTeam("");
        again();
      });
    },
    [run, project.id, chosen, grant, again],
  );
  const chooseTeam = useCallback(
    (event: ChangeEvent<HTMLSelectElement>) => setTeam(event.target.value),
    [],
  );
  const chooseGrant = useCallback(
    (event: ChangeEvent<HTMLSelectElement>) => setGrant(event.target.value as Grant),
    [],
  );
  if (granted.data === null) {
    return (
      <div className="nested wide">
        <Loading error={granted.error} what="team access" />
      </div>
    );
  }
  return (
    <div className="nested wide">
      <Stale error={granted.error} what="team access" />
      {granted.data.length === 0 ? (
        <p className="muted">
          No team has access of its own. Members get{" "}
          {PERMISSION_LABEL[org.base_permission].toLowerCase()} from the organization.
        </p>
      ) : (
        <ul className="roster" aria-label={`Teams with access to ${project.name}`}>
          {granted.data.map((given) => (
            <GrantRow
              key={given.team.id}
              project={project.id}
              given={given}
              admin={admin}
              changed={again}
            />
          ))}
        </ul>
      )}
      {admin && teams.error && <p className="error">Could not load teams: {teams.error}.</p>}
      {admin && ungranted.length > 0 && (
        <form className="inline-form" onSubmit={give}>
          <label htmlFor={teamId}>Give</label>
          <select id={teamId} className="input compact" value={chosen} onChange={chooseTeam}>
            <option value="">a team…</option>
            {ungranted.map((each) => (
              <option key={each.id} value={each.id}>
                {each.name}
              </option>
            ))}
          </select>
          <select id={grantId} className="input compact" value={grant} onChange={chooseGrant}>
            {GRANTS.map((each) => (
              <option key={each} value={each}>
                {PERMISSION_LABEL[each]}
              </option>
            ))}
          </select>
          <label htmlFor={grantId}>access</label>
          <button type="submit" className="button small" disabled={working || chosen === ""}>
            Give access
          </button>
          {error && (
            <span className="error" role="alert">
              {error}
            </span>
          )}
        </form>
      )}
    </div>
  );
}

function GrantRow({
  project,
  given,
  admin,
  changed,
}: {
  project: number;
  given: ProjectTeam;
  admin: boolean;
  changed: () => void;
}) {
  const team = given.team.id;
  const send = useCallback(
    async (permission: Grant) => {
      await grantTeam(project, team, permission);
      changed();
    },
    [project, team, changed],
  );
  const revoke = useCallback(async () => {
    await revokeTeam(project, team);
    changed();
  }, [project, team, changed]);
  return (
    <li>
      <span className="name">{given.team.name}</span>
      <span className="end">
        {admin ? (
          <>
            <Choice
              label={`Access for ${given.team.name}`}
              value={given.permission}
              options={GRANTS}
              names={PERMISSION_LABEL}
              send={send}
            />
            <Confirm
              label="Remove"
              question={`Take ${given.team.name}'s access away?`}
              act={revoke}
            />
          </>
        ) : (
          <span className="muted">{PERMISSION_LABEL[given.permission]}</span>
        )}
      </span>
    </li>
  );
}

// The Account pane's organizations and invitations

/** The person's invitations and organizations, under their account. */
export function Organizations() {
  const directory = useDirectory();
  const [creating, setCreating] = useState(false);
  const startCreating = useCallback(() => setCreating(true), []);
  const stopCreating = useCallback(() => setCreating(false), []);
  useAsked("org", startCreating);
  const { orgs, invitations } = directory;
  return (
    <>
      {invitations && invitations.length > 0 && (
        <>
          <h2>Invitations</h2>
          <ul className="roster" aria-label="Invitations">
            {invitations.map((each) => (
              <InvitationRow key={each.org.id} invitation={each} />
            ))}
          </ul>
        </>
      )}
      <h2>Organizations</h2>
      {orgs !== null && <Stale error={directory.error} what="your organizations" />}
      {orgs === null ? (
        <Loading error={directory.error} what="your organizations" />
      ) : orgs.length === 0 ? (
        !directory.error && (
          <p className="muted">
            You are not in any organization. Make one to share projects with other people.
          </p>
        )
      ) : (
        <ul className="roster" aria-label="Organizations">
          {orgs.map((org) => (
            <li key={org.id}>
              <ShowOrgButton org={org} className="link-button name">
                {orgLabel(org, orgs)}
              </ShowOrgButton>
              <span className="muted">{ROLE_LABEL[org.role]}</span>
            </li>
          ))}
        </ul>
      )}
      {creating ? (
        <NameForm
          label="Organization name"
          verb="Create organization"
          note="You will be its owner. Only people you invite can see it."
          send={makeOrg}
          done={stopCreating}
        />
      ) : (
        <div className="actions">
          <button type="button" className="button" onClick={startCreating}>
            New organization
          </button>
        </div>
      )}
    </>
  );
}

async function makeOrg(name: string) {
  const made = await createOrg(name);
  await reload();
  showOrg(made.id, made.name);
}

function InvitationRow({
  invitation,
}: {
  invitation: { org: { id: number; name: string }; role: Role; invited_by: string | null };
}) {
  const { working, error, run } = useAction();
  const { org } = invitation;
  const orgs = useStore(directoryStore, (state) => state.orgs);
  const accept = useCallback(
    () =>
      run(async () => {
        await acceptInvitation(org.id);
        await reload();
      }),
    [run, org.id],
  );
  const decline = useCallback(
    () =>
      run(async () => {
        await declineInvitation(org.id);
        await reload();
      }),
    [run, org.id],
  );
  return (
    <li>
      <span>
        <b>{orgLabel(org, orgs)}</b> invites you as{" "}
        {invitation.role === "owner" ? "an owner" : "a member"}
        {invitation.invited_by && <span className="muted"> (from {invitation.invited_by})</span>}
      </span>
      <span className="end">
        <button type="button" className="button small primary" disabled={working} onClick={accept}>
          Accept
        </button>
        <button type="button" className="button small" disabled={working} onClick={decline}>
          Decline
        </button>
      </span>
      {error && (
        <span className="error wide" role="alert">
          {error}
        </span>
      )}
    </li>
  );
}

// The Organization pane

const SECTIONS = ["members", "projects", "teams", "settings"] as const;
type Section = (typeof SECTIONS)[number];
const SECTION_LABEL: Record<Section, string> = {
  members: "Members",
  projects: "Projects",
  teams: "Teams",
  settings: "Settings",
};

export function OrgBody({ id, pane }: PaneProps) {
  const orgId = Number(pane.key);
  const now = useAccount();
  const directory = useDirectory();
  const params = useMemo(() => orgParams(pane), [pane]);
  // Kept in the layout too, so the pane opens where it was left.
  const [section, setSection] = useState(params.section);
  const keep = useCallback(
    (changes: Partial<typeof params>) =>
      change((layout) => setParams(layout, id, { ...params, ...changes } as Json)),
    [id, params],
  );
  const pick = useCallback(
    (each: Section) => {
      setSection(each);
      keep({ section: each });
    },
    [keep],
  );
  const org = directory.orgs?.find((each) => each.id === orgId);
  useEffect(() => {
    void checkAccount();
  }, []);
  const label = org ? orgLabel(org, directory.orgs) : "";
  // The tab shows the name services last gave, told apart from a twin.
  useEffect(() => {
    if (org && label !== params.name) {
      keep({ name: label });
    }
    // oxlint-disable-next-line react-hooks/exhaustive-deps -- only a new name matters
  }, [label]);
  if (now.kind !== "in") {
    return <SignedOut now={now} what="Organizations" />;
  }
  if (!org) {
    return (
      <div className="pane-body pane-text">
        {directory.error ? (
          <Stale error={directory.error} what="your organizations" />
        ) : directory.orgs === null ? (
          <Loading error={null} what="this organization" />
        ) : (
          <p className="muted">
            {orgName(pane) || "This organization"} is gone, or you are no longer in it.
          </p>
        )}
      </div>
    );
  }
  const owner = org.role === "owner";
  return (
    <div className="pane-body pane-text org">
      <div className="org-head">
        <h1>{label}</h1>
        <span className="muted">You are {owner ? "an owner" : "a member"}.</span>
      </div>
      <Stale error={directory.error} what="your organizations" />
      <fieldset className="segmented" aria-label="Sections">
        {SECTIONS.map((each) => (
          <SectionButton key={each} section={each} pressed={section === each} onPick={pick} />
        ))}
      </fieldset>
      {section === "members" ? (
        <Members org={org} me={now.user.id} pane={id} />
      ) : section === "projects" ? (
        <OrgProjects
          org={org}
          projects={directory.projects ?? NO_PROJECTS}
          stale={directory.error !== null}
        />
      ) : section === "teams" ? (
        <Teams org={org} />
      ) : (
        <Settings org={org} pane={id} />
      )}
    </div>
  );
}

function SectionButton({
  section,
  pressed,
  onPick,
}: {
  section: Section;
  pressed: boolean;
  onPick(section: Section): void;
}) {
  const onClick = useCallback(() => onPick(section), [onPick, section]);
  return (
    <button type="button" aria-pressed={pressed} onClick={onClick}>
      {SECTION_LABEL[section]}
    </button>
  );
}

/** Closes the pane of an organization the person just left or deleted. */
function leave(pane: PaneId): void {
  change((layout) => (layout.panes[pane] ? close(layout, pane) : layout));
}

function Members({ org, me, pane }: { org: Org; me: number; pane: PaneId }) {
  const owner = org.role === "owner";
  const members = useLoaded(() => listMembers(org.id), String(org.id));
  // Only owners may see who is invited.
  const invitations = useLoaded(
    () => (owner ? listInvitations(org.id) : Promise.resolve([])),
    `${org.id}:${owner}`,
  );
  const again = members.again;
  const changed = useCallback(
    async (member: Member, left: boolean) => {
      if (left) {
        await reload();
        leave(pane);
        return;
      }
      again();
      // Someone's own role decides what they may do next.
      if (member.id === me) {
        await reload();
      }
    },
    [again, pane, me],
  );
  return (
    <>
      {owner && <InviteForm org={org} invited={invitations.again} />}
      <h2>Members</h2>
      {members.data !== null && <Stale error={members.error} what="members" />}
      {members.data === null ? (
        <Loading error={members.error} what="members" />
      ) : (
        <ul className="roster" aria-label="Members">
          {members.data.map((member) => (
            <MemberRow
              key={member.id}
              org={org}
              member={member}
              me={member.id === me}
              changed={changed}
            />
          ))}
        </ul>
      )}
      {owner && invitations.data && invitations.data.length > 0 && (
        <>
          <h2>Invited</h2>
          <ul className="roster" aria-label="Invited">
            {invitations.data.map((each) => (
              <InvitedRow
                key={each.user_id}
                org={org.id}
                invitation={each}
                changed={invitations.again}
              />
            ))}
          </ul>
        </>
      )}
      {owner && invitations.error && (
        <p className="error">Could not load invitations: {invitations.error}.</p>
      )}
    </>
  );
}

function InvitedRow({
  org,
  invitation,
  changed,
}: {
  org: number;
  invitation: Invitation;
  changed: () => void;
}) {
  const user = invitation.user_id;
  const cancel = useCallback(async () => {
    await cancelInvitation(org, user);
    changed();
  }, [org, user, changed]);
  return (
    <li>
      <span className="name">{invitation.username}</span>
      <span className="muted">{ROLE_LABEL[invitation.role]}</span>
      <span className="end">
        <Confirm
          label="Cancel invitation"
          question={`Cancel ${invitation.username}'s invitation?`}
          act={cancel}
        />
      </span>
    </li>
  );
}

function InviteForm({ org, invited }: { org: Org; invited: () => void }) {
  const nameId = useId();
  const roleId = useId();
  const [username, setUsername] = useState("");
  const [role, setRoleChoice] = useState<Role>("member");
  const [sent, setSent] = useState<string | null>(null);
  const { working, error, run } = useAction();
  const submit = useCallback(
    (event: FormEvent) => {
      event.preventDefault();
      const name = username.trim();
      setSent(null);
      run(async () => {
        await invite(org.id, name, role);
        setUsername("");
        setSent(name);
        invited();
      });
    },
    [username, run, org.id, role, invited],
  );
  const type = useCallback(
    (event: ChangeEvent<HTMLInputElement>) => setUsername(event.target.value),
    [],
  );
  const choose = useCallback(
    (event: ChangeEvent<HTMLSelectElement>) => setRoleChoice(event.target.value as Role),
    [],
  );
  return (
    <form className="pane-form" onSubmit={submit}>
      <h2>Invite someone</h2>
      <div className="inline-form">
        <div className="field grow">
          <label htmlFor={nameId}>Username</label>
          <input
            id={nameId}
            className="input"
            value={username}
            spellCheck={false}
            autoComplete="off"
            aria-invalid={error !== null}
            onChange={type}
          />
        </div>
        <div className="field">
          <label htmlFor={roleId}>Role</label>
          <select id={roleId} className="input" value={role} onChange={choose}>
            {ROLES.map((each) => (
              <option key={each} value={each}>
                {ROLE_LABEL[each]}
              </option>
            ))}
          </select>
        </div>
        <button type="submit" className="button primary" disabled={working || !username.trim()}>
          Invite
        </button>
      </div>
      <span className="note muted">They accept from their Account pane.</span>
      {sent && (
        // oxlint-disable-next-line jsx-a11y/prefer-tag-over-role -- a styled span, not a form output
        <span role="status" className="muted">
          Invited {sent}.
        </span>
      )}
      {error && (
        <span className="error" role="alert">
          {error}
        </span>
      )}
    </form>
  );
}

function MemberRow(props: {
  org: Org;
  member: Member;
  me: boolean;
  changed: (member: Member, left: boolean) => Promise<void>;
}) {
  const { org, member, me, changed } = props;
  const owner = org.role === "owner";
  const send = useCallback(
    async (role: Role) => {
      await setRole(org.id, member.id, role);
      await changed(member, false);
    },
    [org.id, member, changed],
  );
  const leaveOrg = useCallback(async () => {
    await removeMember(org.id, member.id);
    await changed(member, true);
  }, [org.id, member, changed]);
  const remove = useCallback(async () => {
    await removeMember(org.id, member.id);
    await changed(member, false);
  }, [org.id, member, changed]);
  return (
    <li>
      <span className="name">
        {member.username}
        {me && <span className="muted"> (you)</span>}
      </span>
      <span className="end">
        {owner ? (
          <Choice
            label={`Role of ${member.username}`}
            value={member.role}
            options={ROLES}
            names={ROLE_LABEL}
            send={send}
          />
        ) : (
          <span className="muted">{ROLE_LABEL[member.role]}</span>
        )}
        {me ? (
          <Confirm
            label="Leave"
            question={`Leave ${org.name}? Only an owner can invite you back.`}
            act={leaveOrg}
          />
        ) : (
          owner && (
            <Confirm
              label="Remove"
              question={`Remove ${member.username} from ${org.name}?`}
              act={remove}
            />
          )
        )}
      </span>
    </li>
  );
}

function OrgProjects(props: { org: Org; projects: readonly Project[]; stale: boolean }) {
  const { org, projects } = props;
  const [creating, setCreating] = useState(false);
  const owner = org.role === "owner";
  const mine = projects.filter((project) => project.org?.id === org.id);
  const startCreating = useCallback(() => setCreating(true), []);
  const stopCreating = useCallback(() => setCreating(false), []);
  return (
    <>
      {owner &&
        (creating ? (
          <NewProject owners={NO_ORGS} fixed={org} done={stopCreating} />
        ) : (
          <div className="actions">
            <button type="button" className="button" onClick={startCreating}>
              New project
            </button>
          </div>
        ))}
      {mine.length === 0 ? (
        // The pane's alert already says the list may be out of date.
        props.stale ? null : (
          <p className="muted">
            {owner ? "No projects yet." : "No projects here that you can see."}
          </p>
        )
      ) : (
        <ul className="roster" aria-label={`${org.name} projects`}>
          {mine.map((project) => (
            <ProjectRow key={project.id} project={project} org={org} />
          ))}
        </ul>
      )}
    </>
  );
}

function Teams({ org }: { org: Org }) {
  const owner = org.role === "owner";
  const teams = useLoaded(() => listTeams(org.id), String(org.id));
  // An owner adds people from the organization's members.
  const members = useLoaded(
    () => (owner ? listMembers(org.id) : Promise.resolve([])),
    `${org.id}:${owner}`,
  );
  const [creating, setCreating] = useState(false);
  const again = teams.again;
  const create = useCallback(
    async (name: string) => {
      await createTeam(org.id, name);
      again();
    },
    [org.id, again],
  );
  const startCreating = useCallback(() => setCreating(true), []);
  const stopCreating = useCallback(() => setCreating(false), []);
  return (
    <>
      {owner &&
        (creating ? (
          <NameForm label="Team name" verb="Create team" send={create} done={stopCreating} />
        ) : (
          <div className="actions">
            <button type="button" className="button" onClick={startCreating}>
              New team
            </button>
          </div>
        ))}
      {teams.data !== null && <Stale error={teams.error} what="teams" />}
      {owner &&
        (members.data === null ? (
          members.error && <Loading error={members.error} what="the members to add to teams" />
        ) : (
          <Stale error={members.error} what="the members to add to teams" />
        ))}
      {teams.data === null ? (
        <Loading error={teams.error} what="teams" />
      ) : teams.data.length === 0 ? (
        <p className="muted">
          {owner
            ? "No teams yet. Make one, then give it access to projects."
            : "No teams yet. Owners make them."}
        </p>
      ) : (
        <ul className="roster" aria-label="Teams">
          {teams.data.map((team) => (
            <TeamRow
              key={team.id}
              org={org}
              team={team}
              members={members.data ?? NO_MEMBERS}
              changed={teams.again}
            />
          ))}
        </ul>
      )}
    </>
  );
}

function TeamRow(props: { org: Org; team: Team; members: readonly Member[]; changed: () => void }) {
  const { org, team, changed } = props;
  const owner = org.role === "owner";
  const [renaming, setRenaming] = useState(false);
  const [open, setOpen] = useState(false);
  const rename = useCallback(
    async (name: string) => {
      await renameTeam(org.id, team.id, name);
      changed();
    },
    [org.id, team.id, changed],
  );
  const remove = useCallback(async () => {
    await deleteTeam(org.id, team.id);
    changed();
  }, [org.id, team.id, changed]);
  const startRenaming = useCallback(() => setRenaming(true), []);
  const stopRenaming = useCallback(() => setRenaming(false), []);
  const toggle = useCallback(() => setOpen(!open), [open]);
  return (
    <li>
      {renaming ? (
        <NameForm
          label={`New name for ${team.name}`}
          initial={team.name}
          verb="Rename"
          send={rename}
          done={stopRenaming}
        />
      ) : (
        <>
          <span className="name">{team.name}</span>
          <span className="end">
            <button type="button" className="button small" aria-expanded={open} onClick={toggle}>
              Members
            </button>
            {owner && (
              <>
                <button type="button" className="button small" onClick={startRenaming}>
                  Rename
                </button>
                <Confirm
                  label="Delete"
                  question={`Delete ${team.name}? The team loses access to all its projects.`}
                  act={remove}
                />
              </>
            )}
          </span>
        </>
      )}
      {open && <TeamMembers org={org} team={team} members={props.members} />}
    </li>
  );
}

function TeamMembers(props: { org: Org; team: Team; members: readonly Member[] }) {
  const { org, team } = props;
  const owner = org.role === "owner";
  const people = useLoaded(() => teamMembers(org.id, team.id), `${org.id}:${team.id}`);
  const pickId = useId();
  const [pick, setPick] = useState("");
  const { working, error, run } = useAction();
  const outside = props.members.filter(
    (member) => !people.data?.some((each) => each.id === member.id),
  );
  const chosen = outside.some((member) => String(member.id) === pick) ? pick : "";
  const again = people.again;
  const add = useCallback(
    (event: FormEvent) => {
      event.preventDefault();
      run(async () => {
        await addToTeam(org.id, team.id, Number(chosen));
        setPick("");
        again();
      });
    },
    [run, org.id, team.id, chosen, again],
  );
  const choose = useCallback(
    (event: ChangeEvent<HTMLSelectElement>) => setPick(event.target.value),
    [],
  );
  if (people.data === null) {
    return (
      <div className="nested wide">
        <Loading error={people.error} what="the team's members" />
      </div>
    );
  }
  return (
    <div className="nested wide">
      <Stale error={people.error} what="the team's members" />
      {people.data.length === 0 ? (
        <p className="muted">Nobody is in {team.name} yet.</p>
      ) : (
        <ul className="roster" aria-label={`Members of ${team.name}`}>
          {people.data.map((person) => (
            <TeamMemberRow
              key={person.id}
              org={org.id}
              team={team}
              person={person}
              owner={owner}
              changed={again}
            />
          ))}
        </ul>
      )}
      {owner && outside.length > 0 && (
        <form className="inline-form" onSubmit={add}>
          <label htmlFor={pickId}>Add</label>
          <select id={pickId} className="input compact" value={chosen} onChange={choose}>
            <option value="">a member…</option>
            {outside.map((member) => (
              <option key={member.id} value={member.id}>
                {member.username}
              </option>
            ))}
          </select>
          <button type="submit" className="button small" disabled={working || chosen === ""}>
            Add to team
          </button>
          {error && (
            <span className="error" role="alert">
              {error}
            </span>
          )}
        </form>
      )}
    </div>
  );
}

function TeamMemberRow({
  org,
  team,
  person,
  owner,
  changed,
}: {
  org: number;
  team: Team;
  person: TeamMember;
  owner: boolean;
  changed: () => void;
}) {
  const remove = useCallback(async () => {
    await removeFromTeam(org, team.id, person.id);
    changed();
  }, [org, team.id, person.id, changed]);
  return (
    <li>
      <span className="name">{person.username}</span>
      {owner && (
        <span className="end">
          <Confirm
            label="Remove"
            question={`Take ${person.username} out of ${team.name}?`}
            act={remove}
          />
        </span>
      )}
    </li>
  );
}

function Settings(props: { org: Org; pane: PaneId }) {
  const { org, pane } = props;
  const [renaming, setRenaming] = useState(false);
  const rename = useCallback(
    async (name: string) => {
      await updateOrg(org.id, { name });
      await reload();
    },
    [org.id],
  );
  const remove = useCallback(async () => {
    await deleteOrg(org.id);
    await reload();
    leave(pane);
  }, [org.id, pane]);
  const startRenaming = useCallback(() => setRenaming(true), []);
  const stopRenaming = useCallback(() => setRenaming(false), []);
  if (org.role !== "owner") {
    return (
      <p>
        Every member gets {PERMISSION_LABEL[org.base_permission].toLowerCase()} on every project.
        Teams can give more. Only owners change this.
      </p>
    );
  }
  return (
    <>
      <h2>Name</h2>
      {renaming ? (
        <NameForm
          label="Organization name"
          initial={org.name}
          verb="Rename"
          send={rename}
          done={stopRenaming}
        />
      ) : (
        <div className="actions">
          <span className="name">{org.name}</span>
          <button type="button" className="button small" onClick={startRenaming}>
            Rename
          </button>
        </div>
      )}
      <h2>Base permission</h2>
      <BasePermission org={org} />
      <OrgSingleSignOn org={org} />
      <h2>Delete</h2>
      <p className="muted">
        Deleting {org.name} deletes its projects and teams, and removes everyone from it. This
        cannot be undone.
      </p>
      <div className="actions">
        <Confirm label="Delete organization" question={`Delete ${org.name}?`} act={remove} />
      </div>
    </>
  );
}

function BasePermission({ org }: { org: Org }) {
  const id = useId();
  const [chosen, setChosen] = useState<Permission>(org.base_permission);
  const { working, error, run } = useAction();
  const save = useCallback(
    (event: FormEvent) => {
      event.preventDefault();
      run(async () => {
        await updateOrg(org.id, { base_permission: chosen });
        await reload();
      });
    },
    [run, org.id, chosen],
  );
  const choose = useCallback(
    (event: ChangeEvent<HTMLSelectElement>) => setChosen(event.target.value as Permission),
    [],
  );
  return (
    <form className="pane-form" onSubmit={save}>
      <div className="inline-form">
        <div className="field">
          <label htmlFor={id}>Every member gets</label>
          <select id={id} className="input" value={chosen} onChange={choose}>
            {PERMISSIONS.map((each) => (
              <option key={each} value={each}>
                {PERMISSION_LABEL[each]}
              </option>
            ))}
          </select>
        </div>
        <button
          type="submit"
          className="button"
          disabled={working || chosen === org.base_permission}
        >
          Save
        </button>
      </div>
      <span className="note muted">
        This applies to every project in {org.name}. Teams can give more, never less. Owners are
        admins everywhere.
      </span>
      {error && (
        <span className="error" role="alert">
          {error}
        </span>
      )}
    </form>
  );
}
