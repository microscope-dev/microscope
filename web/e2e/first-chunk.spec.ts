import { type ChildProcess, spawn } from "node:child_process";
import { randomBytes } from "node:crypto";
import { existsSync } from "node:fs";
import { mkdtemp, rm } from "node:fs/promises";
import { tmpdir } from "node:os";
import * as path from "node:path";

import type { Page } from "@playwright/test";

import { allowErrors, test as base, expect, menu, root } from "./server";
import { haveServices, type Services, servicesBinary, startServices } from "./services";

const hotCallsBinary = path.join(root, "build", "test-programs", "hot-calls");

const test = base.extend<{ services: Services }>({
  services: async ({ context }, provide, info) => {
    allowErrors(/GET \S+\/api\/users\/me: 401$/u);
    const services = await startServices({ agents: true, storage: true });
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
test.skip(!existsSync(hotCallsBinary), `fixture is not built at ${hotCallsBinary}`);

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

function startAgent(enroll: string, state: string, log: string[]): ChildProcess {
  const child = spawn(
    path.join(root, "target", "debug", "microscope"),
    ["agent", "run", "--config", "/dev/null"],
    {
      env: {
        ...process.env,
        MICROSCOPE_ENROLL: enroll,
        MICROSCOPE_STATE_DIR: state,
        MICROSCOPE_CLOUD_METADATA: "false",
        MICROSCOPE_RECORD_PROCESS: "hot-calls",
        MICROSCOPE_RECORD_SERVICE: "hot-calls",
        MICROSCOPE_RECORD_PROJECT: "services",
        MICROSCOPE_RECORD_PRESET: "dev",
        MICROSCOPE_RECORD_CHUNK_MS: "1500",
        MICROSCOPE_RECORD_POST_MS: "1000",
      },
      stdio: ["ignore", "ignore", "pipe"],
    },
  );
  child.stderr?.on("data", (chunk: Buffer) => log.push(chunk.toString()));
  return child;
}

function stopChild(child: ChildProcess, signal: NodeJS.Signals): Promise<number | null> {
  if (child.exitCode !== null || child.signalCode !== null) {
    return Promise.resolve(child.exitCode);
  }
  return new Promise((done) => {
    child.on("exit", (code) => done(code));
    child.kill(signal);
  });
}

test("records a live process with the real agent and renders its hot function in the hosted flame graph", async ({
  page,
  services,
}, info) => {
  test.setTimeout(60_000);
  await signUp(page, services);

  await menu(page, "File", "New project…");
  const projects = page.getByRole("tabpanel", { name: "Projects" });
  await projects.getByLabel("Project name").fill("services");
  await projects.getByRole("button", { name: "Create project" }).click();
  await expect(projects).toContainText("services");

  await menu(page, "Machines", "Add machines…");
  const add = page.getByRole("dialog", { name: "Add machines" });
  await add.getByRole("textbox", { name: "Environment" }).fill("dev");
  await add.getByRole("button", { name: "Create key" }).click();
  await add.getByRole("tab", { name: "Linux (systemd)" }).click();
  const snippet = await add.getByRole("tabpanel", { name: "Linux (systemd)" }).textContent();
  const enroll = /enroll = "([^"]+)"/u.exec(snippet ?? "")?.[1] ?? "";
  expect(enroll).toMatch(/^http:\/\/mse_[a-z0-9]{56}@127\.0\.0\.1:\d+$/u);
  await page.keyboard.press("Escape");
  await expect(add).toBeHidden();

  const state = await mkdtemp(path.join(tmpdir(), "microscope-first-chunk-e2e-"));
  const agentLog: string[] = [];
  const workload = spawn(hotCallsBinary, [], { stdio: "ignore" });
  const running = startAgent(enroll, state, agentLog);
  try {
    await menu(page, "Dashboards", "Flame graph…");
    const flame = page.getByRole("tabpanel", { name: "Flame graph" });
    await expect(flame.getByRole("combobox", { name: "Project" })).toBeVisible();

    await expect(async () => {
      await flame.getByRole("button", { name: "Refresh" }).click();
      await expect(flame.getByRole("img", { name: "Flame graph" })).toBeVisible({
        timeout: 1_000,
      });
    }).toPass({ timeout: 25_000 });

    await flame.getByRole("searchbox", { name: "Search frames" }).fill("hot_function");
    await expect(flame.getByTestId("matched")).toHaveText(
      /^matched (?!0\.0%)\d+(\.\d+)?% of samples$/u,
    );
  } finally {
    await stopChild(workload, "SIGKILL");
    const agentCode = await stopChild(running, "SIGTERM");
    await rm(state, { recursive: true, force: true });
    if (info.status !== info.expectedStatus) {
      await info.attach("agent.log", { body: agentLog.join("") });
    }
    expect(agentCode).toBe(0);
  }
});
