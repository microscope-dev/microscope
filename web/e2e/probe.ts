// Drives a real `microscope web` through a few steps and saves a screenshot
// after each, printing the page's errors: the quickest look at one interaction.
// `node e2e/probe.ts PROGRAM STEP...`, where a step is `key:F5`, `click:TEXT`,
// `type:TEXT`, `wait:TEXT`, `goto:PATH`, `eval:JS`, or `sleep:MS`; `views:PATH`
// and `arg:TEXT`, wherever they are, start microscope with that views file or
// hand the program that argument.

import * as path from "node:path";

import { chromium } from "@playwright/test";

import { root, startMicroscope } from "./server.ts";

const [program, ...given] = process.argv.slice(2);
if (!program) {
  throw new Error("usage: probe.ts PROGRAM STEP...");
}
const starting = given.filter((step) => step.startsWith("views:") || step.startsWith("arg:"));
const steps = given.filter((step) => !starting.includes(step));
const views = starting.flatMap((step) =>
  step.startsWith("views:") ? ["--views", path.resolve(root, step.slice("views:".length))] : [],
);
const args = starting.flatMap((step) =>
  step.startsWith("arg:") ? [step.slice("arg:".length)] : [],
);
const out = path.join(root, "target", "web-shots");
const server = await startMicroscope([...views, program, ...args], "");
const browser = await chromium.launch();
try {
  const page = await browser.newPage({ viewport: { width: 1440, height: 800 } });
  page.on("pageerror", (error) => console.error(`page error: ${error.stack ?? error.message}`));
  page.on("console", (message) => console.log(`console.${message.type()}: ${message.text()}`));
  await page.goto(server.link);
  await page.waitForURL(/\/(s|profile)\//);
  for (const [index, step] of steps.entries()) {
    const [kind, ...rest] = step.split(":");
    const argument = rest.join(":");
    switch (kind) {
      case "key":
        await page.keyboard.press(argument);
        break;
      case "click":
        // Whatever shows the text: its own, a label, or a placeholder.
        await page
          .getByText(argument)
          .or(page.getByLabel(argument))
          .or(page.getByPlaceholder(argument))
          .first()
          .click({ timeout: 5000 });
        break;
      case "type":
        await page.keyboard.type(argument);
        break;
      case "wait":
        await page.getByText(argument, { exact: false }).first().waitFor({ timeout: 5000 });
        break;
      case "goto":
        await page.goto(new URL(argument, page.url()).href);
        break;
      case "eval":
        console.log(`eval: ${JSON.stringify(await page.evaluate(argument))}`);
        break;
      case "sleep":
        await page.waitForTimeout(Number(argument));
        break;
      default:
        throw new Error(`unknown step ${step}`);
    }
    await page.waitForTimeout(150);
    const file = path.join(out, `probe-${index + 1}.png`);
    await page.screenshot({ path: file });
    console.log(`${step} → ${page.url()} (${file})`);
  }
} finally {
  await browser.close();
  await server.stop();
}
