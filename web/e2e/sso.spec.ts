// Single sign-on against the real services backend and services' mock
// identity provider, which stands in for Google, GitHub, and an
// organization's SAML identity provider: its sign-in pages are where a
// person would sign in to the real ones. Everything else is real: the
// redirects, services' checks of what comes back, and the page.

import { randomBytes } from "node:crypto";
import * as path from "node:path";
import type { Page } from "@playwright/test";
import { base32, totp } from "./authenticator";
import { allowErrors, test as base, expect, expectErrors, menu } from "./server";
import { haveServices, type Services, servicesBinary, startServices } from "./services";

const test = base.extend<{ services: Services }>({
  services: async ({ context }, use, info) => {
    allowErrors(
      /GET \S+\/api\/users\/me: 401$/,
      // The mock's sign-in pages have no icon.
      /GET http:\/\/127\.0\.0\.1:\d+\/favicon\.ico: 404$/,
    );
    const services = await startServices({ sso: true });
    try {
      await use(services);
    } finally {
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

// Tall enough that a screenshot holds the whole pane.
test.beforeEach(async ({ page }) => {
  if (process.env.SSO_SHOTS) {
    await page.setViewportSize({ width: 1100, height: 1500 });
  }
});

const account = (page: Page) => page.getByRole("tabpanel", { name: "Account" });

/** Saves a screenshot of `what` when SSO_SHOTS names a directory. */
async function shot(page: Page, name: string, what = account(page)): Promise<void> {
  const dir = process.env.SSO_SHOTS;
  if (dir) {
    await what.screenshot({ path: path.join(dir, `${name}.png`) });
  }
}

/** A new address, unique in the shared database. */
const address = () => `e2e-${randomBytes(6).toString("hex")}@example.com`;

/** Signs in on the mock's page as someone it does not list. */
async function atProvider(
  page: Page,
  who: { email: string; login?: string; fault?: string },
): Promise<void> {
  await expect(page.getByText("This is a mock for testing.")).toBeVisible();
  await shot(page, "provider", page.locator("body"));
  await page.getByLabel("Email", { exact: true }).fill(who.email);
  if (who.login) {
    await page.getByLabel("GitHub login").fill(who.login);
  }
  if (who.fault) {
    await page.getByLabel("Make it misbehave").selectOption(who.fault);
  }
  await page.getByRole("button", { name: "Sign in" }).click();
}

async function continueWith(page: Page, provider: "Google" | "GitHub"): Promise<void> {
  await account(page)
    .getByRole("link", { name: `Continue with ${provider}` })
    .click();
}

const methods = (page: Page) => account(page).getByRole("list", { name: "Sign-in methods" });

test("signs up with Google, connects GitHub, and signs in with either", async ({
  page,
  services,
}) => {
  const email = address();
  const login = `octo-${randomBytes(4).toString("hex")}`;

  await page.goto(`${services.origin}/`);
  await expect(account(page).getByRole("link", { name: "Continue with GitHub" })).toBeVisible();
  await shot(page, "01-signed-out");

  // A new person: their account is made, its email already verified by Google.
  await continueWith(page, "Google");
  await atProvider(page, { email });
  await expect(account(page)).toContainText(`(${email})`);
  await expect(page).toHaveURL(`${services.origin}/`);
  await expect(methods(page)).toHaveText(/^Google\s*\S+@example\.com$/);
  await shot(page, "02-google-only");

  // Connecting GitHub leaves and comes back to the same account.
  await account(page).getByRole("button", { name: "Connect GitHub" }).click();
  await atProvider(page, { email, login });
  await expect(account(page).getByRole("status")).toHaveText(
    "GitHub is connected to your account.",
  );
  await expect(methods(page).getByRole("listitem")).toHaveCount(2);
  await expect(methods(page).getByRole("button", { name: "Disconnect" })).toHaveCount(2);
  await shot(page, "03-both");
  const username = /Signed in as (\S+)/.exec((await account(page).textContent()) ?? "")?.[1];
  expect(username).toBeTruthy();

  // GitHub signs in to it.
  await account(page).getByRole("button", { name: "Sign out" }).click();
  await continueWith(page, "GitHub");
  await atProvider(page, { email, login });
  await expect(account(page)).toContainText(`Signed in as ${username} (${email})`);

  // Disconnecting Google leaves GitHub as the only way in, which cannot go.
  const google = methods(page).getByRole("listitem").filter({ hasText: "Google" });
  await google.getByRole("button", { name: "Disconnect" }).click();
  await google.getByRole("button", { name: "Disconnect" }).click();
  await expect(methods(page)).toHaveText(/^GitHub\s*\S+$/);
  await expect(account(page).getByRole("button", { name: "Connect Google" })).toBeVisible();

  // Google now makes a different account, since it is no longer connected
  // and the address is taken: services refuses rather than link by email.
  await account(page).getByRole("button", { name: "Sign out" }).click();
  await continueWith(page, "Google");
  await atProvider(page, { email });
  await expect(account(page).getByRole("alert")).toContainText(`An account already uses ${email}`);
  await expect(account(page).getByRole("link", { name: "Continue with Google" })).toBeVisible();
  await expect(page).toHaveURL(`${services.origin}/`);
  await shot(page, "04-taken");
});

test("a provider that misbehaves signs nobody in", async ({ page, services }) => {
  await page.goto(`${services.origin}/`);
  await continueWith(page, "Google");
  await atProvider(page, { email: address(), fault: "bad_signature" });
  await expect(account(page).getByRole("alert")).toContainText(
    "Google sent a sign-in Microscope could not verify.",
  );
  await expect(account(page).getByLabel("Username or email")).toBeVisible();

  await continueWith(page, "GitHub");
  await atProvider(page, { email: address(), fault: "unverified_email" });
  await expect(account(page).getByRole("alert")).toContainText("verified");
  await expect(account(page).getByLabel("Username or email")).toBeVisible();
});

test("an owner sets up SAML, and a new person signs in through it and joins", async ({
  page,
  browser,
  services,
}) => {
  const org = `acme ${randomBytes(3).toString("hex")}`;
  expectErrors(/PUT \S+\/api\/orgs\/\d+\/sso: 400$/);
  const orgPane = page.getByRole("tabpanel", { name: org });

  // An owner, signed up with Google, makes an organization.
  await page.goto(`${services.origin}/`);
  await continueWith(page, "Google");
  await atProvider(page, { email: address() });
  await expect(account(page)).toContainText("Signed in as");
  await menu(page, "File", "New organization…");
  await account(page).getByLabel("Organization name").fill(org);
  await account(page).getByRole("button", { name: "Create organization" }).click();
  await orgPane
    .getByRole("group", { name: "Sections" })
    .getByRole("button", { name: "Settings" })
    .click();

  // Services' values go to the identity provider; its values come back.
  // Until the identity provider is saved, nothing is on to turn off; the
  // owner can back out instead.
  await orgPane.getByRole("button", { name: "Set up single sign-on" }).click();
  await expect(orgPane.getByLabel("ACS URL (reply URL)")).toBeVisible();
  await expect(orgPane.getByRole("button", { name: "Turn off single sign-on" })).toHaveCount(0);
  await orgPane.getByRole("button", { name: "Cancel setup" }).click();
  await orgPane.getByRole("button", { name: "Keep setting up" }).click();
  await orgPane.getByRole("button", { name: "Cancel setup" }).click();
  await orgPane.getByRole("button", { name: "Cancel setup" }).click();
  await orgPane.getByRole("button", { name: "Set up single sign-on" }).click();
  await expect(orgPane.getByLabel("ACS URL (reply URL)")).toHaveValue(
    new RegExp(`^${services.origin}/api/sso/saml/[0-9a-f]{32}/acs$`),
  );
  await shot(page, "05-org-setup", orgPane);
  const certificate = await (await fetch(`${services.idp}/saml/certificate.pem`)).text();
  const form = orgPane.getByRole("form", { name: "Identity provider" });
  await form.getByLabel("Identity provider entity ID (issuer)").fill(`${services.idp}/saml`);
  await form.getByLabel("Single sign-on URL").fill(`${services.idp}/saml/sso`);
  await form.getByLabel("Signing certificate", { exact: true }).fill("not a certificate");
  await form.getByRole("button", { name: "Save" }).click();
  await expect(form.getByRole("alert")).toHaveText(
    "a certificate is not a valid X.509 certificate",
  );
  await form.getByLabel("Signing certificate", { exact: true }).fill(certificate);
  await form.getByRole("button", { name: "Save" }).click();
  const link = orgPane.getByLabel("Sign-in link");
  await expect(link).toHaveValue(new RegExp(`^${services.origin}/sso#key=[0-9a-f]{32}$`));
  await expect(orgPane.getByRole("list", { name: "Identity provider" })).toContainText(
    `${services.idp}/saml`,
  );
  await shot(page, "06-org-configured", orgPane);
  const signInLink = await link.inputValue();

  // Someone new, in their own browser, follows the link.
  const elsewhere = await browser.newContext();
  try {
    const other = await elsewhere.newPage();
    const email = address();

    // A forged assertion beside the signed one is refused.
    await other.goto(signInLink);
    await expect(account(other).getByRole("heading", { name: org })).toBeVisible();
    await shot(other, "07-org-link");
    await account(other)
      .getByRole("link", { name: `Sign in with ${org}` })
      .click();
    await atProvider(other, { email, fault: "wrapped" });
    await expect(account(other).getByRole("alert")).toContainText("could not verify");
    await expect(account(other).getByLabel("Username or email")).toBeVisible();

    await other.goto(signInLink);
    await account(other)
      .getByRole("link", { name: `Sign in with ${org}` })
      .click();
    await atProvider(other, { email });

    // An identity provider does not vouch for the address the way Google
    // does, so services, which sends email here, has it confirmed first.
    await expect(account(other).getByRole("status")).toHaveText(
      `Check ${email} for a link to confirm your address, then sign in again.`,
    );
    await other.goto((await services.mail(email)).link);
    await expect(account(other)).toContainText("Your email address is confirmed");
    await other.goto(signInLink);
    await account(other)
      .getByRole("link", { name: `Sign in with ${org}` })
      .click();
    await atProvider(other, { email });
    await expect(account(other)).toContainText(`(${email})`);
    await expect(account(other).getByRole("list", { name: "Organizations" })).toHaveText(
      new RegExp(`^${org}\\s*Member$`),
    );
    await expect(methods(other)).toHaveText(new RegExp(`^${org}'s single sign-on\\s*${email}$`));
  } finally {
    await elsewhere.close();
  }

  // The owner turns it off; the link stops working.
  await orgPane.getByRole("button", { name: "Turn off single sign-on" }).click();
  await orgPane.getByRole("button", { name: "Turn off single sign-on" }).click();
  await expect(orgPane.getByRole("button", { name: "Set up single sign-on" })).toBeVisible();
});

test("an account with two-factor authentication still needs its code after Google", async ({
  page,
  services,
}) => {
  test.setTimeout(30_000);
  const username = `e2e-${randomBytes(6).toString("hex")}`;
  const email = `${username}@example.com`;
  const password = "[REDACTED:password] horse battery";

  // A password account, confirmed and signed in with its emailed code.
  await page.goto(`${services.origin}/`);
  await account(page).getByRole("button", { name: "Create one" }).click();
  await account(page).getByLabel("Username").fill(username);
  await account(page).getByLabel("Email").fill(email);
  await account(page).getByLabel("Password").fill(password);
  await account(page).getByRole("button", { name: "Create account" }).click();
  await page.goto((await services.mail(email)).link);
  await account(page).getByLabel("Username or email").fill(username);
  await account(page).getByLabel("Password").fill(password);
  await account(page).getByRole("button", { name: "Sign in" }).click();
  await account(page)
    .getByLabel("Code")
    .fill((await services.mail(email)).code ?? "");
  await account(page).getByRole("button", { name: "Sign in" }).click();
  await expect(account(page)).toContainText(`Signed in as ${username}`);

  // It connects Google, then turns on two-factor authentication.
  await account(page).getByRole("button", { name: "Connect Google" }).click();
  await atProvider(page, { email });
  await expect(methods(page)).toHaveText(/^Password\s*Google\s*\S+\s*Disconnect$/);
  await account(page).getByRole("button", { name: "Set up" }).click();
  await account(page).getByLabel("Password").fill(password);
  await account(page).getByRole("button", { name: "Continue" }).click();
  const secret = base32((await account(page).getByTestId("totp-secret").textContent()) ?? "");
  await account(page).getByLabel("Code from the app").fill(totp(secret));
  await account(page).getByRole("button", { name: "Turn on" }).click();
  await account(page).getByLabel("I saved these codes somewhere safe").check();
  await account(page).getByRole("button", { name: "Done" }).click();
  await expect(account(page)).toContainText("On. Signing in takes your password");

  // Google alone is not enough.
  await account(page).getByRole("button", { name: "Sign out" }).click();
  await continueWith(page, "Google");
  await atProvider(page, { email });
  await expect(account(page)).toContainText("Enter the code from your authenticator app");
  await expect(page).toHaveURL(`${services.origin}/`);
  await shot(page, "08-totp-after-google");
  await account(page).getByLabel("Code").fill(totp(secret, 1));
  await account(page).getByRole("button", { name: "Sign in" }).click();
  await expect(account(page)).toContainText(`Signed in as ${username}`);
});
