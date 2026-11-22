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
        MICROSCOPE_RECORD_CHUNK_MS: "1500",
        MICROSCOPE_RECORD_POST_MS: "1500",
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

test("records two of three programs into two projects, never mentions the third, and exercises Data window", async ({
  page,
  services,
}, info) => {
  test.setTimeout(120_000);

  const state = await mkdtemp(path.join(tmpdir(), "microscope-p7-e2e-"));
  const suffix = randomBytes(3).toString("hex");
  const progAName = `pa-${suffix}`;
  const progBName = `pb-${suffix}`;
  const progCName = `pc-${suffix}`;
  const binA = path.join(state, progAName);
  const binB = path.join(state, progBName);
  const binC = path.join(state, progCName);
  await Promise.all([
    symlink(hotCallsBinary, binA),
    symlink(hotCallsBinary, binB),
    symlink(hotCallsBinary, binC),
  ]);

  const agentLog: string[] = [];
  let procA: ChildProcess | null = null;
  let procB: ChildProcess | null = null;
  let procC: ChildProcess | null = null;
  let agent: ChildProcess | null = null;

  try {
    await signUp(page, services);

    const orgResp = await page.evaluate(async () => {
      const r = await fetch("/api/orgs", {
        method: "POST",
        headers: { "Content-Type": "application/json" },
        body: JSON.stringify({ name: "PolicyOrg" }),
      });
      return r.json() as Promise<{ id: number }>;
    });
    const orgId = orgResp.id;

    const [projAlpha, projBeta] = await page.evaluate(async (org) => {
      const mk = async (name: string) => {
        const r = await fetch("/api/projects", {
          method: "POST",
          headers: { "Content-Type": "application/json" },
          body: JSON.stringify({ name, org_id: org }),
        });
        return r.json() as Promise<{ id: number; name: string }>;
      };
      return [await mk("alpha"), await mk("beta")] as const;
    }, orgId);

    const enrollResp = await page.evaluate(async (org) => {
      const r = await fetch("/api/fleet/keys", {
        method: "POST",
        headers: { "Content-Type": "application/json" },
        body: JSON.stringify({ org_id: org, name: "p7-key", tags: { environment: "prod" } }),
      });
      return r.json() as Promise<{ enroll_url: string }>;
    }, orgId);

    procA = spawn(binA, [], {
      env: { ...process.env, MICROSCOPE_PROJECT: "alpha", MICROSCOPE_SERVICE: "svc-alpha" },
      stdio: "ignore",
    });
    procB = spawn(binB, [], { stdio: "ignore" });
    procC = spawn(binC, [], { stdio: "ignore" });
    agent = startAgent(enrollResp.enroll_url, state, agentLog);

    await expect
      .poll(
        () =>
          page.evaluate(async (org) => {
            const r = await fetch(`/api/fleet/machines?org=${org}&state=live`);
            const body = (await r.json()) as { machines?: unknown[] };
            return body.machines?.length ?? 0;
          }, orgId),
        { timeout: 15_000 },
      )
      .toBe(1);

    await page.goto(`${services.origin}/#data/services?org=${orgId}`);
    const dialog = page.getByRole("dialog", { name: "Data" });
    await expect(dialog).toBeVisible();

    await expect
      .poll(
        () =>
          page.evaluate(async (org) => {
            const r = await fetch(`/api/data/services?org=${org}`);
            const body = (await r.json()) as { services?: { name: string }[] };
            return (body.services ?? []).some((s) => s.name === "svc-alpha");
          }, orgId),
        { timeout: 25_000 },
      )
      .toBe(true);

    await dialog.getByLabel("Rule pattern").fill(progBName);
    await dialog.getByLabel("Target project").selectOption(String(projBeta.id));
    await dialog.getByLabel("Service name").fill("svc-beta");
    await dialog.getByRole("button", { name: "Add rule" }).click();

    await expect
      .poll(
        () =>
          page.evaluate(async (org) => {
            const r = await fetch(`/api/data/services?org=${org}`);
            const body = (await r.json()) as { services?: { name: string }[] };
            return (body.services ?? []).some((s) => s.name === "svc-beta");
          }, orgId),
        { timeout: 25_000 },
      )
      .toBe(true);

    const checkPayload = await page.evaluate(
      async ({ org, alphaId, betaId }) => {
        const svcs = await (await fetch(`/api/data/services?org=${org}`)).text();
        const flameFor = async (pid: number) => {
          const r = await fetch("/api/query/flame", {
            method: "POST",
            headers: { "Content-Type": "application/json" },
            body: JSON.stringify({
              scope: {
                org_id: org,
                project_ids: [pid],
                filters: [],
                time_range: { kind: "relative", duration_secs: 900 },
                refresh: "off",
              },
            }),
          });
          return r.json() as Promise<{ samples: number }>;
        };
        const dims = await (
          await fetch("/api/query/dimensions", {
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
            }),
          })
        ).text();
        return {
          svcs,
          dims,
          alphaSamples: (await flameFor(alphaId)).samples,
          betaSamples: (await flameFor(betaId)).samples,
        };
      },
      { org: orgId, alphaId: projAlpha.id, betaId: projBeta.id },
    );

    expect(checkPayload.alphaSamples).toBeGreaterThan(0);
    expect(checkPayload.betaSamples).toBeGreaterThan(0);
    expect(checkPayload.svcs).not.toContain(progCName);
    expect(checkPayload.dims).not.toContain(progCName);

    await dialog.getByRole("tab", { name: "Recording" }).click();
    await dialog.getByRole("button", { name: "Preset dev for alpha prod" }).click();
    await dialog.getByRole("button", { name: "Record a burst…" }).click();
    await dialog.getByRole("button", { name: "Start burst" }).click();
    await expect(dialog.getByRole("button", { name: "Open session" }).first()).toBeVisible({
      timeout: 10_000,
    });

    await dialog.getByRole("tab", { name: "Privacy" }).click();
    await expect(dialog).toContainText("Always on");
    await dialog.getByPlaceholder("e.g. customer id").fill("customer id");
    await dialog.getByPlaceholder("e.g. cust_[0-9]+").fill("cust_[0-9]+");
    await dialog.getByLabel("Preview sample text").fill("tenant cust_998877");
    await dialog.getByRole("button", { name: "Test" }).click();
    await expect(dialog).toContainText("replaced");
    await expect(dialog).not.toContainText("cust_998877");
    await dialog.getByRole("button", { name: "Add sanitizer rule" }).click();
    await expect(dialog).toContainText("customer id");

    await dialog.getByRole("tab", { name: "Symbols" }).click();
    await expect(dialog).toContainText("Project upload tokens");

    await dialog.getByRole("tab", { name: "Storage" }).click();
    await expect(dialog).toContainText("Your own bucket");
    await expect(dialog).toContainText("Phase 10");
    await dialog.getByLabel("Detail retention (days)").fill("14");
    await dialog.getByRole("button", { name: "Save retention" }).click();
    await expect(dialog.getByLabel("Detail retention (days)")).toHaveValue("14");

    await dialog.getByRole("tab", { name: "Recording" }).click();
    await dialog.getByRole("button", { name: "Open session" }).first().click();
    await expect(page).toHaveURL(/\/s\/\d+/u);
  } finally {
    if (procA) {
      await stopChild(procA, "SIGKILL");
    }
    if (procB) {
      await stopChild(procB, "SIGKILL");
    }
    if (procC) {
      await stopChild(procC, "SIGKILL");
    }
    if (agent) {
      const code = await stopChild(agent, "SIGTERM");
      expect(code).toBe(0);
    }
    await rm(state, { recursive: true, force: true });
    if (info.status !== info.expectedStatus) {
      await info.attach("agent.log", { body: agentLog.join("\n") });
    }
  }
});
