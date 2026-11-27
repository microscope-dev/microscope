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
    allowErrors(/GET \S+\/api\/sessions\/\d+: 404$/u);
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
test.beforeEach(() => {
  expect(existsSync(hotCallsBinary), `fixture is not built at ${hotCallsBinary}`).toBe(true);
});

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

function startAgent(
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
        MICROSCOPE_TEST_DIE_WITH_PARENT: String(process.pid),
        MICROSCOPE_CLOUD_METADATA: "false",
        MICROSCOPE_RECORD_PROCESS: processName,
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

test("opens Fleet overview on live agent data, narrows scope into a session, and shares it with a second user", async ({
  browser,
  services,
}, info) => {
  test.setTimeout(90_000);

  const ctxAlice = await browser.newContext();
  const ctxBob = await browser.newContext();
  const ctxOutsider = await browser.newContext();
  const alice = await ctxAlice.newPage();
  const bob = await ctxBob.newPage();
  const outsider = await ctxOutsider.newPage();

  const state = await mkdtemp(path.join(tmpdir(), "microscope-dashboards-e2e-"));
  const processName = `hot-${randomBytes(4).toString("hex")}`;
  const workloadBinary = path.join(state, processName);
  await symlink(hotCallsBinary, workloadBinary);
  const agentLog: string[] = [];
  let workload: ChildProcess | null = null;
  let running: ChildProcess | null = null;

  let failed = false;
  try {
    await signUp(alice, services);
    const bobName = await signUp(bob, services);
    await signUp(outsider, services);

    const orgResp = await alice.evaluate(async () => {
      const r = await fetch("/api/orgs", {
        method: "POST",
        headers: { "Content-Type": "application/json" },
        body: JSON.stringify({ name: "FleetDashOrg" }),
      });
      return r.json() as Promise<{ id: number }>;
    });
    const orgId = orgResp.id;

    await alice.evaluate(
      async ({ org, username }) => {
        await fetch(`/api/orgs/${org}/invitations`, {
          method: "POST",
          headers: { "Content-Type": "application/json" },
          body: JSON.stringify({ username, role: "member" }),
        });
      },
      { org: orgId, username: bobName },
    );

    await bob.evaluate(async (org) => {
      await fetch(`/api/invitations/${org}/accept`, {
        method: "POST",
        headers: { "Content-Type": "application/json" },
        body: "null",
      });
    }, orgId);

    await alice.evaluate(async (org) => {
      await fetch("/api/projects", {
        method: "POST",
        headers: { "Content-Type": "application/json" },
        body: JSON.stringify({ name: "services", org_id: org }),
      });
    }, orgId);

    const enrollResp = await alice.evaluate(async (org) => {
      const r = await fetch("/api/fleet/keys", {
        method: "POST",
        headers: { "Content-Type": "application/json" },
        body: JSON.stringify({ org_id: org, name: "dev-key", tags: { environment: "dev" } }),
      });
      return r.json() as Promise<{ enroll_url: string }>;
    }, orgId);
    expect(enrollResp.enroll_url).toMatch(/^http:\/\/mse_[a-z0-9]{56}@127\.0\.0\.1:\d+$/u);

    workload = spawn(workloadBinary, [], { stdio: "ignore" });
    running = startAgent(enrollResp.enroll_url, state, processName, agentLog);

    await alice.goto(`${services.origin}/d/fleet-overview?org=${orgId}`);
    const scopeBar = alice.getByTestId("scope-bar");
    await expect(scopeBar).toBeVisible();
    await expect(alice).toHaveURL(/\/d\/fleet-overview/u);

    const flame = alice.getByTestId("fleet-flame-pane");
    const overTime = alice.getByTestId("fleet-over-time-pane");
    const byMachine = alice.getByTestId("fleet-by-machine-pane");
    await expect(flame).toBeVisible();
    await expect(overTime).toBeVisible();
    await expect(byMachine).toBeVisible();

    await expect(async () => {
      await scopeBar.getByRole("button", { name: "Refresh" }).click();
      await expect(flame.getByRole("img", { name: "Flame graph" })).toBeVisible({
        timeout: 1_500,
      });
    }).toPass({ timeout: 30_000 });

    await expect(overTime.getByRole("img")).toBeVisible();
    await expect(byMachine.getByTestId("machine-heatmap-row").first()).toBeVisible();

    await expect(async () => {
      const options = await scopeBar
        .getByLabel("Service filter")
        .locator("option")
        .allTextContents();
      expect(options).toContain("hot-calls");
    }).toPass({ timeout: 10_000 });

    await scopeBar.getByLabel("Service filter").selectOption("hot-calls");
    await expect(alice).toHaveURL(/\/s\/\d+$/u, { timeout: 10_000 });
    await expect(scopeBar.getByTestId("scope-filter-chip")).toContainText("service=hot-calls");

    await alice.evaluate(() => {
      Object.defineProperty(navigator, "clipboard", {
        configurable: true,
        value: {
          writeText: async (text: string) => {
            (window as unknown as { copiedLink?: string }).copiedLink = text;
            await Promise.resolve();
          },
        },
      });
    });
    await scopeBar.getByRole("button", { name: "Copy link" }).click();
    await expect
      .poll(() =>
        alice.evaluate(() => (window as unknown as { copiedLink?: string }).copiedLink ?? ""),
      )
      .toMatch(/\/s\/\d+$/u);
    const sessionUrl = await alice.evaluate(
      () => (window as unknown as { copiedLink?: string }).copiedLink ?? "",
    );
    const sessionId = Number(/\/s\/(\d+)$/u.exec(sessionUrl)?.[1] ?? "0");
    expect(sessionId).toBeGreaterThan(0);

    const saved = await alice.evaluate(async (id) => {
      const r = await fetch(`/api/sessions/${id}`);
      return r.json() as Promise<{
        scope: {
          filters: { dimension: string; value: string }[];
          time_range: { kind: string; start_time?: number; end_time?: number };
        };
      }>;
    }, sessionId);
    expect(saved.scope.filters).toContainEqual({
      dimension: "service.name",
      value: "hot-calls",
    });
    expect(saved.scope.time_range.kind).toBe("absolute");
    expect(typeof saved.scope.time_range.start_time).toBe("number");
    expect(typeof saved.scope.time_range.end_time).toBe("number");

    await bob.goto(sessionUrl);
    const bobScopeBar = bob.getByTestId("scope-bar");
    await expect(bobScopeBar).toBeVisible({ timeout: 10_000 });
    await expect(bobScopeBar.getByTestId("scope-filter-chip")).toContainText("service=hot-calls");
    const bobSession = await bob.evaluate(async (id) => {
      const r = await fetch(`/api/sessions/${id}`);
      return r.json() as Promise<{
        scope: { time_range: { kind: string; start_time?: number; end_time?: number } };
      }>;
    }, sessionId);
    expect(bobSession.scope.time_range).toEqual(saved.scope.time_range);
    const bobFlame = bob.getByTestId("fleet-flame-pane");
    await expect(bobFlame.getByRole("img", { name: "Flame graph" })).toBeVisible({
      timeout: 15_000,
    });
    await bobFlame.getByRole("searchbox", { name: "Search frames" }).fill("hot_function");
    await expect(bobFlame.getByTestId("matched")).toHaveText(
      /^matched (?!0(?:\.0+)?%)\d+(\.\d+)?% of samples$/u,
    );

    const sessionMeta = await alice.evaluate(async (id) => {
      const r = await fetch(`/api/sessions/${id}`);
      return r.json() as Promise<{
        sharedAt: number | null;
        keptAt: number | null;
        pinStatus: string;
        visitorCount: number;
      }>;
    }, sessionId);
    expect(sessionMeta.sharedAt).not.toBeNull();
    expect(sessionMeta.keptAt).not.toBeNull();
    expect(sessionMeta.visitorCount).toBe(1);
    expect(sessionMeta.pinStatus).toBe("pinned");

    await outsider.goto(sessionUrl);
    await expect(outsider.getByRole("alert")).toContainText(/no such session/iu, {
      timeout: 10_000,
    });
  } catch (error) {
    failed = true;
    throw error;
  } finally {
    if (workload) {
      await stopChild(workload, "SIGKILL");
    }
    if (running) {
      const agentCode = await stopChild(running, "SIGTERM");
      expect.soft(agentCode).toBe(0);
    }
    await ctxAlice.close();
    await ctxBob.close();
    await ctxOutsider.close();
    await rm(state, { recursive: true, force: true });
    if (failed) {
      await info.attach("agent.log", { body: agentLog.join("") });
    }
  }
});
