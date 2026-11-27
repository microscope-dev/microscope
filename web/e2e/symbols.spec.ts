import { type ChildProcess, execFile, execFileSync, spawn } from "node:child_process";
import { randomBytes } from "node:crypto";
import { mkdtemp, rm, writeFile } from "node:fs/promises";
import { tmpdir } from "node:os";
import * as path from "node:path";
import { promisify } from "node:util";

import type { Page } from "@playwright/test";

import { allowErrors, test as base, expect, root } from "./server";
import { type Services, skippedServices, skipServices, startServices } from "./services";

const execFileAsync = promisify(execFile);

const microscopeBin = path.join(root, "target", "debug", "microscope");

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
  uploadSymbols: boolean,
  log: string[],
): ChildProcess {
  const child = spawn(microscopeBin, ["agent", "run", "--config", "/dev/null"], {
    env: {
      ...process.env,
      MICROSCOPE_ENROLL: enroll,
      MICROSCOPE_STATE_DIR: state,
      MICROSCOPE_TEST_DIE_WITH_PARENT: String(process.pid),
      MICROSCOPE_CLOUD_METADATA: "false",
      MICROSCOPE_RECORD_CHUNK_MS: "1500",
      MICROSCOPE_RECORD_POST_MS: "1500",
      MICROSCOPE_SYMBOLS_UPLOAD: uploadSymbols ? "true" : "false",
    },
    stdio: ["ignore", "ignore", "pipe"],
  });
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

async function compileStrippedFixture(
  dir: string,
  suffix: string,
): Promise<{
  debugPath: string;
  strippedPath: string;
  strippedName: string;
}> {
  const srcPath = path.join(dir, "fixture.c");
  const debugPath = path.join(dir, `app-debug-${suffix}`);
  const strippedName = `app-stripped-${suffix}`;
  const strippedPath = path.join(dir, strippedName);
  await writeFile(
    srcPath,
    `
#include <stdint.h>
static volatile uint64_t sink = 0;

__attribute__((noinline)) uint64_t hot_stripped_work(uint64_t rounds) {
    uint64_t acc = 1;
    for (uint64_t i = 0; i < rounds; i++) {
        acc = acc * 6364136223846793005ULL + i + 1ULL;
    }
    sink = acc;
    return acc;
}

int main(void) {
    for (;;) {
        hot_stripped_work(2000000ULL);
    }
}
`,
  );
  execFileSync("cc", [
    "-g",
    "-O2",
    "-fno-omit-frame-pointer",
    "-Wl,--build-id=sha1",
    "-o",
    debugPath,
    srcPath,
  ]);
  execFileSync("strip", ["--strip-all", "-o", strippedPath, debugPath]);
  return { debugPath, strippedPath, strippedName };
}

test("stripped binary shows addresses, then function names immediately after upload without re-storing, plus CLI login and debuginfod", async ({
  page,
  services,
}, info) => {
  test.setTimeout(120_000);

  const suffix = randomBytes(4).toString("hex");
  const projectName = `symproj-${suffix}`;
  const serviceName = `stripped-svc-${suffix}`;
  const workDir = await mkdtemp(path.join(tmpdir(), "microscope-p8-e2e-"));
  const stateDir = path.join(workDir, "agent-state");
  const credsFile = path.join(workDir, "credentials.json");
  const { debugPath, strippedPath, strippedName } = await compileStrippedFixture(workDir, suffix);

  const agentLog: string[] = [];
  let workload: ChildProcess | null = null;
  let agent: ChildProcess | null = null;
  let loginProc: ChildProcess | null = null;

  let failed = false;
  try {
    await signUp(page, services);

    const orgResp = await page.evaluate(async () => {
      const r = await fetch("/api/orgs", {
        method: "POST",
        headers: { "Content-Type": "application/json" },
        body: JSON.stringify({ name: "SymbolsOrg" }),
      });
      return r.json() as Promise<{ id: number }>;
    });
    const orgId = orgResp.id;

    const projResp = await page.evaluate(
      async ({ org, name }) => {
        const r = await fetch("/api/projects", {
          method: "POST",
          headers: { "Content-Type": "application/json" },
          body: JSON.stringify({ name, org_id: org }),
        });
        return r.json() as Promise<{ id: number; name: string }>;
      },
      { org: orgId, name: projectName },
    );
    const projectId = projResp.id;

    const enrollResp = await page.evaluate(async (org) => {
      const r = await fetch("/api/fleet/keys", {
        method: "POST",
        headers: { "Content-Type": "application/json" },
        body: JSON.stringify({ org_id: org, name: "p8-key", tags: { environment: "prod" } }),
      });
      return r.json() as Promise<{ enroll_url: string }>;
    }, orgId);

    const startNs = (Date.now() - 60_000) * 1_000_000;
    workload = spawn(strippedPath, [], {
      env: { ...process.env, MICROSCOPE_PROJECT: projectName, MICROSCOPE_SERVICE: serviceName },
      stdio: "ignore",
    });
    agent = startAgent(enrollResp.enroll_url, stateDir, false, agentLog);

    let flameBefore: {
      keys: { name: string; kind: string; module?: string | null }[];
      samples: number;
    } = { keys: [], samples: 0 };

    await expect
      .poll(
        async () => {
          const endNs = (Date.now() + 60_000) * 1_000_000;
          flameBefore = await page.evaluate(
            async ({ org, project, s, e }) => {
              const r = await fetch("/api/query/flame", {
                method: "POST",
                headers: { "Content-Type": "application/json" },
                body: JSON.stringify({
                  scope: {
                    org_id: org,
                    project_ids: [project],
                    time_range: { kind: "absolute", start_time: s, end_time: e },
                  },
                }),
              });
              return r.json() as Promise<{
                keys: { name: string; kind: string; module?: string | null }[];
                samples: number;
              }>;
            },
            { org: orgId, project: projectId, s: startNs, e: endNs },
          );
          return flameBefore.keys.some(
            (k) =>
              (k.kind === "address" || k.name.startsWith("<unnamed ")) &&
              (k.name.includes(strippedName) || (k.module ?? "").includes(strippedName)),
          );
        },
        { timeout: 30_000, intervals: [500] },
      )
      .toBe(true);

    await stopChild(workload, "SIGTERM");
    workload = null;
    await stopChild(agent, "SIGTERM");
    agent = null;

    expect(flameBefore.keys.some((k) => k.name === "hot_stripped_work")).toBe(false);

    await page.goto(`${services.origin}/#data/symbols?org=${orgId}`);
    const banner = page.getByTestId("symbols-missing-banner");
    await expect(banner).toBeVisible({ timeout: 15_000 });
    await expect(banner).toContainText("missing debug symbols");

    const table = page.getByTestId("symbols-builds-table");
    await expect(table).toContainText(serviceName);
    await expect(table).toContainText("Missing");

    await page.getByLabel("Upload token name").fill("e2e-ci");
    await page.getByRole("button", { name: "Create upload token" }).click();
    const secretEl = page.getByTestId("symbols-token-secret");
    await expect(secretEl).toBeVisible();
    const mspToken = ((await secretEl.textContent()) ?? "").trim();
    expect(mspToken.startsWith("msp_")).toBe(true);

    const { stdout: uploadOut } = await execFileAsync(
      microscopeBin,
      [
        "symbols",
        "upload",
        "--server",
        services.origin,
        "--token",
        mspToken,
        "--service",
        serviceName,
        "--git-commit",
        "123456789abc",
        debugPath,
      ],
      { encoding: "utf8" },
    );
    expect(uploadOut).toContain("uploaded ");

    const endNsAfter = (Date.now() + 60_000) * 1_000_000;
    const flameAfter = await page.evaluate(
      async ({ org, project, s, e }) => {
        const r = await fetch("/api/query/flame", {
          method: "POST",
          headers: { "Content-Type": "application/json" },
          body: JSON.stringify({
            scope: {
              org_id: org,
              project_ids: [project],
              time_range: { kind: "absolute", start_time: s, end_time: e },
            },
          }),
        });
        return r.json() as Promise<{ keys: { name: string; kind: string }[] }>;
      },
      { org: orgId, project: projectId, s: startNs, e: endNsAfter },
    );
    expect(
      flameAfter.keys.some((k) => k.name === "hot_stripped_work" && k.kind === "function"),
    ).toBe(true);
    expect(flameAfter.keys.some((k) => k.name === "main" && k.kind === "function")).toBe(true);

    await page.reload();
    await expect(page.getByTestId("symbols-builds-table")).toContainText("CI upload");
    await expect(page.getByTestId("symbols-builds-table")).toContainText("123456789abc");

    const loginStderr: string[] = [];
    loginProc = spawn(microscopeBin, ["login", "--server", services.origin, "--no-browser"], {
      env: { ...process.env, MICROSCOPE_CREDENTIALS_FILE: credsFile },
      stdio: ["ignore", "pipe", "pipe"],
    });
    const login = loginProc;
    const loginExited = new Promise<number | null>((resolve) => {
      login.on("exit", (code) => resolve(code));
    });
    loginProc.stderr?.on("data", (c: Buffer) => loginStderr.push(c.toString()));

    let verifyUrl = "";
    await expect
      .poll(
        () => {
          const match = /https?:\/\/\S+\/cli-login\?code=[A-Z0-9-]+/u.exec(loginStderr.join(""));
          if (match) {
            verifyUrl = match[0];
          }
          return verifyUrl.length > 0;
        },
        { timeout: 10_000, intervals: [200] },
      )
      .toBe(true);

    const parsedVerify = new URL(verifyUrl);
    await page.goto(`${services.origin}${parsedVerify.pathname}${parsedVerify.search}`);
    await page.getByTestId("cli-login-approve-btn").click();
    await expect(page.getByTestId("cli-login-approved")).toBeVisible();

    const loginExit = await loginExited;
    loginProc = null;
    expect(loginExit).toBe(0);

    const { stdout: whoami } = await execFileAsync(microscopeBin, ["whoami"], {
      env: { ...process.env, MICROSCOPE_CREDENTIALS_FILE: credsFile },
      encoding: "utf8",
    });
    expect(whoami).toContain(services.origin);

    const buildIdMatch = /uploaded ([0-9a-f]{16,128})/u.exec(uploadOut);
    expect(buildIdMatch).not.toBeNull();
    const buildId = buildIdMatch?.[1] ?? "";
    const debuginfoMagic = await page.evaluate(async (bid) => {
      const r = await fetch(`/buildid/${bid}/debuginfo`);
      if (!r.ok) {
        return `status:${r.status}`;
      }
      const buf = new Uint8Array(await r.arrayBuffer());
      return Array.from(buf.slice(0, 4)).join(",");
    }, buildId);
    expect(debuginfoMagic).toBe("127,69,76,70");

    await execFileAsync(microscopeBin, ["logout"], {
      env: { ...process.env, MICROSCOPE_CREDENTIALS_FILE: credsFile },
      encoding: "utf8",
    });
  } catch (error) {
    failed = true;
    throw error;
  } finally {
    if (loginProc) {
      await stopChild(loginProc, "SIGKILL");
    }
    if (workload) {
      await stopChild(workload, "SIGKILL");
    }
    if (agent) {
      await stopChild(agent, "SIGTERM");
    }
    if (failed && agentLog.length > 0) {
      await info.attach("agent.log", { body: agentLog.join("") });
    }
    await rm(workDir, { recursive: true, force: true });
  }
});
