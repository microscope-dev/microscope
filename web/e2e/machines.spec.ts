import { type ChildProcess, spawn } from "node:child_process";
import { randomBytes } from "node:crypto";
import { mkdtemp, rm } from "node:fs/promises";
import { hostname, tmpdir } from "node:os";
import * as path from "node:path";

import type { Page } from "@playwright/test";

import { allowErrors, test as base, expect, menu, root } from "./server";
import { haveServices, type Services, servicesBinary, startServices } from "./services";

const test = base.extend<{ services: Services }>({
  services: async ({ context }, provide, info) => {
    allowErrors(/GET \S+\/api\/users\/me: 401$/u);
    const services = await startServices({ agents: true });
    try {
      await provide(services);
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

async function signUp(page: Page, services: Services): Promise<void> {
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
}

function agent(enroll: string, state: string): ChildProcess {
  const child = spawn(
    path.join(root, "target", "debug", "microscope"),
    ["agent", "run", "--config", "/dev/null"],
    {
      env: {
        ...process.env,
        MICROSCOPE_ENROLL: enroll,
        MICROSCOPE_STATE_DIR: state,
        MICROSCOPE_CLOUD_METADATA: "false",
      },
      stdio: ["ignore", "ignore", "pipe"],
    },
  );
  child.stderr?.on("data", () => {});
  return child;
}

function stopped(child: ChildProcess): Promise<number | null> {
  if (child.exitCode !== null) {
    return Promise.resolve(child.exitCode);
  }
  return new Promise((done) => {
    child.on("exit", (code) => done(code));
    child.kill("SIGTERM");
  });
}

test("adds a machine with a real agent, finds it, and revokes the key that let it in", async ({
  page,
  services,
}) => {
  test.setTimeout(40_000);
  await signUp(page, services);

  await menu(page, "Machines", "Add machines…");
  const add = page.getByRole("dialog", { name: "Add machines" });
  await add.getByRole("textbox", { name: "Environment" }).fill("prod");
  await add.getByRole("textbox", { name: /Labels/u }).fill("team=payments");
  await add.getByRole("button", { name: "Create key" }).click();
  await add.getByRole("tab", { name: "Linux (systemd)" }).click();
  const snippet = await add.getByRole("tabpanel", { name: "Linux (systemd)" }).textContent();
  const enroll = /enroll = "([^"]+)"/u.exec(snippet ?? "")?.[1] ?? "";
  expect(enroll).toMatch(/^http:\/\/mse_[a-z0-9]{56}@127\.0\.0\.1:\d+$/u);
  await expect(add.getByText(/^Waiting for the first machine/u)).toBeVisible();

  const state = await mkdtemp(path.join(tmpdir(), "microscope-machines-e2e-"));
  const running = agent(enroll, state);
  try {
    const host = hostname();
    await expect(add.getByRole("list", { name: "Arrived" })).toContainText(host, {
      timeout: 15_000,
    });

    await page.keyboard.press("Escape");
    await expect(add).toBeHidden();
    await menu(page, "Machines", "Find a machine…");
    const machines = page.getByRole("dialog", { name: "Machines" });
    await expect(machines.getByRole("searchbox", { name: "Find a machine" })).toBeFocused();
    await page.keyboard.type(host.slice(0, 4));
    const row = machines.getByRole("button", { name: `${host}, live` });
    await row.click();
    const panel = machines.getByRole("complementary", { name: "Machine" });
    await expect(panel.getByRole("heading", { name: host })).toBeVisible();
    await expect(panel).toContainText("prod · team=payments");
    await expect(panel).toContainText("with key prod");
    expect(new URL(page.url()).hash).toMatch(/^#machines\/m\/\d+$/u);

    await machines.getByRole("tab", { name: "Enrollment keys" }).click();
    const keyRow = machines.getByRole("row").filter({ hasText: "team=payments" });
    await expect(keyRow.getByRole("button", { name: "1", exact: true })).toHaveCount(2);
    await machines.getByRole("button", { name: "Revoke" }).click();
    const question = page.getByRole("alertdialog", { name: "Revoke key prod?" });
    await question.getByRole("checkbox").check();
    await question.getByRole("textbox", { name: "Type prod to confirm" }).fill("prod");
    await question.getByRole("button", { name: "Revoke key" }).click();
    await expect(question).toBeHidden();
    await expect(keyRow).toHaveClass(/revoked/u);

    await machines.getByRole("tab", { name: "Machines" }).click();
    await machines.getByRole("combobox", { name: "State" }).selectOption("any");
    await expect(machines.getByRole("button", { name: `${host}, revoked` })).toBeVisible();
  } finally {
    expect(await stopped(running)).toBe(0);
    await rm(state, { recursive: true, force: true });
  }
});
