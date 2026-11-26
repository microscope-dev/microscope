import { type ChildProcess, spawn } from "node:child_process";
import { randomBytes } from "node:crypto";
import { existsSync } from "node:fs";
import { copyFile, mkdtemp, rm, symlink } from "node:fs/promises";
import { tmpdir } from "node:os";
import * as path from "node:path";

import type { Page } from "@playwright/test";

import { allowErrors, test as base, expect, menu, root } from "./server";
import { type Services, skippedServices, skipServices, startServices } from "./services";

const hotCallsBinary = path.join(root, "build", "test-programs", "hot-calls");
const nodeBinary = path.join(process.env.MICROSCOPE_NODE ?? "", "26", "node");
const busyScript = path.join(root, "tests", "fixtures", "node", "busy.js");

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

test.skip(skipServices, skippedServices);

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

function startAgent(
  enroll: string,
  state: string,
  processName: string,
  service: string,
  log: string[],
): ChildProcess {
  const child = spawn(
    path.join(root, "target", "debug", "microscope"),
    ["agent", "run", "--config", "/dev/null"],
    {
      env: {
        ...process.env,
        MICROSCOPE_ENROLL: enroll,
        MICROSCOPE_STATE_DIR: state,
        MICROSCOPE_CLOUD_METADATA: "false",
        MICROSCOPE_RECORD_PROCESS: processName,
        MICROSCOPE_RECORD_SERVICE: service,
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

async function enrollKey(page: Page, services: Services): Promise<string> {
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
  return enroll;
}

async function expectHostedFrame(page: Page, name: string): Promise<void> {
  await menu(page, "Dashboards", "Dashboards…");
  const dashDialog = page.getByRole("dialog", { name: "Dashboards" });
  await expect(dashDialog).toBeVisible();
  await dashDialog.getByTestId("dashboard-item-fleet-overview").click();
  await expect(dashDialog).toBeHidden();

  const scopeBar = page.getByTestId("scope-bar");
  await expect(scopeBar).toBeVisible();
  const flame = page.getByTestId("fleet-flame-pane");
  await expect(flame).toBeVisible();

  await expect(async () => {
    await scopeBar.getByRole("button", { name: "Refresh" }).click();
    await expect(flame.getByRole("img", { name: "Flame graph" })).toBeVisible({
      timeout: 1_000,
    });
  }).toPass({ timeout: 25_000 });

  await expect(async () => {
    await scopeBar.getByRole("button", { name: "Refresh" }).click();
    await flame.getByRole("searchbox", { name: "Search frames" }).fill(name);
    await expect(flame.getByTestId("matched")).toHaveText(
      /^matched (?!0(?:\.0+)?%)\d+(\.\d+)?% of samples$/u,
      { timeout: 1_000 },
    );
  }).toPass({ timeout: 25_000 });
}

test("records a live process with the real agent and renders its hot function in the hosted flame graph", async ({
  page,
  services,
}, info) => {
  test.skip(!existsSync(hotCallsBinary), `fixture is not built at ${hotCallsBinary}`);
  test.setTimeout(60_000);
  const enroll = await enrollKey(page, services);

  const state = await mkdtemp(path.join(tmpdir(), "microscope-first-chunk-e2e-"));
  const processName = `hot-${randomBytes(4).toString("hex")}`;
  const workloadBinary = path.join(state, processName);
  await symlink(hotCallsBinary, workloadBinary);
  const agentLog: string[] = [];
  const workload = spawn(workloadBinary, [], { stdio: "ignore" });
  const running = startAgent(enroll, state, processName, "hot-calls", agentLog);
  try {
    await expectHostedFrame(page, "hot_function");
    const version = page.getByRole("combobox", { name: "Version filter" }).locator("option").nth(1);
    await expect(version).toHaveText(/^[0-9a-f]{12}$/u);
    await expect(version).toHaveAttribute("title", /^[0-9a-f]{40}$/u);
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

test("names a Node process's JIT-compiled JavaScript in the hosted flame graph", async ({
  page,
  services,
}, info) => {
  test.skip(!existsSync(nodeBinary), `node is not at ${nodeBinary}`);
  test.setTimeout(60_000);
  const enroll = await enrollKey(page, services);

  const state = await mkdtemp(path.join(tmpdir(), "microscope-node-jit-e2e-"));
  const processName = `node-${randomBytes(4).toString("hex")}`;
  const workloadBinary = path.join(state, processName);
  await copyFile(nodeBinary, workloadBinary);
  const agentLog: string[] = [];
  const workload = spawn(
    workloadBinary,
    [
      "--perf-basic-prof",
      "--allow-natives-syntax",
      "--sparkplug-filter=sparkplugSpin",
      busyScript,
      "--seconds",
      "60",
    ],
    { cwd: state, stdio: "ignore" },
  );
  const running = startAgent(enroll, state, processName, "busy-node", agentLog);
  try {
    await expectHostedFrame(page, "JS:*'turbofanSpin");
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
