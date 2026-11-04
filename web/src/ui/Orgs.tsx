// Organizations and projects on the services backend, for a signed-in
// person on a page served on its own: the Projects pane, the Organization
// pane, and the Account pane's organizations and invitations. Each control
// shows only for someone services would let use it, and when services
// refuses anyway, its reason shows where the control was.

import { type FormEvent, type ReactNode, useEffect, useId, useRef, useState } from "react";
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
  // biome-ignore lint/correctness/useExhaustiveDependencies: only the ask matters
  useEffect(() => {
    if (wanted) {
      asked.setState({ form: null });
      open();
    }
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

/** One request a control makes: whether it is in flight, and why services
 * refused it. */
export function useAction() {
  const [working, setWorking] = useState(false);
  const [error, setError] = useState<string | null>(null);
  const run = (send: () => Promise<unknown>) => {
    setWorking(true);
    setError(null);
    send()
      .catch((failure: Error) => setError(failure.message))
      .finally(() => setWorking(false));
  };
  return { working, error, run, clear: () => setError(null) };
}

/** An answer a pane asks for itself, again whenever `key` changes or it
 * calls `again`. */
export function useLoaded<T>(load: () => Promise<T>, key: string) {
  const [state, setState] = useState<{ data: T | null; error: string | null }>({
    data: null,
    error: null,
  });
  const [round, setRound] = useState(0);
  // biome-ignore lint/correctness/useExhaustiveDependencies: `key` names what `load` asks
  useEffect(() => {
    let live = true;
    load().then(
      (data) => live && setState({ data, error: null }),
      (failure: Error) => live && setState((before) => ({ ...before, error: failure.message })),
    );
    return () => {
      live = false;
    };
  }, [key, round]);
  return { ...state, again: () => setRound((round) => round + 1) };
}

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
            <button type="button" className="button" onClick={() => showPane(KIND.account)}>
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
  const submit = (event: FormEvent) => {
    event.preventDefault();
    run(async () => {
      await props.send(name.trim());
      props.done();
    });
  };
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
          onChange={(event) => setName(event.target.value)}
          onKeyDown={(event) => {
            if (event.key === "Escape") {
              event.stopPropagation();
              props.done();
            }
          }}
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
        <button type="button" className="link-button" onClick={props.done}>
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
  if (!asking) {
    return (
      <button type="button" className="button small" onClick={() => setAsking(true)}>
        {props.label}
      </button>
    );
  }
  return (
    <span className="confirm">
      <span>{props.question}</span>
      <button
        type="button"
        className="button small danger"
        disabled={working}
        onClick={() => run(props.act)}
      >
        {props.label}
      </button>
      <button type="button" className="link-button" onClick={() => setAsking(false)}>
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
  return (
    <>
      <select
        className="input compact"
        aria-label={props.label}
        value={props.value}
        disabled={working}
        onChange={(event) => run(() => props.send(event.target.value as T))}
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
  useAsked("project", () => setCreating(true));
  if (now.kind !== "in") {
    return <SignedOut now={now} what="Projects" />;
  }
  const { orgs, projects } = directory;
  const owned = (orgs ?? []).filter((org) => org.role === "owner");
  return (
    <div className="pane-body pane-text projects">
      {creating ? (
        <NewProject owners={owned} done={() => setCreating(false)} />
      ) : (
        <div className="actions">
          <button type="button" className="button" onClick={() => setCreating(true)}>
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
                  <button
                    type="button"
                    className="link-button"
                    aria-label={`Open ${orgLabel(group.org, orgs)}`}
                    onClick={() => group.org && showOrg(group.org.id, group.org.name)}
                  >
                    Open organization
                  </button>
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

/** Asks for a new project's name and, for an owner of any organization,
 * whose it is: the person's own by default. */
function NewProject(props: { owners: readonly Org[]; fixed?: Org; done: () => void }) {
  const ownerId = useId();
  const orgs = useStore(directoryStore, (state) => state.orgs);
  const [owner, setOwner] = useState(props.fixed ? String(props.fixed.id) : "");
  return (
    <NameForm
      label="Project name"
      verb="Create project"
      send={async (name) => {
        await createProject(name, owner === "" ? null : Number(owner));
        await reload();
      }}
      done={props.done}
    >
      {!props.fixed && props.owners.length > 0 && (
        <div className="field">
          <label htmlFor={ownerId}>Owner</label>
          <select
            id={ownerId}
            className="input"
            value={owner}
            onChange={(event) => setOwner(event.target.value)}
          >
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
  return (
    <li>
      {renaming ? (
        <NameForm
          label={`New name for ${project.name}`}
          initial={project.name}
          verb="Rename"
          send={async (name) => {
            await renameProject(project.id, name);
            await reload();
          }}
          done={() => setRenaming(false)}
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
                onClick={() => setTeams(!teams)}
              >
                Team access
              </button>
            )}
            {admin && (
              <>
                <button type="button" className="button small" onClick={() => setRenaming(true)}>
                  Rename
                </button>
                <Confirm
                  label="Delete"
                  question={`Delete ${project.name}? This cannot be undone.`}
                  act={async () => {
                    await deleteProject(project.id);
                    await reload();
                  }}
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
  if (granted.data === null) {
    return (
      <div className="nested wide">
        <Loading error={granted.error} what="team access" />
      </div>
    );
  }
  const ungranted = (teams.data ?? []).filter(
    (each) => !granted.data?.some((given) => given.team.id === each.id),
  );
  const chosen = ungranted.some((each) => String(each.id) === team) ? team : "";
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
            <li key={given.team.id}>
              <span className="name">{given.team.name}</span>
              <span className="end">
                {admin ? (
                  <>
                    <Choice
                      label={`Access for ${given.team.name}`}
                      value={given.permission}
                      options={GRANTS}
                      names={PERMISSION_LABEL}
                      send={async (permission) => {
                        await grantTeam(project.id, given.team.id, permission);
                        granted.again();
                      }}
                    />
                    <Confirm
                      label="Remove"
                      question={`Take ${given.team.name}'s access away?`}
                      act={async () => {
                        await revokeTeam(project.id, given.team.id);
                        granted.again();
                      }}
                    />
                  </>
                ) : (
                  <span className="muted">{PERMISSION_LABEL[given.permission]}</span>
                )}
              </span>
            </li>
          ))}
        </ul>
      )}
      {admin && teams.error && <p className="error">Could not load teams: {teams.error}.</p>}
      {admin && ungranted.length > 0 && (
        <form
          className="inline-form"
          onSubmit={(event) => {
            event.preventDefault();
            run(async () => {
              await grantTeam(project.id, Number(chosen), grant);
              setTeam("");
              granted.again();
            });
          }}
        >
          <label htmlFor={teamId}>Give</label>
          <select
            id={teamId}
            className="input compact"
            value={chosen}
            onChange={(event) => setTeam(event.target.value)}
          >
            <option value="">a team…</option>
            {ungranted.map((each) => (
              <option key={each.id} value={each.id}>
                {each.name}
              </option>
            ))}
          </select>
          <select
            id={grantId}
            className="input compact"
            value={grant}
            onChange={(event) => setGrant(event.target.value as Grant)}
          >
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

// The Account pane's organizations and invitations

/** The person's invitations and organizations, under their account. */
export function Organizations() {
  const directory = useDirectory();
  const [creating, setCreating] = useState(false);
  useAsked("org", () => setCreating(true));
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
              <button
                type="button"
                className="link-button name"
                onClick={() => showOrg(org.id, org.name)}
              >
                {orgLabel(org, orgs)}
              </button>
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
          send={async (name) => {
            const made = await createOrg(name);
            await reload();
            showOrg(made.id, made.name);
          }}
          done={() => setCreating(false)}
        />
      ) : (
        <div className="actions">
          <button type="button" className="button" onClick={() => setCreating(true)}>
            New organization
          </button>
        </div>
      )}
    </>
  );
}

function InvitationRow({
  invitation,
}: {
  invitation: { org: { id: number; name: string }; role: Role; invited_by: string | null };
}) {
  const { working, error, run } = useAction();
  const { org } = invitation;
  const orgs = useStore(directoryStore, (state) => state.orgs);
  return (
    <li>
      <span>
        <b>{orgLabel(org, orgs)}</b> invites you as{" "}
        {invitation.role === "owner" ? "an owner" : "a member"}
        {invitation.invited_by && <span className="muted"> (from {invitation.invited_by})</span>}
      </span>
      <span className="end">
        <button
          type="button"
          className="button small primary"
          disabled={working}
          onClick={() =>
            run(async () => {
              await acceptInvitation(org.id);
              await reload();
            })
          }
        >
          Accept
        </button>
        <button
          type="button"
          className="button small"
          disabled={working}
          onClick={() =>
            run(async () => {
              await declineInvitation(org.id);
              await reload();
            })
          }
        >
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
  const params = orgParams(pane);
  // Kept in the layout too, so the pane opens where it was left.
  const [section, setSection] = useState(params.section);
  const keep = (changes: Partial<typeof params>) =>
    change((layout) => setParams(layout, id, { ...params, ...changes } as Json));
  const org = directory.orgs?.find((each) => each.id === orgId);
  useEffect(() => {
    void checkAccount();
  }, []);
  const label = org ? orgLabel(org, directory.orgs) : "";
  // The tab shows the name services last gave, told apart from a twin.
  // biome-ignore lint/correctness/useExhaustiveDependencies: only a new name matters
  useEffect(() => {
    if (org && label !== params.name) {
      keep({ name: label });
    }
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
          <button
            key={each}
            type="button"
            aria-pressed={section === each}
            onClick={() => {
              setSection(each);
              keep({ section: each });
            }}
          >
            {SECTION_LABEL[each]}
          </button>
        ))}
      </fieldset>
      {section === "members" ? (
        <Members org={org} me={now.user.id} pane={id} />
      ) : section === "projects" ? (
        <OrgProjects
          org={org}
          projects={directory.projects ?? []}
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
              changed={async (left) => {
                if (left) {
                  await reload();
                  leave(pane);
                  return;
                }
                members.again();
                // Someone's own role decides what they may do next.
                if (member.id === me) {
                  await reload();
                }
              }}
            />
          ))}
        </ul>
      )}
      {owner && invitations.data && invitations.data.length > 0 && (
        <>
          <h2>Invited</h2>
          <ul className="roster" aria-label="Invited">
            {invitations.data.map((each) => (
              <li key={each.user_id}>
                <span className="name">{each.username}</span>
                <span className="muted">{ROLE_LABEL[each.role]}</span>
                <span className="end">
                  <Confirm
                    label="Cancel invitation"
                    question={`Cancel ${each.username}'s invitation?`}
                    act={async () => {
                      await cancelInvitation(org.id, each.user_id);
                      invitations.again();
                    }}
                  />
                </span>
              </li>
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

function InviteForm({ org, invited }: { org: Org; invited: () => void }) {
  const nameId = useId();
  const roleId = useId();
  const [username, setUsername] = useState("");
  const [role, setRoleChoice] = useState<Role>("member");
  const [sent, setSent] = useState<string | null>(null);
  const { working, error, run } = useAction();
  return (
    <form
      className="pane-form"
      onSubmit={(event) => {
        event.preventDefault();
        const name = username.trim();
        setSent(null);
        run(async () => {
          await invite(org.id, name, role);
          setUsername("");
          setSent(name);
          invited();
        });
      }}
    >
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
            onChange={(event) => setUsername(event.target.value)}
          />
        </div>
        <div className="field">
          <label htmlFor={roleId}>Role</label>
          <select
            id={roleId}
            className="input"
            value={role}
            onChange={(event) => setRoleChoice(event.target.value as Role)}
          >
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
  changed: (left: boolean) => Promise<void>;
}) {
  const { org, member, me } = props;
  const owner = org.role === "owner";
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
            send={async (role) => {
              await setRole(org.id, member.id, role);
              await props.changed(false);
            }}
          />
        ) : (
          <span className="muted">{ROLE_LABEL[member.role]}</span>
        )}
        {me ? (
          <Confirm
            label="Leave"
            question={`Leave ${org.name}? Only an owner can invite you back.`}
            act={async () => {
              await removeMember(org.id, member.id);
              await props.changed(true);
            }}
          />
        ) : (
          owner && (
            <Confirm
              label="Remove"
              question={`Remove ${member.username} from ${org.name}?`}
              act={async () => {
                await removeMember(org.id, member.id);
                await props.changed(false);
              }}
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
  return (
    <>
      {owner &&
        (creating ? (
          <NewProject owners={[]} fixed={org} done={() => setCreating(false)} />
        ) : (
          <div className="actions">
            <button type="button" className="button" onClick={() => setCreating(true)}>
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
  return (
    <>
      {owner &&
        (creating ? (
          <NameForm
            label="Team name"
            verb="Create team"
            send={async (name) => {
              await createTeam(org.id, name);
              teams.again();
            }}
            done={() => setCreating(false)}
          />
        ) : (
          <div className="actions">
            <button type="button" className="button" onClick={() => setCreating(true)}>
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
              members={members.data ?? []}
              changed={teams.again}
            />
          ))}
        </ul>
      )}
    </>
  );
}

function TeamRow(props: { org: Org; team: Team; members: readonly Member[]; changed: () => void }) {
  const { org, team } = props;
  const owner = org.role === "owner";
  const [renaming, setRenaming] = useState(false);
  const [open, setOpen] = useState(false);
  return (
    <li>
      {renaming ? (
        <NameForm
          label={`New name for ${team.name}`}
          initial={team.name}
          verb="Rename"
          send={async (name) => {
            await renameTeam(org.id, team.id, name);
            props.changed();
          }}
          done={() => setRenaming(false)}
        />
      ) : (
        <>
          <span className="name">{team.name}</span>
          <span className="end">
            <button
              type="button"
              className="button small"
              aria-expanded={open}
              onClick={() => setOpen(!open)}
            >
              Members
            </button>
            {owner && (
              <>
                <button type="button" className="button small" onClick={() => setRenaming(true)}>
                  Rename
                </button>
                <Confirm
                  label="Delete"
                  question={`Delete ${team.name}? Its access to projects goes with it.`}
                  act={async () => {
                    await deleteTeam(org.id, team.id);
                    props.changed();
                  }}
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
  if (people.data === null) {
    return (
      <div className="nested wide">
        <Loading error={people.error} what="the team's members" />
      </div>
    );
  }
  const outside = props.members.filter(
    (member) => !people.data?.some((each) => each.id === member.id),
  );
  const chosen = outside.some((member) => String(member.id) === pick) ? pick : "";
  return (
    <div className="nested wide">
      <Stale error={people.error} what="the team's members" />
      {people.data.length === 0 ? (
        <p className="muted">Nobody is in {team.name} yet.</p>
      ) : (
        <ul className="roster" aria-label={`Members of ${team.name}`}>
          {people.data.map((person) => (
            <li key={person.id}>
              <span className="name">{person.username}</span>
              {owner && (
                <span className="end">
                  <Confirm
                    label="Remove"
                    question={`Take ${person.username} out of ${team.name}?`}
                    act={async () => {
                      await removeFromTeam(org.id, team.id, person.id);
                      people.again();
                    }}
                  />
                </span>
              )}
            </li>
          ))}
        </ul>
      )}
      {owner && outside.length > 0 && (
        <form
          className="inline-form"
          onSubmit={(event) => {
            event.preventDefault();
            run(async () => {
              await addToTeam(org.id, team.id, Number(chosen));
              setPick("");
              people.again();
            });
          }}
        >
          <label htmlFor={pickId}>Add</label>
          <select
            id={pickId}
            className="input compact"
            value={chosen}
            onChange={(event) => setPick(event.target.value)}
          >
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

function Settings(props: { org: Org; pane: PaneId }) {
  const { org } = props;
  const [renaming, setRenaming] = useState(false);
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
          send={async (name) => {
            await updateOrg(org.id, { name });
            await reload();
          }}
          done={() => setRenaming(false)}
        />
      ) : (
        <div className="actions">
          <span className="name">{org.name}</span>
          <button type="button" className="button small" onClick={() => setRenaming(true)}>
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
        <Confirm
          label="Delete organization"
          question={`Delete ${org.name}?`}
          act={async () => {
            await deleteOrg(org.id);
            await reload();
            leave(props.pane);
          }}
        />
      </div>
    </>
  );
}

function BasePermission({ org }: { org: Org }) {
  const id = useId();
  const [chosen, setChosen] = useState<Permission>(org.base_permission);
  const { working, error, run } = useAction();
  return (
    <form
      className="pane-form"
      onSubmit={(event) => {
        event.preventDefault();
        run(async () => {
          await updateOrg(org.id, { base_permission: chosen });
          await reload();
        });
      }}
    >
      <div className="inline-form">
        <div className="field">
          <label htmlFor={id}>Every member gets</label>
          <select
            id={id}
            className="input"
            value={chosen}
            onChange={(event) => setChosen(event.target.value as Permission)}
          >
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
