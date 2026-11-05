// Each test gets its own `microscope web`, started from the development build
// (which serves build/web from disk), and stopped when the test ends.

import { type ChildProcess, spawn } from "node:child_process";
import { mkdir, rm, writeFile } from "node:fs/promises";
import * as path from "node:path";
import { createInterface } from "node:readline";

import { test as base, expect, type Page } from "@playwright/test";

export { expect };

export const root = path.join(import.meta.dirname, "..", "..");

export function fixture(name: string): string {
  return path.join(root, "build", "test-programs", name);
}

export interface Microscope {
  /** The server's origin, such as `http://127.0.0.1:41234`. */
  origin: string;
  /** The link `microscope web` printed, which gives control. */
  link: string;
  /** Starts another `microscope web` with these arguments. */
  start(args: string[]): Promise<Microscope>;
  /** Everything the server printed so far, for failure messages. */
  log: string[];
  stop(): Promise<void>;
}

/** Starts `microscope web`, flight-recording to `recording`. */
export async function startMicroscope(args: string[], recording: string): Promise<Microscope> {
  const child = spawn(
    // A release build can stand in, to see how fast a large program is.
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
    // SIGINT ends the session the way Ctrl+C does, killing its program.
    child.kill("SIGINT");
  });
}

/**
 * `microscope` serves `program`. A failing test keeps each server's output and
 * flight recording in its output directory.
 */
export const test = base.extend<{ program: string[]; microscope: Microscope; quiet: void }>({
  // Every page stays free of console errors, such as a blocked font or a
  // failed request, unless a test expects one.
  quiet: [
    async ({ context }, provide) => {
      const errors: string[] = [];
      const watch = (page: Page) => {
        page.on("console", (message) => {
          // A failed request is recorded once, by its response, below. Firefox
          // also reports a font download it aborted as the page went away
          // (NS_BINDING_ABORTED), which no blocked or missing font causes.
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
    // Pages go before their server, so none sees it vanish and reconnects
    // while the test's errors are counted.
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

/** Declares the errors this test's pages will show, in order, as text or
 * as patterns. */
export function expectErrors(...errors: (string | RegExp)[]): void {
  expectedErrors.set(test.info().testId, errors);
}

const allowedErrors = new Map<string, RegExp[]>();

/** Declares errors this test's pages may show any number of times, which
 * `expectErrors` then leaves out. */
export function allowErrors(...errors: RegExp[]): void {
  allowedErrors.set(test.info().testId, errors);
}

/** Opens the join link and waits until the page shows the session, or
 * the profile it links to. */
/** Chooses an item from a menu on the menu bar. */
export async function menu(page: Page, name: string, item: string): Promise<void> {
  await page.getByRole("menuitem", { name, exact: true }).click();
  await page.getByRole("menu", { name }).getByRole("menuitem", { name: item, exact: true }).click();
}

/** The tab strip of the documents group: the open files, Disassembly,
 * Memory, and Drawings. */
export function documents(page: Page) {
  return page
    .getByRole("tablist")
    .filter({ has: page.getByRole("tab", { name: "Disassembly", exact: true }) });
}

/** Opens a join link and waits for where it leads: the session, the
 * profile the server serves, or else Account. */
export async function join(page: Page, link: string): Promise<void> {
  await page.goto(link);
  const start = page.getByRole("tab", { name: "Account", exact: true });
  await expect(async () => {
    const { pathname } = new URL(page.url());
    expect(/^\/(s|profile)\/[0-9a-f]+/u.test(pathname) || (await start.isVisible())).toBe(true);
  }).toPass();
}
