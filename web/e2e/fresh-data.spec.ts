import { type ChildProcess, spawn } from "node:child_process";
import { randomBytes } from "node:crypto";
import { existsSync } from "node:fs";
import { mkdtemp, rm, symlink } from "node:fs/promises";
import { tmpdir } from "node:os";
import * as path from "node:path";

import type { Page } from "@playwright/test";

import { allowErrors, test as base, expect, root } from "./server";
import { type Services, skippedServices, skipServices, startServices } from "./services";

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

test.skip(skipServices, skippedServices);
test.skip(!existsSync(hotCallsBinary), `fixture is not built at ${hotCallsBinary}`);

const account = (page: Page) => page.getByRole("tabpanel", { name: "Account" });

async function signUp(page: Page, services: Services): Promise<string> {
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
  return username;
}

function startSlowCadenceAgent(
  enroll: string,
  state: string,
  processName: string,
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
        MICROSCOPE_RECORD_SERVICE: "hot-calls",
        MICROSCOPE_RECORD_PROJECT: "services",
        MICROSCOPE_RECORD_PRESET: "dev",
        MICROSCOPE_RECORD_CHUNK_MS: "2000",
        MICROSCOPE_RECORD_POST_MS: "60000",
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

test("at 5-second refresh, a 60s-cadence real agent delivers hot_function on screen within 15 seconds", async ({
  page,
  services,
}, info) => {
  test.setTimeout(90_000);

  const state = await mkdtemp(path.join(tmpdir(), "microscope-fresh-e2e-"));
  const processName = `hot-${randomBytes(4).toString("hex")}`;
  const workloadBinary = path.join(state, processName);
  await symlink(hotCallsBinary, workloadBinary);
  const agentLog: string[] = [];
  let workload: ChildProcess | null = null;
  let running: ChildProcess | null = null;

  try {
    await signUp(page, services);

    const orgResp = await page.evaluate(async () => {
      const r = await fetch("/api/orgs", {
        method: "POST",
        headers: { "Content-Type": "application/json" },
        body: JSON.stringify({ name: "FreshDataOrg" }),
      });
      return r.json() as Promise<{ id: number }>;
    });
    const orgId = orgResp.id;

    await page.evaluate(async (org) => {
      await fetch("/api/projects", {
        method: "POST",
        headers: { "Content-Type": "application/json" },
        body: JSON.stringify({ name: "services", org_id: org }),
      });
    }, orgId);

    const enrollResp = await page.evaluate(async (org) => {
      const r = await fetch("/api/fleet/keys", {
        method: "POST",
        headers: { "Content-Type": "application/json" },
        body: JSON.stringify({ org_id: org, name: "fresh-key", tags: { environment: "dev" } }),
      });
      return r.json() as Promise<{ enroll_url: string }>;
    }, orgId);

    workload = spawn(workloadBinary, [], { stdio: "ignore" });
    running = startSlowCadenceAgent(enrollResp.enroll_url, state, processName, agentLog);

    await expect
      .poll(
        async () => {
          const machines = await page.evaluate(async (org) => {
            const r = await fetch(`/api/fleet/machines?org=${org}&state=live`);
            const body = (await r.json()) as { machines?: { state: string }[] };
            return body.machines?.length ?? 0;
          }, orgId);
          return machines;
        },
        { timeout: 15_000 },
      )
      .toBe(1);

    await page.goto(`${services.origin}/d/fleet-overview?org=${orgId}`);
    const scopeBar = page.getByTestId("scope-bar");
    await expect(scopeBar).toBeVisible();

    const startedWatchAt = Date.now();
    await scopeBar.getByLabel("Refresh interval").selectOption("s5");

    await expect(scopeBar.getByTestId("scope-watching")).toContainText("1 watched", {
      timeout: 5_000,
    });

    const flame = page.getByTestId("fleet-flame-pane");
    await expect(flame.getByRole("img", { name: "Flame graph" })).toBeVisible({
      timeout: 15_000,
    });
    const elapsedMs = Date.now() - startedWatchAt;
    expect(elapsedMs).toBeLessThan(15_000);

    await flame.getByRole("searchbox", { name: "Search frames" }).fill("hot_function");
    await expect(flame.getByTestId("matched")).toHaveText(
      /^matched (?!0\.0%)\d+(\.\d+)?% of samples$/u,
      { timeout: 5_000 },
    );
    await expect(flame.getByTestId("pane-lag")).toBeVisible();

    await page.evaluate(() => {
      Object.defineProperty(document, "visibilityState", {
        configurable: true,
        get: () => "hidden",
      });
      document.dispatchEvent(new Event("visibilitychange"));
    });
    await expect(scopeBar.getByTestId("scope-watching")).toBeHidden({ timeout: 3_000 });

    await page.evaluate(() => {
      Object.defineProperty(document, "visibilityState", {
        configurable: true,
        get: () => "visible",
      });
      document.dispatchEvent(new Event("visibilitychange"));
    });
    await expect(scopeBar.getByTestId("scope-watching")).toContainText("1 watched", {
      timeout: 5_000,
    });

    const freshFlame = await page.evaluate(async (org) => {
      const r = await fetch("/api/query/flame", {
        method: "POST",
        headers: { "Content-Type": "application/json" },
        body: JSON.stringify({
          scope: {
            org_id: org,
            project_ids: [],
            filters: [],
            time_range: { kind: "relative", duration_secs: 900 },
            refresh: "off",
          },
          fresh: true,
          flush_timeout_ms: 8000,
        }),
      });
      return r.json() as Promise<{
        samples: number;
        flush?: { requested: number; flushed: number; timedOut: unknown[] };
      }>;
    }, orgId);
    expect(freshFlame.samples).toBeGreaterThan(0);
    expect(freshFlame.flush?.flushed).toBeGreaterThanOrEqual(1);
    expect(freshFlame.flush?.timedOut).toEqual([]);
  } finally {
    if (workload) {
      await stopChild(workload, "SIGKILL");
    }
    if (running) {
      const agentCode = await stopChild(running, "SIGTERM");
      expect(agentCode).toBe(0);
    }
    await rm(state, { recursive: true, force: true });
    if (info.status !== info.expectedStatus) {
      await info.attach("agent.log", { body: agentLog.join("") });
    }
  }
});
