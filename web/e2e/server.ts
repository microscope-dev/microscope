import { type ChildProcess, spawn, spawnSync } from "node:child_process";
import { mkdir, rm, writeFile } from "node:fs/promises";
import * as path from "node:path";
import { createInterface } from "node:readline";

import { test as base, expect, type Page } from "@playwright/test";

export { expect };

export const root = path.join(import.meta.dirname, "..", "..");

export function fixture(name: string): string {
  return path.join(root, "build", "test-programs", name);
}

export function recordCalls(file: string, symbol: string): number {
  const recorded = spawnSync(
    path.join(root, "target", "debug", "microscope"),
    [
      "record",
      "-o",
      file,
      "--function",
      symbol,
      "--function-period",
      "1",

      "--stack-bytes",
      "512",
      "--",
      fixture("profile-stacks-gcc-o0"),
      "1",
    ],
    { encoding: "utf8", stdio: ["ignore", "ignore", "pipe"] },
  );
  const said = recorded.stderr;
  const kept = /recorded (\d+) samples/u.exec(said);
  if (recorded.status !== 0 || !kept) {
    throw new Error(`recording ${symbol} failed: ${said}`);
  }
  const dropped = /the kernel dropped (\d+) of 6 calls/u.exec(said);
  const samples = Number(kept[1]);
  expect(samples + Number(dropped?.[1] ?? 0), said).toBe(6);
  expect(samples, said).toBeGreaterThan(0);
  return samples;
}

export interface Microscope {
  origin: string;

  link: string;

  start(args: string[]): Promise<Microscope>;

  log: string[];
  stop(): Promise<void>;
}

export async function startMicroscope(args: string[], recording: string): Promise<Microscope> {
  const child = spawn(
    process.env.MICROSCOPE_WEB_BINARY ?? path.join(root, "target", "debug", "microscope"),
    ["web", "--port", "0", ...args],
    {
      cwd: root,
      env: { ...process.env, USER: "tester", MICROSCOPE_FLIGHT_RECORDING: recording },
      stdio: ["ignore", "pipe", "pipe"],
    },
  );
  const log: string[] = [];
  const lines = createInterface({ input: child.stdout });
  createInterface({ input: child.stderr }).on("line", (line) => log.push(`stderr: ${line}`));
  const link = await new Promise<string>((resolve, reject) => {
    const timer = setTimeout(
      () => reject(new Error(`microscope web never printed a link: ${log}`)),
      10_000,
    );
    lines.on("line", (line) => {
      log.push(line);
      if (line.startsWith("open ")) {
        clearTimeout(timer);
        resolve(line.slice("open ".length));
      }
    });
    child.on("exit", (code) => reject(new Error(`microscope web exited with ${code}: ${log}`)));
  });
  return {
    origin: new URL(link).origin,
    link,
    log,
    start: (more) => startMicroscope(more, `${recording}.next`),
    stop: () => stopChild(child),
  };
}

function stopChild(child: ChildProcess): Promise<void> {
  if (child.exitCode !== null || child.signalCode !== null) {
    return Promise.resolve();
  }
  return new Promise((resolve) => {
    child.on("exit", () => resolve());

    child.kill("SIGINT");
  });
}

export const test = base.extend<{ program: string[]; microscope: Microscope; quiet: void }>({
  quiet: [
    async ({ context }, provide) => {
      const errors: string[] = [];
      const watch = (page: Page) => {
        page.on("console", (message) => {
          const aborted = /downloadable font: download failed.*status=2152398850/u.test(
            message.text(),
          );
          if (
            message.type() === "error" &&
            !aborted &&
            !/failed to load resource/iu.test(message.text())
          ) {
            errors.push(`${page.url()}: ${message.text().slice(0, 300)}`);
          }
        });
        page.on("pageerror", (error) => errors.push(`${page.url()}: ${error.message}`));
        page.on("response", (response) => {
          if (response.status() >= 400) {
            errors.push(`${response.request().method()} ${response.url()}: ${response.status()}`);
          }
        });
      };
      for (const page of context.pages()) {
        watch(page);
      }
      context.on("page", watch);
      await provide();
      const allowed = allowedErrors.get(test.info().testId) ?? [];
      const unexpected = errors.filter((error) => !allowed.some((each) => each.test(error)));
      const expected = (expectedErrors.get(test.info().testId) ?? []).map((error) =>
        typeof error === "string" ? error : expect.stringMatching(error),
      );
      expect(unexpected).toEqual(expected);
    },
    { auto: true },
  ],
  program: [[fixture("spin")], { option: true }],
  microscope: async ({ program, context }, provide, info) => {
    const servers: Microscope[] = [];
    await mkdir(info.outputDir, { recursive: true });
    const recording = (index: number) => info.outputPath(`microscope-${index}.flight.log`);
    const first = await startMicroscope(program, recording(0));
    servers.push(first);
    await provide({
      ...first,
      start: async (args) => {
        const another = await startMicroscope(args, recording(servers.length));
        servers.push(another);
        return another;
      },
    });

    for (const page of context.pages()) {
      await page.close();
    }
    for (const server of servers) {
      await server.stop();
    }
    if (info.status === info.expectedStatus) {
      await rm(info.outputDir, { recursive: true, force: true });
    } else {
      for (const [index, server] of servers.entries()) {
        await writeFile(info.outputPath(`microscope-${index}.log`), server.log.join("\n"));
      }
    }
  },
});

const expectedErrors = new Map<string, (string | RegExp)[]>();

export function expectErrors(...errors: (string | RegExp)[]): void {
  expectedErrors.set(test.info().testId, errors);
}

const allowedErrors = new Map<string, RegExp[]>();

export function allowErrors(...errors: RegExp[]): void {
  allowedErrors.set(test.info().testId, errors);
}

export async function menu(page: Page, name: string, item: string): Promise<void> {
  await page.getByRole("menuitem", { name, exact: true }).click();
  await page.getByRole("menu", { name }).getByRole("menuitem", { name: item, exact: true }).click();
}

export function documents(page: Page) {
  return page
    .getByRole("tablist")
    .filter({ has: page.getByRole("tab", { name: "Disassembly", exact: true }) });
}

export async function join(page: Page, link: string): Promise<void> {
  await page.goto(link);
  const start = page.getByRole("tab", { name: "Account", exact: true });
  await expect(async () => {
    const { pathname } = new URL(page.url());
    expect(/^\/(s|profile)\/[0-9a-f]+/u.test(pathname) || (await start.isVisible())).toBe(true);
  }).toPass();
}

export async function showView(page: Page, label: string, profile?: string): Promise<void> {
  const titled = page.getByRole("tab").filter({
    has: page.locator("[data-part=tab-title]", { hasText: new RegExp(`^${label}$`, "u") }),
  });
  const chosen = profile === undefined ? titled : titled.filter({ hasText: profile });
  if ((await chosen.count()) === 0) {
    await menu(page, "View", label);
  } else {
    await chosen.click();
  }
  await expect(chosen).toHaveAttribute("aria-selected", "true");
}
