// The Projects, Organization, and Account panes against a stand-in for
// services: each shows only the controls its person may use, and says why
// when services refuses.

import { cleanup, render } from "@testing-library/react";
import { afterEach, expect, it, vi } from "vitest";
import { type Locator, page, userEvent } from "vitest/browser";
import { account, checkAccount, type User } from "../src/account";
import { KIND } from "../src/documents";
import { directory, type Member, type Org, type Project } from "../src/orgs";
import { AccountBody } from "../src/ui/Account";
import { OrgBody, ProjectsBody } from "../src/ui/Orgs";
import type { PaneId } from "../src/workbench/layout";

const ME: User = { id: 1, username: "ana", email: "ana@example.com", totp_enabled: false };

type Answer = [number, unknown?];

/** Services as these tests need it: an answer per method and path, and
 * every request kept. `/api/users/me` says who is signed in. */
function services(routes: Record<string, Answer | ((body: unknown) => Answer)>) {
  const asked: string[] = [];
  const real = globalThis.fetch;
  vi.stubGlobal(
    "fetch",
    vi.fn(async (input: RequestInfo | URL, init: RequestInit = {}) => {
      const path = String(input);
      if (!path.startsWith("/api/")) {
        return real(input, init);
      }
      const route = `${init.method ?? "GET"} ${path}`;
      asked.push(route);
      const body = typeof init.body === "string" ? JSON.parse(init.body) : undefined;
      const found = route === "GET /api/users/me" ? ([200, ME] as Answer) : routes[route];
      const [status, answer] = typeof found === "function" ? found(body) : (found ?? [404]);
      return status === 204 || answer === undefined
        ? new Response(null, { status: status === 404 ? 404 : status })
        : new Response(JSON.stringify(answer), {
            status,
            headers: { "Content-Type": "application/json" },
          });
    }),
  );
  account.setState({ now: { kind: "in", user: ME }, notice: null });
  return asked;
}

afterEach(() => {
  cleanup();
  vi.unstubAllGlobals();
  account.setState({ now: { kind: "checking" }, notice: null });
  directory.setState(
    { user: null, orgs: null, projects: null, invitations: null, error: null },
    true,
  );
});

const acme = (role: Org["role"]): Org => ({ id: 7, name: "acme", base_permission: "read", role });
const inAcme = { id: 7, name: "acme" };

/** What a part of the page says, its parts run together. */
const text = (locator: Locator) => locator.query()?.textContent ?? "";

const orgPane = (org: Org) => (
  <OrgBody id={"p1" as PaneId} pane={{ kind: KIND.org, key: String(org.id), params: null }} />
);

const members: Member[] = [
  { id: 1, username: "ana", role: "member" },
  { id: 2, username: "bo", role: "owner" },
];

it("lists the person's own projects first, then each organization's, with their access", async () => {
  const projects: Project[] = [
    { id: 1, name: "mine", org: null, permission: "admin" },
    { id: 2, name: "api", org: inAcme, permission: "write" },
  ];
  services({
    "GET /api/orgs": [200, [acme("member")]],
    "GET /api/projects": [200, projects],
    "GET /api/users/me/invitations": [200, []],
  });
  render(<ProjectsBody />);
  const own = page.getByRole("list", { name: "Your projects" });
  await expect.poll(() => text(own)).toMatch(/mine\s*Admin/);
  await expect.element(own.getByRole("button", { name: "Rename" })).toBeVisible();
  const shared = page.getByRole("list", { name: "acme projects" });
  await expect.poll(() => text(shared)).toMatch(/api\s*Write/);
  // Write is not admin: no renaming or deleting.
  expect(shared.getByRole("button").elements()).toHaveLength(0);
  // Owning no organization, a new project can only be the person's own.
  await userEvent.click(page.getByRole("button", { name: "New project" }));
  await expect.element(page.getByLabelText("Project name")).toHaveFocus();
  expect(page.getByLabelText("Owner").elements()).toHaveLength(0);
});

it("asks an owner whose new project it is, and shows services' reason when it refuses", async () => {
  const asked = services({
    "GET /api/orgs": [200, [acme("owner")]],
    "GET /api/projects": [200, []],
    "GET /api/users/me/invitations": [200, []],
    "POST /api/projects": (body) =>
      (body as { org_id?: number }).org_id === 7
        ? [409, { error: "a project of that name exists" }]
        : [201, { id: 3, name: "x", org: null, permission: "admin" }],
  });
  render(<ProjectsBody />);
  await expect.element(page.getByText("None of your own yet.")).toBeVisible();
  await userEvent.click(page.getByRole("button", { name: "New project" }));
  await expect.element(page.getByLabelText("Owner")).toHaveValue("");
  await userEvent.fill(page.getByLabelText("Project name"), "api");
  await userEvent.selectOptions(page.getByLabelText("Owner"), "acme");
  await userEvent.click(page.getByRole("button", { name: "Create project" }));
  await expect.element(page.getByRole("alert")).toHaveTextContent("a project of that name exists");
  // The form stays, so the name can be changed and tried again.
  await expect.element(page.getByLabelText("Project name")).toHaveValue("api");
  expect(asked.filter((each) => each === "POST /api/projects")).toHaveLength(1);
});

/** An owner's view of acme, open at `section`. */
async function asOwner(section: string) {
  services({
    "GET /api/orgs": [200, [acme("owner")]],
    "GET /api/projects": [
      200,
      [{ id: 2, name: "api", org: inAcme, permission: "admin" }] satisfies Project[],
    ],
    "GET /api/users/me/invitations": [200, []],
    "GET /api/orgs/7/members": [
      200,
      members.map((each) => ({ ...each, role: each.id === 1 ? "owner" : each.role })),
    ],
    "GET /api/orgs/7/invitations": [200, [{ user_id: 3, username: "cy", role: "member" }]],
    "GET /api/orgs/7/teams": [200, [{ id: 4, name: "core" }]],
    "GET /api/orgs/7/teams/4/members": [200, []],
    "GET /api/projects/2/teams": [200, []],
  });
  render(orgPane(acme("owner")));
  await expect.element(page.getByRole("heading", { name: "acme" })).toBeVisible();
  if (section !== "Members") {
    await userEvent.click(page.getByRole("button", { name: section }));
  }
}

it("gives an owner the members' controls and who is invited", async () => {
  await asOwner("Members");
  await expect.element(page.getByLabelText("Username")).toBeVisible();
  await expect.element(page.getByRole("combobox", { name: "Role of bo" })).toBeVisible();
  await expect.element(page.getByRole("button", { name: "Remove" })).toBeVisible();
  await expect.element(page.getByRole("button", { name: "Leave" })).toBeVisible();
  await expect
    .poll(() => text(page.getByRole("list", { name: "Invited" })))
    .toMatch(/cy\s*Member\s*Cancel invitation/);
});

it("gives an owner the projects' controls and each one's team access", async () => {
  await asOwner("Projects");
  await expect.element(page.getByRole("button", { name: "New project" })).toBeVisible();
  await userEvent.click(page.getByRole("button", { name: "Team access" }));
  await expect
    .poll(() => text(page.getByRole("list", { name: "acme projects" })))
    .toContain("No team has access");
  await expect.element(page.getByRole("button", { name: "Give access" })).toBeDisabled();
});

it("gives an owner the teams' controls, adding from the members", async () => {
  await asOwner("Teams");
  await expect.element(page.getByRole("button", { name: "New team" })).toBeVisible();
  await userEvent.click(page.getByRole("button", { name: "Members", exact: true }).last());
  await expect.element(page.getByLabelText("Add")).toBeVisible();
  expect(
    page
      .getByLabelText("Add")
      .getByRole("option")
      .elements()
      .map((each) => each.textContent),
  ).toEqual(["a member…", "ana", "bo"]);
});

it("gives an owner the settings", async () => {
  await asOwner("Settings");
  await expect.element(page.getByLabelText("Every member gets")).toHaveValue("read");
  await expect.element(page.getByRole("button", { name: "Delete organization" })).toBeVisible();
});

it("shows a member what is there but nothing they may not change", async () => {
  services({
    "GET /api/orgs": [200, [acme("member")]],
    "GET /api/projects": [
      200,
      [{ id: 2, name: "api", org: inAcme, permission: "read" }] satisfies Project[],
    ],
    "GET /api/users/me/invitations": [200, []],
    "GET /api/orgs/7/members": [200, members],
    "GET /api/orgs/7/teams": [200, [{ id: 4, name: "core" }]],
    "GET /api/orgs/7/teams/4/members": [200, [{ id: 2, username: "bo" }]],
    "GET /api/projects/2/teams": [200, [{ team: { id: 4, name: "core" }, permission: "write" }]],
  });
  render(orgPane(acme("member")));
  const list = page.getByRole("list", { name: "Members" });
  await expect.poll(() => text(list)).toMatch(/ana \(you\)\s*Member\s*Leave/);
  await expect.poll(() => text(list)).toMatch(/bo\s*Owner/);
  expect(page.getByLabelText("Username").elements()).toHaveLength(0);
  expect(page.getByRole("combobox").elements()).toHaveLength(0);
  expect(page.getByRole("button", { name: "Remove" }).elements()).toHaveLength(0);

  await userEvent.click(page.getByRole("button", { name: "Projects" }));
  expect(page.getByRole("button", { name: "New project" }).elements()).toHaveLength(0);
  await userEvent.click(page.getByRole("button", { name: "Team access" }));
  await expect
    .poll(() => text(page.getByRole("list", { name: "Teams with access to api" })))
    .toMatch(/core\s*Write/);
  expect(page.getByRole("combobox").elements()).toHaveLength(0);

  await userEvent.click(page.getByRole("button", { name: "Teams" }));
  await userEvent.click(page.getByRole("button", { name: "Members", exact: true }).last());
  await expect.element(page.getByRole("list", { name: "Members of core" })).toHaveTextContent("bo");
  expect(page.getByRole("button", { name: /New team|Rename|Delete|Remove/ }).elements()).toEqual(
    [],
  );

  await userEvent.click(page.getByRole("button", { name: "Settings" }));
  await expect.element(page.getByText(/Every member gets read on every project/)).toBeVisible();
  expect(page.getByRole("button", { name: /Delete|Rename|Save/ }).elements()).toEqual([]);
});

it("says why services would not let a member leave", async () => {
  services({
    "GET /api/orgs": [200, [acme("owner")]],
    "GET /api/projects": [200, []],
    "GET /api/users/me/invitations": [200, []],
    "GET /api/orgs/7/members": [200, [{ id: 1, username: "ana", role: "owner" }]],
    "GET /api/orgs/7/invitations": [200, []],
    "DELETE /api/orgs/7/members/1": [409, { error: "an organization needs an owner" }],
  });
  render(orgPane(acme("owner")));
  await userEvent.click(page.getByRole("button", { name: "Leave" }));
  await expect
    .element(page.getByText("Leave acme? Only an owner can invite you back."))
    .toBeVisible();
  await userEvent.click(page.getByRole("button", { name: "Leave" }));
  await expect.element(page.getByRole("alert")).toHaveTextContent("an organization needs an owner");
});

it("lets a person accept an invitation from their Account pane", async () => {
  let joined = false;
  services({
    "GET /api/orgs": () => [200, joined ? [acme("member")] : []],
    "GET /api/projects": [200, []],
    "GET /api/users/me/invitations": () => [
      200,
      joined ? [] : [{ org: inAcme, role: "member", invited_by: "bo" }],
    ],
    "POST /api/invitations/7/accept": () => {
      joined = true;
      return [200, acme("member")];
    },
  });
  render(<AccountBody />);
  const invitations = page.getByRole("list", { name: "Invitations" });
  await expect.poll(() => text(invitations)).toContain("acme invites you as a member (from bo)");
  await expect.element(page.getByText(/^You are not in any organization/)).toBeVisible();
  await userEvent.click(invitations.getByRole("button", { name: "Accept" }));
  await expect
    .poll(() => text(page.getByRole("list", { name: "Organizations" })))
    .toMatch(/acme\s*Member/);
  expect(page.getByRole("list", { name: "Invitations" }).elements()).toHaveLength(0);
});

it("says what went wrong when services cannot be asked", async () => {
  services({
    "GET /api/orgs": [500, { error: "database is down" }],
    "GET /api/projects": [200, []],
    "GET /api/users/me/invitations": [200, []],
  });
  render(<ProjectsBody />);
  await expect
    .element(page.getByRole("alert"))
    .toHaveTextContent("Could not load your projects: database is down.");
});

it("shows the next person only their own projects, even while the last person's are still being asked for", async () => {
  const bo: User = { id: 2, username: "bo", email: "bo@example.com", totp_enabled: false };
  const held: (() => void)[] = [];
  const answer = (value: unknown) =>
    new Response(JSON.stringify(value), { headers: { "Content-Type": "application/json" } });
  vi.stubGlobal(
    "fetch",
    vi.fn(async (input: RequestInfo | URL) => {
      const path = String(input);
      const now = account.getState().now;
      const who = now.kind === "in" ? now.user : null;
      if (path === "/api/users/me") {
        return answer(who);
      }
      if (who?.id === ME.id) {
        // Ana's questions are still out when Bo signs in, and answer afterwards.
        await new Promise<void>((resolve) => held.push(resolve));
        return answer(
          path === "/api/projects" ? [{ id: 1, name: "anas", org: null, permission: "admin" }] : [],
        );
      }
      return answer(
        path === "/api/projects" ? [{ id: 2, name: "bos", org: null, permission: "admin" }] : [],
      );
    }),
  );
  account.setState({ now: { kind: "in", user: ME }, notice: null });
  render(<ProjectsBody />);
  await vi.waitFor(() => expect(held.length).toBeGreaterThan(0));
  // The pane's own check of who is signed in settles first, as it would
  // before a real sign-in.
  await checkAccount();

  account.setState({ now: { kind: "in", user: bo }, notice: null });
  const own = page.getByRole("list", { name: "Your projects" });
  await expect.poll(() => text(own)).toMatch(/bos/);

  for (const release of held) {
    release();
  }
  await new Promise((resolve) => setTimeout(resolve, 50));
  expect(text(own)).not.toMatch(/anas/);
});

it("tells same-named organizations apart, and says when the members to add could not load", async () => {
  services({
    "GET /api/orgs": [200, [acme("owner"), { ...acme("member"), id: 8, name: "ACME" }]],
    "GET /api/projects": [200, []],
    "GET /api/users/me/invitations": [200, []],
    "GET /api/orgs/7/members": [500, { error: "the database is down" }],
    "GET /api/orgs/7/invitations": [200, []],
    "GET /api/orgs/7/teams": [200, []],
  });
  render(orgPane(acme("owner")));
  await expect.element(page.getByRole("heading", { name: "acme (#7)" })).toBeVisible();
  await userEvent.click(page.getByRole("button", { name: "Teams" }));
  await expect
    .element(page.getByText("Could not load the members to add to teams: the database is down."))
    .toBeVisible();
});
