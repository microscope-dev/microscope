// How long the metrics fixture's drawings take with a million samples, from a
// stop to its picture on screen: `just web-bench [ROUNDS]`. It reports the
// median and the slowest of ROUNDS stops for each drawing, with the binary
// MICROSCOPE_WEB_BINARY names (the recipe builds a release one). Not a test:
// the times depend on the machine.

import * as path from "node:path";

import { chromium } from "@playwright/test";

import { fixture, root, startMicroscope } from "./server.ts";

const rounds = Number(process.argv[2] ?? 5);
const views = path.join(root, "tests/fixtures/c/metrics/metrics.views");
const server = await startMicroscope(["--views", views, fixture("metrics"), "--", "--large"], "");
const browser = await chromium.launch();
try {
  const page = await browser.newPage({ viewport: { width: 1440, height: 900 } });
  page.on("pageerror", (error) => console.error(`page error: ${error.message}`));
  await page.goto(server.link);
  await page.waitForURL(/\/s\//u);
  const adder = page.getByRole("textbox", { name: "Add a breakpoint" });
  await adder.fill("metrics.c:119");
  await adder.press("Enter");
  await adder.press("Escape");
  let stop = 2;
  await page.keyboard.press("F5");
  await page.waitForURL(/\/stop\/2\//u);
  await page.keyboard.press("Alt+v");
  const board = page.getByRole("region", { name: "Drawing of m", exact: true });
  const head = board.locator(".drawing-head");
  const group = board.getByRole("group", { name: "Drawings of m" });
  await group.waitFor();
  const tabs = await group.getByRole("button").allTextContents();
  console.log(`${rounds} stops each, from the stop to its picture:`);
  for (const tab of tabs) {
    await board.getByRole("button", { name: tab, exact: true }).click();
    await head.getByText(`stop #${stop}`).waitFor();
    const times: number[] = [];
    for (let round = 0; round < rounds; round++) {
      stop += 1;
      const started = performance.now();
      await page.keyboard.press("F5");
      await head.getByText(`stop #${stop}`).waitFor();
      times.push(performance.now() - started);
    }
    times.sort((a, b) => a - b);
    const median = times[Math.floor(times.length / 2)] ?? 0;
    const slowest = times.at(-1) ?? 0;
    console.log(
      `  ${tab.padEnd(14)} median ${median.toFixed(0).padStart(5)} ms   slowest ${slowest.toFixed(0).padStart(5)} ms`,
    );
  }
} finally {
  await browser.close();
  await server.stop();
}
