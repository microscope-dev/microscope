import { mkdir, readFile, writeFile } from "node:fs/promises";
import * as path from "node:path";

import type { Page } from "@playwright/test";

import { expect, fixture, join, type Microscope, test } from "./server";

const node = path.join(process.env.MICROSCOPE_NODE ?? "", "26", "node");

function npm(): string {
  for (const directory of (process.env.PATH ?? "").split(path.delimiter)) {
    if (directory.includes("nodejs")) {
      return path.join(directory, "npm");
    }
  }
  throw new Error(`no nodejs directory on PATH: ${process.env.PATH}`);
}

async function project(files: Record<string, string>): Promise<string> {
  const directory = test.info().outputPath("project");
  await mkdir(directory, { recursive: true });
  const all = { "package.json": JSON.stringify({ name: "app", version: "1.0.0" }), ...files };
  for (const [name, text] of Object.entries(all)) {
    await writeFile(path.join(directory, name), text);
  }
  return directory;
}

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

  await page.getByRole("tab", { name: /^Processes/u }).click();
  const switcher = page.getByRole("navigation", { name: "Processes" });
  const app = switcher.getByRole("button", { name: /stopped here/u });
  await expect(app).toContainText(/\[\d+\] node · executed by \[\d+\]/u, START);
  await expect(app).toHaveAttribute("aria-pressed", "true");
  await expect(switcher.getByRole("button", { name: /^\[1\] /u })).toContainText("launched");
  await expect(page.locator(".cm-frame-line")).toContainText("console.log('listening', port);");

  await page.keyboard.press("F5");
  await expect(switcher.getByRole("button", { name: /^\[1\] /u })).toContainText("exited", START);
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

  await page.getByRole("tab", { name: /^Processes/u }).click();
  const switcher = page.getByRole("navigation", { name: "Processes" });
  const stopped = switcher.getByRole("button", { name: /stopped here/u });
  const numbers: string[] = [];
  for (const _ of ["first", "second"]) {
    for (const number of numbers) {
      await expect(stopped).not.toContainText(`[${number}]`, START);
    }
    await expect(stopped).toContainText(/\[\d+\] node · (forked|executed) by \[\d+\]/u, START);
    await expect(stopped).toHaveAttribute("aria-pressed", "true");
    await expect(stopped).not.toContainText(/^\[1\]/u);
    await expect(page.locator(".cm-frame-line")).toContainText("return answer;");
    const number = /^\[(\d+)\]/u.exec((await stopped.textContent()) ?? "")?.[1] ?? "";
    expect(numbers).not.toContain(number);
    numbers.push(number);
    await page.keyboard.press("F5");
  }
  await expect(switcher.getByRole("button", { name: /^\[1\] /u })).toContainText("exited", START);
  await expect(page.getByTestId("status")).toContainText("Exited");
});

test("node --watch stops in the app, and again in the app it restarts", async ({
  page,
  microscope,
}) => {
  test.setTimeout(90_000);

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

  await page.getByRole("tab", { name: /^Processes/u }).click();
  const switcher = page.getByRole("navigation", { name: "Processes" });
  const stopped = switcher.getByRole("button", { name: /stopped here/u });
  await expect(stopped).toContainText(/\[\d+\] node · (forked|executed) by \[1\]/u, START);
  await expect(page.locator(".cm-frame-line")).toContainText("const marker =");
  const first = /^\[(\d+)\]/u.exec((await stopped.textContent()) ?? "")?.[1] ?? "";
  await page.keyboard.press("F5");

  const status = page.getByTestId("status");
  for (const sent of ["SIGTERM (SI_USER)", "SIGTERM (SI_TKILL)"]) {
    await expect(status).toContainText(sent, START);
    await expect(stopped).toContainText(`[${first}]`);
    await page.keyboard.press("F5");
  }

  await expect(stopped).not.toContainText(`[${first}]`, START);
  await expect(stopped).toContainText(/\[\d+\] node · (forked|executed) by \[1\]/u);
  await expect(page.locator(".cm-frame-line")).toContainText("const marker =");
  await switcher.locator("summary").click();
  await expect(
    switcher.getByRole("button", { name: new RegExp(`^\\[${first}\\] `, "u") }),
  ).toContainText("exited");
});

test("a Node core's tasks panel lists its event loops' work, each with its callback", async ({
  page,
  microscope,
}) => {
  const server = await microscope.start(["--core", fixture("node-tasks-26.core"), node]);
  await join(page, server.link);
  await expect(page.getByTestId("status")).toContainText("Core dump");

  await page.getByRole("tab", { name: /^Tasks/u }).click();
  const tasks = page.getByTestId("tasks");
  const timer = tasks.getByRole("listitem", { name: "task 2", exact: true });
  await expect(timer).toContainText("timeout 30000 ms");
  await expect(timer).toContainText(
    /callback sessionTimeout at \S*tests\/fixtures\/node\/tasks\.js:78$/u,
  );
  await expect(timer.getByRole("button")).toHaveCount(0);
  const opened = tasks.getByRole("listitem", { name: "task 5", exact: true });
  await expect(opened).toContainText("fs open");
  await expect(opened).toContainText("callback configOpened");
  await expect(tasks.getByRole("button", { name: /^task \d+$/u })).toHaveCount(2);
  await expect(tasks).toContainText("thread-pool job");

  const log = await readFile(`${fixture("node-tasks-26.core")}.log`, "utf8");
  const jobs = [...log.matchAll(/^TRUTH\tpool\t(\d+)$/gmu)]
    .map((match) => ((1n << 62n) | BigInt(match[1] ?? "")).toString())
    .toSorted();
  expect(jobs).toHaveLength(2);
  await page.getByRole("button", { name: "group" }).click();
  const groups = page.getByTestId("task-group");
  await expect(groups.getByRole("button")).toHaveCount(2);
  const shown = (await groups.getByRole("button").allTextContents()).toSorted();
  expect(shown).toEqual(jobs);
  const uncalled = groups.filter({ hasText: "no callback" });
  await expect(uncalled).toContainText("16 in");
  await expect(uncalled.getByTestId("stackless-task")).toHaveCount(16);
  await expect(uncalled.getByRole("button")).toHaveCount(0);

  const job = jobs[0] ?? "";
  await groups.getByRole("button", { name: `task ${job}`, exact: true }).click();
  await expect(page).toHaveURL(new RegExp(`/task/\\d+\\.${job}/f/`, "u"));
  await expect(page.getByRole("tab", { name: /^Call Stack/u })).toContainText(`task ${job}`);
  await expect(page.getByTestId("stack")).toContainText("worker");
});
