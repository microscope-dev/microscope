// Organizations, their members and teams, and projects, on the services
// backend (account.ts). Everyone has a personal organization that services
// never shows: its projects are the person's own, with no organization.
//
// Services decides who may do what on every request. The role and
// permission each answer carries only tell the page which controls to show.

import { useEffect } from "react";
import { createStore, useStore } from "zustand";
import { type Account, account, failure, useAccount } from "./account";

export type Permission = "none" | "read" | "write" | "admin";
/** What a team can be given on a project, and what anyone who can see a
 * project has on it. */
export type Grant = Exclude<Permission, "none">;
export type Role = "owner" | "member";

export interface Org {
  id: number;
  name: string;
  base_permission: Permission;
  role: Role;
}

export interface Member {
  id: number;
  username: string;
  role: Role;
}

/** Someone an organization's owners invited, as they see it. */
export interface Invitation {
  user_id: number;
  username: string;
  role: Role;
}

/** An invitation to the signed-in person. */
export interface MyInvitation {
  org: { id: number; name: string };
  role: Role;
  invited_by: string | null;
}

export interface Project {
  id: number;
  name: string;
  /** Null for the person's own. */
  org: { id: number; name: string } | null;
  permission: Grant;
}

export interface Team {
  id: number;
  name: string;
}

export interface TeamMember {
  id: number;
  username: string;
}

export interface ProjectTeam {
  team: Team;
  permission: Grant;
}

export const PERMISSIONS: readonly Permission[] = ["none", "read", "write", "admin"];
export const GRANTS: readonly Grant[] = ["read", "write", "admin"];
export const ROLES: readonly Role[] = ["member", "owner"];

export const PERMISSION_LABEL: Record<Permission, string> = {
  none: "No access",
  read: "Read",
  write: "Write",
  admin: "Admin",
};

export const ROLE_LABEL: Record<Role, string> = { owner: "Owner", member: "Member" };

export async function request<T>(method: string, path: string, body?: unknown): Promise<T> {
  const response = await fetch(path, {
    method,
    credentials: "same-origin",
    ...(body === undefined
      ? {}
      : { headers: { "Content-Type": "application/json" }, body: JSON.stringify(body) }),
  });
  if (!response.ok) {
    throw await failure(response);
  }
  return (response.status === 204 ? undefined : await response.json()) as T;
}

const org = (id: number) => `/api/orgs/${id}`;
const project = (id: number) => `/api/projects/${id}`;

export const listOrgs = () => request<Org[]>("GET", "/api/orgs");
export const createOrg = (name: string) => request<Org>("POST", "/api/orgs", { name });
export const updateOrg = (id: number, change: { name?: string; base_permission?: Permission }) =>
  request<Org>("PATCH", org(id), change);
export const deleteOrg = (id: number) => request<void>("DELETE", org(id));

export const listMembers = (id: number) => request<Member[]>("GET", `${org(id)}/members`);
export const setRole = (id: number, user: number, role: Role) =>
  request<Member>("PATCH", `${org(id)}/members/${user}`, { role });
/** Removes someone, or, for the person's own id, leaves. */
export const removeMember = (id: number, user: number) =>
  request<void>("DELETE", `${org(id)}/members/${user}`);

export const listInvitations = (id: number) =>
  request<Invitation[]>("GET", `${org(id)}/invitations`);
export const invite = (id: number, username: string, role: Role) =>
  request<Invitation>("POST", `${org(id)}/invitations`, { username, role });
export const cancelInvitation = (id: number, user: number) =>
  request<void>("DELETE", `${org(id)}/invitations/${user}`);

export const myInvitations = () => request<MyInvitation[]>("GET", "/api/users/me/invitations");
// An empty JSON body, so the browser asks before sending either from
// another site's page, as it does for every other change.
export const acceptInvitation = (id: number) =>
  request<Org>("POST", `/api/invitations/${id}/accept`, {});
export const declineInvitation = (id: number) =>
  request<void>("POST", `/api/invitations/${id}/decline`, {});

export const listProjects = () => request<Project[]>("GET", "/api/projects");
/** A project of the person's own when `orgId` is null. */
export const createProject = (name: string, orgId: number | null) =>
  request<Project>("POST", "/api/projects", orgId === null ? { name } : { name, org_id: orgId });
export const renameProject = (id: number, name: string) =>
  request<Project>("PATCH", project(id), { name });
export const deleteProject = (id: number) => request<void>("DELETE", project(id));

export const projectTeams = (id: number) => request<ProjectTeam[]>("GET", `${project(id)}/teams`);
export const grantTeam = (id: number, team: number, permission: Grant) =>
  request<ProjectTeam>("PUT", `${project(id)}/teams/${team}`, { permission });
export const revokeTeam = (id: number, team: number) =>
  request<void>("DELETE", `${project(id)}/teams/${team}`);

export const listTeams = (id: number) => request<Team[]>("GET", `${org(id)}/teams`);
export const createTeam = (id: number, name: string) =>
  request<Team>("POST", `${org(id)}/teams`, { name });
export const renameTeam = (id: number, team: number, name: string) =>
  request<Team>("PATCH", `${org(id)}/teams/${team}`, { name });
export const deleteTeam = (id: number, team: number) =>
  request<void>("DELETE", `${org(id)}/teams/${team}`);
export const teamMembers = (id: number, team: number) =>
  request<TeamMember[]>("GET", `${org(id)}/teams/${team}/members`);
export const addToTeam = (id: number, team: number, user: number) =>
  request<void>("PUT", `${org(id)}/teams/${team}/members/${user}`);
export const removeFromTeam = (id: number, team: number, user: number) =>
  request<void>("DELETE", `${org(id)}/teams/${team}/members/${user}`);

/** What the signed-in person belongs to, can read, and is invited to,
 * which the Account, Projects, and Organization panes share. */
export interface Directory {
  /** Whose these are, or null when nobody is signed in. */
  readonly user: number | null;
  readonly orgs: readonly Org[] | null;
  readonly projects: readonly Project[] | null;
  readonly invitations: readonly MyInvitation[] | null;
  readonly error: string | null;
}

const EMPTY: Directory = { user: null, orgs: null, projects: null, invitations: null, error: null };

export const directory = createStore<Directory>(() => EMPTY);

/** Counts reloads, so an older answer never replaces a newer one. */
let asked = 0;
let inFlight: Promise<void> | null = null;

const signedInAs = (now: Account): number | null => (now.kind === "in" ? now.user.id : null);

// When someone else signs in, or nobody is, the last person's answers go
// before anything renders, and a question asked for them is not shared with
// the next person.
account.subscribe((state, before) => {
  const user = signedInAs(state.now);
  if (user !== signedInAs(before.now)) {
    asked += 1;
    inFlight = null;
    directory.setState({ ...EMPTY, user }, true);
  }
});

/** Asks services for the directory again, after a change to it. */
export function reload(): Promise<void> {
  const now = account.getState().now;
  if (now.kind !== "in") {
    directory.setState(EMPTY, true);
    return Promise.resolve();
  }
  const user = now.user.id;
  if (directory.getState().user !== user) {
    // Someone else's answers never show while this person's load.
    directory.setState({ ...EMPTY, user }, true);
  }
  asked += 1;
  const mine = asked;
  const current = () => {
    const signed = account.getState().now;
    return mine === asked && signed.kind === "in" && signed.user.id === user;
  };
  const loading = Promise.all([listOrgs(), listProjects(), myInvitations()])
    .then(([orgs, projects, invitations]) => {
      if (current()) {
        directory.setState({ orgs, projects, invitations, error: null });
      }
    })
    .catch((error: Error) => {
      if (current()) {
        directory.setState({ error: error.message });
      }
    })
    .finally(() => {
      if (inFlight === loading) {
        inFlight = null;
      }
    });
  inFlight = loading;
  return loading;
}

/** The directory, asked for when a pane showing it opens and whenever
 * someone else signs in. Panes opening together share one question. */
export function useDirectory(): Directory {
  const user = signedInAs(useAccount());
  // biome-ignore lint/correctness/useExhaustiveDependencies: asks again only for another person
  useEffect(() => {
    void (inFlight ?? reload());
  }, [user]);
  return useStore(directory);
}

/** An organization's name, with its id when another the person is in has
 * the same name, since names are labels and need not be unique. */
export function orgLabel(org: { id: number; name: string }, orgs: readonly Org[] | null): string {
  const name = org.name.toLowerCase();
  const twin = (orgs ?? []).some((each) => each.id !== org.id && each.name.toLowerCase() === name);
  return twin ? `${org.name} (#${org.id})` : org.name;
}

/** The projects of one owner: the person's own when `org` is null. */
export interface ProjectGroup {
  readonly org: { id: number; name: string; role: Role | null } | null;
  readonly projects: readonly Project[];
}

const byName = (a: { name: string; id: number }, b: { name: string; id: number }) =>
  a.name.localeCompare(b.name) || a.id - b.id;

/**
 * The person's own projects first, then each organization's by its name.
 * Organizations are told apart by id, since two can share a name. Every
 * organization the person is in has a group, even with no projects they
 * can see, so an owner sees where to make one.
 */
export function groupProjects(
  projects: readonly Project[],
  orgs: readonly Org[],
): readonly ProjectGroup[] {
  const own: Project[] = [];
  const shared = new Map<number, { org: NonNullable<ProjectGroup["org"]>; projects: Project[] }>();
  for (const each of orgs) {
    shared.set(each.id, { org: { id: each.id, name: each.name, role: each.role }, projects: [] });
  }
  for (const each of projects) {
    if (each.org === null) {
      own.push(each);
      continue;
    }
    let group = shared.get(each.org.id);
    if (!group) {
      group = { org: { ...each.org, role: null }, projects: [] };
      shared.set(each.org.id, group);
    }
    group.projects.push(each);
  }
  const groups = [...shared.values()].sort((a, b) => byName(a.org, b.org));
  return [
    { org: null, projects: own.sort(byName) },
    ...groups.map((group) => ({ org: group.org, projects: group.projects.sort(byName) })),
  ];
}
