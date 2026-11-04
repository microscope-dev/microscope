// The Projects, Organization, and Account panes against a stand-in for
// services: each shows only the controls its person may use, and says why
// when services refuses.

import { cleanup, render } from "@testing-library/react";
import { afterEach, expect, it, vi } from "vitest";
import { type Locator, page, userEvent } from "vitest/browser";

import { account, checkAccount, type User } from "../src/account";
import { KIND } from "../src/documents";
import { directory, type Member, type Org, type Project } from "../src/orgs";
import { proving } from "../src/sso";
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
      const found =
        routes[route] ?? (route === "GET /api/users/me" ? ([200, ME] as Answer) : undefined);
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
  account.setState({ now: { kind: "checking" }, notice: null, reset: null });
  proving.setState({ retry: null, asked: null });
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

it("lists sign-in methods only when there is one to connect or one connected", async () => {
  const off = { google: false, github: false, saml: true };
  const saml = { id: 3, provider: "saml", email: ME.email, org_id: 7, org_name: "acme" };
  const methods = () => page.getByRole("list", { name: "Sign-in methods" });
  const show = (providers: unknown, identities: unknown[], passkeys = false) => {
    const asked = services({
      "GET /api/orgs": [200, []],
      "GET /api/projects": [200, []],
      "GET /api/users/me/invitations": [200, []],
      "GET /api/features": [200, { email: false, totp: true, sso: providers, passkeys }],
      "GET /api/users/me/identities": [200, { has_password: true, identities, passkeys: [] }],
    });
    render(<AccountBody />);
    return asked;
  };

  // Nothing to connect, and nothing connected: no section at all.
  const asked = show(off, []);
  await expect
    .poll(() => asked)
    .toEqual(expect.arrayContaining(["GET /api/features", "GET /api/users/me/identities"]));
  await expect.element(page.getByText(/^You are not in any organization/)).toBeVisible();
  await new Promise((settle) => setTimeout(settle, 50));
  expect(page.getByRole("heading", { name: "Sign-in methods" }).elements()).toHaveLength(0);
  cleanup();

  // An organization's single sign-on, connected, still shows and can go.
  show(off, [saml]);
  await expect.poll(() => text(methods())).toMatch(/^Password\s*acme's single sign-on/);
  await expect.element(methods().getByRole("button", { name: "Disconnect" })).toBeVisible();
  cleanup();

  // GitHub offered: something to connect.
  show({ ...off, github: true }, []);
  await expect.poll(() => text(methods())).toBe("Password");
  await expect.element(page.getByRole("button", { name: "Connect GitHub" })).toBeVisible();
  expect(page.getByRole("button", { name: "Connect Google" }).elements()).toHaveLength(0);
  expect(page.getByRole("button", { name: "Add a passkey" }).elements()).toHaveLength(0);
  cleanup();

  // Passkeys offered: something to add.
  show(off, [], true);
  await expect.element(page.getByRole("button", { name: "Add a passkey" })).toBeVisible();
});

const PASSKEY = { id: 9, name: "Mac", backed_up: true, created_at: 1_790_000_000 };

it("lists passkeys, and keeps the last way in", async () => {
  const methods = () => page.getByRole("list", { name: "Sign-in methods" });
  const show = (has_password: boolean, passkeys: unknown[]) => {
    services({
      "GET /api/orgs": [200, []],
      "GET /api/projects": [200, []],
      "GET /api/users/me/invitations": [200, []],
      "GET /api/features": [
        200,
        {
          email: false,
          totp: false,
          sso: { google: false, github: false, saml: false },
          passkeys: true,
        },
      ],
      "GET /api/users/me/identities": [200, { has_password, identities: [], passkeys }],
    });
    render(<AccountBody />);
  };

  show(true, [
    { ...PASSKEY, last_used_at: null },
    { ...PASSKEY, id: 10, name: "YubiKey", last_used_at: 1_792_000_000 },
  ]);
  const mac = methods().getByRole("listitem").filter({ hasText: "Passkey: Mac" });
  const key = methods().getByRole("listitem").filter({ hasText: "Passkey: YubiKey" });
  const day = (unix: number) =>
    new Date(unix * 1000).toLocaleDateString(undefined, { dateStyle: "medium" });
  await expect.element(mac).toHaveTextContent(`Passkey: MacAdded ${day(1_790_000_000)}Remove`);
  await expect
    .element(key)
    .toHaveTextContent(`Passkey: YubiKeyLast used ${day(1_792_000_000)}Remove`);
  cleanup();

  // Its only way in has nothing to remove it with.
  show(false, [{ ...PASSKEY, last_used_at: null }]);
  await expect.poll(() => text(methods())).toMatch(/^Passkey: Mac/);
  expect(methods().getByRole("button").elements()).toHaveLength(0);
});

it("adds a passkey once the person proves who they are, sending what the device made", async () => {
  const options = {
    challenge: "AAEC_-8",
    rp: { id: "localhost", name: "Microscope" },
    user: { id: "BAUG", name: ME.email, displayName: ME.username },
    pubKeyCredParams: [{ type: "public-key", alg: -7 }],
    excludeCredentials: [{ type: "public-key", id: "-_8" }],
    authenticatorSelection: { residentKey: "required", userVerification: "required" },
    attestation: "none",
  };
  let verified = false;
  let added: unknown = null;
  services({
    "GET /api/orgs": [200, []],
    "GET /api/projects": [200, []],
    "GET /api/users/me/invitations": [200, []],
    "GET /api/features": [
      200,
      {
        email: false,
        totp: false,
        sso: { google: false, github: false, saml: false },
        passkeys: true,
      },
    ],
    "GET /api/users/me/identities": () => [
      200,
      {
        has_password: true,
        identities: [],
        passkeys: added ? [{ ...PASSKEY, last_used_at: null }] : [],
      },
    ],
    "POST /api/passkeys/register/start": () =>
      verified ? [200, options] : [403, { error: "confirm it is you first" }],
    "POST /api/users/me/verify": () => {
      verified = true;
      return [204];
    },
    "POST /api/passkeys/register/finish": (body) => {
      added = body;
      return [201, { id: 9, name: "Mac" }];
    },
  });
  const made: CredentialCreationOptions[] = [];
  vi.spyOn(navigator.credentials, "create").mockImplementation(async (asked) => {
    made.push(asked as CredentialCreationOptions);
    return {
      rawId: new Uint8Array([0xfb, 0xff, 0x01]).buffer,
      response: {
        clientDataJSON: new TextEncoder().encode("{}").buffer,
        attestationObject: new Uint8Array([0xa0]).buffer,
      },
    } as unknown as Credential;
  });
  render(<AccountBody />);

  await userEvent.click(page.getByRole("button", { name: "Add a passkey" }));
  await userEvent.fill(page.getByLabelText("Passkey name"), " Mac ");
  await userEvent.click(page.getByRole("button", { name: "Add passkey" }));
  const prove = page.getByRole("region", { name: "Confirm it's you" });
  await userEvent.fill(prove.getByLabelText("Password"), "the password");
  await userEvent.click(prove.getByRole("button", { name: "Confirm" }));

  await expect
    .poll(() => text(page.getByRole("list", { name: "Sign-in methods" })))
    .toMatch(/Passkey: Mac/);
  expect(added).toEqual({
    credential_id: "-_8B",
    client_data_json: "e30",
    attestation_object: "oA",
    name: "Mac",
  });
  expect(made).toHaveLength(1);
  const key = made[0]?.publicKey;
  expect([...new Uint8Array(key?.challenge as ArrayBuffer)]).toEqual([0, 1, 2, 0xff, 0xef]);
  expect([...new Uint8Array(key?.user.id as ArrayBuffer)]).toEqual([4, 5, 6]);
  expect([...new Uint8Array(key?.excludeCredentials?.[0]?.id as ArrayBuffer)]).toEqual([
    0xfb, 0xff,
  ]);
  expect(key?.rp).toEqual({ id: "localhost", name: "Microscope" });
  await expect.element(page.getByLabelText("Passkey name")).not.toBeInTheDocument();
});

it("offers two-factor authentication only when the server can keep its secrets", async () => {
  const heading = () => page.getByRole("heading", { name: "Two-factor authentication" });
  const show = (totp: boolean, user: User) => {
    const asked = services({
      "GET /api/orgs": [200, []],
      "GET /api/projects": [200, []],
      "GET /api/users/me/invitations": [200, []],
      "GET /api/features": [
        200,
        { email: false, totp, sso: { google: false, github: false, saml: false } },
      ],
      "GET /api/users/me/identities": [200, { has_password: true, identities: [], passkeys: [] }],
      "GET /api/users/me": [200, user],
    });
    account.setState({ now: { kind: "in", user } });
    render(<AccountBody />);
    return asked;
  };

  const asked = show(false, ME);
  await expect.poll(() => asked).toContain("GET /api/features");
  await expect.element(page.getByText(/^You are not in any organization/)).toBeVisible();
  await new Promise((settle) => setTimeout(settle, 50));
  expect(heading().elements()).toHaveLength(0);
  cleanup();

  // An account that turned it on before can still see it, to turn it off.
  show(false, { ...ME, totp_enabled: true });
  await expect.element(heading()).toBeVisible();
  cleanup();

  show(true, ME);
  await expect.element(heading()).toBeVisible();
});

it("offers a password reset only when the server sends email", async () => {
  const forgot = () => page.getByRole("button", { name: "Forgot password?" });
  const show = (email: boolean) => {
    const asked = services({
      "GET /api/users/me": [401, { error: "not logged in" }],
      "GET /api/features": [
        200,
        { email, totp: false, sso: { google: false, github: false, saml: false } },
      ],
    });
    account.setState({ now: { kind: "out" } });
    render(<AccountBody />);
    return asked;
  };

  const asked = show(false);
  await expect.poll(() => asked).toContain("GET /api/features");
  await expect.element(page.getByLabelText("Username or email")).toBeVisible();
  await new Promise((settle) => setTimeout(settle, 50));
  expect(forgot().elements()).toHaveLength(0);
  cleanup();

  show(true);
  await userEvent.fill(page.getByLabelText("Username or email"), "ana@example.com ");
  await userEvent.click(forgot());
  await expect.element(page.getByLabelText("Email")).toHaveValue("ana@example.com");
});

it("a reset link for another account leaves this browser signed in as it was", async () => {
  services({
    "GET /api/orgs": [200, []],
    "GET /api/projects": [200, []],
    "GET /api/users/me/invitations": [200, []],
    "GET /api/features": [
      200,
      { email: true, totp: false, sso: { google: false, github: false, saml: false } },
    ],
    "GET /api/users/me/identities": [200, { has_password: true, identities: [], passkeys: [] }],
    "POST /api/password-resets/complete": [204],
  });
  account.setState({ reset: "ab".repeat(32) });
  render(<AccountBody />);
  await userEvent.fill(page.getByLabelText("New password"), "a whole new password");
  await userEvent.click(page.getByRole("button", { name: "Change password" }));
  await expect
    .element(page.getByText(/that account is signed out everywhere/))
    .toHaveTextContent(
      "The password is changed, and that account is signed out everywhere. You are still signed in as ana.",
    );
  await expect
    .element(page.getByText(/^Signed in as/))
    .toHaveTextContent("Signed in as ana (ana@example.com).");
});

it("an account without a password proves who it is by signing in again", async () => {
  const google = { id: 3, provider: "google", email: ME.email, org_id: null, org_name: null };
  const github = { id: 4, provider: "github", email: ME.email, org_id: null, org_name: null };
  const asked = services({
    "GET /api/orgs": [200, []],
    "GET /api/projects": [200, []],
    "GET /api/users/me/invitations": [200, []],
    "GET /api/features": [
      200,
      { email: false, totp: false, sso: { google: true, github: true, saml: false } },
    ],
    "GET /api/users/me/identities": [
      200,
      { has_password: false, identities: [google, github], passkeys: [] },
    ],
    "DELETE /api/users/me/identities/4": [403, { error: "confirm it is you first" }],
  });
  render(<AccountBody />);
  const row = page.getByRole("listitem").filter({ hasText: "GitHub" });
  await userEvent.click(row.getByRole("button", { name: "Disconnect" }));
  await userEvent.click(row.getByRole("button", { name: "Disconnect" }));

  const prove = page.getByRole("region", { name: "Confirm it's you" });
  await expect
    .element(prove.getByRole("link", { name: "Sign in again with Google" }))
    .toHaveAttribute("href", "/api/sso/google/start");
  await expect
    .element(prove.getByRole("link", { name: "Sign in again with GitHub" }))
    .toHaveAttribute("href", "/api/sso/github/start");
  expect(prove.getByLabelText("Password").elements()).toHaveLength(0);
  // The refusal is a question, not an error.
  expect(page.getByRole("alert").elements()).toHaveLength(0);
  expect(asked.filter((each) => each.startsWith("DELETE"))).toHaveLength(1);

  await userEvent.click(prove.getByRole("button", { name: "Cancel" }));
  await expect.element(prove).not.toBeInTheDocument();
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
