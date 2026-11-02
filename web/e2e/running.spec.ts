// Running a program from the page with VS Code's keys, and choosing another.

import { documents, expect, fixture, join, menu, test } from "./server";

test("F5, F6, and Shift+F5 run, pause, and kill without the browser acting", async ({
  page,
  microscope,
}) => {
  await join(page, microscope.link);
  const status = page.getByTestId("status");
  // A reload would lose this marker.
  await page.evaluate(() => {
    (window as { marker?: boolean }).marker = true;
  });
  await page.keyboard.press("F5");
  await expect(status).toContainText("Running");
  await page.keyboard.press("F6");
  await expect(status).toContainText("Stopped");
  await expect(page.getByRole("tabpanel", { name: "Threads" })).toContainText("stopped here");
  await page.keyboard.press("F5");
  await expect(status).toContainText("Running");
  await page.keyboard.press("Shift+F5");
  await expect(status).toContainText("Exited");
  await expect(status).toContainText("SIGKILL");
  expect(await page.evaluate(() => (window as { marker?: boolean }).marker)).toBe(true);
});

test("reloading keeps the session and its output", async ({ page, microscope }) => {
  await microscope.stop();
  const server = await microscope.start([fixture("output-streams")]);
  await join(page, server.link);
  await page.keyboard.press("F5");
  // It reads its input to the end, which Ctrl+D closes.
  await page.getByRole("textbox", { name: "Program input" }).press("Control+d");
  await expect(page.getByTestId("status")).toContainText("Exited");
  const output = page.getByTestId("output");
  await expect(output).toContainText("stdin: eof");
  const url = page.url();
  await page.reload();
  await expect(page).toHaveURL(url);
  await expect(output).toContainText("stdin: eof");
  await expect(page.getByTestId("status")).toContainText("Exited");
});

test("the Launch pane completes a path and loads it in place of the program", async ({
  page,
  microscope,
}) => {
  await join(page, microscope.link);
  const first = page.url();
  const watcher = await page.context().newPage();
  await watcher.goto(first);
  await expect(watcher.getByTestId("status")).toContainText("Not started");
  // Launching opens a pane beside the session, which closes with nothing lost.
  await menu(page, "File", "Launch a program…");
  await page.getByRole("button", { name: "Close Launch" }).click();
  await expect(page.getByRole("tab", { name: "Launch", exact: true })).toHaveCount(0);
  await expect(page).toHaveURL(first);
  await menu(page, "File", "Launch a program…");
  await expect(page.getByRole("tab", { name: "Launch", exact: true })).toHaveAttribute(
    "aria-selected",
    "true",
  );
  const program = page.getByRole("combobox", { name: "Program" });
  await program.pressSequentially("build/test-prog");
  await page.keyboard.press("Tab");
  await expect(program).toHaveValue("build/test-programs/");
  await program.pressSequentially("basic");
  await page.getByRole("option", { name: /^basic\s*executable$/ }).click();
  await expect(program).toHaveValue("build/test-programs/basic");
  await page.getByRole("button", { name: "Load", exact: true }).click();
  // Spin is still loaded, so loading asks first.
  await page.getByRole("button", { name: /End it and continue/ }).click();
  // The address follows to the new session.
  await expect(page).not.toHaveURL(first);
  await expect(page).toHaveURL(/\/s\/[0-9a-f]+$/);
  await expect(page).toHaveTitle(/^basic/);

  // A tab that watched the old session follows to the new one; a link to
  // it opened later says it ended, and offers the current one.
  await expect(watcher).toHaveURL(page.url());
  const late = await page.context().newPage();
  await late.goto(first);
  await expect(late.getByTestId("session-ended")).toBeVisible();
  await late.getByTestId("session-ended").getByRole("link").click();
  await expect(late).toHaveURL(page.url());
});

test("a program chosen from a Start pane keeps nothing of the one it replaced", async ({
  page,
  microscope,
}) => {
  await join(page, microscope.link);
  await page.keyboard.press("F5");
  await expect(page.getByTestId("status")).toContainText("Running");
  await page.keyboard.press("F6");
  const files = documents(page);
  await expect(files.getByRole("tab", { name: "spin.c", exact: true })).toBeVisible();
  await expect(page.getByTestId("registers")).toContainText("rip");
  await page.getByRole("tab", { name: "Console" }).click();
  await page.getByRole("textbox", { name: "Console" }).fill("spin_value + 1");
  await page.getByRole("textbox", { name: "Console" }).press("Enter");
  await expect(page.getByTestId("console")).toContainText("43");

  await menu(page, "File", "Open a core dump…");
  await page.getByRole("combobox", { name: "Core dump" }).fill(fixture("crash-gcc-o0-segv.core"));
  await page.keyboard.press("Escape");
  await page.getByRole("button", { name: "Open", exact: true }).click();
  await page.getByRole("button", { name: /End it and continue/ }).click();
  await expect(page.getByTestId("status")).toContainText("Core dump");

  // The core's own file, and registers with nothing to compare against.
  await expect(files.getByRole("tab", { name: "main.c", exact: true })).toBeVisible();
  await expect(files.getByRole("tab", { name: "spin.c", exact: true })).toHaveCount(0);
  await expect(page.getByTestId("registers")).toContainText("rip");
  await expect(page.getByTestId("registers").locator(".was")).toHaveCount(0);
  await expect(page.getByTestId("console")).not.toContainText("spin_value");
});

test("a replacement that fails leaves the old session running", async ({ page, microscope }) => {
  await join(page, microscope.link);
  const first = page.url();
  const watcher = await page.context().newPage();
  await watcher.goto(first);
  await expect(watcher.getByTestId("status")).toContainText("Not started");

  await menu(page, "File", "Launch a program…");
  await page.getByRole("combobox", { name: "Program" }).fill("/nonexistent/program");
  await page.keyboard.press("Escape");
  await page.getByRole("button", { name: "Load", exact: true }).click();
  await page.getByRole("button", { name: /End it and continue/ }).click();
  await expect(page.getByRole("alert")).toContainText("No such file");
  // The failure is not a program worth recalling.
  await expect(page.getByRole("button", { name: "program", exact: true })).toHaveCount(0);

  // The program that could not load ended nothing.
  await page.getByRole("button", { name: "Close Launch" }).click();
  await expect(page.getByTestId("status")).toContainText("Not started");
  await expect(watcher.getByTestId("status")).toContainText("Not started");
  await expect(watcher.getByTestId("session-ended")).toHaveCount(0);
});
