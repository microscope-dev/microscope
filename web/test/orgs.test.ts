// How the page asks services about organizations and projects, and how it
// orders projects: the person's own first, then each organization's.

import { afterEach, describe, expect, it, vi } from "vitest";
import { account, Refused } from "../src/account";
import {
  createProject,
  directory,
  groupProjects,
  listOrgs,
  type Org,
  orgLabel,
  type Project,
  reload,
  removeMember,
} from "../src/orgs";

const org = (id: number, name: string, role: Org["role"] = "member"): Org => ({
  id,
  name,
  base_permission: "read",
  role,
});

const project = (
  id: number,
  name: string,
  owner: { id: number; name: string } | null,
  permission: Project["permission"] = "read",
): Project => ({ id, name, org: owner, permission });

afterEach(() => {
  vi.unstubAllGlobals();
  account.setState({ now: { kind: "checking" } });
  directory.setState(
    { user: null, orgs: null, projects: null, invitations: null, error: null },
    true,
  );
});

describe("grouping projects", () => {
  it("puts the person's own first, then organizations by name, each sorted", () => {
    const acme = { id: 2, name: "acme" };
    const zeta = { id: 1, name: "Zeta" };
    const groups = groupProjects(
      [
        project(5, "web", zeta),
        project(4, "b", null, "admin"),
        project(3, "api", acme),
        project(2, "a", null, "admin"),
        project(1, "Aardvark", acme),
      ],
      [org(1, "Zeta"), org(2, "acme")],
    );
    expect(groups.map((group) => group.org?.name ?? null)).toEqual([null, "acme", "Zeta"]);
    expect(groups.map((group) => group.projects.map((each) => each.name))).toEqual([
      ["a", "b"],
      ["Aardvark", "api"],
      ["web"],
    ]);
  });

  it("keeps two organizations of the same name apart", () => {
    const groups = groupProjects(
      [project(1, "one", { id: 7, name: "ops" }), project(2, "two", { id: 3, name: "ops" })],
      [org(7, "ops", "owner"), org(3, "ops")],
    );
    expect(groups.map((group) => [group.org?.id, group.org?.role])).toEqual([
      [undefined, undefined],
      [3, "member"],
      [7, "owner"],
    ]);
    expect(groups.map((group) => group.projects.map((each) => each.id))).toEqual([[], [2], [1]]);
  });

  it("shows an organization with nothing the person can see, and the person's own when empty", () => {
    const groups = groupProjects([], [org(1, "acme", "owner")]);
    expect(groups).toEqual([
      { org: null, projects: [] },
      { org: { id: 1, name: "acme", role: "owner" }, projects: [] },
    ]);
  });

  it("still shows a project whose organization the list of organizations lacks", () => {
    const groups = groupProjects([project(1, "late", { id: 9, name: "new" })], []);
    expect(groups[1]).toEqual({
      org: { id: 9, name: "new", role: null },
      projects: [project(1, "late", { id: 9, name: "new" })],
    });
  });
});

/** Answers each request with `answer`, and keeps what was asked. */
function serve(answer: (method: string, path: string) => Response) {
  const asked: { method: string; path: string; body: unknown }[] = [];
  vi.stubGlobal(
    "fetch",
    vi.fn(async (path: string, init: RequestInit = {}) => {
      const method = init.method ?? "GET";
      asked.push({
        method,
        path,
        body: typeof init.body === "string" ? JSON.parse(init.body) : undefined,
      });
      return answer(method, path);
    }),
  );
  return asked;
}

const json = (body: unknown, status = 200) =>
  new Response(JSON.stringify(body), { status, headers: { "Content-Type": "application/json" } });

describe("the client", () => {
  it("makes a project of the person's own without an organization, and an organization's with one", async () => {
    const asked = serve(() => json(project(1, "x", null, "admin"), 201));
    await createProject("mine", null);
    await createProject("theirs", 4);
    expect(asked.map((each) => each.body)).toEqual([
      { name: "mine" },
      { name: "theirs", org_id: 4 },
    ]);
  });

  it("reads nothing from an answer with no content", async () => {
    const asked = serve(() => new Response(null, { status: 204 }));
    await expect(removeMember(3, 8)).resolves.toBeUndefined();
    expect(asked).toEqual([{ method: "DELETE", path: "/api/orgs/3/members/8", body: undefined }]);
  });

  it("gives services' reason and status when it refuses", async () => {
    serve(() => json({ error: "a project of that name exists" }, 409));
    const refused = await createProject("dup", null).catch((error: unknown) => error);
    expect(refused).toBeInstanceOf(Refused);
    expect(refused).toMatchObject({ message: "a project of that name exists", status: 409 });
  });

  it("says what status came back when services gives no reason", async () => {
    serve(() => new Response("<html>", { status: 502 }));
    await expect(listOrgs()).rejects.toThrow("the services backend answered 502");
  });
});

describe("the directory", () => {
  const signIn = (id: number) =>
    account.setState({
      now: { kind: "in", user: { id, username: `u${id}`, email: "", totp_enabled: false } },
    });

  it("keeps the newest answer when an older one comes back last", async () => {
    signIn(1);
    const pending: ((orgs: Org[]) => void)[] = [];
    vi.stubGlobal(
      "fetch",
      vi.fn(async (path: string) => {
        if (path === "/api/orgs") {
          return json(await new Promise<Org[]>((resolve) => pending.push(resolve)));
        }
        return json([]);
      }),
    );
    const first = reload();
    const second = reload();
    await vi.waitFor(() => expect(pending).toHaveLength(2));
    pending[1]?.([org(1, "new")]);
    await second;
    pending[0]?.([org(1, "old")]);
    await first;
    expect(directory.getState().orgs?.map((each) => each.name)).toEqual(["new"]);
  });

  it("drops one person's answers when another signs in", async () => {
    signIn(1);
    serve(() => json([org(1, "first's")]));
    await reload();
    expect(directory.getState().orgs).toHaveLength(1);

    signIn(2);
    const held: (() => void)[] = [];
    vi.stubGlobal(
      "fetch",
      vi.fn(async () => {
        await new Promise<void>((resolve) => held.push(resolve));
        return json([]);
      }),
    );
    const loading = reload();
    expect(directory.getState()).toMatchObject({ user: 2, orgs: null, projects: null });
    await vi.waitFor(() => expect(held).toHaveLength(3));
    for (const release of held) {
      release();
    }
    await loading;
    expect(directory.getState()).toMatchObject({ user: 2, orgs: [] });
  });

  it("forgets everything when nobody is signed in", async () => {
    signIn(1);
    serve(() => json([org(1, "acme")]));
    await reload();
    account.setState({ now: { kind: "out" } });
    await reload();
    expect(directory.getState()).toMatchObject({ user: null, orgs: null });
  });
});

describe("naming organizations", () => {
  it("adds the id only where another organization has the same name", () => {
    const orgs = [org(3, "ops"), org(4, "OPS"), org(5, "web")];
    expect(orgs.map((each) => orgLabel(each, orgs))).toEqual(["ops (#3)", "OPS (#4)", "web"]);
    expect(orgLabel(org(3, "ops"), null)).toBe("ops");
  });
});
