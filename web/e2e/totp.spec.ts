// Two-factor authentication against the real services backend, its TOTP
// secrets sealed by a fake Cloud KMS: setting it up from the Account pane,
// signing in with an authenticator code or a recovery code, and turning it
// off. Codes are computed here from the key the page shows, as an
// authenticator app would.

import { randomBytes } from "node:crypto";
import * as path from "node:path";
import type { Page } from "@playwright/test";
import { base32, totp, wrong } from "./authenticator";
import { allowErrors, test as base, expect, expectErrors } from "./server";
import { haveServices, type Services, servicesBinary, startServices } from "./services";

const test = base.extend<{ services: Services }>({
  services: async ({ context }, use, info) => {
    allowErrors(/GET \S+\/api\/users\/me: 401$/);
    const services = await startServices();
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

const account = (page: Page) => page.getByRole("tabpanel", { name: "Account" });

/** Saves a screenshot of the Account pane when TOTP_SHOTS names a directory. */
async function shot(page: Page, name: string): Promise<void> {
  const dir = process.env.TOTP_SHOTS;
  if (dir) {
    await account(page).screenshot({ path: path.join(dir, `${name}.png`) });
  }
}

async function signIn(page: Page, name: string, password: string): Promise<void> {
  await account(page).getByLabel("Username or email").fill(name);
  await account(page).getByLabel("Password").fill(password);
  await account(page).getByRole("button", { name: "Sign in" }).click();
}

test("sets up two-factor authentication, signs in with it, and turns it off", async ({
  page,
  services,
}) => {
  test.setTimeout(40_000);
  const username = `e2e-${randomBytes(6).toString("hex")}`;
  const email = `${username}@example.com`;
  const password = "correct horse battery";
  expectErrors(
    /POST \S+\/api\/users\/me\/totp\/confirm: 400$/,
    /POST \S+\/api\/login\/totp: 401$/,
    /POST \S+\/api\/users\/me\/totp\/disable: 403$/,
  );

  // Tall enough that a screenshot holds the whole Account pane.
  if (process.env.TOTP_SHOTS) {
    await page.setViewportSize({ width: 1100, height: 1500 });
  }

  // An account, confirmed and signed in with its emailed code.
  await page.goto(`${services.origin}/`);
  await account(page).getByRole("button", { name: "Create one" }).click();
  await account(page).getByLabel("Username").fill(username);
  await account(page).getByLabel("Email").fill(email);
  await account(page).getByLabel("Password").fill(password);
  await account(page).getByRole("button", { name: "Create account" }).click();
  await page.goto((await services.mail(email)).link);
  await expect(account(page)).toContainText("Your email address is confirmed");
  await signIn(page, username, password);
  await account(page)
    .getByLabel("Code")
    .fill((await services.mail(email)).code ?? "");
  await account(page).getByRole("button", { name: "Sign in" }).click();
  await expect(account(page)).toContainText(`Signed in as ${username}`);
  await expect(account(page)).toContainText("Off. Turn it on");
  await shot(page, "01-off");

  // Set up: the password, then the key, then a code from it.
  await account(page).getByRole("button", { name: "Set up" }).click();
  await account(page).getByLabel("Password").fill(password);
  await shot(page, "02-password");
  await account(page).getByRole("button", { name: "Continue" }).click();
  const shown = await account(page).getByTestId("totp-secret").textContent();
  const secret = base32(shown ?? "");
  expect(secret.length).toBe(20);
  const qr = account(page).getByRole("img", { name: /QR code/ });
  await expect(qr).toBeVisible();
  await shot(page, "03-scan");
  if (process.env.TOTP_SHOTS) {
    await qr.screenshot({ path: path.join(process.env.TOTP_SHOTS, "03-qr.png") });
  }

  await account(page).getByLabel("Code from the app").fill(wrong(secret));
  await account(page).getByRole("button", { name: "Turn on" }).click();
  await expect(account(page)).toContainText("code is wrong");
  await account(page).getByLabel("Code from the app").fill(totp(secret));
  await account(page).getByRole("button", { name: "Turn on" }).click();

  const list = account(page).getByRole("list", { name: "Recovery codes" });
  await expect(list.getByRole("listitem")).toHaveCount(10);
  const recovery = await list.getByRole("listitem").allTextContents();
  for (const code of recovery) {
    expect(code).toMatch(/^[a-z2-7]{4}-[a-z2-7]{4}-[a-z2-7]{4}-[a-z2-7]{4}$/);
  }
  const done = account(page).getByRole("button", { name: "Done" });
  await expect(done).toBeDisabled();
  await shot(page, "04-recovery-codes");
  await account(page).getByLabel("I saved these codes somewhere safe").check();
  await done.click();
  await expect(account(page)).toContainText("On. Signing in takes your password");
  expect((await services.mail(email)).subject).toBe("Two-factor authentication is on");
  await shot(page, "05-on");

  // Signing in now takes an authenticator code, and no email is sent.
  await account(page).getByRole("button", { name: "Sign out", exact: true }).click();
  await signIn(page, username, password);
  await expect(account(page)).toContainText("Enter the code from your authenticator app");
  await account(page).getByLabel("Code").fill(wrong(secret));
  await account(page).getByRole("button", { name: "Sign in" }).click();
  await expect(account(page)).toContainText("code is invalid or the login has expired");
  await shot(page, "06-totp-wrong");
  expect((await services.mail(email)).subject).toBe(
    "Wrong two-factor code for your Microscope account",
  );
  // The code that turned it on is used up, so the next step's code signs in.
  await account(page).getByLabel("Code").fill(totp(secret, 1));
  await account(page).getByRole("button", { name: "Sign in" }).click();
  await expect(account(page)).toContainText(`Signed in as ${username}`);
  await expect(account(page)).toContainText("On. Signing in takes your password");

  // A recovery code, typed the way people copy them.
  await account(page).getByRole("button", { name: "Sign out", exact: true }).click();
  await signIn(page, email, password);
  await account(page).getByRole("button", { name: "Use a recovery code" }).click();
  await account(page)
    .getByLabel("Recovery code")
    .fill((recovery[0] ?? "").toUpperCase().replaceAll("-", ""));
  await shot(page, "07-recovery-login");
  await account(page).getByRole("button", { name: "Sign in" }).click();
  await expect(account(page)).toContainText(`Signed in as ${username}`);
  expect((await services.mail(email)).subject).toBe("A Microscope recovery code was used");

  // Turning it off takes the password and a second factor; a used recovery
  // code is not one.
  await account(page).getByRole("button", { name: "Turn off" }).click();
  await account(page).getByLabel("Password").fill(password);
  await account(page)
    .getByLabel("Code from your app, or a recovery code")
    .fill(recovery[0] ?? "");
  await account(page).getByRole("button", { name: "Turn off" }).click();
  await expect(account(page)).toContainText("code is wrong");
  await account(page)
    .getByLabel("Code from your app, or a recovery code")
    .fill(recovery[1] ?? "");
  await account(page).getByRole("button", { name: "Turn off" }).click();
  await expect(account(page)).toContainText("Off. Turn it on");
  await shot(page, "08-off-again");

  // A password and the emailed code sign in again.
  await account(page).getByRole("button", { name: "Sign out", exact: true }).click();
  await signIn(page, username, password);
  await expect(account(page)).toContainText("Enter the code from your email");
});
