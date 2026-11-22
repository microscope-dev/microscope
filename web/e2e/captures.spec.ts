import { type ChildProcess, execFile, execFileSync, spawn } from "node:child_process";
import { randomBytes } from "node:crypto";
import { mkdtemp, rm, writeFile } from "node:fs/promises";
import { tmpdir } from "node:os";
import * as path from "node:path";
import { promisify } from "node:util";

import type { BrowserContext, Page } from "@playwright/test";

import { allowErrors, test as base, expect, root } from "./server";
import { type Services, skippedServices, skipServices, startServices } from "./services";

const execFileAsync = promisify(execFile);

const microscopeBin = path.join(root, "target", "debug", "microscope");

const test = base.extend<{ services: Services }>({
  services: async ({ context }, provide, info) => {
    allowErrors(
      /GET \S+\/api\/users\/me: 401$/u,
      /POST \S+\/api\/captures\/\d+\/query: 404$/u,
      /GET \S+\/api\/sessions\/\d+: 404$/u,
      /GET \S+\/api\/captures\/\d+\/download: 404$/u,
    );
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

async function signUp(page: Page, services: Services, prefix: string): Promise<string> {
  const username = `${prefix}-${randomBytes(4).toString("hex")}`;
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

function stopChild(child: ChildProcess, signal: NodeJS.Signals): Promise<number | null> {
  if (child.exitCode !== null || child.signalCode !== null) {
    return Promise.resolve(child.exitCode);
  }
  return new Promise((done) => {
    child.on("exit", (code) => done(code));
    child.kill(signal);
  });
}

async function compileWorkstationProgram(dir: string): Promise<string> {
  const srcPath = path.join(dir, "work.c");
  const binPath = path.join(dir, "work-demo");
  await writeFile(
    srcPath,
    `
#include <stdint.h>
static volatile uint64_t sink = 0;

__attribute__((noinline)) uint64_t workstation_hot_loop(uint64_t rounds) {
    uint64_t acc = 1;
    for (uint64_t i = 0; i < rounds; i++) {
        acc = acc * 6364136223846793005ULL + i + 1ULL;
    }
    sink = acc;
    return acc;
}

int main(int argc, char **argv) {
    (void)argc;
    (void)argv;
    for (int j = 0; j < 25; j++) {
        workstation_hot_loop(5000000ULL);
    }
    return 0;
}
`,
  );
  execFileSync("cc", ["-g", "-O2", "-fno-omit-frame-pointer", "-o", binPath, srcPath]);
  return binPath;
}

async function startLocalWeb(
  capturePath: string,
  credsFile: string,
  log: string[],
): Promise<{ child: ChildProcess; url: string }> {
  const child = spawn(microscopeBin, ["web", "--port", "0", capturePath], {
    env: {
      ...process.env,
      MICROSCOPE_CREDENTIALS_FILE: credsFile,
    },
    stdio: ["ignore", "pipe", "pipe"],
  });
  child.stdout?.on("data", (chunk: Buffer) => log.push(chunk.toString()));
  child.stderr?.on("data", (chunk: Buffer) => log.push(chunk.toString()));

  let url = "";
  await expect
    .poll(
      () => {
        const match = /open (http:\/\/127\.0\.0\.1:\d+\S*)/u.exec(log.join(""));
        if (match) {
          url = match[1] ?? "";
        }
        return url.length > 0;
      },
      { timeout: 15_000, intervals: [150] },
    )
    .toBe(true);
  return { child, url };
}

test("workstation capture uploads sanitized, opens from a link in a colleague's browser, enforces project grants, supports local Share, pinning, and microscope query", async ({
  browser,
  page,
  services,
}, info) => {
  test.setTimeout(150_000);

  const workDir = await mkdtemp(path.join(tmpdir(), "microscope-p9-e2e-"));
  const credsFile = path.join(workDir, "credentials.json");
  const capturePath = path.join(workDir, "workstation.usprof");
  const workBin = await compileWorkstationProgram(workDir);

  const plantedGithubToken = "ghp_1234567890abcdef1234567890abcdef1234";
  const plantedCustomerId = "CUSTOMER_987654";

  let loginProc: ChildProcess | null = null;
  let localWeb: ChildProcess | null = null;
  const localWebLog: string[] = [];
  const extraContexts: BrowserContext[] = [];

  try {
    await signUp(page, services, "alice");

    const bobCtx = await browser.newContext();
    extraContexts.push(bobCtx);
    const bobPage = await bobCtx.newPage();
    const bobUsername = await signUp(bobPage, services, "bob");

    const carolCtx = await browser.newContext();
    extraContexts.push(carolCtx);
    const carolPage = await carolCtx.newPage();
    const carolUsername = await signUp(carolPage, services, "carol");

    const outsiderCtx = await browser.newContext();
    extraContexts.push(outsiderCtx);
    const outsiderPage = await outsiderCtx.newPage();
    await signUp(outsiderPage, services, "outsider");

    const bobId = await bobPage.evaluate(async () => {
      const r = await fetch("/api/users/me");
      const data = (await r.json()) as { id: number };
      return data.id;
    });

    const setup = await page.evaluate(
      async ({ bobUser, carolUser }) => {
        const orgRes = await fetch("/api/orgs", {
          method: "POST",
          headers: { "Content-Type": "application/json" },
          body: JSON.stringify({ name: "CaptureOrg" }),
        });
        const org = (await orgRes.json()) as { id: number };

        await fetch(`/api/orgs/${org.id}`, {
          method: "PATCH",
          headers: { "Content-Type": "application/json" },
          body: JSON.stringify({ base_permission: "none" }),
        });

        const projRes = await fetch("/api/projects", {
          method: "POST",
          headers: { "Content-Type": "application/json" },
          body: JSON.stringify({ name: "checkout", org_id: org.id }),
        });
        const proj = (await projRes.json()) as { id: number };

        await fetch(`/api/orgs/${org.id}/invitations`, {
          method: "POST",
          headers: { "Content-Type": "application/json" },
          body: JSON.stringify({ username: bobUser, role: "member" }),
        });
        await fetch(`/api/orgs/${org.id}/invitations`, {
          method: "POST",
          headers: { "Content-Type": "application/json" },
          body: JSON.stringify({ username: carolUser, role: "member" }),
        });

        const teamRes = await fetch(`/api/orgs/${org.id}/teams`, {
          method: "POST",
          headers: { "Content-Type": "application/json" },
          body: JSON.stringify({ name: "eng" }),
        });
        const team = (await teamRes.json()) as { id: number };

        const ruleRes = await fetch("/api/data/privacy/rules", {
          method: "POST",
          headers: { "Content-Type": "application/json" },
          body: JSON.stringify({
            orgId: org.id,
            projectId: proj.id,
            name: "customer-id",
            fields: ["process.args"],
            pattern: "CUSTOMER_[0-9]+",
            action: "redact",
          }),
        });
        if (!ruleRes.ok) {
          throw new Error(`privacy rule failed: ${ruleRes.status} ${await ruleRes.text()}`);
        }

        return { orgId: org.id, projectId: proj.id, teamId: team.id };
      },
      { bobUser: bobUsername, carolUser: carolUsername },
    );

    await bobPage.evaluate(async (org) => {
      await fetch(`/api/invitations/${org}/accept`, {
        method: "POST",
        headers: { "Content-Type": "application/json" },
        body: "null",
      });
    }, setup.orgId);

    await carolPage.evaluate(async (org) => {
      await fetch(`/api/invitations/${org}/accept`, {
        method: "POST",
        headers: { "Content-Type": "application/json" },
        body: "null",
      });
    }, setup.orgId);

    await page.evaluate(
      async ({ orgId, projectId, teamId, uid }) => {
        const addMember = await fetch(`/api/orgs/${orgId}/teams/${teamId}/members/${uid}`, {
          method: "PUT",
        });
        if (!addMember.ok) {
          throw new Error(`team add failed: ${addMember.status}`);
        }
        const grant = await fetch(`/api/projects/${projectId}/teams/${teamId}`, {
          method: "PUT",
          headers: { "Content-Type": "application/json" },
          body: JSON.stringify({ permission: "read" }),
        });
        if (!grant.ok) {
          throw new Error(`project grant failed: ${grant.status}`);
        }
      },
      { orgId: setup.orgId, projectId: setup.projectId, teamId: setup.teamId, uid: bobId },
    );

    await execFileAsync(
      microscopeBin,
      [
        "record",
        "-o",
        capturePath,
        "--",
        workBin,
        "--token",
        plantedGithubToken,
        "--cust",
        plantedCustomerId,
      ],
      { encoding: "utf8" },
    );

    const loginStderr: string[] = [];
    loginProc = spawn(microscopeBin, ["login", "--server", services.origin, "--no-browser"], {
      env: { ...process.env, MICROSCOPE_CREDENTIALS_FILE: credsFile },
      stdio: ["ignore", "pipe", "pipe"],
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
        { timeout: 10_000, intervals: [150] },
      )
      .toBe(true);

    const parsedVerify = new URL(verifyUrl);
    await page.goto(`${services.origin}${parsedVerify.pathname}${parsedVerify.search}`);
    await page.getByTestId("cli-login-approve-btn").click();
    await expect(page.getByTestId("cli-login-approved")).toBeVisible();

    const loginExit = await new Promise<number | null>((resolve) => {
      loginProc?.on("exit", (code) => resolve(code));
    });
    loginProc = null;
    expect(loginExit).toBe(0);

    const { stdout: uploadJson } = await execFileAsync(
      microscopeBin,
      [
        "upload",
        capturePath,
        "--project",
        String(setup.projectId),
        "--server",
        services.origin,
        "--json",
      ],
      {
        env: { ...process.env, MICROSCOPE_CREDENTIALS_FILE: credsFile },
        encoding: "utf8",
      },
    );
    const uploaded = JSON.parse(uploadJson) as {
      url: string;
      sessionId: number;
      captureId: number;
      redactedCount: number;
    };
    expect(uploaded.url).toContain(`/s/${uploaded.sessionId}`);
    expect(uploaded.redactedCount).toBeGreaterThan(0);

    await bobPage.goto(uploaded.url);
    const bar = bobPage.getByTestId("capture-session-bar");
    await expect(bar).toBeVisible({ timeout: 15_000 });
    await expect(bar).toContainText("workstation.usprof");
    await expect(bar).toContainText("checkout");
    await expect(bar).toContainText("pinned");
    await expect(bobPage.getByTestId("profile").first()).toBeVisible({ timeout: 15_000 });

    await bobPage.getByRole("tab", { name: /Functions/u }).click();
    await expect(bobPage.getByText("workstation_hot_loop").first()).toBeVisible({
      timeout: 15_000,
    });

    await bobPage.getByRole("tab", { name: "workstation.usprof", exact: true }).click();
    await bobPage.getByRole("button", { name: "File details" }).click();
    await expect(bobPage.getByTestId("command")).toBeVisible({ timeout: 15_000 });
    const commandText = (await bobPage.getByTestId("command").textContent()) ?? "";
    expect(commandText).not.toContain(plantedGithubToken);
    expect(commandText).not.toContain(plantedCustomerId);
    expect(commandText).toContain("[redacted]");

    const downloadMagic = await bobPage.evaluate(async (cid) => {
      const r = await fetch(`/api/captures/${cid}/download`);
      if (!r.ok) {
        return `status:${r.status}`;
      }
      const bytes = new Uint8Array(await r.arrayBuffer());
      return new TextDecoder().decode(bytes.slice(0, 8));
    }, uploaded.captureId);
    expect(downloadMagic).toBe("USCOPEOB");

    await carolPage.goto(uploaded.url);
    await expect(carolPage.getByTestId("session-locked-notice")).toBeVisible({ timeout: 15_000 });
    const carolQueryStatus = await carolPage.evaluate(async (cid) => {
      const r = await fetch(`/api/captures/${cid}/query`, {
        method: "POST",
        headers: { "Content-Type": "application/json" },
        body: JSON.stringify({ method: "profile", params: {} }),
      });
      return r.status;
    }, uploaded.captureId);
    expect(carolQueryStatus).toBe(404);

    const outsiderStatuses = await outsiderPage.evaluate(
      async ({ sid, cid }) => {
        const s = await fetch(`/api/sessions/${sid}`);
        const d = await fetch(`/api/captures/${cid}/download`);
        return { session: s.status, download: d.status };
      },
      { sid: uploaded.sessionId, cid: uploaded.captureId },
    );
    expect(outsiderStatuses).toEqual({ session: 404, download: 404 });

    await page.goto(uploaded.url);
    await expect(page.getByTestId("capture-unpin")).toBeVisible({ timeout: 15_000 });
    await page.getByTestId("capture-unpin").click();
    await expect(page.getByTestId("capture-keep-pin")).toBeVisible({ timeout: 15_000 });
    await page.getByTestId("capture-keep-pin").click();
    await expect(page.getByTestId("capture-pin-confirm")).toBeVisible();
    await page.getByRole("button", { name: "Confirm keep & pin" }).click();
    await expect(page.getByTestId("capture-unpin")).toBeVisible({ timeout: 15_000 });

    const startedLocal = await startLocalWeb(capturePath, credsFile, localWebLog);
    localWeb = startedLocal.child;
    const localCtx = await browser.newContext();
    extraContexts.push(localCtx);
    const localPage = await localCtx.newPage();
    await localPage.goto(startedLocal.url);
    await expect(localPage.getByTestId("profile-share")).toBeVisible({ timeout: 15_000 });
    await localPage.getByTestId("profile-share").click();
    await expect(localPage.getByTestId("share-upload-submit")).toBeVisible({ timeout: 15_000 });
    await localPage.getByTestId("share-upload-submit").click();
    const sharedBox = localPage.getByRole("textbox", { name: "Shared capture link" });
    await expect(sharedBox).toBeVisible({ timeout: 20_000 });
    const sharedFromWebUrl = await sharedBox.inputValue();
    expect(sharedFromWebUrl).toContain(`${services.origin}/s/`);

    await bobPage.goto(sharedFromWebUrl);
    await expect(bobPage.getByTestId("capture-session-bar")).toBeVisible({ timeout: 15_000 });
    await expect(bobPage.getByTestId("profile").first()).toBeVisible({ timeout: 15_000 });

    await page.goto(`${services.origin}/d/fleet-overview?org=${setup.orgId}`);
    await expect(page.getByTestId("scope-bar")).toBeVisible({ timeout: 15_000 });
    const copyBtn = page.getByTestId("copy-query-btn").first();
    await expect(copyBtn).toBeVisible({ timeout: 15_000 });
    await copyBtn.click();
    await expect(page.getByTestId("copy-query-cli")).toBeVisible();
    await expect(page.getByTestId("copy-query-curl")).toBeVisible();

    const { stdout: dimsJson } = await execFileAsync(
      microscopeBin,
      ["query", "dimensions", "--org", String(setup.orgId), "--server", services.origin, "--json"],
      {
        env: { ...process.env, MICROSCOPE_CREDENTIALS_FILE: credsFile },
        encoding: "utf8",
      },
    );
    const dims = JSON.parse(dimsJson) as { orgId: number; projects: { name: string }[] };
    expect(dims.orgId).toBe(setup.orgId);
    expect(dims.projects.some((p) => p.name === "checkout")).toBe(true);

    const { stdout: flameJson } = await execFileAsync(
      microscopeBin,
      [
        "query",
        "flame",
        "--org",
        String(setup.orgId),
        "--project",
        "checkout",
        "--last",
        "1h",
        "--server",
        services.origin,
        "--json",
      ],
      {
        env: { ...process.env, MICROSCOPE_CREDENTIALS_FILE: credsFile },
        encoding: "utf8",
      },
    );
    const flame = JSON.parse(flameJson) as { locked: boolean; nodes: number };
    expect(flame.locked).toBe(false);
    expect(typeof flame.nodes).toBe("number");
  } finally {
    for (const ctx of extraContexts) {
      await ctx.close();
    }
    if (loginProc) {
      await stopChild(loginProc, "SIGKILL");
    }
    if (localWeb) {
      await stopChild(localWeb, "SIGTERM");
    }
    if (info.status !== info.expectedStatus && localWebLog.length > 0) {
      await info.attach("local-web.log", { body: localWebLog.join("") });
    }
    await rm(workDir, { recursive: true, force: true });
  }
});
