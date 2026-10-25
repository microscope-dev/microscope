// Node programs that start other nodes, debugged from the page: a
// breakpoint in a .js file, set before the run, stops the node that runs
// it, which the switcher shows among the group's processes.

import { mkdir, readFile, writeFile } from "node:fs/promises";
import * as path from "node:path";
import type { Page } from "@playwright/test";
import { expect, fixture, join, type Microscope, test } from "./server";

const node = path.join(process.env.MICROSCOPE_NODE ?? "", "26", "node");

/** The npm beside the node on PATH, which the dev shell provides. */
function npm(): string {
  for (const directory of (process.env.PATH ?? "").split(path.delimiter)) {
    if (directory.includes("nodejs")) {
      return path.join(directory, "npm");
    }
  }
  throw new Error(`no nodejs directory on PATH: ${process.env.PATH}`);
}

/** Writes `files` into a directory of the test's output, and returns it.
 * A package.json of its own keeps web's `"type": "module"` from applying. */
async function project(files: Record<string, string>): Promise<string> {
  const directory = test.info().outputPath("project");
  await mkdir(directory, { recursive: true });
  const all = { "package.json": JSON.stringify({ name: "app", version: "1.0.0" }), ...files };
  for (const [name, text] of Object.entries(all)) {
    await writeFile(path.join(directory, name), text);
  }
  return directory;
}

/** Joins `server`'s session and sets a breakpoint at each location. */
async function breakAt(page: Page, server: Microscope, locations: string[]): Promise<void> {
  await join(page, server.link);
  await expect(page.getByTestId("status")).toContainText("Not started");
  const add = page.getByRole("textbox", { name: "Add a breakpoint" });
  for (const location of locations) {
    await add.fill(location);
    await add.press("Enter");
    await expect(page.getByTestId("breakpoints")).toContainText(location);
  }
}

// A node starting under the debugger, through npm's own node first, takes
// longer than the page's default wait.
const START = { timeout: 30_000 };

test("npm start stops at the app's breakpoint, its node beside npm's", async ({
  page,
  microscope,
}) => {
  test.setTimeout(90_000);
  const directory = await project({
    "package.json": JSON.stringify({
      name: "app",
      version: "1.0.0",
      scripts: { start: "node server.js" },
    }),
    "server.js": "const port = 8080;\nconsole.log('listening', port);\n",
  });
  const server = await microscope.start([
    "--cwd",
    directory,
    "--env",
    `npm_config_cache=${path.join(directory, "npm-cache")}`,
    "--env",
    "npm_config_update_notifier=false",
    npm(),
    "--",
    "start",
  ]);
  await breakAt(page, server, ["server.js:2"]);
  await page.keyboard.press("F5");

  // The app is the node npm's shell executed, which stopped the group.
  const switcher = page.getByRole("navigation", { name: "Processes" });
  const app = switcher.getByRole("button", { name: /stopped here/ });
  await expect(app).toContainText(/\[\d+\] node · executed by \[\d+\]/, START);
  await expect(app).toHaveAttribute("aria-pressed", "true");
  await expect(switcher.getByRole("button", { name: /^\[1\] / })).toContainText("launched");
  await expect(page.locator(".cm-frame-line")).toContainText("console.log('listening', port);");

  // Continuing runs the app and npm to their ends.
  await page.keyboard.press("F5");
  await expect(switcher.getByRole("button", { name: /^\[1\] / })).toContainText("exited", START);
  await expect(page.getByTestId("status")).toContainText("Exited");
});

test("node --test stops in each file's node at its breakpoint", async ({ page, microscope }) => {
  test.setTimeout(90_000);
  const body =
    "require('node:test')('runs', () => {\n  const answer = 42;\n  return answer;\n});\n";
  const directory = await project({ "a.test.js": body, "b.test.js": body });
  const server = await microscope.start([
    "--cwd",
    directory,
    node,
    "--",
    "--test",
    "a.test.js",
    "b.test.js",
  ]);
  await breakAt(page, server, ["a.test.js:3", "b.test.js:3"]);
  await page.keyboard.press("F5");

  // Each file runs in a node of its own, started by the runner, which
  // stops where its file's breakpoint is; the runner runs neither.
  const switcher = page.getByRole("navigation", { name: "Processes" });
  const stopped = switcher.getByRole("button", { name: /stopped here/ });
  const numbers: string[] = [];
  for (const _ of ["first", "second"]) {
    for (const number of numbers) {
      await expect(stopped).not.toContainText(`[${number}]`, START);
    }
    await expect(stopped).toContainText(/\[\d+\] node · (forked|executed) by \[\d+\]/, START);
    await expect(stopped).toHaveAttribute("aria-pressed", "true");
    await expect(stopped).not.toContainText(/^\[1\]/);
    await expect(page.locator(".cm-frame-line")).toContainText("return answer;");
    const number = /^\[(\d+)\]/.exec((await stopped.textContent()) ?? "")?.[1] ?? "";
    expect(numbers).not.toContain(number);
    numbers.push(number);
    await page.keyboard.press("F5");
  }
  await expect(switcher.getByRole("button", { name: /^\[1\] / })).toContainText("exited", START);
  await expect(page.getByTestId("status")).toContainText("Exited");
});

test("node --watch stops in the app, and again in the app it restarts", async ({
  page,
  microscope,
}) => {
  test.setTimeout(90_000);
  // The first run changes its own file and waits to be replaced; the
  // second, which finds the marker, ends.
  const directory = await project({
    "app.js": [
      "const fs = require('node:fs');",
      "const marker = __dirname + '/restarted';",
      "if (!fs.existsSync(marker)) {",
      "  fs.writeFileSync(marker, '');",
      "  setTimeout(() => fs.appendFileSync(__filename, '// changed\\n'), 200);",
      "  setInterval(() => {}, 1000);",
      "}",
      "",
    ].join("\n"),
  });
  const server = await microscope.start(["--cwd", directory, node, "--", "--watch", "app.js"]);
  await breakAt(page, server, ["app.js:2"]);
  await page.keyboard.press("F5");

  const switcher = page.getByRole("navigation", { name: "Processes" });
  const stopped = switcher.getByRole("button", { name: /stopped here/ });
  await expect(stopped).toContainText(/\[\d+\] node · (forked|executed) by \[1\]/, START);
  await expect(page.locator(".cm-frame-line")).toContainText("const marker =");
  const first = /^\[(\d+)\]/.exec((await stopped.textContent()) ?? "")?.[1] ?? "";
  await page.keyboard.press("F5");

  // The watcher ends the app with SIGTERM to start it again, which stops
  // the group as any signal does, and so does the SIGTERM the app's node
  // raises again at itself once its handler has run; continuing passes each.
  const status = page.getByTestId("status");
  for (const sent of ["SIGTERM (SI_USER)", "SIGTERM (SI_TKILL)"]) {
    await expect(status).toContainText(sent, START);
    await expect(stopped).toContainText(`[${first}]`);
    await page.keyboard.press("F5");
  }

  // The restarted app stops at the same line, and the one it replaced has
  // exited.
  await expect(stopped).not.toContainText(`[${first}]`, START);
  await expect(stopped).toContainText(/\[\d+\] node · (forked|executed) by \[1\]/);
  await expect(page.locator(".cm-frame-line")).toContainText("const marker =");
  await expect(
    switcher.getByRole("button", { name: new RegExp(`^\\[${first}\\] `) }),
  ).toContainText("exited");
});

test("a Node core's tasks panel lists its event loops' work, each with its callback", async ({
  page,
  microscope,
}) => {
  const server = await microscope.start(["--core", fixture("node-tasks-26.core"), node]);
  await join(page, server.link);
  await expect(page.getByTestId("status")).toContainText("Core dump");

  // A timer is what it waits for, then the program's callback, and opens
  // no stack; a pool job runs on its thread.
  const tasks = page.getByTestId("tasks");
  const timer = tasks.getByRole("listitem", { name: "task 2", exact: true });
  await expect(timer).toContainText("timeout 30000 ms");
  await expect(timer).toContainText(
    /callback sessionTimeout at \S*tests\/fixtures\/node\/tasks\.js:78$/,
  );
  await expect(timer.getByRole("button")).toHaveCount(0);
  const opened = tasks.getByRole("listitem", { name: "task 5", exact: true });
  await expect(opened).toContainText("fs open");
  await expect(opened).toContainText("callback configOpened");
  await expect(tasks.getByRole("button", { name: /^task \d+$/ })).toHaveCount(2);
  await expect(tasks).toContainText("thread-pool job");

  // Grouped, only the pool jobs open a stack. Their numbers, bit 62 and
  // the thread ids the core's log names, are above 2^53, and are shown
  // exactly.
  const log = await readFile(`${fixture("node-tasks-26.core")}.log`, "utf8");
  const jobs = [...log.matchAll(/^TRUTH\tpool\t(\d+)$/gm)]
    .map((match) => ((1n << 62n) | BigInt(match[1] ?? "")).toString())
    .sort();
  expect(jobs).toHaveLength(2);
  await page.getByRole("button", { name: "group" }).click();
  const groups = page.getByTestId("task-group");
  await expect(groups.getByRole("button")).toHaveCount(2);
  const shown = (await groups.getByRole("button").allTextContents()).sort();
  expect(shown).toEqual(jobs);
  const uncalled = groups.filter({ hasText: "no callback" });
  await expect(uncalled).toContainText("16 in");
  await expect(uncalled.getByTestId("stackless-task")).toHaveCount(16);
  await expect(uncalled.getByRole("button")).toHaveCount(0);

  // A pool job opens its own thread's stack, through libuv's worker.
  const job = jobs[0] ?? "";
  await groups.getByRole("button", { name: `task ${job}`, exact: true }).click();
  await expect(page).toHaveURL(new RegExp(`/task/\\d+\\.${job}/f/`));
  await expect(page.getByRole("region", { name: "Call stack" })).toContainText(`task ${job}`);
  await expect(page.getByTestId("stack")).toContainText("worker");
});
