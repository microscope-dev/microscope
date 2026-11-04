// Passkeys against the real services backend, with Chromium's virtual
// authenticator standing in for the person's device: it makes real keys and
// signs real challenges, and services checks them as it would a phone's.
// Firefox has no such authenticator, so these run in Chromium only.

import { randomBytes } from "node:crypto";
import * as path from "node:path";
import type { CDPSession, Page } from "@playwright/test";
import { allowErrors, test as base, expect, expectErrors } from "./server";
import { haveServices, type Services, servicesBinary, startServices } from "./services";

const test = base.extend<{ services: Services }>({
  services: async ({ context }, use, info) => {
    allowErrors(/GET \S+\/api\/users\/me: 401$/);
    const services = await startServices({ localhost: true });
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
test.skip(
  ({ browserName }) => browserName !== "chromium",
  "only Chromium has a virtual authenticator",
);

const account = (page: Page) => page.getByRole("tabpanel", { name: "Account" });

/** Saves a screenshot of the Account pane when PASSKEY_SHOTS names a directory. */
async function shot(page: Page, name: string): Promise<void> {
  const dir = process.env.PASSKEY_SHOTS;
  if (dir) {
    await account(page).screenshot({ path: path.join(dir, `${name}.png`) });
  }
}

/** A platform authenticator that checks who is there, like Touch ID. */
async function device(page: Page): Promise<{ cdp: CDPSession; id: string }> {
  const cdp = await page.context().newCDPSession(page);
  await cdp.send("WebAuthn.enable");
  const { authenticatorId } = await cdp.send("WebAuthn.addVirtualAuthenticator", {
    options: {
      protocol: "ctap2",
      transport: "internal",
      hasResidentKey: true,
      hasUserVerification: true,
      isUserVerified: true,
      automaticPresenceSimulation: true,
    },
  });
  return { cdp, id: authenticatorId };
}

/** Signs up, confirms the address, and signs in with the emailed code. */
async function join(page: Page, services: Services): Promise<{ username: string }> {
  const username = `e2e-${randomBytes(6).toString("hex")}`;
  const email = `${username}@example.com`;
  const password = "correct horse battery";
  await page.goto(`${services.origin}/`);
  await account(page).getByRole("button", { name: "Create one" }).click();
  await account(page).getByLabel("Username").fill(username);
  await account(page).getByLabel("Email").fill(email);
  await account(page).getByLabel("Password").fill(password);
  await account(page).getByRole("button", { name: "Create account" }).click();
  await page.goto((await services.mail(email)).link);
  await expect(account(page)).toContainText("Your email address is confirmed");
  await account(page).getByLabel("Username or email").fill(username);
  await account(page).getByLabel("Password").fill(password);
  await account(page).getByRole("button", { name: "Sign in", exact: true }).click();
  await account(page)
    .getByLabel("Code")
    .fill((await services.mail(email)).code ?? "");
  await account(page).getByRole("button", { name: "Sign in", exact: true }).click();
  await expect(account(page)).toContainText(`Signed in as ${username}`);
  return { username };
}

async function signOut(page: Page): Promise<void> {
  await account(page).getByRole("button", { name: "Sign out", exact: true }).click();
  await expect(account(page).getByLabel("Username or email")).toBeVisible();
}

test("adds a passkey, signs in with it and no code, and removes it", async ({ page, services }) => {
  test.setTimeout(30_000);
  // The removed passkey, which the device still offers.
  expectErrors(/POST \S+\/api\/passkeys\/sign-in\/finish: 401$/);
  const { cdp, id } = await device(page);
  const { username } = await join(page, services);

  await account(page).getByRole("button", { name: "Add a passkey" }).click();
  await account(page).getByLabel("Passkey name").fill("Work laptop");
  await shot(page, "naming");
  await account(page).getByRole("button", { name: "Add passkey" }).click();
  const methods = account(page).getByRole("list", { name: "Sign-in methods" });
  await expect(methods).toContainText("Passkey: Work laptop");
  await expect(methods).toContainText("Added");
  await shot(page, "added");
  const { credentials } = await cdp.send("WebAuthn.getCredentials", { authenticatorId: id });
  expect(credentials).toHaveLength(1);
  expect(credentials[0]?.isResidentCredential).toBe(true);
  expect(credentials[0]?.rpId).toBe("localhost");

  // The sign-in form offers the passkey in the username field's autofill,
  // where the virtual authenticator picks it at once, as a person would. It
  // takes the passkey alone: no password, and no emailed code.
  const autofilled = page.waitForResponse(
    (response) => response.url().endsWith("/api/passkeys/sign-in/finish") && response.ok(),
  );
  await account(page).getByRole("button", { name: "Sign out", exact: true }).click();
  await autofilled;
  await expect(account(page)).toContainText(`Signed in as ${username}`);
  await expect(methods).toContainText("Last used");
  const after = await cdp.send("WebAuthn.getCredentials", { authenticatorId: id });
  expect(after.credentials[0]?.signCount).toBeGreaterThan(0);

  // In a browser without that autofill, the button does it.
  await page.addInitScript(() => {
    PublicKeyCredential.isConditionalMediationAvailable = async () => false;
  });
  await page.reload();
  await signOut(page);
  await shot(page, "sign-in");
  await account(page).getByRole("button", { name: "Sign in with a passkey" }).click();
  await expect(account(page)).toContainText(`Signed in as ${username}`);

  // A second one from the same device is refused by the device itself.
  await account(page).getByRole("button", { name: "Add a passkey" }).click();
  await account(page).getByRole("button", { name: "Add passkey" }).click();
  await expect(account(page).getByRole("alert")).toHaveText(
    "this device already has a passkey for your account",
  );
  await account(page).getByRole("button", { name: "Cancel" }).click();

  const row = methods.getByRole("listitem").filter({ hasText: "Passkey: Work laptop" });
  await row.getByRole("button", { name: "Remove" }).click();
  await row.getByRole("button", { name: "Remove" }).click();
  await expect(methods).not.toContainText("Passkey");

  // The device still has it, but it no longer signs in to anything.
  await signOut(page);
  await account(page).getByRole("button", { name: "Sign in with a passkey" }).click();
  await expect(account(page).getByRole("alert")).toHaveText(
    "this passkey is not added to any account here; remove it from your device",
  );
  await expect(account(page).getByLabel("Username or email")).toBeVisible();
});
