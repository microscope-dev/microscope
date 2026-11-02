// Accounts against the real services backend with email turned on: signing
// up, confirming the address by its emailed link, and signing in with the
// password and then the emailed code or link. Services comes from its own
// repository, built, with its database up (`just up` there).

import { randomBytes } from "node:crypto";
import type { Page } from "@playwright/test";
import { allowErrors, expect, expectErrors, test } from "./server";
import { haveServices, type Services, servicesBinary, startServices } from "./services";

test.skip(!haveServices, `services is not built at ${servicesBinary}`);

let services: Services;

test.beforeEach(async () => {
  // Services' answer when nobody is signed in, which each pane asks as it opens.
  allowErrors(/GET \S+\/api\/users\/me: 401$/);
  services = await startServices();
});

test.afterEach(async ({ context }, info) => {
  // Pages go before services, so none sees it vanish.
  for (const page of context.pages()) {
    await page.close();
  }
  await services.stop();
  if (info.status !== info.expectedStatus) {
    await info.attach("services.log", { body: services.log.join("\n") });
  }
});

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
    await other.goto(login.link);
    await expect(account(other)).toContainText(`Signed in as ${who.username}`);
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
