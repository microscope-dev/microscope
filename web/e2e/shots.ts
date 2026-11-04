// Screenshots of every screen in light and dark at a few widths, for
// reviewing the look without clicking through: `just web-shot [PROGRAM]`.

import { spawnSync } from "node:child_process";
import { access, mkdir } from "node:fs/promises";
import * as path from "node:path";

import { chromium, type Page } from "@playwright/test";

import { fixture, menu, root, startMicroscope } from "./server.ts";

const out = path.join(root, "target", "web-shots");
const program = process.argv[2] ?? fixture("kvstore");
const widths = [1440, 1024, 720];

/** Shoots each scheme at each width. Drawings draw again in a new scheme's
 * colors and at a new width, so a page with them waits `settle` ms. */
async function shoot(page: Page, name: string, settle = 0): Promise<void> {
  for (const scheme of ["dark", "light"] as const) {
    await page.emulateMedia({ colorScheme: scheme });
    for (const width of widths) {
      await page.setViewportSize({ width, height: 800 });
      if (settle > 0) {
        await page.waitForTimeout(settle);
      }
      await page.screenshot({ path: path.join(out, `${name}-${scheme}-${width}.png`) });
    }
  }
  console.log(name);
}

await mkdir(out, { recursive: true });
const server = await startMicroscope([program], "");
const browser = await chromium.launch();
try {
  const page = await browser.newPage({ viewport: { width: 1440, height: 800 } });
  // What went wrong in the page, printed as it happens.
  page.on("pageerror", (error) => console.error(`page error: ${error.stack ?? error.message}`));
  page.on("console", (message) => {
    if (message.type() === "error") {
      console.error(`console: ${message.text()}`);
    }
  });
  const status = page.getByTestId("status");
  await page.goto(server.link);
  await page.waitForURL(/\/s\//);
  await shoot(page, "1-loaded");

  await page.keyboard.press("F5");
  await status.getByText("Running").waitFor();
  await page.waitForTimeout(1000);
  await shoot(page, "2-running");

  await page.keyboard.press("F6");
  await status.getByText("Stopped").waitFor();
  await shoot(page, "3-stopped");

  await page.getByRole("button", { name: "Share" }).click();
  await page
    .getByRole("textbox", { name: "Join link" })
    .and(page.locator("[value*=join]"))
    .waitFor();
  await shoot(page, "4-share");
  await page.keyboard.press("Escape");

  await menu(page, "File", "Launch a program…");
  const launch = page.getByRole("tabpanel", { name: "Launch" });
  await launch.getByRole("combobox", { name: "Program" }).pressSequentially("build/test-programs/");
  await launch.getByRole("option").first().waitFor();
  await shoot(page, "5-launch");
  await page.keyboard.press("Escape");
  await menu(page, "File", "Attach to a process…");
  await page.waitForTimeout(300);
  await shoot(page, "6-attach");
  await menu(page, "File", "Settings");
  await shoot(page, "6-settings");

  const viewer = await browser.newPage({ viewport: { width: 1440, height: 800 } });
  await page.getByRole("button", { name: "Share" }).click();
  const field = page.getByRole("textbox", { name: "Join link" });
  await field.and(page.locator("[value*=join]")).waitFor();
  await viewer.goto(await field.inputValue());
  await viewer.getByTestId("status").getByText("Stopped").waitFor();
  await shoot(viewer, "7-viewer");

  await page.keyboard.press("Escape");
  await page.keyboard.press("Shift+F5");
  await status.getByText("Exited").waitFor();
  await shoot(page, "8-exited");

  await shootWelcome(page);
  await shootProfile(page);
  await shootDrawings(page);
  await shootMesh(page);
} finally {
  await browser.close();
  await server.stop();
}

/** Where `microscope web` with nothing to debug starts. */
async function shootWelcome(page: Page): Promise<void> {
  const empty = await startMicroscope([], "");
  try {
    await page.setViewportSize({ width: 1440, height: 800 });
    await page.goto(empty.link);
    await page.getByRole("tab", { name: "Welcome", exact: true }).waitFor();
    await shoot(page, "0-welcome");
  } finally {
    await empty.stop();
  }
}

/** The Profile layout: a second of spin recorded, then served. */
async function shootProfile(page: Page): Promise<void> {
  const file = path.join(out, "spin.usprof");
  // Ending the recording kills spin, so record exits as it did; the file
  // is what counts.
  spawnSync(
    path.join(root, "target", "debug", "microscope"),
    ["record", "-o", file, "--duration", "1", "--", fixture("spin")],
    { stdio: "ignore" },
  );
  await access(file);
  const served = await startMicroscope([file], "");
  try {
    await page.setViewportSize({ width: 1440, height: 800 });
    await page.goto(served.link);
    await page.waitForURL(/\/profile\//);
    await page.getByTestId("accounting").waitFor();
    await shoot(page, "0-profile", 300);
  } finally {
    await served.stop();
  }
}

/** The Drawings view of the metrics fixture after two ticks, so each chart
 * shows the stop before. */
async function shootDrawings(page: Page): Promise<void> {
  const views = path.join(root, "tests/fixtures/c/metrics/metrics.views");
  const metrics = await startMicroscope(["--views", views, fixture("metrics")], "");
  try {
    await page.setViewportSize({ width: 1440, height: 800 });
    await page.goto(metrics.link);
    await page.waitForURL(/\/s\//);
    const adder = page.getByRole("textbox", { name: "Add a breakpoint" });
    await adder.fill("metrics.c:119");
    await adder.press("Enter");
    await adder.press("Escape");
    for (const stop of ["#2", "#3"]) {
      await page.keyboard.press("F5");
      await page.getByTestId("stops").getByText(stop).waitFor();
    }
    await page.keyboard.press("Alt+v");
    const board = page.getByRole("region", { name: "Drawing of m", exact: true });
    await board.locator(".drawing-caption").waitFor();
    await shoot(page, "9-drawings", 400);
    await board.getByRole("button", { name: "heatmap", exact: true }).click();
    await shoot(page, "10-drawings-heatmap", 400);
  } finally {
    await metrics.stop();
  }
}
/** The mesh fixture's torus in the built-in live viewer. */
async function shootMesh(page: Page): Promise<void> {
  const views = path.join(root, "tests/fixtures/cpp/mesh/mesh.views");
  const mesh = await startMicroscope(["--views", views, fixture("mesh")], "");
  try {
    await page.setViewportSize({ width: 1440, height: 800 });
    await page.goto(mesh.link);
    await page.waitForURL(/\/s\//);
    const adder = page.getByRole("textbox", { name: "Add a breakpoint" });
    await adder.fill("mesh.cpp:124");
    await adder.press("Enter");
    await adder.press("Escape");
    await page.keyboard.press("F5");
    await page.getByTestId("stops").getByText("#2").waitFor();
    await page.keyboard.press("Alt+v");
    const board = page.getByRole("region", { name: "Drawing of mesh", exact: true });
    await board.locator("canvas.live-canvas[data-stop]").waitFor();
    await shoot(page, "11-drawings-mesh", 400);
  } finally {
    await mesh.stop();
  }
}

console.log(`screenshots in ${out}`);
