// Accounts against the real services backend with email turned on: signing
// up, confirming the address by its emailed link, and signing in with the
// password and then the emailed code or link. Services comes from its own
// repository, built, with its database up (`just up` there).

import { randomBytes } from "node:crypto";
import type { Page } from "@playwright/test";
import { allowErrors, test as base, expect, expectErrors, menu } from "./server";
import { haveServices, type Services, servicesBinary, startServices } from "./services";

const test = base.extend<{ services: Services }>({
  services: async ({ context }, use, info) => {
    // Services' answer when nobody is signed in, which each pane asks as it opens.
    allowErrors(/GET \S+\/api\/users\/me: 401$/);
    const services = await startServices();
    try {
      await use(services);
    } finally {
      // Pages go before services, so none sees it vanish.
      for (const page of context.pages()) {
        await page.close();
      }
      await services.stop();
      if (info.status !== info.expectedStatus) {
        await info.attach("services.log", { body: services.log.join("\n") });
      }
    }
  },
});

test.skip(!haveServices, `services is not built at ${servicesBinary}`);

const account = (page: Page) => page.getByRole("tabpanel", { name: "Account" });

/** A new person's name, unique in the shared database. */
function person(): { username: string; email: string; password: string } {
  const username = `e2e-${randomBytes(6).toString("hex")}`;
  return { username, email: `${username}@example.com`, password: "correct horse battery" };
}

async function signIn(page: Page, name: string, password: string): Promise<void> {
  await account(page).getByLabel("Username or email").fill(name);
  await account(page).getByLabel("Password").fill(password);
  await account(page).getByRole("button", { name: "Sign in" }).click();
}

test("signs up, confirms the email by its link, and signs in with the emailed code", async ({
  page,
  services,
}) => {
  const who = person();
  expectErrors(/POST \S+\/api\/login: 403$/, /POST \S+\/api\/login\/code: 401$/);

  await page.goto(`${services.origin}/`);
  await account(page).getByRole("button", { name: "Create one" }).click();
  await account(page).getByLabel("Username").fill(who.username);
  await account(page).getByLabel("Email").fill(who.email);
  await account(page).getByLabel("Password").fill(who.password);
  await account(page).getByRole("button", { name: "Create account" }).click();
  await expect(account(page)).toContainText(`A confirmation link is on its way to ${who.email}`);
  const confirmation = await services.mail(who.email);
  expect(confirmation.subject).toBe("Confirm your Microscope email address");
  expect(confirmation.link).toMatch(new RegExp(`^${services.origin}/confirm-email#token=`));

  // Until the address is confirmed, the password is not enough, and the
  // pane offers another link.
  await account(page).getByRole("button", { name: "Sign in" }).click();
  await signIn(page, who.email, who.password);
  await expect(account(page)).toContainText("email address is not confirmed");
  await account(page).getByRole("button", { name: "Send another link" }).click();
  await expect(account(page)).toContainText(`If ${who.email} still needs confirming`);
  const another = await services.mail(who.email);
  expect(another.link).not.toBe(confirmation.link);

  // The first link still works, and leaves no token in the address bar.
  await page.goto(confirmation.link);
  await expect(account(page)).toContainText("Your email address is confirmed");
  await expect(page).toHaveURL(`${services.origin}/`);

  await signIn(page, who.username, who.password);
  const login = await services.mail(who.email);
  expect(login.subject).toBe("Your Microscope login code");
  const code = login.code ?? "";
  expect(code).toMatch(/^\d{6}$/);

  const field = account(page).getByLabel("Code");
  await field.fill(code === "000000" ? "111111" : "000000");
  await account(page).getByRole("button", { name: "Sign in" }).click();
  await expect(account(page)).toContainText("login code is invalid or has expired");
  await field.fill(code);
  await account(page).getByRole("button", { name: "Sign in" }).click();
  await expect(account(page)).toContainText(`Signed in as ${who.username} (${who.email})`);
  await expect(page.getByRole("tabpanel", { name: "Welcome" })).toContainText(
    `Welcome, ${who.username}`,
  );

  // The session is services' cookie, which outlives the page.
  await page.reload();
  await expect(account(page)).toContainText(`Signed in as ${who.username}`);
  await account(page).getByRole("button", { name: "Sign out" }).click();
  await expect(account(page).getByLabel("Username or email")).toBeVisible();
  await page.reload();
  await expect(account(page).getByLabel("Username or email")).toBeVisible();
});

test("the link in a login email signs in the browser that opens it, once", async ({
  page,
  browser,
  services,
}) => {
  const who = person();
  expectErrors(
    /POST \S+\/api\/email-confirmations: 400$/,
    /POST \S+\/api\/login\/code: 401$/,
    /POST \S+\/api\/login\/link: 401$/,
  );

  // A link that is not one says so.
  await page.goto(`${services.origin}/confirm-email#token=0123`);
  await expect(account(page)).toContainText(
    "Could not confirm: confirmation link is invalid or has expired.",
  );

  await account(page).getByRole("button", { name: "Create one" }).click();
  await account(page).getByLabel("Username").fill(who.username);
  await account(page).getByLabel("Email").fill(who.email);
  await account(page).getByLabel("Password").fill(who.password);
  await account(page).getByRole("button", { name: "Create account" }).click();
  await page.goto((await services.mail(who.email)).link);
  await expect(account(page)).toContainText("Your email address is confirmed");

  await signIn(page, who.email, who.password);
  await expect(account(page).getByLabel("Code")).toBeVisible();
  const login = await services.mail(who.email);
  expect(login.link).toMatch(new RegExp(`^${services.origin}/login-link#token=`));

  // Another device: its own browser, with nothing of this one's.
  const phone = await browser.newContext();
  try {
    const other = await phone.newPage();
    // While the link is held, whatever the page asks services about who is
    // signed in must not show the form: the pane waits for the link.
    const asked: Promise<unknown>[] = [];
    other.on("request", (request) => {
      if (request.url().endsWith("/api/users/me")) {
        asked.push(request.response());
      }
    });
    let release = () => {};
    const held = new Promise<void>((resolve) => {
      release = resolve;
    });
    let entered = () => {};
    const holding = new Promise<void>((resolve) => {
      entered = resolve;
    });
    await other.route("**/api/login/link", async (route) => {
      entered();
      await held;
      await route.continue();
    });
    await other.goto(login.link);
    await holding;
    await expect(account(other)).toContainText("Checking whether you are signed in");
    // Two frames, so every pane's effects have asked what they will.
    await other.evaluate(
      () => new Promise((done) => requestAnimationFrame(() => requestAnimationFrame(done))),
    );
    await Promise.all(asked);
    await expect(account(other)).toContainText("Checking whether you are signed in");
    await expect(account(other).getByLabel("Username or email")).toHaveCount(0);
    release();
    await expect(account(other)).toContainText(`Signed in as ${who.username}`);
    await other.unroute("**/api/login/link");
    await expect(other).toHaveURL(`${services.origin}/`);

    // The link finished the login, so neither it nor the code works again.
    await other.goto(login.link);
    await expect(account(other)).toContainText(
      "Could not sign in: login link is invalid or has expired.",
    );
    // That page's cookie is still good.
    await expect(account(other)).toContainText(`Signed in as ${who.username}`);
  } finally {
    await phone.close();
  }

  await account(page)
    .getByLabel("Code")
    .fill(login.code ?? "");
  await account(page).getByRole("button", { name: "Sign in" }).click();
  await expect(account(page)).toContainText("login code is invalid or has expired");

  await page.goto(`${services.origin}/login-link#token=00`);
  await expect(account(page)).toContainText("Could not sign in: login link is invalid");
});

/** Signs a new person up, confirms their address, and signs them in with
 * the emailed code. */
async function join(page: Page, services: Services, who: ReturnType<typeof person>) {
  await page.goto(`${services.origin}/`);
  await account(page).getByRole("button", { name: "Create one" }).click();
  await account(page).getByLabel("Username").fill(who.username);
  await account(page).getByLabel("Email").fill(who.email);
  await account(page).getByLabel("Password").fill(who.password);
  await account(page).getByRole("button", { name: "Create account" }).click();
  await page.goto((await services.mail(who.email)).link);
  await expect(account(page)).toContainText("Your email address is confirmed");
  await signIn(page, who.username, who.password);
  await account(page)
    .getByLabel("Code")
    .fill((await services.mail(who.email)).code ?? "");
  await account(page).getByRole("button", { name: "Sign in" }).click();
  await expect(account(page)).toContainText(`Signed in as ${who.username}`);
}

test("a forgotten password is reset by its emailed link, which signs out every device", async ({
  page,
  browser,
  services,
}) => {
  const who = person();
  // The short password, then the link used a second time.
  expectErrors(
    /POST \S+\/api\/password-resets\/complete: 400$/,
    /POST \S+\/api\/password-resets\/complete: 400$/,
    /POST \S+\/api\/login: 401$/,
  );

  // Signed in on another device, which the reset signs out.
  const laptop = await browser.newContext();
  try {
    const other = await laptop.newPage();
    await join(other, services, who);

    await page.goto(`${services.origin}/`);
    await account(page).getByLabel("Username or email").fill(who.email);
    await account(page).getByRole("button", { name: "Forgot password?" }).click();
    await expect(account(page).getByLabel("Email")).toHaveValue(who.email);
    await account(page).getByRole("button", { name: "Send reset link" }).click();
    await expect(account(page)).toContainText(
      `If an account uses ${who.email}, a reset link is on its way.`,
    );
    const mail = await services.mail(who.email);
    expect(mail.subject).toBe("Reset your Microscope password");
    expect(mail.link).toMatch(new RegExp(`^${services.origin}/reset-password#token=[0-9a-f]{64}$`));

    // The token leaves the address bar as soon as the page has it.
    await page.goto(mail.link);
    await expect(page).toHaveURL(`${services.origin}/`);
    const field = account(page).getByLabel("New password");
    await field.fill("short");
    await account(page).getByRole("button", { name: "Change password" }).click();
    await expect(account(page)).toContainText("password must be 8 to 256 characters");
    const fresh = "an entirely new password";
    await field.fill(fresh);
    await account(page).getByRole("button", { name: "Change password" }).click();
    await expect(account(page)).toContainText("Your password is changed");

    await other.reload();
    await expect(account(other).getByLabel("Username or email")).toBeVisible();

    // The link works once.
    await page.goto(mail.link);
    await account(page).getByLabel("New password").fill("yet another password");
    await account(page).getByRole("button", { name: "Change password" }).click();
    await expect(account(page)).toContainText("password reset link is invalid or has expired");
    await account(page).getByRole("button", { name: "Cancel" }).click();

    await signIn(page, who.email, who.password);
    await expect(account(page)).toContainText("invalid username, email, or password");
    await signIn(page, who.email, fresh);
    await account(page)
      .getByLabel("Code")
      .fill((await services.mail(who.email)).code ?? "");
    await account(page).getByRole("button", { name: "Sign in" }).click();
    await expect(account(page)).toContainText(`Signed in as ${who.username}`);
  } finally {
    await laptop.close();
  }
});

test("an owner shares a project with a team, and a member sees the access it gives", async ({
  page,
  browser,
  services,
}) => {
  const owner = person();
  const user = person();
  const org = `acme ${randomBytes(3).toString("hex")}`;
  const projects = (on: Page) => on.getByRole("tabpanel", { name: "Projects" });
  const orgPane = (on: Page) => on.getByRole("tabpanel", { name: org });
  const section = (on: Page, name: string) =>
    orgPane(on).getByRole("group", { name: "Sections" }).getByRole("button", { name });

  const elsewhere = await browser.newContext();
  try {
    const other = await elsewhere.newPage();

    // Someone makes a project of their own.
    await join(other, services, user);
    await menu(other, "File", "New project…");
    await projects(other).getByLabel("Project name").fill("notes");
    await projects(other).getByRole("button", { name: "Create project" }).click();
    await expect(projects(other).getByRole("list", { name: "Your projects" })).toContainText(
      /notes\s*Admin/,
    );

    // An owner makes an organization and a project in it, and invites them.
    await join(page, services, owner);
    await menu(page, "File", "New organization…");
    await account(page).getByLabel("Organization name").fill(org);
    await account(page).getByRole("button", { name: "Create organization" }).click();
    await expect(orgPane(page).getByRole("heading", { name: org })).toBeVisible();
    await section(page, "Projects").click();
    await orgPane(page).getByRole("button", { name: "New project" }).click();
    await orgPane(page).getByLabel("Project name").fill("api");
    await orgPane(page).getByRole("button", { name: "Create project" }).click();
    await expect(orgPane(page).getByRole("list", { name: `${org} projects` })).toContainText(
      /api\s*Admin/,
    );
    await section(page, "Members").click();
    await orgPane(page).getByLabel("Username").fill(user.username);
    await orgPane(page).getByRole("button", { name: "Invite" }).click();
    await expect(orgPane(page).getByRole("list", { name: "Invited" })).toContainText(user.username);

    // They accept.
    await other.reload();
    await menu(other, "File", "Account");
    const invitations = account(other).getByRole("list", { name: "Invitations" });
    await expect(invitations).toContainText(
      `${org} invites you as a member (from ${owner.username})`,
    );
    await invitations.getByRole("button", { name: "Accept" }).click();
    await expect(account(other).getByRole("list", { name: "Organizations" })).toHaveText(
      new RegExp(`^${org}\\s*Member$`),
    );
    await expect(invitations).toHaveCount(0);

    // The owner makes a team, puts them in it, and gives it write.
    await section(page, "Teams").click();
    await orgPane(page).getByRole("button", { name: "New team" }).click();
    await orgPane(page).getByLabel("Team name").fill("core");
    await orgPane(page).getByRole("button", { name: "Create team" }).click();
    await orgPane(page)
      .getByRole("list", { name: "Teams" })
      .getByRole("button", { name: "Members" })
      .click();
    await orgPane(page).getByLabel("Add").selectOption({ label: user.username });
    await orgPane(page).getByRole("button", { name: "Add to team" }).click();
    await expect(orgPane(page).getByRole("list", { name: "Members of core" })).toContainText(
      user.username,
    );
    await section(page, "Projects").click();
    await orgPane(page).getByRole("button", { name: "Team access" }).click();
    await orgPane(page).getByLabel("Give", { exact: true }).selectOption({ label: "core" });
    await orgPane(page).getByLabel("access", { exact: true }).selectOption("write");
    await orgPane(page).getByRole("button", { name: "Give access" }).click();
    await expect(orgPane(page).getByRole("combobox", { name: "Access for core" })).toHaveValue(
      "write",
    );

    // They see both projects, with what each lets them do, and nothing of
    // anyone's own projects but theirs.
    await other.reload();
    await menu(other, "File", "Projects");
    await expect(projects(other).getByRole("list", { name: "Your projects" })).toHaveText(
      /^notes\s*Admin/,
    );
    const shared = projects(other).getByRole("list", { name: `${org} projects` });
    await expect(shared).toHaveText(/^api\s*Write$/);
    await expect(projects(other).getByRole("heading")).toHaveText(["Yours", org]);
  } finally {
    await elsewhere.close();
  }
});
